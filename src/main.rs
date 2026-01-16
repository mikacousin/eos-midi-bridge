#![windows_subsystem = "windows"]
mod gui;

use anyhow::Context;
use eos_midi_bridge::{
    CrossfadeState, MackieEvent, config,
    midi::{Midi, handle_event_logic},
    osc::{OscClient, OscServer},
};
use log::{error, info, warn};
use midir::{MidiInput, MidiInputConnection, MidiOutput};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Load Persisted Configuration
    let cfg = match config::load_config() {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to load config, using defaults: {}", e);
            config::BridgeConfig::default()
        }
    };
    info!("Starting Bridge with Eos IP: {}", cfg.eos_ip);

    // Setup Async Runtime for background tasks
    let rt = Runtime::new().context("Failed to create Tokio runtime")?;
    let _guard = rt.enter();
    let cancel_token = CancellationToken::new();

    // Initialize OSC Client
    let osc_client = rt
        .block_on(OscClient::new(&cfg.eos_ip, cfg.eos_osc_port))
        .context("Failed to create OSC client")?;
    let midi = Arc::new(Mutex::new(Midi::new(osc_client)));

    // Scan available MIDI ports for the GUI dropdowns and Auto-connect logic
    let midi_in_scanner =
        MidiInput::new("Scanner In").context("Failed to create MIDI input scanner")?;
    let midi_out_scanner =
        MidiOutput::new("Scanner Out").context("Failed to create MIDI output scanner")?;

    let in_ports: Vec<String> = midi_in_scanner
        .ports()
        .iter()
        .filter_map(|p| midi_in_scanner.port_name(p).ok())
        .collect();

    let out_ports: Vec<String> = midi_out_scanner
        .ports()
        .iter()
        .filter_map(|p| midi_out_scanner.port_name(p).ok())
        .collect();

    // Setup Event Channel
    let (tx, mut rx) = mpsc::channel::<MackieEvent>(100);

    // Start the Background Event Processor (handle_event_logic)
    let midi_logic = Arc::clone(&midi);
    rt.spawn(async move {
        while let Some(event) = rx.recv().await {
            handle_event_logic(event, Arc::clone(&midi_logic)).await;
        }
    });

    // Store MIDI input connection to keep it alive
    let midi_input_conn: Arc<Mutex<Option<MidiInputConnection<()>>>> = Arc::new(Mutex::new(None));

    // Auto-connect if config matches available hardware
    if let (Some(saved_in), Some(saved_out)) = (&cfg.midi_in_name, &cfg.midi_out_name) {
        if in_ports.contains(saved_in) && out_ports.contains(saved_out) {
            info!("Auto-connecting to {} and {}", saved_in, saved_out);
            match setup_midi(Arc::clone(&midi), saved_in, saved_out, tx.clone()) {
                Ok(conn_in) => {
                    *midi_input_conn.lock().unwrap() = Some(conn_in);
                    info!("MIDI auto-connection successful");
                }
                Err(e) => {
                    error!("Failed to auto-connect MIDI: {}", e);
                }
            }
        } else {
            info!("Saved ports not found. Use GUI to select available ports.");
        }
    }

    // Request initial fader configuration and data from Eos
    let init_client = match midi.lock() {
        Ok(m) => m.osc_client.clone(),
        Err(e) => {
            error!("Failed to lock MIDI state: {}", e);
            return Err(anyhow::anyhow!("MIDI state poisoned"));
        }
    };

    rt.spawn(async move {
        info!("Requesting fader configuration from Eos...");
        if let Err(e) = init_client
            .send("/eos/user/1/fader/1/config/1/10", vec![])
            .await
        {
            warn!("Failed to send fader config request: {}", e);
        }

        if let Err(e) = init_client
            .send("/eos/subscribe", vec![rosc::OscType::Int(1)])
            .await
        {
            warn!("Failed to subscribe to Eos updates: {}", e);
        }
    });

    // Flash Play button on Pause
    let flash_midi = Arc::clone(&midi);
    rt.spawn(async move {
        let mut tick = false;
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            match flash_midi.lock() {
                Ok(m) => {
                    if m.crossfade_state == CrossfadeState::Pause {
                        tick = !tick;
                        let vel = if tick { 127 } else { 0 };
                        m.send_queue.enqueue(vec![0x90, 94, vel]);
                    }
                }
                Err(e) => {
                    error!("Flash task: Failed to lock MIDI state: {}", e);
                    break;
                }
            }
        }
    });

    // Start the Outgoing MIDI Pump
    let loop_midi = Arc::clone(&midi);
    let loop_token = cancel_token.clone();
    rt.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(5));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match loop_midi.lock() {
                        Ok(mut m) => {
                            while let Some(msg) = m.send_queue.dequeue() {
                                for conn in &mut m.connections {
                                    if let Err(e) = conn.send(&msg) {
                                        error!("Failed to send MIDI message: {}", e);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("MIDI pump: Failed to lock MIDI state: {}", e);
                            break;
                        }
                    }
                }
                _ = loop_token.cancelled() => break,
            }
        }
    });

    // Spawn OSC Server for feedback from Eos
    let osc_server_midi = Arc::clone(&midi);
    let osc_server_token = cancel_token.clone();
    rt.spawn(async move {
        let server = OscServer {
            port: cfg.bridge_listen_port,
        };
        if let Err(e) = server.start(osc_server_midi, osc_server_token).await {
            error!("OSC Server Error: {}", e);
        }
    });

    // Launch the GUI
    let gui_midi = midi.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    let gui_result = eframe::run_native(
        "Eos Mackie Bridge",
        options,
        Box::new(|_cc| {
            Ok(Box::new(gui::BridgeApp::new(
                gui_midi, cfg, in_ports, out_ports,
            )))
        }),
    );

    // Shutdown and Flush (equivalent to flush_midi)
    info!("Shutting down and resetting controller...");
    match midi.lock() {
        Ok(mut m) => {
            m.controller_reset(); // Queues reset sysex

            // Immediately flush the queue to the hardware before dropping connections
            while let Some(msg) = m.send_queue.dequeue() {
                for conn in &mut m.connections {
                    let _ = conn.send(&msg);
                }
            }
        }
        Err(e) => {
            error!("Failed to reset controller on shutdown: {}", e);
        }
    }

    // Drop MIDI input connection cleanly
    drop(midi_input_conn);

    // Cleanup on Exit
    cancel_token.cancel();
    rt.block_on(async { tokio::time::sleep(Duration::from_millis(200)).await });

    gui_result.map_err(|e| anyhow::anyhow!("GUI Error: {}", e))
}

/// Helper to connect to MIDI ports by the exact names found in scan/config
fn setup_midi(
    midi_state: Arc<Mutex<Midi>>,
    in_name: &str,
    out_name: &str,
    tx: mpsc::Sender<MackieEvent>,
) -> anyhow::Result<MidiInputConnection<()>> {
    let midi_in = MidiInput::new("Bridge In").context("Failed to create MIDI input")?;
    let midi_out = MidiOutput::new("Bridge Out").context("Failed to create MIDI output")?;

    let in_port = midi_in
        .ports()
        .into_iter()
        .find(|p| midi_in.port_name(p).ok().as_deref() == Some(in_name))
        .context(format!("MIDI Input '{}' not found", in_name))?;

    let out_port = midi_out
        .ports()
        .into_iter()
        .find(|p| midi_out.port_name(p).ok().as_deref() == Some(out_name))
        .context(format!("MIDI Output '{}' not found", out_name))?;

    let conn_in = midi_in
        .connect(
            &in_port,
            "bridge-in-conn",
            move |_, msg, _| {
                if let Err(e) = tx.blocking_send(MackieEvent::MidiIn(msg.to_vec())) {
                    error!("Failed to send MIDI event to processor: {}", e);
                }
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!("MIDI In Connect Error: {}", e))?;

    let conn_out = midi_out
        .connect(&out_port, "bridge-out-conn")
        .map_err(|e| anyhow::anyhow!("MIDI Out Connect Error: {}", e))?;

    match midi_state.lock() {
        Ok(mut m) => {
            m.connections.push(conn_out);
            info!("Successfully connected MIDI ports.");
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to lock MIDI state: {}", e));
        }
    }

    Ok(conn_in)
}

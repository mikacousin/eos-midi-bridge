mod gui;

use anyhow::Context;
use eos_midi_bridge::{
    CrossfadeState, MackieEvent, config,
    midi::{Midi, handle_event_logic},
    osc::{OscClient, OscServer},
};
use log::{error, info};
use midir::{MidiInput, MidiOutput};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Load Persisted Configuration
    let cfg = config::load_config();
    info!("Starting Bridge with Eos IP: {}", cfg.eos_ip);

    // Setup Async Runtime for background tasks
    let rt = Runtime::new()?;
    let _guard = rt.enter();
    let cancel_token = CancellationToken::new();

    // Initialize OSC Client
    let osc_client = rt.block_on(OscClient::new(&cfg.eos_ip, cfg.eos_osc_port))?;
    let midi = Arc::new(Mutex::new(Midi::new(osc_client)));

    // Scan available MIDI ports for the GUI dropdowns and Auto-connect logic
    let midi_in_scanner = MidiInput::new("Scanner In")?;
    let midi_out_scanner = MidiOutput::new("Scanner Out")?;

    let in_ports: Vec<String> = midi_in_scanner
        .ports()
        .iter()
        .map(|p| midi_in_scanner.port_name(p).unwrap_or_default())
        .collect();

    let out_ports: Vec<String> = midi_out_scanner
        .ports()
        .iter()
        .map(|p| midi_out_scanner.port_name(p).unwrap_or_default())
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

    // Auto-connect if config matches available hardware
    if let (Some(saved_in), Some(saved_out)) = (&cfg.midi_in_name, &cfg.midi_out_name) {
        if in_ports.contains(saved_in) && out_ports.contains(saved_out) {
            info!("Auto-connecting to {} and {}", saved_in, saved_out);
            let _ = setup_midi(Arc::clone(&midi), saved_in, saved_out, tx.clone());
        } else {
            info!("Saved ports not found. Use GUI to select available ports.");
        }
    }

    // Request initial fader configuration and data from Eos
    let init_client = midi.lock().unwrap().osc_client.clone();
    rt.spawn(async move {
        info!("Requesting fader configuration from Eos...");
        // This tells Eos we are on Page 1 and want data for 10 faders
        let _ = init_client
            .send("/eos/user/1/fader/1/config/1/10", vec![])
            .await;

        // Also good to subscribe to general updates
        let _ = init_client
            .send("/eos/subscribe", vec![rosc::OscType::Int(1)])
            .await;
    });

    // Flash Play button on Pause
    let flash_midi = Arc::clone(&midi);
    rt.spawn(async move {
        let mut tick = false;
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            let m = flash_midi.lock().unwrap();
            if m.crossfade_state == CrossfadeState::Pause {
                tick = !tick;
                let vel = if tick { 127 } else { 0 };
                m.send_queue.enqueue(vec![0x90, 94, vel]);
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
                    let mut m = loop_midi.lock().unwrap();
                    while let Some(msg) = m.send_queue.dequeue() {
                        for conn in &mut m.connections {
                            if let Err(e) = conn.send(&msg) {
                                log::error!("Failed to send MIDI message: {}", e);
                            }
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
            error!("OSC Server Error : {}", e);
        }
    });

    // Launch the GUI
    let gui_midi = midi.clone();
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Eos Mackie Bridge",
        options,
        Box::new(|_cc| {
            Ok(Box::new(gui::BridgeApp::new(
                gui_midi, cfg, in_ports, out_ports,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("GUI Error: {}", e))?;

    // Shutdown and Flush (equivalent to flush_midi)
    info!("Shutting down and resetting controller...");
    {
        let mut m = midi.lock().unwrap();
        m.controller_reset(); // Queues reset sysex

        // Immediately flush the queue to the hardware before dropping connections
        while let Some(msg) = m.send_queue.dequeue() {
            for conn in &mut m.connections {
                let _ = conn.send(&msg);
            }
        }
    }

    // Cleanup on Exit
    cancel_token.cancel();
    rt.block_on(async { tokio::time::sleep(Duration::from_millis(200)).await });
    Ok(())
}

/// Helper to connect to MIDI ports by the exact names found in scan/config
fn setup_midi(
    midi_state: Arc<Mutex<Midi>>,
    in_name: &str,
    out_name: &str,
    tx: mpsc::Sender<MackieEvent>,
) -> anyhow::Result<()> {
    let midi_in = MidiInput::new("Bridge In")?;
    let midi_out = MidiOutput::new("Bridge Out")?;

    let in_port = midi_in
        .ports()
        .into_iter()
        .find(|p| midi_in.port_name(p).unwrap_or_default() == in_name)
        .context("Could not find MIDI Input matching config")?;

    let out_port = midi_out
        .ports()
        .into_iter()
        .find(|p| midi_out.port_name(p).unwrap_or_default() == out_name)
        .context("Could not find MIDI Output matching config")?;

    let _conn_in = midi_in
        .connect(
            &in_port,
            "bridge-in-conn",
            move |_, msg, _| {
                let _ = tx.blocking_send(MackieEvent::MidiIn(msg.to_vec()));
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!("MIDI In Connect Error: {}", e))?;

    let conn_out = midi_out
        .connect(&out_port, "bridge-out-conn")
        .map_err(|e| anyhow::anyhow!("MIDI Out Connect Error: {}", e))?;

    let mut m = midi_state.lock().unwrap();
    m.connections.push(conn_out);
    // Keep the input connection alive by leaking it or storing it in the Midi struct
    Box::leak(Box::new(_conn_in));

    info!("Successfully connected MIDI ports.");
    Ok(())
}

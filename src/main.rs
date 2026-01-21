#![windows_subsystem = "windows"]
mod gui;

use anyhow::Context;
use eos_midi_bridge::{
    CrossfadeState, MackieEvent, SystemCommand, clean_midi_name, config, controller,
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

    // Load Initial Configuration
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

    // Setup Channels
    let (tx_midi, mut rx_midi) = mpsc::channel::<MackieEvent>(100);
    let (tx_system, mut rx_system) = mpsc::channel::<SystemCommand>(10);

    // Initialize Shared Midi State (start with dummy OSC client, will be replaced)
    let rt_handle = rt.handle().clone();
    let osc_client = rt_handle
        .block_on(OscClient::new(&cfg.eos_ip, cfg.eos_osc_port))
        .context("Failed to create initial OSC client")?;

    let profile = controller::load_profile(&cfg.controller_profile).unwrap_or_else(|e| {
        warn!(
            "Failed to load controller profile {}, using default: {}",
            cfg.controller_profile, e
        );
        controller::ControllerProfile::default()
    });

    let midi = Arc::new(Mutex::new(Midi::new(osc_client, profile)));

    // Background Event Processor (always running)
    let midi_logic = Arc::clone(&midi);
    rt.spawn(async move {
        while let Some(event) = rx_midi.recv().await {
            handle_event_logic(event, Arc::clone(&midi_logic)).await;
        }
    });

    // Flash Play button on Pause (always running)
    let flash_midi = Arc::clone(&midi);
    rt.spawn(async move {
        let mut tick = false;
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            if let Ok(m) = flash_midi.lock() {
                if m.crossfade_state == CrossfadeState::Pause {
                    tick = !tick;
                    let vel = if tick { 127 } else { 0 };
                    if let Some((note, chan, _)) = m
                        .profile
                        .get_midi_output_for_action(crate::controller::LogicalAction::Go)
                    {
                        m.send_queue.enqueue(vec![0x90 | chan, note, vel]);
                    }
                }
            }
        }
    });

    // Supervision Loop: Handles (re)starting MIDI and OSC services
    let supervision_midi = Arc::clone(&midi);
    let supervision_tx_midi = tx_midi.clone();
    let initial_config = cfg.clone();
    rt.spawn(async move {
        let mut current_cancel_token = CancellationToken::new();
        let mut _midi_input_conn: Option<MidiInputConnection<()>> = None;

        let mut config = initial_config;

        loop {
            // Cancel previous tasks and cleanup
            current_cancel_token.cancel();
            current_cancel_token = CancellationToken::new();
            _midi_input_conn = None; // Dropping old connection

            {
                let mut m = supervision_midi.lock().unwrap();
                m.connections.clear();

                // Reload profile if needed
                let profile_path = &config.controller_profile;
                // Reload the profile based on the current config
                match controller::load_profile(profile_path) {
                    Ok(p) => {
                        info!("Loaded controller profile: {}", p.name);
                        m.profile = p;
                    }
                    Err(e) => error!("Failed to reload profile {}: {}", profile_path, e),
                }
            }

            // Small delay to ensure OS releases UDP ports
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Start OSC Server FIRST so we are ready for Eos replies
            let server_midi = Arc::clone(&supervision_midi);
            let server_token = current_cancel_token.clone();
            let listen_port = config.bridge_listen_port;
            tokio::spawn(async move {
                let server = OscServer { port: listen_port };
                if let Err(e) = server.start(server_midi, server_token).await {
                    error!("OSC Server Error: {}", e);
                }
            });

            // OSC Heartbeat Loop
            let heartbeat_midi = Arc::clone(&supervision_midi);
            let heartbeat_token = current_cancel_token.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(2));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let (client, last_heartbeat) = {
                                let m = heartbeat_midi.lock().unwrap();
                                (m.osc_client.clone(), m.last_osc_heartbeat)
                            };

                            // Send Ping
                            let _ = client.send("/eos/ping", vec![]).await;

                            // Check for timeout if we ever received a pong
                            if let Some(last) = last_heartbeat {
                                if last.elapsed() > Duration::from_secs(4) {
                                    let mut m = heartbeat_midi.lock().unwrap();
                                    m.needs_sync = true; // Retry sync when it comes back
                                }
                            }
                        }
                        _ = heartbeat_token.cancelled() => break,
                    }
                }
            });

            // Let the server bind
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Create new OSC client with updated config
            match OscClient::new(&config.eos_ip, config.eos_osc_port).await {
                Ok(new_client) => {
                    let mut m = supervision_midi.lock().unwrap();
                    m.osc_client = new_client.clone();
                    // Mark as needing sync, will be triggered by first Pong
                    m.needs_sync = true;
                    m.last_osc_heartbeat = None;
                }
                Err(e) => {
                    error!("Failed to create OSC client: {}", e);
                }
            }

            // Start MIDI Pump
            let pump_midi = Arc::clone(&supervision_midi);
            let pump_token = current_cancel_token.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(5));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Ok(mut m) = pump_midi.lock() {
                                while let Some(msg) = m.send_queue.dequeue() {
                                    for conn in &mut m.connections {
                                        let _ = conn.send(&msg);
                                    }
                                }
                            }
                        }
                        _ = pump_token.cancelled() => break,
                    }
                }
            });

            // MIDI Connections
            if let (Some(in_name), Some(out_name)) = (&config.midi_in_name, &config.midi_out_name) {
                match setup_midi(
                    Arc::clone(&supervision_midi),
                    in_name,
                    out_name,
                    supervision_tx_midi.clone(),
                ) {
                    Ok(conn) => {
                        _midi_input_conn = Some(conn);
                        info!("MIDI connected to {}/{}", in_name, out_name);
                        if let Ok(mut m) = supervision_midi.lock() {
                            m.connection_status =
                                format!("✓ Connected to {} / {}", in_name, out_name);
                        }
                    }
                    Err(e) => {
                        error!("Failed to connect MIDI: {}", e);
                        if let Ok(mut m) = supervision_midi.lock() {
                            m.connection_status = format!("❌ MIDI Error: {}", e);
                        }
                    }
                }
            } else {
                if let Ok(mut m) = supervision_midi.lock() {
                    m.connection_status = "⚠ MIDI Ports not selected".to_string();
                }
            }

            // Wait for reconfiguration command
            if let Some(SystemCommand::Reconfigure(new_config)) = rx_system.recv().await {
                info!("Reconfiguring with Eos IP: {}", new_config.eos_ip);
                config = new_config;
            } else {
                break; // Channel closed
            }
        }
    });

    // Pre-flight scan: Populate available MIDI ports before launching anything else
    {
        let in_ports = match MidiInput::new("Preflight In") {
            Ok(scanner) => scanner
                .ports()
                .iter()
                .filter_map(|p| scanner.port_name(p).ok().map(|name| clean_midi_name(&name)))
                .collect::<Vec<String>>(),
            Err(_) => Vec::new(),
        };
        let out_ports = match MidiOutput::new("Preflight Out") {
            Ok(scanner) => scanner
                .ports()
                .iter()
                .filter_map(|p| scanner.port_name(p).ok().map(|name| clean_midi_name(&name)))
                .collect::<Vec<String>>(),
            Err(_) => Vec::new(),
        };
        if let Ok(mut m) = midi.lock() {
            m.available_in_ports = in_ports;
            m.available_out_ports = out_ports;
        }
    }

    // MIDI Monitoring Task: Periodically scan ports and check connectivity
    let monitor_midi = Arc::clone(&midi);
    rt.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;

            let in_ports = match MidiInput::new("Monitor In") {
                Ok(scanner) => scanner
                    .ports()
                    .iter()
                    .filter_map(|p| scanner.port_name(p).ok().map(|name| clean_midi_name(&name)))
                    .collect::<Vec<String>>(),
                Err(_) => Vec::new(),
            };

            let out_ports = match MidiOutput::new("Monitor Out") {
                Ok(scanner) => scanner
                    .ports()
                    .iter()
                    .filter_map(|p| scanner.port_name(p).ok().map(|name| clean_midi_name(&name)))
                    .collect::<Vec<String>>(),
                Err(_) => Vec::new(),
            };

            if let Ok(mut m) = monitor_midi.lock() {
                m.available_in_ports = in_ports;
                m.available_out_ports = out_ports;
            }
        }
    });

    // Launch the GUI
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([840.0, 510.0]),
        ..Default::default()
    };
    let app_midi = midi.clone();
    let app_tx_system = tx_system.clone();
    let gui_result = eframe::run_native(
        "Eos Mackie Bridge",
        options,
        Box::new(move |_cc| Ok(Box::new(gui::BridgeApp::new(app_midi, cfg, app_tx_system)))),
    );

    // Shutdown and Reset
    info!("Shutting down and resetting controller...");
    if let Ok(mut m) = midi.lock() {
        m.controller_reset();
        while let Some(msg) = m.send_queue.dequeue() {
            for conn in &mut m.connections {
                let _ = conn.send(&msg);
            }
        }
    }

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
        .find(|p| {
            midi_in
                .port_name(p)
                .ok()
                .map(|name| clean_midi_name(&name))
                .as_deref()
                == Some(in_name)
        })
        .context(format!("MIDI Input '{}' not found", in_name))?;

    let out_port = midi_out
        .ports()
        .into_iter()
        .find(|p| {
            midi_out
                .port_name(p)
                .ok()
                .map(|name| clean_midi_name(&name))
                .as_deref()
                == Some(out_name)
        })
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

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
    let (tx_log, rx_log) = mpsc::channel::<eos_midi_bridge::LogEntry>(500);

    // Initialize Shared Midi State (start with dummy OSC client, will be replaced)
    let rt_handle = rt.handle().clone();
    let osc_client = rt_handle
        .block_on(OscClient::new(
            &cfg.eos_ip,
            cfg.eos_osc_port,
            tx_log.clone(),
        ))
        .context("Failed to create initial OSC client")?;

    let midi = Arc::new(Mutex::new(Midi::new(osc_client, tx_log.clone())));

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
            if let Ok(m) = flash_midi.lock()
                && m.crossfade_state == CrossfadeState::Pause
            {
                tick = !tick;
                let vel = if tick { 127 } else { 0 };
                // Iterate over all devices
                for (device_name, profile) in &m.device_profiles {
                    if let Some((note, chan, _)) =
                        profile.get_midi_output_for_action(crate::controller::LogicalAction::Go)
                    {
                        m.enqueue(device_name, vec![0x90 | chan, note, vel]);
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
        let mut mv_midi_input_conns: Vec<MidiInputConnection<()>> = Vec::new();

        let mut config = initial_config;

        loop {
            // Cancel previous tasks and cleanup
            current_cancel_token.cancel();
            current_cancel_token = CancellationToken::new();
            mv_midi_input_conns.clear(); // Dropping old connections

            {
                let mut m = supervision_midi.lock().unwrap();
                m.device_connections.clear();
                m.device_profiles.clear();
                m.device_fader_values.clear();

                for (device_name, profile_path) in &config.enabled_devices {
                    match controller::load_profile(profile_path) {
                        Ok(mut p) => {
                            info!("Loaded profile '{}' for device '{}'", p.name, device_name);
                            p.inject_feedback_mappings();
                            m.device_profiles.insert(device_name.clone(), p);
                        }
                        Err(e) => error!("Failed to load profile {} for {}: {}", profile_path, device_name, e),
                    }
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

            // Visual Effects (Blink) Loop
            let blink_midi = Arc::clone(&supervision_midi);
            let blink_token = current_cancel_token.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(500));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                             let mut m = blink_midi.lock().unwrap();
                             m.tick_blink();
                        }
                        _ = blink_token.cancelled() => break,
                    }
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
                            if let Some(last) = last_heartbeat
                                && last.elapsed() > Duration::from_secs(4)
                            {
                                let mut m = heartbeat_midi.lock().unwrap();
                                m.needs_sync = true; // Retry sync when it comes back
                            }
                        }
                        _ = heartbeat_token.cancelled() => break,
                    }
                }
            });

            // Let the server bind
            tokio::time::sleep(Duration::from_millis(100)).await;

            let m_log_sender = {
                let m = supervision_midi.lock().unwrap();
                m.log_sender.clone()
            };

            // Create new OSC client with updated config
            match OscClient::new(&config.eos_ip, config.eos_osc_port, m_log_sender.clone()).await {
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
                                while let Some((target_device, msg)) = m.send_queue.dequeue() {
                                    // VISUAL FEEDBACK: Removed to avoid overwriting specific logs from midi.rs
                                    // let matches = m.find_mappings_for_midi_message(&msg, &target_device);
                                    // ...

                                    if let Some(conn) = m.device_connections.get_mut(&target_device) {
                                        let _ = conn.send(&msg);
                                        // Log Output
                                        let _ = m.log_sender.try_send(eos_midi_bridge::LogEntry {
                                            time: std::time::Instant::now(),
                                            source: eos_midi_bridge::LogSource::MidiOut,
                                            content: format!("{} -> {:02X?}", target_device, msg),
                                        });
                                    }
                                }
                            }
                        }
                        _ = pump_token.cancelled() => break,
                    }
                }
            });

            // MIDI Connections
            // MIDI Connections for all enabled devices
            if !config.enabled_devices.is_empty() {
                match setup_midi_devices(
                    Arc::clone(&supervision_midi),
                    &config.enabled_devices,
                    supervision_tx_midi.clone(),
                ) {
                    Ok(conns) => {
                        mv_midi_input_conns = conns;
                        info!("MIDI devices connected: {:?}", config.enabled_devices);
                        if let Ok(mut m) = supervision_midi.lock() {
                            m.connection_status = format!(
                                "✓ Connected to {} devices",
                                config.enabled_devices.len()
                            );
                        }
                    }
                    Err(e) => {
                        error!("Failed to connect MIDI devices: {}", e);
                        if let Ok(mut m) = supervision_midi.lock() {
                            m.connection_status = format!("❌ MIDI Error: {}", e);
                        }
                    }
                }
            } else if let Ok(mut m) = supervision_midi.lock() {
                m.connection_status = "⚠ No MIDI Devices Enabled".to_string();
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
                .filter(|name| !name.starts_with("Bridge"))
                .collect::<Vec<String>>(),
            Err(_) => Vec::new(),
        };
        let out_ports = match MidiOutput::new("Preflight Out") {
            Ok(scanner) => scanner
                .ports()
                .iter()
                .filter_map(|p| scanner.port_name(p).ok().map(|name| clean_midi_name(&name)))
                .filter(|name| !name.starts_with("Bridge"))
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
                    .filter(|name| !name.starts_with("Bridge"))
                    .collect::<Vec<String>>(),
                Err(_) => Vec::new(),
            };

            let out_ports = match MidiOutput::new("Monitor Out") {
                Ok(scanner) => scanner
                    .ports()
                    .iter()
                    .filter_map(|p| scanner.port_name(p).ok().map(|name| clean_midi_name(&name)))
                    .filter(|name| !name.starts_with("Bridge"))
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
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([840.0, 510.0])
            .with_maximized(true),
        ..Default::default()
    };
    let app_midi = midi.clone();
    let app_tx_system = tx_system.clone();
    let gui_result = eframe::run_native(
        "Eos Mackie Bridge",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(gui::BridgeApp::new(
                app_midi,
                cfg,
                app_tx_system,
                rx_log,
            )))
        }),
    );

    // Shutdown and Reset
    info!("Shutting down and resetting controller...");
    if let Ok(mut m) = midi.lock() {
        m.reset_all_outputs();
        while let Some((target, msg)) = m.send_queue.dequeue() {
            if let Some(conn) = m.device_connections.get_mut(&target) {
                let _ = conn.send(&msg);
            }
        }
    }

    gui_result.map_err(|e| anyhow::anyhow!("GUI Error: {}", e))
}

/// Helper to connect to MIDI ports by the exact names found in scan/config
/// Helper to connect to MULTIPLE MIDI devices by name
fn setup_midi_devices(
    midi_state: Arc<Mutex<Midi>>,
    enabled_devices: &std::collections::HashMap<String, String>,
    tx: mpsc::Sender<MackieEvent>,
) -> anyhow::Result<Vec<MidiInputConnection<()>>> {
    let mut input_connections = Vec::new(); // Keep alive

    // Iterate over enabled devices and attempt to connect
    for device_name in enabled_devices.keys() {
        // --- INPUT ---
        // We create a new client for each device attempt because we consume it
        let midi_in = MidiInput::new(&format!("Bridge In {}", device_name))
            .context("Failed to create MIDI input client")?;

        if let Some(in_port) = midi_in.ports().into_iter().find(|p| {
            midi_in
                .port_name(p)
                .ok()
                .map(|name| clean_midi_name(&name))
                .as_deref()
                == Some(device_name)
        }) {
            let tx_clone = tx.clone();
            let d_name_log = device_name.clone();
            let d_name_event = d_name_log.clone();
            let midi_state_clone = midi_state.clone();
            match midi_in.connect(
                &in_port,
                "bridge-in-conn",
                move |_, msg, _| {
                    // Log Input
                    if let Ok(m) = midi_state_clone.lock() {
                        let _ = m.log_sender.try_send(eos_midi_bridge::LogEntry {
                            time: std::time::Instant::now(),
                            source: eos_midi_bridge::LogSource::MidiIn,
                            content: format!("{} <- {:02X?}", d_name_log, msg),
                        });
                    }

                    if let Err(e) = tx_clone.blocking_send(MackieEvent::MidiIn {
                        device_name: d_name_event.clone(),
                        data: msg.to_vec(),
                    }) {
                        error!(
                            "Failed to send MIDI event from {} to processor: {}",
                            d_name_log, e
                        );
                    }
                },
                (),
            ) {
                Ok(conn) => {
                    input_connections.push(conn);
                    info!("Connected Input: {}", device_name);
                }
                Err(e) => error!("Failed to connect Input for {}: {}", device_name, e),
            }
        } else {
            warn!("MIDI Input device '{}' not found", device_name);
        }

        // --- OUTPUT ---
        let midi_out = MidiOutput::new(&format!("Bridge Out {}", device_name))
            .context("Failed to create MIDI output client")?;

        if let Some(out_port) = midi_out.ports().into_iter().find(|p| {
            midi_out
                .port_name(p)
                .ok()
                .map(|name| clean_midi_name(&name))
                .as_deref()
                == Some(device_name)
        }) {
            match midi_out.connect(&out_port, "bridge-out-conn") {
                Ok(mut conn) => {
                    info!("Connected Output: {}", device_name);

                    // Send startup messages
                    if let Ok(m) = midi_state.lock() {
                        if let Some(profile) = m.device_profiles.get(device_name) {
                            for msg in &profile.startup_messages {
                                let _ = conn.send(msg);
                            }
                        }
                    }

                    if let Ok(mut m) = midi_state.lock() {
                        m.device_connections.insert(device_name.clone(), conn);
                    }
                }
                Err(e) => error!("Failed to connect Output for {}: {}", device_name, e),
            }
        } else {
            warn!("MIDI Output device '{}' not found", device_name);
        }
    }

    Ok(input_connections)
}

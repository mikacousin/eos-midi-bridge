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
    let (tx_midi, rx_midi) = mpsc::channel::<MackieEvent>(100);
    let (tx_system, rx_system) = mpsc::channel::<SystemCommand>(10);
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

    // Background Event Processor
    spawn_event_processor(&rt, rx_midi, midi.clone());

    // Flash Play button on Pause
    spawn_flash_loop(&rt, midi.clone());

    // Supervision Loop
    spawn_supervision_loop(&rt, midi.clone(), tx_midi, rx_system, cfg.clone());

    // Load Icon
    let icon = match load_icon() {
        Ok(i) => Some(i),
        Err(e) => {
            warn!("Failed to load app icon: {}", e);
            None
        }
    };

    // Launch the GUI
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([840.0, 510.0])
            .with_maximized(true)
            .with_icon(icon.unwrap_or_default()), // Handle optional efficiently
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

fn spawn_event_processor(
    rt: &Runtime,
    mut rx_midi: mpsc::Receiver<MackieEvent>,
    midi_logic: Arc<Mutex<Midi>>,
) {
    rt.spawn(async move {
        while let Some(event) = rx_midi.recv().await {
            handle_event_logic(event, Arc::clone(&midi_logic)).await;
        }
    });
}

fn spawn_flash_loop(rt: &Runtime, flash_midi: Arc<Mutex<Midi>>) {
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
}

fn spawn_monitor_loop(rt: &Runtime, monitor_midi: Arc<Mutex<Midi>>) {
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
}

fn spawn_supervision_loop(
    rt: &Runtime,
    supervision_midi: Arc<Mutex<Midi>>,
    tx_midi: mpsc::Sender<MackieEvent>,
    mut rx_system: mpsc::Receiver<SystemCommand>,
    initial_config: config::BridgeConfig,
) {
    let supervision_tx_midi = tx_midi.clone();

    // Pre-flight scan
    perform_preflight_scan(&supervision_midi);

    // Monitor Loop
    spawn_monitor_loop(rt, supervision_midi.clone());

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
                        Err(e) => error!(
                            "Failed to load profile {} for {}: {}",
                            profile_path, device_name, e
                        ),
                    }
                }
            }

            // Small delay to ensure OS releases UDP ports
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Start OSC Server FIRST so we are ready for Eos replies
            spawn_osc_server(
                supervision_midi.clone(),
                current_cancel_token.clone(),
                config.bridge_listen_port,
            );

            // Visual Effects (Blink) Loop
            spawn_blink_loop(supervision_midi.clone(), current_cancel_token.clone());

            // OSC Heartbeat Loop
            spawn_heartbeat_loop(supervision_midi.clone(), current_cancel_token.clone());

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
            spawn_midi_pump(supervision_midi.clone(), current_cancel_token.clone());

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
                            m.connection_status =
                                format!("✓ Connected to {} devices", config.enabled_devices.len());
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
}

fn spawn_osc_server(midi: Arc<Mutex<Midi>>, token: CancellationToken, port: u16) {
    tokio::spawn(async move {
        let server = OscServer { port };
        if let Err(e) = server.start(midi, token).await {
            error!("OSC Server Error: {}", e);
        }
    });
}

fn spawn_blink_loop(midi: Arc<Mutex<Midi>>, token: CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let mut m = midi.lock().unwrap();
                    m.tick_blink();
                }
                _ = token.cancelled() => break,
            }
        }
    });
}

fn spawn_heartbeat_loop(midi: Arc<Mutex<Midi>>, token: CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let (client, last_heartbeat) = {
                        let m = midi.lock().unwrap();
                        (m.osc_client.clone(), m.last_osc_heartbeat)
                    };

                    // Send Ping
                    let _ = client.send("/eos/ping", vec![]).await;

                    // Check for timeout if we ever received a pong
                    if let Some(last) = last_heartbeat
                        && last.elapsed() > Duration::from_secs(4)
                    {
                        let mut m = midi.lock().unwrap();
                        m.needs_sync = true; // Retry sync when it comes back
                    }
                }
                _ = token.cancelled() => break,
            }
        }
    });
}

fn spawn_midi_pump(midi: Arc<Mutex<Midi>>, token: CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Ok(mut m) = midi.lock() {
                        while let Some((target_device, msg)) = m.send_queue.dequeue() {
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
                _ = token.cancelled() => break,
            }
        }
    });
}

fn perform_preflight_scan(supervision_midi: &Arc<Mutex<Midi>>) {
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
    if let Ok(mut m) = supervision_midi.lock() {
        m.available_in_ports = in_ports;
        m.available_out_ports = out_ports;
    }
}

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
        if let Some(conn) = connect_device_input(midi_state.clone(), device_name, tx.clone()) {
            input_connections.push(conn);
        }

        // --- OUTPUT ---
        connect_device_output(midi_state.clone(), device_name);
    }

    Ok(input_connections)
}

fn connect_device_input(
    midi_state: Arc<Mutex<Midi>>,
    device_name: &str,
    tx: mpsc::Sender<MackieEvent>,
) -> Option<MidiInputConnection<()>> {
    let midi_in = match MidiInput::new(&format!("Bridge In {}", device_name)) {
        Ok(m) => m,
        Err(e) => {
            error!(
                "Failed to create MIDI input client for {}: {}",
                device_name, e
            );
            return None;
        }
    };

    if let Some(in_port) = midi_in.ports().into_iter().find(|p| {
        midi_in
            .port_name(p)
            .ok()
            .map(|name| clean_midi_name(&name))
            .as_deref()
            == Some(device_name)
    }) {
        let d_name_log = device_name.to_string();
        let d_name_event = d_name_log.clone();

        match midi_in.connect(
            &in_port,
            "bridge-in-conn",
            move |_, msg, _| {
                // Log Input
                if let Ok(m) = midi_state.lock() {
                    let _ = m.log_sender.try_send(eos_midi_bridge::LogEntry {
                        time: std::time::Instant::now(),
                        source: eos_midi_bridge::LogSource::MidiIn,
                        content: format!("{} <- {:02X?}", d_name_log, msg),
                    });
                }

                if let Err(e) = tx.blocking_send(MackieEvent::MidiIn {
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
                info!("Connected Input: {}", device_name);
                Some(conn)
            }
            Err(e) => {
                error!("Failed to connect Input for {}: {}", device_name, e);
                None
            }
        }
    } else {
        warn!("MIDI Input device '{}' not found", device_name);
        None
    }
}

fn connect_device_output(midi_state: Arc<Mutex<Midi>>, device_name: &str) {
    let midi_out = match MidiOutput::new(&format!("Bridge Out {}", device_name)) {
        Ok(m) => m,
        Err(e) => {
            error!(
                "Failed to create MIDI output client for {}: {}",
                device_name, e
            );
            return;
        }
    };

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
                if let Ok(m) = midi_state.lock()
                    && let Some(profile) = m.device_profiles.get(device_name)
                {
                    for msg in &profile.startup_messages {
                        let _ = conn.send(msg);
                    }
                }

                if let Ok(mut m) = midi_state.lock() {
                    m.device_connections.insert(device_name.to_string(), conn);
                }
            }
            Err(e) => error!("Failed to connect Output for {}: {}", device_name, e),
        }
    } else {
        warn!("MIDI Output device '{}' not found", device_name);
    }
}

fn load_icon() -> anyhow::Result<egui::IconData> {
    let icon_bytes = include_bytes!("../assets/icon.png");
    let image = image::load_from_memory(icon_bytes)
        .context("Failed to decode icon PNG")?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

use crate::config::{float_to_pitch_bend, Config, MidiEventType};
use deunicode::deunicode;
use iced::futures::SinkExt;
use midir::{MidiInput, MidiOutput, MidiOutputConnection};
use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum BridgeEvent {
    None,
    Log(String),
    FaderUpdate(u8, f32),
    LabelUpdate(u8, String),
    MidiCaptured(MidiEventType, u8, [u8; 3]),
    ConnectionHeartbeat,
}

/// Full structure to manage fader state
#[derive(Debug, Clone, Copy)]
struct FaderState {
    is_touched: bool,
    last_touch_change: Instant,
    last_sent_value: Option<f32>,
    last_sent_time: Instant,
}

impl Default for FaderState {
    fn default() -> Self {
        Self {
            is_touched: false,
            last_touch_change: Instant::now(),
            last_sent_value: None,
            last_sent_time: Instant::now(),
        }
    }
}

/// Sends MCU Sysex commands to update the iCon D2 LCD scribble strips
fn send_mcu_label(conn: &mut MidiOutputConnection, fader_idx: u8, label: &str) {
    // MCU Sysex Header for iCon/Mackie Display
    let mut sysex = vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x12];

    // Calculate character offset (7 chars per fader)
    let offset = (fader_idx.saturating_sub(1)) * 7;
    sysex.push(offset);

    // Format: Center-aligned within 7 characters
    let display_text = format!("{: ^7}", label);
    // Take exactly 7 bytes to avoid overlapping into the next fader's space
    let truncated = &display_text.as_bytes()[..7.min(display_text.len())];

    sysex.extend_from_slice(truncated);
    sysex.push(0xF7);

    if let Err(e) = conn.send(&sysex) {
        eprintln!("⚠️ Failed to send MCU label: {}", e);
    }
}

pub fn clear_mcu_display(conn: &mut midir::MidiOutputConnection) {
    // Standard Mackie LCD Header (0x12 = LCD command)
    let mut sysex = vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x12];
    sysex.push(0x00); // Start at the first character

    // 56 spaces to clear all 8 fader segments (8 faders * 7 chars)
    let spaces = " ".repeat(56);
    sysex.extend_from_slice(spaces.as_bytes());

    sysex.push(0xF7); // End of Sysex

    if let Err(e) = conn.send(&sysex) {
        eprintln!("⚠️ Failed to clear MCU display: {}", e);
    }
}

pub fn bridge_subscription(
    in_name: String,
    out_name: String,
    cfg: Arc<Config>,
) -> iced::Subscription<BridgeEvent> {
    iced::subscription::channel(
        std::any::TypeId::of::<()>(),
        100,
        move |mut output| async move {
            // --- MIDI Initialization with error handling ---
            let midi_in = match MidiInput::new("Eos-Bridge-In") {
                Ok(m) => m,
                Err(e) => {
                    let _ = output
                        .send(BridgeEvent::Log(format!("❌ MIDI Input Error: {}", e)))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };

            let midi_out = match MidiOutput::new("Eos-Bridge-Out") {
                Ok(m) => m,
                Err(e) => {
                    let _ = output
                        .send(BridgeEvent::Log(format!("❌ MIDI Output Error: {}", e)))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };

            // Search for MIDI Input port
            let in_p = match midi_in
                .ports()
                .into_iter()
                .find(|p| midi_in.port_name(p).unwrap_or_default() == in_name)
            {
                Some(port) => port,
                None => {
                    let _ = output
                        .send(BridgeEvent::Log(format!(
                            "❌ MIDI Input port '{}' not found",
                            in_name
                        )))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };

            // Search for MIDI Output port
            let out_p = match midi_out
                .ports()
                .into_iter()
                .find(|p| midi_out.port_name(p).unwrap_or_default() == out_name)
            {
                Some(port) => port,
                None => {
                    let _ = output
                        .send(BridgeEvent::Log(format!(
                            "❌ MIDI Output port '{}' not found",
                            out_name
                        )))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };

            let _ = output
                .send(BridgeEvent::Log("✅ MIDI Ports connected".to_string()))
                .await;

            // --- Network Initialization ---
            let eos_addr = format!("{}:{}", cfg.eos_ip, cfg.eos_port);

            let send_socket = match std::net::UdpSocket::bind("0.0.0.0:0") {
                Ok(s) => s,
                Err(e) => {
                    let _ = output
                        .send(BridgeEvent::Log(format!("❌ UDP Send Socket Error: {}", e)))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };

            let recv_socket = match UdpSocket::bind(format!("0.0.0.0:{}", cfg.listen_port)).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = output
                        .send(BridgeEvent::Log(format!(
                            "❌ UDP Recv Socket Error (port {}): {}",
                            cfg.listen_port, e
                        )))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };

            let _ = output
                .send(BridgeEvent::Log(format!(
                    "✅ Listening for OSC on port {}",
                    cfg.listen_port
                )))
                .await;

            // --- Sync Task: Request current fader config from Eos ---
            let hb_socket = match send_socket.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    let _ = output
                        .send(BridgeEvent::Log(format!("❌ Socket clone error: {}", e)))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };

            let hb_addr = eos_addr.clone();
            let mut hb_output = output.clone();

            tokio::spawn(async move {
                // Initial sync
                let init_msg = OscMessage {
                    addr: "/eos/fader/1/config/10".into(),
                    args: vec![],
                };
                if let Ok(buf) = encoder::encode(&OscPacket::Message(init_msg)) {
                    if let Err(e) = hb_socket.send_to(&buf, &hb_addr) {
                        let _ = hb_output
                            .send(BridgeEvent::Log(format!("⚠️ Initial sync error: {}", e)))
                            .await;
                    } else {
                        let _ = hb_output
                            .send(BridgeEvent::Log("📡 Initial sync sent to Eos".to_string()))
                            .await;
                    }
                }

                loop {
                    // Ping every 5 seconds to keep the UI "Green"
                    sleep(Duration::from_secs(5)).await;
                    let ping = OscMessage {
                        addr: "/eos/ping".into(),
                        args: vec![OscType::String("BridgeSync".into())],
                    };
                    if let Ok(buf) = encoder::encode(&OscPacket::Message(ping)) {
                        let _ = hb_socket.send_to(&buf, &hb_addr);
                    }
                }
            });

            // --- Fader state with full structure ---
            let touched_faders = Arc::new(std::sync::Mutex::new([FaderState::default(); 13]));
            let touched_faders_cb = touched_faders.clone();
            let mut midi_tx = output.clone();
            let tx_sock = match send_socket.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    let _ = output
                        .send(BridgeEvent::Log(format!("❌ TX socket clone error: {}", e)))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };
            let tx_addr = eos_addr.clone();
            let cfg_midi = cfg.clone();

            // --- OSC Rx Loop (Eos Feedback) ---
            let out_conn = match midi_out.connect(&out_p, "write") {
                Ok(conn) => conn,
                Err(e) => {
                    let _ = output
                        .send(BridgeEvent::Log(format!(
                            "❌ MIDI Output connection error: {}",
                            e
                        )))
                        .await;
                    loop {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
                }
            };
            let shared_midi_out = Arc::new(std::sync::Mutex::new(out_conn));
            let midi_out_for_callback = shared_midi_out.clone();

            // --- MIDI Input to OSC Out ---
            let _conn_in = match midi_in.connect(
                &in_p,
                "read",
                move |_, msg, _| {
                    if msg.len() < 3 {
                        return;
                    }
                    let status = msg[0] & 0xF0;

                    // Handle Fader Touch for Motor Safety
                    if status == 0x90 || status == 0x80 {
                        let note = msg[1];
                        let is_touch = status == 0x90 && msg[2] > 0;

                        if let Ok(mut touched) = touched_faders_cb.lock() {
                            // Notes 104-111 are fader touches 1-8 on Platform M+
                            // Note 112 is fader 9 (Master)
                            let idx = if note >= 104 && note <= 111 {
                                (note - 103) as usize
                            } else if note == 112 {
                                9
                            } else {
                                0
                            };

                            if idx > 0 && idx < touched.len() {
                                touched[idx].is_touched = is_touch;
                                touched[idx].last_touch_change = Instant::now();

                                // Debugging log
                                let action = if is_touch { "touched" } else { "released" };
                                let _ = midi_tx.try_send(BridgeEvent::Log(format!(
                                    "🎹 Fader {} {}",
                                    idx, action
                                )));
                            }
                        }
                    }

                    let (etype, dnum) = match status {
                        0xE0 => (MidiEventType::PitchBend, (msg[0] & 0x0F) + 1),
                        0x90 => (MidiEventType::NoteOn, msg[1]),
                        0xB0 => (MidiEventType::ControlChange, msg[1]),
                        _ => return,
                    };

                    // Optional: Send event to UI for monitoring
                    let _ = midi_tx.try_send(BridgeEvent::MidiCaptured(
                        etype.clone(),
                        dnum,
                        [msg[0], msg[1], msg[2]],
                    ));

                    if let Some(m) = cfg_midi
                        .mappings
                        .iter()
                        .find(|map| map.event_type == etype && map.data_number == dnum)
                    {
                        let mut args = vec![];
                        let value_opt: Option<f32> = match etype {
                            MidiEventType::PitchBend => {
                                let val =
                                    ((msg[2] as u16) * 128 + (msg[1] as u16)) as f32 / 16383.0;
                                args.push(OscType::Float(val));
                                if let Ok(mut conn) = midi_out_for_callback.lock() {
                                    let _ = conn.send(&[msg[0], msg[1], msg[2]]);
                                };
                                Some(val)
                            }
                            MidiEventType::ControlChange => {
                                let val = msg[2] as f32 / 127.0;
                                args.push(OscType::Float(val));
                                Some(val)
                            }
                            MidiEventType::NoteOn => {
                                if let Some(v) = m.fixed_osc_value {
                                    args.push(OscType::Float(v));
                                }
                                None
                            }
                        };

                        // ✅ NEW: Record the value sent
                        if let Some(val) = value_opt {
                            let idx = dnum as usize;
                            if idx > 0 && idx < 13 {
                                if let Ok(mut touched) = touched_faders_cb.lock() {
                                    touched[idx].last_sent_value = Some(val);
                                    touched[idx].last_sent_time = Instant::now();
                                }
                            }
                        }

                        let p = OscPacket::Message(OscMessage {
                            addr: m.osc_address.clone(),
                            args,
                        });

                        if let Ok(b) = encoder::encode(&p) {
                            if let Err(e) = tx_sock.send_to(&b, &tx_addr) {
                                let _ = midi_tx.try_send(BridgeEvent::Log(format!(
                                    "⚠️ OSC Send Error: {}",
                                    e
                                )));
                            }
                        }
                    }
                },
                (),
            ) {
                Ok(conn) => conn,
                Err(e) => {
                    let _ = output
                        .send(BridgeEvent::Log(format!(
                            "❌ MIDI Input connection error: {}",
                            e
                        )))
                        .await;
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                    }
                }
            };

            let _ = output
                .send(BridgeEvent::Log("✅ MIDI→OSC Bridge active".to_string()))
                .await;

            let mut buf = [0u8; 4096];
            loop {
                match recv_socket.recv_from(&mut buf).await {
                    Ok((len, _)) => {
                        let _ = output.send(BridgeEvent::ConnectionHeartbeat).await;

                        // decode_udp is the standard for network-received OSC
                        match decoder::decode_udp(&buf[..len]) {
                            Ok((_, packet)) => {
                                let mut midi_result = None;
                                {
                                    if let Ok(_guard) = shared_midi_out.lock() {
                                        midi_result = Some(packet);
                                    }
                                }
                                if let Some(p) = midi_result {
                                    process_packet(
                                        p,
                                        shared_midi_out.clone(),
                                        &mut output,
                                        &cfg,
                                        &touched_faders,
                                    )
                                    .await;
                                }
                            }
                            Err(e) => {
                                let _ = output
                                    .send(BridgeEvent::Log(format!("⚠️ OSC Decoding Error: {}", e)))
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = output
                            .send(BridgeEvent::Log(format!("⚠️ UDP Reception Error: {}", e)))
                            .await;
                    }
                }
            }
        },
    )
}

/// Recursive helper to process OSC Bundles and Messages
#[async_recursion::async_recursion]
async fn process_packet(
    packet: OscPacket,
    midi_out: Arc<std::sync::Mutex<MidiOutputConnection>>,
    output_channel: &mut iced::futures::channel::mpsc::Sender<BridgeEvent>,
    cfg: &Arc<Config>,
    touched: &Arc<std::sync::Mutex<[FaderState; 13]>>,
) {
    match packet {
        OscPacket::Message(msg) => {
            // Listen for Eos Ping Response or any "out" message
            if msg.addr.starts_with("/eos/out/ping") || msg.addr.starts_with("/eos/out") {
                let _ = output_channel.send(BridgeEvent::ConnectionHeartbeat).await;
            }

            // Handle Fader Labels
            if msg.addr.contains("/name") {
                let parts: Vec<&str> = msg.addr.split('/').collect();

                // Expected format: /eos/out/fader/{page}/name/{idx}
                // Example: /eos/out/fader/1/name/1
                if parts.len() >= 6 && parts[1] == "eos" && parts[2] == "out" && parts[3] == "fader"
                {
                    if let (Some(idx_str), Some(OscType::String(name))) =
                        (parts.get(5), msg.args.get(0))
                    {
                        if let Ok(idx) = idx_str.parse::<u8>() {
                            // ✅ FIX: Include fader 9 (Master)
                            if idx >= 1 && idx <= 9 {
                                // Send to UI
                                let _ = output_channel
                                    .send(BridgeEvent::LabelUpdate(idx, name.clone()))
                                    .await;

                                // Send to iCon D2 Display (only faders 1-8 have a screen)
                                if idx <= 8 {
                                    let words: Vec<&str> = name.split_whitespace().collect();
                                    let mcu_name = if words.len() > 2 {
                                        words[2..].join(" ")
                                    } else {
                                        name.clone()
                                    };
                                    // Remove accents
                                    let ascii_name = deunicode(&mcu_name);
                                    {
                                        if let Ok(mut conn) = midi_out.lock() {
                                            send_mcu_label(&mut *conn, idx, &ascii_name);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Handle Motorized Fader Feedback
            else if let Some(m) = cfg.mappings.iter().find(|map| msg.addr == map.osc_address) {
                if let Some(OscType::Float(f)) = msg.args.get(0) {
                    let idx = m.data_number;

                    // ✅ FIX: Include fader 9 (Master)
                    if idx >= 1 && idx <= 9 {
                        // ✅ NEW: Intelligent feedback verification
                        let should_apply_feedback = if let Ok(state_array) = touched.lock() {
                            let state = state_array[idx as usize];

                            // Do not move if fader is currently touched
                            if state.is_touched {
                                false
                            }
                            // Wait 300ms after release (anti-jitter)
                            else if state.last_touch_change.elapsed() < Duration::from_millis(300)
                            {
                                false
                            }
                            // ✅ BUG FIX: Filter obsolete feedbacks
                            else if let Some(sent_val) = state.last_sent_value {
                                // If we sent a value less than 800ms ago
                                if state.last_sent_time.elapsed() < Duration::from_millis(800) {
                                    // Accept only if the difference is < 2% (avoids "ghost" movements)
                                    let diff = (*f - sent_val).abs();
                                    diff < 0.02
                                } else {
                                    // After 800ms, accept all feedbacks
                                    true
                                }
                            } else {
                                // No value sent recently, accept
                                true
                            }
                        } else {
                            false
                        };

                        if should_apply_feedback {
                            let pb = float_to_pitch_bend(*f);
                            let midi_channel = idx - 1; // MIDI channels 0-8 for faders 1-9

                            let mut error_msg: Option<String> = None;
                            let mut send_success = false;
                            {
                                if let Ok(mut midi_out_lock) = midi_out.lock() {
                                    if let Err(e) = midi_out_lock.send(&[
                                        0xE0 | midi_channel,
                                        (pb & 0x7F) as u8,
                                        (pb >> 7) as u8,
                                    ]) {
                                        error_msg =
                                            Some(format!("⚠️ Fader motor error {}: {}", idx, e));
                                    } else {
                                        send_success = true;
                                    }
                                }
                            }
                            if let Some(log_msg) = error_msg {
                                let _ = output_channel.send(BridgeEvent::Log(log_msg)).await;
                            }
                            if send_success {
                                let _ =
                                    output_channel.send(BridgeEvent::FaderUpdate(idx, *f)).await;
                            }
                        } else {
                            // Debugging log
                            let _ = output_channel
                                .send(BridgeEvent::Log(format!(
                                    "🚫 Fader feedback {} ignored (protection)",
                                    idx
                                )))
                                .await;
                        }
                    }
                }
            }
        }
        OscPacket::Bundle(bundle) => {
            for content in bundle.content {
                process_packet(content, midi_out.clone(), output_channel, cfg, touched).await;
            }
        }
    }
}

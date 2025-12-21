use crate::config::{float_to_pitch_bend, Config, MidiEventType};
use iced::futures::SinkExt;
use midir::{MidiInput, MidiOutput, MidiOutputConnection};
use rosc::{decoder, encoder, OscMessage, OscPacket, OscType};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum BridgeEvent {
    Log(String),
    FaderUpdate(u8, f32),
    LabelUpdate(u8, String),
    MidiCaptured(MidiEventType, u8, [u8; 3]),
    ConnectionHeartbeat,
}

/// Sends MCU Sysex commands to update the iCon D2 LCD scribble strips
fn send_mcu_label(conn: &mut MidiOutputConnection, fader_idx: u8, label: &str) {
    // MCU Sysex Header for iCon/Mackie Display
    let mut sysex = vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x12];

    // Calculate character offset (7 chars per fader)
    let offset = (fader_idx.saturating_sub(1)) * 7;
    sysex.push(offset);

    // Clean up Eos string (e.g., "Fader 1: Vox" -> "Vox")
    let clean = label.split(':').last().unwrap_or(label).trim();
    let display_text = format!("{: ^7}", clean); // Center in 7 spaces

    let bytes = display_text.as_bytes();
    sysex.extend_from_slice(&bytes[0..7.min(bytes.len())]);

    sysex.push(0xF7);
    let _ = conn.send(&sysex);
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
            let midi_in = MidiInput::new("Eos-Bridge-In").unwrap();
            let midi_out = MidiOutput::new("Eos-Bridge-Out").unwrap();

            let in_p = midi_in
                .ports()
                .into_iter()
                .find(|p| midi_in.port_name(p).unwrap_or_default() == in_name)
                .expect("MIDI In Port Missing");
            let out_p = midi_out
                .ports()
                .into_iter()
                .find(|p| midi_out.port_name(p).unwrap_or_default() == out_name)
                .expect("MIDI Out Port Missing");

            let eos_addr = format!("{}:{}", cfg.eos_ip, cfg.eos_port);
            let send_socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
            let recv_socket = UdpSocket::bind(format!("0.0.0.0:{}", cfg.listen_port))
                .await
                .unwrap();

            // --- Sync Task: Request current fader config from Eos ---
            let hb_socket = send_socket.try_clone().unwrap();
            let hb_addr = eos_addr.clone();
            tokio::spawn(async move {
                // Initial sync
                let init_msg = OscMessage {
                    addr: "/eos/fader/1/config/10".into(),
                    args: vec![],
                };
                if let Ok(buf) = encoder::encode(&OscPacket::Message(init_msg)) {
                    let _ = hb_socket.send_to(&buf, &hb_addr);
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

            // --- MIDI Input to OSC Out ---
            let touched_faders = Arc::new(std::sync::Mutex::new([false; 13]));
            let touched_faders_cb = touched_faders.clone();
            let mut midi_tx = output.clone();
            let tx_sock = send_socket.try_clone().unwrap();
            let tx_addr = eos_addr.clone();
            let cfg_midi = cfg.clone();

            let _conn_in = midi_in
                .connect(
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
                                // Notes 104-111 are fader touches on Platform M+
                                if note >= 104 && note <= 111 {
                                    touched[(note - 103) as usize] = is_touch;
                                } else if note == 112 {
                                    touched[9] = is_touch;
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
                            match etype {
                                MidiEventType::PitchBend => {
                                    let val =
                                        ((msg[2] as u16) * 128 + (msg[1] as u16)) as f32 / 16383.0;
                                    args.push(OscType::Float(val));
                                }
                                MidiEventType::ControlChange => {
                                    args.push(OscType::Float(msg[2] as f32 / 127.0))
                                }
                                MidiEventType::NoteOn => {
                                    if let Some(v) = m.fixed_osc_value {
                                        args.push(OscType::Float(v));
                                    }
                                }
                            }
                            let p = OscPacket::Message(OscMessage {
                                addr: m.osc_address.clone(),
                                args,
                            });
                            if let Ok(b) = encoder::encode(&p) {
                                let _ = tx_sock.send_to(&b, &tx_addr);
                            }
                        }
                    },
                    (),
                )
                .unwrap();

            // --- OSC Rx Loop (Eos Feedback) ---
            let mut out_conn = midi_out.connect(&out_p, "write").unwrap();
            let mut buf = [0u8; 4096];
            loop {
                if let Ok((len, _)) = recv_socket.recv_from(&mut buf).await {
                    let _ = output.send(BridgeEvent::ConnectionHeartbeat).await;

                    // decode_udp is the standard for network-received OSC
                    if let Ok((_, packet)) = decoder::decode_udp(&buf[..len]) {
                        process_packet(packet, &mut out_conn, &mut output, &cfg, &touched_faders)
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
    midi_out: &mut MidiOutputConnection,
    output_channel: &mut iced::futures::channel::mpsc::Sender<BridgeEvent>,
    cfg: &Arc<Config>,
    touched: &Arc<std::sync::Mutex<[bool; 13]>>,
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
                if let (Some(idx_str), Some(OscType::String(name))) =
                    (parts.get(4), msg.args.get(0))
                {
                    if let Ok(idx) = idx_str.parse::<u8>() {
                        let _ = output_channel
                            .send(BridgeEvent::LabelUpdate(idx, name.clone()))
                            .await;
                        send_mcu_label(midi_out, idx, name);
                    }
                }
            }
            // Handle Motorized Fader Feedback
            else if let Some(m) = cfg
                .mappings
                .iter()
                .find(|map| msg.addr.starts_with(&map.osc_address))
            {
                if let Some(OscType::Float(f)) = msg.args.get(0) {
                    let idx = m.data_number;
                    // Only move the motor if the user isn't physically touching it
                    let is_touched = if let Ok(t) = touched.lock() {
                        t[idx as usize]
                    } else {
                        false
                    };

                    if !is_touched {
                        let pb = float_to_pitch_bend(*f);
                        let _ =
                            midi_out.send(&[0xE0 | (idx - 1), (pb & 0x7F) as u8, (pb >> 7) as u8]);
                        let _ = output_channel.send(BridgeEvent::FaderUpdate(idx, *f)).await;
                    }
                }
            }
        }
        OscPacket::Bundle(bundle) => {
            for content in bundle.content {
                process_packet(content, midi_out, output_channel, cfg, touched).await;
            }
        }
    }
}

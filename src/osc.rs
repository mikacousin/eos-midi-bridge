use crate::{CrossfadeState, midi::Midi, strip_accents};
use log::{debug, info};
use rosc::{OscMessage, OscPacket, OscType};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct OscClient {
    pub host: String,
    pub port: u16,
    pub socket: Arc<UdpSocket>,
}

impl OscClient {
    pub async fn new(host: &str, port: u16) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Self {
            host: host.to_string(),
            port,
            socket: Arc::new(socket),
        })
    }

    pub async fn send(&self, path: &str, args: Vec<OscType>) -> anyhow::Result<()> {
        let msg = OscMessage {
            addr: path.to_string(),
            args,
        };
        let packet = OscPacket::Message(msg);
        let buf = rosc::encoder::encode(&packet)?;
        self.socket
            .send_to(&buf, format!("{}:{}", self.host, self.port))
            .await?;
        Ok(())
    }
}

pub struct OscServer {
    pub port: u16,
}

impl OscServer {
    pub async fn start(
        self,
        midi: Arc<Mutex<Midi>>,
        token: CancellationToken,
    ) -> anyhow::Result<()> {
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", self.port)).await?;
        let mut buf = [0u8; rosc::decoder::MTU];
        info!("Start OSC Server on port {}...", self.port);

        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                result = socket.recv_from(&mut buf) => {
                    if let Ok((size, _)) = result {
                        if let Ok((_, packet)) = rosc::decoder::decode_udp(&buf[..size]) {
                            self.handle_packet(packet, &midi).await;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn handle_packet(&self, packet: OscPacket, midi: &Arc<Mutex<Midi>>) {
        if let OscPacket::Message(msg) = packet {
            debug!("Received OSC from Eos: {} args: {:?}", msg.addr, msg.args);

            let parts: Vec<&str> = msg.addr.split('/').collect();

            // Handle Generic OSC Triggers from Profile
            let mut handled = false;
            // Only check for generic triggers if it looks like a fader command to avoid overhead
            if msg.addr.starts_with("/eos/fader/1/") || msg.addr.starts_with("/eos/out/event/cue/")
            {
                let mappings: Vec<(usize, crate::controller::Mapping)> = {
                    let m = midi.lock().unwrap();
                    m.profile.mappings.iter().enumerate()
                        .filter(|(_, map)| matches!(&map.trigger, crate::controller::Trigger::Osc { addr } if addr == &msg.addr))
                        .map(|(i, map)| (i, map.clone()))
                        .collect()
                };

                for (idx, mapping) in mappings {
                    if let Some(OscType::Float(val)) = msg.args.first() {
                        let midi_val = (val * 16383.0).round() as u16;
                        let d1 = (midi_val & 0x7F) as u8;
                        let d2 = ((midi_val >> 7) & 0x7F) as u8;

                        // Log Trigger Activity
                        {
                            let mut m = midi.lock().unwrap();
                            m.activity_log.push(crate::ActivityEvent {
                                mapping_idx: idx,
                                part: crate::ActivityPart::Trigger,
                                value: format!("OSC {:.2}", val),
                            });
                        }

                        crate::midi::execute_mapping(midi.clone(), &mapping, idx, d1, d2).await;
                        handled = true;

                        // Update state if we triggered a Stop or Go action
                        match mapping.action {
                            crate::controller::LogicalAction::Stop => {
                                let mut m = midi.lock().unwrap();
                                m.set_crossfade_state(crate::CrossfadeState::Pause, false);
                            }
                            crate::controller::LogicalAction::Go => {
                                let mut m = midi.lock().unwrap();
                                m.set_crossfade_state(crate::CrossfadeState::Go, false);
                            }
                            _ => {}
                        }
                    }
                }
            }

            // FALLBACK: Dynamic Event Handling (regardless of mapping match)
            // This is required because EOS events include the specific cue number,
            // which changes, making static mapping impossible for transport state tracking.
            if msg.addr.starts_with("/eos/out/event/cue/") {
                if msg.addr.ends_with("/resume") {
                    let mut m = midi.lock().unwrap();
                    // FORCE UPDATE because Resume is definitive
                    m.set_crossfade_state(crate::CrossfadeState::Go, true);
                } else if msg.addr.ends_with("/stop") {
                    let mut m = midi.lock().unwrap();
                    // FORCE UPDATE because Stop is definitive
                    m.set_crossfade_state(crate::CrossfadeState::Pause, true);
                }
            }

            // Handle Fader Labels: /eos/out/fader/1/{index}/name
            if msg.addr.starts_with("/eos/out/fader/1/") && msg.addr.ends_with("/name") {
                if let (Some(f_str), Some(OscType::String(text))) = (parts.get(5), msg.args.first())
                {
                    if let Ok(f_num) = f_str.parse::<usize>() {
                        {
                            let mut m = midi.lock().unwrap();
                            let eos_bank_size = m.profile.eos_bank_size;
                            if f_num > 0 && f_num <= eos_bank_size {
                                let mut text = text.clone();
                                if text.starts_with("S") {
                                    let split: Vec<&str> = text.split_whitespace().collect();
                                    if split.len() > 2 {
                                        text = split[2..].join(" ");
                                    }
                                }
                                m.fader_names[f_num - 1] = text;
                            }
                        }

                        // Check if we should ignore this update to keep the Page number visible
                        {
                            let m = midi.lock().unwrap();
                            let lockout = m
                                .page_display_time
                                .saturating_sub(Duration::from_millis(100));
                            if m.last_page_change.elapsed() < lockout {
                                return;
                            }
                        }

                        // Send to LCD strip if physically available
                        let should_display = {
                            let m = midi.lock().unwrap();
                            if let Some(ref visible) = m.profile.display.visible_faders {
                                visible.contains(&f_num)
                            } else {
                                // Fallback to "all faders that fit on screen have displays"
                                let lcd_segments =
                                    m.profile.display.line_length / m.profile.display.strip_width;
                                f_num <= lcd_segments
                            }
                        };

                        if should_display {
                            let clean_text = {
                                let m = midi.lock().unwrap();
                                m.fader_names[f_num - 1].clone()
                            };

                            let clean_text_stripped = strip_accents(&clean_text);
                            let line0: String = clean_text_stripped.chars().take(6).collect();
                            let line1: String =
                                clean_text_stripped.chars().skip(6).take(6).collect();

                            let m = midi.lock().unwrap();
                            m.send_to_strip(&line0, 0, f_num - 1);
                            m.send_to_strip(&line1, 1, f_num - 1);
                        }
                    }
                }
            }

            // Move Motorized Faders: /eos/fader/1/{index}

            // Move Motorized Faders: /eos/fader/1/{index}
            if !handled && msg.addr.starts_with("/eos/fader/1/") {
                // For "/eos/fader/1/5", parts are ["", "eos", "fader", "1", "5"] -> index 4
                if let (Some(f_str), Some(OscType::Float(val))) = (parts.get(4), msg.args.first()) {
                    if let Ok(f_num) = f_str.parse::<usize>() {
                        let mut m = midi.lock().unwrap();
                        let eos_bank_size = m.profile.eos_bank_size;
                        if f_num > 0 && f_num <= eos_bank_size {
                            let pitch = (val * 16383.0).round() as i16 - 8192;
                            // Update physical pitchwheel if a mapping exists for this fader index
                            m.enqueue_pitchwheel((f_num - 1) as u8, pitch);
                            m.fader_levels[f_num - 1] = *val;
                        }
                    }
                }
            }

            // INDEPENDENT CHECK: Cue Events (Update State even if mapped)
            if msg.addr == "/eos/out/event/cue/1/0/stop" {
                let mut m = midi.lock().unwrap();
                if m.crossfade_state == CrossfadeState::Go
                    || m.crossfade_state == CrossfadeState::GoBack
                {
                    m.crossfade_state = CrossfadeState::Pause;
                    if let Some((note, chan, _)) = m
                        .profile
                        .get_midi_output_for_action(crate::controller::LogicalAction::Stop)
                    {
                        m.send_queue.enqueue(vec![0x90 | chan, note, 127]);
                    }
                }
            } else if msg.addr == "/eos/out/event/cue/1/0/resume" {
                let mut m = midi.lock().unwrap();
                if m.crossfade_state == CrossfadeState::Pause {
                    m.crossfade_state = CrossfadeState::Go;
                    if let Some((s_note, s_chan, _)) = m
                        .profile
                        .get_midi_output_for_action(crate::controller::LogicalAction::Stop)
                    {
                        m.send_queue.enqueue(vec![0x90 | s_chan, s_note, 0]);
                    }
                    if let Some((g_note, g_chan, _)) = m
                        .profile
                        .get_midi_output_for_action(crate::controller::LogicalAction::Go)
                    {
                        m.send_queue.enqueue(vec![0x90 | g_chan, g_note, 127]);
                    }
                }
            } else if msg.addr == "/eos/out/active/cue/text" {
                if let Some(OscType::String(text)) = msg.args.first() {
                    // Extract the cue number part
                    if let Some(new_cue_num) = crate::parse_cue_number(text) {
                        let mut m = midi.lock().unwrap();
                        let current_state = m.crossfade_state;

                        // Calculate intended direction based on cue change
                        if let Some(direction) = crate::resolve_crossfade_direction(
                            m.current_cue,
                            new_cue_num,
                            current_state,
                        ) {
                            m.intended_dir = direction;
                        }

                        m.current_cue = Some(new_cue_num);
                    }
                }
            } else if msg.addr == "/eos/out/ping" {
                let mut sync_needed = false;
                {
                    let mut m = midi.lock().unwrap();
                    m.last_osc_heartbeat = Some(std::time::Instant::now());
                    if m.needs_sync {
                        m.needs_sync = false;
                        sync_needed = true;
                    }
                }

                if sync_needed {
                    info!("Eos Pong received! Triggering initial sync...");
                    let client = {
                        let m = midi.lock().unwrap();
                        m.osc_client.clone()
                    };
                    let midi_clone = Arc::clone(midi);
                    tokio::spawn(async move {
                        let eos_bank_size = {
                            let m = midi_clone.lock().unwrap();
                            m.profile.eos_bank_size
                        };
                        let _ = client
                            .send(
                                &format!("/eos/user/1/fader/1/config/{}", eos_bank_size),
                                vec![],
                            )
                            .await;
                        let _ = client
                            .send("/eos/subscribe", vec![rosc::OscType::Int(1)])
                            .await;
                    });
                }
            } else if msg.addr.starts_with("/eos/out/active/cue") {
                if let Some(OscType::Float(progress)) = msg.args.get(0) {
                    let mut m = midi.lock().unwrap();

                    if *progress < 1.0 {
                        // Activate the intended direction if we are not already in it (or if overriding something other than Pause)
                        let intended = m.intended_dir;
                        let last = m.last_applied_dir;
                        let current = m.crossfade_state;

                        // Only apply change if direction is new OR if we are Inactive
                        // Respect Pause: If Paused, do not switch unless the intended direction actually changed (e.g. Go -> Back)
                        if intended != last {
                            m.set_crossfade_state(intended, false);
                            m.last_applied_dir = intended;
                        } else if current == CrossfadeState::Inactive {
                            // Starting a fade from Inactive
                            m.set_crossfade_state(intended, false);
                            m.last_applied_dir = intended;
                        }
                    } else {
                        if m.crossfade_state != CrossfadeState::Inactive {}
                        m.set_crossfade_state(CrossfadeState::Inactive, false);
                    }
                }
            }
        }
    }
}

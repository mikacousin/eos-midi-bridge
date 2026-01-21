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
            debug!(
                "DEBUG: Received OSC from Eos: {} args: {:?}",
                msg.addr, msg.args
            );

            let parts: Vec<&str> = msg.addr.split('/').collect();

            // Handle Fader Labels: /eos/out/fader/1/{index}/name
            if msg.addr.starts_with("/eos/out/fader/1/") && msg.addr.ends_with("/name") {
                if let (Some(f_str), Some(OscType::String(text))) = (parts.get(5), msg.args.first())
                {
                    if let Ok(f_num) = f_str.parse::<usize>() {
                        {
                            let mut m = midi.lock().unwrap();
                            let eos_bank_size = m.profile.eos_bank_size;
                            if f_num <= eos_bank_size {
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
                        let (lcd_segments, clean_text) = {
                            let m = midi.lock().unwrap();
                            let lcd_seg =
                                m.profile.display.line_length / m.profile.display.strip_width;
                            let t = m.fader_names[f_num - 1].clone();
                            (lcd_seg, t)
                        };

                        if f_num <= lcd_segments {
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
            else if msg.addr.starts_with("/eos/fader/1/") {
                // For "/eos/fader/1/5", parts are ["", "eos", "fader", "1", "5"] -> index 4
                if let (Some(f_str), Some(OscType::Float(val))) = (parts.get(4), msg.args.first()) {
                    if let Ok(f_num) = f_str.parse::<usize>() {
                        let mut m = midi.lock().unwrap();
                        let eos_bank_size = m.profile.eos_bank_size;
                        if f_num <= eos_bank_size {
                            let pitch = (val * 16383.0).round() as i16 - 8192;
                            // Update physical pitchwheel if a mapping exists for this fader index
                            m.enqueue_pitchwheel((f_num - 1) as u8, pitch);
                            m.fader_levels[f_num - 1] = *val;
                        }
                    }
                }
            } else if msg.addr == "/eos/out/event/cue/1/0/stop" {
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
                    if let Some(cue_part) = text
                        .split('/')
                        .nth(1)
                        .and_then(|s| s.split_whitespace().next())
                    {
                        let simplified_cue =
                            cue_part.split('/').take(2).collect::<Vec<_>>().join(".");
                        if let Ok(new_cue_num) = simplified_cue.parse::<f32>() {
                            let mut m = midi.lock().unwrap();

                            // Compare to know the direction if we were Inactive
                            if m.crossfade_state == CrossfadeState::Inactive
                                || m.crossfade_state == CrossfadeState::Pause
                            {
                                if let Some(old_cue) = m.current_cue {
                                    if new_cue_num > old_cue {
                                        m.crossfade_state = CrossfadeState::Go;
                                    } else if new_cue_num < old_cue {
                                        m.crossfade_state = CrossfadeState::GoBack;
                                    }
                                } else {
                                    m.crossfade_state = CrossfadeState::Go;
                                }
                            }

                            m.current_cue = Some(new_cue_num);
                        }
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
                        let go_fb = m
                            .profile
                            .get_midi_output_for_action(crate::controller::LogicalAction::Go);
                        let stop_fb = m
                            .profile
                            .get_midi_output_for_action(crate::controller::LogicalAction::Stop);

                        match m.crossfade_state {
                            CrossfadeState::Go => {
                                if let Some((n, c, v)) = go_fb {
                                    m.send_queue.enqueue(vec![0x90 | c, n, v]);
                                }
                                if let Some((n, c, _)) = stop_fb {
                                    m.send_queue.enqueue(vec![0x90 | c, n, 0]);
                                }
                            }
                            CrossfadeState::GoBack | CrossfadeState::Pause => {
                                if let Some((n, c, v)) = stop_fb {
                                    m.send_queue.enqueue(vec![0x90 | c, n, v]);
                                }
                                if let Some((n, c, _)) = go_fb {
                                    m.send_queue.enqueue(vec![0x90 | c, n, 0]);
                                }
                            }
                            CrossfadeState::Inactive => {}
                        }
                    } else {
                        let go_fb = m
                            .profile
                            .get_midi_output_for_action(crate::controller::LogicalAction::Go);
                        let stop_fb = m
                            .profile
                            .get_midi_output_for_action(crate::controller::LogicalAction::Stop);
                        m.crossfade_state = CrossfadeState::Inactive;
                        if let Some((n, c, _)) = go_fb {
                            m.send_queue.enqueue(vec![0x90 | c, n, 0]);
                        }
                        if let Some((n, c, _)) = stop_fb {
                            m.send_queue.enqueue(vec![0x90 | c, n, 0]);
                        }
                    }
                }
            }
        }
    }
}

use crate::{CrossfadeState, MackieEvent, Queue, osc::OscClient, strip_accents};
use midir::MidiOutputConnection;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;

pub struct Midi {
    pub osc_client: OscClient,
    pub profile: crate::controller::ControllerProfile,
    pub fader_page: u8,
    pub send_queue: Queue<Vec<u8>>,
    pub connections: Vec<MidiOutputConnection>,
    pub last_page_change: time::Instant,
    pub page_display_time: Duration,
    pub crossfade_state: CrossfadeState,
    pub current_cue: Option<f32>,
    pub fader_levels: Vec<f32>,
    pub fader_names: Vec<String>,
    pub connection_status: String,
    pub available_in_ports: Vec<String>,
    pub available_out_ports: Vec<String>,
    pub last_osc_heartbeat: Option<std::time::Instant>,
    pub needs_sync: bool,
    pub activity_log: Vec<crate::ActivityEvent>,
}

impl Midi {
    pub fn new(osc_client: OscClient, profile: crate::controller::ControllerProfile) -> Self {
        let bank_size = profile.eos_bank_size;
        Self {
            osc_client,
            profile,
            fader_page: 1,
            send_queue: Queue::new(),
            connections: Vec::new(),
            last_page_change: time::Instant::now() - Duration::from_secs(2),
            page_display_time: Duration::from_millis(500),
            crossfade_state: CrossfadeState::Inactive,
            current_cue: None,
            fader_levels: vec![0.0; bank_size],
            fader_names: vec![String::new(); bank_size],
            connection_status: "Disconnected".to_string(),
            available_in_ports: Vec::new(),
            available_out_ports: Vec::new(),
            last_osc_heartbeat: None,
            needs_sync: true,
            activity_log: Vec::new(),
        }
    }

    pub fn enqueue_sysex(&self, data: Vec<u8>) {
        self.send_queue.enqueue(data);
    }

    pub fn enqueue_pitchwheel(&self, fader_index: u8, value: i16) {
        // Find a mapping for FaderMove { index: fader_index } that has a MidiPitchwheel output
        for mapping in &self.profile.mappings {
            if let crate::controller::LogicalAction::FaderMove { index } = mapping.action {
                if index == fader_index as usize {
                    for output in &mapping.outputs {
                        if let crate::controller::Output::MidiPitchwheel { channel } = output {
                            let val = (value + 8192) as u16; // Convert pitch (-8192 to 8191) to 0-16383 range
                            let mut data = vec![0xE0 | channel];
                            data.push((val & 0x7F) as u8); // LSB
                            data.push(((val >> 7) & 0x7F) as u8); // MSB
                            self.send_queue.enqueue(data);
                            return;
                        }
                    }
                }
            }
        }
    }

    pub fn send_lcd(&self, text: &str, line: u8) {
        let text = strip_accents(text);
        if line as usize >= self.profile.display.line_offsets.len() {
            return;
        }
        let start = self.profile.display.line_offsets[line as usize];
        let mut data = vec![0xF0];
        data.extend(&self.profile.display.sysex_prefix);
        data.push(start);
        data.extend(text.bytes().take(self.profile.display.line_length));
        data.push(0xF7);
        self.enqueue_sysex(data);
    }

    pub fn send_to_strip(&self, text: &str, line: u8, strip: usize) {
        if line as usize >= self.profile.display.line_offsets.len() {
            return;
        }
        let start = self.profile.display.line_offsets[line as usize]
            + (strip * self.profile.display.strip_width) as u8;

        let width = self.profile.display.strip_width;
        let text = format!("{:<width$}|", strip_accents(text), width = width - 1);
        let mut data = vec![0xF0];
        data.extend(&self.profile.display.sysex_prefix);
        data.push(start);
        data.extend(text.bytes().take(width));
        data.push(0xF7);
        self.enqueue_sysex(data);
    }

    pub fn show_page_number(&self, page: u8) {
        // Clear line
        let len = self.profile.display.line_length;
        self.send_lcd(&" ".repeat(len), 0);

        let page_text = format!("Page {}", page);
        // Center the text
        let centered_text = format!("{:^len$}", page_text, len = len);

        if self.profile.display.line_offsets.len() > 1 {
            let start = self.profile.display.line_offsets[1];
            let mut data = vec![0xF0];
            data.extend(&self.profile.display.sysex_prefix);
            data.push(start);
            data.extend(centered_text.bytes().take(len));
            data.push(0xF7);
            self.enqueue_sysex(data);
        }
    }

    pub fn controller_reset(&self) {
        let len = self.profile.display.line_length;
        for line in 0..self.profile.display.line_offsets.len() {
            self.send_lcd(&" ".repeat(len), line as u8);
        }

        // Reset all physical faders found in mappings
        for mapping in &self.profile.mappings {
            if let crate::controller::LogicalAction::FaderMove { index } = mapping.action {
                self.enqueue_pitchwheel(index as u8, -8192);
            }
        }

        for note in 0..128 {
            self.send_queue.enqueue(vec![0x90, note, 0]);
        }
    }
}

pub async fn handle_event_logic(event: MackieEvent, midi: Arc<Mutex<Midi>>) {
    match event {
        MackieEvent::MidiIn(msg) => {
            if msg.len() < 3 {
                return;
            }
            let (status, d1, d2) = (msg[0], msg[1], msg[2]);
            let (msg_type, chan) = (status & 0xF0, status & 0x0F);

            // Get the client and profile without holding a lock on the whole struct
            let (client, profile) = {
                let m = midi.lock().unwrap();
                (m.osc_client.clone(), m.profile.clone())
            };

            // Identify the trigger
            let trigger = match msg_type {
                0x90 => {
                    let mode = if d2 == 127 {
                        crate::controller::MidiTriggerMode::Press
                    } else {
                        crate::controller::MidiTriggerMode::Release
                    };
                    Some(crate::controller::Trigger::MidiNote {
                        note: d1,
                        channel: chan,
                        mode,
                    })
                }
                0xB0 => Some(crate::controller::Trigger::MidiCc {
                    cc: d1,
                    channel: chan,
                }),
                0xE0 => Some(crate::controller::Trigger::MidiPitchwheel { channel: chan }),
                _ => None,
            };

            let trigger = match trigger {
                Some(t) => t,
                None => return,
            };

            // Find matching mappings
            for (mapping_idx, mapping) in profile.mappings.iter().enumerate() {
                let matches = match (&mapping.trigger, &trigger) {
                    (
                        crate::controller::Trigger::MidiNote {
                            note: n1,
                            channel: c1,
                            mode: m1,
                        },
                        crate::controller::Trigger::MidiNote {
                            note: n2,
                            channel: c2,
                            mode: m2,
                        },
                    ) => {
                        n1 == n2
                            && c1 == c2
                            && (*m1 == crate::controller::MidiTriggerMode::Both || m1 == m2)
                    }
                    (
                        crate::controller::Trigger::MidiCc {
                            cc: cc1,
                            channel: c1,
                        },
                        crate::controller::Trigger::MidiCc {
                            cc: cc2,
                            channel: c2,
                        },
                    ) => cc1 == cc2 && c1 == c2,
                    (
                        crate::controller::Trigger::MidiPitchwheel { channel: c1 },
                        crate::controller::Trigger::MidiPitchwheel { channel: c2 },
                    ) => c1 == c2,
                    _ => false,
                };

                if matches {
                    // Log trigger activity
                    {
                        let mut m = midi.lock().unwrap();
                        m.activity_log.push(crate::ActivityEvent {
                            mapping_idx,
                            part: crate::ActivityPart::Trigger,
                            value: match &trigger {
                                crate::controller::Trigger::MidiNote { note, .. } => {
                                    format!("Note {} (V:{})", note, d2)
                                }
                                crate::controller::Trigger::MidiCc { cc, .. } => {
                                    format!("CC {} (V:{})", cc, d2)
                                }
                                crate::controller::Trigger::MidiPitchwheel { .. } => {
                                    let value = ((d2 as i16) << 7) | (d1 as i16);
                                    format!("Pitch {}", value - 8192)
                                }
                                _ => "Trigger".to_string(),
                            },
                        });
                    }

                    // Logic Action Processing
                    match &mapping.action {
                        crate::controller::LogicalAction::Go => {
                            let mut m = midi.lock().unwrap();
                            m.crossfade_state = CrossfadeState::Go;
                            m.activity_log.push(crate::ActivityEvent {
                                mapping_idx,
                                part: crate::ActivityPart::Action,
                                value: "GO".to_string(),
                            });
                        }
                        crate::controller::LogicalAction::Stop => {
                            let mut m = midi.lock().unwrap();
                            let label = if m.crossfade_state == CrossfadeState::Go {
                                m.crossfade_state = CrossfadeState::Pause;
                                "PAUSE"
                            } else {
                                m.crossfade_state = CrossfadeState::GoBack;
                                "BACK"
                            };
                            m.activity_log.push(crate::ActivityEvent {
                                mapping_idx,
                                part: crate::ActivityPart::Action,
                                value: label.to_string(),
                            });
                        }
                        crate::controller::LogicalAction::Resume => {
                            let mut m = midi.lock().unwrap();
                            m.crossfade_state = CrossfadeState::Go;
                            m.activity_log.push(crate::ActivityEvent {
                                mapping_idx,
                                part: crate::ActivityPart::Action,
                                value: "RESUME".to_string(),
                            });
                        }
                        crate::controller::LogicalAction::FaderPageUp
                        | crate::controller::LogicalAction::FaderPageDown => {
                            let (new_page, display_duration, bank_size) = {
                                let mut m = midi.lock().unwrap();
                                if let crate::controller::LogicalAction::FaderPageDown =
                                    mapping.action
                                {
                                    m.fader_page = if m.fader_page >= 99 {
                                        1
                                    } else {
                                        m.fader_page + 1
                                    };
                                } else {
                                    m.fader_page = if m.fader_page <= 1 {
                                        99
                                    } else {
                                        m.fader_page - 1
                                    };
                                }
                                m.last_page_change = time::Instant::now();
                                m.show_page_number(m.fader_page);
                                let page = m.fader_page;
                                m.activity_log.push(crate::ActivityEvent {
                                    mapping_idx,
                                    part: crate::ActivityPart::Action,
                                    value: format!("PAGE {}", page),
                                });
                                (m.fader_page, m.page_display_time, profile.eos_bank_size)
                            };

                            let client_clone = client.clone();
                            tokio::spawn(async move {
                                let _ = client_clone
                                    .send(
                                        &format!(
                                            "/eos/user/1/fader/1/config/{}/{}",
                                            new_page, bank_size
                                        ),
                                        vec![],
                                    )
                                    .await;
                                time::sleep(display_duration).await;
                                let _ = client_clone
                                    .send(
                                        &format!(
                                            "/eos/user/1/fader/1/config/{}/{}",
                                            new_page, bank_size
                                        ),
                                        vec![],
                                    )
                                    .await;
                            });
                        }
                        crate::controller::LogicalAction::FaderMove { index } => {
                            let value = ((d2 as i16) << 7) | (d1 as i16);
                            let f_val = (value as f32) / 16383.0;
                            let mut m = midi.lock().unwrap();
                            if *index < m.fader_levels.len() {
                                m.fader_levels[*index] = f_val;
                                m.activity_log.push(crate::ActivityEvent {
                                    mapping_idx,
                                    part: crate::ActivityPart::Action,
                                    value: format!("{:.0}%", f_val * 100.0),
                                });
                            }
                        }
                    }

                    // Capture dynamic values for OSC replacement
                    let (current_page, bank_size) = {
                        let m = midi.lock().unwrap();
                        (m.fader_page, m.profile.eos_bank_size)
                    };

                    // Output Processing
                    for (out_idx, output) in mapping.outputs.iter().enumerate() {
                        match output {
                            crate::controller::Output::Osc { addr, arg_type } => {
                                let client_clone = client.clone();
                                let mut final_addr = addr.clone();

                                // Perform dynamic replacement
                                if final_addr.contains("{page}") {
                                    final_addr =
                                        final_addr.replace("{page}", &current_page.to_string());
                                }
                                if final_addr.contains("{bank_size}") {
                                    final_addr =
                                        final_addr.replace("{bank_size}", &bank_size.to_string());
                                }

                                let arg_type = arg_type.clone();

                                // For Faders, we need the value
                                let arg =
                                    if let crate::controller::LogicalAction::FaderMove { .. } =
                                        mapping.action
                                    {
                                        let value = ((d2 as i16) << 7) | (d1 as i16);
                                        let f_val = (value as f32) / 16383.0;
                                        Some(rosc::OscType::Float(f_val))
                                    } else {
                                        None
                                    };

                                let val_suffix = if let Some(rosc::OscType::Float(f)) = &arg {
                                    format!(" {:.2}", f)
                                } else {
                                    String::new()
                                };

                                let midi_for_log = Arc::clone(&midi);
                                let addr_for_log = final_addr.clone();
                                tokio::spawn(async move {
                                    let mut success = false;
                                    if arg_type == "float" {
                                        if let Some(a) = arg {
                                            if client_clone.send(&final_addr, vec![a]).await.is_ok()
                                            {
                                                success = true;
                                            }
                                        }
                                    } else {
                                        if client_clone.send(&final_addr, vec![]).await.is_ok() {
                                            success = true;
                                        }
                                    }

                                    if success {
                                        let mut m = midi_for_log.lock().unwrap();
                                        m.activity_log.push(crate::ActivityEvent {
                                            mapping_idx,
                                            part: crate::ActivityPart::Output(out_idx),
                                            value: format!("{}{}", addr_for_log, val_suffix),
                                        });
                                    }
                                });
                            }
                            crate::controller::Output::MidiNote {
                                note,
                                channel,
                                velocity,
                            } => {
                                let mut m = midi.lock().unwrap();
                                m.send_queue.enqueue(vec![0x90 | channel, *note, *velocity]);
                                m.activity_log.push(crate::ActivityEvent {
                                    mapping_idx,
                                    part: crate::ActivityPart::Output(out_idx),
                                    value: format!("Note {} (V:{})", note, velocity),
                                });
                            }
                            crate::controller::Output::MidiPitchwheel { channel } => {
                                let value = ((d2 as i16) << 7) | (d1 as i16);
                                let mut m = midi.lock().unwrap();
                                let mut data = vec![0xE0 | channel];
                                data.push((value & 0x7F) as u8);
                                data.push(((value >> 7) & 0x7F) as u8);
                                m.send_queue.enqueue(data);
                                m.activity_log.push(crate::ActivityEvent {
                                    mapping_idx,
                                    part: crate::ActivityPart::Output(out_idx),
                                    value: format!("Pitch {}", value - 8192),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

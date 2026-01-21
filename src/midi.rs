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
            for mapping in &profile.mappings {
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
                    // Logic Action Processing
                    match &mapping.action {
                        crate::controller::LogicalAction::Go => {
                            let mut m = midi.lock().unwrap();
                            m.crossfade_state = CrossfadeState::Go;
                        }
                        crate::controller::LogicalAction::Stop => {
                            let mut m = midi.lock().unwrap();
                            if m.crossfade_state == CrossfadeState::Go {
                                m.crossfade_state = CrossfadeState::Pause;
                            } else {
                                m.crossfade_state = CrossfadeState::GoBack;
                            }
                        }
                        crate::controller::LogicalAction::Resume => {
                            let mut m = midi.lock().unwrap();
                            m.crossfade_state = CrossfadeState::Go;
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
                                // We don't echo back here usually, but if the user wants it, they add a MidiPitchwheel output
                            }
                        }
                        crate::controller::LogicalAction::MasterFaderMove => {
                            // Similar logic for master fader
                        }
                    }

                    // Output Processing
                    for output in &mapping.outputs {
                        match output {
                            crate::controller::Output::Osc { addr, arg_type } => {
                                let client_clone = client.clone();
                                let addr = addr.clone();
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

                                tokio::spawn(async move {
                                    if arg_type == "float" {
                                        if let Some(a) = arg {
                                            let _ = client_clone.send(&addr, vec![a]).await;
                                        }
                                    } else {
                                        let _ = client_clone.send(&addr, vec![]).await;
                                    }
                                });
                            }
                            crate::controller::Output::MidiNote {
                                note,
                                channel,
                                velocity,
                            } => {
                                let m = midi.lock().unwrap();
                                m.send_queue.enqueue(vec![0x90 | channel, *note, *velocity]);
                            }
                            crate::controller::Output::MidiPitchwheel { channel } => {
                                let value = ((d2 as i16) << 7) | (d1 as i16);
                                let m = midi.lock().unwrap();
                                let mut data = vec![0xE0 | channel];
                                data.push((value & 0x7F) as u8);
                                data.push(((value >> 7) & 0x7F) as u8);
                                m.send_queue.enqueue(data);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

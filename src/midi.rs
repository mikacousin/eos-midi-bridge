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
        if let Some(&channel) = self.profile.fader_channels.get(fader_index as usize) {
            let val = (value + 8192) as u16; // Convert pitch (-8192 to 8191) to 0-16383 range
            let mut data = vec![0xE0 | channel];
            data.push((val & 0x7F) as u8); // LSB
            data.push(((val >> 7) & 0x7F) as u8); // MSB
            self.send_queue.enqueue(data);
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
        for i in 0..self.profile.fader_count {
            self.enqueue_pitchwheel(i as u8, -8192);
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

            match msg_type {
                0x90 => {
                    // d2 == 127 is Press, d2 == 0 is Release
                    let is_pressed = d2 == 127;

                    if d1 == profile.buttons.go_note {
                        // GO Button
                        if is_pressed {
                            let _ = client.send("/eos/user/1/key/go_0", vec![]).await;
                            let mut m = midi.lock().unwrap();
                            m.crossfade_state = CrossfadeState::Go;
                            m.send_queue
                                .enqueue(vec![0x90, profile.buttons.go_note, 127]); // LED On
                            m.send_queue
                                .enqueue(vec![0x90, profile.buttons.stop_note, 0]); // Ensure Stop LED Off
                        }
                    } else if d1 == profile.buttons.stop_note {
                        // STOP / GoBack Button
                        if is_pressed {
                            let _ = client.send("/eos/user/1/key/stop", vec![]).await;
                            // Turn LED ON immediately
                            let mut m = midi.lock().unwrap();
                            if m.crossfade_state == CrossfadeState::Go {
                                m.crossfade_state = CrossfadeState::Pause;
                                m.send_queue
                                    .enqueue(vec![0x90, profile.buttons.stop_note, 127]);
                            } else {
                                m.crossfade_state = CrossfadeState::GoBack;
                                m.send_queue
                                    .enqueue(vec![0x90, profile.buttons.stop_note, 127]);
                                m.send_queue.enqueue(vec![0x90, profile.buttons.go_note, 0]);
                            }
                        }
                    } else if (d1 == profile.buttons.page_up_note
                        || d1 == profile.buttons.page_down_note)
                        && is_pressed
                    {
                        // Page Up / Down
                        let (new_page, display_duration, bank_size) = {
                            let mut m = midi.lock().unwrap();
                            if d1 == profile.buttons.page_down_note {
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
                            m.last_page_change = time::Instant::now(); // Mark the start of the 1s window
                            m.show_page_number(m.fader_page);
                            (m.fader_page, m.page_display_time, profile.eos_bank_size)
                        };

                        let client_clone = client.clone();

                        // Spawn a timer task
                        tokio::spawn(async move {
                            // Send the config to Eos immediately
                            let _ = client_clone
                                .send(
                                    &format!(
                                        "/eos/user/1/fader/1/config/{}/{}",
                                        new_page, bank_size
                                    ),
                                    vec![],
                                )
                                .await;

                            // Wait for 1 second
                            time::sleep(display_duration).await;

                            // Request fader names again to refresh the LCD and remove the Page message
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
                }
                0xE0 => {
                    // Pitch Wheel (Fader Movement)
                    let value = ((d2 as i16) << 7) | (d1 as i16); // This is the 0-16383 value
                    let pitch = value - 8192; // Convert to -8192 to 8191 range
                    let f_val = (value as f32) / 16383.0; // Normalized 0.0 to 1.0

                    let mut m = midi.lock().unwrap();
                    // Map MIDI channel to fader index using profile
                    if let Some(fader_idx) =
                        m.profile.fader_channels.iter().position(|&x| x == chan)
                    {
                        // Echo the value back to the controller immediately
                        m.enqueue_pitchwheel(fader_idx as u8, pitch);

                        if fader_idx < m.profile.eos_bank_size {
                            m.fader_levels[fader_idx] = f_val;

                            // Send to Eos: /eos/user/1/fader/1/{index}
                            let client_clone = client.clone();
                            tokio::spawn(async move {
                                let _ = client_clone
                                    .send(
                                        &format!("/eos/user/1/fader/1/{}", fader_idx + 1),
                                        vec![rosc::OscType::Float(f_val)],
                                    )
                                    .await;
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

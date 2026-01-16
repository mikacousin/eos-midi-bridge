use crate::{CrossfadeState, MackieEvent, Queue, osc::OscClient, strip_accents};
use midir::MidiOutputConnection;
use rosc::OscType;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;

pub struct Midi {
    pub osc_client: OscClient,
    pub fader_page: u8,
    pub send_queue: Queue<Vec<u8>>,
    pub connections: Vec<MidiOutputConnection>,
    pub last_page_change: time::Instant,
    pub page_display_time: Duration,
    pub crossfade_state: CrossfadeState,
    pub current_cue: Option<f32>,
    pub fader_levels: [f32; 9],
    pub fader_names: [String; 9],
    pub connection_status: String,
}

impl Midi {
    pub fn new(osc_client: OscClient) -> Self {
        Self {
            osc_client,
            fader_page: 1,
            send_queue: Queue::new(),
            connections: Vec::new(),
            last_page_change: time::Instant::now() - Duration::from_secs(2),
            page_display_time: Duration::from_millis(500),
            crossfade_state: CrossfadeState::Inactive,
            current_cue: None,
            fader_levels: [0.0; 9],
            fader_names: Default::default(),
            connection_status: "Disconnected".to_string(),
        }
    }

    pub fn enqueue_sysex(&self, data: Vec<u8>) {
        self.send_queue.enqueue(data);
    }

    pub fn enqueue_pitchwheel(&self, channel: u8, pitch: i16) {
        let val = (pitch + 8192) as u16;
        self.send_queue
            .enqueue(vec![0xE0 | channel, (val & 0x7F) as u8, (val >> 7) as u8]);
    }

    pub fn send_lcd(&self, text: &str, line: u8) {
        let text = strip_accents(text);
        let start = line * 56;
        let mut data = vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x12, start];
        data.extend(text.bytes().take(56));
        data.push(0xF7);
        self.enqueue_sysex(data);
    }

    pub fn send_to_strip(&self, text: &str, line: u8, strip: usize) {
        // Reverse logic: If line 0 is physically below line 1,
        // we swap the offset (line 0 = offset 56, line 1 = offset 0)
        let physical_line = if line == 0 { 1 } else { 0 };
        let start = (physical_line * 56) + (strip * 7) as u8;

        let text = format!("{:<6}|", strip_accents(text));
        let mut data = vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x12, start];
        data.extend(text.bytes().take(7));
        data.push(0xF7);
        self.enqueue_sysex(data);
    }

    pub fn show_page_number(&self, page: u8) {
        // Clear line
        self.send_lcd(&" ".repeat(56), 0);

        let page_text = format!("Page {}", page);
        // Center the text within a 56-character string padded with spaces
        let centered_text = format!("{:^56}", page_text);

        let mut data = vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x12, 56]; // Start at beginning of line 2
        data.extend(centered_text.bytes().take(56));
        data.push(0xF7);
        self.enqueue_sysex(data);
    }

    pub fn controller_reset(&self) {
        self.send_lcd(&" ".repeat(56), 0);
        self.send_lcd(&" ".repeat(56), 1);
        for i in 0..8 {
            self.enqueue_pitchwheel(i, -8192);
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

            // Get the client without holding a lock on the whole struct
            let client = {
                let m = midi.lock().unwrap();
                m.osc_client.clone()
            };

            match msg_type {
                0x90 => {
                    // d2 == 127 is Press, d2 == 0 is Release
                    let is_pressed = d2 == 127;

                    match d1 {
                        94 => {
                            // GO Button
                            if is_pressed {
                                let _ = client.send("/eos/user/1/key/go_0", vec![]).await;
                                let mut m = midi.lock().unwrap();
                                m.crossfade_state = CrossfadeState::Go;
                                m.send_queue.enqueue(vec![0x90, 94, 127]); // LED On
                                m.send_queue.enqueue(vec![0x90, 93, 0]); // Ensure GoBack LED Off
                            }
                        }
                        93 => {
                            // STOP / GoBack Button
                            if is_pressed {
                                let _ = client.send("/eos/user/1/key/stop", vec![]).await;
                                // Turn LED ON immediately
                                let mut m = midi.lock().unwrap();
                                if m.crossfade_state == CrossfadeState::Go {
                                    m.crossfade_state = CrossfadeState::Pause;
                                    m.send_queue.enqueue(vec![0x90, 93, 127]);
                                } else {
                                    m.crossfade_state = CrossfadeState::GoBack;
                                    m.send_queue.enqueue(vec![0x90, 93, 127]);
                                    m.send_queue.enqueue(vec![0x90, 94, 0]);
                                }
                            }
                        }
                        46 | 47 if is_pressed => {
                            // Page Up / Down
                            let (new_page, display_duration) = {
                                let mut m = midi.lock().unwrap();
                                if d1 == 47 {
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
                                (m.fader_page, m.page_display_time)
                                // MutexGuard 'm' is dropped here automatically at the end of the block
                            };

                            let client_clone = client.clone();

                            // Spawn a timer task
                            tokio::spawn(async move {
                                // Send the config to Eos immediately
                                let _ = client_clone
                                    .send(
                                        &format!("/eos/user/1/fader/1/config/{}/10", new_page),
                                        vec![],
                                    )
                                    .await;

                                // Wait for 1 second
                                time::sleep(display_duration).await;

                                // Request fader names again to refresh the LCD and remove the Page message
                                // Eos will respond with the names, which our OscServer handles by writing to both lines
                                let _ = client_clone
                                    .send(
                                        &format!("/eos/user/1/fader/1/config/{}/10", new_page),
                                        vec![],
                                    )
                                    .await;
                            });
                        }
                        _ => {}
                    }
                }
                0xE0 => {
                    // Calculate the pitch value
                    let pitch = (((d1 as u16) | ((d2 as u16) << 7)) as i16) - 8192;
                    let value = (pitch as f32 + 8192.0) / 16383.0;
                    // Echo the value back to the controler immediately
                    {
                        let mut m = midi.lock().unwrap();
                        m.enqueue_pitchwheel(chan, pitch);
                        if chan < 9 {
                            m.fader_levels[chan as usize] = value;
                        }
                    }
                    // Send the OSC command to Eos
                    let _ = client
                        .send(
                            &format!("/eos/user/1/fader/1/{}", chan + 1),
                            vec![OscType::Float(value)],
                        )
                        .await;
                }
                _ => {}
            }
        }
    }
}

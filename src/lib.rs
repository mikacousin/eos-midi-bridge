pub mod config;
pub mod midi;
pub mod osc;

use crate::midi::Midi;
use deunicode::deunicode;
use log::{debug, info};
use rosc::{OscMessage, OscPacket, OscType};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time;
use tokio_util::sync::CancellationToken;

// ============================================================================
// Types & Messaging
// ============================================================================

pub enum MackieEvent {
    MidiIn(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CrossfadeState {
    Inactive,
    Go,
    GoBack,
    Pause,
}

#[derive(Clone)]
pub struct Queue<T> {
    elements: Arc<Mutex<VecDeque<T>>>,
}

impl<T> Queue<T> {
    pub fn new() -> Self {
        Self {
            elements: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
    pub fn enqueue(&self, element: T) {
        self.elements.lock().unwrap().push_back(element);
    }
    pub fn dequeue(&self) -> Option<T> {
        self.elements.lock().unwrap().pop_front()
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
                                // 2. MutexGuard 'm' is dropped here automatically at the end of the block
                            };

                            let client_clone = client.clone();

                            // Spawn a timer task
                            tokio::spawn(async move {
                                // 1. Send the config to Eos immediately
                                let _ = client_clone
                                    .send(
                                        &format!("/eos/user/1/fader/1/config/{}/10", new_page),
                                        vec![],
                                    )
                                    .await;

                                // 2. Wait for 1 second
                                time::sleep(display_duration).await;

                                // 3. Request fader names again to refresh the LCD and remove the Page message
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
                        let m = midi.lock().unwrap();
                        m.enqueue_pitchwheel(chan, pitch);
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

// ============================================================================
// OSC (Async)
// ============================================================================

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
                // Check if we should ignore this update to keep the Page number visible
                {
                    let m = midi.lock().unwrap();
                    // Ignore updates for slightly less than the total display time to ensure refresh works
                    let lockout = m
                        .page_display_time
                        .saturating_sub(Duration::from_millis(100));
                    if m.last_page_change.elapsed() < lockout {
                        return;
                    }
                }
                if let (Some(f_str), Some(OscType::String(text))) = (parts.get(5), msg.args.first())
                {
                    if let Ok(f_num) = f_str.parse::<usize>() {
                        if f_num <= 8 {
                            let m = midi.lock().unwrap();
                            let mut text = text.clone();
                            if text.starts_with("S") {
                                let split: Vec<&str> = text.split_whitespace().collect();
                                if split.len() > 2 {
                                    text = split[2..].join(" ");
                                }
                            }

                            // Transliterate to ASCII first
                            let clean_text = strip_accents(&text);

                            // Split text into two lines (Top 7 chars, Bottom 7 chars)
                            let line0: String = clean_text.chars().take(6).collect();
                            let line1: String = clean_text.chars().skip(6).take(6).collect();

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
                        if f_num <= 8 {
                            let pitch = (val * 16383.0).round() as i16 - 8192;
                            let m = midi.lock().unwrap();
                            m.enqueue_pitchwheel((f_num - 1) as u8, pitch);
                        }
                    }
                }
            } else if msg.addr == "/eos/out/event/cue/1/0/stop" {
                let mut m = midi.lock().unwrap();
                // If the fader was currently moving (Go or GoBack), set to Pause
                if m.crossfade_state == CrossfadeState::Go
                    || m.crossfade_state == CrossfadeState::GoBack
                {
                    m.crossfade_state = CrossfadeState::Pause;
                    m.send_queue.enqueue(vec![0x90, 93, 127]); // Ensure Stop LED is Solid On
                    debug!("Eos Stop event detected: State set to Pause");
                }
            } else if msg.addr == "/eos/out/event/cue/1/0/resume" {
                let mut m = midi.lock().unwrap();
                if m.crossfade_state == CrossfadeState::Pause {
                    // Resume the 'Go' state so the Play button stops flashing and stays solid
                    m.crossfade_state = CrossfadeState::Go;
                    m.send_queue.enqueue(vec![0x90, 93, 0]); // Stop LED Off
                    m.send_queue.enqueue(vec![0x90, 94, 127]); // Go LED Solid
                    debug!("Eos Resume: State set to Go");
                }
            } else if msg.addr == "/eos/out/active/cue/text" {
                if let Some(OscType::String(text)) = msg.args.first() {
                    // Extract the cue number part
                    if let Some(cue_part) = text
                        .split('/')
                        .nth(1)
                        .and_then(|s| s.split_whitespace().next())
                    {
                        if let Ok(new_cue_num) = cue_part.parse::<f32>() {
                            let mut m = midi.lock().unwrap();

                            // Compare to know the direction if we were Inactive
                            if m.crossfade_state == CrossfadeState::Inactive
                                || m.crossfade_state == CrossfadeState::Pause
                            {
                                if new_cue_num > m.current_cue {
                                    m.crossfade_state = CrossfadeState::Go;
                                } else if new_cue_num < m.current_cue {
                                    m.crossfade_state = CrossfadeState::GoBack;
                                }
                            }

                            m.current_cue = new_cue_num;
                            debug!("Current Cue updated to: {}", m.current_cue);
                        }
                    }
                }
            } else if msg.addr.starts_with("/eos/out/active/cue") {
                if let Some(OscType::Float(progress)) = msg.args.get(0) {
                    let mut m = midi.lock().unwrap();

                    if *progress >= 1.0 {
                        m.crossfade_state = CrossfadeState::Inactive;
                        m.send_queue.enqueue(vec![0x90, 94, 0]); // Go LED Off
                        m.send_queue.enqueue(vec![0x90, 93, 0]); // Stop LED Off
                    } else if *progress > 0.0 {
                        // Apply LED status based on the state determined by text parsing or MIDI press
                        match m.crossfade_state {
                            CrossfadeState::Go => {
                                m.send_queue.enqueue(vec![0x90, 94, 127]);
                                m.send_queue.enqueue(vec![0x90, 93, 0]);
                            }
                            CrossfadeState::GoBack => {
                                m.send_queue.enqueue(vec![0x90, 93, 127]);
                                m.send_queue.enqueue(vec![0x90, 94, 0]);
                            }
                            CrossfadeState::Pause => {
                                m.send_queue.enqueue(vec![0x90, 93, 127]);
                                m.send_queue.enqueue(vec![0x90, 94, 0]);
                            }
                            CrossfadeState::Inactive => {}
                        }
                    }
                }
            }
        }
    }
}

pub fn strip_accents(text: &str) -> String {
    deunicode(text)
}

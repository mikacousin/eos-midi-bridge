use deunicode::deunicode;
use log::{debug, info};
use midir::MidiOutputConnection;
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

// ============================================================================
// Main Application State
// ============================================================================

pub struct Midi {
    pub osc_client: OscClient,
    pub fader_page: u8,
    pub send_queue: Queue<Vec<u8>>,
    pub connections: Vec<MidiOutputConnection>,
    pub last_page_change: time::Instant,
    pub page_display_time: Duration,
    pub crossfade_state: CrossfadeState,
    pub current_cue: f32,
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
            current_cue: 0.0,
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

pub fn strip_accents(text: &str) -> String {
    deunicode(text)
}

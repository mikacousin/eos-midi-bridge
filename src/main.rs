use std::collections::VecDeque;
use std::io::{stdin, stdout, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deunicode::deunicode;
use log::{debug, error, info};
use midir::{MidiInput, MidiOutput, MidiOutputConnection};
use rosc::{OscMessage, OscPacket, OscType};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time;
use tokio_util::sync::CancellationToken;

// ============================================================================
// Types & Messaging
// ============================================================================

enum MackieEvent {
    MidiIn(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CrossfadeState {
    Inactive,
    Go,
    GoBack,
    Pause,
}

#[derive(Clone)]
struct Queue<T> {
    elements: Arc<Mutex<VecDeque<T>>>,
}

impl<T> Queue<T> {
    fn new() -> Self {
        Self {
            elements: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
    fn enqueue(&self, element: T) {
        self.elements.lock().unwrap().push_back(element);
    }
    fn dequeue(&self) -> Option<T> {
        self.elements.lock().unwrap().pop_front()
    }
}

// ============================================================================
// OSC (Async)
// ============================================================================

#[derive(Clone)]
struct OscClient {
    host: String,
    port: u16,
    socket: Arc<UdpSocket>,
}

impl OscClient {
    async fn new(host: &str, port: u16) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Self {
            host: host.to_string(),
            port,
            socket: Arc::new(socket),
        })
    }

    async fn send(&self, path: &str, args: Vec<OscType>) -> anyhow::Result<()> {
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

struct OscServer {
    port: u16,
}

impl OscServer {
    async fn start(self, midi: Arc<Mutex<Midi>>, token: CancellationToken) -> anyhow::Result<()> {
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

    async fn handle_packet(&self, packet: OscPacket, midi: &Arc<Mutex<Midi>>) {
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

struct Midi {
    osc_client: OscClient,
    fader_page: u8,
    send_queue: Queue<Vec<u8>>,
    connections: Vec<MidiOutputConnection>,
    last_page_change: time::Instant,
    page_display_time: Duration,
    crossfade_state: CrossfadeState,
    current_cue: f32,
}

impl Midi {
    fn new(osc_client: OscClient) -> Self {
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

    fn enqueue_sysex(&self, data: Vec<u8>) {
        self.send_queue.enqueue(data);
    }

    fn enqueue_pitchwheel(&self, channel: u8, pitch: i16) {
        let val = (pitch + 8192) as u16;
        self.send_queue
            .enqueue(vec![0xE0 | channel, (val & 0x7F) as u8, (val >> 7) as u8]);
    }

    fn send_lcd(&self, text: &str, line: u8) {
        let text = strip_accents(text);
        let start = line * 56;
        let mut data = vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x12, start];
        data.extend(text.bytes().take(56));
        data.push(0xF7);
        self.enqueue_sysex(data);
    }

    fn send_to_strip(&self, text: &str, line: u8, strip: usize) {
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

    fn show_page_number(&self, page: u8) {
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

    fn controller_reset(&self) {
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

fn strip_accents(text: &str) -> String {
    deunicode(text)
}

// ============================================================================
// The Core Worker (MPSC Consumer)
// ============================================================================

async fn handle_event_logic(event: MackieEvent, midi: Arc<Mutex<Midi>>) {
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
// Main
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cancel_token = CancellationToken::new();

    // 1. MIDI Selection
    let midi_in = MidiInput::new("Mackie In")?;
    let midi_out = MidiOutput::new("Mackie Out")?;

    let in_ports = midi_in.ports();
    for (i, p) in in_ports.iter().enumerate() {
        println!("{}: {}", i, midi_in.port_name(p)?);
    }
    print!("Select In: ");
    stdout().flush()?;
    let mut input = String::new();
    stdin().read_line(&mut input)?;
    let in_port = &in_ports[input.trim().parse::<usize>()?];

    let out_ports = midi_out.ports();
    for (i, p) in out_ports.iter().enumerate() {
        println!("{}: {}", i, midi_out.port_name(p)?);
    }
    print!("Select Out: ");
    stdout().flush()?;
    let mut output = String::new();
    stdin().read_line(&mut output)?;
    let out_port = &out_ports[output.trim().parse::<usize>()?];

    // 2. Init State & Channels
    let (tx, mut rx) = mpsc::channel::<MackieEvent>(100);
    let osc_client = OscClient::new("192.168.1.42", 8000).await?;
    let midi = Arc::new(Mutex::new(Midi::new(osc_client)));

    // 3. Connect MIDI
    let conn_out = midi_out
        .connect(out_port, "mackie-out-conn")
        .map_err(|e| anyhow::anyhow!("Out Error: {}", e))?;
    {
        let mut m = midi.lock().unwrap();
        m.connections.push(conn_out);
        let _ = m
            .osc_client
            .send("/eos/user/1/fader/1/config/1/10", vec![])
            .await;
        m.controller_reset();
    }

    let tx_clone = tx.clone();
    let _conn_in = midi_in
        .connect(
            in_port,
            "mackie-in-conn",
            move |_, msg, _| {
                let _ = tx_clone.blocking_send(MackieEvent::MidiIn(msg.to_vec()));
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!("In Error: {}", e))?;

    // 4. Spawn Worker Tasks
    let worker_midi = Arc::clone(&midi);
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            handle_event_logic(event, Arc::clone(&worker_midi)).await;
        }
    });

    // Flash Play button on Pause
    let flash_midi = Arc::clone(&midi);
    tokio::spawn(async move {
        let mut tick = false;
        let mut interval = time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            let m = flash_midi.lock().unwrap();
            if m.crossfade_state == CrossfadeState::Pause {
                tick = !tick;
                let vel = if tick { 127 } else { 0 };
                m.send_queue.enqueue(vec![0x90, 94, vel]);
            }
        }
    });

    let loop_midi = Arc::clone(&midi);
    let loop_token = cancel_token.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(20));
        loop {
            tokio::select! {
                _ = loop_token.cancelled() => break,
                _ = interval.tick() => {
                    let mut m = loop_midi.lock().unwrap();
                    while let Some(msg) = m.send_queue.dequeue() {
                        debug!("Sending MIDI: {:?}", msg);
                        for conn in &mut m.connections {
                            if let Err(e) = conn.send(&msg) {
                            error!("Failed to send MIDI message: {}", e);
                            }
                        }
                    }
                }
            }
        }
    });

    // Spawn the OSC Server to listen for feedback from Eos
    let osc_server_midi = Arc::clone(&midi);
    let osc_server_token = cancel_token.clone();
    tokio::spawn(async move {
        let server = OscServer { port: 8001 };
        if let Err(e) = server.start(osc_server_midi, osc_server_token).await {
            error!("OSC Server Error : {}", e);
        }
    });

    println!("\nRunning. Ctrl+C to exit.");
    tokio::signal::ctrl_c().await?;

    info!("\nShutting down and resetting controller...");
    // 1. Trigger the reset
    {
        let mut m = midi.lock().unwrap();
        m.controller_reset();

        // 2. Immediately flush the queue to the hardware
        // We do this manually here because the loop task may be cancelled
        // or shut down before it gets one last tick.
        while let Some(msg) = m.send_queue.dequeue() {
            for conn in &mut m.connections {
                let _ = conn.send(&msg);
            }
        }
    }

    // 3. Cancel tokens and wait briefly for tasks to clean up
    cancel_token.cancel();
    time::sleep(Duration::from_millis(200)).await;
    info!("Shutdown complete.");
    Ok(())
}

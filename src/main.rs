use log::{debug, error, info};
use midir::{MidiInput, MidiOutput};
use rosc::OscType;
use std::io::{stdin, stdout, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time;
use tokio_util::sync::CancellationToken;

use eos_midi_bridge::*;

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

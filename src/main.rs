use log::{debug, error, info};
use midir::{MidiInput, MidiOutput};
use std::io::{Write, stdin, stdout};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time;
use tokio_util::sync::CancellationToken;

use eos_midi_bridge::{
    CrossfadeState, MackieEvent, config,
    midi::{Midi, handle_event_logic},
    osc::{OscClient, OscServer},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Load persisted configuration
    let cfg = config::load_config();
    info!(
        "Loaded Config: Eos IP: {}, Port: {}",
        cfg.eos_ip, cfg.eos_osc_port
    );

    let cancel_token = CancellationToken::new();

    // MIDI Selection
    let midi_in = MidiInput::new("Mackie In")?;
    let midi_out = MidiOutput::new("Mackie Out")?;

    let in_ports = midi_in.ports();
    for (i, p) in in_ports.iter().enumerate() {
        println!("{}: {}", i, midi_in.port_name(p)?);
    }

    let in_port = loop {
        print!("Select In: ");
        stdout().flush()?;
        let mut input = String::new();
        stdin().read_line(&mut input)?;

        if let Ok(idx) = input.trim().parse::<usize>() {
            if idx < in_ports.len() {
                break &in_ports[idx];
            }
        }
        println!(
            "Invalid index. Please choose a number between 0 and {}.",
            in_ports.len() - 1
        );
    };

    let out_ports = midi_out.ports();
    for (i, p) in out_ports.iter().enumerate() {
        println!("{}: {}", i, midi_out.port_name(p)?);
    }

    let out_port = loop {
        print!("Select Out: ");
        stdout().flush()?;
        let mut output = String::new();
        stdin().read_line(&mut output)?;

        if let Ok(idx) = output.trim().parse::<usize>() {
            if idx < out_ports.len() {
                break &out_ports[idx];
            }
        }
        println!(
            "Invalid index. Please choose a number between 0 and {}.",
            out_ports.len() - 1
        );
    };

    // Init State & Channels
    let (tx, mut rx) = mpsc::channel::<MackieEvent>(100);
    let osc_client = OscClient::new(&cfg.eos_ip, cfg.eos_osc_port).await?;
    let midi = Arc::new(Mutex::new(Midi::new(osc_client)));

    // Connect MIDI
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

    // Spawn Worker Tasks
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
        let server = OscServer {
            port: cfg.bridge_listen_port,
        };
        if let Err(e) = server.start(osc_server_midi, osc_server_token).await {
            error!("OSC Server Error : {}", e);
        }
    });

    println!("\nRunning. Ctrl+C to exit.");
    tokio::signal::ctrl_c().await?;

    info!("\nShutting down and resetting controller...");
    // Trigger the reset
    {
        let mut m = midi.lock().unwrap();
        m.controller_reset();

        // Immediately flush the queue to the hardware
        // We do this manually here because the loop task may be cancelled
        // or shut down before it gets one last tick.
        while let Some(msg) = m.send_queue.dequeue() {
            for conn in &mut m.connections {
                let _ = conn.send(&msg);
            }
        }
    }

    // Cancel tokens and wait briefly for tasks to clean up
    cancel_token.cancel();
    time::sleep(Duration::from_millis(200)).await;
    info!("Shutdown complete.");
    Ok(())
}

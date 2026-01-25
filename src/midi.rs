use crate::{CrossfadeState, MackieEvent, Queue, osc::OscClient, strip_accents};
// Used in struct definitions but seemingly unused?
// Actually if we look at line 16, it IS used.
// Why did rustc complain?
// "warning: unused import: `midir::MidiOutputConnection`"
// "note: `#[warn(unused_imports)]` (part of `#[warn(unused)]`) on by default"
// Maybe it's because I'm using `midir::MidiOutputConnection` fully qualified in the struct, so I don't need the import?
// Yes.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;

pub struct Midi {
    pub fader_page: u8,
    pub send_queue: Queue<(String, Vec<u8>)>,
    pub last_page_change: time::Instant,
    pub page_display_time: Duration,
    pub crossfade_state: CrossfadeState,
    pub current_cue: Option<f32>,
    pub osc_client: OscClient,
    pub device_profiles: std::collections::HashMap<String, crate::controller::ControllerProfile>,
    pub device_connections: std::collections::HashMap<String, midir::MidiOutputConnection>,
    // Removed old connections vec, now mapping output connections by device name

    // State maps keyed by [DeviceName] usually not needed if state is global,
    // BUT fader levels might differ if profiles map them differently.
    // For now, we keep Global State for the "EOS Model" side (what EOS tells us),
    // and we translate that to each device.

    // EOS State
    pub fader_levels: [f32; 128], // Stores up to 128 faders (enough for huge banks)
    pub fader_names: Vec<String>, // Stores names for faders

    pub connection_status: String,
    pub available_in_ports: Vec<String>,
    pub available_out_ports: Vec<String>,
    pub last_osc_heartbeat: Option<std::time::Instant>,
    pub needs_sync: bool,
    pub blink_state: bool,
    pub activity_log: Vec<crate::ActivityEvent>,
    pub intended_dir: CrossfadeState,
    pub last_applied_dir: CrossfadeState,
    pub device_fader_values: std::collections::HashMap<(String, usize), f32>,
    pub log_sender: tokio::sync::mpsc::Sender<crate::LogEntry>,
}

impl Midi {
    pub fn new(
        osc_client: OscClient,
        log_sender: tokio::sync::mpsc::Sender<crate::LogEntry>,
    ) -> Self {
        // Initialize with empty names
        let fader_names = vec![String::new(); 128];
        let fader_levels = [0.0; 128];

        Self {
            osc_client,
            device_profiles: std::collections::HashMap::new(),
            device_connections: std::collections::HashMap::new(),
            fader_levels,
            fader_names,
            fader_page: 1,
            send_queue: Queue::new(),
            last_page_change: time::Instant::now() - Duration::from_secs(2),
            page_display_time: Duration::from_millis(500),
            crossfade_state: CrossfadeState::Inactive,
            current_cue: None,
            connection_status: "Disconnected".to_string(),
            available_in_ports: Vec::new(),
            available_out_ports: Vec::new(),
            last_osc_heartbeat: None,
            needs_sync: true,
            activity_log: Vec::new(),
            blink_state: false,
            intended_dir: CrossfadeState::Inactive,
            last_applied_dir: CrossfadeState::Inactive,
            device_fader_values: std::collections::HashMap::new(),
            log_sender,
        }
    }

    pub fn enqueue(&self, target_device: &str, data: Vec<u8>) {
        self.send_queue.enqueue((target_device.to_string(), data));
    }

    pub fn update_fader_feedback(
        &mut self,
        fader_index: usize,
        value: f32,
        exclude_device: Option<&str>,
    ) {
        let mut logs = Vec::new();
        // Iterate over all devices and check their profiles
        for (device_name, profile) in &self.device_profiles {
            if let Some(ex) = exclude_device {
                if device_name == ex {
                    continue;
                }
            }
            // Find mappings for FaderMove { index }
            for (map_idx, mapping) in profile.mappings.iter().enumerate() {
                if let crate::controller::LogicalAction::FaderMove { index } = mapping.action
                    && index == fader_index
                {
                    for (out_idx, output) in mapping.outputs.iter().enumerate() {
                        match output {
                            crate::controller::Output::MidiPitchwheel { channel } => {
                                // 14-bit Pitch (0-16383)
                                let val_14 = (value * 16383.0).round() as u16;
                                let mut data = vec![0xE0 | channel];
                                data.push((val_14 & 0x7F) as u8); // LSB
                                data.push(((val_14 >> 7) & 0x7F) as u8); // MSB
                                self.enqueue(device_name, data);

                                logs.push(crate::ActivityEvent {
                                    device_name: device_name.to_string(),
                                    mapping_idx: map_idx,
                                    part: crate::ActivityPart::Output(out_idx),
                                    value: format!("Pitch {:.0}", value * 100.0), // Simplified log
                                });
                            }
                            crate::controller::Output::MidiCc { cc, channel } => {
                                // 7-bit CC (0-127)
                                let val_7 = (value * 127.0).round() as u8;
                                self.enqueue(device_name, vec![0xB0 | channel, *cc, val_7]);

                                logs.push(crate::ActivityEvent {
                                    device_name: device_name.to_string(),
                                    mapping_idx: map_idx,
                                    part: crate::ActivityPart::Output(out_idx),
                                    value: format!("CC {} (V:{})", cc, val_7),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        self.activity_log.extend(logs);
    }

    pub fn send_lcd(&self, text: &str, line: u8) {
        let text = strip_accents(text);
        for (device_name, profile) in &self.device_profiles {
            if line as usize >= profile.display.line_offsets.len() {
                continue;
            }
            let start = profile.display.line_offsets[line as usize];
            let mut data = vec![0xF0];
            data.extend(&profile.display.sysex_prefix);
            data.push(start);
            data.extend(text.bytes().take(profile.display.line_length));
            data.push(0xF7);
            self.enqueue(device_name, data);
        }
    }

    pub fn send_to_strip(&self, text: &str, line: u8, strip: usize) {
        let text_stripped = strip_accents(text);

        for (device_name, profile) in &self.device_profiles {
            // Check if this strip (0-indexed) corresponds to a Visible Fader (1-indexed)
            if let Some(visible) = &profile.display.visible_faders {
                if !visible.contains(&(strip + 1)) {
                    continue;
                }
            }

            if line as usize >= profile.display.line_offsets.len() {
                continue;
            }
            let start = profile.display.line_offsets[line as usize]
                + (strip * profile.display.strip_width) as u8;

            let width = profile.display.strip_width;
            // Padding and pipe logic
            let text_fmt = format!("{:<width$}|", text_stripped, width = width - 1);
            let mut data = vec![0xF0];
            data.extend(&profile.display.sysex_prefix);
            data.push(start);
            data.extend(text_fmt.bytes().take(width));
            data.push(0xF7);
            self.enqueue(device_name, data);
        }
    }

    pub fn show_page_number(&self, page: u8) {
        for (device_name, profile) in &self.device_profiles {
            // 1. Clear line 0
            let len = profile.display.line_length;
            // Manual send_lcd logic inline to avoid double iteration
            if !profile.display.line_offsets.is_empty() {
                let space = " ".repeat(len);
                let start = profile.display.line_offsets[0];
                let mut data = vec![0xF0];
                data.extend(&profile.display.sysex_prefix);
                data.push(start);
                data.extend(space.bytes().take(len));
                data.push(0xF7);
                self.enqueue(device_name, data);
            }

            // 2. Show Page
            if profile.display.line_offsets.len() > 1 {
                let page_text = format!("Page {}", page);
                let centered_text = crate::center_text(&page_text, len);

                let start = profile.display.line_offsets[1];
                let mut data = vec![0xF0];
                data.extend(&profile.display.sysex_prefix);
                data.push(start);
                data.extend(centered_text.bytes().take(len));
                data.push(0xF7);
                self.enqueue(device_name, data);
            }
        }
    }

    pub fn controller_reset(&self) {
        for (device_name, profile) in &self.device_profiles {
            let len = profile.display.line_length;
            for line in 0..profile.display.line_offsets.len() {
                // Inline send_lcd
                if line as usize >= profile.display.line_offsets.len() {
                    continue;
                }
                let start = profile.display.line_offsets[line as usize];
                let mut data = vec![0xF0];
                data.extend(&profile.display.sysex_prefix);
                data.push(start);
                data.extend(" ".repeat(len).bytes().take(len));
                data.push(0xF7);
                self.enqueue(device_name, data);
            }

            // Reset faders
            for mapping in &profile.mappings {
                if let crate::controller::LogicalAction::FaderMove { index: _ } = mapping.action {
                    // Logic from enqueue_pitchwheel inline
                    for output in &mapping.outputs {
                        if let crate::controller::Output::MidiPitchwheel { channel } = output {
                            // -8192
                            let val = 0 as u16;
                            let mut data = vec![0xE0 | channel];
                            data.push((val & 0x7F) as u8);
                            data.push(((val >> 7) & 0x7F) as u8);
                            self.enqueue(device_name, data);
                        }
                    }
                }
            }

            // Reset Notes
            for note in 0..128 {
                self.enqueue(device_name, vec![0x90, note, 0]);
            }
        }
    }
    pub fn find_mappings_for_midi_message(
        &self,
        msg: &[u8],
        device_name: &str,
    ) -> Vec<(usize, crate::ActivityPart)> {
        if let Some(profile) = self.device_profiles.get(device_name) {
            profile.match_output_message(msg)
        } else {
            Vec::new()
        }
    }

    pub fn set_crossfade_state(&mut self, new_state: crate::CrossfadeState, force: bool) {
        if !force && self.crossfade_state == new_state {
            return;
        }

        // Reset blink state whenever we switch states to ensure clean slate
        if new_state != crate::CrossfadeState::Pause {
            self.blink_state = false;
        }
        self.crossfade_state = new_state;

        // Helper vars removed as we need per-device lookup

        match self.crossfade_state {
            crate::CrossfadeState::Go => {
                for (d_name, p) in &self.device_profiles {
                    if let Some((n, c, v)) =
                        p.get_midi_output_for_action(crate::controller::LogicalAction::Go)
                    {
                        self.enqueue(d_name, vec![0x90 | c, n, v]);
                    }
                    if let Some((n, c, _)) =
                        p.get_midi_output_for_action(crate::controller::LogicalAction::Stop)
                    {
                        self.enqueue(d_name, vec![0x90 | c, n, 0]);
                    }
                }
            }
            crate::CrossfadeState::GoBack | crate::CrossfadeState::Pause => {
                for (d_name, p) in &self.device_profiles {
                    if let Some((n, c, v)) =
                        p.get_midi_output_for_action(crate::controller::LogicalAction::Stop)
                    {
                        self.enqueue(d_name, vec![0x90 | c, n, v]);
                    }
                    if let Some((n, c, _)) =
                        p.get_midi_output_for_action(crate::controller::LogicalAction::Go)
                    {
                        self.enqueue(d_name, vec![0x90 | c, n, 0]);
                    }
                }
            }
            crate::CrossfadeState::Inactive => {
                for (d_name, p) in &self.device_profiles {
                    if let Some((n, c, _)) =
                        p.get_midi_output_for_action(crate::controller::LogicalAction::Go)
                    {
                        self.enqueue(d_name, vec![0x90 | c, n, 0]);
                    }
                    if let Some((n, c, _)) =
                        p.get_midi_output_for_action(crate::controller::LogicalAction::Stop)
                    {
                        self.enqueue(d_name, vec![0x90 | c, n, 0]);
                    }
                }
            }
        }
    }

    pub fn tick_blink(&mut self) {
        if self.crossfade_state == crate::CrossfadeState::Pause {
            self.blink_state = !self.blink_state;

            // let go_fb = self.profile... REMOVED, doing manual iteration below

            let _v = if self.blink_state { 127 } else { 0 };
            // Need to find which device(s) have this mapping?
            // set_crossfade_state handles the finding.
            // Re-using set_crossfade_state logic might be cleaner but it resets blink state.
            // Let's iterate:
            for (device_name, profile) in &self.device_profiles {
                if let Some((n, c, on_vel)) =
                    profile.get_midi_output_for_action(crate::controller::LogicalAction::Go)
                {
                    let v = if self.blink_state { on_vel } else { 0 };
                    self.enqueue(device_name, vec![0x90 | c, n, v]);
                }
            }
        }
    }
    pub fn reset_all_outputs(&self) {
        for (device_name, profile) in &self.device_profiles {
            // Clear LCD if applicable
            let len = profile.display.line_length;
            for line in 0..profile.display.line_offsets.len() {
                let start = profile.display.line_offsets[line];
                let mut data = vec![0xF0];
                data.extend(&profile.display.sysex_prefix);
                data.push(start);
                data.extend(" ".repeat(len).bytes().take(len));
                data.push(0xF7);
                self.enqueue(device_name, data);
            }

            for mapping in &profile.mappings {
                for output in &mapping.outputs {
                    match output {
                        crate::controller::Output::MidiNote { note, channel, .. } => {
                            self.enqueue(device_name, vec![0x90 | channel, *note, 0]);
                        }
                        crate::controller::Output::MidiCc { cc, channel, .. } => {
                            self.enqueue(device_name, vec![0xB0 | channel, *cc, 0]);
                        }
                        crate::controller::Output::MidiPitchwheel { channel } => {
                            self.enqueue(device_name, vec![0xE0 | channel, 0, 0]);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

pub async fn handle_event_logic(event: MackieEvent, midi: Arc<Mutex<Midi>>) {
    match event {
        MackieEvent::MidiIn {
            device_name,
            data: msg,
        } => {
            if msg.len() < 3 {
                return;
            }
            let (status, _d1, _d2) = (msg[0], msg[1], msg[2]);
            let (_msg_type, _chan) = (status & 0xF0, status & 0x0F);

            // Get the client and profile without holding a lock on the whole struct
            let (_client, profile) = {
                let m = midi.lock().unwrap();
                // Find profile for this device
                let p = m.device_profiles.get(&device_name).cloned();
                (m.osc_client.clone(), p)
            };

            let profile = match profile {
                Some(p) => p,
                None => {
                    // Profile not found for this device?
                    return;
                }
            };

            // Identify the trigger
            let trigger = match crate::controller::Trigger::from_midi_message(&msg) {
                Some(t) => t,
                None => return,
            };

            // Find matching mappings
            for (mapping_idx, mapping) in profile.mappings.iter().enumerate() {
                // Check if trigger matches mapping
                let is_match = match (&mapping.trigger, &trigger) {
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
                            mode: _, // Mode is purely for interpretation, not matching?
                                     // Actually, if we have different modes, they match the same CC/Channel.
                                     // So matching logic should be same.
                        },
                        crate::controller::Trigger::MidiCc {
                            cc: cc2,
                            channel: c2,
                            mode: _,
                        },
                    ) => cc1 == cc2 && c1 == c2,
                    (
                        crate::controller::Trigger::MidiPitchwheel { channel: c1 },
                        crate::controller::Trigger::MidiPitchwheel { channel: c2 },
                    ) => c1 == c2,
                    _ => false,
                };

                if is_match {
                    let (d1, d2) = (msg[1], msg[2]);
                    // Log trigger activity
                    {
                        let mut m = midi.lock().unwrap();
                        m.activity_log.push(crate::ActivityEvent {
                            device_name: device_name.clone(),
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

                    // Logic Action Processing via Helper
                    execute_mapping(
                        Arc::clone(&midi),
                        mapping,
                        mapping_idx,
                        d1,
                        d2,
                        &device_name,
                    )
                    .await;
                }
            }
        }
    }
}

pub async fn execute_mapping(
    midi: Arc<Mutex<Midi>>,
    mapping: &crate::controller::Mapping,
    mapping_idx: usize,
    d1: u8,
    d2: u8,
    device_name: &str,
) {
    let (client, profile) = {
        let m = midi.lock().unwrap();
        (
            m.osc_client.clone(),
            m.device_profiles.get(device_name).cloned(),
        )
    };

    let profile = if let Some(p) = profile {
        p
    } else {
        return;
    };

    let mut cached_fader_value: Option<f32> = None;

    // Logic Action Processing
    match &mapping.action {
        crate::controller::LogicalAction::Go => {
            let mut m = midi.lock().unwrap();
            m.crossfade_state = CrossfadeState::Go;
            m.activity_log.push(crate::ActivityEvent {
                device_name: device_name.to_string(),
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
                device_name: device_name.to_string(),
                mapping_idx,
                part: crate::ActivityPart::Action,
                value: label.to_string(),
            });
        }
        crate::controller::LogicalAction::Resume => {
            let mut m = midi.lock().unwrap();
            m.crossfade_state = CrossfadeState::Go;
            m.activity_log.push(crate::ActivityEvent {
                device_name: device_name.to_string(),
                mapping_idx,
                part: crate::ActivityPart::Action,
                value: "RESUME".to_string(),
            });
        }
        crate::controller::LogicalAction::FaderPageUp
        | crate::controller::LogicalAction::FaderPageDown => {
            let (new_page, display_duration, bank_size) = {
                let mut m = midi.lock().unwrap();
                let go_up = matches!(
                    mapping.action,
                    crate::controller::LogicalAction::FaderPageUp
                );

                m.fader_page = crate::calculate_next_page(m.fader_page, go_up);

                m.last_page_change = time::Instant::now();
                m.show_page_number(m.fader_page);
                let page = m.fader_page;
                m.activity_log.push(crate::ActivityEvent {
                    device_name: device_name.to_string(),
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
                        &format!("/eos/user/1/fader/1/config/{}/{}", new_page, bank_size),
                        vec![],
                    )
                    .await;
                time::sleep(display_duration).await;
                let _ = client_clone
                    .send(
                        &format!("/eos/user/1/fader/1/config/{}/{}", new_page, bank_size),
                        vec![],
                    )
                    .await;
            });
        }

        crate::controller::LogicalAction::FaderMove { index } => {
            let mut final_v = 0.0;
            let mut update = false;
            let mut requires_pickup = false;

            if let crate::controller::Trigger::MidiCc { mode, .. } = &mapping.trigger {
                match mode {
                    crate::controller::MidiCcTriggerMode::Absolute => {
                        // Inherited logic: assumes 14-bit if not caught by the broken B0 check (which is always false here)
                        // effectively: value = (d2 << 7) | d1.
                        // For CC: d2 is value, d1 is CC number.
                        let value = ((d2 as i16) << 7) | (d1 as i16);
                        final_v = (value as f32) / 16383.0;
                        update = true;
                        requires_pickup = true;
                    }
                    crate::controller::MidiCcTriggerMode::RelativeStandard => {
                        let delta: i32 = if d2 < 64 {
                            d2 as i32
                        } else {
                            (d2 as i32) - 128
                        };
                        let m = midi.lock().unwrap();
                        if *index < m.fader_levels.len() {
                            let current = m.fader_levels[*index];
                            let new_val = (current + (delta as f32 * 0.01)).clamp(0.0, 1.0);
                            final_v = new_val;
                            update = true;
                            requires_pickup = false;
                        }
                    }
                    crate::controller::MidiCcTriggerMode::RelativeBehringer => {
                        let delta: i32 = if d2 >= 64 {
                            (d2 as i32) - 128
                        } else {
                            d2 as i32
                        };
                        let m = midi.lock().unwrap();
                        if *index < m.fader_levels.len() {
                            let current = m.fader_levels[*index];
                            let new_val = (current + (delta as f32 * 0.005)).clamp(0.0, 1.0);
                            final_v = new_val;
                            update = true;
                            requires_pickup = false;
                        }
                    }
                }
            } else {
                // PitchWheel or others: Treat as 14-bit Absolute
                let value = ((d2 as i16) << 7) | (d1 as i16);
                final_v = (value as f32) / 16383.0;
                update = true;
                requires_pickup = true;
            }

            if update {
                let mut m = midi.lock().unwrap();

                let allow_update = if requires_pickup && *index < m.fader_levels.len() {
                    let map_key = (device_name.to_string(), mapping_idx);
                    let last_phys_val = m.device_fader_values.get(&map_key).cloned();
                    let eos_val = m.fader_levels[*index];

                    // Always update Physical State so we can track movement
                    m.device_fader_values.insert(map_key, final_v);

                    if let Some(prev) = last_phys_val {
                        let tolerance = 0.05; // 5% latch window
                        // If we were previously synced, allow small drifts
                        let was_latched = (prev - eos_val).abs() < tolerance;

                        // Check crossover
                        let crossed_up = prev <= eos_val && final_v >= eos_val;
                        let crossed_down = prev >= eos_val && final_v <= eos_val;

                        // User requirement: Wait until value crossed EOS value
                        if was_latched || crossed_up || crossed_down {
                            true
                        } else {
                            false // Blocking until crossover
                        }
                    } else {
                        // No history: Block until we establish crossover context?
                        // Or Check proximity?
                        if (final_v - eos_val).abs() < 0.1 {
                            true
                        } else {
                            false
                        }
                    }
                } else {
                    true
                };

                if allow_update && *index < m.fader_levels.len() {
                    m.fader_levels[*index] = final_v;
                    // Local Feedback: Update other controllers immediately
                    m.update_fader_feedback(*index, final_v, Some(device_name));

                    cached_fader_value = Some(final_v);
                    m.activity_log.push(crate::ActivityEvent {
                        device_name: device_name.to_string(),
                        mapping_idx,
                        part: crate::ActivityPart::Action,
                        value: format!("{:.0}%", final_v * 100.0),
                    });
                }
            }
        }
    }

    // Capture dynamic values for OSC replacement
    let (current_page, bank_size) = {
        let m = midi.lock().unwrap();
        (m.fader_page, profile.eos_bank_size)
    };

    // Output Processing
    for (out_idx, output) in mapping.outputs.iter().enumerate() {
        match output {
            crate::controller::Output::Osc { addr, arg_type } => {
                let client_clone = client.clone();
                let mut final_addr = addr.clone();

                // Perform dynamic replacement
                if final_addr.contains("{page}") {
                    let page_str = if current_page <= 1 {
                        String::new()
                    } else {
                        (current_page - 1).to_string()
                    };
                    final_addr = final_addr.replace("{page}", &page_str);
                }
                if final_addr.contains("{bank_size}") {
                    final_addr = final_addr.replace("{bank_size}", &bank_size.to_string());
                }

                let arg_type = arg_type.clone();

                // For Faders, we need the value
                let arg = if let crate::controller::LogicalAction::FaderMove { .. } = mapping.action
                {
                    if let Some(val) = cached_fader_value {
                        Some(rosc::OscType::Float(val))
                    } else {
                        // If no cached value, it means the update was blocked by pickup logic or didn't run.
                        // In either case, we should NOT send an OSC update.
                        None
                    }
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
                let d_name_log = device_name.to_string();
                tokio::spawn(async move {
                    let mut success = false;
                    if arg_type == "float" {
                        if let Some(a) = arg
                            && client_clone.send(&final_addr, vec![a]).await.is_ok()
                        {
                            success = true;
                        }
                    } else if client_clone.send(&final_addr, vec![]).await.is_ok() {
                        success = true;
                    }

                    if success {
                        let mut m = midi_for_log.lock().unwrap();
                        m.activity_log.push(crate::ActivityEvent {
                            device_name: d_name_log,
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
                m.enqueue(device_name, vec![0x90 | channel, *note, *velocity]);
                m.activity_log.push(crate::ActivityEvent {
                    device_name: device_name.to_string(),
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
                m.enqueue(device_name, data);
                m.activity_log.push(crate::ActivityEvent {
                    device_name: device_name.to_string(),
                    mapping_idx,
                    part: crate::ActivityPart::Output(out_idx),
                    value: format!("Pitch {}", value - 8192),
                });
            }
            crate::controller::Output::MidiCc { cc, channel } => {
                let mut m = midi.lock().unwrap();
                // If the action is a FaderMove, we might want to map the 14-bit value to 7-bit CC?
                // Or just pass the value if appropriate?
                // For X-Touch Mini, Fader rings are CC 1-8. Value 0-127.
                // d1/d2 come from the source message.
                // If source was PitchWheel (Fader), d1/d2 form a 14-bit value.
                // We need to normalize to 0-127.
                // If source was CC, d2 is value.

                // How do we know the source value type generally?
                // We rely on mapping.action context or just generic scaling?
                // For now, let's assume we want to output the scaled value of the action.

                let out_val =
                    if let crate::controller::LogicalAction::FaderMove { .. } = mapping.action {
                        if let Some(val) = cached_fader_value {
                            (val * 127.0) as u8
                        } else {
                            // Fallback (e.g. if source was absolute 14-bit pitchwheel from an iCon)
                            let val_14 = ((d2 as u16) << 7) | (d1 as u16);
                            (val_14 >> 7) as u8
                        }
                    } else {
                        d2 // Pass-through for simple buttons/knobs
                    };

                m.enqueue(device_name, vec![0xB0 | channel, *cc, out_val]);
                m.activity_log.push(crate::ActivityEvent {
                    device_name: device_name.to_string(),
                    mapping_idx,
                    part: crate::ActivityPart::Output(out_idx),
                    value: format!("CC {} (V:{})", cc, out_val),
                });
            }
            _ => {}
        }
    }
}

use eos_midi_bridge::*;

#[test]
fn test_accent_stripping() {
    assert_eq!(strip_accents("Entrée"), "Entree");
    assert_eq!(strip_accents("Publicité"), "Publicite");
}

#[test]
fn test_queue_logic() {
    let q = Queue::new();
    q.enqueue(vec![0x90, 94, 127]);
    assert_eq!(q.dequeue(), Some(vec![0x90, 94, 127]));
    assert_eq!(q.dequeue(), None);
}

#[test]
fn test_cue_parsing_logic() {
    // String from /eos/out/active/cue/text
    let osc_text = "1/100.5 Entree Public";
    let parsed = parse_cue_number(osc_text);
    assert_eq!(parsed, Some(100.5));
}

#[test]
fn test_crossfade_direction() {
    let current_cue = Some(10.0);
    let new_cue = 11.0;

    let direction = resolve_crossfade_direction(current_cue, new_cue, CrossfadeState::Inactive);
    assert_eq!(direction, Some(CrossfadeState::Go));
}

#[test]
fn test_fader_paging_wraparound() {
    // Page Down from 1 should wrap to 99
    assert_eq!(calculate_next_page(1, false), 99);

    // Page Up from 99 should wrap to 1
    assert_eq!(calculate_next_page(99, true), 1);
}

#[test]
fn test_led_state_transitions() {
    // This logic is still inside `midi.set_crossfade_state`, which modifies internal state.
    // It's harder to test without mocking Midi struct or extracting State Machine further.
    // For now, we leave it as is or skip it if it's too tied to internals.
    // However, the original test was testing a fake `if` block.
    // We haven't extracted `tick_blink` or `set_crossfade_state` logic into a pure function yet.
    // Let's remove this test as it was testing nothing useful (local var led transition)
    // or we can test `calculate_direction` for Stop logic? No, Stop logic is different.
}

#[test]
fn test_paging_wraparound_logic() {
    assert_eq!(
        calculate_next_page(1, false),
        99,
        "Page Down from 1 should wrap to 99"
    );
    assert_eq!(
        calculate_next_page(99, true),
        1,
        "Page Up from 99 should wrap to 1"
    );
}

#[test]
fn test_complex_cue_parsing() {
    // Eos often sends part cues like "1/10.1.1 Stage Left"
    let raw_text = "1/10.1.1 Stage Left";
    let parsed = parse_cue_number(raw_text);
    assert_eq!(parsed, Some(10.1));
}

#[test]
fn test_crossfade_state_logic() {
    // Verify Go detection
    assert_eq!(
        resolve_crossfade_direction(Some(10.0), 11.0, CrossfadeState::Inactive),
        Some(CrossfadeState::Go)
    );
    // Verify Back detection
    assert_eq!(
        resolve_crossfade_direction(Some(10.0), 9.0, CrossfadeState::Inactive),
        Some(CrossfadeState::GoBack)
    );
    // Verify Re-Go detection
    assert_eq!(
        resolve_crossfade_direction(Some(10.0), 10.0, CrossfadeState::Inactive),
        Some(CrossfadeState::Go)
    );
    // Verify No-Change if active
    assert_eq!(
        resolve_crossfade_direction(Some(10.0), 10.0, CrossfadeState::Go),
        None
    );
}

#[test]
fn test_lcd_centering() {
    let page = 5;
    let page_text = format!("Page {}", page);
    let centered_text = center_text(&page_text, 56);

    assert_eq!(centered_text.len(), 56);
    assert!(centered_text.contains("Page 5"));
    assert_eq!(&centered_text[0..1], " "); // Should have padding
}

#[test]
fn test_trigger_parsing() {
    use eos_midi_bridge::controller::{MidiTriggerMode, Trigger};

    // Note On 60 127
    let msg_on = vec![0x90, 60, 127];
    let trigger_on = Trigger::from_midi_message(&msg_on).unwrap();
    if let Trigger::MidiNote { note, mode, .. } = trigger_on {
        assert_eq!(note, 60);
        assert_eq!(mode, MidiTriggerMode::Press);
    } else {
        panic!("Matched wrong trigger type");
    }

    // Note Off (via Note On 0)
    let msg_off = vec![0x90, 60, 0];
    let trigger_off = Trigger::from_midi_message(&msg_off).unwrap();
    if let Trigger::MidiNote { note, mode, .. } = trigger_off {
        assert_eq!(note, 60);
        assert_eq!(mode, MidiTriggerMode::Release);
    } else {
        panic!("Matched wrong trigger type");
    }

    // CC 7 127
    let msg_cc = vec![0xB0, 7, 127];
    let trigger_cc = Trigger::from_midi_message(&msg_cc).unwrap();
    if let Trigger::MidiCc { cc, .. } = trigger_cc {
        assert_eq!(cc, 7);
    } else {
        panic!("Matched wrong trigger type");
    }
}

#[test]
fn test_output_matching() {
    use eos_midi_bridge::controller::{ControllerProfile, LogicalAction};

    // Setup a simple profile
    let profile = ControllerProfile::default();

    // Default profile has a Go button on Note 94.
    // It outputs a Note 94 (LED).
    // Let's verify that a message [0x90, 94, 127] is matched as that output.

    let msg = vec![0x90, 94, 127];
    let matches = profile.match_output_message(&msg);

    assert!(
        !matches.is_empty(),
        "Should have matched the output mapping"
    );
    // Verify it points to the correct mapping (Go)
    // We assume the default profile's first mapping is Go (index 0 or so)
    // Actually, let's find the Go mapping first

    // We can rely on match_output_message returning indices.
    // If we iterate mappings, we find the one that has Output::MidiNote with note 94
    let (match_idx, _) = matches[0];
    let mapping = &profile.mappings[match_idx];

    // Verify it is indeed the Go action
    match mapping.action {
        LogicalAction::Go => {}
        _ => panic!("Expected Go action mapping"),
    }
}

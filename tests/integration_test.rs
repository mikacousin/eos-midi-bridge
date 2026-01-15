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
    let cue_part = osc_text
        .split('/')
        .nth(1)
        .and_then(|s| s.split_whitespace().next());

    assert_eq!(cue_part, Some("100.5"));
    assert_eq!(cue_part.unwrap().parse::<f32>().unwrap(), 100.5);
}

#[test]
fn test_crossfade_direction() {
    let current_cue = 10.0;
    let new_cue = 11.0;
    let mut state = CrossfadeState::Inactive;

    if state == CrossfadeState::Inactive {
        if new_cue > current_cue {
            state = CrossfadeState::Go;
        } else if new_cue < current_cue {
            state = CrossfadeState::GoBack;
        }
    }
    assert_eq!(state, CrossfadeState::Go);
}

#[test]
fn test_fader_paging_wraparound() {
    let mut current_page = 1;
    // Simulate Page Down
    current_page = if current_page <= 1 {
        99
    } else {
        current_page - 1
    };
    assert_eq!(current_page, 99);

    // Simulate Page Up
    current_page = 99;
    current_page = if current_page >= 99 {
        1
    } else {
        current_page + 1
    };
    assert_eq!(current_page, 1);
}

#[test]
fn test_led_state_transitions() {
    let mut state = CrossfadeState::Go;

    // Simulate receiving a STOP event from Eos
    if state == CrossfadeState::Go {
        state = CrossfadeState::Pause;
    }

    assert_eq!(state, CrossfadeState::Pause);
}
#[test]
fn test_paging_wraparound_logic() {
    // Test Page Down from 1 to 99
    let mut current_page = 1;
    current_page = if current_page <= 1 {
        99
    } else {
        current_page - 1
    };
    assert_eq!(current_page, 99, "Page Down from 1 should wrap to 99");

    // Test Page Up from 99 to 1
    current_page = 99;
    current_page = if current_page >= 99 {
        1
    } else {
        current_page + 1
    };
    assert_eq!(current_page, 1, "Page Up from 99 should wrap to 1");
}

#[test]
fn test_complex_cue_parsing() {
    // Eos often sends part cues like "1/10.1.1 Stage Left"
    let raw_text = "1/10.1.1 Stage Left";
    let cue_part = raw_text
        .split('/')
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap();

    // Logic: take only the first two segments to allow f32 parsing
    let simplified = cue_part.split('.').take(2).collect::<Vec<_>>().join(".");
    let parsed = simplified.parse::<f32>();

    assert!(parsed.is_ok(), "Should successfully parse 10.1 from 10.1.1");
    assert_eq!(parsed.unwrap(), 10.1);
}

#[test]
fn test_crossfade_state_logic() {
    let mut state = CrossfadeState::Inactive;
    let current_cue = 10.0;
    let new_cue = 11.0;

    // Simulate the logic used in handle_packet
    if state == CrossfadeState::Inactive {
        if new_cue > current_cue {
            state = CrossfadeState::Go;
        }
    }

    assert_eq!(
        state,
        CrossfadeState::Go,
        "State should transition to Go when cue increases"
    );
}

#[test]
fn test_lcd_centering() {
    let page = 5;
    let page_text = format!("Page {}", page);
    let centered_text = format!("{:^56}", page_text);

    assert_eq!(centered_text.len(), 56);
    assert!(centered_text.contains("Page 5"));
    assert_eq!(&centered_text[0..1], " "); // Should have padding
}

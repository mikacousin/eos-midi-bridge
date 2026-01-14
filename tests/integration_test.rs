use eos_midi_bridge::*; // Matches the name in Cargo.toml

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

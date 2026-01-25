pub mod config;
pub mod controller;
pub mod midi;
pub mod nodes;
pub mod osc;

use deunicode::deunicode;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ============================================================================
// Types & Messaging
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogSource {
    MidiIn,
    MidiOut,
    OscIn,
    OscOut,
}

#[derive(Clone)]
pub struct LogEntry {
    pub time: std::time::Instant,
    pub source: LogSource,
    pub content: String,
}

pub enum MackieEvent {
    MidiIn { device_name: String, data: Vec<u8> },
}

pub enum SystemCommand {
    Reconfigure(crate::config::BridgeConfig),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CrossfadeState {
    Inactive,
    Go,
    GoBack,
    Pause,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActivityPart {
    Trigger,
    Action,
    Output(usize),
}

#[derive(Debug, Clone)]
pub struct ActivityEvent {
    pub device_name: String,
    pub mapping_idx: usize,
    pub part: ActivityPart,
    pub value: String,
}

#[derive(Clone)]
pub struct Queue<T> {
    elements: Arc<Mutex<VecDeque<T>>>,
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
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

pub fn strip_accents(text: &str) -> String {
    deunicode(text)
}

pub fn clean_midi_name(name: &str) -> String {
    name.split(':').next().unwrap_or(name).trim().to_string()
}

pub fn center_text(text: &str, width: usize) -> String {
    format!("{:^width$}", text, width = width)
}

pub fn parse_cue_number(text: &str) -> Option<f32> {
    text.split('/')
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .map(|s| s.split('.').take(2).collect::<Vec<_>>().join("."))
        .and_then(|s| s.parse::<f32>().ok())
}

// Actually, looking at the original code in osc.rs:276:
// if current_state == Inactive { intended = Go } else { /* do nothing to intended */ }
// So if we return Option<CrossfadeState>, we can signify "change" vs "no change".
pub fn resolve_crossfade_direction(
    current_cue: Option<f32>,
    new_cue: f32,
    current_state: CrossfadeState,
) -> Option<CrossfadeState> {
    if let Some(old_cue) = current_cue {
        if new_cue > old_cue {
            Some(CrossfadeState::Go)
        } else if new_cue < old_cue {
            Some(CrossfadeState::GoBack)
        } else if current_state == CrossfadeState::Inactive {
            Some(CrossfadeState::Go)
        } else {
            None
        }
    } else {
        Some(CrossfadeState::Go)
    }
}

pub fn calculate_next_page(current_page: u8, go_up: bool) -> u8 {
    if go_up {
        if current_page >= 99 {
            1
        } else {
            current_page + 1
        }
    } else if current_page <= 1 {
        99
    } else {
        current_page - 1
    }
}

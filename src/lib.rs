pub mod config;
pub mod midi;
pub mod osc;

use deunicode::deunicode;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ============================================================================
// Types & Messaging
// ============================================================================

pub enum MackieEvent {
    MidiIn(Vec<u8>),
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

pub fn strip_accents(text: &str) -> String {
    deunicode(text)
}

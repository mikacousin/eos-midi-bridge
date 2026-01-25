use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DisplayProfile {
    pub sysex_prefix: Vec<u8>,
    pub line_length: usize,
    pub strip_width: usize,
    pub line_offsets: Vec<u8>,
    pub visible_faders: Option<Vec<usize>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MidiTriggerMode {
    Press,
    Release,
    Both,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MidiCcTriggerMode {
    Absolute,
    // Standard relative: 1..64 = +1..+64, 65..127 = -1..-63
    RelativeStandard,
    // Behringer often uses: 1..7 (speed +) and 65..71 (speed -) OR 127/123 etc.
    // Let's implement a generic "RelativeSigned" where < 64 is +, > 64 is -.
    // For Behringer X-Touch Mini in 'Relative 1': CW = 1..7, CCW = 65..71.
    RelativeBehringer,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    MidiNote {
        note: u8,
        channel: u8,
        mode: MidiTriggerMode,
    },
    MidiCc {
        cc: u8,
        channel: u8,
        #[serde(default = "default_cc_mode")]
        mode: MidiCcTriggerMode,
    },
    MidiPitchwheel {
        channel: u8,
    },
    Osc {
        addr: String,
    },
}

impl Trigger {
    pub fn from_midi_message(msg: &[u8]) -> Option<Self> {
        if msg.len() < 3 {
            return None;
        }
        let (status, d1, d2) = (msg[0], msg[1], msg[2]);
        let (msg_type, chan) = (status & 0xF0, status & 0x0F);

        match msg_type {
            0x90 => {
                let mode = if d2 == 127 {
                    MidiTriggerMode::Press
                } else {
                    MidiTriggerMode::Release
                };
                Some(Trigger::MidiNote {
                    note: d1,
                    channel: chan,
                    mode,
                })
            }
            0xB0 => Some(Trigger::MidiCc {
                cc: d1,
                channel: chan,
                mode: MidiCcTriggerMode::Absolute, // Default for sniffing
            }),
            0xE0 => Some(Trigger::MidiPitchwheel { channel: chan }),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Output {
    Osc {
        addr: String,
        #[serde(default)]
        arg_type: String,
    },
    MidiNote {
        note: u8,
        channel: u8,
        velocity: u8,
    },
    MidiPitchwheel {
        channel: u8,
    },
    LcdStrip {
        line: u8,
        strip: u8,
    },
    LcdText {
        line: u8,
        text: String,
    },
    MidiCc {
        cc: u8,
        channel: u8,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogicalAction {
    Go,
    Stop,
    Resume,
    FaderPageUp,
    FaderPageDown,
    FaderMove { index: usize },
    // MasterFaderMove removed
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Mapping {
    pub trigger: Trigger,
    pub action: LogicalAction,
    pub outputs: Vec<Output>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ControllerProfile {
    pub name: String,
    pub eos_bank_size: usize,
    pub display: DisplayProfile,
    pub mappings: Vec<Mapping>,
    #[serde(default)]
    pub startup_messages: Vec<Vec<u8>>,
}

impl Default for ControllerProfile {
    fn default() -> Self {
        Self {
            name: "iCon Platform M+ (Internal)".to_string(),
            eos_bank_size: 10,
            display: DisplayProfile {
                sysex_prefix: vec![0x00, 0x00, 0x66, 0x14, 0x12],
                line_length: 56,
                strip_width: 7,
                line_offsets: vec![56, 0],
                visible_faders: None,
            },
            startup_messages: Vec::new(),
            mappings: vec![
                // Example: Go button
                Mapping {
                    trigger: Trigger::MidiNote {
                        note: 94,
                        channel: 0,
                        mode: MidiTriggerMode::Press,
                    },
                    action: LogicalAction::Go,
                    outputs: vec![
                        Output::Osc {
                            addr: "/eos/user/1/key/go_0".to_string(),
                            arg_type: "none".to_string(),
                        },
                        Output::MidiNote {
                            note: 94,
                            channel: 0,
                            velocity: 127,
                        }, // LED ON
                    ],
                },
                // Example: Fader 1
                Mapping {
                    trigger: Trigger::MidiPitchwheel { channel: 0 },
                    action: LogicalAction::FaderMove { index: 0 },
                    outputs: vec![
                        Output::Osc {
                            addr: "/eos/user/1/fader/1/1".to_string(),
                            arg_type: "float".to_string(),
                        },
                        Output::MidiPitchwheel { channel: 0 }, // FB
                    ],
                },
            ],
        }
    }
}

impl ControllerProfile {
    pub fn find_mapping_by_action(&self, action: &LogicalAction) -> Option<&Mapping> {
        self.mappings.iter().find(|m| m.action == *action)
    }

    pub fn find_mappings_by_trigger(&self, trigger: &Trigger) -> Vec<&Mapping> {
        self.mappings
            .iter()
            .filter(|m| m.trigger == *trigger)
            .collect()
    }

    pub fn get_midi_output_for_action(&self, action: LogicalAction) -> Option<(u8, u8, u8)> {
        if let Some(mapping) = self.find_mapping_by_action(&action) {
            for output in &mapping.outputs {
                if let Output::MidiNote {
                    note,
                    channel,
                    velocity,
                } = output
                {
                    return Some((*note, *channel, *velocity));
                }
            }
        }
        None
    }

    pub fn inject_feedback_mappings(&mut self) {
        let mut new_mappings = Vec::new();

        for mapping in &self.mappings {
            match mapping.action {
                LogicalAction::FaderMove { index } => {
                    // Determine the feedback OSC address (1-indexed based on index)
                    let feedback_addr = format!("/eos/fader/1/{}", index + 1);

                    // Use existing MIDI outputs (CC or PitchWheel) for feedback
                    let feedback_outputs: Vec<Output> = mapping
                        .outputs
                        .iter()
                        .filter(|o| {
                            matches!(o, Output::MidiCc { .. } | Output::MidiPitchwheel { .. })
                        })
                        .cloned()
                        .collect();

                    if !feedback_outputs.is_empty() {
                        new_mappings.push(Mapping {
                            trigger: Trigger::Osc {
                                addr: feedback_addr,
                            },
                            action: mapping.action.clone(),
                            outputs: feedback_outputs,
                        });
                    }
                }
                LogicalAction::Go => {
                    new_mappings.push(Mapping {
                        trigger: Trigger::Osc {
                            addr: "/eos/out/event/cue/1/0/resume".to_string(),
                        },
                        action: mapping.action.clone(),
                        outputs: mapping
                            .outputs
                            .iter()
                            .filter(|o| matches!(o, Output::MidiNote { .. }))
                            .cloned()
                            .collect(),
                    });
                }
                LogicalAction::Stop => {
                    new_mappings.push(Mapping {
                        trigger: Trigger::Osc {
                            addr: "/eos/out/event/cue/1/0/stop".to_string(),
                        },
                        action: mapping.action.clone(),
                        outputs: mapping
                            .outputs
                            .iter()
                            .filter(|o| matches!(o, Output::MidiNote { .. }))
                            .cloned()
                            .collect(),
                    });
                }

                _ => {}
            }
        }

        self.mappings.extend(new_mappings);
    }

    pub fn match_output_message(&self, msg: &[u8]) -> Vec<(usize, crate::ActivityPart)> {
        let mut results = Vec::new();

        // Helper to check if a message matches a Note output
        let matches_note = |msg: &[u8], channel: u8, note: u8| -> bool {
            if msg.len() < 3 {
                return false;
            }
            let status = msg[0] & 0xF0;
            let msg_channel = msg[0] & 0x0F;
            if msg_channel != channel {
                return false;
            }
            if msg[1] != note {
                return false;
            }
            status == 0x90 || status == 0x80 // Note On or Note Off
        };

        // Helper to check if a message matches a PitchWheel output
        let matches_pitch = |msg: &[u8], channel: u8| -> bool {
            if msg.len() < 3 {
                return false;
            }
            let status = msg[0] & 0xF0;
            let msg_channel = msg[0] & 0x0F;
            status == 0xE0 && msg_channel == channel
        };

        for (idx, mapping) in self.mappings.iter().enumerate() {
            for (out_idx, output) in mapping.outputs.iter().enumerate() {
                let matched = match output {
                    Output::MidiNote { channel, note, .. } => matches_note(msg, *channel, *note),
                    Output::MidiPitchwheel { channel } => matches_pitch(msg, *channel),
                    Output::MidiCc { channel, cc } => {
                        // Check if msg is CC and matches channel/cc
                        if msg.len() >= 3 && (msg[0] & 0xF0) == 0xB0 {
                            (msg[0] & 0x0F) == *channel && msg[1] == *cc
                        } else {
                            false
                        }
                    }
                    _ => false,
                };

                if matched {
                    results.push((idx, crate::ActivityPart::Output(out_idx)));
                }
            }
        }
        results
    }
}

pub fn load_profile(path: &str) -> Result<ControllerProfile> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read controller profile: {}", path))?;

    let profile: ControllerProfile = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse controller profile JSON: {}", path))?;

    Ok(profile)
}

fn default_cc_mode() -> MidiCcTriggerMode {
    MidiCcTriggerMode::Absolute
}

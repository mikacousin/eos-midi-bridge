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
    },
    MidiPitchwheel {
        channel: u8,
    },
    Osc {
        addr: String,
    },
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

                    // Synthesize MIDI Pitchwheel output for motor feedback
                    let feedback_outputs = vec![Output::MidiPitchwheel {
                        channel: index as u8,
                    }];

                    new_mappings.push(Mapping {
                        trigger: Trigger::Osc {
                            addr: feedback_addr,
                        },
                        action: mapping.action.clone(), 
                        outputs: feedback_outputs,
                    });
                }
                LogicalAction::Go => {
                     new_mappings.push(Mapping {
                        trigger: Trigger::Osc { addr: "/eos/out/event/cue/1/0/resume".to_string() },
                        action: mapping.action.clone(),
                        outputs: mapping.outputs.iter().filter(|o| matches!(o, Output::MidiNote { .. })).cloned().collect(),
                    });
                }
                LogicalAction::Stop => {
                     new_mappings.push(Mapping {
                        trigger: Trigger::Osc { addr: "/eos/out/event/cue/1/0/stop".to_string() },
                        action: mapping.action.clone(),
                        outputs: mapping.outputs.iter().filter(|o| matches!(o, Output::MidiNote { .. })).cloned().collect(),
                    });
                }

                _ => {}
            }
        }

        self.mappings.extend(new_mappings);
    }
}

pub fn load_profile(path: &str) -> Result<ControllerProfile> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read controller profile: {}", path))?;

    let profile: ControllerProfile = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse controller profile JSON: {}", path))?;

    Ok(profile)
}

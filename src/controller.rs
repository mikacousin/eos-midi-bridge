use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FaderProtocol {
    Pitchwheel,
    ControlChange,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DisplayProfile {
    pub sysex_prefix: Vec<u8>,
    pub line_length: usize,
    pub strip_width: usize,
    pub line_offsets: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ButtonProfile {
    pub go_note: u8,
    pub stop_note: u8,
    pub page_up_note: u8,
    pub page_down_note: u8,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ControllerProfile {
    pub name: String,
    pub fader_count: usize,
    pub eos_bank_size: usize,
    pub fader_protocol: FaderProtocol,
    pub fader_channels: Vec<u8>,
    pub display: DisplayProfile,
    pub buttons: ButtonProfile,
}

impl Default for ControllerProfile {
    fn default() -> Self {
        // Fallback to iCon Platform M+ settings
        Self {
            name: "iCon Platform M+ (Internal)".to_string(),
            fader_count: 9,
            eos_bank_size: 10,
            fader_protocol: FaderProtocol::Pitchwheel,
            fader_channels: vec![0, 1, 2, 3, 4, 5, 6, 7, 8],
            display: DisplayProfile {
                sysex_prefix: vec![0x00, 0x00, 0x66, 0x14, 0x12],
                line_length: 56,
                strip_width: 7,
                line_offsets: vec![56, 0],
            },
            buttons: ButtonProfile {
                go_note: 94,
                stop_note: 93,
                page_up_note: 46,
                page_down_note: 47,
            },
        }
    }
}

pub fn load_profile(path: &str) -> Result<ControllerProfile> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read controller profile: {}", path))?;

    let profile: ControllerProfile = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse controller profile JSON: {}", path))?;

    Ok(profile)
}

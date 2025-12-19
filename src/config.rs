use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub enum MidiEventType {
    PitchBend,
    NoteOn,
    ControlChange,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MidiOscMapping {
    pub event_type: MidiEventType,
    pub data_number: u8,
    pub osc_address: String,
    pub fixed_osc_value: Option<f32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub eos_ip: String,
    pub eos_port: u16,
    pub listen_port: u16,
    pub fader_bank_size: u8,
    pub mappings: Vec<MidiOscMapping>,
}

impl Default for Config {
    fn default() -> Self {
        let mut mappings = vec![];

        // Default: Faders 1-9 (Physical Faders 1-8 + Master Fader)
        for i in 1..=9 {
            mappings.push(MidiOscMapping {
                event_type: MidiEventType::PitchBend,
                data_number: i as u8,
                osc_address: format!("/eos/fader/1/{}", i),
                fixed_osc_value: None,
            });
        }

        // Default: Page Navigation (iCon Bank Buttons)
        mappings.push(MidiOscMapping {
            event_type: MidiEventType::NoteOn,
            data_number: 46, // Bank Left
            osc_address: "/eos/fader/1/page/-1".to_string(),
            fixed_osc_value: Some(1.0),
        });
        mappings.push(MidiOscMapping {
            event_type: MidiEventType::NoteOn,
            data_number: 47, // Bank Right
            osc_address: "/eos/fader/1/page/+1".to_string(),
            fixed_osc_value: Some(1.0),
        });

        Config {
            eos_ip: "127.0.0.1".to_string(),
            eos_port: 8000,
            listen_port: 9000,
            fader_bank_size: 10,
            mappings,
        }
    }
}

pub fn float_to_pitch_bend(value: f32) -> u16 {
    (value.clamp(0.0, 1.0) * 16383.0).round() as u16
}

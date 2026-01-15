use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BridgeConfig {
    pub eos_ip: String,
    pub eos_osc_port: u16,
    pub bridge_listen_port: u16,
    pub midi_in_name: Option<String>,
    pub midi_out_name: Option<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            eos_ip: "127.0.0.1".to_string(),
            eos_osc_port: 8000,
            bridge_listen_port: 8001,
            midi_in_name: None,
            midi_out_name: None,
        }
    }
}

pub fn load_config() -> BridgeConfig {
    confy::load("eos-midi-bridge", None).unwrap_or_default()
}

pub fn store_config(config: &BridgeConfig) -> Result<(), confy::ConfyError> {
    confy::store("eos-midi-bridge", None, config)
}

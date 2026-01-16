use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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

impl BridgeConfig {
    // Validate the configuration values
    pub fn validate(&self) -> Result<()> {
        if self.eos_ip.is_empty() {
            anyhow::bail!("Eos IP address cannot be empty");
        }

        // Basic IP validation
        if !self.eos_ip.contains('.') && self.eos_ip != "localhost" {
            anyhow::bail!("Invalid Eos IP address format");
        }

        if self.eos_osc_port == 0 {
            anyhow::bail!("Eos OSC port cannot be 0");
        }

        if self.bridge_listen_port == 0 {
            anyhow::bail!("Bridge listen port cannot be 0");
        }

        if self.eos_osc_port == self.bridge_listen_port {
            anyhow::bail!("Eos OSC port and bridge listen port cannot be the same");
        }

        Ok(())
    }
}

pub fn load_config() -> Result<BridgeConfig> {
    let config: BridgeConfig =
        confy::load("eos-midi-bridge", None).context("Failed to load configuration from disk")?;

    config
        .validate()
        .context("Configuration validation failed")?;

    Ok(config)
}

pub fn store_config(config: &BridgeConfig) -> Result<()> {
    config
        .validate()
        .context("Cannot save invalid configuration")?;
    confy::store("eos-midi-bridge", None, config)
        .context("Failed to write configuration to disk")?;

    Ok(())
}

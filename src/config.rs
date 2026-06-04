use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub api: ApiConfig,
    pub node: NodeConfig,
    pub daemon: DaemonConfig,
    pub sdr: SdrConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ApiConfig {
    pub url: String,
    pub key: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NodeConfig {
    pub id: String,
    pub hardware_type: String,
    pub label: String,
    pub location: LocationConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LocationConfig {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DaemonConfig {
    pub heartbeat_interval_secs: u64,
    pub submit_interval_secs: u64,
    pub status_bind: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SdrConfig {
    pub device_index: u32,
    pub sample_rate: u32,
    pub center_frequency: u64,
    pub gain: i32,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config at {}: {}", path.display(), e))?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

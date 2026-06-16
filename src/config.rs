use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub api: ApiConfig,
    pub node: NodeConfig,
    pub daemon: DaemonConfig,
    pub sdr: SdrConfig,
    #[serde(default)]
    pub scanning: Option<ScanningConfig>,
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
    #[serde(default)]
    pub device_index: Option<u32>,
    pub sample_rate: u32,
    #[serde(default = "default_center_frequency")]
    pub center_frequency: u64,
    pub gain: i32,
    #[serde(default = "default_dbm_offset")]
    pub dbm_offset: f32,
}

fn default_center_frequency() -> u64 {
    100_000_000
}

fn default_dbm_offset() -> f32 {
    -30.0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScanningConfig {
    #[serde(default = "default_scan_mode")]
    pub mode: String,
    #[serde(default = "default_scan_interval")]
    pub interval_seconds: u32,
    #[serde(default = "default_weight_lte")]
    pub weight_lte: u32,
    #[serde(default = "default_weight_dtv")]
    pub weight_dtv: u32,
    #[serde(default = "default_weight_fm")]
    pub weight_fm: u32,
    #[serde(default = "default_retune_delay")]
    pub retune_delay_ms: u32,
    #[serde(default = "default_capture_retries")]
    pub capture_retries: u32,
    #[serde(default)]
    pub fm: FmScanConfig,
    #[serde(default)]
    pub lte: LteScanConfig,
    #[serde(default)]
    pub dtv: DtvScanConfig,
}

fn default_scan_mode() -> String {
    "both".to_string()
}
fn default_scan_interval() -> u32 {
    30
}
fn default_weight_lte() -> u32 {
    3
}
fn default_weight_dtv() -> u32 {
    2
}
fn default_weight_fm() -> u32 {
    1
}
fn default_retune_delay() -> u32 {
    100
}
fn default_capture_retries() -> u32 {
    3
}

impl Default for ScanningConfig {
    fn default() -> Self {
        Self {
            mode: default_scan_mode(),
            interval_seconds: default_scan_interval(),
            weight_lte: default_weight_lte(),
            weight_dtv: default_weight_dtv(),
            weight_fm: default_weight_fm(),
            retune_delay_ms: default_retune_delay(),
            capture_retries: default_capture_retries(),
            fm: FmScanConfig::default(),
            lte: LteScanConfig::default(),
            dtv: DtvScanConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FmScanConfig {
    #[serde(default)]
    pub frequencies: Vec<u64>,
    #[serde(default = "default_fm_sweep_start")]
    pub sweep_start: u64,
    #[serde(default = "default_fm_sweep_end")]
    pub sweep_end: u64,
    #[serde(default = "default_fm_sweep_step")]
    pub sweep_step: u64,
    #[serde(default = "default_fm_sweep_threshold")]
    pub sweep_threshold: f32,
}

impl Default for FmScanConfig {
    fn default() -> Self {
        Self {
            frequencies: Vec::new(),
            sweep_start: default_fm_sweep_start(),
            sweep_end: default_fm_sweep_end(),
            sweep_step: default_fm_sweep_step(),
            sweep_threshold: default_fm_sweep_threshold(),
        }
    }
}

fn default_fm_sweep_start() -> u64 {
    87_500_000
}
fn default_fm_sweep_end() -> u64 {
    108_000_000
}
fn default_fm_sweep_step() -> u64 {
    200_000
}
fn default_fm_sweep_threshold() -> f32 {
    -40.0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LteScanConfig {
    #[serde(default = "default_lte_frequencies")]
    pub frequencies: Vec<u64>,
    #[serde(default)]
    pub sweep_bands: Vec<BandRange>,
}

impl Default for LteScanConfig {
    fn default() -> Self {
        Self {
            frequencies: default_lte_frequencies(),
            sweep_bands: Vec::new(),
        }
    }
}

fn default_lte_frequencies() -> Vec<u64> {
    vec![631_000_000, 637_000_000, 737_000_000, 881_000_000]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BandRange {
    pub start: u64,
    pub end: u64,
    pub step: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DtvScanConfig {
    #[serde(default)]
    pub frequencies: Vec<u64>,
    #[serde(default = "default_dtv_sweep_start")]
    pub sweep_start: u64,
    #[serde(default = "default_dtv_sweep_end")]
    pub sweep_end: u64,
    #[serde(default = "default_dtv_sweep_step")]
    pub sweep_step: u64,
    #[serde(default = "default_dtv_sweep_threshold")]
    pub sweep_threshold: f32,
}

impl Default for DtvScanConfig {
    fn default() -> Self {
        Self {
            frequencies: Vec::new(),
            sweep_start: default_dtv_sweep_start(),
            sweep_end: default_dtv_sweep_end(),
            sweep_step: default_dtv_sweep_step(),
            sweep_threshold: default_dtv_sweep_threshold(),
        }
    }
}

fn default_dtv_sweep_start() -> u64 {
    470_000_000
}
fn default_dtv_sweep_end() -> u64 {
    608_000_000
}
fn default_dtv_sweep_step() -> u64 {
    6_000_000
}
fn default_dtv_sweep_threshold() -> f32 {
    -45.0
}

impl Config {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.api.url.is_empty() {
            errors.push("api.url: cannot be empty".into());
        } else if !self.api.url.starts_with("http") {
            errors.push(format!(
                "api.url: '{}' is not a valid URL (must start with http/https)",
                self.api.url
            ));
        }
        if self.api.key.is_empty() || self.api.key == "YOUR_API_KEY_HERE" {
            errors.push("api.key: not configured — run 'azimuth-daemon --setup' to configure".into());
        }

        if self.node.location.latitude < -90.0 || self.node.location.latitude > 90.0 {
            errors.push(format!(
                "node.location.latitude: {} is out of range (-90 to 90)",
                self.node.location.latitude
            ));
        }
        if self.node.location.longitude < -180.0 || self.node.location.longitude > 180.0 {
            errors.push(format!(
                "node.location.longitude: {} is out of range (-180 to 180)",
                self.node.location.longitude
            ));
        }
        if self.node.location.latitude == 0.0 && self.node.location.longitude == 0.0 {
            errors.push(
                "node.location: still at default (0,0) — set your actual location".into(),
            );
        }

        if self.sdr.sample_rate == 0 {
            errors.push("sdr.sample_rate: cannot be zero".into());
        }
        if self.sdr.gain > 50 {
            errors.push(format!(
                "sdr.gain: {} is unreasonably high (max ~50 dB)",
                self.sdr.gain
            ));
        }

        if let Some(ref s) = self.scanning {
            if s.interval_seconds == 0 {
                errors.push("scanning.interval_seconds: cannot be zero".into());
            }
            if s.weight_fm == 0 && s.weight_lte == 0 && s.weight_dtv == 0 {
                errors.push(
                    "scanning: all weights are zero — at least one signal type must have weight > 0"
                        .into(),
                );
            }
            for freq in &s.fm.frequencies {
                if *freq < 87_500_000 || *freq > 108_000_000 {
                    errors.push(format!(
                        "scanning.fm.frequencies: {} Hz is outside FM band (87.5-108 MHz)",
                        freq
                    ));
                }
            }
            for freq in &s.lte.frequencies {
                if *freq > 1_766_000_000 {
                    errors.push(format!(
                        "scanning.lte.frequencies: {} Hz exceeds RTL-SDR max (~1.766 GHz)",
                        freq
                    ));
                }
            }
            for freq in &s.dtv.frequencies {
                if *freq < 470_000_000 || *freq > 700_000_000 {
                    errors.push(format!(
                        "scanning.dtv.frequencies: {} Hz is outside UHF TV range (470-700 MHz)",
                        freq
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn has_placeholder_values(&self) -> bool {
        self.api.key.is_empty()
            || self.api.key == "YOUR_API_KEY_HERE"
            || self.node.id.is_empty()
            || self.node.id == "YOUR_NODE_ID_HERE"
    }

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

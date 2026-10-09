use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::regions::RegionResolver;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub api: ApiConfig,
    pub node: NodeConfig,
    pub daemon: DaemonConfig,
    pub sdr: SdrConfig,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default = "default_scanning")]
    pub scanning: Option<ScanningConfig>,
    #[serde(default)]
    pub ntrip: Option<NtripConfig>,
    #[serde(default)]
    pub gps: Option<GpsConfig>,
}

/// One validation problem: the config field it concerns and a message that
/// names the fix. Rendered by the desktop app, so no CLI advice lives here.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigProblem {
    pub field: String,
    pub message: String,
}

/// Why `Config::load_checked` could not produce a config.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadError {
    /// The file (or its directory) does not exist: a first run, not a fault.
    Missing(String),
    /// Present but unreadable or unparsable.
    Failed(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(m) | Self::Failed(m) => write!(f, "{m}"),
        }
    }
}

/// Which region plan `apply_region` ended up applying and why.
#[derive(Debug, Clone, PartialEq)]
pub struct RegionResolution {
    /// Region code actually applied ("FR", "_DEFAULT", ...).
    pub effective: String,
    /// "explicit" | "auto:location" | "default"
    pub source: &'static str,
}

#[derive(Deserialize, Serialize)]
pub struct ApiConfig {
    pub url: String,
    pub key: String,
}

/// Hand-written so the API key can never be printed.
///
/// With `#[derive(Debug)]`, a single `debug!(?config)` anywhere — now that debug
/// logging is a user-facing toggle that writes to a file users are asked to
/// send in — would dump the Bearer key verbatim. This is the control that
/// survives a future contributor who doesn't know that.
impl std::fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiConfig")
            .field("url", &self.url)
            .field("key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeConfig {
    pub id: String,
    pub hardware_type: String,
    pub label: String,
    pub location: LocationConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    #[serde(default = "default_max_tuner_mhz")]
    pub max_tuner_mhz: u64,
}

impl Default for Config {
    /// Safe in-memory defaults used when config.toml is missing or corrupt.
    /// The daemon starts, reports the problem via /status config_error, and
    /// waits for setup instead of crash-looping under the service manager.
    fn default() -> Self {
        Self {
            api: ApiConfig {
                url: "https://azimuth.day".to_string(),
                key: String::new(),
            },
            node: NodeConfig {
                id: String::new(),
                hardware_type: "tier1_rtlsdr".to_string(),
                label: String::new(),
                location: LocationConfig {
                    latitude: 0.0,
                    longitude: 0.0,
                    altitude_m: 0.0,
                },
            },
            daemon: DaemonConfig {
                heartbeat_interval_secs: 15,
                submit_interval_secs: 30,
                status_bind: "127.0.0.1:8420".to_string(),
            },
            sdr: SdrConfig {
                device_index: None,
                sample_rate: 2_048_000,
                center_frequency: default_center_frequency(),
                gain: -1,
                dbm_offset: default_dbm_offset(),
                max_tuner_mhz: default_max_tuner_mhz(),
            },
            region: None,
            scanning: default_scanning(),
            ntrip: None,
            gps: None,
        }
    }
}

fn default_center_frequency() -> u64 {
    100_000_000
}

fn default_dbm_offset() -> f32 {
    -30.0
}

fn default_max_tuner_mhz() -> u64 {
    1766
}

fn default_scanning() -> Option<ScanningConfig> {
    Some(ScanningConfig::default())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScanningConfig {
    /// Master switch for the multi-signal scanner. `false` drops the daemon
    /// back to the legacy single-frequency loop (Stage A behavior).
    #[serde(default = "default_scanning_enabled")]
    pub enabled: bool,
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
    #[serde(default = "default_max_obs_per_cycle")]
    pub max_observations_per_cycle: u32,
    /// Seconds between repeat discovery sweeps. Discovery used to be a one-shot
    /// latch, so whatever a single sweep found at process start was what the
    /// node scanned until it restarted. 0 restores that one-shot behavior.
    #[serde(default = "default_rediscovery_interval")]
    pub rediscovery_interval_secs: u64,
    /// Re-run discovery early while the FM list is below this count. 0 disables
    /// the low-yield trigger. Matches the fallback gate's minimum by default.
    #[serde(default = "default_rediscovery_min_fm_stations")]
    pub rediscovery_min_fm_stations: usize,
    #[serde(default)]
    pub fm: FmScanConfig,
    #[serde(default)]
    pub lte: LteScanConfig,
    #[serde(default)]
    pub dtv: DtvScanConfig,
}

fn default_scanning_enabled() -> bool {
    true
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
fn default_max_obs_per_cycle() -> u32 {
    20
}
/// 6 hours. A full sweep is ~103 FM steps plus LTE/DTV, each with a 256 KB
/// capture — minutes during which nothing is submitted. Nightly is too slow for
/// a user who attaches an antenna after first run; hourly wastes capture time in
/// a healthy environment. 6 h bounds "wrong forever" to 6 h.
fn default_rediscovery_interval() -> u64 {
    21_600
}
fn default_rediscovery_min_fm_stations() -> usize {
    3
}

impl Default for ScanningConfig {
    fn default() -> Self {
        Self {
            enabled: default_scanning_enabled(),
            mode: default_scan_mode(),
            interval_seconds: default_scan_interval(),
            weight_lte: default_weight_lte(),
            weight_dtv: default_weight_dtv(),
            weight_fm: default_weight_fm(),
            retune_delay_ms: default_retune_delay(),
            capture_retries: default_capture_retries(),
            max_observations_per_cycle: default_max_obs_per_cycle(),
            rediscovery_interval_secs: default_rediscovery_interval(),
            rediscovery_min_fm_stations: default_rediscovery_min_fm_stations(),
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
    #[serde(default = "default_fm_sweep_min_snr")]
    pub sweep_min_snr_db: f32,
}

impl Default for FmScanConfig {
    fn default() -> Self {
        Self {
            frequencies: Vec::new(),
            sweep_start: default_fm_sweep_start(),
            sweep_end: default_fm_sweep_end(),
            sweep_step: default_fm_sweep_step(),
            sweep_threshold: default_fm_sweep_threshold(),
            sweep_min_snr_db: default_fm_sweep_min_snr(),
        }
    }
}

fn default_fm_sweep_min_snr() -> f32 {
    8.0
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
    -20.0
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
    /// Region-derived (never written to config.toml): the DTV standard the
    /// scanner routes on ("ATSC", "DVB-T2-694", ...), the channel width and the
    /// first channel number of the raster.
    #[serde(default = "default_dtv_standard", skip_serializing)]
    pub standard: String,
    #[serde(default = "default_dtv_channel_mhz", skip_serializing)]
    pub channel_mhz: u32,
    #[serde(default = "default_dtv_first_channel", skip_serializing)]
    pub first_channel: u32,
}

impl Default for DtvScanConfig {
    fn default() -> Self {
        Self {
            frequencies: Vec::new(),
            sweep_start: default_dtv_sweep_start(),
            sweep_end: default_dtv_sweep_end(),
            sweep_step: default_dtv_sweep_step(),
            sweep_threshold: default_dtv_sweep_threshold(),
            standard: default_dtv_standard(),
            channel_mhz: default_dtv_channel_mhz(),
            first_channel: default_dtv_first_channel(),
        }
    }
}

fn default_dtv_standard() -> String {
    "ATSC".to_string()
}
fn default_dtv_channel_mhz() -> u32 {
    6
}
fn default_dtv_first_channel() -> u32 {
    14
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
    -25.0
}

#[derive(Clone, Deserialize, Serialize)]
pub struct NtripConfig {
    pub caster_url: String,
    #[serde(default = "default_ntrip_port")]
    pub port: u16,
    pub mountpoint: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default = "default_gga_interval")]
    pub gga_interval_secs: u32,
    #[serde(default)]
    pub serial_port: Option<String>,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
}

/// Hand-written for the same reason as `ApiConfig`: the NTRIP username and
/// password are Basic-auth credentials for a third-party caster, and must not
/// reach a log file a user is asked to email in.
impl std::fmt::Debug for NtripConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NtripConfig")
            .field("caster_url", &self.caster_url)
            .field("port", &self.port)
            .field("mountpoint", &self.mountpoint)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("gga_interval_secs", &self.gga_interval_secs)
            .field("serial_port", &self.serial_port)
            .field("baud_rate", &self.baud_rate)
            .finish()
    }
}

fn default_ntrip_port() -> u16 {
    2101
}
fn default_gga_interval() -> u32 {
    10
}
fn default_baud_rate() -> u32 {
    115200
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GpsConfig {
    #[serde(default = "default_gps_enabled")]
    pub enabled: bool,
    /// Pin a specific serial port. Highest priority — skips auto-detection
    /// entirely and never silently falls back to another port.
    #[serde(default)]
    pub port: Option<String>,
    #[serde(default, alias = "baud")]
    pub baud_rate: Option<u32>,
    #[serde(default = "default_sample_interval")]
    pub sample_interval_secs: u64,
    /// Probe serial ports that expose no usable USB vendor id. Detection used
    /// to be a strict vid whitelist, so a receiver the OS reports as a plain
    /// COM port (no vid) was invisible no matter what it emitted. Valid NMEA is
    /// the real proof; the vid is only a fast path. `false` restores the old
    /// whitelist-only behavior.
    #[serde(default = "default_probe_unknown_ports")]
    pub probe_unknown_ports: bool,
    /// If non-empty, only these port names are considered.
    #[serde(default)]
    pub allow_ports: Vec<String>,
    /// Never probe these port names, even a recognised GPS vendor id.
    #[serde(default)]
    pub deny_ports: Vec<String>,
    #[serde(default = "default_probe_retry_base")]
    pub probe_retry_base_secs: u64,
    #[serde(default = "default_probe_retry_max")]
    pub probe_retry_max_secs: u64,
}

/// Hand-written, NOT derived. `#[derive(Default)]` ignores `#[serde(default =
/// "...")]` and would yield `enabled: false` — and since no shipped config has
/// a `[gps]` section, a single `config.gps.unwrap_or_default()` would then
/// silently disable GPS on every node in the fleet.
impl Default for GpsConfig {
    fn default() -> Self {
        Self {
            enabled: default_gps_enabled(),
            port: None,
            baud_rate: None,
            sample_interval_secs: default_sample_interval(),
            probe_unknown_ports: default_probe_unknown_ports(),
            allow_ports: Vec::new(),
            deny_ports: Vec::new(),
            probe_retry_base_secs: default_probe_retry_base(),
            probe_retry_max_secs: default_probe_retry_max(),
        }
    }
}

fn default_gps_enabled() -> bool {
    true
}

fn default_sample_interval() -> u64 {
    10
}

fn default_probe_unknown_ports() -> bool {
    true
}

fn default_probe_retry_base() -> u64 {
    60
}

fn default_probe_retry_max() -> u64 {
    1800
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
            // FM floor is 65 MHz, not 87.5: Russia/Ukraine still broadcast on
            // the OIRT band (65.9-74 MHz), and Japan uses 76-95 MHz.
            for freq in &s.fm.frequencies {
                if *freq < 65_000_000 || *freq > 108_000_000 {
                    errors.push(format!(
                        "scanning.fm.frequencies: {} Hz is outside FM band (65-108 MHz)",
                        freq
                    ));
                }
            }
            let lte_max_hz = self.sdr.max_tuner_mhz * 1_000_000;
            for freq in &s.lte.frequencies {
                if *freq > lte_max_hz {
                    errors.push(format!(
                        "scanning.lte.frequencies: {} Hz exceeds tuner max ({} MHz)",
                        freq, self.sdr.max_tuner_mhz
                    ));
                }
            }
            // 862 MHz is the DVB-T/T2 upper edge; ATSC tops out lower but a
            // region-agnostic bound has to admit the widest standard.
            for freq in &s.dtv.frequencies {
                if *freq < 470_000_000 || *freq > 862_000_000 {
                    errors.push(format!(
                        "scanning.dtv.frequencies: {} Hz is outside UHF TV range (470-862 MHz)",
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

    /// Resolve `region` (ISO country code or AUTO) into concrete sweep plans and
    /// populate the [scanning] sub-configs. See `apply_region_with` for the
    /// rules. Resolution failures keep the configured sweep plans — never fatal.
    pub fn apply_region(&mut self) -> RegionResolution {
        self.apply_region_with(crate::regions::resolve_for_location)
    }

    /// `apply_region` with the location resolver injected (tests use a fake
    /// table; production uses the bundled boundary dataset).
    ///
    /// - explicit code ("FR", "jp"): that region's full FM/LTE/DTV plan;
    /// - `AUTO` in any case: the region the node's location falls in, applied
    ///   exactly like an explicit one; (0,0), open water or an unknown place
    ///   gets the worldwide `_DEFAULT` plan and a WARN;
    /// - unset: existing nodes keep their configured plan, except that FM and
    ///   DTV follow the location while they still hold the built-in defaults,
    ///   and an empty LTE band list is filled.
    pub fn apply_region_with(
        &mut self,
        locate: impl Fn(f64, f64) -> Option<&'static str>,
    ) -> RegionResolution {
        let configured = self.region.clone().unwrap_or_default();
        let configured = configured.trim();
        let is_auto = configured.eq_ignore_ascii_case("AUTO");
        let explicit = !configured.is_empty() && !is_auto;
        let unset = configured.is_empty();
        let (lat, lon) = (self.node.location.latitude, self.node.location.longitude);
        let max_tuner_mhz = self.sdr.max_tuner_mhz;

        let unresolved = RegionResolution { effective: "_DEFAULT".to_string(), source: "default" };
        let Some(scanning) = self.scanning.as_mut() else {
            return unresolved;
        };

        // Which region code to resolve, and how it was chosen.
        let located = if explicit { None } else { locate(lat, lon) };
        let (code, mut source): (String, &'static str) = if explicit {
            (configured.to_uppercase(), "explicit")
        } else if let Some(code) = located {
            (code.to_string(), "auto:location")
        } else {
            if is_auto {
                tracing::warn!(
                    latitude_known = !(lat == 0.0 && lon == 0.0),
                    "region AUTO: the node location is not inside a known region — using the \
                     worldwide default plan; set `region` or the node location"
                );
            }
            ("_DEFAULT".to_string(), "default")
        };

        let resolver = match RegionResolver::new(max_tuner_mhz) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e,
                    "embedded region database failed to load — keeping configured sweep plans");
                return unresolved;
            }
        };
        let resolved = match resolver.resolve(&code) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, region = %code,
                    "region resolution failed — keeping configured sweep plans");
                return unresolved;
            }
        };
        if resolved.region_code != code {
            // The database has no entry for this code and fell back.
            source = "default";
        }

        let full_plan = explicit || is_auto;
        if full_plan || scanning.lte.sweep_bands.is_empty() {
            scanning.lte.sweep_bands = resolved
                .lte_sweep_bands
                .iter()
                .map(|b| BandRange {
                    start: b.start,
                    end: b.end,
                    step: b.step,
                })
                .collect();
        }
        let fm_is_default = (scanning.fm.sweep_start, scanning.fm.sweep_end, scanning.fm.sweep_step)
            == (default_fm_sweep_start(), default_fm_sweep_end(), default_fm_sweep_step());
        let dtv_is_default = (scanning.dtv.sweep_start, scanning.dtv.sweep_end, scanning.dtv.sweep_step)
            == (default_dtv_sweep_start(), default_dtv_sweep_end(), default_dtv_sweep_step());
        if full_plan || (unset && fm_is_default) {
            scanning.fm.sweep_start = resolved.fm_sweep_start;
            scanning.fm.sweep_end = resolved.fm_sweep_end;
            scanning.fm.sweep_step = resolved.fm_sweep_step;
        }
        if full_plan || (unset && dtv_is_default) {
            scanning.dtv.sweep_start = resolved.dtv_sweep_start;
            scanning.dtv.sweep_end = resolved.dtv_sweep_end;
            scanning.dtv.sweep_step = resolved.dtv_sweep_step;
            scanning.dtv.standard = resolved.dtv_standard.clone();
            scanning.dtv.channel_mhz = resolved.dtv_channel_mhz;
            scanning.dtv.first_channel = resolved.dtv_first_channel;
        }
        tracing::info!(
            region = %resolved.region_code,
            source,
            lte_bands = scanning.lte.sweep_bands.len(),
            skipped_bands = resolved.skipped_bands.len(),
            "regional frequency plan applied"
        );
        RegionResolution { effective: resolved.region_code.to_string(), source }
    }

    #[allow(dead_code)]
    pub fn has_placeholder_values(&self) -> bool {
        self.api.key.is_empty()
            || self.api.key == "YOUR_API_KEY_HERE"
            || self.node.id.is_empty()
            || self.node.id == "YOUR_NODE_ID_HERE"
    }

    /// `validate()` as structured problems. The CLI-only "run 'azimuth-daemon
    /// --setup'" advice is stripped so a GUI can show the text as is.
    pub fn problems(&self) -> Vec<ConfigProblem> {
        match self.validate() {
            Ok(()) => Vec::new(),
            Err(errors) => errors
                .into_iter()
                .map(|e| {
                    let e = e.replace(" — run 'azimuth-daemon --setup' to configure", "");
                    match e.split_once(": ") {
                        Some((field, message)) => ConfigProblem {
                            field: field.to_string(),
                            message: message.to_string(),
                        },
                        None => ConfigProblem { field: "config".to_string(), message: e },
                    }
                })
                .collect(),
        }
    }

    /// Like `load`, but keeps "file not found" distinguishable from a real
    /// read or parse failure (os error 2 and 3 both map to NotFound).
    pub fn load_checked(path: &Path) -> Result<Self, LoadError> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(LoadError::Missing(format!(
                    "Failed to read config at {}: {}",
                    path.display(),
                    e
                )))
            }
            Err(e) => {
                return Err(LoadError::Failed(format!(
                    "Failed to read config at {}: {}",
                    path.display(),
                    e
                )))
            }
        };
        toml::from_str(&content).map_err(|e| {
            LoadError::Failed(format!("Failed to parse config at {}: {}", path.display(), e))
        })
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config at {}: {}", path.display(), e))?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        // Fresh installs may not have the config directory yet (e.g. AppData\Roaming\<app>
        // on Windows) — create it or the write fails with os error 3.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!("Failed to create config directory {}: {}", parent.display(), e)
            })?;
        }
        let content = toml::to_string_pretty(self)?;
        // Write beside the target and rename, so a crash or a concurrent reader
        // never sees a half-written config.toml.
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_creates_missing_parent_dir() {
        let base = std::env::temp_dir().join(format!("azd-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join("nested").join("deeper").join("config.toml");
        assert!(!path.parent().unwrap().exists());

        let cfg = Config::default();
        cfg.save(&path).expect("save must create missing parent dirs");

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("[api]"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_scanning_section_enables_scanning_by_default() {
        // Regression test for legacy-fallback bug: config.toml WITHOUT [scanning]
        // should enable scanning (Some(ScanningConfig::default())), NOT None.
        // This ensures fielded nodes on upgrade get regional frequency scanning automatically.
        let toml_str = r#"
[api]
url = "https://azimuth.day"
key = "test-key"

[node]
id = "test-node"
hardware_type = "tier1_rtlsdr"
label = "Test Node"

[node.location]
latitude = 40.7128
longitude = -74.0060
altitude_m = 0.0

[daemon]
heartbeat_interval_secs = 15
submit_interval_secs = 30
status_bind = "127.0.0.1:8420"

[sdr]
sample_rate = 2048000
center_frequency = 100000000
gain = -1
"#;

        let config: Config = toml::from_str(toml_str)
            .expect("config should parse even without [scanning] section");

        // This is the RED test: currently fails because scanning is None
        // After fix, scanning should be Some(...) with default values
        assert!(
            config.scanning.is_some(),
            "scanning should be enabled (Some) by default when [scanning] section is missing, \
             got None (legacy single-frequency mode)"
        );

        // Verify the defaults are sensible
        let scanning = config.scanning.as_ref().unwrap();
        assert_eq!(scanning.mode, "both");
        assert_eq!(scanning.interval_seconds, 30);
        assert_eq!(scanning.weight_lte, 3);
        assert_eq!(scanning.weight_dtv, 2);
        assert_eq!(scanning.weight_fm, 1);
        assert!(scanning.enabled, "scanner must default to enabled");
    }

    fn valid_base_config() -> Config {
        let mut config = Config::default();
        config.api.key = "test-key".to_string();
        config.node.id = "test-node".to_string();
        config.node.location.latitude = 40.7128;
        config.node.location.longitude = -74.0060;
        config
    }

    #[test]
    fn validate_accepts_oirt_and_japan_fm_frequencies() {
        let mut config = valid_base_config();
        let scanning = config.scanning.as_mut().unwrap();
        // OIRT (Russia/Ukraine) 65.9 MHz and Japan 76.0 MHz are real FM carriers.
        scanning.fm.frequencies = vec![65_900_000, 76_000_000, 107_900_000];
        assert!(config.validate().is_ok(), "OIRT/Japan FM frequencies must pass");
    }

    #[test]
    fn validate_rejects_fm_below_65_mhz() {
        let mut config = valid_base_config();
        config.scanning.as_mut().unwrap().fm.frequencies = vec![64_000_000];
        let errors = config.validate().expect_err("64 MHz is below any FM band");
        assert!(errors.iter().any(|e| e.contains("outside FM band")));
    }

    #[test]
    fn validate_accepts_dvbt_uhf_up_to_862_mhz() {
        let mut config = valid_base_config();
        config.scanning.as_mut().unwrap().dtv.frequencies = vec![858_000_000];
        assert!(config.validate().is_ok(), "858 MHz DVB-T channel must pass");
    }

    #[test]
    fn validate_lte_bound_follows_max_tuner_mhz() {
        let mut config = valid_base_config();
        config.sdr.max_tuner_mhz = 1000;
        config.scanning.as_mut().unwrap().lte.frequencies = vec![1_100_000_000];
        let errors = config.validate().expect_err("1.1 GHz exceeds a 1000 MHz tuner");
        assert!(errors.iter().any(|e| e.contains("exceeds tuner max (1000 MHz)")));
    }

    #[test]
    fn apply_region_fills_empty_sweep_bands_without_region() {
        let mut config = valid_base_config();
        assert!(config.region.is_none());
        assert!(config.scanning.as_ref().unwrap().lte.sweep_bands.is_empty());

        config.apply_region();

        let scanning = config.scanning.as_ref().unwrap();
        assert!(
            !scanning.lte.sweep_bands.is_empty(),
            "worldwide default bands must be populated when none are configured"
        );
        // Without an explicit region the FM sweep range stays untouched.
        assert_eq!(scanning.fm.sweep_start, 87_500_000);
    }

    #[test]
    fn apply_region_explicit_region_overrides_sweep_ranges() {
        let mut config = valid_base_config();
        config.region = Some("JP".to_string());

        config.apply_region();

        let scanning = config.scanning.as_ref().unwrap();
        assert_eq!(scanning.fm.sweep_start, 76_000_000, "Japan FM band starts at 76 MHz");
        assert_eq!(scanning.fm.sweep_end, 95_000_000, "Japan FM band ends at 95 MHz");
        assert!(!scanning.lte.sweep_bands.is_empty());
        // Capability filter: nothing above the default 1766 MHz tuner ceiling.
        for band in &scanning.lte.sweep_bands {
            assert!(band.end <= 1_766_000_000);
        }
    }

    #[test]
    fn apply_region_preserves_user_sweep_bands_without_region() {
        let mut config = valid_base_config();
        let user_band = BandRange {
            start: 700_000_000,
            end: 750_000_000,
            step: 5_000_000,
        };
        config.scanning.as_mut().unwrap().lte.sweep_bands = vec![user_band];

        config.apply_region();

        let bands = &config.scanning.as_ref().unwrap().lte.sweep_bands;
        assert_eq!(bands.len(), 1, "hand-configured sweep bands must survive");
        assert_eq!(bands[0].start, 700_000_000);
    }

    // --- secret safety ----------------------------------------------------
    //
    // Debug logging is now a user-facing toggle that writes to a file users are
    // asked to send in for support. A single `debug!(?config)` must not turn
    // that file into a credential leak, so the Debug impls are hand-written and
    // these tests pin them.

    #[test]
    fn debug_never_prints_the_api_key() {
        let cfg = ApiConfig {
            url: "https://azimuth.day".to_string(),
            key: "az_live_supersecretvalue".to_string(),
        };
        let s = format!("{cfg:?}");
        assert!(!s.contains("az_live_supersecretvalue"), "API key leaked: {s}");
        assert!(s.contains("<redacted>"));
        assert!(s.contains("https://azimuth.day"), "non-secret fields should survive");
    }

    #[test]
    fn debug_never_prints_ntrip_credentials() {
        let cfg = NtripConfig {
            caster_url: "rtk.example.net".to_string(),
            port: 2101,
            mountpoint: "NEAR".to_string(),
            username: Some("ntripuser".to_string()),
            password: Some("ntrippassword".to_string()),
            gga_interval_secs: 10,
            serial_port: None,
            baud_rate: 115200,
        };
        let s = format!("{cfg:?}");
        assert!(!s.contains("ntrippassword"), "NTRIP password leaked: {s}");
        assert!(!s.contains("ntripuser"), "NTRIP username leaked: {s}");
        assert!(s.contains("<redacted>"));
    }

    #[test]
    fn whole_config_debug_inherits_the_redaction() {
        // Config still derives Debug; it must inherit the field-level redaction
        // rather than bypass it.
        let mut cfg = Config::default();
        cfg.api.key = "az_live_anothersecret".to_string();
        let s = format!("{cfg:?}");
        assert!(!s.contains("az_live_anothersecret"), "key leaked via Config: {s}");
    }

    // --- G1: region = AUTO follows the node's location -----------------------

    /// Stand-in for the boundary dataset: the six acceptance cities plus the
    /// open ocean (None). The real lookup is DAEMON-DSP's `resolve_for_location`.
    fn fake_locate(lat: f64, lon: f64) -> Option<&'static str> {
        let near = |a: f64, b: f64| (lat - a).abs() < 1.0 && (lon - b).abs() < 1.0;
        if near(48.8566, 2.3522) {
            Some("FR")
        } else if near(51.5074, -0.1278) {
            Some("GB")
        } else if near(52.52, 13.405) {
            Some("DE")
        } else if near(39.8283, -98.5795) {
            Some("US")
        } else if near(43.6532, -79.3832) {
            Some("CA")
        } else if near(35.6762, 139.6503) {
            Some("JP")
        } else {
            None
        }
    }

    fn at(lat: f64, lon: f64, region: Option<&str>) -> Config {
        let mut c = valid_base_config();
        c.node.location.latitude = lat;
        c.node.location.longitude = lon;
        c.region = region.map(str::to_string);
        c
    }

    #[test]
    fn auto_applies_the_full_plan_of_the_location_region() {
        // Tokyo: FM 76-95 MHz, exactly like an explicit JP.
        let mut auto = at(35.6762, 139.6503, Some("AUTO"));
        let res = auto.apply_region_with(fake_locate);
        assert_eq!(res, RegionResolution { effective: "JP".to_string(), source: "auto:location" });
        let s = auto.scanning.as_ref().unwrap();
        assert_eq!((s.fm.sweep_start, s.fm.sweep_end), (76_000_000, 95_000_000));

        let mut explicit = at(35.6762, 139.6503, Some("JP"));
        let res = explicit.apply_region_with(fake_locate);
        assert_eq!(res, RegionResolution { effective: "JP".to_string(), source: "explicit" });
        let (a, e) = (auto.scanning.as_ref().unwrap(), explicit.scanning.as_ref().unwrap());
        assert_eq!(
            (a.fm.sweep_start, a.fm.sweep_end, a.dtv.sweep_start, a.dtv.sweep_end, a.dtv.sweep_step),
            (e.fm.sweep_start, e.fm.sweep_end, e.dtv.sweep_start, e.dtv.sweep_end, e.dtv.sweep_step),
            "AUTO must equal the explicit region's plan"
        );
        assert_eq!(a.lte.sweep_bands.len(), e.lte.sweep_bands.len());
    }

    #[test]
    fn auto_resolves_the_acceptance_cities() {
        for (lat, lon, code) in [
            (48.8566, 2.3522, "FR"),
            (51.5074, -0.1278, "GB"),
            (52.5200, 13.4050, "DE"),
            (39.8283, -98.5795, "US"),
            (43.6532, -79.3832, "CA"),
            (35.6762, 139.6503, "JP"),
        ] {
            let mut c = at(lat, lon, Some("AUTO"));
            let r = c.apply_region_with(fake_locate);
            assert_eq!((r.effective.as_str(), r.source), (code, "auto:location"), "({lat},{lon})");
        }
    }

    #[test]
    fn auto_in_the_open_ocean_falls_back_to_default() {
        let mut c = at(30.0, -40.0, Some("AUTO"));
        let r = c.apply_region_with(fake_locate);
        assert_eq!(r, RegionResolution { effective: "_DEFAULT".to_string(), source: "default" });
    }

    #[test]
    fn auto_at_0_0_is_default() {
        let mut c = at(0.0, 0.0, Some("AUTO"));
        assert_eq!(c.apply_region_with(fake_locate).source, "default");
    }

    #[test]
    fn auto_is_case_insensitive() {
        for spelling in ["AUTO", "auto", "Auto", " aUtO "] {
            let mut c = at(48.8566, 2.3522, Some(spelling));
            let r = c.apply_region_with(fake_locate);
            assert_eq!((r.effective.as_str(), r.source), ("FR", "auto:location"), "{spelling:?}");
        }
    }

    #[test]
    fn an_explicit_region_overrides_the_location() {
        let mut c = at(48.8566, 2.3522, Some("jp"));
        let r = c.apply_region_with(fake_locate);
        assert_eq!((r.effective.as_str(), r.source), ("JP", "explicit"));
        assert_eq!(c.scanning.as_ref().unwrap().fm.sweep_start, 76_000_000);
    }

    #[test]
    fn an_unknown_explicit_code_reports_the_default_plan() {
        let mut c = at(48.8566, 2.3522, Some("ZZ"));
        let r = c.apply_region_with(fake_locate);
        assert_eq!(r, RegionResolution { effective: "_DEFAULT".to_string(), source: "default" });
    }

    #[test]
    fn unset_region_follows_the_location_only_while_ranges_are_still_defaults() {
        let mut c = at(35.6762, 139.6503, None);
        let r = c.apply_region_with(fake_locate);
        assert_eq!((r.effective.as_str(), r.source), ("JP", "auto:location"));
        assert_eq!(c.scanning.as_ref().unwrap().fm.sweep_start, 76_000_000);

        // A hand-customised FM range is never overwritten.
        let mut custom = at(35.6762, 139.6503, None);
        custom.scanning.as_mut().unwrap().fm.sweep_start = 90_000_000;
        custom.apply_region_with(fake_locate);
        assert_eq!(custom.scanning.as_ref().unwrap().fm.sweep_start, 90_000_000);
    }

    #[test]
    fn derived_dtv_plan_fields_are_never_written_to_disk() {
        let mut c = at(48.8566, 2.3522, Some("AUTO"));
        c.apply_region_with(fake_locate);
        let toml = toml::to_string_pretty(&c).unwrap();
        assert!(!toml.contains("first_channel") && !toml.contains("channel_mhz"), "{toml}");
        // ...and a config written by an older daemon still loads.
        let _: Config = toml::from_str(&toml).unwrap();
    }
}

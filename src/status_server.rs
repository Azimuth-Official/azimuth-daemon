use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::{Config, ConfigProblem, NtripConfig};
use crate::node_runtime::{self, ApplyMsg, ConfigState, Registrar, SdrParams};
use crate::rtk::{self, PersistedRtkConfig};
use crate::sdr::driver_state::{self, DriverInfo, DriverStateKind, SdrTestState};
use crate::sdr::SdrManager;
use crate::gps::AccuracyPoint;
use std::collections::VecDeque;


#[derive(Debug, Clone, Serialize)]
pub struct SignalBreakdown {
    pub lte: u64,
    pub fm: u64,
    pub dtv: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentSignal {
    pub id: String,
    #[serde(rename = "type")]
    pub signal_type: String,
    pub frequency_hz: u64,
    pub source_id: Option<String>,
    pub snr_db: Option<f64>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusInfo {
    pub version: String,
    pub node_id: String,
    pub hardware_type: String,
    pub uptime_secs: u64,
    pub observations_submitted: u64,
    pub last_heartbeat: Option<String>,
    pub sdr_connected: bool,
    pub sdr_name: String,
    pub sdr_multiplier: f64,
    pub device_count: usize,
    pub driver_status: String,
    pub scanning_enabled: bool,
    pub scanner_active: bool,
    pub network_support_enabled: bool,
    pub rtk_enabled: bool,
    pub rtk_connected: bool,
    pub rtk_provider: Option<String>,
    pub rtk_messages_received: u64,
    pub rtk_last_correction: Option<String>,
    pub signals_found: u64,
    pub signal_breakdown: SignalBreakdown,
    pub recent_signals: Vec<RecentSignal>,
    pub gps_detected: bool,
    pub gps_fix_quality: u8,
    pub gps_hdop: Option<f32>,
    /// Non-null when the daemon started with a config problem (missing file,
    /// validation failure, unconfigured API key). The desktop app surfaces
    /// this as a "setup needed" state instead of appearing dead.
    pub config_error: Option<String>,
    /// Non-null when the API rejected the key with HTTP 401. Cleared when a
    /// heartbeat succeeds or a new key arrives via POST /config/write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_error: Option<String>,
    // --- additive (C3) ---
    /// "awaiting_setup" | "ok" | "invalid". `config_error` is non-null only for "invalid".
    pub config_state: ConfigState,
    pub config_problems: Vec<ConfigProblem>,
    pub daemon_pid: u32,
    pub daemon_version: String,
    pub config_path: String,
    pub started_at: String,
    pub node_display_name: Option<String>,
    pub registration_state: String,
    pub region_effective: Option<String>,
    pub region_source: String,
    pub on_air: bool,
    pub on_air_reason: Option<String>,
    /// State of Interface 0 of the attached RTL2832U (refreshed on re-detect).
    pub driver: DriverInfo,
    pub sdr_test_last: Option<serde_json::Value>,
    pub sdr_diagnostics: SdrDiagnostics,
}

/// Counters that help support triage SDR problems.
#[derive(Debug, Clone, Serialize)]
pub struct SdrDiagnostics {
    /// "[R82xx] PLL not locked!" records seen from the tuner driver.
    pub pll_lock_failures: u64,
}

pub struct StatusState {
    pub node_id: String,
    pub hardware_type: String,
    pub start_time: Instant,
    pub observations_submitted: u64,
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
    pub sdr_connected: bool,
    pub sdr_name: String,
    pub device_count: usize,
    pub driver_status: String,
    pub network_support_enabled: bool,
    pub rtk_enabled: bool,
    pub rtk_connected: bool,
    pub rtk_provider: Option<String>,
    pub rtk_messages_received: u64,
    pub rtk_last_correction: Option<chrono::DateTime<chrono::Utc>>,
    pub rtk_config: Option<NtripConfig>,
    pub signals_found: u64,
    pub signal_breakdown: SignalBreakdown,
    pub recent_signals: Vec<RecentSignal>,
    pub gps_detected: bool,
    pub gps_port: Option<String>,
    pub gps_device_name: Option<String>,
    pub gps_fix_quality: u8,
    pub gps_satellites: u8,
    pub gps_hdop: Option<f32>,
    pub gps_latitude: Option<f64>,
    pub gps_longitude: Option<f64>,
    pub gps_altitude_m: Option<f64>,
    pub gps_speed_knots: Option<f32>,
    pub gps_lat_error_m: Option<f32>,
    pub gps_lon_error_m: Option<f32>,
    pub gps_last_fix: Option<String>,
    pub gps_accuracy_history: VecDeque<AccuracyPoint>,
    pub config_error: Option<String>,
    pub auth_error: Option<String>,
    pub config_state: ConfigState,
    pub config_problems: Vec<ConfigProblem>,
    pub config_path_abs: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub node_display_name: Option<String>,
    pub registration_state: String,
    pub region_effective: Option<String>,
    pub region_source: String,
    pub last_accepted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Location the node currently runs with (updated by POST /config/write).
    pub live_location: (f64, f64, f64),
    pub driver: DriverInfo,
    pub sdr_test_last: Option<serde_json::Value>,
    /// When the observation loop last completed a cycle with an open device.
    pub sdr_last_capture_ok_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A node is "on air" when it is registered, has an open SDR and the server
/// accepted an observation from it recently.
pub const ON_AIR_WINDOW_SECS: i64 = 600;

/// Minimum spacing between two signals of the same type for both to count as
/// distinct frequencies, mirroring the server's `sdr_min_spacing_*_hz` config.
fn min_spacing_hz(signal_type: &str) -> u64 {
    match signal_type {
        "fm" => 2_000_000,
        "lte" => 5_000_000,
        "dtv" => 6_000_000,
        _ => 0,
    }
}

/// Local estimate of the SDR multiplier for the dashboard.
///
/// The server owns the canonical value: it counts distinct frequencies observed
/// recently — collapsing any that sit closer together than the per-type minimum
/// spacing — and caps that by the node's device count. This reproduces the same
/// shape from the signals this daemon has in hand, so the status page shows
/// something truthful instead of a hardcoded 1.0. Expect small divergence from
/// the server, which scores a different observation window.
fn estimate_sdr_multiplier(recent_signals: &[RecentSignal], device_count: usize) -> f64 {
    if recent_signals.is_empty() || device_count == 0 {
        return 1.0;
    }

    let mut by_type: std::collections::HashMap<&str, Vec<u64>> = std::collections::HashMap::new();
    for signal in recent_signals {
        by_type
            .entry(crate::observation::category_for_signal_type(&signal.signal_type))
            .or_default()
            .push(signal.frequency_hz);
    }

    let mut valid_frequencies = 0usize;
    for (signal_type, mut frequencies) in by_type {
        frequencies.sort_unstable();
        let spacing = min_spacing_hz(signal_type);
        let mut last_counted: Option<u64> = None;
        for frequency in frequencies {
            let far_enough = last_counted.is_none_or(|last| frequency.saturating_sub(last) >= spacing);
            if far_enough {
                valid_frequencies += 1;
                last_counted = Some(frequency);
            }
        }
    }

    valid_frequencies.min(device_count).max(1) as f64
}

/// Message surfaced through /status when the API rejects the key.
pub const AUTH_ERROR_MSG: &str =
    "API key rejected by server (401). Generate a new key at azimuth.day/dashboard";

impl StatusState {
    pub fn new(node_id: String, hardware_type: String) -> Self {
        Self {
            node_id,
            hardware_type,
            start_time: Instant::now(),
            observations_submitted: 0,
            last_heartbeat: None,
            sdr_connected: false,
            sdr_name: "No device".to_string(),
            device_count: 0,
            driver_status: "not_applicable".to_string(),
            network_support_enabled: false,
            rtk_enabled: false,
            rtk_connected: false,
            rtk_provider: None,
            rtk_messages_received: 0,
            rtk_last_correction: None,
            rtk_config: None,
            signals_found: 0,
            signal_breakdown: SignalBreakdown { lte: 0, fm: 0, dtv: 0 },
            recent_signals: Vec::new(),
            gps_detected: false,
            gps_port: None,
            gps_device_name: None,
            gps_fix_quality: 0,
            gps_satellites: 0,
            gps_hdop: None,
            gps_latitude: None,
            gps_longitude: None,
            gps_altitude_m: None,
            gps_speed_knots: None,
            gps_lat_error_m: None,
            gps_lon_error_m: None,
            gps_last_fix: None,
            gps_accuracy_history: VecDeque::new(),
            config_error: None,
            auth_error: None,
            config_state: ConfigState::AwaitingSetup,
            config_problems: Vec::new(),
            config_path_abs: String::new(),
            started_at: chrono::Utc::now(),
            node_display_name: None,
            registration_state: "unregistered".to_string(),
            region_effective: None,
            region_source: "default".to_string(),
            last_accepted_at: None,
            live_location: (0.0, 0.0, 0.0),
            driver: DriverInfo::not_applicable(),
            sdr_test_last: None,
            sdr_last_capture_ok_at: None,
        }
    }

    /// Record the driver state and keep the legacy `driver_status` in step.
    pub fn set_driver(&mut self, info: DriverInfo) {
        self.driver_status = info.legacy_status();
        self.driver = info;
    }

    /// (on_air, reason). Reason codes are checked in priority order.
    fn on_air(&self) -> (bool, Option<String>) {
        let reason = if self.config_state != ConfigState::Ok {
            Some("config")
        } else if self.registration_state != "registered" {
            Some("not_registered")
        } else if !self.sdr_connected {
            Some("no_sdr")
        } else if !self
            .last_accepted_at
            .is_some_and(|t| (chrono::Utc::now() - t).num_seconds() < ON_AIR_WINDOW_SECS)
        {
            Some("no_accepted_observations")
        } else {
            None
        };
        (reason.is_none(), reason.map(str::to_string))
    }

    fn to_info(&self,
        scanning_enabled: bool,
        scanner_active: bool,
    ) -> StatusInfo {
        StatusInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            node_id: self.node_id.clone(),
            hardware_type: self.hardware_type.clone(),
            uptime_secs: self.start_time.elapsed().as_secs(),
            observations_submitted: self.observations_submitted,
            last_heartbeat: self.last_heartbeat.map(|t| t.to_rfc3339()),
            sdr_connected: self.sdr_connected,
            sdr_name: self.sdr_name.clone(),
            sdr_multiplier: estimate_sdr_multiplier(&self.recent_signals, self.device_count),
            device_count: self.device_count,
            driver_status: self.driver_status.clone(),
            scanning_enabled,
            scanner_active,
            network_support_enabled: self.network_support_enabled,
            rtk_enabled: self.rtk_enabled,
            rtk_connected: self.rtk_connected,
            rtk_provider: self.rtk_provider.clone(),
            rtk_messages_received: self.rtk_messages_received,
            rtk_last_correction: self.rtk_last_correction.map(|t| t.to_rfc3339()),
            signals_found: self.signals_found,
            signal_breakdown: self.signal_breakdown.clone(),
            recent_signals: self.recent_signals.clone(),
            gps_detected: self.gps_detected,
            gps_fix_quality: self.gps_fix_quality,
            gps_hdop: self.gps_hdop,
            config_error: self.config_error.clone(),
            auth_error: self.auth_error.clone(),
            config_state: self.config_state,
            config_problems: self.config_problems.clone(),
            daemon_pid: std::process::id(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            config_path: self.config_path_abs.clone(),
            started_at: self.started_at.to_rfc3339(),
            node_display_name: self.node_display_name.clone(),
            registration_state: self.registration_state.clone(),
            region_effective: self.region_effective.clone(),
            region_source: self.region_source.clone(),
            on_air: self.on_air().0,
            on_air_reason: self.on_air().1,
            driver: self.driver.clone(),
            sdr_test_last: self.sdr_test_last.clone(),
            sdr_diagnostics: SdrDiagnostics {
                pll_lock_failures: crate::sdr::pll_lock_failures(),
            },
        }
    }
}

/// Shared application state used by the HTTP status/control server.
pub struct AppState {
    pub status: Arc<RwLock<StatusState>>,
    pub scanning_enabled: Arc<AtomicBool>,
    pub scanner_active: Arc<AtomicBool>,
    pub config_path: PathBuf,
    /// Live API key shared with ApiClient — POST /config/write swaps it so a
    /// corrected key takes effect without a daemon restart.
    pub api_key: Arc<std::sync::RwLock<String>>,
    /// Runtime log-level control, so the desktop Debug mode toggle takes effect
    /// without restarting the sidecar.
    pub log: Arc<crate::logging::LogHandle>,
    /// Serialises concurrent POST /config/write applications.
    pub apply_lock: tokio::sync::Mutex<()>,
    /// Hands an applied config to the observation loop (scan plan, SDR params).
    pub apply_tx: tokio::sync::mpsc::UnboundedSender<ApplyMsg>,
    /// Hands GPS settings to the GPS task.
    pub gps_cfg_tx: tokio::sync::watch::Sender<crate::config::GpsConfig>,
    /// Deferred node registration, kicked by a config write.
    pub registrar: Arc<Registrar>,
    /// The SDR devices, shared with the observation loop so `/sdr/test` can
    /// test through the handle the scanner already holds.
    pub sdr: Arc<std::sync::Mutex<SdrManager>>,
    /// Notified when an authorised local caller asks the daemon to stop.
    pub stop_signal: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Clone, Serialize)]
struct SdrListEntry {
    index: usize,
    vendor_id: String,
    product_id: String,
    product_name: String,
    serial: Option<String>,
    bus: u8,
    address: u8,
    /// Interface 0 driver state (same value as `/status.driver.state`).
    driver_state: DriverStateKind,
    bound_driver: Option<String>,
    /// Tuner chip, known only for a device the daemon holds open.
    tuner: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SdrTestRequest {
    index: u32,
}

/// `/sdr/test` response (C5). `opened` is the legacy boolean older desktop
/// builds still read.
#[derive(Debug, Clone, Serialize)]
struct SdrTestResponse {
    index: u32,
    opened: bool,
    state: SdrTestState,
    detail: String,
    tuner: Option<String>,
    holders: Vec<String>,
    vid: Option<String>,
    pid: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct MessageResponse {
    message: String,
}

async fn status_handler(State(state): State<Arc<AppState>>) -> Json<StatusInfo> {
    let scanning_enabled = state.scanning_enabled.load(Ordering::Relaxed);
    let scanner_active = state.scanner_active.load(Ordering::Relaxed);
    let s = state.status.read().await;
    Json(s.to_info(scanning_enabled, scanner_active))
}

async fn sdr_list_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let sdr = Arc::clone(&state.sdr);
    let entries = tokio::task::spawn_blocking(move || enumerate_sdr_devices(&sdr))
        .await
        .map_err(|e| anyhow::anyhow!("enumeration task failed: {e}"))
        .and_then(|r| r);
    match entries {
        Ok(list) => Ok(Json(list)),
        Err(e) => {
            warn!(error = %e, "Failed to enumerate SDR devices");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(MessageResponse {
                    message: format!("Enumeration failed: {}", e),
                }),
            ))
        }
    }
}

/// How long `/sdr/test` waits for the scanner to release the device lock.
const SDR_TEST_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

fn lock_with_timeout(
    m: &std::sync::Mutex<SdrManager>,
    wait: std::time::Duration,
) -> Option<std::sync::MutexGuard<'_, SdrManager>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// What the scanner's own bookkeeping says, used only when the device lock
/// cannot be obtained because a sweep is running.
struct ScannerFacts {
    scanner_active: bool,
    sdr_connected: bool,
}

/// A REAL test of device `index`.
///   - scanner idle or device lock free: test through the handle the scanner
///     already holds (tuner name + a live read), or open the device fresh when
///     none is held;
///   - lock busy for the whole wait: a sweep is in progress, so this daemon itself
///     holds the device and is streaming from it (`ok`, with that detail);
///     if no sweep is active the timeout is reported as a USB error.
fn run_sdr_test(
    sdr: &std::sync::Mutex<SdrManager>,
    index: u32,
    facts: ScannerFacts,
    lock_wait: std::time::Duration,
) -> (driver_state::SdrTestVerdict, DriverInfo) {
    let driver = crate::sdr::current_driver_info();
    let access_means_in_use = cfg!(windows);
    let Some(mut guard) = lock_with_timeout(sdr, lock_wait) else {
        let streaming = facts.scanner_active && facts.sdr_connected;
        let verdict = if streaming {
            driver_state::SdrTestVerdict {
                state: SdrTestState::Ok,
                detail: "The scanner is using the RTL-SDR right now (a sweep is in progress).".to_string(),
                tuner: None,
            }
        } else {
            driver_state::SdrTestVerdict {
                state: SdrTestState::UsbError,
                detail: "usb_error: the device did not become available within 10 s".to_string(),
                tuner: None,
            }
        };
        return (verdict, driver);
    };

    // Through the scanner's own handle.
    if let Some(dev) = guard.device_mut(index as usize) {
        let tuner = dev.tuner_id().unwrap_or_else(|| "unknown".to_string());
        let verdict = if dev.live_check() {
            driver_state::SdrTestVerdict {
                state: SdrTestState::Ok,
                detail: format!("The RTL-SDR is open and streaming (tuner {tuner})."),
                tuner: Some(tuner),
            }
        } else {
            driver_state::SdrTestVerdict {
                state: SdrTestState::UsbError,
                detail: "usb_error: the open device returned no samples".to_string(),
                tuner: Some(tuner),
            }
        };
        return (verdict, driver);
    }

    // Nothing held: a fresh open attempt (the lock stays held so the
    // observation loop cannot race us for the device).
    let open = if matches!(driver.state, DriverStateKind::NotFound) {
        Err(driver_state::OpenFailure::NotFound)
    } else {
        crate::sdr::probe_open(index)
    };
    let verdict = driver_state::classify_test(&driver, open, access_means_in_use);
    (verdict, driver)
}

async fn sdr_test_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SdrTestRequest>,
) -> impl IntoResponse {
    let facts = {
        let s = state.status.read().await;
        ScannerFacts {
            scanner_active: state.scanner_active.load(Ordering::Relaxed),
            sdr_connected: s.sdr_connected,
        }
    };
    let sdr = Arc::clone(&state.sdr);
    let index = req.index;
    let joined = tokio::task::spawn_blocking(move || run_sdr_test(&sdr, index, facts, SDR_TEST_LOCK_WAIT)).await;
    let (verdict, driver) = match joined {
        Ok(r) => r,
        Err(e) => {
            warn!(index, error = %e, "SDR test task failed");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(MessageResponse { message: format!("SDR test failed: {e}") }),
            ));
        }
    };
    let holders = if verdict.state == SdrTestState::InUse {
        tokio::task::spawn_blocking(crate::sdr::holders::likely_holders).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let response = SdrTestResponse {
        index,
        opened: verdict.state == SdrTestState::Ok,
        state: verdict.state,
        detail: verdict.detail,
        tuner: verdict.tuner,
        holders,
        vid: driver.vid.clone(),
        pid: driver.pid.clone(),
    };
    {
        let mut s = state.status.write().await;
        s.set_driver(driver);
        s.sdr_test_last = serde_json::to_value(&response).ok();
    }
    Ok((StatusCode::OK, Json(response)))
}

#[derive(Debug, Serialize)]
struct ConfigWriteResponse {
    config_state: ConfigState,
    config_problems: Vec<ConfigProblem>,
    registration_state: String,
    node_id: String,
    restart_required: bool,
}

fn config_rejection(
    state: ConfigState,
    problems: Vec<ConfigProblem>,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({
            "config_state": state,
            "config_problems": problems,
            "error": "configuration not saved",
        })),
    )
}

/// POST /config/write: validate, persist atomically, then apply to the running
/// daemon (API key, location, region/scan plan, GPS) and start registration.
async fn config_write_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let config_path = state.config_path.clone();
    let apply = body.get("apply").and_then(|v| v.as_bool()).unwrap_or(true);
    let mut config: Config = match serde_json::from_value(body) {
        Ok(c) => c,
        Err(e) => {
            return config_rejection(
                ConfigState::Invalid,
                vec![ConfigProblem { field: "body".to_string(), message: e.to_string() }],
            )
        }
    };

    // Identity preservation chokepoint: the setup wizard (and any other
    // writer) sends node.id = "" — re-running setup must NOT discard an
    // already-registered node identity, or every reinstall creates a
    // duplicate node. Fill an empty incoming id from the on-disk config,
    // then from the machine identity file. Explicit non-empty ids are
    // honored unchanged.
    if config.node.id.is_empty() {
        let existing_id = Config::load(&config_path)
            .ok()
            .map(|c| c.node.id)
            .filter(|id| !node_runtime::is_placeholder(id));
        let adopted = existing_id.or_else(|| {
            crate::identity::load(&config_path).map(|i| i.node_id)
        });
        if let Some(id) = adopted {
            info!(node_id = %id, "Preserving existing node identity through config write");
            config.node.id = id;
        }
    }

    // Validate BEFORE touching disk: an unusable config is never saved.
    let verdict = node_runtime::evaluate_config(&config);
    if verdict.state != ConfigState::Ok {
        return config_rejection(verdict.state, config.problems());
    }
    let region = config.apply_region();

    let path_for_save = config_path.clone();
    let node_for_kick = config.node.clone();
    let scanning = config.scanning.clone();
    let sdr_params = SdrParams::from_config(&config);
    let gps_cfg = config.gps.clone().unwrap_or_default();
    let new_api_key = config.api.key.clone();
    let hint_id = config.node.id.clone();
    let location = (
        config.node.location.latitude,
        config.node.location.longitude,
        config.node.location.altitude_m,
    );
    let saved = tokio::task::block_in_place(move || config.save(&path_for_save));
    if let Err(e) = saved {
        warn!(path = %config_path.display(), error = %e, "Failed to write config");
        return config_rejection(
            ConfigState::Invalid,
            vec![ConfigProblem {
                field: "config_file".to_string(),
                message: format!("Could not write {}: {}", config_path.display(), e),
            }],
        );
    }
    info!(path = %config_path.display(), "Config written from HTTP request");

    if !apply {
        let s = state.status.read().await;
        return (
            StatusCode::OK,
            Json(serde_json::json!(ConfigWriteResponse {
                config_state: s.config_state,
                config_problems: s.config_problems.clone(),
                registration_state: s.registration_state.clone(),
                node_id: s.node_id.clone(),
                restart_required: true,
            })),
        );
    }

    let _guard = state.apply_lock.lock().await;
    if let Ok(mut key) = state.api_key.write() {
        *key = new_api_key;
    }
    let (registration_state, node_id, needs_registration) = {
        let mut s = state.status.write().await;
        s.config_state = ConfigState::Ok;
        s.config_error = None;
        s.config_problems = Vec::new();
        s.auth_error = None;
        s.region_effective = Some(region.effective.clone());
        s.region_source = region.source.to_string();
        s.live_location = location;
        // The saved id is only a reclaim HINT: the registrar offers it to the
        // server, which honours it only for this account's nodes.
        let needs = node_runtime::is_placeholder(&s.node_id);
        if needs {
            s.registration_state = "registering".to_string();
        }
        (
            s.registration_state.clone(),
            s.node_id.clone(),
            node_runtime::is_placeholder(&s.node_id),
        )
    };
    info!(region = %region.effective, source = region.source, "Configuration applied");
    let _ = state.apply_tx.send(ApplyMsg { scanning, sdr: sdr_params });
    state.gps_cfg_tx.send_replace(gps_cfg);
    if needs_registration {
        // Make the reclaim hint available to the registrar even if the
        // identity file was never written (id came from config.toml).
        if !node_runtime::is_placeholder(&hint_id) && crate::identity::load(&config_path).is_none() {
            crate::identity::save(&config_path, &hint_id);
        }
        state.registrar.kick(node_for_kick);
    }

    (
        StatusCode::OK,
        Json(serde_json::json!(ConfigWriteResponse {
            config_state: ConfigState::Ok,
            config_problems: Vec::new(),
            registration_state,
            node_id,
            restart_required: false,
        })),
    )
}

async fn scanner_start_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.scanning_enabled.store(true, Ordering::Relaxed);
    info!("Scanner enabled via HTTP request");
    Json(MessageResponse {
        message: "Scanner started".to_string(),
    })
}

#[derive(Debug, Clone, Deserialize)]
struct NetworkSupportRequest {
    enabled: bool,
}

async fn network_support_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<NetworkSupportRequest>,
) -> impl IntoResponse {
    let mut s = state.status.write().await;
    s.network_support_enabled = req.enabled;
    info!(enabled = req.enabled, "Network support toggled via HTTP");
    Json(MessageResponse {
        message: format!("Network support {}", if req.enabled { "enabled" } else { "disabled" }),
    })
}

#[derive(Debug, Clone, Deserialize)]
struct RtkConfigRequest {
    enabled: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    caster_url: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

async fn rtk_config_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RtkConfigRequest>,
) -> impl IntoResponse {
    let config_path = state.config_path.clone();

    // Snapshot values after mutating state, then drop the write guard before
    // the blocking disk write below (same pattern as config_write_handler).
    let (snapshot_enabled, snapshot_provider, snapshot_ntrip) = {
        let mut s = state.status.write().await;
        s.rtk_enabled = req.enabled;
        if let Some(provider) = req.provider.clone() {
            s.rtk_provider = Some(provider);
        }

        // Merge request fields over the existing NtripConfig (sourced from
        // config.toml, a restored persisted file, or a prior POST) instead of
        // rebuilding from scratch — a bare {enabled:false} or a partial update
        // must not wipe serial_port/baud_rate/gga_interval_secs. Request
        // fields win when present; defaults only apply when nothing existed
        // before.
        let existing = s.rtk_config.clone();
        let caster_url = req
            .caster_url
            .clone()
            .filter(|v| !v.is_empty())
            .or_else(|| existing.as_ref().map(|c| c.caster_url.clone()));
        let mountpoint = req
            .mountpoint
            .clone()
            .filter(|v| !v.is_empty())
            .or_else(|| existing.as_ref().map(|c| c.mountpoint.clone()));

        if let (Some(caster_url), Some(mountpoint)) = (caster_url, mountpoint) {
            s.rtk_config = Some(NtripConfig {
                caster_url,
                port: req.port.or_else(|| existing.as_ref().map(|c| c.port)).unwrap_or(2101),
                mountpoint,
                username: req
                    .username
                    .clone()
                    .or_else(|| existing.as_ref().and_then(|c| c.username.clone())),
                password: req
                    .password
                    .clone()
                    .or_else(|| existing.as_ref().and_then(|c| c.password.clone())),
                gga_interval_secs: existing.as_ref().map(|c| c.gga_interval_secs).unwrap_or(10),
                serial_port: existing.as_ref().and_then(|c| c.serial_port.clone()),
                baud_rate: existing.as_ref().map(|c| c.baud_rate).unwrap_or(115200),
            });
        }

        if !req.enabled {
            s.rtk_connected = false;
        }

        (s.rtk_enabled, s.rtk_provider.clone(), s.rtk_config.clone())
    };

    info!(enabled = snapshot_enabled, provider = ?snapshot_provider, "RTK config updated via HTTP");

    let mut message = format!("RTK {}", if snapshot_enabled { "enabled" } else { "disabled" });

    // Persist even on disable — a disable must be durable across a restart,
    // not just cleared in memory. Nothing to persist yet if no caster config
    // has ever been set (e.g. toggling before first configuring a caster).
    if let Some(ntrip) = snapshot_ntrip {
        let persisted = PersistedRtkConfig {
            version: 1,
            enabled: snapshot_enabled,
            provider: snapshot_provider,
            ntrip,
        };
        let persist_result =
            tokio::task::block_in_place(|| rtk::save(&config_path, &persisted));
        if let Err(e) = persist_result {
            warn!(error = %e, "Failed to persist RTK config");
            message.push_str(" (warning: failed to persist to disk — will reset on restart)");
        }
    }

    Json(MessageResponse { message })
}

#[derive(Debug, Clone, Serialize)]
struct RtkStatusResponse {
    enabled: bool,
    connected: bool,
    provider: Option<String>,
    messages_received: u64,
    last_correction: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RtkTestRequest {
    #[serde(default)]
    caster_url: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RtkTestResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn rtk_status_handler(State(state): State<Arc<AppState>>) -> Json<RtkStatusResponse> {
    let s = state.status.read().await;
    Json(RtkStatusResponse {
        enabled: s.rtk_enabled,
        connected: s.rtk_connected,
        provider: s.rtk_provider.clone(),
        messages_received: s.rtk_messages_received,
        last_correction: s.rtk_last_correction.map(|t| t.to_rfc3339()),
    })
}

async fn rtk_test_handler(
    State(_state): State<Arc<AppState>>,
    Json(req): Json<RtkTestRequest>,
) -> impl IntoResponse {
    let caster_url = match req.caster_url {
        Some(url) if !url.is_empty() => url,
        _ => return Ok::<_, StatusCode>((
            StatusCode::BAD_REQUEST,
            Json(RtkTestResponse {
                ok: false,
                status_line: None,
                error: Some("Missing caster_url".to_string()),
            }),
        )),
    };

    let mountpoint = match req.mountpoint {
        Some(mp) if !mp.is_empty() => mp,
        _ => return Ok((
            StatusCode::BAD_REQUEST,
            Json(RtkTestResponse {
                ok: false,
                status_line: None,
                error: Some("Missing mountpoint".to_string()),
            }),
        )),
    };

    let port = req.port.unwrap_or(2101);

    // Run test connection with 20s timeout
    let test_result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::ntrip::test_ntrip_connection(
            &caster_url,
            port,
            &mountpoint,
            req.username.clone(),
            req.password.clone(),
        ),
    )
    .await;

    match test_result {
        Ok(Ok((status_line, _))) => {
            info!(caster = %caster_url, port, mountpoint = %mountpoint, "RTK test connection succeeded");
            Ok((
                StatusCode::OK,
                Json(RtkTestResponse {
                    ok: true,
                    status_line: Some(status_line),
                    error: None,
                }),
            ))
        }
        Ok(Err(e)) => {
            warn!(caster = %caster_url, port, mountpoint = %mountpoint, error = %e, "RTK test connection failed");
            Ok((
                StatusCode::OK,
                Json(RtkTestResponse {
                    ok: false,
                    status_line: None,
                    error: Some(format!("{}", e)),
                }),
            ))
        }
        Err(_timeout) => {
            warn!(caster = %caster_url, port, mountpoint = %mountpoint, "RTK test connection timeout");
            Ok((
                StatusCode::OK,
                Json(RtkTestResponse {
                    ok: false,
                    status_line: None,
                    error: Some("Connection timeout (>20s)".to_string()),
                }),
            ))
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct GpsStatusResponse {
    detected: bool,
    port: Option<String>,
    device_name: Option<String>,
    fix_quality: u8,
    satellites: u8,
    hdop: Option<f32>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    altitude_m: Option<f64>,
    speed_knots: Option<f32>,
    lat_error_m: Option<f32>,
    lon_error_m: Option<f32>,
    last_fix: Option<String>,
    accuracy_history: Vec<AccuracyPoint>,
}

async fn gps_status_handler(State(state): State<Arc<AppState>>) -> Json<GpsStatusResponse> {
    let s = state.status.read().await;
    Json(GpsStatusResponse {
        detected: s.gps_detected,
        port: s.gps_port.clone(),
        device_name: s.gps_device_name.clone(),
        fix_quality: s.gps_fix_quality,
        satellites: s.gps_satellites,
        hdop: s.gps_hdop,
        latitude: s.gps_latitude,
        longitude: s.gps_longitude,
        altitude_m: s.gps_altitude_m,
        speed_knots: s.gps_speed_knots,
        lat_error_m: s.gps_lat_error_m,
        lon_error_m: s.gps_lon_error_m,
        last_fix: s.gps_last_fix.clone(),
        accuracy_history: s.gps_accuracy_history.iter().cloned().collect(),
    })
}

#[derive(Debug, Deserialize)]
struct LogLevelRequest {
    /// Friendly level: trace|debug|info|warn|error.
    level: Option<String>,
    /// Raw EnvFilter directive for support sessions. Validated — a directive
    /// that would raise a non-azimuth_daemon target is rejected, because
    /// dependency debug logging includes the Bearer API key.
    filter: Option<String>,
}

#[derive(Debug, Serialize)]
struct LogLevelResponse {
    filter: String,
    log_dir: String,
    files: Vec<LogFileEntry>,
}

#[derive(Debug, Serialize)]
struct LogFileEntry {
    name: String,
    bytes: u64,
}

/// Flip the runtime log level without restarting the sidecar. Never logs the
/// request body.
async fn log_level_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LogLevelRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let directive =
        match crate::logging::parse_level_directive(req.level.as_deref(), req.filter.as_deref()) {
            Ok(d) => d,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "message": e })),
                );
            }
        };

    match state.log.set_filter(&directive) {
        Ok(applied) => {
            tracing::info!(filter = %applied, "log level changed at runtime");
            let files = state
                .log
                .files()
                .into_iter()
                .map(|(name, bytes)| LogFileEntry { name, bytes })
                .collect();
            let body = LogLevelResponse {
                filter: applied,
                log_dir: state.log.dir().display().to_string(),
                files,
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(body).unwrap_or_default()),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "message": e })),
        ),
    }
}

/// Where the logs are and what is on disk, so "Export logs" does not guess
/// filenames.
async fn log_path_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let files: Vec<LogFileEntry> = state
        .log
        .files()
        .into_iter()
        .map(|(name, bytes)| LogFileEntry { name, bytes })
        .collect();
    Json(serde_json::json!({
        "log_dir": state.log.dir().display().to_string(),
        "files": serde_json::to_value(files).unwrap_or_default(),
    }))
}

async fn serial_ports_handler(State(_state): State<Arc<AppState>>) -> Json<Vec<crate::gps::SerialPortInfo>> {
    let ports = crate::gps::list_serial_ports();
    Json(ports)
}

async fn scanner_stop_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.scanning_enabled.store(false, Ordering::Relaxed);
    info!("Scanner disabled via HTTP request");
    Json(MessageResponse {
        message: "Scanner stopped".to_string(),
    })
}

fn enumerate_sdr_devices(sdr: &std::sync::Mutex<SdrManager>) -> anyhow::Result<Vec<SdrListEntry>> {
    let driver = crate::sdr::current_driver_info();
    // Tuner names are known only for devices the daemon already holds open;
    // do not block on a running sweep for them.
    let tuners: Vec<Option<String>> = match sdr.try_lock() {
        Ok(mut g) => (0..g.device_count()).map(|i| g.device_mut(i).and_then(|d| d.tuner_id())).collect(),
        Err(_) => Vec::new(),
    };
    let mut entries = Vec::new();
    let mut index = 0usize;
    for device in rusb::devices()?.iter() {
        let desc = device.device_descriptor()?;
        if !driver_state::is_known_device(desc.vendor_id(), desc.product_id()) {
            continue;
        }
        let serial = device
            .open()
            .ok()
            .and_then(|h| h.read_serial_number_string_ascii(&desc).ok());
        entries.push(SdrListEntry {
            index,
            vendor_id: format!("{:04X}", desc.vendor_id()),
            product_id: format!("{:04X}", desc.product_id()),
            product_name: driver_state::device_label(desc.vendor_id(), desc.product_id()),
            serial,
            bus: device.bus_number(),
            address: device.address(),
            driver_state: driver.state,
            bound_driver: driver.bound_driver.clone(),
            tuner: tuners.get(index).cloned().flatten(),
        });
        index += 1;
    }
    Ok(entries)
}

/// Exit code: the port is held by something that is not an Azimuth daemon.
pub const EXIT_PORT_FOREIGN: i32 = 42;
/// Exit code: the port is held by ANOTHER Azimuth daemon we could not (or must
/// not) replace. Distinct from success so a stale sibling is never silently
/// mistaken for "this daemon is running".
pub const EXIT_PORT_SIBLING: i32 = 43;

/// What a process holding our status port reports about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct SiblingStatus {
    pub has_node_id: bool,
    pub daemon_pid: Option<u32>,
    pub config_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingAction {
    /// Our own earlier instance (same config): ask it to stop and take over.
    TakeOver,
    /// Another Azimuth daemon: leave it alone and exit 43.
    ExitSibling,
    /// Not an Azimuth daemon: exit 42.
    ExitForeign,
}

fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

pub fn decide_sibling(our_config_path: &str, sibling: Option<&SiblingStatus>) -> SiblingAction {
    match sibling {
        Some(s) if s.has_node_id => {
            let same_config = s
                .config_path
                .as_deref()
                .is_some_and(|p| !p.is_empty() && same_path(p, our_config_path));
            // Only a daemon new enough to report its pid (and so to honour a
            // stop request) can be taken over.
            if same_config && s.daemon_pid.is_some() {
                SiblingAction::TakeOver
            } else {
                SiblingAction::ExitSibling
            }
        }
        _ => SiblingAction::ExitForeign,
    }
}

async fn probe_sibling(bind_addr: &str) -> Option<SiblingStatus> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;
    let v: serde_json::Value = client
        .get(format!("http://{bind_addr}/status"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(SiblingStatus {
        has_node_id: v.get("node_id").is_some(),
        daemon_pid: v.get("daemon_pid").and_then(|p| p.as_u64()).map(|p| p as u32),
        config_path: v.get("config_path").and_then(|p| p.as_str()).map(str::to_string),
    })
}

/// Bind the status port, retrying briefly for TIME_WAIT or a just-exiting old
/// instance. Err carries the process exit code to use.
pub async fn bind_with_policy(
    bind_addr: &str,
    our_config_path: &str,
    max_attempts: u32,
    backoff: impl Fn(u32) -> std::time::Duration,
) -> Result<tokio::net::TcpListener, i32> {
    for attempt in 1..=max_attempts {
        match tokio::net::TcpListener::bind(bind_addr).await {
            Ok(l) => {
                info!(addr = bind_addr, "Status server listening");
                return Ok(l);
            }
            Err(e) if attempt < max_attempts => {
                let wait = backoff(attempt);
                tracing::warn!(
                    error = %e, addr = bind_addr, attempt, backoff_ms = wait.as_millis() as u64,
                    "Status server bind failed, retrying"
                );
                tokio::time::sleep(wait).await;
            }
            Err(e) => {
                tracing::error!(error = %e, addr = bind_addr, "Status server bind failed after retries");
            }
        }
    }

    let sibling = probe_sibling(bind_addr).await;
    match decide_sibling(our_config_path, sibling.as_ref()) {
        SiblingAction::ExitForeign => {
            tracing::error!(
                addr = bind_addr,
                "Port is held by a non-Azimuth process — exiting {EXIT_PORT_FOREIGN} for service restart"
            );
            Err(EXIT_PORT_FOREIGN)
        }
        SiblingAction::ExitSibling => {
            let s = sibling.unwrap_or(SiblingStatus { has_node_id: true, daemon_pid: None, config_path: None });
            tracing::error!(
                addr = bind_addr, holder_pid = ?s.daemon_pid, holder_config = ?s.config_path,
                "Another Azimuth daemon holds this port and was not replaced — exiting {EXIT_PORT_SIBLING}; \
                 stop that daemon (or the AzimuthDaemon service) and start again"
            );
            Err(EXIT_PORT_SIBLING)
        }
        SiblingAction::TakeOver => {
            tracing::warn!(addr = bind_addr, "A previous instance with this config holds the port — asking it to stop");
            let stopped = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .ok();
            if let Some(c) = stopped {
                let _ = c.post(format!("http://{bind_addr}/shutdown")).send().await;
            }
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if let Ok(l) = tokio::net::TcpListener::bind(bind_addr).await {
                    info!(addr = bind_addr, "Took over the status port from the previous instance");
                    return Ok(l);
                }
            }
            tracing::error!(addr = bind_addr, "Previous instance did not release the port within 10 s");
            Err(EXIT_PORT_SIBLING)
        }
    }
}

/// Browser origins allowed to ask the daemon to stop: the desktop webview and
/// the daemon's own local dashboard. Any other web page is refused.
pub fn origin_allowed(origin: &str) -> bool {
    matches!(
        origin,
        "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
    ) || origin.starts_with("http://127.0.0.1")
        || origin.starts_with("http://localhost")
}

/// POST /shutdown: graceful stop, loopback callers only.
async fn stop_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"message": "loopback callers only"})),
        );
    }
    if let Some(origin) = headers.get(axum::http::header::ORIGIN).and_then(|o| o.to_str().ok()) {
        if !origin_allowed(origin) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"message": "origin not allowed"})),
            );
        }
    }
    info!(peer = %peer, "Stop requested over HTTP");
    state.stop_signal.notify_one();
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"message": "stopping", "daemon_pid": std::process::id()})),
    )
}

pub async fn run_status_server(bind_addr: &str, state: Arc<AppState>) {
    let app = Router::new()
        .route("/", get(crate::dashboard::dashboard_handler))
        .route("/status", get(status_handler))
        .route("/sdr/list", get(sdr_list_handler))
        .route("/sdr/test", post(sdr_test_handler))
        .route("/config/write", post(config_write_handler))
        .route("/scanner/start", post(scanner_start_handler))
        .route("/scanner/stop", post(scanner_stop_handler))
        .route("/network-support", post(network_support_handler))
        .route("/rtk-config", post(rtk_config_handler))
        .route("/rtk/status", get(rtk_status_handler))
        .route("/rtk/test", post(rtk_test_handler))
        .route("/gps/status", get(gps_status_handler))
        .route("/serial/ports", get(serial_ports_handler))
        .route("/log/level", post(log_level_handler))
        .route("/log/path", get(log_path_handler))
        .route("/shutdown", post(stop_handler))
        // The desktop app's webview calls this loopback-only server cross-origin
        // (tauri.localhost -> 127.0.0.1); without CORS every fetch is blocked.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(Arc::clone(&state));

    // A daemon without its status server is invisible to the desktop app
    // ("never fully loads"), so if the port stays taken we exit rather than
    // run headless (see bind_with_policy for the exit codes).
    let our_config = state.status.read().await.config_path_abs.clone();
    let listener = match bind_with_policy(bind_addr, &our_config, 10, |attempt| {
        std::time::Duration::from_millis(std::cmp::min(100u64 * (1 << attempt), 5_000))
    })
    .await
    {
        Ok(l) => l,
        Err(code) => std::process::exit(code),
    };
    let stop = Arc::clone(&state.stop_signal);
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        stop.notified().await;
        // Never wait forever on a stuck keep-alive connection.
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            std::process::exit(0);
        });
    });
    match served.await {
        Ok(()) => {
            info!("Status server stopped on request — exiting");
            // Give the non-blocking log writer a moment to flush.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            std::process::exit(0);
        }
        Err(e) => tracing::error!(error = %e, "Status server error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtk_config_persists_and_restores_across_simulated_restart() {
        // Regression test for Bug 4: RTK config POSTed via /rtk-config was
        // runtime-only — after a container recreation / daemon restart it
        // reset to enabled:false / provider:null. This simulates the full
        // round trip: persist what the fixed handler would have written, then
        // restore into a *fresh* StatusState the way main() does at startup.
        let base = std::env::temp_dir()
            .join(format!("azd-rtk-restart-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join(crate::rtk::RTK_CONFIG_FILE);
        let paths = vec![path.clone()];

        let ntrip = NtripConfig {
            caster_url: "rtk2go.com".to_string(),
            port: 2101,
            mountpoint: "TESTMOUNT".to_string(),
            username: Some("user".to_string()),
            password: Some("secret".to_string()),
            gga_interval_secs: 10,
            serial_port: None,
            baud_rate: 115200,
        };
        let persisted = crate::rtk::PersistedRtkConfig {
            version: 1,
            enabled: true,
            provider: Some("rtk2go".to_string()),
            ntrip: ntrip.clone(),
        };
        crate::rtk::save_to_paths(&paths, &persisted)
            .expect("save must succeed to an isolated tempdir");

        // "After restart" — a brand new StatusState, as main() creates at startup.
        let mut fresh = StatusState::new("node-1".to_string(), "tier1_rtlsdr".to_string());
        assert!(!fresh.rtk_enabled, "sanity: fresh state starts disabled");
        assert!(fresh.rtk_provider.is_none());

        let restored = crate::rtk::restore_from_paths(&mut fresh, &paths);

        assert!(restored, "restore must report it found a persisted config");
        assert!(fresh.rtk_enabled, "POSTed enabled:true must survive the simulated restart");
        assert_eq!(fresh.rtk_provider.as_deref(), Some("rtk2go"));
        assert_eq!(
            fresh.rtk_config.as_ref().map(|c| c.caster_url.as_str()),
            Some("rtk2go.com"),
            "POSTed caster config must survive the simulated restart"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn status_json_includes_auth_error_when_set() {
        let mut state = StatusState::new("node-1".to_string(), "tier1_rtlsdr".to_string());
        state.auth_error = Some(AUTH_ERROR_MSG.to_string());
        let json = serde_json::to_value(state.to_info(true, false)).unwrap();
        assert_eq!(
            json["auth_error"].as_str(),
            Some(AUTH_ERROR_MSG),
            "auth_error must be serialized when set"
        );
    }

    #[test]
    fn status_json_omits_auth_error_when_none() {
        let state = StatusState::new("node-1".to_string(), "tier1_rtlsdr".to_string());
        let json = serde_json::to_value(state.to_info(true, false)).unwrap();
        assert!(
            json.get("auth_error").is_none(),
            "auth_error must be omitted when None"
        );
    }

    #[test]
    fn status_json_serializes_config_error_as_literal_null_when_unset() {
        // Contract the Dockerfile HEALTHCHECK relies on: unlike auth_error,
        // config_error has NO skip_serializing_if, so /status always includes
        // the key — `"config_error":null` when healthy, a message string
        // otherwise. `grep -q '"config_error":null'` in the HEALTHCHECK CMD
        // depends on this key never being omitted.
        let state = StatusState::new("node-1".to_string(), "tier1_rtlsdr".to_string());
        let compact = serde_json::to_string(&state.to_info(true, false)).unwrap();
        assert!(
            compact.contains("\"config_error\":null"),
            "config_error must serialize as a literal null (not be omitted) when unset: {compact}"
        );
    }

    #[test]
    fn status_json_serializes_config_error_message_when_set() {
        let mut state = StatusState::new("node-1".to_string(), "tier1_rtlsdr".to_string());
        state.config_error = Some("Node not registered".to_string());
        let compact = serde_json::to_string(&state.to_info(true, false)).unwrap();
        assert!(
            compact.contains("\"config_error\":\"Node not registered\""),
            "config_error must serialize its message when set: {compact}"
        );
        assert!(!compact.contains("\"config_error\":null"));
    }

    fn signal(signal_type: &str, frequency_hz: u64) -> RecentSignal {
        RecentSignal {
            id: format!("{signal_type}-{frequency_hz}"),
            signal_type: signal_type.to_string(),
            frequency_hz,
            source_id: None,
            snr_db: None,
            status: "accepted".to_string(),
        }
    }

    #[test]
    fn multiplier_is_one_without_signals_or_devices() {
        assert_eq!(estimate_sdr_multiplier(&[], 4), 1.0);
        assert_eq!(estimate_sdr_multiplier(&[signal("fm", 98_500_000)], 0), 1.0);
    }

    #[test]
    fn frequencies_closer_than_min_spacing_count_once() {
        // 100 kHz apart, well inside the 2 MHz FM spacing rule.
        let signals = [signal("fm", 98_500_000), signal("fm", 98_600_000)];
        assert_eq!(
            estimate_sdr_multiplier(&signals, 4),
            1.0,
            "adjacent FM signals must not both count"
        );
    }

    #[test]
    fn well_separated_frequencies_each_count() {
        let signals = [
            signal("fm", 92_000_000),
            signal("fm", 98_500_000),
            signal("lte", 751_000_000),
        ];
        assert_eq!(estimate_sdr_multiplier(&signals, 4), 3.0);
    }

    #[test]
    fn device_count_caps_the_multiplier() {
        let signals = [
            signal("fm", 92_000_000),
            signal("fm", 98_500_000),
            signal("dtv", 605_000_000),
        ];
        assert_eq!(
            estimate_sdr_multiplier(&signals, 1),
            1.0,
            "a single dongle cannot earn more than 1x"
        );
    }

    #[test]
    fn spacing_is_per_signal_type() {
        // Same numeric gap (3 MHz): legal for FM (2 MHz), too close for LTE (5 MHz).
        let fm = [signal("fm", 90_000_000), signal("fm", 93_000_000)];
        let lte = [signal("lte", 750_000_000), signal("lte", 753_000_000)];
        assert_eq!(estimate_sdr_multiplier(&fm, 4), 2.0);
        assert_eq!(estimate_sdr_multiplier(&lte, 4), 1.0);
    }

    // X4: real signal type names (not bare categories) must map to their
    // category before the spacing rule is applied.
    #[test]
    fn real_fm_rds_type_uses_fm_spacing() {
        let signals = [signal("fm_rds", 98_500_000), signal("fm_rds", 98_600_000)];
        assert_eq!(estimate_sdr_multiplier(&signals, 4), 1.0);
    }

    #[test]
    fn real_lte_pss_type_uses_lte_spacing() {
        let signals = [signal("lte_pss", 750_000_000), signal("lte_pss", 753_000_000)];
        assert_eq!(estimate_sdr_multiplier(&signals, 4), 1.0);
    }

    #[test]
    fn real_dtv_types_use_dtv_spacing() {
        for t in ["dtv_atsc", "dtv_dvbt", "dtv_dvbt2"] {
            let close = [signal(t, 600_000_000), signal(t, 602_000_000)];
            assert_eq!(estimate_sdr_multiplier(&close, 4), 1.0, "{t} 2 MHz apart counts once");
            let far = [signal(t, 600_000_000), signal(t, 606_000_000)];
            assert_eq!(estimate_sdr_multiplier(&far, 4), 2.0, "{t} 6 MHz apart counts twice");
        }
    }

    // --- R1: hot-apply of a saved config -------------------------------------

    /// Recorded requests of the mock Azimuth API.
    type Seen = Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>;

    async fn spawn_mock_api() -> (String, Seen) {
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = Arc::clone(&seen);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
            let rec = Arc::clone(&rec);
            async move {
                let v: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let path = uri.path().to_string();
                rec.lock().unwrap().push((path.clone(), v));
                if path == "/api/nodes/register" {
                    axum::Json(serde_json::json!({"node_id": "real-node-1"}))
                } else if path == "/api/nodes/mine" {
                    axum::Json(serde_json::json!([{"id":"real-node-1","display_name":"Brave Otter"}]))
                } else {
                    axum::Json(serde_json::json!({"accepted": 1, "rejected": []}))
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), seen)
    }

    struct Harness {
        state: Arc<AppState>,
        apply_rx: tokio::sync::mpsc::UnboundedReceiver<ApplyMsg>,
        dir: PathBuf,
        seen: Seen,
        client: Arc<crate::api_client::ApiClient>,
    }

    async fn harness(tag: &str) -> Harness {
        let (base, seen) = spawn_mock_api().await;
        let dir = std::env::temp_dir().join(format!("az-r1-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config_path = dir.join("config.toml");
        let key = Arc::new(std::sync::RwLock::new(String::new()));
        let client = Arc::new(crate::api_client::ApiClient::new(&base, Arc::clone(&key)));
        let status = Arc::new(RwLock::new(StatusState::new(
            "placeholder-test".to_string(),
            "tier1_rtlsdr".to_string(),
        )));
        let registrar = Arc::new(
            Registrar::new(Arc::clone(&client), Arc::clone(&status), config_path.clone(), "fp".to_string())
                .with_schedule(vec![std::time::Duration::ZERO], 3),
        );
        let (apply_tx, apply_rx) = tokio::sync::mpsc::unbounded_channel();
        let (gps_cfg_tx, _gps_rx) = tokio::sync::watch::channel(crate::config::GpsConfig::default());
        let state = Arc::new(AppState {
            status,
            scanning_enabled: Arc::new(AtomicBool::new(true)),
            scanner_active: Arc::new(AtomicBool::new(false)),
            config_path,
            api_key: key,
            log: crate::logging::LogHandle::for_tests(dir.join("logs")),
            apply_lock: tokio::sync::Mutex::new(()),
            apply_tx,
            gps_cfg_tx,
            registrar,
            sdr: Arc::new(std::sync::Mutex::new(SdrManager::empty())),
            stop_signal: Arc::new(tokio::sync::Notify::new()),
        });
        Harness { state, apply_rx, dir, seen, client }
    }

    fn wizard_body(lat: f64, lon: f64, key: &str) -> serde_json::Value {
        let mut c = Config::default();
        c.api.key = key.to_string();
        c.node.label = "Test".to_string();
        c.node.location.latitude = lat;
        c.node.location.longitude = lon;
        serde_json::to_value(&c).unwrap()
    }

    async fn post_config(h: &Harness, body: serde_json::Value) -> StatusCode {
        config_write_handler(State(Arc::clone(&h.state)), Json(body))
            .await
            .into_response()
            .status()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_write_replaces_a_stale_config_error() {
        let h = harness("clear").await;
        {
            let mut s = h.state.status.write().await;
            s.config_state = ConfigState::Invalid;
            s.config_error = Some("Config load failed: stale".to_string());
        }
        let code = post_config(&h, wizard_body(39.8283, -98.5795, "k1")).await;
        assert_eq!(code, StatusCode::OK);
        let s = h.state.status.read().await;
        assert_eq!(s.config_error, None, "stale startup error must be replaced");
        assert_eq!(s.config_state, ConfigState::Ok);
        assert_eq!(s.live_location.0, 39.8283);
        assert_eq!(h.state.api_key.read().unwrap().as_str(), "k1");
        let saved = Config::load(&h.state.config_path).unwrap();
        assert_eq!(saved.node.location.latitude, 39.8283);
        let _ = std::fs::remove_dir_all(&h.dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_write_with_zero_location_is_rejected_and_not_saved() {
        let h = harness("zero").await;
        let code = post_config(&h, wizard_body(0.0, 0.0, "k1")).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!h.state.config_path.exists(), "an unusable config must not be written");
        assert_ne!(h.state.status.read().await.config_state, ConfigState::Ok);
        let _ = std::fs::remove_dir_all(&h.dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_write_registers_the_node_without_a_restart() {
        let mut h = harness("reg").await;
        assert_eq!(post_config(&h, wizard_body(48.8566, 2.3522, "k1")).await, StatusCode::OK);
        // The scan plan was handed to the observation loop.
        assert!(h.apply_rx.try_recv().is_ok());
        let mut registered = false;
        for _ in 0..100 {
            let st = h.state.status.read().await;
            // The display name is fetched right after the state flips.
            if st.registration_state == "registered" && st.node_display_name.is_some() {
                registered = true;
                break;
            }
            drop(st);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(registered, "registration must complete after the write");
        let s = h.state.status.read().await;
        assert_eq!(s.node_id, "real-node-1");
        assert!(!node_runtime::is_placeholder(&s.node_id));
        assert_eq!(s.node_display_name.as_deref(), Some("Brave Otter"));
        let saved = Config::load(&h.state.config_path).unwrap();
        assert_eq!(saved.node.id, "real-node-1", "the issued id is persisted");
        let _ = std::fs::remove_dir_all(&h.dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn submissions_use_the_live_node_id_after_deferred_registration() {
        let h = harness("x2").await;
        let obs = || {
            vec![crate::observation::Observation {
                signal_type: "fm_rds".to_string(),
                observed_at: chrono::Utc::now(),
                frequency_hz: Some(98_500_000),
                timestamp_ns: None,
                tdoa_offset_ns: None,
                signal_strength_dbm: None,
                snr_db: None,
                source_id: None,
                raw_data: None,
            }]
        };
        h.state.status.write().await.config_state = ConfigState::Ok;
        // Placeholder: nothing is sent.
        node_runtime::submit_batch(&h.state.status, &h.client, obs()).await;
        assert!(h.seen.lock().unwrap().iter().all(|(p, _)| p != "/api/observations"));
        // Deferred registration completes; the next batch carries the real id.
        h.state.registrar.finish_registration("real-node-1").await;
        node_runtime::submit_batch(&h.state.status, &h.client, obs()).await;
        let seen = h.seen.lock().unwrap();
        let sent: Vec<_> = seen.iter().filter(|(p, _)| p == "/api/observations").collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1["node_id"], "real-node-1");
        drop(seen);
        assert!(h.state.status.read().await.last_accepted_at.is_some());
        let _ = std::fs::remove_dir_all(&h.dir);
    }

    // R1.7: the Dockerfile HEALTHCHECK is healthy ONLY when config_state == "ok".
    fn healthcheck_pattern() -> String {
        let dockerfile = include_str!("../Dockerfile");
        let line = dockerfile
            .lines()
            .find(|l| l.contains("curl -fsS http://127.0.0.1:8420/status"))
            .expect("Dockerfile HEALTHCHECK command");
        let start = line.find("grep -q '").expect("grep -q in HEALTHCHECK") + "grep -q '".len();
        let end = line[start..].find('\'').unwrap() + start;
        line[start..end].to_string()
    }

    #[test]
    fn healthcheck_is_healthy_only_when_config_state_is_ok() {
        let pattern = healthcheck_pattern();
        assert_eq!(pattern, "\"config_state\":\"ok\"");
        for (state, healthy) in [
            (ConfigState::Ok, true),
            (ConfigState::AwaitingSetup, false),
            (ConfigState::Invalid, false),
        ] {
            let mut s = StatusState::new("n".to_string(), "tier1_rtlsdr".to_string());
            s.config_state = state;
            let compact = serde_json::to_string(&s.to_info(true, false)).unwrap();
            assert_eq!(compact.contains(&pattern), healthy, "{state:?}: {compact}");
        }
    }

    #[test]
    fn config_error_is_null_unless_state_is_invalid() {
        let mut s = StatusState::new("n".to_string(), "tier1_rtlsdr".to_string());
        s.config_state = ConfigState::AwaitingSetup;
        let compact = serde_json::to_string(&s.to_info(true, false)).unwrap();
        assert!(compact.contains("\"config_error\":null"), "{compact}");
        assert!(compact.contains("\"config_state\":\"awaiting_setup\""), "{compact}");
    }

    // --- X3: sibling / stale daemon on the status port -----------------------

    fn sib(pid: Option<u32>, cfg: Option<&str>) -> SiblingStatus {
        SiblingStatus {
            has_node_id: true,
            daemon_pid: pid,
            config_path: cfg.map(str::to_string),
        }
    }

    #[test]
    fn sibling_decision_matrix() {
        let ours = "/home/u/config.toml";
        assert_eq!(decide_sibling(ours, None), SiblingAction::ExitForeign);
        let not_azimuth = SiblingStatus { has_node_id: false, daemon_pid: None, config_path: None };
        assert_eq!(decide_sibling(ours, Some(&not_azimuth)), SiblingAction::ExitForeign);
        assert_eq!(decide_sibling(ours, Some(&sib(Some(7), Some(ours)))), SiblingAction::TakeOver);
        assert_eq!(
            decide_sibling(ours, Some(&sib(Some(7), Some("/other/config.toml")))),
            SiblingAction::ExitSibling
        );
        // v0.3.8 style sibling: no pid, no config path -> cannot be replaced.
        assert_eq!(decide_sibling(ours, Some(&sib(None, None))), SiblingAction::ExitSibling);
    }

    #[test]
    fn stop_origin_allowlist() {
        assert!(origin_allowed("tauri://localhost"));
        assert!(origin_allowed("http://tauri.localhost"));
        assert!(origin_allowed("http://127.0.0.1:8420"));
        assert!(!origin_allowed("https://evil.example"));
        assert!(!origin_allowed("null"));
    }

    /// A fake daemon on an ephemeral port: serves /status and, when it is
    /// asked to stop, releases the port.
    async fn fake_daemon(status: serde_json::Value) -> String {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
        let app = Router::new()
            .route("/status", get(move || {
                let v = status.clone();
                async move { Json(v) }
            }))
            .route("/shutdown", post(move || {
                let tx = Arc::clone(&tx);
                async move {
                    if let Some(t) = tx.lock().unwrap().take() {
                        let _ = t.send(());
                    }
                    StatusCode::ACCEPTED
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
                .ok();
        });
        addr
    }

    fn quick(_: u32) -> std::time::Duration {
        std::time::Duration::from_millis(5)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn port_held_by_a_non_azimuth_process_exits_42() {
        let holder = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = holder.local_addr().unwrap().to_string();
        let err = bind_with_policy(&addr, "/c.toml", 2, quick).await.unwrap_err();
        assert_eq!(err, EXIT_PORT_FOREIGN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn port_held_by_another_azimuth_daemon_exits_43_not_0() {
        let addr = fake_daemon(serde_json::json!({
            "node_id": "n", "daemon_pid": 99, "config_path": "/other/config.toml"
        }))
        .await;
        let err = bind_with_policy(&addr, "/c.toml", 2, quick).await.unwrap_err();
        assert_eq!(err, EXIT_PORT_SIBLING);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn old_sibling_without_pid_exits_43() {
        let addr = fake_daemon(serde_json::json!({"node_id": "n"})).await;
        let err = bind_with_policy(&addr, "/c.toml", 2, quick).await.unwrap_err();
        assert_eq!(err, EXIT_PORT_SIBLING);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn own_stale_instance_is_stopped_and_the_port_taken_over() {
        let addr = fake_daemon(serde_json::json!({
            "node_id": "n", "daemon_pid": 99, "config_path": "/c.toml"
        }))
        .await;
        let listener = bind_with_policy(&addr, "/c.toml", 2, quick)
            .await
            .expect("same-config sibling must be replaced");
        assert_eq!(listener.local_addr().unwrap().to_string(), addr);
    }

    // --- R2: /sdr/test really tests ------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn sdr_test_never_trusts_the_cached_connected_flag() {
        // HEAD answered `opened: true` from `sdr_connected && device_count > index`
        // whenever scanning was enabled (always), without touching the device.
        let h = harness("sdrtest").await;
        // No dongle attached (never touch the real hardware of the test machine).
        *crate::sdr::TEST_HW.lock().unwrap() = Some((
            DriverInfo::new(DriverStateKind::NotFound, "No RTL-SDR is connected."),
            Err(driver_state::OpenFailure::NotFound),
        ));
        {
            let mut s = h.state.status.write().await;
            s.sdr_connected = true;
            s.device_count = 1;
        }
        let resp = sdr_test_handler(State(Arc::clone(&h.state)), Json(SdrTestRequest { index: 0 }))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_ne!(v["opened"], true, "cached flag must not produce opened:true: {v}");
        assert_eq!(v["state"], "not_found", "no dongle is attached to this machine: {v}");
        assert!(v["detail"].as_str().unwrap().len() > 3);
        // The result is kept for /status.
        let s = h.state.status.read().await;
        assert_eq!(s.sdr_test_last.as_ref().unwrap()["state"], "not_found");
        assert_eq!(s.driver.state, DriverStateKind::NotFound);
        assert_eq!(s.driver_status, "DeviceNotFound");
        *crate::sdr::TEST_HW.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&h.dir);
    }

    #[test]
    fn sdr_test_while_a_sweep_holds_the_device_is_ok_only_if_the_scanner_is_active() {
        let m = std::sync::Mutex::new(SdrManager::empty());
        let _held = m.lock().unwrap();
        let wait = std::time::Duration::from_millis(150);
        let active = ScannerFacts { scanner_active: true, sdr_connected: true };
        let (v, _) = std::thread::scope(|sc| {
            sc.spawn(|| run_sdr_test(&m, 0, active, wait)).join().unwrap()
        });
        assert_eq!(v.state, SdrTestState::Ok);
        let idle = ScannerFacts { scanner_active: false, sdr_connected: true };
        let (v, _) = std::thread::scope(|sc| {
            sc.spawn(|| run_sdr_test(&m, 0, idle, wait)).join().unwrap()
        });
        assert_eq!(v.state, SdrTestState::UsbError);
    }
}

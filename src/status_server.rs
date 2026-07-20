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

use crate::config::{Config, NtripConfig};
use crate::sdr::SdrManager;
use crate::gps::AccuracyPoint;
use std::collections::VecDeque;

const RTL_SDR_VID: u16 = 0x0BDA;
const RTL_SDR_PID_V4: u16 = 0x2832; // RTL2832U V4
const RTL_SDR_PID_V3: u16 = 0x2838; // RTL2832U V3

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
}

pub struct StatusState {
    pub node_id: String,
    pub hardware_type: String,
    pub start_time: Instant,
    pub observations_submitted: u64,
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
    pub sdr_connected: bool,
    pub sdr_name: String,
    pub sdr_multiplier: f64,
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
}

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
            // TODO: fetch actual multiplier from web API
            sdr_multiplier: 1.0,
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
        }
    }

    fn to_info(&self,
        scanning_enabled: bool,
        scanner_active: bool,
    ) -> StatusInfo {
        StatusInfo {
            node_id: self.node_id.clone(),
            hardware_type: self.hardware_type.clone(),
            uptime_secs: self.start_time.elapsed().as_secs(),
            observations_submitted: self.observations_submitted,
            last_heartbeat: self.last_heartbeat.map(|t| t.to_rfc3339()),
            sdr_connected: self.sdr_connected,
            sdr_name: self.sdr_name.clone(),
            sdr_multiplier: self.sdr_multiplier,
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
        }
    }
}

/// Shared application state used by the HTTP status/control server.
pub struct AppState {
    pub status: Arc<RwLock<StatusState>>,
    pub scanning_enabled: Arc<AtomicBool>,
    pub scanner_active: Arc<AtomicBool>,
    pub config_path: PathBuf,
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
}

#[derive(Debug, Clone, Deserialize)]
struct SdrTestRequest {
    index: u32,
}

#[derive(Debug, Clone, Serialize)]
struct SdrTestResponse {
    index: u32,
    opened: bool,
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

async fn sdr_list_handler(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let entries = tokio::task::block_in_place(enumerate_sdr_devices);
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

async fn sdr_test_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SdrTestRequest>,
) -> impl IntoResponse {
    // When the scanner holds the device handle, opening a second WinUSB handle
    // fails with "Access denied". Use the scanner's cached connection state instead.
    if state.scanning_enabled.load(Ordering::Relaxed) {
        let s = state.status.read().await;
        let connected = s.sdr_connected && s.device_count > req.index as usize;
        return Ok((StatusCode::OK, Json(SdrTestResponse {
            index: req.index,
            opened: connected,
        })));
    }

    // Scanner not enabled — safe to open a test handle (no contention).
    let result = tokio::task::block_in_place(move || test_sdr_device(req.index));
    match result {
        Ok(opened) => Ok((StatusCode::OK, Json(SdrTestResponse {
            index: req.index,
            opened,
        }))),
        Err(e) => {
            warn!(index = req.index, error = %e, "SDR test failed");
            Err((
                StatusCode::BAD_REQUEST,
                Json(MessageResponse {
                    message: format!("SDR test failed: {}", e),
                }),
            ))
        }
    }
}

async fn config_write_handler(
    State(state): State<Arc<AppState>>,
    Json(mut config): Json<Config>,
) -> impl IntoResponse {
    let config_path = state.config_path.clone();

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
            .filter(|id| !id.is_empty() && !id.starts_with("placeholder-"));
        let adopted = existing_id.or_else(|| {
            crate::identity::load(&config_path).map(|i| i.node_id)
        });
        if let Some(id) = adopted {
            info!(node_id = %id, "Preserving existing node identity through config write");
            config.node.id = id;
        }
    }

    let config_path_for_save = config_path.clone();
    let result = tokio::task::block_in_place(move || config.save(&config_path_for_save));
    match result {
        Ok(()) => {
            info!(path = %config_path.display(), "Config written from HTTP request");
            Ok((StatusCode::OK, Json(MessageResponse {
                message: "Config written".to_string(),
            })))
        }
        Err(e) => {
            warn!(path = %config_path.display(), error = %e, "Failed to write config");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(MessageResponse {
                    message: format!("Failed to write config: {}", e),
                }),
            ))
        }
    }
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
    let mut s = state.status.write().await;
    s.rtk_enabled = req.enabled;
    if let Some(provider) = req.provider.clone() {
        s.rtk_provider = Some(provider);
    }
    // Build NtripConfig from request fields if provided
    if let (Some(caster_url), Some(mountpoint)) = (req.caster_url.clone(), req.mountpoint.clone()) {
        if !caster_url.is_empty() && !mountpoint.is_empty() {
            s.rtk_config = Some(NtripConfig {
                caster_url,
                port: req.port.unwrap_or(2101),
                mountpoint,
                username: req.username.clone(),
                password: req.password.clone(),
                gga_interval_secs: 10,
                serial_port: None,
                baud_rate: 115200,
            });
        }
    }
    if !req.enabled {
        s.rtk_connected = false;
    }
    info!(enabled = req.enabled, provider = ?req.provider, "RTK config updated via HTTP");
    Json(MessageResponse {
        message: format!("RTK {}", if req.enabled { "enabled" } else { "disabled" }),
    })
}

#[derive(Debug, Clone, Serialize)]
struct RtkStatusResponse {
    enabled: bool,
    connected: bool,
    provider: Option<String>,
    messages_received: u64,
    last_correction: Option<String>,
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

fn enumerate_sdr_devices() -> anyhow::Result<Vec<SdrListEntry>> {
    let mut entries = Vec::new();
    let mut index = 0usize;
    for device in rusb::devices()?.iter() {
        let desc = device.device_descriptor()?;
        if desc.vendor_id() == RTL_SDR_VID {
            let product_name = match desc.product_id() {
                RTL_SDR_PID_V4 => "RTL2832U V4".to_string(),
                RTL_SDR_PID_V3 => "RTL2832U V3".to_string(),
                _ => continue, // Skip non-matching PIDs
            };
            let serial = device
                .open()
                .ok()
                .and_then(|h| h.read_serial_number_string_ascii(&desc).ok());
            entries.push(SdrListEntry {
                index,
                vendor_id: format!("{:04X}", desc.vendor_id()),
                product_id: format!("{:04X}", desc.product_id()),
                product_name,
                serial,
                bus: device.bus_number(),
                address: device.address(),
            });
            index += 1;
        }
    }
    Ok(entries)
}

fn test_sdr_device(index: u32) -> anyhow::Result<bool> {
    // Use default sample rate and auto gain for a brief open/close test.
    let sample_rate = 2_400_000;
    let gain = 0;
    let manager = SdrManager::new(Some(index), sample_rate, gain);
    let opened = manager.is_connected();
    // Manager is dropped here, closing the handle.
    Ok(opened)
}

pub async fn run_status_server(bind_addr: &str, state: Arc<AppState>) {
    let app = Router::new()
        .route("/status", get(status_handler))
        .route("/sdr/list", get(sdr_list_handler))
        .route("/sdr/test", post(sdr_test_handler))
        .route("/config/write", post(config_write_handler))
        .route("/scanner/start", post(scanner_start_handler))
        .route("/scanner/stop", post(scanner_stop_handler))
        .route("/network-support", post(network_support_handler))
        .route("/rtk-config", post(rtk_config_handler))
        .route("/rtk/status", get(rtk_status_handler))
        .route("/gps/status", get(gps_status_handler))
        .route("/serial/ports", get(serial_ports_handler))
        .with_state(Arc::clone(&state));

    // Bind with retry: transient TIME_WAIT or a just-exiting old instance
    // resolves within seconds. A daemon without its status server is invisible
    // to the desktop app ("never fully loads"), so if the port stays taken we
    // exit rather than run headless:
    //   - a sibling Azimuth daemon already serves the port -> exit 0 (the app
    //     talks to that one; NSSM AppExit for 0 is configured to Exit)
    //   - anything else -> exit 42 (NSSM restarts us; sidecar exit is visible)
    const MAX_BIND_ATTEMPTS: u32 = 10;
    let mut listener = None;
    for attempt in 1..=MAX_BIND_ATTEMPTS {
        match tokio::net::TcpListener::bind(bind_addr).await {
            Ok(l) => {
                info!(addr = bind_addr, "Status server listening");
                listener = Some(l);
                break;
            }
            Err(e) if attempt < MAX_BIND_ATTEMPTS => {
                let backoff_ms = std::cmp::min(100u64 * (1 << attempt), 5_000);
                tracing::warn!(
                    error = %e, addr = bind_addr, attempt, backoff_ms,
                    "Status server bind failed, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }
            Err(e) => {
                tracing::error!(error = %e, addr = bind_addr, "Status server bind failed after retries");
                let probe_url = format!("http://{bind_addr}/status");
                let sibling = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(3))
                    .build()
                    .ok();
                let is_sibling = match sibling {
                    Some(c) => match c.get(&probe_url).send().await {
                        Ok(resp) => resp
                            .json::<serde_json::Value>()
                            .await
                            .map(|v| v.get("node_id").is_some())
                            .unwrap_or(false),
                        Err(_) => false,
                    },
                    None => false,
                };
                if is_sibling {
                    tracing::warn!(
                        addr = bind_addr,
                        "Another Azimuth daemon already serves this port — exiting (it stays in charge)"
                    );
                    std::process::exit(0);
                }
                tracing::error!(
                    addr = bind_addr,
                    "Port is held by a non-Azimuth process — exiting 42 for service restart"
                );
                std::process::exit(42);
            }
        }
    }
    let listener = listener.expect("bind loop guarantees a listener or an exit");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "Status server error");
    }
}

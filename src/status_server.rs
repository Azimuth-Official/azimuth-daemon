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

const RTL_SDR_VID: u16 = 0x0BDA;
const RTL_SDR_PID: u16 = 0x2832;

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
    let entries = tokio::task::block_in_place(|| enumerate_sdr_devices());
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
    Json(config): Json<Config>,
) -> impl IntoResponse {
    let config_path = state.config_path.clone();
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
        if desc.vendor_id() == RTL_SDR_VID && desc.product_id() == RTL_SDR_PID {
            let serial = device
                .open()
                .ok()
                .and_then(|h| h.read_serial_number_string_ascii(&desc).ok());
            entries.push(SdrListEntry {
                index,
                vendor_id: format!("{:04X}", desc.vendor_id()),
                product_id: format!("{:04X}", desc.product_id()),
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
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("Failed to bind status server");
    info!(addr = bind_addr, "Status server listening");
    axum::serve(listener, app).await.expect("Status server error");
}

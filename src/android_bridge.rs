//! JNI bridge for the Android Observer app's OTG SDR mode.
//!
//! The app's USB Host integration opens the RTL-SDR dongle and hands its file
//! descriptor across JNI. This module wraps that fd via libusb
//! (`wrap_sys_device` under rusb's `open_device_with_fd`) and runs the same
//! scanner pipeline the desktop daemon uses: regional scan cycles → signed
//! observation submission through [`crate::api_client::ApiClient`].
//!
//! Design notes:
//! - The phone registers as a SEPARATE `tier1_rtlsdr` node. The machine
//!   fingerprint comes from Kotlin (hash of ANDROID_ID) because the desktop
//!   fingerprint sources (machine GUID / machine-id) don't exist on Android
//!   and would collide across devices.
//! - No status server, GPS, or NTRIP here — Kotlin polls `getStatus()` JSON.
//! - Everything runs on one dedicated OS thread; async API calls go through a
//!   local current-thread tokio runtime via `block_on`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::api_client::{self, ApiClient};
use crate::config::Config;
use crate::pending;
use crate::sdr::scanner::Scanner;
use crate::sdr::SdrManager;

/// `LIBUSB_OPTION_NO_DEVICE_DISCOVERY` (alias `LIBUSB_OPTION_WEAK_AUTHORITY`).
/// Must be set on the NULL context before the first `libusb_init`, or libusb
/// tries to scan /dev/bus/usb, which Android's SELinux policy denies.
const LIBUSB_OPTION_NO_DEVICE_DISCOVERY: u32 = 2;

static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default, Clone, Serialize)]
struct PipelineStatus {
    running: bool,
    node_id: Option<String>,
    sdr_connected: bool,
    signals_found: u64,
    observations_submitted: u64,
    last_heartbeat_unix: Option<i64>,
    last_error: Option<String>,
    version: &'static str,
}

fn status() -> &'static Mutex<PipelineStatus> {
    static STATUS: Mutex<PipelineStatus> = Mutex::new(PipelineStatus {
        running: false,
        node_id: None,
        sdr_connected: false,
        signals_found: 0,
        observations_submitted: 0,
        last_heartbeat_unix: None,
        last_error: None,
        version: env!("CARGO_PKG_VERSION"),
    });
    &STATUS
}

fn set_status(f: impl FnOnce(&mut PipelineStatus)) {
    if let Ok(mut s) = status().lock() {
        f(&mut s);
    }
}

/// Config JSON handed over from Kotlin (`SdrNodeService.buildConfigJson`).
#[derive(Debug, Deserialize)]
struct AndroidConfig {
    #[serde(default)]
    api_url: Option<String>,
    api_key: String,
    node_label: String,
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    altitude_m: f64,
    #[serde(default)]
    region: Option<String>,
    /// <=0 → AGC, >0 → manual dB (same semantics as config.toml `sdr.gain`).
    #[serde(default)]
    gain: i32,
    /// Device-stable fingerprint computed by Kotlin (sha256 of ANDROID_ID).
    fingerprint: String,
    /// Previously registered SDR node id, offered to the server as a reclaim
    /// hint so reinstalls don't create duplicate nodes.
    #[serde(default)]
    node_id_hint: Option<String>,
}

/// Forward `tracing` output to logcat line-by-line through the `log` facade
/// (android_logger). Keeps the daemon's existing tracing calls visible in
/// `adb logcat` without touching any shared module.
struct LogcatWriter(Vec<u8>);

impl std::io::Write for LogcatWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        while let Some(pos) = self.0.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.0.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line);
            let trimmed = text.trim_end();
            if !trimmed.is_empty() {
                log::info!(target: "AzimuthDaemon", "{}", trimmed);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn init_logging() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("AzimuthDaemon"),
    );
    // Ignore the error if a subscriber is already installed (repeat start).
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(|| LogcatWriter(Vec::new()))
        .try_init();
}

/// Start the SDR pipeline with a pre-opened USB file descriptor.
/// Kotlin: `NativeDaemon.startWithFd(fd: Int, configJson: String): Boolean`
#[no_mangle]
pub extern "system" fn Java_day_azimuth_observer_sdr_NativeDaemon_startWithFd<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    fd: jint,
    config_json: JString<'local>,
) -> jboolean {
    init_logging();

    if RUNNING.swap(true, Ordering::SeqCst) {
        warn!("startWithFd called while pipeline already running");
        return JNI_FALSE;
    }

    let config_str: String = match env.get_string(&config_json) {
        Ok(s) => s.into(),
        Err(e) => {
            error!(error = %e, "failed to read config JSON from JNI");
            RUNNING.store(false, Ordering::SeqCst);
            return JNI_FALSE;
        }
    };

    let android_config: AndroidConfig = match serde_json::from_str(&config_str) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "invalid config JSON");
            set_status(|s| s.last_error = Some(format!("invalid config JSON: {e}")));
            RUNNING.store(false, Ordering::SeqCst);
            return JNI_FALSE;
        }
    };

    STOP.store(false, Ordering::SeqCst);
    set_status(|s| {
        *s = PipelineStatus {
            running: true,
            version: env!("CARGO_PKG_VERSION"),
            ..Default::default()
        };
    });

    info!(fd, "starting SDR pipeline thread");
    let spawn_result = std::thread::Builder::new()
        .name("azimuth-sdr".into())
        .spawn(move || {
            pipeline_thread(fd, android_config);
            RUNNING.store(false, Ordering::SeqCst);
            set_status(|s| {
                s.running = false;
                s.sdr_connected = false;
            });
            info!("SDR pipeline thread exited");
        });

    match spawn_result {
        Ok(_) => JNI_TRUE,
        Err(e) => {
            error!(error = %e, "failed to spawn pipeline thread");
            RUNNING.store(false, Ordering::SeqCst);
            JNI_FALSE
        }
    }
}

/// Request pipeline shutdown. The scanner loop observes the flag between
/// cycles and closes the SDR handle on exit.
#[no_mangle]
pub extern "system" fn Java_day_azimuth_observer_sdr_NativeDaemon_stop(
    _env: JNIEnv,
    _class: JClass,
) {
    info!("stop requested");
    STOP.store(true, Ordering::SeqCst);
}

/// Current pipeline status as a JSON string.
#[no_mangle]
pub extern "system" fn Java_day_azimuth_observer_sdr_NativeDaemon_getStatus<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let json = status()
        .lock()
        .ok()
        .and_then(|s| serde_json::to_string(&*s).ok())
        .unwrap_or_else(|| "{}".to_string());
    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Try to register (or reclaim) the phone's tier1 node. Returns the node id on
/// success. A present-but-unreachable server adopts the hint so observations
/// can queue against the known id once connectivity returns.
fn ensure_registered(
    rt: &tokio::runtime::Runtime,
    client: &ApiClient,
    cfg: &AndroidConfig,
) -> Option<String> {
    match rt.block_on(client.register_node(
        "tier1_rtlsdr",
        &cfg.node_label,
        cfg.latitude,
        cfg.longitude,
        cfg.altitude_m,
        Some(&cfg.fingerprint),
        cfg.node_id_hint.as_deref(),
    )) {
        Ok(node_id) => {
            info!(node_id = %node_id, "SDR node registered");
            set_status(|s| {
                s.node_id = Some(node_id.clone());
                s.last_error = None;
            });
            Some(node_id)
        }
        Err(e) => {
            warn!(error = %e, "node registration failed");
            set_status(|s| s.last_error = Some(format!("registration failed: {e}")));
            if let Some(hint) = &cfg.node_id_hint {
                info!(node_id = %hint, "adopting persisted node id while offline");
                set_status(|s| s.node_id = Some(hint.clone()));
                Some(hint.clone())
            } else {
                None
            }
        }
    }
}

fn pipeline_thread(fd: i32, android_config: AndroidConfig) {
    // Allow libusb to wrap the app-provided fd without scanning /dev/bus/usb.
    unsafe {
        rusb::ffi::libusb_set_option(std::ptr::null_mut(), LIBUSB_OPTION_NO_DEVICE_DISCOVERY);
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "failed to build tokio runtime");
            set_status(|s| s.last_error = Some(format!("runtime init failed: {e}")));
            return;
        }
    };

    // Assemble a daemon Config from the Kotlin-provided values; regional sweep
    // plans resolve exactly as on desktop (embedded region database).
    let mut config = Config::default();
    config.api.url = android_config
        .api_url
        .clone()
        .unwrap_or_else(|| "https://azimuth.day".to_string());
    config.api.key = android_config.api_key.clone();
    config.node.hardware_type = "tier1_rtlsdr".to_string();
    config.node.label = android_config.node_label.clone();
    config.node.location.latitude = android_config.latitude;
    config.node.location.longitude = android_config.longitude;
    config.node.location.altitude_m = android_config.altitude_m;
    config.sdr.gain = android_config.gain;
    config.region = android_config.region.clone();
    config.apply_region();

    let api_key = Arc::new(std::sync::RwLock::new(config.api.key.clone()));
    let client = ApiClient::new(&config.api.url, api_key);

    let mut node_id = ensure_registered(&rt, &client, &android_config);

    // Open the SDR from the wrapped fd. Failure here is terminal for this run:
    // Android revokes the fd on unplug, and the app restarts the pipeline on
    // the next permission grant.
    let mut sdr_manager = match SdrManager::new_from_fd(
        fd,
        config.sdr.sample_rate,
        config.sdr.gain,
        &android_config.fingerprint,
    ) {
        Ok(m) => m,
        Err(e) => {
            error!(error = %e, fd, "failed to open SDR from fd");
            set_status(|s| s.last_error = Some(format!("SDR open failed: {e}")));
            return;
        }
    };
    set_status(|s| s.sdr_connected = true);
    let device_serials = sdr_manager.device_serials();

    let scanning_config = config.scanning.clone().unwrap_or_default();
    let mut scanner = Scanner::new(&scanning_config, config.sdr.dbm_offset);

    if scanner.needs_discovery() {
        info!("running first-time station discovery sweep");
        let discovery = scanner.discover_stations(&mut sdr_manager);
        info!(
            fm = discovery.fm_stations.len(),
            lte = discovery.lte_cells.len(),
            dtv = discovery.dtv_channels.len(),
            "discovery complete"
        );
    }

    let scan_interval = Duration::from_secs(scanner.interval_seconds() as u64);
    let heartbeat_interval = Duration::from_secs(config.daemon.heartbeat_interval_secs.max(15));
    let retry_buf = pending::PendingBuffer::default();
    let mut last_heartbeat = Instant::now()
        .checked_sub(heartbeat_interval)
        .unwrap_or_else(Instant::now);

    info!(
        interval_secs = scanner.interval_seconds(),
        "scanner loop started (OTG mode)"
    );

    while !STOP.load(Ordering::SeqCst) {
        // Registration retry: keep trying until the node exists server-side.
        if node_id.is_none() {
            node_id = ensure_registered(&rt, &client, &android_config);
        }

        if let Some(id) = &node_id {
            if !id.starts_with("placeholder-") && last_heartbeat.elapsed() >= heartbeat_interval {
                match rt.block_on(client.heartbeat(
                    id,
                    sdr_manager.device_count(),
                    device_serials.clone(),
                    false,
                    false,
                    None,
                )) {
                    Ok(()) => {
                        last_heartbeat = Instant::now();
                        set_status(|s| {
                            s.last_heartbeat_unix = Some(chrono::Utc::now().timestamp());
                        });
                    }
                    Err(e) => warn!(error = %e, "heartbeat failed"),
                }
            }
        }

        let observations = scanner.run_cycle(&mut sdr_manager);
        if !observations.is_empty() {
            set_status(|s| s.signals_found += observations.len() as u64);
        }

        // Pending observations from failed uploads are resent even on a cycle
        // that captured nothing.
        if let Some(id) = node_id.clone() {
            if let Some(sent) = rt.block_on(pending::flush(&retry_buf, &client, &id, observations, Instant::now())) {
                let count = sent.count;
                match sent.result {
                    Ok(outcome) => {
                        let accepted = outcome.accepted;
                        info!(submitted = count, accepted, "batch submitted");
                        set_status(|s| {
                            s.observations_submitted += accepted as u64;
                            s.last_error = None;
                        });
                    }
                    Err(e) => {
                        if api_client::is_unauthorized(&e) {
                            error!("observation submit 401 — API key rejected");
                            set_status(|s| {
                                s.last_error = Some("API key rejected (401)".to_string())
                            });
                        } else if !sent.kept {
                            warn!(error = %e, "observation submit failed — batch dropped");
                        }
                    }
                }
            }
        }

        // Interruptible inter-cycle sleep so stop() takes effect quickly.
        let sleep_until = Instant::now() + scan_interval;
        while Instant::now() < sleep_until && !STOP.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    info!("stop flag observed — closing SDR");
    drop(sdr_manager);
}

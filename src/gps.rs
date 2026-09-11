use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, warn, debug, error};

use crate::config::GpsConfig;
use crate::status_server::StatusState;

/// Represents a serial port found on the system
#[derive(Clone, Debug, Serialize)]
pub struct SerialPortInfo {
    pub port_name: String,
    pub port_type: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial_number: Option<String>,
}

/// Accuracy sample recorded periodically during GPS operation
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccuracyPoint {
    pub timestamp: i64,
    pub hdop: f32,
    pub fix_quality: u8,
    pub lat_error_m: Option<f32>,
    pub lon_error_m: Option<f32>,
}

/// Known GPS vendor IDs
const UBLOX_VID: u16 = 0x1546;
const PROLIFIC_VID: u16 = 0x067B;
const FTDI_VID: u16 = 0x0403;
const SILICON_LABS_VID: u16 = 0x10C4;
const CH340_VID: u16 = 0x1A86;

/// List all available serial ports with details
pub fn list_serial_ports() -> Vec<SerialPortInfo> {
    match serialport::available_ports() {
        Ok(ports) => {
            ports
                .iter()
                .map(|p| {
                    let (vid, pid, manufacturer, product, serial_number, port_type) = match &p.port_type {
                        serialport::SerialPortType::UsbPort(info) => (
                            Some(info.vid),
                            Some(info.pid),
                            info.manufacturer.clone(),
                            info.product.clone(),
                            info.serial_number.clone(),
                            "usb",
                        ),
                        serialport::SerialPortType::PciPort => (None, None, None, None, None, "pci"),
                        serialport::SerialPortType::BluetoothPort => (None, None, None, None, None, "bluetooth"),
                        serialport::SerialPortType::Unknown => (None, None, None, None, None, "unknown"),
                    };
                    SerialPortInfo {
                        port_name: p.port_name.clone(),
                        port_type: port_type.to_string(),
                        vid,
                        pid,
                        manufacturer,
                        product,
                        serial_number,
                    }
                })
                .collect()
        }
        Err(e) => {
            warn!(error = %e, "Failed to enumerate serial ports");
            Vec::new()
        }
    }
}

/// Probe priority for a candidate serial port. Lower is tried first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Explicitly pinned by `[gps] port` in config.
    Configured = 0,
    /// Matches a known GPS vendor id (u-blox).
    KnownGps = 1,
    /// Matches a generic USB-serial bridge commonly used by GPS modules.
    UsbSerialBridge = 2,
    /// No usable vid — probed only because valid NMEA is the real proof.
    Unidentified = 3,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GpsCandidate {
    pub port_name: String,
    pub device_name: String,
    pub tier: Tier,
}

/// Everything `rank_candidates` needs to know, assembled per detection round so
/// it can pick up live RTK state. Kept separate from `GpsConfig` so the ranker
/// stays a pure function of its inputs.
#[derive(Debug, Clone, Default)]
pub struct GpsProbePolicy {
    pub enabled: bool,
    pub forced_port: Option<String>,
    pub probe_unknown_ports: bool,
    pub allow_ports: Vec<String>,
    pub deny_ports: Vec<String>,
    /// Serial port currently claimed by NTRIP/RTK. Windows COM ports are
    /// exclusive, so probing this one is a self-inflicted ACCESS_DENIED.
    pub rtk_serial_port: Option<String>,
    pub retry_base: Duration,
    pub retry_max: Duration,
}

/// Identity of the device behind a port name, so a cache entry can't outlive
/// the device that earned it (COM numbers get reused).
///
/// On Windows an unidentified port reports no vid/pid/serial at all, so this
/// degrades to name-only there. That is accepted: the disappearance check below
/// is what actually protects a later hot-plug.
type PortFingerprint = (Option<u16>, Option<u16>, Option<String>, String);

fn fingerprint(p: &SerialPortInfo) -> PortFingerprint {
    (
        p.vid,
        p.pid,
        p.serial_number.clone(),
        p.port_type.clone(),
    )
}

#[derive(Debug, Clone)]
struct BadPort {
    fingerprint: PortFingerprint,
    failures: u32,
    next_retry: Instant,
}

/// Ports that failed to yield NMEA, with exponential backoff.
///
/// Deliberately NOT a blacklist: entries expire, and any port that disappears
/// from enumeration is dropped outright, so unplugging and replugging a
/// receiver always re-probes immediately. The cap only ever applies to a port
/// that is permanently present and permanently silent (an onboard COM port, an
/// idle Bluetooth port).
#[derive(Debug, Default)]
pub struct ProbeCache {
    entries: HashMap<String, BadPort>,
}

impl ProbeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop entries whose port is no longer enumerated. A returning port is a
    /// fresh probe, not a resumed sentence.
    fn retain_present(&mut self, ports: &[SerialPortInfo]) {
        let present: std::collections::HashSet<&str> =
            ports.iter().map(|p| p.port_name.as_str()).collect();
        self.entries.retain(|name, _| present.contains(name.as_str()));
    }

    fn is_suppressed(&self, p: &SerialPortInfo, now: Instant) -> bool {
        match self.entries.get(&p.port_name) {
            // A different device on the same port name starts clean.
            Some(bad) if bad.fingerprint != fingerprint(p) => false,
            Some(bad) => now < bad.next_retry,
            None => false,
        }
    }

    pub fn record_failure(
        &mut self,
        p: &SerialPortInfo,
        now: Instant,
        base: Duration,
        max: Duration,
    ) {
        let entry = self.entries.entry(p.port_name.clone()).or_insert(BadPort {
            fingerprint: fingerprint(p),
            failures: 0,
            next_retry: now,
        });
        if entry.fingerprint != fingerprint(p) {
            entry.fingerprint = fingerprint(p);
            entry.failures = 0;
        }
        entry.failures = entry.failures.saturating_add(1);
        let shift = entry.failures.saturating_sub(1).min(16);
        let backoff = base
            .checked_mul(1u32 << shift)
            .unwrap_or(max)
            .min(max);
        entry.next_retry = now + backoff;
    }

    pub fn record_success(&mut self, port_name: &str) {
        self.entries.remove(port_name);
    }
}

/// True for ports the OS tells us are Bluetooth. Only Linux/macOS actually
/// report this: the Windows backend classifies every non-USB port as `Unknown`,
/// so on Windows the deny list and the backoff cache are the real protection.
fn is_bluetooth_port(p: &SerialPortInfo) -> bool {
    if p.port_type == "bluetooth" {
        return true;
    }
    let n = p.port_name.to_ascii_lowercase();
    n.contains("rfcomm") || n.contains("bluetooth")
}

/// Rank the serial ports into a probe order. Pure: no enumeration, no serial
/// I/O, no clock — so the whole policy is unit-testable, which the original
/// `detect_gps_port()` was not (it called `list_serial_ports()` internally and
/// took no arguments).
pub fn rank_candidates(
    ports: &[SerialPortInfo],
    policy: &GpsProbePolicy,
    cache: &ProbeCache,
    now: Instant,
) -> Vec<GpsCandidate> {
    if !policy.enabled {
        return Vec::new();
    }

    // An explicitly configured port wins outright and is exempt from every
    // heuristic below — including the cache. An override that quietly probes
    // something else is worse than one that fails loudly.
    if let Some(forced) = policy.forced_port.as_ref().filter(|s| !s.is_empty()) {
        let device_name = ports
            .iter()
            .find(|p| &p.port_name == forced)
            .and_then(|p| p.product.clone())
            .unwrap_or_else(|| "Configured GPS".to_string());
        return vec![GpsCandidate {
            port_name: forced.clone(),
            device_name,
            tier: Tier::Configured,
        }];
    }

    let mut out: Vec<GpsCandidate> = Vec::new();

    for p in ports {
        if !policy.allow_ports.is_empty() && !policy.allow_ports.contains(&p.port_name) {
            continue;
        }
        if policy.deny_ports.contains(&p.port_name) {
            continue;
        }
        if policy.rtk_serial_port.as_deref() == Some(p.port_name.as_str()) {
            continue;
        }
        if is_bluetooth_port(p) {
            continue;
        }

        let tier = if p.vid == Some(UBLOX_VID) {
            Tier::KnownGps
        } else if matches!(
            p.vid,
            Some(PROLIFIC_VID) | Some(FTDI_VID) | Some(SILICON_LABS_VID) | Some(CH340_VID)
        ) {
            Tier::UsbSerialBridge
        } else if p.vid.is_none() {
            Tier::Unidentified
        } else {
            // A USB device with a vid we don't recognise. Treat it like any
            // other unidentified port rather than dropping it outright.
            Tier::Unidentified
        };

        if tier == Tier::Unidentified && !policy.probe_unknown_ports {
            continue;
        }
        // Backoff applies only to the speculative tiers; a u-blox port keeps
        // the original aggressive retry cadence.
        if tier >= Tier::UsbSerialBridge && cache.is_suppressed(p, now) {
            continue;
        }

        let device_name = p.product.clone().unwrap_or_else(|| match tier {
            Tier::KnownGps => "u-blox GPS".to_string(),
            Tier::UsbSerialBridge => "USB Serial (GPS)".to_string(),
            _ => format!("Unknown serial device ({})", p.port_name),
        });

        out.push(GpsCandidate {
            port_name: p.port_name.clone(),
            device_name,
            tier,
        });
    }

    out.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| a.port_name.cmp(&b.port_name)));
    out
}

/// Thin I/O wrapper around `rank_candidates`.
fn detect_gps_candidates(policy: &GpsProbePolicy, cache: &mut ProbeCache) -> Vec<GpsCandidate> {
    let ports = list_serial_ports();
    cache.retain_present(&ports);
    rank_candidates(&ports, policy, cache, Instant::now())
}

/// Check if a line looks like valid NMEA
fn nmea_line_valid(line: &str) -> bool {
    let trimmed = line.trim();
    // NMEA sentences start with $ and contain alphanumeric talker/formatter
    // Examples: $GNGGA..., $GPGGA..., $GLGGA...
    trimmed.starts_with('$') && trimmed.len() > 6 && trimmed.chars().skip(1).take(5).all(|c| c.is_ascii_alphanumeric())
}

/// Try to open a serial port at various baud rates with NMEA validation
/// Returns a buffered serial stream and the detected baud rate if validation succeeds
async fn try_open_serial(
    port: &str,
    bauds: &[u32],
) -> Option<(BufReader<tokio_serial::SerialStream>, u32)> {
    for &baud in bauds {
        match tokio_serial::new(port, baud).open_native_async() {
            Ok(stream) => {
                debug!(port = %port, baud, "Serial port opened, validating NMEA");

                let mut buf_reader = BufReader::new(stream);
                let mut validation_attempts = 0;
                let max_validation_attempts = 2;

                while validation_attempts < max_validation_attempts {
                    validation_attempts += 1;
                    let mut line = String::new();

                    let read_result = tokio::time::timeout(
                        tokio::time::Duration::from_secs(2),
                        buf_reader.read_line(&mut line)
                    ).await;

                    match read_result {
                        Ok(Ok(0)) => {
                            // EOF — port is open but not producing data
                            debug!(port = %port, baud, "Serial port reached EOF");
                            break;
                        }
                        Ok(Ok(_)) => {
                            if nmea_line_valid(&line) {
                                info!(port = %port, baud, "NMEA validated on serial port");
                                return Some((buf_reader, baud));
                            }
                        }
                        Ok(Err(_)) => {
                            debug!(port = %port, baud, "Serial read error during validation");
                            break;
                        }
                        Err(_) => {
                            // Timeout — no data received
                            debug!(port = %port, baud, attempt = validation_attempts, "NMEA validation timeout");
                        }
                    }
                }

                debug!(port = %port, baud, "NMEA validation failed on this baud rate");
                // Drop this stream and try next baud
            }
            Err(e) => {
                if e.kind == tokio_serial::ErrorKind::Io(std::io::ErrorKind::PermissionDenied) {
                    warn_permission_denied(port);
                    // Permission does not depend on baud rate — no point
                    // retrying the remaining rates on this port.
                    return None;
                }
                debug!(port = %port, baud, error = %e, "Serial port open failed");
                continue;
            }
        }
    }
    None
}

/// Log the dialout-group fix instructions once per port at error level;
/// the 5-second detection loop would otherwise repeat it endlessly.
fn warn_permission_denied(port: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
    let first = warned
        .lock()
        .map(|mut set| set.insert(port.to_string()))
        .unwrap_or(true);
    if first {
        error!(port = %port, "{}", permission_denied_hint(port));
    } else {
        debug!(port = %port, "GPS serial port still permission-denied");
    }
}

/// Platform-appropriate advice for a permission-denied serial open.
///
/// Extracted so it is testable, and split per-OS because the previous single
/// message told Windows users to run `sudo usermod -aG dialout` — advice that
/// cannot apply and sends them down the wrong path.
fn permission_denied_hint(port: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        format!(
            "Access denied opening GPS serial port '{port}'. Windows serial ports are exclusive — \
             exactly one process may hold a handle. Close u-center, PuTTY or any terminal on this \
             port; check Task Manager for a second azimuth-daemon or an Azimuth Windows service; \
             and confirm this is not the port configured for RTK/NTRIP serial forwarding."
        )
    }
    #[cfg(target_os = "linux")]
    {
        format!(
            "Permission denied opening GPS serial port '{port}'.\n  \
             On Linux, your user must be in the 'dialout' group:\n    \
             sudo usermod -aG dialout $USER\n  \
             Then log out and back in (or reboot).\n  \
             If running in Docker, ensure the device is passed through:\n    \
             devices: [\"/dev/ttyUSB0:/dev/ttyUSB0\"]\n  \
             and the container has sufficient privileges."
        )
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        format!(
            "Permission denied opening GPS serial port '{port}'. Another application is most \
             likely holding the port open — close any other GPS or serial terminal software and \
             confirm this is not the port configured for RTK/NTRIP serial forwarding."
        )
    }
}

/// Extract latitude/longitude/altitude from a GGA sentence
fn parse_gga(sentence: &str) -> Option<(f64, f64, f64, u8, u8)> {
    // Format: $GPGGA,HHMMSS.SS,DDMM.MMMMM,N/S,DDDMM.MMMMM,E/W,1,12,1.0,ALT,M,0.0,M,,*HH
    let parts: Vec<&str> = sentence.split(',').collect();
    if parts.len() < 10 {
        return None;
    }

    // Field 0: GPGGA
    // Field 1: Time
    // Field 2: Latitude
    // Field 3: N/S
    // Field 4: Longitude
    // Field 5: E/W
    // Field 6: Fix quality (0-8)
    // Field 7: Number of satellites
    // Field 8: HDOP
    // Field 9: Altitude

    // Parse fix quality
    let fix_quality: u8 = parts[6].parse().unwrap_or(0);
    let satellites: u8 = parts[7].parse().unwrap_or(0);

    // Only process if we have a fix
    if fix_quality == 0 {
        return None;
    }

    // Parse latitude
    let lat_str = parts[2];
    if lat_str.len() < 5 {
        return None;
    }
    let lat_deg: f64 = lat_str[..2].parse().ok()?;
    let lat_min: f64 = lat_str[2..].parse().ok()?;
    let mut lat = lat_deg + lat_min / 60.0;
    if parts[3].trim() == "S" {
        lat = -lat;
    }

    // Parse longitude
    let lon_str = parts[4];
    if lon_str.len() < 6 {
        return None;
    }
    let lon_deg: f64 = lon_str[..3].parse().ok()?;
    let lon_min: f64 = lon_str[3..].parse().ok()?;
    let mut lon = lon_deg + lon_min / 60.0;
    if parts[5].trim() == "W" {
        lon = -lon;
    }

    // Parse altitude
    let alt: f64 = parts[9].parse().unwrap_or(0.0);

    Some((lat, lon, alt, fix_quality, satellites))
}

/// Run the GPS reader task
/// - Detects GPS on available serial ports every 5s
/// - Parses NMEA GGA sentences and updates StatusState
/// - Maintains accuracy history (up to 180 samples at 10s interval)
/// - Resilient to transient timeouts: allows 3 consecutive timeouts before reconnecting
pub async fn run_gps_task(
    state: Arc<RwLock<StatusState>>,
    _rtcm_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    gps_config: GpsConfig,
) {
    const DETECTION_INTERVAL_SECS: u64 = 5;
    const ACCURACY_HISTORY_MAX: usize = 180;
    const MAX_CONSECUTIVE_TIMEOUTS: u32 = 3;
    /// Wall-clock ceiling for speculative probing in one detection round, so a
    /// machine with many unidentified COM ports doesn't stall the loop.
    const TIER3_PROBE_BUDGET: Duration = Duration::from_secs(20);

    let sample_interval_secs = gps_config.sample_interval_secs.max(1);

    if !gps_config.enabled {
        info!("GPS disabled by config ([gps] enabled = false); GPS task parked");
        return;
    }

    // A configured port skips auto-detection entirely, so it only needs the one
    // baud (plus fallbacks). Unidentified ports get 9600 only on first contact —
    // the VK-162 and u-blox modules default to it — and are promoted to the full
    // sweep only once they have proven they emit bytes.
    let full_bauds: Vec<u32> = match gps_config.baud_rate {
        Some(b) => {
            let mut v = vec![b, 9600, 115200, 4800];
            v.dedup();
            v
        }
        None => vec![9600, 115200, 4800],
    };
    let first_contact_bauds = &[9600u32];

    let mut cache = ProbeCache::new();
    let mut last_sample_time = tokio::time::Instant::now(); // accuracy sampling interval

    loop {
        debug!("GPS detection scan starting");

        let rtk_serial_port = {
            let s = state.read().await;
            s.rtk_config.as_ref().and_then(|c| c.serial_port.clone())
        };
        let policy = GpsProbePolicy {
            enabled: gps_config.enabled,
            forced_port: gps_config.port.clone(),
            probe_unknown_ports: gps_config.probe_unknown_ports,
            allow_ports: gps_config.allow_ports.clone(),
            deny_ports: gps_config.deny_ports.clone(),
            rtk_serial_port,
            retry_base: Duration::from_secs(gps_config.probe_retry_base_secs.max(1)),
            retry_max: Duration::from_secs(gps_config.probe_retry_max_secs.max(1)),
        };

        let candidates = detect_gps_candidates(&policy, &mut cache);
        if candidates.is_empty() {
            debug!("No GPS candidates, retrying in {}s", DETECTION_INTERVAL_SECS);
            tokio::time::sleep(tokio::time::Duration::from_secs(DETECTION_INTERVAL_SECS)).await;
            continue;
        }

        // Probe candidates in tier order until one yields valid NMEA. The old
        // code returned only the first VID match and retried that same port
        // forever; a port that merely LOOKS like a GPS could therefore mask a
        // real receiver sitting behind it.
        let budget_start = Instant::now();
        let mut opened: Option<(BufReader<tokio_serial::SerialStream>, u32, GpsCandidate)> = None;
        for cand in &candidates {
            if cand.tier >= Tier::Unidentified && budget_start.elapsed() > TIER3_PROBE_BUDGET {
                debug!("Tier-3 probe budget spent for this round; resuming next scan");
                break;
            }
            let bauds: &[u32] = if cand.tier >= Tier::Unidentified {
                first_contact_bauds
            } else {
                &full_bauds
            };
            debug!(port = %cand.port_name, tier = ?cand.tier, "Probing serial port for NMEA");
            if let Some((reader, baud)) = try_open_serial(&cand.port_name, bauds).await {
                opened = Some((reader, baud, cand.clone()));
                break;
            }
            // Only speculative tiers earn a backoff entry.
            if cand.tier >= Tier::UsbSerialBridge {
                if let Some(info) = list_serial_ports()
                    .into_iter()
                    .find(|p| p.port_name == cand.port_name)
                {
                    cache.record_failure(&info, Instant::now(), policy.retry_base, policy.retry_max);
                }
            }
        }

        let (port_name, device_name) = match &opened {
            Some((_, _, c)) => (c.port_name.clone(), c.device_name.clone()),
            None => {
                debug!("No GPS validated this round, retrying in {}s", DETECTION_INTERVAL_SECS);
                tokio::time::sleep(tokio::time::Duration::from_secs(DETECTION_INTERVAL_SECS)).await;
                continue;
            }
        };

        // Only claim detection once NMEA actually validated. The previous code
        // set gps_detected BEFORE try_open_serial, so the UI flashed "detected"
        // for a port that then failed — and with speculative probing enabled it
        // would have flashed for every COM port on the machine.
        info!(port = %port_name, device = %device_name, "GPS detected");
        cache.record_success(&port_name);
        {
            let mut s = state.write().await;
            s.gps_detected = true;
            s.gps_port = Some(port_name.clone());
            s.gps_device_name = Some(device_name.clone());
        }

        match opened.map(|(r, b, _)| (r, b)) {
            Some((mut buf_reader, detected_baud)) => {
                info!(port = %port_name, baud = detected_baud, "GPS connection established");

                // Read NMEA sentences
                let mut line = String::new();
                let mut consecutive_timeouts: u32 = 0;

                loop {
                    // Check if GPS task should stop (detection re-triggered)
                    {
                        let s = state.read().await;
                        if !s.gps_detected {
                            break;
                        }
                    }

                    line.clear();
                    // Bound read with timeout to detect silent/disconnected ports
                    let read_result = tokio::time::timeout(
                        tokio::time::Duration::from_secs(10),
                        buf_reader.read_line(&mut line)
                    ).await;

                    match read_result {
                        Ok(Ok(0)) => {
                            // EOF
                            info!("GPS connection closed (EOF)");
                            break;
                        }
                        Ok(Ok(_)) => {
                            consecutive_timeouts = 0; // Reset timeout counter on successful read

                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }

                            // Try to parse GGA sentence
                            if trimmed.starts_with("$") && trimmed.contains("GGA") {
                                if let Some((lat, lon, alt, fix_quality, satellites)) = parse_gga(trimmed) {
                                    let mut s = state.write().await;

                                    // Update state from GGA
                                    s.gps_fix_quality = fix_quality;
                                    s.gps_satellites = satellites;
                                    s.gps_latitude = Some(lat);
                                    s.gps_longitude = Some(lon);
                                    s.gps_altitude_m = Some(alt);
                                    s.gps_last_fix = Some(chrono::Utc::now().to_rfc3339());

                                    // Sample accuracy history every 10s
                                    if last_sample_time.elapsed() >= tokio::time::Duration::from_secs(sample_interval_secs) {
                                        last_sample_time = tokio::time::Instant::now();

                                        let point = AccuracyPoint {
                                            timestamp: chrono::Utc::now().timestamp(),
                                            hdop: s.gps_hdop.unwrap_or(999.9),
                                            fix_quality,
                                            lat_error_m: s.gps_lat_error_m,
                                            lon_error_m: s.gps_lon_error_m,
                                        };

                                        s.gps_accuracy_history.push_back(point);
                                        if s.gps_accuracy_history.len() > ACCURACY_HISTORY_MAX {
                                            s.gps_accuracy_history.pop_front();
                                        }
                                    }

                                    debug!(
                                        fix_quality,
                                        satellites,
                                        lat,
                                        lon,
                                        alt,
                                        "GPS GGA parsed"
                                    );
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "GPS read error");
                            break;
                        }
                        Err(_timeout) => {
                            consecutive_timeouts += 1;

                            if consecutive_timeouts < MAX_CONSECUTIVE_TIMEOUTS {
                                warn!(
                                    port = %port_name,
                                    attempt = consecutive_timeouts,
                                    "GPS read timeout (no data for 10s) — {} attempt(s) remaining before reconnect",
                                    MAX_CONSECUTIVE_TIMEOUTS - consecutive_timeouts
                                );
                            } else {
                                warn!(
                                    port = %port_name,
                                    "GPS read timeout: {} consecutive failures — reconnecting",
                                    MAX_CONSECUTIVE_TIMEOUTS
                                );
                                break;
                            }
                        }
                    }
                }

                // Clear GPS state
                {
                    let mut s = state.write().await;
                    s.gps_detected = false;
                    s.gps_port = None;
                    s.gps_device_name = None;
                    s.gps_fix_quality = 0;
                    s.gps_satellites = 0;
                    s.gps_hdop = None;
                    s.gps_latitude = None;
                    s.gps_longitude = None;
                    s.gps_altitude_m = None;
                    s.gps_speed_knots = None;
                    s.gps_lat_error_m = None;
                    s.gps_lon_error_m = None;
                    s.gps_last_fix = None;
                }

                info!("GPS disconnected, restarting detection");
            }
            None => {
                warn!(port = %port_name, "Failed to open GPS serial port or validate NMEA");
                tokio::time::sleep(tokio::time::Duration::from_secs(DETECTION_INTERVAL_SECS)).await;
                continue;
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(DETECTION_INTERVAL_SECS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // VK-172-class u-blox receivers emit combined-constellation "$GN" talkers.
    const GNGGA: &str = "$GNGGA,173000.00,4139.7541,N,09121.3906,W,1,08,1.2,215.3,M,-33.9,M,,*40";
    const GPGGA: &str = "$GPGGA,173000.00,4139.7541,N,09121.3906,W,1,08,1.2,215.3,M,-33.9,M,,*4E";

    #[test]
    fn parse_gga_accepts_gn_talker() {
        let (lat, lon, _alt, fix, sats) = parse_gga(GNGGA).expect("$GNGGA must parse");
        assert!((lat - 41.662568).abs() < 0.001);
        assert!((lon + 91.356510).abs() < 0.001);
        assert_eq!(fix, 1);
        assert_eq!(sats, 8);
    }

    #[test]
    fn parse_gga_accepts_gp_talker() {
        let parsed = parse_gga(GPGGA).expect("$GPGGA must parse");
        assert_eq!(parsed.3, 1);
    }

    #[test]
    fn parse_gga_rejects_garbage() {
        assert!(parse_gga("$GNGGA,badfield").is_none());
        assert!(parse_gga("not nmea at all").is_none());
        assert!(parse_gga("").is_none());
    }

    #[test]
    fn nmea_line_valid_accepts_gn_talker() {
        assert!(nmea_line_valid("$GNGGA,173000.00,4139.7541,N,09121.3906,W,1,08,1.2,215.3,M,-33.9,M,,*40"));
    }

    #[test]
    fn nmea_line_valid_accepts_gp_talker() {
        assert!(nmea_line_valid("$GPGGA,173000.00,4139.7541,N,09121.3906,W,1,08,1.2,215.3,M,-33.9,M,,*4E"));
    }

    #[test]
    fn nmea_line_valid_accepts_gl_talker() {
        assert!(nmea_line_valid("$GLGGA,data"));
    }

    #[test]
    fn nmea_line_valid_rejects_non_nmea() {
        assert!(!nmea_line_valid("not nmea at all"));
        assert!(!nmea_line_valid(""));
        assert!(!nmea_line_valid("$"));
        assert!(!nmea_line_valid("$AB"));
    }

    #[test]
    fn nmea_line_valid_accepts_with_trailing_whitespace() {
        // BufReader::read_line includes trailing \n but not leading whitespace from stream
        assert!(nmea_line_valid("$GNGGA,data\n"));
    }

    // --- detection / ranking ---------------------------------------------
    //
    // These are the regression coverage for BUG-4. The old detect_gps_port()
    // took no arguments and called list_serial_ports() internally, so the
    // selection policy could not be tested at all — and the policy was the bug:
    // a port with no usable vid (which is how Windows reports anything it can't
    // parse as USB) was dropped BEFORE the NMEA validator ever saw it, making a
    // working receiver permanently invisible.

    fn port(name: &str, vid: Option<u16>, kind: &str) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: kind.to_string(),
            vid,
            pid: None,
            manufacturer: None,
            product: None,
            serial_number: None,
        }
    }

    fn policy() -> GpsProbePolicy {
        GpsProbePolicy {
            enabled: true,
            forced_port: None,
            probe_unknown_ports: true,
            allow_ports: Vec::new(),
            deny_ports: Vec::new(),
            rtk_serial_port: None,
            retry_base: Duration::from_secs(60),
            retry_max: Duration::from_secs(1800),
        }
    }

    #[test]
    fn unidentified_port_is_probed_not_dropped() {
        // THE BUG-4 REGRESSION TEST. Previously this yielded zero candidates.
        let ports = vec![port("COM7", None, "unknown")];
        let got = rank_candidates(&ports, &policy(), &ProbeCache::new(), Instant::now());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].port_name, "COM7");
        assert_eq!(got[0].tier, Tier::Unidentified);
    }

    #[test]
    fn ublox_vid_ranks_ahead_of_everything_else() {
        let ports = vec![
            port("COM3", None, "unknown"),
            port("COM7", Some(UBLOX_VID), "usb"),
            port("COM4", Some(FTDI_VID), "usb"),
        ];
        let got = rank_candidates(&ports, &policy(), &ProbeCache::new(), Instant::now());
        assert_eq!(got[0].port_name, "COM7");
        assert_eq!(got[0].tier, Tier::KnownGps);
    }

    #[test]
    fn known_bridge_ranks_above_unidentified() {
        let ports = vec![
            port("COM3", None, "unknown"),
            port("COM4", Some(CH340_VID), "usb"),
        ];
        let got = rank_candidates(&ports, &policy(), &ProbeCache::new(), Instant::now());
        assert_eq!(got[0].port_name, "COM4");
        assert_eq!(got[1].port_name, "COM3");
    }

    #[test]
    fn bluetooth_typed_port_is_never_probed() {
        let ports = vec![port("COM9", None, "bluetooth")];
        let got = rank_candidates(&ports, &policy(), &ProbeCache::new(), Instant::now());
        assert!(got.is_empty());
    }

    #[test]
    fn rfcomm_name_is_skipped_even_when_type_is_unknown() {
        let ports = vec![port("/dev/rfcomm0", None, "unknown")];
        let got = rank_candidates(&ports, &policy(), &ProbeCache::new(), Instant::now());
        assert!(got.is_empty());
    }

    #[test]
    fn deny_list_wins_over_ublox_vid() {
        let mut p = policy();
        p.deny_ports = vec!["COM7".to_string()];
        let ports = vec![port("COM7", Some(UBLOX_VID), "usb")];
        let got = rank_candidates(&ports, &p, &ProbeCache::new(), Instant::now());
        assert!(got.is_empty());
    }

    #[test]
    fn allow_list_restricts_every_tier() {
        let mut p = policy();
        p.allow_ports = vec!["COM4".to_string()];
        let ports = vec![
            port("COM7", Some(UBLOX_VID), "usb"),
            port("COM4", None, "unknown"),
        ];
        let got = rank_candidates(&ports, &p, &ProbeCache::new(), Instant::now());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].port_name, "COM4");
    }

    #[test]
    fn rtk_serial_port_is_skipped_to_avoid_self_inflicted_access_denied() {
        let mut p = policy();
        p.rtk_serial_port = Some("COM5".to_string());
        let ports = vec![port("COM5", Some(UBLOX_VID), "usb")];
        let got = rank_candidates(&ports, &p, &ProbeCache::new(), Instant::now());
        assert!(got.is_empty());
    }

    #[test]
    fn configured_port_is_the_only_candidate() {
        let mut p = policy();
        p.forced_port = Some("COM12".to_string());
        let ports = vec![
            port("COM7", Some(UBLOX_VID), "usb"),
            port("COM12", None, "unknown"),
        ];
        let got = rank_candidates(&ports, &p, &ProbeCache::new(), Instant::now());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].port_name, "COM12");
        assert_eq!(got[0].tier, Tier::Configured);
    }

    #[test]
    fn probe_unknown_ports_false_restores_whitelist_only_behavior() {
        let mut p = policy();
        p.probe_unknown_ports = false;
        let ports = vec![port("COM7", None, "unknown")];
        let got = rank_candidates(&ports, &p, &ProbeCache::new(), Instant::now());
        assert!(got.is_empty());
    }

    #[test]
    fn disabled_policy_yields_no_candidates() {
        let mut p = policy();
        p.enabled = false;
        let ports = vec![port("COM7", Some(UBLOX_VID), "usb")];
        let got = rank_candidates(&ports, &p, &ProbeCache::new(), Instant::now());
        assert!(got.is_empty());
    }

    #[test]
    fn cache_suppresses_reprobe_until_backoff_expires() {
        let now = Instant::now();
        let p = policy();
        let info = port("COM3", None, "unknown");
        let mut cache = ProbeCache::new();
        cache.record_failure(&info, now, Duration::from_secs(60), Duration::from_secs(1800));

        assert!(rank_candidates(&[info.clone()], &p, &cache, now).is_empty());
        let later = now + Duration::from_secs(61);
        assert_eq!(rank_candidates(&[info], &p, &cache, later).len(), 1);
    }

    #[test]
    fn cache_backoff_grows_then_caps() {
        let now = Instant::now();
        let info = port("COM3", None, "unknown");
        let mut cache = ProbeCache::new();
        let base = Duration::from_secs(60);
        let max = Duration::from_secs(1800);
        for _ in 0..12 {
            cache.record_failure(&info, now, base, max);
        }
        let entry = cache.entries.get("COM3").expect("entry");
        assert!(entry.next_retry <= now + max);
    }

    #[test]
    fn cache_never_suppresses_a_ublox_port() {
        // Tier 1 keeps the original aggressive 5s retry cadence.
        let now = Instant::now();
        let info = port("COM7", Some(UBLOX_VID), "usb");
        let mut cache = ProbeCache::new();
        cache.record_failure(&info, now, Duration::from_secs(60), Duration::from_secs(1800));
        assert_eq!(rank_candidates(&[info], &policy(), &cache, now).len(), 1);
    }

    #[test]
    fn cache_entry_dropped_when_port_disappears_so_replug_probes_immediately() {
        let now = Instant::now();
        let info = port("COM3", None, "unknown");
        let mut cache = ProbeCache::new();
        cache.record_failure(&info, now, Duration::from_secs(60), Duration::from_secs(1800));
        // Port vanishes (unplugged), then comes back.
        cache.retain_present(&[]);
        assert_eq!(rank_candidates(&[info], &policy(), &cache, now).len(), 1);
    }

    #[test]
    fn cache_entry_ignored_when_device_fingerprint_changes() {
        let now = Instant::now();
        let old = port("COM3", None, "unknown");
        let mut cache = ProbeCache::new();
        cache.record_failure(&old, now, Duration::from_secs(60), Duration::from_secs(1800));
        // Same COM name, different device now behind it.
        let new = port("COM3", Some(UBLOX_VID), "usb");
        assert_eq!(rank_candidates(&[new], &policy(), &cache, now).len(), 1);
    }

    #[test]
    fn permission_denied_hint_is_platform_appropriate() {
        let hint = permission_denied_hint("COM7");
        assert!(hint.contains("COM7"));
        #[cfg(target_os = "windows")]
        {
            assert!(hint.contains("Access denied"));
            // The old single message sent Windows users to a Linux group.
            assert!(!hint.contains("dialout"));
        }
        #[cfg(target_os = "linux")]
        assert!(hint.contains("dialout"));
    }

    #[test]
    fn gps_config_default_is_enabled_and_probes_unknown_ports() {
        // Guards the derived-Default trap: #[derive(Default)] ignores serde
        // defaults and would yield enabled: false, disabling GPS fleet-wide.
        let c = GpsConfig::default();
        assert!(c.enabled);
        assert!(c.probe_unknown_ports);
    }

    #[test]
    fn gps_config_absent_section_deserializes_to_enabled() {
        let cfg: GpsConfig = toml::from_str("").expect("empty [gps] table");
        assert!(cfg.enabled);
    }
}

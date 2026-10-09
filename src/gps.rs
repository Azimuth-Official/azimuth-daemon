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
    /// A USB device with a vendor id we don't recognise (and not a phone or
    /// modem). Tried only after the ports that reported no vid at all.
    UnknownUsb = 4,
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

        // Phones and cellular modems expose AT/diagnostic ports that are never
        // a GPS; skip them (they stay selectable by hand).
        if is_modem_port(p) {
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
            // A USB device with a vid we don't recognise. Probed, but only after
            // the ports that reported no vid at all (a VK-162 on a generic
            // driver shows up as such a plain COM port).
            Tier::UnknownUsb
        };

        if tier >= Tier::Unidentified && !policy.probe_unknown_ports {
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

    out.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| natural_cmp(&a.port_name, &b.port_name)));
    out
}

/// Check if a line looks like valid NMEA
fn nmea_line_valid(line: &str) -> bool {
    let trimmed = line.trim();
    // NMEA sentences start with $ and contain alphanumeric talker/formatter
    // Examples: $GNGGA..., $GPGGA..., $GLGGA...
    trimmed.starts_with('$') && trimmed.len() > 6 && trimmed.chars().skip(1).take(5).all(|c| c.is_ascii_alphanumeric())
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

/// Extract latitude/longitude/altitude/HDOP from a GGA sentence
fn parse_gga(sentence: &str) -> Option<(f64, f64, f64, u8, u8, Option<f32>)> {
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

    // Parse HDOP. Some receivers/fixes leave this field blank, so a parse
    // failure is a legitimate "unknown" rather than a hard error.
    let hdop: Option<f32> = parts[8].trim().parse().ok();

    Some((lat, lon, alt, fix_quality, satellites, hdop))
}

/// Extract ground speed (knots) from a VTG sentence.
///
/// Format: $GPVTG,track_true,T,track_mag,M,speed_knots,N,speed_kmh,K,mode*HH
/// Field 5 is speed in knots; only that field feeds `gps_speed_knots`, which
/// only GGA parsing was ever wired to (and GGA carries no speed field).
fn parse_vtg(sentence: &str) -> Option<f32> {
    let parts: Vec<&str> = sentence.split(',').collect();
    if parts.len() < 6 {
        return None;
    }
    parts[5].trim().parse().ok()
}

// ---------------------------------------------------------------------------
// M2: natural port ordering, modem exclusion, baud promotion, precise errors
// ---------------------------------------------------------------------------

/// Natural ordering for port names: `COM5` sorts before `COM11` (a plain string
/// compare put COM11..COM14 ahead of COM5 and spent the probe budget on them).
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, buf)) if *d == digit => buf.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(cb.iter()) {
        let ord = if x.0 && y.0 {
            let (nx, ny) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
            nx.len().cmp(&ny.len()).then_with(|| nx.cmp(ny))
        } else {
            x.1.to_ascii_lowercase().cmp(&y.1.to_ascii_lowercase())
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    ca.len().cmp(&cb.len())
}

/// The enumerated port that `requested` names, or None. Only ports the OS
/// lists can be opened by `/gps/preview`; Windows names are matched without case
/// and without a `\\.\` device prefix.
pub fn canonical_listed_port(requested: &str, ports: &[SerialPortInfo], case_insensitive: bool) -> Option<String> {
    let norm = |s: &str| {
        let s = s.trim().trim_start_matches("\\\\.\\");
        if case_insensitive { s.to_ascii_lowercase() } else { s.to_string() }
    };
    let want = norm(requested);
    ports.iter().find(|p| norm(&p.port_name) == want).map(|p| p.port_name.clone())
}

/// Phones and cellular modems expose AT/diagnostic serial ports that can never
/// be a GPS receiver. They are not auto-probed (still selectable by hand).
pub fn is_modem_port(p: &SerialPortInfo) -> bool {
    let text = format!(
        "{} {}",
        p.product.as_deref().unwrap_or(""),
        p.manufacturer.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();
    text.contains("modem") || text.contains("mobile usb") || text.contains("diagnostic")
}

/// Result of one baud attempt on one port.
pub enum Attempt<T> {
    /// Valid NMEA arrived.
    Nmea(T),
    /// Bytes arrived but no valid NMEA line: a receiver at another baud rate
    /// (or something that is not a GPS at all).
    Bytes,
    /// Nothing arrived.
    Silent,
    /// The OS refused to open the port: another program holds it.
    Denied,
    Failed(String),
}

pub enum BaudSearch<T> {
    Found(T, u32),
    NotFound { saw_bytes: bool, denied: bool, error: Option<String> },
}

/// Try `bauds` in order until one yields NMEA. A denied open ends the search at
/// once (permission does not depend on baud rate).
pub async fn find_baud<T, F, Fut>(bauds: &[u32], mut attempt: F) -> BaudSearch<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Attempt<T>>,
{
    let (mut saw_bytes, mut error) = (false, None);
    for &baud in bauds {
        match attempt(baud).await {
            Attempt::Nmea(t) => return BaudSearch::Found(t, baud),
            Attempt::Bytes => saw_bytes = true,
            Attempt::Silent => {}
            Attempt::Denied => return BaudSearch::NotFound { saw_bytes, denied: true, error: None },
            Attempt::Failed(e) => error = Some(e),
        }
    }
    BaudSearch::NotFound { saw_bytes, denied: false, error }
}

/// What the daemon has learned about a port in this session.
#[derive(Debug, Default, Clone)]
pub struct PortContact {
    /// First contact (9600 only) has happened; the next probe sweeps every baud.
    pub escalated: bool,
}

/// Probe one port. Speculative (unidentified) ports get 9600 on first contact,
/// then the REST of the sweep as soon as they show bytes, or on the next round
/// after a silent first contact. Known receivers sweep every baud at once.
pub async fn probe_with<T, F, Fut>(
    speculative: bool,
    contact: &mut PortContact,
    full_bauds: &[u32],
    mut attempt: F,
) -> BaudSearch<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Attempt<T>>,
{
    if !speculative || contact.escalated {
        return find_baud(full_bauds, &mut attempt).await;
    }
    let first = find_baud(&[9600], &mut attempt).await;
    contact.escalated = true;
    match first {
        BaudSearch::NotFound { saw_bytes: true, denied: false, .. } => {
            let rest: Vec<u32> = full_bauds.iter().copied().filter(|b| *b != 9600).collect();
            match find_baud(&rest, &mut attempt).await {
                BaudSearch::NotFound { error, denied, .. } => {
                    BaudSearch::NotFound { saw_bytes: true, denied, error }
                }
                found => found,
            }
        }
        other => other,
    }
}

/// Short, user-facing explanation of an access-denied open. The log has the
/// long form (`permission_denied_hint`); this is what `/status` carries.
pub fn access_denied_hint(port: &str) -> String {
    access_denied_hint_for(port, cfg!(windows))
}

pub fn access_denied_hint_for(port: &str, windows: bool) -> String {
    if windows {
        format!(
            "{port} is held by another program. Close u-center, PuTTY or any serial terminal that \
             has it open, and any other azimuth-daemon. If Windows installed \"Microsoft Serial \
             BallPoint\" (or a vendor GNSS sensor driver) for this receiver, disable that device in \
             Device Manager so the port is free."
        )
    } else {
        format!(
            "Permission denied on {port}. Add your user to the 'dialout' group and sign in again, or \
             close any program that has the port open."
        )
    }
}

/// Plain-language reason for a failed probe, for `/status.gps` and the
/// candidate list. `(last_result, error_kind)`.
pub fn describe_failure(saw_bytes: bool, denied: bool, error: Option<&str>) -> (String, Option<&'static str>) {
    if denied {
        ("access_denied".to_string(), Some("access_denied"))
    } else if saw_bytes {
        ("wrong_baud".to_string(), Some("wrong_baud"))
    } else if let Some(e) = error {
        (format!("open_failed: {e}"), Some("no_data"))
    } else {
        ("no_data".to_string(), Some("no_data"))
    }
}

/// One entry of `/status.gps.candidates`.
#[derive(Debug, Clone, Serialize)]
pub struct GpsCandidateInfo {
    pub port_name: String,
    pub port_type: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub product: Option<String>,
    pub probed: bool,
    pub last_result: String,
}

/// `/status.gps`: what the GPS task is doing and why.
#[derive(Debug, Clone, Serialize)]
pub struct GpsInfo {
    /// "absent" | "searching" | "fix" | "error"
    pub state: String,
    pub port: Option<String>,
    pub baud: Option<u32>,
    /// null | "access_denied" | "no_data" | "wrong_baud"
    pub error_kind: Option<String>,
    pub error_hint: Option<String>,
    pub candidates: Vec<GpsCandidateInfo>,
}

impl Default for GpsInfo {
    fn default() -> Self {
        Self {
            state: "absent".to_string(),
            port: None,
            baud: None,
            error_kind: None,
            error_hint: None,
            candidates: Vec::new(),
        }
    }
}

/// Build the candidate list shown to the user: every port with its type, ids,
/// whether it is probed automatically, and the latest probe result.
pub fn ui_candidates(
    ports: &[SerialPortInfo],
    probed: &[GpsCandidate],
    results: &HashMap<String, String>,
) -> Vec<GpsCandidateInfo> {
    let mut v: Vec<GpsCandidateInfo> = ports
        .iter()
        .map(|p| {
            let auto = probed.iter().any(|c| c.port_name == p.port_name);
            let last_result = results.get(&p.port_name).cloned().unwrap_or_else(|| {
                if auto {
                    "pending".to_string()
                } else if is_modem_port(p) {
                    "skipped: phone or modem port".to_string()
                } else {
                    "not probed".to_string()
                }
            });
            GpsCandidateInfo {
                port_name: p.port_name.clone(),
                port_type: p.port_type.clone(),
                vid: p.vid,
                pid: p.pid,
                product: p.product.clone(),
                probed: auto,
                last_result,
            }
        })
        .collect();
    v.sort_by(|a, b| natural_cmp(&a.port_name, &b.port_name));
    v
}

/// Read from an opened port for up to `window`, classifying what arrives.
async fn read_window(
    reader: &mut BufReader<tokio_serial::SerialStream>,
    window: Duration,
    keep: &mut Vec<String>,
) -> Attempt<()> {
    use tokio::io::AsyncReadExt;
    let deadline = tokio::time::Instant::now() + window;
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, reader.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                data.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&data);
                if text.lines().any(nmea_line_valid) && text.contains('\n') {
                    for l in text.lines().filter(|l| nmea_line_valid(l)).take(20) {
                        keep.push(l.trim().to_string());
                    }
                    return Attempt::Nmea(());
                }
                if data.len() > 4096 {
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    if data.is_empty() {
        Attempt::Silent
    } else {
        Attempt::Bytes
    }
}

/// One real attempt at one baud: open, read for `window`, classify. On NMEA the
/// still-open reader is returned.
async fn attempt_port(
    port: &str,
    baud: u32,
    window: Duration,
) -> Attempt<BufReader<tokio_serial::SerialStream>> {
    match tokio_serial::new(port, baud).open_native_async() {
        Ok(stream) => {
            debug!(port = %port, baud, "Serial port opened, validating NMEA");
            let mut reader = BufReader::new(stream);
            let mut lines = Vec::new();
            match read_window(&mut reader, window, &mut lines).await {
                Attempt::Nmea(()) => {
                    info!(port = %port, baud, "NMEA validated on serial port");
                    Attempt::Nmea(reader)
                }
                Attempt::Bytes => Attempt::Bytes,
                Attempt::Silent => Attempt::Silent,
                Attempt::Denied => Attempt::Denied,
                Attempt::Failed(e) => Attempt::Failed(e),
            }
        }
        Err(e) => {
            if e.kind == tokio_serial::ErrorKind::Io(std::io::ErrorKind::PermissionDenied) {
                warn_permission_denied(port);
                Attempt::Denied
            } else {
                debug!(port = %port, baud, error = %e, "Serial port open failed");
                Attempt::Failed(e.to_string())
            }
        }
    }
}

/// Outcome of `preview_port`, returned by `POST /gps/preview`.
#[derive(Debug, Clone, Serialize)]
pub struct PreviewResult {
    pub lines: Vec<String>,
    /// "nmea" | "wrong_baud" | "no_data" | "access_denied" | "open_failed"
    pub verdict: String,
    pub baud: Option<u32>,
    pub error_kind: Option<String>,
    pub error_hint: Option<String>,
}

/// Open `port` briefly, show what it sends and close it again. `baud` None
/// means auto (sweep). Never keeps the port open afterwards.
pub async fn preview_port(port: &str, baud: Option<u32>, seconds: f32) -> PreviewResult {
    let total = Duration::from_secs_f32(seconds.clamp(0.5, 3.0));
    let bauds: Vec<u32> = match baud {
        Some(b) => vec![b],
        None => vec![9600, 4800, 38400, 115200],
    };
    let per = total / bauds.len() as u32;
    let (mut saw_bytes, mut error) = (false, None::<String>);
    for &b in &bauds {
        match tokio_serial::new(port, b).open_native_async() {
            Ok(stream) => {
                let mut reader = BufReader::new(stream);
                let mut lines = Vec::new();
                match read_window(&mut reader, per, &mut lines).await {
                    Attempt::Nmea(()) => {
                        return PreviewResult {
                            lines,
                            verdict: "nmea".to_string(),
                            baud: Some(b),
                            error_kind: None,
                            error_hint: None,
                        }
                    }
                    Attempt::Bytes => saw_bytes = true,
                    _ => {}
                }
            }
            Err(e) => {
                if e.kind == tokio_serial::ErrorKind::Io(std::io::ErrorKind::PermissionDenied) {
                    return PreviewResult {
                        lines: vec![],
                        verdict: "access_denied".to_string(),
                        baud: None,
                        error_kind: Some("access_denied".to_string()),
                        error_hint: Some(access_denied_hint(port)),
                    };
                }
                error = Some(e.to_string());
            }
        }
    }
    let (verdict, kind) = describe_failure(saw_bytes, false, error.as_deref());
    let verdict = if verdict.starts_with("open_failed") { "open_failed".to_string() } else { verdict };
    PreviewResult {
        lines: vec![],
        verdict,
        baud: None,
        error_kind: kind.map(str::to_string),
        error_hint: Some(match kind {
            Some("wrong_baud") => format!(
                "{port} sends data that is not NMEA at the rates tried. Pick the receiver's baud rate explicitly."
            ),
            _ => format!("{port} sent nothing. Check that this is the GPS receiver's port and that it is plugged in."),
        }),
    }
}

/// Run the GPS reader task
/// - Detects GPS on available serial ports every 5s
/// - Parses NMEA GGA sentences and updates StatusState
/// - Maintains accuracy history (up to 180 samples at 10s interval)
/// - Resilient to transient timeouts: allows 3 consecutive timeouts before reconnecting
pub async fn run_gps_task(
    state: Arc<RwLock<StatusState>>,
    _rtcm_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    mut cfg_rx: tokio::sync::watch::Receiver<GpsConfig>,
) {
    const DETECTION_INTERVAL_SECS: u64 = 5;
    const ACCURACY_HISTORY_MAX: usize = 180;
    const NMEA_TAIL_MAX: usize = 20;
    const MAX_CONSECUTIVE_TIMEOUTS: u32 = 3;
    /// Wall-clock ceiling for speculative probing in one detection round, so a
    /// machine with many unidentified COM ports doesn't stall the loop.
    const TIER3_PROBE_BUDGET: Duration = Duration::from_secs(20);
    /// An access-denied port is retried quickly: the holder (u-center, a
    /// terminal) is often closed within seconds of reading the hint.
    const DENIED_RETRY: Duration = Duration::from_secs(30);

    /// Sleep, but wake early when the GPS settings change.
    async fn pause(cfg_rx: &mut tokio::sync::watch::Receiver<GpsConfig>, secs: u64) {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(secs)) => {}
            _ = cfg_rx.changed() => {}
        }
    }

    let mut cache = ProbeCache::new();
    let mut contacts: HashMap<String, PortContact> = HashMap::new();
    let mut results: HashMap<String, String> = HashMap::new();
    let mut last_sample_time = tokio::time::Instant::now(); // accuracy sampling interval

    loop {
        // Settings can change at runtime (a manual port pick from the app).
        let gps_config = cfg_rx.borrow_and_update().clone();
        let sample_interval_secs = gps_config.sample_interval_secs.max(1);

        if !gps_config.enabled {
            info!("GPS disabled by config ([gps] enabled = false); GPS task parked");
            state.write().await.gps_info = GpsInfo::default();
            if cfg_rx.changed().await.is_err() {
                return;
            }
            continue;
        }

        // A configured port skips auto-detection entirely, so it only needs the one
        // baud (plus fallbacks). Unidentified ports get 9600 only on first contact —
        // the VK-162 and u-blox modules default to it — and are promoted to the full
        // sweep once they show bytes or after a silent first contact.
        let mut full_bauds: Vec<u32> = Vec::new();
        for b in gps_config.baud_rate.into_iter().chain([9600, 4800, 38400, 115200]) {
            if !full_bauds.contains(&b) {
                full_bauds.push(b);
            }
        }

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

        let ports = list_serial_ports();
        cache.retain_present(&ports);
        contacts.retain(|name, _| ports.iter().any(|p| &p.port_name == name));
        results.retain(|name, _| ports.iter().any(|p| &p.port_name == name));
        let candidates = rank_candidates(&ports, &policy, &cache, Instant::now());
        let forced = policy.forced_port.clone().filter(|p| !p.is_empty());

        if candidates.is_empty() {
            debug!("No GPS candidates, retrying in {}s", DETECTION_INTERVAL_SECS);
            {
                let mut s = state.write().await;
                s.gps_info.state = "absent".to_string();
                s.gps_info.error_kind = None;
                s.gps_info.error_hint = None;
                s.gps_info.candidates = ui_candidates(&ports, &candidates, &results);
            }
            pause(&mut cfg_rx, DETECTION_INTERVAL_SECS).await;
            continue;
        }

        // Probe candidates in tier order until one yields valid NMEA. The old
        // code returned only the first VID match and retried that same port
        // forever; a port that merely LOOKS like a GPS could therefore mask a
        // real receiver sitting behind it.
        let budget_start = Instant::now();
        let mut opened: Option<(BufReader<tokio_serial::SerialStream>, u32, GpsCandidate)> = None;
        let mut denied_port: Option<String> = None;
        for cand in &candidates {
            if cand.tier >= Tier::Unidentified && budget_start.elapsed() > TIER3_PROBE_BUDGET {
                debug!("Tier-3 probe budget spent for this round; resuming next scan");
                break;
            }
            let speculative = cand.tier >= Tier::Unidentified;
            let window = Duration::from_millis(if speculative { 1500 } else { 2000 });
            debug!(port = %cand.port_name, tier = ?cand.tier, "Probing serial port for NMEA");

            let mut contact = contacts.get(&cand.port_name).cloned().unwrap_or_default();
            let was_escalated = contact.escalated;
            let name = cand.port_name.clone();
            // Tell /gps/preview this port is being probed right now.
            state.write().await.gps_probing = Some(name.clone());
            let search = probe_with(speculative, &mut contact, &full_bauds, |baud| {
                let name = name.clone();
                async move { attempt_port(&name, baud, window).await }
            })
            .await;
            state.write().await.gps_probing = None;
            contacts.insert(cand.port_name.clone(), contact);

            match search {
                BaudSearch::Found(reader, baud) => {
                    results.insert(cand.port_name.clone(), format!("nmea@{baud}"));
                    opened = Some((reader, baud, cand.clone()));
                    break;
                }
                BaudSearch::NotFound { saw_bytes, denied, error } => {
                    let (result, _kind) = describe_failure(saw_bytes, denied, error.as_deref());
                    results.insert(cand.port_name.clone(), result);
                    if denied {
                        denied_port.get_or_insert_with(|| cand.port_name.clone());
                    }
                    // Only speculative tiers earn a backoff entry, and a silent
                    // first contact does not: the next round sweeps every baud.
                    let first_contact_miss = speculative && !was_escalated && !saw_bytes && !denied;
                    if cand.tier >= Tier::UsbSerialBridge && !first_contact_miss {
                        if let Some(info) = ports.iter().find(|p| p.port_name == cand.port_name) {
                            let (base, max) = if denied {
                                (DENIED_RETRY, DENIED_RETRY)
                            } else {
                                (policy.retry_base, policy.retry_max)
                            };
                            cache.record_failure(info, Instant::now(), base, max);
                        }
                    }
                }
            }
        }

        let (port_name, device_name) = match &opened {
            Some((_, _, c)) => (c.port_name.clone(), c.device_name.clone()),
            None => {
                // Publish WHY nothing was found: a held port (access denied) is
                // an error the user can fix; plain silence is just "absent".
                {
                    let mut s = state.write().await;
                    s.gps_info.candidates = ui_candidates(&ports, &candidates, &results);
                    s.gps_info.port = None;
                    s.gps_info.baud = None;
                    if let Some(p) = denied_port.as_ref() {
                        s.gps_info.state = "error".to_string();
                        s.gps_info.error_kind = Some("access_denied".to_string());
                        s.gps_info.error_hint = Some(access_denied_hint(p));
                        s.gps_info.port = Some(p.clone());
                    } else if let Some(f) = forced.as_ref() {
                        let r = results.get(f).cloned().unwrap_or_default();
                        let kind = if r == "wrong_baud" { "wrong_baud" } else { "no_data" };
                        s.gps_info.state = "error".to_string();
                        s.gps_info.error_kind = Some(kind.to_string());
                        s.gps_info.error_hint = Some(if kind == "wrong_baud" {
                            format!("{f} sends data that is not NMEA at the rates tried; set the receiver's baud rate explicitly.")
                        } else {
                            format!("Nothing was read from the selected port {f}. Check the port choice and the cable.")
                        });
                        s.gps_info.port = Some(f.clone());
                    } else {
                        s.gps_info.state = "absent".to_string();
                        s.gps_info.error_kind = None;
                        s.gps_info.error_hint = None;
                    }
                }
                debug!("No GPS validated this round, retrying in {}s", DETECTION_INTERVAL_SECS);
                pause(&mut cfg_rx, DETECTION_INTERVAL_SECS).await;
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
            s.gps_info.baud = opened.as_ref().map(|(_, b, _)| *b);
            s.gps_info.candidates = ui_candidates(&ports, &candidates, &results);
            s.refresh_gps_state();
        }

        match opened.map(|(r, b, _)| (r, b)) {
            Some((mut buf_reader, detected_baud)) => {
                info!(port = %port_name, baud = detected_baud, "GPS connection established");

                // Read NMEA sentences
                let mut line = String::new();
                let mut consecutive_timeouts: u32 = 0;

                loop {
                    // Check if GPS task should stop (detection re-triggered, or the
                    // settings changed: e.g. the user picked another port)
                    {
                        let s = state.read().await;
                        if !s.gps_detected {
                            break;
                        }
                    }
                    if cfg_rx.has_changed().unwrap_or(false) {
                        info!("GPS settings changed — re-detecting");
                        break;
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
                            if trimmed.starts_with('$') {
                                let mut s = state.write().await;
                                s.gps_nmea_tail.push_back(trimmed.to_string());
                                while s.gps_nmea_tail.len() > NMEA_TAIL_MAX {
                                    s.gps_nmea_tail.pop_front();
                                }
                            }

                            // Try to parse GGA sentence
                            if trimmed.starts_with("$") && trimmed.contains("GGA") {
                                if let Some((lat, lon, alt, fix_quality, satellites, hdop)) = parse_gga(trimmed) {
                                    let mut s = state.write().await;

                                    // Update state from GGA
                                    s.gps_fix_quality = fix_quality;
                                    s.gps_satellites = satellites;
                                    s.gps_hdop = hdop;
                                    s.gps_latitude = Some(lat);
                                    s.gps_longitude = Some(lon);
                                    s.gps_altitude_m = Some(alt);
                                    s.gps_last_fix = Some(chrono::Utc::now().to_rfc3339());
                                    s.refresh_gps_state();

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
                            } else if trimmed.starts_with("$") && trimmed.contains("VTG") {
                                // GGA carries no speed field — VTG is the only
                                // sentence that ever populates gps_speed_knots.
                                if let Some(speed_knots) = parse_vtg(trimmed) {
                                    let mut s = state.write().await;
                                    s.gps_speed_knots = Some(speed_knots);
                                    debug!(speed_knots, "GPS VTG parsed");
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
                    s.gps_nmea_tail.clear();
                    s.gps_info.state = "absent".to_string();
                    s.gps_info.port = None;
                    s.gps_info.baud = None;
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
        let (lat, lon, _alt, fix, sats, _hdop) = parse_gga(GNGGA).expect("$GNGGA must parse");
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
    fn parse_gga_returns_hdop() {
        // THE BUG-4 HDOP REGRESSION TEST. Field 8 is documented directly above
        // parse_gga as HDOP but was never read, so gps_hdop stayed None forever
        // and a real 1.2 HDOP surfaced downstream as the 999.9 sentinel default.
        let (.., hdop) = parse_gga(GNGGA).expect("$GNGGA must parse");
        assert_eq!(hdop, Some(1.2));
    }

    #[test]
    fn parse_vtg_returns_speed_knots() {
        // THE BUG-4 VTG REGRESSION TEST. Only GGA sentences were ever parsed,
        // and GGA carries no speed field, so gps_speed_knots stayed None for
        // the lifetime of every GPS session regardless of receiver output.
        const VTG: &str = "$GPVTG,054.7,T,034.4,M,005.5,N,010.2,K*48";
        let speed = parse_vtg(VTG).expect("$GPVTG must parse");
        assert!((speed - 5.5).abs() < 0.01);
    }

    #[test]
    fn parse_vtg_rejects_garbage() {
        assert!(parse_vtg("$GPVTG,too,few").is_none());
        assert!(parse_vtg("").is_none());
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

    // --- M2: the reporter's real port list, ordering, baud promotion --------

    fn usb(name: &str, vid: u16, pid: u16, product: &str) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: "usb".to_string(),
            vid: Some(vid),
            pid: Some(pid),
            manufacturer: Some("SAMSUNG Electronics Co., Ltd.".to_string()),
            product: Some(product.to_string()),
            serial_number: None,
        }
    }

    /// The exact 5-port list from the field report (COM5 is the VK-162).
    fn reporter_ports() -> Vec<SerialPortInfo> {
        vec![
            port("COM5", None, "unknown"),
            usb("COM11", 1256, 26720, "SAMSUNG Mobile USB Modem"),
            usb("COM12", 1256, 26720, "SAMSUNG Mobile USB Modem #2"),
            usb("COM13", 1256, 26720, "SAMSUNG Mobile USB Modem #3"),
            usb("COM14", 1256, 26720, "SAMSUNG Mobile USB Modem #4"),
        ]
    }

    #[test]
    fn reporters_port_list_probes_com5_and_no_phone_modem() {
        let got = rank_candidates(&reporter_ports(), &policy(), &ProbeCache::new(), Instant::now());
        let names: Vec<&str> = got.iter().map(|c| c.port_name.as_str()).collect();
        assert_eq!(names, vec!["COM5"], "phone modems must not be auto-probed");
    }

    #[test]
    fn modem_ports_stay_listed_for_manual_choice_but_are_not_probed() {
        let ports = reporter_ports();
        let probed = rank_candidates(&ports, &policy(), &ProbeCache::new(), Instant::now());
        let ui = ui_candidates(&ports, &probed, &HashMap::new());
        assert_eq!(ui.len(), 5);
        let com5 = ui.iter().find(|c| c.port_name == "COM5").unwrap();
        assert!(com5.probed);
        let com11 = ui.iter().find(|c| c.port_name == "COM11").unwrap();
        assert!(!com11.probed);
        assert!(com11.last_result.contains("modem"), "{}", com11.last_result);
        assert_eq!(com11.vid, Some(1256));
        assert_eq!(com11.product.as_deref(), Some("SAMSUNG Mobile USB Modem"));
    }

    #[test]
    fn com5_sorts_before_com11_naturally() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("COM5", "COM11"), Less);
        assert_eq!(natural_cmp("COM11", "COM5"), Greater);
        assert_eq!(natural_cmp("COM14", "COM14"), Equal);
        assert_eq!(natural_cmp("/dev/ttyUSB2", "/dev/ttyUSB10"), Less);
        // Two plain no-VID ports (no modems): numeric, not lexicographic.
        let ports = vec![
            port("COM12", None, "unknown"),
            port("COM5", None, "unknown"),
            port("COM3", None, "unknown"),
        ];
        let got = rank_candidates(&ports, &policy(), &ProbeCache::new(), Instant::now());
        let names: Vec<&str> = got.iter().map(|c| c.port_name.as_str()).collect();
        assert_eq!(names, vec!["COM3", "COM5", "COM12"]);
    }

    #[test]
    fn no_vid_ports_rank_before_unknown_usb_devices() {
        let ports = vec![
            usb("COM3", 0x1234, 0x5678, "Some USB Serial Thing"),
            port("COM12", None, "unknown"),
        ];
        let got = rank_candidates(&ports, &policy(), &ProbeCache::new(), Instant::now());
        assert_eq!(got[0].port_name, "COM12");
        assert_eq!(got[0].tier, Tier::Unidentified);
        assert_eq!(got[1].port_name, "COM3");
        assert_eq!(got[1].tier, Tier::UnknownUsb);
    }

    #[tokio::test]
    async fn no_vid_port_at_4800_is_detected_after_first_contact_shows_bytes() {
        // The receiver streams NMEA at 4800 only. At 9600 the same stream is
        // garbled but non-empty ("bytes seen"): promotion must follow at once.
        let mut contact = PortContact::default();
        let tried = std::sync::Mutex::new(Vec::new());
        let r = probe_with(true, &mut contact, &[9600, 4800, 38400, 115200], |baud| {
            tried.lock().unwrap().push(baud);
            async move {
                match baud {
                    4800 => Attempt::Nmea("reader"),
                    9600 => Attempt::Bytes,
                    _ => Attempt::Silent,
                }
            }
        })
        .await;
        assert!(matches!(r, BaudSearch::Found("reader", 4800)));
        assert_eq!(*tried.lock().unwrap(), vec![9600, 4800]);
    }

    #[tokio::test]
    async fn silent_first_contact_escalates_to_the_full_sweep_next_round() {
        let mut contact = PortContact::default();
        let attempt = |baud: u32| async move {
            if baud == 4800 { Attempt::Nmea(()) } else { Attempt::Silent }
        };
        let first = probe_with(true, &mut contact, &[9600, 4800, 38400, 115200], attempt).await;
        assert!(matches!(first, BaudSearch::NotFound { saw_bytes: false, .. }));
        assert!(contact.escalated, "a first-contact miss must promote the port");
        let second = probe_with(true, &mut contact, &[9600, 4800, 38400, 115200], attempt).await;
        assert!(matches!(second, BaudSearch::Found((), 4800)));
    }

    #[tokio::test]
    async fn known_receivers_sweep_every_baud_immediately() {
        let mut contact = PortContact::default();
        let r = probe_with(false, &mut contact, &[9600, 4800], |b| async move {
            if b == 4800 { Attempt::Nmea(()) } else { Attempt::Silent }
        })
        .await;
        assert!(matches!(r, BaudSearch::Found((), 4800)));
    }

    #[tokio::test]
    async fn access_denied_stops_the_search_and_is_reported() {
        let mut contact = PortContact::default();
        let tried = std::sync::Mutex::new(0u32);
        let r: BaudSearch<()> = probe_with(false, &mut contact, &[9600, 4800, 38400], |_| {
            *tried.lock().unwrap() += 1;
            async { Attempt::Denied }
        })
        .await;
        assert!(matches!(r, BaudSearch::NotFound { denied: true, .. }));
        assert_eq!(*tried.lock().unwrap(), 1, "permission does not depend on baud");
        let (res, kind) = describe_failure(false, true, None);
        assert_eq!((res.as_str(), kind), ("access_denied", Some("access_denied")));
    }

    #[test]
    fn failure_kinds_are_distinguished() {
        assert_eq!(describe_failure(true, false, None).1, Some("wrong_baud"));
        assert_eq!(describe_failure(false, false, None).1, Some("no_data"));
    }

    #[test]
    fn windows_access_denied_hint_names_serial_ballpoint_and_the_port() {
        let h = access_denied_hint_for("COM5", true);
        assert!(h.contains("COM5") && h.contains("Microsoft Serial BallPoint"), "{h}");
        assert!(h.contains("u-center"));
        assert!(!h.contains("dialout"));
        assert!(access_denied_hint_for("/dev/ttyUSB0", false).contains("dialout"));
    }

    #[test]
    fn gps_status_object_has_the_contract_keys() {
        let info = GpsInfo {
            candidates: ui_candidates(&reporter_ports(), &[], &HashMap::new()),
            ..GpsInfo::default()
        };
        let v = serde_json::to_value(&info).unwrap();
        for k in ["state", "port", "baud", "error_kind", "error_hint", "candidates"] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        for k in ["port_name", "port_type", "vid", "pid", "product", "probed", "last_result"] {
            assert!(v["candidates"][0].get(k).is_some(), "candidate missing {k}");
        }
    }

    #[test]
    fn preview_only_opens_ports_the_os_lists() {
        let ports = vec![port("COM5", None, "unknown"), port("/dev/ttyUSB0", None, "usb")];
        assert_eq!(canonical_listed_port("COM5", &ports, true).as_deref(), Some("COM5"));
        assert_eq!(canonical_listed_port("com5", &ports, true).as_deref(), Some("COM5"));
        assert_eq!(canonical_listed_port("\\\\.\\COM5", &ports, true).as_deref(), Some("COM5"));
        assert_eq!(canonical_listed_port("/dev/ttyUSB0", &ports, false).as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(canonical_listed_port("com5", &ports, false), None, "unix names are case-sensitive");
        for bad in ["/dev/sda", "/etc/passwd", "COM99", "", "../COM5"] {
            assert_eq!(canonical_listed_port(bad, &ports, true), None, "{bad}");
        }
    }
}

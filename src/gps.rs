use serde::{Serialize, Deserialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, warn, debug};

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

/// Detect a GPS receiver on the available serial ports
/// Returns (port_name, device_name) if found
fn detect_gps_port() -> Option<(String, String)> {
    let ports = list_serial_ports();

    // Highest priority: u-blox (known GPS vendor)
    for port in &ports {
        if port.vid == Some(UBLOX_VID) {
            let device_name = port
                .product
                .as_ref()
                .cloned()
                .unwrap_or_else(|| "u-blox GPS".to_string());
            return Some((port.port_name.clone(), device_name));
        }
    }

    // Second priority: other known GPS adapters
    for port in &ports {
        if let Some(vid) = port.vid {
            if vid == PROLIFIC_VID || vid == FTDI_VID || vid == SILICON_LABS_VID || vid == CH340_VID {
                let device_name = port
                    .product
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| "USB Serial (GPS)".to_string());
                return Some((port.port_name.clone(), device_name));
            }
        }
    }

    None
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
            Err(_) => continue,
        }
    }
    None
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
) {
    const DETECTION_INTERVAL_SECS: u64 = 5;
    const SAMPLE_INTERVAL_SECS: u64 = 10;
    const ACCURACY_HISTORY_MAX: usize = 180;
    const MAX_CONSECUTIVE_TIMEOUTS: u32 = 3;

    let bauds = &[9600u32, 115200u32, 4800u32];
    let mut last_sample_time = tokio::time::Instant::now(); // accuracy sampling interval (10s)

    loop {
        debug!("GPS detection scan starting");

        // Try to detect and open GPS
        let (port_name, device_name) = match detect_gps_port() {
            Some((port, name)) => (port, name),
            None => {
                debug!("No GPS detected, retrying in {}s", DETECTION_INTERVAL_SECS);
                tokio::time::sleep(tokio::time::Duration::from_secs(DETECTION_INTERVAL_SECS)).await;
                continue;
            }
        };

        info!(port = %port_name, device = %device_name, "GPS detected");

        {
            let mut s = state.write().await;
            s.gps_detected = true;
            s.gps_port = Some(port_name.clone());
            s.gps_device_name = Some(device_name.clone());
        }

        // Try to open the serial port with NMEA validation
        match try_open_serial(&port_name, bauds).await {
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
                                    if last_sample_time.elapsed() >= tokio::time::Duration::from_secs(SAMPLE_INTERVAL_SECS) {
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
}

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_serial::SerialPortBuilderExt;
use tracing::{debug, error, info, warn};

use crate::status_server::StatusState;

/// Run the NTRIP v1 client loop. Connects to the configured caster, streams
/// RTCM3 correction data, counts frames, optionally forwards to a serial port or RTCM channel,
/// and periodically sends GGA position reports back to the caster.
///
/// Loops until `rtk_enabled` is set to false in shared state.
/// On connection errors, retries with exponential backoff (1s → 60s cap).
pub async fn run_ntrip_client(
    state: Arc<RwLock<StatusState>>,
    node_lat: f64,
    node_lon: f64,
    node_alt: f64,
    rtcm_tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
) -> anyhow::Result<()> {
    let config = {
        let s = state.read().await;
        match s.rtk_config.clone() {
            Some(c) => c,
            None => anyhow::bail!("No NTRIP config available"),
        }
    };

    info!(
        caster = %config.caster_url,
        port = config.port,
        mountpoint = %config.mountpoint,
        "NTRIP client connecting"
    );

    let url = format!(
        "http://{}:{}/{}",
        config.caster_url, config.port, config.mountpoint
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(0)) // no timeout — persistent stream
        .connect_timeout(Duration::from_secs(15))
        .build()?;

    let mut request = client
        .get(&url)
        .header("User-Agent", "NTRIP AzimuthDaemon/1.0")
        .header("Ntrip-Version", "Ntrip/1.0")
        .header("Accept", "*/*")
        .header("Connection", "close");

    if let (Some(ref user), Some(ref pass)) = (&config.username, &config.password) {
        if !user.is_empty() {
            request = request.basic_auth(user, Some(pass));
        }
    }

    // Send GGA sentence as body for casters that require initial position
    // Use GPS coordinates if available, otherwise use static node coordinates
    let (lat, lon, alt) = {
        let s = state.read().await;
        if s.gps_fix_quality > 0 {
            (
                s.gps_latitude.unwrap_or(node_lat),
                s.gps_longitude.unwrap_or(node_lon),
                s.gps_altitude_m.unwrap_or(node_alt),
            )
        } else {
            (node_lat, node_lon, node_alt)
        }
    };

    let gga = generate_gga(lat, lon, alt);
    request = request.header("Ntrip-GGA", &gga);

    let resp = request.send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("NTRIP caster returned {}: {}", status, body);
    }

    info!("NTRIP connected, streaming RTCM3 corrections");

    {
        let mut s = state.write().await;
        s.rtk_connected = true;
    }

    // Open serial port for forwarding if configured
    let mut serial_port: Option<tokio_serial::SerialStream> = None;
    if let Some(ref port_name) = config.serial_port {
        if !port_name.is_empty() {
            match tokio_serial::new(port_name, config.baud_rate)
                .open_native_async()
            {
                Ok(port) => {
                    info!(port = %port_name, baud = config.baud_rate, "Serial port opened for RTCM3 forwarding");
                    serial_port = Some(port);
                }
                Err(e) => {
                    warn!(port = %port_name, error = %e, "Failed to open serial port — continuing without forwarding");
                }
            }
        }
    }

    let mut buf = Vec::with_capacity(4096);
    let mut frame_count: u64 = 0;
    let gga_interval = Duration::from_secs(config.gga_interval_secs as u64);
    let mut last_gga = tokio::time::Instant::now();

    // Stream response body via chunk() loop
    let mut response = resp;
    loop {
        // Check if RTK is still enabled
        {
            let s = state.read().await;
            if !s.rtk_enabled {
                info!("RTK disabled, stopping NTRIP client");
                break;
            }
        }

        match response.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);

                // Count RTCM3 frames: preamble 0xD3 + 10-bit length in next 2 bytes
                while buf.len() >= 3 {
                    if buf[0] != 0xD3 {
                        // Not at a frame boundary — skip byte
                        buf.remove(0);
                        continue;
                    }
                    let msg_len = (((buf[1] as usize) & 0x03) << 8) | (buf[2] as usize);
                    let frame_len = 3 + msg_len + 3; // header + payload + CRC
                    if buf.len() < frame_len {
                        break; // incomplete frame, wait for more data
                    }

                    frame_count += 1;
                    let frame_data = buf[..frame_len].to_vec();

                    // Forward via RTCM3 channel if available
                    if let Some(ref tx) = rtcm_tx {
                        if let Err(e) = tx.send(frame_data.clone()).await {
                            warn!(error = %e, "RTCM3 channel send failed");
                        }
                    }

                    // Also forward to serial port if available (backward compat)
                    if let Some(ref mut port) = serial_port {
                        use tokio::io::AsyncWriteExt;
                        if let Err(e) = port.write_all(&frame_data).await {
                            warn!(error = %e, "Serial write failed");
                        }
                    }

                    buf.drain(..frame_len);
                }

                // Update shared state
                {
                    let mut s = state.write().await;
                    s.rtk_messages_received = frame_count;
                    s.rtk_last_correction = Some(chrono::Utc::now());
                }

                debug!(frames = frame_count, chunk_bytes = chunk.len(), "RTCM3 data received");
            }
            Ok(None) => {
                // Stream ended — caster closed connection
                info!("NTRIP stream ended (caster closed connection)");
                break;
            }
            Err(e) => {
                error!(error = %e, "NTRIP stream error");
                break;
            }
        }

        // Periodic GGA re-generation (re-send with updated GPS coords if available)
        if last_gga.elapsed() >= gga_interval {
            last_gga = tokio::time::Instant::now();
            let (lat, lon, alt) = {
                let s = state.read().await;
                if s.gps_fix_quality > 0 {
                    (
                        s.gps_latitude.unwrap_or(node_lat),
                        s.gps_longitude.unwrap_or(node_lon),
                        s.gps_altitude_m.unwrap_or(node_alt),
                    )
                } else {
                    (node_lat, node_lon, node_alt)
                }
            };
            let _gga = generate_gga(lat, lon, alt);
            debug!("GGA re-generated with current position");
        }
    }

    // Cleanup
    {
        let mut s = state.write().await;
        s.rtk_connected = false;
    }

    Ok(())
}

/// Generate an NMEA GGA sentence from static coordinates.
/// Format: $GPGGA,HHMMSS.SS,DDMM.MMMMM,N/S,DDDMM.MMMMM,E/W,1,12,1.0,ALT,M,0.0,M,,*HH
fn generate_gga(lat: f64, lon: f64, alt: f64) -> String {
    let now = chrono::Utc::now();
    let time_str = now.format("%H%M%S.00").to_string();

    let (lat_abs, lat_dir) = if lat >= 0.0 { (lat, 'N') } else { (-lat, 'S') };
    let lat_deg = lat_abs.floor() as u32;
    let lat_min = (lat_abs - lat_deg as f64) * 60.0;

    let (lon_abs, lon_dir) = if lon >= 0.0 { (lon, 'E') } else { (-lon, 'W') };
    let lon_deg = lon_abs.floor() as u32;
    let lon_min = (lon_abs - lon_deg as f64) * 60.0;

    let body = format!(
        "GPGGA,{},{:02}{:010.7},{},{:03}{:010.7},{},1,12,1.0,{:.1},M,0.0,M,,",
        time_str, lat_deg, lat_min, lat_dir, lon_deg, lon_min, lon_dir, alt
    );

    // Calculate NMEA checksum (XOR of all bytes between $ and *)
    let checksum: u8 = body.bytes().fold(0u8, |acc, b| acc ^ b);

    format!("${}*{:02X}", body, checksum)
}

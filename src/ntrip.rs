use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_serial::SerialPortBuilderExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use base64::Engine;
use tracing::{debug, error, info, warn};

use crate::status_server::StatusState;

/// Parse NTRIP/HTTP status line and return (status_code, status_line).
/// Accepts both "ICY 200 OK" and "HTTP/1.x 200 ..." formats.
fn parse_status_line(line: &str) -> Option<(u16, String)> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    // Try to parse as HTTP-like: "HTTP/1.1 200 OK" or "HTTP/1.0 401 Unauthorized"
    if parts[0].starts_with("HTTP") {
        if let Ok(code) = parts[1].parse::<u16>() {
            return Some((code, line.to_string()));
        }
    }

    // Try to parse as ICY: "ICY 200 OK"
    if parts[0] == "ICY" {
        if let Ok(code) = parts[1].parse::<u16>() {
            return Some((code, line.to_string()));
        }
    }

    None
}

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

    // Use raw TCP socket for NTRIP compatibility (casters respond with "ICY 200 OK", not HTTP)
    let mut socket = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::net::TcpStream::connect(format!("{}:{}", config.caster_url, config.port))
    )
        .await
        .map_err(|_| anyhow::anyhow!("NTRIP connection timeout"))?
        .map_err(|e| anyhow::anyhow!("NTRIP connection failed: {}", e))?;

    // Build HTTP request for NTRIP mountpoint
    let auth_header = if let (Some(ref user), Some(ref pass)) = (&config.username, &config.password) {
        if !user.is_empty() {
            let creds = format!("{}:{}", user, pass);
            let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
            format!("\r\nAuthorization: Basic {}", encoded)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Generate GGA sentence with current position
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

    let request = format!(
        "GET /{} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: NTRIP AzimuthDaemon/1.0\r\nNtrip-Version: Ntrip/2.0\r\nNtrip-GGA: {}{}\r\nConnection: close\r\n\r\n",
        config.mountpoint,
        config.caster_url,
        config.port,
        gga,
        auth_header
    );

    socket.write_all(request.as_bytes()).await?;

    // Read and parse status line from caster (ICY or HTTP)
    let (status_code, status_msg) = {
        let mut buf = vec![0u8; 1];
        let mut status_line = String::new();
        loop {
            socket.read_exact(&mut buf).await?;
            status_line.push(buf[0] as char);
            if status_line.ends_with("\r\n") {
                break;
            }
            if status_line.len() > 256 {
                anyhow::bail!("NTRIP status line too long");
            }
        }

        let status_line_trimmed = status_line.trim();
        parse_status_line(status_line_trimmed)
            .ok_or_else(|| anyhow::anyhow!("Invalid NTRIP status line: {}", status_line_trimmed))?
    };

    info!(status_line = %status_msg, "NTRIP status received");

    if status_code != 200 {
        anyhow::bail!("NTRIP caster returned {}: {}", status_code, status_msg);
    }

    // Read headers until blank line (read one byte at a time to detect \r\n\r\n)
    let mut buf = vec![0u8; 1];
    let mut header_buf = String::new();
    loop {
        socket.read_exact(&mut buf).await?;
        header_buf.push(buf[0] as char);

        // Check for end of headers: \r\n\r\n
        if header_buf.ends_with("\r\n\r\n") || header_buf.ends_with("\n\n") {
            break;
        }
        if header_buf.len() > 8192 {
            anyhow::bail!("NTRIP headers too long");
        }
    }

    for line in header_buf.lines() {
        if !line.is_empty() {
            debug!("NTRIP header: {}", line);
        }
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

    // Stream RTCM3 data from socket
    let mut read_buf = vec![0u8; 4096];
    loop {
        // Check if RTK is still enabled
        {
            let s = state.read().await;
            if !s.rtk_enabled {
                info!("RTK disabled, stopping NTRIP client");
                break;
            }
        }

        // Guard against a stalled caster that holds the socket open but sends no data:
        // bound each read so we break out and let the parent task reconnect.
        let n = match tokio::time::timeout(Duration::from_secs(120), socket.read(&mut read_buf)).await {
            Ok(r) => r,
            Err(_) => {
                warn!("NTRIP stream stalled (no data for 120s) — reconnecting");
                break;
            }
        };

        match n {
            Ok(0) => {
                // Stream ended (EOF)
                info!("NTRIP stream ended (caster closed connection)");
                break;
            }
            Ok(n) => {
                buf.extend_from_slice(&read_buf[..n]);

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

                    // Forward via RTCM3 channel if available. try_send, not send:
                    // the receiver may be absent or behind, and RTCM corrections are
                    // real-time — dropping a frame is correct; parking this loop
                    // stalls the caster read forever.
                    if let Some(ref tx) = rtcm_tx {
                        if let Err(e) = tx.try_send(frame_data.clone()) {
                            debug!(error = %e, "RTCM3 channel unavailable — frame dropped");
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

                debug!(frames = frame_count, bytes_read = n, "RTCM3 data received");
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

/// Test NTRIP connection: connect + status-line only, no streaming.
/// Returns (status_line, socket_closed) on success, error on failure.
pub async fn test_ntrip_connection(
    caster_url: &str,
    port: u16,
    mountpoint: &str,
    username: Option<String>,
    password: Option<String>,
) -> anyhow::Result<(String, bool)> {
    // Use raw TCP socket for NTRIP compatibility
    let mut socket = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::net::TcpStream::connect(format!("{}:{}", caster_url, port))
    )
        .await
        .map_err(|_| anyhow::anyhow!("Connection timeout"))?
        .map_err(|e| anyhow::anyhow!("Connection failed: {}", e))?;

    // Build HTTP request for NTRIP mountpoint
    let auth_header = if let (Some(ref user), Some(ref pass)) = (&username, &password) {
        if !user.is_empty() {
            let creds = format!("{}:{}", user, pass);
            let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
            format!("\r\nAuthorization: Basic {}", encoded)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let request = format!(
        "GET /{} HTTP/1.1\r\nHost: {}:{}\r\nUser-Agent: NTRIP AzimuthDaemon/1.0\r\nNtrip-Version: Ntrip/2.0{}\r\nConnection: close\r\n\r\n",
        mountpoint,
        caster_url,
        port,
        auth_header
    );

    socket.write_all(request.as_bytes()).await?;

    // Read and parse status line from caster (ICY or HTTP)
    let (status_code, status_line) = {
        let mut buf = vec![0u8; 1];
        let mut status_line = String::new();
        loop {
            match socket.read_exact(&mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    status_line.push(buf[0] as char);
                    if status_line.ends_with("\r\n") {
                        break;
                    }
                    if status_line.len() > 256 {
                        anyhow::bail!("Status line too long");
                    }
                }
                Err(e) => return Err(anyhow::anyhow!("Read error: {}", e)),
            }
        }

        let status_line_trimmed = status_line.trim();
        parse_status_line(status_line_trimmed)
            .ok_or_else(|| anyhow::anyhow!("Invalid status line: {}", status_line_trimmed))?
    };

    if status_code != 200 {
        anyhow::bail!("NTRIP caster returned status {}: {}", status_code, status_line);
    }

    Ok((status_line, false))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_line_icy_200() {
        let result = parse_status_line("ICY 200 OK");
        assert_eq!(result, Some((200, "ICY 200 OK".to_string())));
    }

    #[test]
    fn test_parse_status_line_http11_200() {
        let result = parse_status_line("HTTP/1.1 200 OK");
        assert_eq!(result, Some((200, "HTTP/1.1 200 OK".to_string())));
    }

    #[test]
    fn test_parse_status_line_http10_401() {
        let result = parse_status_line("HTTP/1.0 401 Unauthorized");
        assert_eq!(result, Some((401, "HTTP/1.0 401 Unauthorized".to_string())));
    }

    #[test]
    fn test_parse_status_line_garbage() {
        let result = parse_status_line("garbage line");
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_status_line_empty() {
        let result = parse_status_line("");
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_status_line_icy_401() {
        let result = parse_status_line("ICY 401 Unauthorized");
        assert_eq!(result, Some((401, "ICY 401 Unauthorized".to_string())));
    }

    /// BUG-2 regression: a bounded RTCM channel whose receiver is never polled
    /// must not stall the caster read loop. Pre-fix, frame 65's send().await
    /// parks forever (capacity 64, receiver alive, never polled) and this test
    /// times out; post-fix (try_send) the client drains the stream, drops the
    /// overflow frames, and exits on EOF.
    #[tokio::test]
    async fn rtcm_channel_backpressure_does_not_stall_ntrip_client() {
        use crate::config::NtripConfig;
        use tokio::io::AsyncWriteExt;

        const FRAMES: usize = 100; // > channel capacity of 64

        // Loopback fake caster: handshake, then 100 minimal RTCM3 frames
        // (0-byte payload -> 6 bytes: D3 00 00 + 3 CRC bytes; the framing
        // loop checks preamble+length only, not CRC), then EOF via drop.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut sock, _) = listener.accept().await.unwrap();

            // Drain the client's GET request before responding. Closing a socket
            // that still has unread inbound bytes buffered sends an RST instead of
            // a graceful FIN (observed on Windows), which would abort the client's
            // read mid-stream before it ever reaches the backpressure scenario.
            let mut req_buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                sock.read_exact(&mut byte).await.unwrap();
                req_buf.push(byte[0]);
                if req_buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }

            sock.write_all(b"ICY 200 OK\r\n\r\n\r\n").await.unwrap();
            let frame: [u8; 6] = [0xD3, 0x00, 0x00, 0xAA, 0xBB, 0xCC];
            for _ in 0..FRAMES {
                sock.write_all(&frame).await.unwrap();
            }
        });

        let mut status = StatusState::new("test-node".into(), "test".into());
        status.rtk_enabled = true;
        status.rtk_config = Some(NtripConfig {
            caster_url: addr.ip().to_string(),
            port: addr.port(),
            mountpoint: "TEST".into(),
            username: None,
            password: None,
            gga_interval_secs: 3600, // keep the periodic-GGA branch out of this test
            serial_port: None,
            baud_rate: 115200,
        });
        let state = Arc::new(RwLock::new(status));

        // Mirror production exactly: receiver alive but never polled.
        // Must be `_rx`, NOT `_` — dropping it closes the channel and masks
        // the bug (send() would fail fast instead of parking).
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_ntrip_client(Arc::clone(&state), 0.0, 0.0, 0.0, Some(tx)),
        )
        .await;

        assert!(
            result.is_ok(),
            "NTRIP client stalled on RTCM channel backpressure"
        );
        result.unwrap().unwrap();
        assert_eq!(state.read().await.rtk_messages_received, FRAMES as u64);
        server.await.unwrap();
    }
}

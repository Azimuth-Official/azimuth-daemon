#[cfg(target_os = "windows")]
pub mod driver_install;

use crate::observation::Observation;
use chrono::Utc;
use rtl_sdr_rs::{RtlSdr, TunerGain, DEFAULT_BUF_LENGTH};
use tracing::{error, info, warn};

pub struct SdrCapture {
    device: Option<RtlSdr>,
    center_frequency: u64,
}

impl SdrCapture {
    pub fn new(device_index: u32, sample_rate: u32, center_frequency: u64, gain: i32) -> Self {
        // On Windows, check if RTL2832U needs WinUSB driver installed
        #[cfg(target_os = "windows")]
        {
            let drv = driver_install::check_driver_status();
            if drv == driver_install::DriverStatus::NeedsInstall {
                warn!("RTL2832U detected without driver — attempting WinUSB install");
                let result = driver_install::ensure_driver();
                match &result {
                    driver_install::DriverStatus::Installed => {
                        info!("WinUSB driver installed successfully — waiting for device")
                    }
                    other => warn!(?other, "Driver install did not succeed"),
                }
                // Brief pause for Windows to recognize the new driver
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }

        match Self::init_device(device_index, sample_rate, center_frequency, gain) {
            Ok(device) => {
                info!(
                    device_index,
                    sample_rate,
                    center_frequency,
                    gain,
                    "SDR device opened and configured"
                );
                Self {
                    device: Some(device),
                    center_frequency,
                }
            }
            Err(e) => {
                warn!(error = %e, "SDR device not available — running in stub mode");
                Self {
                    device: None,
                    center_frequency,
                }
            }
        }
    }

    fn init_device(
        device_index: u32,
        sample_rate: u32,
        center_frequency: u64,
        gain: i32,
    ) -> anyhow::Result<RtlSdr> {
        let count = RtlSdr::get_device_count()
            .map_err(|e| anyhow::anyhow!("Failed to enumerate SDR devices: {}", e))?;
        info!(count, "RTL-SDR devices found");

        if count == 0 {
            anyhow::bail!("No RTL-SDR devices found");
        }

        let mut device = RtlSdr::open_with_index(device_index as usize)
            .map_err(|e| anyhow::anyhow!("open device {}: {}", device_index, e))?;

        device
            .set_center_freq(center_frequency as u32)
            .map_err(|e| anyhow::anyhow!("set_center_freq({} Hz): {}", center_frequency, e))?;

        device
            .set_sample_rate(sample_rate)
            .map_err(|e| anyhow::anyhow!("set_sample_rate({} Hz): {}", sample_rate, e))?;

        let tuner_gain = if gain == 0 {
            TunerGain::Auto
        } else {
            TunerGain::Manual(gain)
        };
        device
            .set_tuner_gain(tuner_gain)
            .map_err(|e| anyhow::anyhow!("set_tuner_gain: {}", e))?;

        device
            .reset_buffer()
            .map_err(|e| anyhow::anyhow!("reset_buffer: {}", e))?;

        Ok(device)
    }

    pub fn is_connected(&self) -> bool {
        self.device.is_some()
    }

    /// Stage A: read IQ samples and compute signal power.
    /// Returns a single fm_rds observation with signal_strength_dbm.
    pub fn capture_batch(&self) -> Vec<Observation> {
        let device = match &self.device {
            Some(d) => d,
            None => return Vec::new(),
        };

        // Flush stale buffered samples before capturing fresh data
        if let Err(e) = device.reset_buffer() {
            warn!(error = %e, "reset_buffer before read failed");
        }

        let mut buf = vec![0u8; DEFAULT_BUF_LENGTH];
        let n_read = match device.read_sync(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                error!(error = %e, "SDR read_sync failed");
                return Vec::new();
            }
        };

        if n_read < 2 {
            warn!(bytes = n_read, "SDR read returned too few bytes");
            return Vec::new();
        }

        // RTL2832U outputs interleaved unsigned 8-bit IQ: [I0, Q0, I1, Q1, ...]
        // Normalize to [-1.0, 1.0] range, then compute mean power.
        let n_samples = n_read / 2;
        let mut power_sum = 0.0f64;

        for i in 0..n_samples {
            let iq_i = (buf[2 * i] as f64 - 127.5) / 127.5;
            let iq_q = (buf[2 * i + 1] as f64 - 127.5) / 127.5;
            power_sum += iq_i * iq_i + iq_q * iq_q;
        }

        let mean_power = power_sum / n_samples as f64;
        // dBFS relative to full-scale (1.0). Uncalibrated dBm = dBFS + fixed offset.
        let db_fs = 10.0 * mean_power.log10();
        let signal_strength_dbm = db_fs - 30.0;

        info!(
            samples = n_samples,
            db_fs = format!("{:.1}", db_fs),
            signal_dbm = format!("{:.1}", signal_strength_dbm),
            freq_hz = self.center_frequency,
            "IQ batch captured"
        );

        vec![Observation {
            signal_type: "fm_rds".to_string(),
            observed_at: Utc::now(),
            frequency_hz: Some(self.center_frequency),
            timestamp_ns: None,
            tdoa_offset_ns: None,
            signal_strength_dbm: Some(signal_strength_dbm),
            snr_db: None,
            source_id: None,
            raw_data: None,
        }]
    }
}

#[cfg(target_os = "windows")]
pub mod driver_install;

pub mod dtv;
pub mod fm_demod;
pub mod lte;
pub mod rds;
pub mod scanner;
pub mod recovery;

use rtl_sdr_rs::{RtlSdr, TunerGain};
use tracing::{info, warn};

/// Linux: detach the kernel driver (`dvb_usb_rtl28xxu`) that claims the RTL2832U —
/// especially over usbip — before rtl-sdr-rs opens it, or `open()` fails with "open
/// device N". Uses a PLAIN `detach_kernel_driver` via rusb, NOT `set_auto_detach`
/// (auto mode re-attaches the kernel driver when the handle/interface is released,
/// handing the device straight back before rtl-sdr-rs opens it). The handle is dropped
/// without claiming and without reattach, so the detach persists past close. No-op on
/// non-Linux (Windows uses a WinUSB/libusb driver, no kernel-detach needed).
#[cfg(target_os = "linux")]
fn detach_rtl_kernel_drivers() {
    let devs = match rusb::devices() {
        Ok(d) => d,
        Err(_) => return,
    };
    for device in devs.iter() {
        let desc = match device.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        if desc.vendor_id() == 0x0bda && matches!(desc.product_id(), 0x2832 | 0x2838) {
            if let Ok(handle) = device.open() {
                match handle.detach_kernel_driver(0) {
                    Ok(()) => info!("kernel driver detached for RTL2832U (usbip-safe open)"),
                    Err(rusb::Error::NotFound) => {} // no kernel driver attached — fine
                    Err(e) => warn!(error = %e, "detach_kernel_driver failed (continuing)"),
                }
                // handle dropped here: no claim_interface, no reattach — detach persists
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn detach_rtl_kernel_drivers() {}

/// Gain configuration for SDR tuner.
/// Auto: AGC mode (tuner controls gain).
/// Manual(i32): Fixed gain in dB.
#[derive(Debug, Clone)]
pub enum GainSetting {
    Auto,
    Manual(i32),
}

impl GainSetting {
    /// Parse from config i32: <=0 means Auto, >0 means Manual.
    pub fn from_config(gain: i32) -> Self {
        if gain <= 0 {
            GainSetting::Auto
        } else {
            GainSetting::Manual(gain)
        }
    }
}

/// Stored configuration for an opened SDR device, used for reopen/reset.
#[derive(Debug, Clone)]
pub struct SdrDeviceConfig {
    pub sample_rate: u32,
    pub gain: GainSetting,
    pub freq_correction_ppm: i32,
}

/// Individual SDR device handle.
pub struct SdrDevice {
    device: RtlSdr,
    index: u32,
    serial: String,
    config: SdrDeviceConfig,
    last_tuned_freq_hz: Option<u64>,
}

impl SdrDevice {
    pub fn serial(&self) -> &str {
        &self.serial
    }

    #[allow(dead_code)]
    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    pub fn config(&self) -> &SdrDeviceConfig {
        &self.config
    }

    pub fn last_tuned_freq_hz(&self) -> Option<u64> {
        self.last_tuned_freq_hz
    }

    pub fn set_last_tuned_freq_hz(&mut self, freq_hz: u64) {
        self.last_tuned_freq_hz = Some(freq_hz);
    }

    /// Set center frequency with u64→u32 range check.
    /// RTL-SDR max frequency is ~1.766 GHz (u32::MAX).
    pub fn set_center_freq(&mut self, freq_hz: u64) -> anyhow::Result<()> {
        if freq_hz > u32::MAX as u64 {
            anyhow::bail!(
                "Frequency {} Hz exceeds u32::MAX — RTL-SDR max is ~1.766 GHz",
                freq_hz
            );
        }
        self.device
            .set_center_freq(freq_hz as u32)
            .map_err(|e| anyhow::Error::new(e).context(format!("set_center_freq({} Hz)", freq_hz)))
    }

    #[allow(dead_code)]
    pub fn set_gain(&mut self, gain: &GainSetting) -> anyhow::Result<()> {
        let tuner_gain = match gain {
            GainSetting::Auto => TunerGain::Auto,
            GainSetting::Manual(g) => TunerGain::Manual(*g),
        };
        self.device
            .set_tuner_gain(tuner_gain)
            .map_err(|e| anyhow::Error::new(e).context("set_tuner_gain"))
    }

    /// Capture raw IQ bytes. num_bytes should be even (2 bytes per sample).
    pub fn capture_raw(&self, num_bytes: usize) -> anyhow::Result<Vec<u8>> {
        if let Err(e) = self.device.reset_buffer() {
            warn!(error = %e, "reset_buffer before read failed");
        }

        let mut buf = vec![0u8; num_bytes];
        let n_read = self
            .device
            .read_sync(&mut buf)
            .map_err(|e| anyhow::Error::new(e).context("read_sync"))?;

        buf.truncate(n_read);
        Ok(buf)
    }

    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        true // If we have a handle, it's connected
    }
}

impl Drop for SdrDevice {
    fn drop(&mut self) {
        info!(index = self.index, serial = %self.serial, "Closing SDR device");
        if let Err(e) = self.device.close() {
            warn!(index = self.index, serial = %self.serial, error = %e, "Failed to close SDR device cleanly");
        }
    }
}

/// Manages all connected RTL-SDR devices.
pub struct SdrManager {
    devices: Vec<SdrDevice>,
}

impl Drop for SdrManager {
    fn drop(&mut self) {
        let count = self.devices.len();
        info!(count, "Dropping SDR manager and connected devices");
        self.devices.clear();
    }
}

impl SdrManager {
    /// Enumerate and open RTL-SDR devices.
    /// If device_index is Some(n), open only that device.
    /// If None, open all detected devices.
    pub fn new(device_index: Option<u32>, sample_rate: u32, gain: i32) -> Self {
        // Windows driver install check
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
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        }

        let count = match RtlSdr::get_device_count() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "Failed to enumerate SDR devices");
                return Self {
                    devices: Vec::new(),
                };
            }
        };
        info!(count, "RTL-SDR devices found");

        if count == 0 {
            warn!("No RTL-SDR devices found — running in stub mode");
            return Self {
                devices: Vec::new(),
            };
        }

        let gain_setting = GainSetting::from_config(gain);
        let mut devices = Vec::new();

        let indices: Vec<u32> = match device_index {
            Some(idx) => vec![idx],
            None => (0..count).map(|i| i as u32).collect(),
        };

        for idx in indices {
            match Self::open_device(idx, sample_rate, &gain_setting, 0) {
                Ok(dev) => {
                    info!(index = idx, serial = %dev.serial, "SDR device opened");
                    devices.push(dev);
                }
                Err(e) => {
                    warn!(index = idx, error = %e, "Failed to open SDR device");
                }
            }
        }

        if devices.is_empty() {
            warn!("No SDR devices successfully opened — running in stub mode");
        }

        Self { devices }
    }

    fn open_device(
        index: u32,
        sample_rate: u32,
        gain: &GainSetting,
        freq_correction_ppm: i32,
    ) -> anyhow::Result<SdrDevice> {
        // Linux/usbip: detach the kernel dvb_usb_rtl28xxu driver before opening, or
        // rtl-sdr-rs's open() fails ("open device N"). No-op on Windows.
        detach_rtl_kernel_drivers();
        let mut device = RtlSdr::open_with_index(index as usize)
            .map_err(|e| anyhow::Error::new(e).context(format!("open device {}", index)))?;

        device
            .set_sample_rate(sample_rate)
            .map_err(|e| anyhow::Error::new(e).context(format!("set_sample_rate({} Hz)", sample_rate)))?;

        device
            .set_freq_correction(freq_correction_ppm)
            .map_err(|e| anyhow::Error::new(e).context(format!("set_freq_correction({} ppm)", freq_correction_ppm)))?;

        let tuner_gain = match gain {
            GainSetting::Auto => TunerGain::Auto,
            GainSetting::Manual(g) => TunerGain::Manual(*g),
        };
        device
            .set_tuner_gain(tuner_gain)
            .map_err(|e| anyhow::Error::new(e).context("set_tuner_gain"))?;

        device
            .reset_buffer()
            .map_err(|e| anyhow::Error::new(e).context("reset_buffer"))?;

        // Generate serial: use index as fallback (rtl-sdr-rs v0.3 doesn't expose USB strings)
        let serial = format!("SDR-{:04}", index);

        Ok(SdrDevice {
            device,
            index,
            serial,
            config: SdrDeviceConfig {
                sample_rate,
                gain: gain.clone(),
                freq_correction_ppm,
            },
            last_tuned_freq_hz: None,
        })
    }

    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    #[allow(dead_code)]
    pub fn devices(&self) -> &[SdrDevice] {
        &self.devices
    }

    pub fn device(&self, index: usize) -> Option<&SdrDevice> {
        self.devices.get(index)
    }

    pub fn device_mut(&mut self, index: usize) -> Option<&mut SdrDevice> {
        self.devices.get_mut(index)
    }

    pub fn device_serials(&self) -> Vec<String> {
        self.devices.iter().map(|d| d.serial.clone()).collect()
    }

    pub fn is_connected(&self) -> bool {
        !self.devices.is_empty()
    }

    /// Close a device handle without reopening. Used by L2 recovery to release
    /// the WinUSB handle before rusb can open its own for a bus reset.
    /// Returns the stored config so the caller can reopen later.
    pub fn close_device(&mut self, index: usize) -> Option<(usize, SdrDeviceConfig, String)> {
        let pos = self
            .devices
            .iter()
            .position(|d| d.index == index as u32)?;

        let config = self.devices[pos].config().clone();
        let serial = self.devices[pos].serial().to_string();

        let old_device = self.devices.remove(pos);
        drop(old_device);

        info!(index, serial = %serial, "SDR device closed for recovery");
        Some((pos, config, serial))
    }

    /// Reopen a previously closed device at the given vec position.
    pub fn reopen_device_at(&mut self, index: usize, pos: usize, config: &SdrDeviceConfig) -> anyhow::Result<()> {
        let serial = format!("SDR-{:04}", index);
        info!(index, serial = %serial, "Reopening SDR device");
        std::thread::sleep(std::time::Duration::from_millis(500));

        let new_device = Self::open_device(
            index as u32,
            config.sample_rate,
            &config.gain,
            config.freq_correction_ppm,
        )
        .map_err(|e| e.context(format!("Reopen device {}", index)))?;

        self.devices.insert(pos, new_device);
        info!(index, serial = %serial, "SDR device reopened successfully");
        Ok(())
    }

    /// Close, wait, and reopen a device using its stored configuration.
    /// The old handle is dropped (calling its `Drop::drop`). If reopen fails,
    /// the device is removed from the manager so the scanner skips it.
    pub fn reopen_device(&mut self, index: usize) -> anyhow::Result<()> {
        let pos = self
            .devices
            .iter()
            .position(|d| d.index == index as u32)
            .ok_or_else(|| anyhow::anyhow!("Device index {} not found for reopen", index))?;

        let config = self.devices[pos].config().clone();
        let serial = self.devices[pos].serial().to_string();

        let old_device = self.devices.remove(pos);
        drop(old_device);

        info!(index, serial = %serial, "Reopening SDR device after repeated pipe errors");
        std::thread::sleep(std::time::Duration::from_millis(500));

        let new_device = Self::open_device(
            index as u32,
            config.sample_rate,
            &config.gain,
            config.freq_correction_ppm,
        )
        .map_err(|e| e.context(format!("Reopen device {}", index)))?;

        self.devices.insert(pos, new_device);
        info!(index, serial = %serial, "SDR device reopened successfully");
        Ok(())
    }
}

/// Calculate signal power from raw IQ bytes.
/// Returns (dBFS, dBm) tuple.
/// dbm_offset: calibration offset (typically -30.0).
/// Zero-power guard: if mean_power <= 1e-20, returns (-999.0, -999.0).
pub fn calculate_signal_power(iq_bytes: &[u8], dbm_offset: f32) -> (f64, f64) {
    let n_samples = iq_bytes.len() / 2;
    if n_samples == 0 {
        return (-999.0, -999.0);
    }

    let mut power_sum = 0.0f64;
    for i in 0..n_samples {
        let iq_i = (iq_bytes[2 * i] as f64 - 127.5) / 127.5;
        let iq_q = (iq_bytes[2 * i + 1] as f64 - 127.5) / 127.5;
        power_sum += iq_i * iq_i + iq_q * iq_q;
    }

    let mean_power = power_sum / n_samples as f64;

    // Zero-power guard: avoid log10 on tiny/zero values
    if mean_power <= 1e-20 {
        return (-999.0, -999.0);
    }

    let db_fs = 10.0 * mean_power.log10();
    let dbm = db_fs + dbm_offset as f64;
    (db_fs, dbm)
}

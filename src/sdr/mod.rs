#[cfg(target_os = "windows")]
pub mod driver_install;

pub mod driver_state;
pub mod dtv;
pub mod fm_demod;
pub mod holders;
pub mod lte;
pub mod rds;
pub mod scanner;
pub mod recovery;

use rtl_sdr_rs::{RtlSdr, TunerGain};
use tracing::{info, warn};
use crate::identity;
use driver_state::{DriverInfo, DriverStateKind, OpenFailure};

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

/// "[R82xx] PLL not locked!" records seen from rtl-sdr-rs (see `logging`).
pub static PLL_LOCK_FAILS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Frequency of the most recent tune, for diagnostics that fire from a log hook.
pub static CURRENT_TUNE_HZ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn pll_lock_failures() -> u64 {
    PLL_LOCK_FAILS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Unit tests must not touch whatever dongle happens to be plugged into the
/// machine running them: they install a fake hardware answer here instead.
#[cfg(test)]
pub static TEST_HW: std::sync::Mutex<Option<(DriverInfo, Result<String, OpenFailure>)>> =
    std::sync::Mutex::new(None);

/// Current driver state of Interface 0 of the attached RTL2832U.
pub fn current_driver_info() -> DriverInfo {
    #[cfg(test)]
    if let Some((info, _)) = TEST_HW.lock().unwrap().as_ref() {
        return info.clone();
    }
    #[cfg(feature = "qa-fault-injection")]
    if let Some(injected) = driver_state::driver_fault_from_env() {
        return injected;
    }
    #[cfg(target_os = "windows")]
    {
        driver_install::query_driver_info()
    }
    #[cfg(not(target_os = "windows"))]
    {
        // No driver installation is needed off Windows; just report presence.
        let present = rusb::devices().map(|l| {
            l.iter().any(|d| {
                d.device_descriptor()
                    .map(|desc| driver_state::is_known_device(desc.vendor_id(), desc.product_id()))
                    .unwrap_or(false)
            })
        });
        match present {
            Ok(true) => DriverInfo::not_applicable(),
            Ok(false) => DriverInfo::new(DriverStateKind::NotFound, "No RTL-SDR is connected."),
            Err(e) => DriverInfo::new(DriverStateKind::CheckFailed, format!("USB enumeration failed: {e}")),
        }
    }
}

/// Open device `index` just long enough to read the tuner name, then close it.
/// This is a REAL open (never a cached flag).
pub fn probe_open(index: u32) -> Result<String, OpenFailure> {
    #[cfg(test)]
    if let Some((_, open)) = TEST_HW.lock().unwrap().as_ref() {
        return open.clone();
    }
    detach_rtl_kernel_drivers();
    let mut dev = RtlSdr::open_with_index(index as usize)
        .map_err(|e| driver_state::parse_open_error(&format!("{e}")))?;
    let tuner = dev.get_tuner_id().map(|t| t.to_string()).unwrap_or_else(|_| "unknown".to_string());
    let _ = dev.close();
    Ok(tuner)
}

/// Elevated driver install (`--install-driver`). Windows only.
pub fn run_install_driver(
    vid: Option<u16>,
    pid: Option<u16>,
    interface: u8,
) -> (driver_state::InstallResult, Vec<driver_state::InstallDeviceReport>) {
    #[cfg(target_os = "windows")]
    {
        driver_install::install_cli(vid, pid, interface)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (vid, pid, interface);
        (driver_state::InstallResult::NotApplicable, Vec::new())
    }
}

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

/// Convert our `GainSetting` (plain dB — matches `sdr.gain` in config.toml,
/// which `Config::validate()` bounds at "max ~50 dB") into rtl-sdr-rs's
/// `TunerGain`. `TunerGain::Manual` expects gain in TENTHS of a dB: confirmed
/// against rtl-sdr-rs 0.3.3's R820T gain table (`tuners/r82xx.rs`), whose
/// `GAINS` entries top out at 496 (== 49.6 dB) and whose `set_gain` walks
/// `R82XX_LNA_GAIN_STEPS` / `R82XX_MIXER_GAIN_STEPS` — both tenths-of-dB
/// tables — accumulating until `total_gain >= gain`. Passing a plain-dB value
/// straight through (the previous behavior) silently divided real gain by
/// 10x: a configured 25 dB became 2.5 dB at the tuner, starving sensitivity
/// enough that FM/LTE/DTV sweeps found nothing even with a healthy antenna.
fn tuner_gain_from_setting(gain: &GainSetting) -> TunerGain {
    match gain {
        GainSetting::Auto => TunerGain::Auto,
        GainSetting::Manual(db) => TunerGain::Manual(db.saturating_mul(10)),
    }
}

/// Generate machine-scoped SDR serial number from fingerprint and index.
/// Format: SDR-{fingerprint_prefix}-{index:04}
/// Example: SDR-a1b2c3d4-0001
fn generate_sdr_serial(index: u32) -> String {
    let fp = identity::machine_fingerprint();
    let prefix = fp.chars().take(8).collect::<String>();
    format!("SDR-{}-{:04}", prefix, index)
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

    /// Tuner chip name ("R820T", "R828D", ...), read from the open handle.
    pub fn tuner_id(&self) -> Option<String> {
        self.device.get_tuner_id().ok().map(|t| t.to_string())
    }

    /// Prove the open handle still streams: read a small IQ block.
    pub fn live_check(&self) -> bool {
        self.capture_raw(4096).map(|b| !b.is_empty()).unwrap_or(false)
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
        CURRENT_TUNE_HZ.store(freq_hz, std::sync::atomic::Ordering::Relaxed);
        let before = pll_lock_failures();
        self.device
            .set_center_freq(freq_hz as u32)
            .map_err(|e| anyhow::Error::new(e).context(format!("set_center_freq({} Hz)", freq_hz)))?;
        if pll_lock_failures() > before {
            // The tuner reported "PLL not locked" (rtl-sdr-rs still returns
            // success). One short settle and re-tune; the failure stays counted
            // in /status.sdr_diagnostics so a frequency that never locks shows.
            tracing::debug!(freq_hz, "tuner reported PLL not locked; settling and re-tuning once");
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = self.device.set_center_freq(freq_hz as u32);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn set_gain(&mut self, gain: &GainSetting) -> anyhow::Result<()> {
        self.device
            .set_tuner_gain(tuner_gain_from_setting(gain))
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
    /// A manager with no devices, used until the first real open completes.
    pub fn empty() -> Self {
        Self { devices: Vec::new() }
    }

    /// Enumerate and open RTL-SDR devices.
    /// If device_index is Some(n), open only that device.
    /// If None, open all detected devices.
    pub fn new(device_index: Option<u32>, sample_rate: u32, gain: i32) -> Self {
        // Driver installation needs administrator rights and is never attempted
        // here: the unelevated daemon only reports the state (`/status.driver`);
        // `azimuth-daemon --install-driver` does the elevated install.
        let drv = current_driver_info();
        if !matches!(drv.state, DriverStateKind::Ok | DriverStateKind::NotApplicable | DriverStateKind::NotFound) {
            warn!(state = ?drv.state, message = %drv.message, "RTL-SDR driver is not usable");
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

        device
            .set_tuner_gain(tuner_gain_from_setting(gain))
            .map_err(|e| anyhow::Error::new(e).context("set_tuner_gain"))?;

        device
            .reset_buffer()
            .map_err(|e| anyhow::Error::new(e).context("reset_buffer"))?;

        // Generate serial: machine-scoped with fingerprint prefix for unique identification
        let serial = generate_sdr_serial(index);

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

    /// Android OTG path: open the SDR from a USB file descriptor provided by
    /// the app's USB Host integration (libusb `wrap_sys_device` under
    /// `RtlSdr::open_with_fd`). Enumeration never happens — Android's SELinux
    /// policy blocks /dev/bus/usb scans — so index-based recovery reopen is
    /// unavailable in this mode; an unplug invalidates the fd and the app
    /// restarts the pipeline on the next permission grant.
    /// `serial_seed` is the device fingerprint from Kotlin (ANDROID_ID hash),
    /// keeping the reported SDR serial stable per phone.
    #[cfg(target_os = "android")]
    pub fn new_from_fd(
        fd: i32,
        sample_rate: u32,
        gain: i32,
        serial_seed: &str,
    ) -> anyhow::Result<Self> {
        let gain_setting = GainSetting::from_config(gain);

        let mut device = RtlSdr::open_with_fd(fd)
            .map_err(|e| anyhow::Error::new(e).context(format!("open device from fd {}", fd)))?;

        device
            .set_sample_rate(sample_rate)
            .map_err(|e| anyhow::Error::new(e).context(format!("set_sample_rate({} Hz)", sample_rate)))?;

        device
            .set_freq_correction(0)
            .map_err(|e| anyhow::Error::new(e).context("set_freq_correction(0 ppm)"))?;

        device
            .set_tuner_gain(tuner_gain_from_setting(&gain_setting))
            .map_err(|e| anyhow::Error::new(e).context("set_tuner_gain"))?;

        device
            .reset_buffer()
            .map_err(|e| anyhow::Error::new(e).context("reset_buffer"))?;

        let prefix: String = serial_seed.chars().take(8).collect();
        let serial = format!("SDR-{}-otg0", prefix);
        info!(fd, serial = %serial, "SDR device opened from USB fd");

        Ok(Self {
            devices: vec![SdrDevice {
                device,
                index: 0,
                serial,
                config: SdrDeviceConfig {
                    sample_rate,
                    gain: gain_setting,
                    freq_correction_ppm: 0,
                },
                last_tuned_freq_hz: None,
            }],
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
        let serial = generate_sdr_serial(index as u32);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_sdr_serial_deterministic() {
        // Serial should be deterministic for the same index on the same machine
        let serial1 = generate_sdr_serial(0);
        let serial2 = generate_sdr_serial(0);
        assert_eq!(serial1, serial2);
    }

    #[test]
    fn test_generate_sdr_serial_unique_per_index() {
        // Different indices should produce different serials
        let serial0 = generate_sdr_serial(0);
        let serial1 = generate_sdr_serial(1);
        let serial42 = generate_sdr_serial(42);

        assert_ne!(serial0, serial1);
        assert_ne!(serial1, serial42);
        assert_ne!(serial0, serial42);
    }

    #[test]
    fn test_generate_sdr_serial_format() {
        // Verify format: SDR-{fingerprint_prefix}-{index:04}
        let serial = generate_sdr_serial(123);

        // Should start with SDR-
        assert!(serial.starts_with("SDR-"));

        // Should end with -0123 for index 123
        assert!(serial.ends_with("-0123"));

        // Should be under 32 characters (alnum + dashes)
        assert!(serial.len() < 32);

        // Should only contain alphanumeric and dashes
        assert!(serial.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn tuner_gain_from_setting_converts_configured_db_to_tenths_of_db() {
        // Regression test for the FM/LTE/DTV "zero signals" bug: rtl-sdr-rs's
        // TunerGain::Manual expects tenths of a dB (its R820T gain table tops
        // out at 496 == 49.6 dB), but our config's sdr.gain is documented and
        // validated as plain dB (0-50). A configured 25 dB gain must reach the
        // tuner as 250 (tenths), not 25 (which the tuner would read as 2.5 dB).
        match tuner_gain_from_setting(&GainSetting::Manual(25)) {
            TunerGain::Manual(tenths) => assert_eq!(tenths, 250),
            other => panic!("expected TunerGain::Manual, got {:?}", other),
        }
    }

    #[test]
    fn tuner_gain_from_setting_max_gain_stays_within_gain_table_bounds() {
        // The UI's manual-gain slider tops out at 49.6 dB (Step4SdrDevice.jsx),
        // matching rtl-sdr-rs's real max gain step. Config validation rejects
        // sdr.gain > 50, so 50 is the worst-case integer input.
        match tuner_gain_from_setting(&GainSetting::Manual(50)) {
            TunerGain::Manual(tenths) => assert_eq!(tenths, 500),
            other => panic!("expected TunerGain::Manual, got {:?}", other),
        }
    }

    #[test]
    fn tuner_gain_from_setting_auto_passes_through() {
        match tuner_gain_from_setting(&GainSetting::Auto) {
            TunerGain::Auto => {}
            other => panic!("expected TunerGain::Auto, got {:?}", other),
        }
    }
}

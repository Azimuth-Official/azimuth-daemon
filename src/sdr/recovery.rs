//! SDR USB pipe-error recovery escalation ladder.
//!
//! Level 0: preserve typed RTL-SDR / libusb errors in the anyhow chain.
//! Level 1: close and reopen the RTL-SDR device handle (`SdrManager::reopen_device`).
//! Level 2: reset the USB bus for the device via a separate `rusb` handle, then reopen.
//! Level 3: restart the OS USB device (Windows PnP restart / Linux sysfs authorized toggle).
//! Level 4: terminate the daemon process so the service manager can restart it cleanly.

use std::process::Command;
use tracing::{info, warn};

/// Maximum consecutive pipe errors before triggering a recovery action.
const PIPE_ERROR_THRESHOLD: u32 = 3;
/// Maximum failures at a given level before escalating to the next level.
const LEVEL_FAILURE_THRESHOLD: u32 = 3;

/// Recovery action produced by [`RecoveryManager`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecoveryAction {
    /// Level 1: close and reopen the RTL-SDR device handle.
    ReopenHandle,
    /// Level 2: reset the USB bus for the device, then reopen the handle.
    UsbBusReset,
    /// Level 3: restart the OS USB device.
    OsDeviceRestart,
    /// Level 4: terminate the process with the supplied exit code.
    ProcessExit(i32),
}

/// Tracks pipe-error frequency and recovery-level failures, and decides when to escalate.
#[derive(Debug, Clone, Default)]
pub struct RecoveryManager {
    consecutive_pipe_errors: u32,
    level1_failures: u32,
    level2_failures: u32,
    level3_failures: u32,
    os_device_id: Option<String>,
}

impl RecoveryManager {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn with_os_device_id(os_device_id: Option<String>) -> Self {
        Self {
            os_device_id,
            ..Self::default()
        }
    }

    pub fn os_device_id(&self) -> Option<&str> {
        self.os_device_id.as_deref()
    }

    #[allow(dead_code)]
    pub fn set_os_device_id(&mut self, id: Option<String>) {
        self.os_device_id = id;
    }

    /// Call when a new USB pipe error is observed. Returns the action to execute
    /// when the threshold is hit.
    pub fn record_pipe_error(&mut self) -> Option<RecoveryAction> {
        self.consecutive_pipe_errors += 1;
        info!(
            consecutive_pipe_errors = self.consecutive_pipe_errors,
            threshold = PIPE_ERROR_THRESHOLD,
            "SDR pipe error recorded"
        );

        if self.consecutive_pipe_errors >= PIPE_ERROR_THRESHOLD {
            self.consecutive_pipe_errors = 0;
            info!("Pipe-error threshold reached; escalating to Level 1 (reopen handle)");
            return Some(RecoveryAction::ReopenHandle);
        }

        None
    }

    /// Call after any recovery action succeeds to reset all counters.
    pub fn record_recovery_success(&mut self) {
        if self.consecutive_pipe_errors != 0
            || self.level1_failures != 0
            || self.level2_failures != 0
            || self.level3_failures != 0
        {
            info!(
                consecutive_pipe_errors = self.consecutive_pipe_errors,
                level1_failures = self.level1_failures,
                level2_failures = self.level2_failures,
                level3_failures = self.level3_failures,
                "Recovery succeeded; resetting all counters"
            );
        }
        self.consecutive_pipe_errors = 0;
        self.level1_failures = 0;
        self.level2_failures = 0;
        self.level3_failures = 0;
    }

    /// Call when a recovery action at `level` fails. Returns the next action to
    /// execute when the failure threshold is hit.
    pub fn record_recovery_failure(&mut self, level: RecoveryAction) -> Option<RecoveryAction> {
        match level {
            RecoveryAction::ReopenHandle => {
                self.level1_failures += 1;
                warn!(
                    level1_failures = self.level1_failures,
                    threshold = LEVEL_FAILURE_THRESHOLD,
                    "Level 1 recovery (reopen handle) failed"
                );
                if self.level1_failures >= LEVEL_FAILURE_THRESHOLD {
                    self.level1_failures = 0;
                    info!("Level 1 failure threshold reached; escalating to Level 2 (USB bus reset)");
                    return Some(RecoveryAction::UsbBusReset);
                }
            }
            RecoveryAction::UsbBusReset => {
                self.level2_failures += 1;
                warn!(
                    level2_failures = self.level2_failures,
                    threshold = LEVEL_FAILURE_THRESHOLD,
                    "Level 2 recovery (USB bus reset) failed"
                );
                if self.level2_failures >= LEVEL_FAILURE_THRESHOLD {
                    self.level2_failures = 0;
                    info!("Level 2 failure threshold reached; escalating to Level 3 (OS device restart)");
                    return Some(RecoveryAction::OsDeviceRestart);
                }
            }
            RecoveryAction::OsDeviceRestart => {
                self.level3_failures += 1;
                warn!(
                    level3_failures = self.level3_failures,
                    threshold = LEVEL_FAILURE_THRESHOLD,
                    "Level 3 recovery (OS device restart) failed"
                );
                if self.level3_failures >= LEVEL_FAILURE_THRESHOLD {
                    self.level3_failures = 0;
                    info!(
                        "Level 3 failure threshold reached; escalating to Level 4 (process exit)"
                    );
                    return Some(RecoveryAction::ProcessExit(42));
                }
            }
            RecoveryAction::ProcessExit(code) => {
                // Already terminal; propagate unchanged.
                return Some(RecoveryAction::ProcessExit(code));
            }
        }

        None
    }
}

/// Open a separate `rusb` handle to the Nth RTL2832U device (`VID=0x0BDA`,
/// `PID=0x2832`) and perform a USB bus reset. This does not touch the
/// rtl-sdr-rs handle, so the caller must reopen the device afterwards.
pub fn reset_usb_bus_device(device_index: usize) -> anyhow::Result<()> {
    let vid = 0x0BDA;
    let pid = 0x2832;

    let mut matched = 0usize;
    for device in rusb::devices()?.iter() {
        let desc = device.device_descriptor()?;
        if desc.vendor_id() == vid && desc.product_id() == pid {
            if matched == device_index {
                info!(
                    device_index,
                    bus = device.bus_number(),
                    addr = device.address(),
                    "Resetting USB bus for RTL-SDR device"
                );
                let handle = device.open()?;
                handle.reset()?;
                info!(device_index, "USB bus reset completed");
                return Ok(());
            }
            matched += 1;
        }
    }

    anyhow::bail!(
        "No RTL-SDR device at index {} (VID={:04X}, PID={:04X})",
        device_index,
        vid,
        pid
    );
}

/// Restart the OS-level USB device. On Windows this disables then re-enables
/// the PnP device via PowerShell cmdlets (harder reset than pnputil).
/// On Linux it toggles the sysfs `authorized` flag for the matching VID/PID.
pub fn restart_os_device(os_device_id: Option<&str>, _device_index: usize) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        let instance_id = match os_device_id {
            Some(id) => id.to_string(),
            None => discover_windows_pnp_instance_id()?,
        };
        info!(instance_id, "Disabling Windows PnP device for hard reset");

        // Disable device
        let disable = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Disable-PnpDevice -InstanceId '{}' -Confirm:$false -ErrorAction Stop",
                    instance_id
                ),
            ])
            .output()?;
        if !disable.status.success() {
            let stdout = String::from_utf8_lossy(&disable.stdout);
            let stderr = String::from_utf8_lossy(&disable.stderr);
            anyhow::bail!(
                "Disable-PnpDevice failed: stdout={} stderr={}",
                stdout.trim(),
                stderr.trim()
            );
        }
        info!(instance_id, "Device disabled, waiting before re-enable");
        std::thread::sleep(std::time::Duration::from_millis(2000));

        // Re-enable device
        let enable = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Enable-PnpDevice -InstanceId '{}' -Confirm:$false -ErrorAction Stop",
                    instance_id
                ),
            ])
            .output()?;
        if !enable.status.success() {
            let stdout = String::from_utf8_lossy(&enable.stdout);
            let stderr = String::from_utf8_lossy(&enable.stderr);
            anyhow::bail!(
                "Enable-PnpDevice failed: stdout={} stderr={}",
                stdout.trim(),
                stderr.trim()
            );
        }
        info!(instance_id, "Windows PnP device re-enabled successfully");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        let sysfs_path = match os_device_id {
            Some(id) => std::path::PathBuf::from(id),
            None => discover_linux_sysfs_path(_device_index)?,
        };
        info!(?sysfs_path, "Restarting Linux USB device via sysfs");
        std::fs::write(sysfs_path.join("authorized"), b"0")?;
        std::thread::sleep(std::time::Duration::from_millis(500));
        std::fs::write(sysfs_path.join("authorized"), b"1")?;
        info!(?sysfs_path, "Linux USB device restart completed");
        return Ok(());
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        anyhow::bail!("OS device restart not implemented for this platform")
    }
}

#[cfg(target_os = "windows")]
fn discover_windows_pnp_instance_id() -> anyhow::Result<String> {
    info!("Discovering Windows PnP instance ID for RTL-SDR (VID_0BDA&PID_2832)");
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-PnpDevice | Where-Object { $_.InstanceId -like '*VID_0BDA&PID_2832*' } | Select-Object -ExpandProperty InstanceId",
        ])
        .output()?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "PowerShell PnP discovery failed: stdout={} stderr={}",
            stdout.trim(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let instance_id = stdout
        .lines()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("No RTL-SDR PnP device found"))?;

    info!(instance_id, "Discovered Windows PnP instance ID");
    Ok(instance_id.to_string())
}

#[cfg(target_os = "linux")]
fn discover_linux_sysfs_path(device_index: usize) -> anyhow::Result<std::path::PathBuf> {
    info!(device_index, "Discovering Linux sysfs path for RTL-SDR");
    let base = std::path::PathBuf::from("/sys/bus/usb/devices");
    let mut matched = 0usize;
    for entry in std::fs::read_dir(&base)? {
        let entry = entry?;
        let path = entry.path();
        let id_vendor = path.join("idVendor");
        let id_product = path.join("idProduct");
        if !id_vendor.exists() || !id_product.exists() {
            continue;
        }
        let vendor = std::fs::read_to_string(&id_vendor)?;
        let product = std::fs::read_to_string(&id_product)?;
        if vendor.trim().eq_ignore_ascii_case("0bda") && product.trim().eq_ignore_ascii_case("2832") {
            if matched == device_index {
                info!(?path, "Discovered Linux sysfs path");
                return Ok(path);
            }
            matched += 1;
        }
    }
    anyhow::bail!(
        "No RTL-SDR sysfs entry found at index {} (VID=0BDA, PID=2832)",
        device_index
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_escalation_ladder() {
        let mut rm = RecoveryManager::new();

        // Level 0 -> Level 1: three consecutive pipe errors trigger reopen.
        assert_eq!(rm.record_pipe_error(), None);
        assert_eq!(rm.record_pipe_error(), None);
        assert_eq!(
            rm.record_pipe_error(),
            Some(RecoveryAction::ReopenHandle)
        );

        // Level 1 -> Level 2: three failed reopen attempts trigger USB bus reset.
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::ReopenHandle),
            None
        );
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::ReopenHandle),
            None
        );
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::ReopenHandle),
            Some(RecoveryAction::UsbBusReset)
        );

        // Level 2 -> Level 3: three failed bus resets trigger OS device restart.
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::UsbBusReset),
            None
        );
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::UsbBusReset),
            None
        );
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::UsbBusReset),
            Some(RecoveryAction::OsDeviceRestart)
        );

        // Level 3 -> Level 4: three failed OS restarts trigger process exit.
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::OsDeviceRestart),
            None
        );
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::OsDeviceRestart),
            None
        );
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::OsDeviceRestart),
            Some(RecoveryAction::ProcessExit(42))
        );

        // Level 4 is terminal; the manager propagates it unchanged.
        assert_eq!(
            rm.record_recovery_failure(RecoveryAction::ProcessExit(42)),
            Some(RecoveryAction::ProcessExit(42))
        );
    }

    #[test]
    fn test_recovery_success_resets_all_counters() {
        let mut rm = RecoveryManager::new();

        // Partially build up every counter.
        rm.record_pipe_error();
        rm.record_pipe_error();
        rm.record_recovery_failure(RecoveryAction::ReopenHandle);
        rm.record_recovery_failure(RecoveryAction::UsbBusReset);
        rm.record_recovery_failure(RecoveryAction::OsDeviceRestart);

        // Success should wipe the slate clean.
        rm.record_recovery_success();

        // We should need the full threshold again, not a single remaining failure.
        assert_eq!(rm.record_pipe_error(), None);
        assert_eq!(rm.record_pipe_error(), None);
        assert_eq!(rm.record_pipe_error(), Some(RecoveryAction::ReopenHandle));
    }

    #[test]
    fn test_pipe_error_threshold_constant() {
        // Guard against accidental constant changes that would break the ladder contract.
        assert_eq!(PIPE_ERROR_THRESHOLD, 3);
        assert_eq!(LEVEL_FAILURE_THRESHOLD, 3);
    }
}

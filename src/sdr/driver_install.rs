//! WinUSB driver detection and auto-installation for RTL2832U devices.
//!
//! Detection uses libwdi (via wdi-rs) to enumerate USB devices and check if the
//! RTL2832U has a working driver. Installation uses libwdi's driver preparation
//! and install flow — the same approach used by Zadig — which handles self-signed
//! certificate creation, catalog signing, and driver package installation.

use serde::Serialize;
use tracing::{error, info, warn};
use wdi_rs::{create_list, CreateListOptions, DriverInstaller, DriverType};

const RTL2832U_VID: u16 = 0x0BDA;
const RTL2832U_PID_V4: u16 = 0x2832; // RTL2832U V4
const RTL2832U_PID_V3: u16 = 0x2838; // RTL2832U V3

/// Helper to check if a PID is a valid RTL2832U variant.
fn is_valid_rtl2832u_pid(pid: u16) -> bool {
    matches!(pid, RTL2832U_PID_V4 | RTL2832U_PID_V3)
}

/// Helper to get a product name for a valid RTL2832U PID.
fn get_rtl2832u_product_name(pid: u16) -> &'static str {
    match pid {
        RTL2832U_PID_V4 => "RTL2832U V4",
        RTL2832U_PID_V3 => "RTL2832U V3",
        _ => "Unknown RTL2832U variant",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverStatus {
    Installed,
    NeedsInstall,
    DeviceNotFound,
    CheckFailed(String),
}

impl std::fmt::Display for DriverStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Installed => write!(f, "installed"),
            Self::NeedsInstall => write!(f, "needs_install"),
            Self::DeviceNotFound => write!(f, "not_found"),
            Self::CheckFailed(e) => write!(f, "check_failed: {e}"),
        }
    }
}

/// Check whether the RTL2832U is present and has a working driver.
pub fn check_driver_status() -> DriverStatus {
    let devices = match create_list(CreateListOptions {
        list_all: true,
        list_hubs: false,
        trim_whitespaces: true,
    }) {
        Ok(list) => list,
        Err(e) => {
            warn!(error = ?e, "libwdi device enumeration failed");
            return DriverStatus::CheckFailed(format!("{e:?}"));
        }
    };

    // Try both PID V4 and V3 in order (prefer V4)
    for pid in &[RTL2832U_PID_V4, RTL2832U_PID_V3] {
        let matches = devices.from_vid_pid(RTL2832U_VID, *pid);
        if let Some(dev) = matches.first() {
            let product_name = if *pid == RTL2832U_PID_V3 { "V3" } else { "V4" };
            if let Some(ref driver) = dev.driver {
                info!(driver, pid = ?pid, product = product_name, "RTL2832U has driver installed");
                return DriverStatus::Installed;
            } else {
                info!(desc = ?dev.desc, pid = ?pid, product = product_name, "RTL2832U found without driver");
                return DriverStatus::NeedsInstall;
            }
        }
    }

    info!("No RTL2832U device found (VID_{RTL2832U_VID:04X} PID_{{0x{:04X} or 0x{:04X}}})", RTL2832U_PID_V4, RTL2832U_PID_V3);
    DriverStatus::DeviceNotFound
}

/// Check driver status and install WinUSB if needed.
/// Returns final status after any installation attempt.
pub fn ensure_driver() -> DriverStatus {
    let status = check_driver_status();
    if status != DriverStatus::NeedsInstall {
        return status;
    }

    info!("Attempting WinUSB driver installation for RTL2832U via libwdi");

    // Try both PID V4 and V3 in order (prefer V4)
    for (pid, product_name) in &[(RTL2832U_PID_V4, "V4"), (RTL2832U_PID_V3, "V3")] {
        match DriverInstaller::for_device(RTL2832U_VID, *pid)
            .with_driver_type(DriverType::WinUsb)
            .install()
        {
            Ok(dev) => {
                info!(desc = ?dev.desc, pid = ?pid, product = product_name, "WinUSB driver installed successfully");
                return DriverStatus::Installed;
            }
            Err(e) => {
                warn!(pid = ?pid, product = product_name, error = ?e, "WinUSB driver installation failed for this PID, trying next");
            }
        }
    }

    error!("WinUSB driver installation failed for all RTL2832U variants");
    DriverStatus::CheckFailed("install failed for all PID variants".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_rtl2832u_pid_v4() {
        assert!(is_valid_rtl2832u_pid(RTL2832U_PID_V4));
    }

    #[test]
    fn test_is_valid_rtl2832u_pid_v3() {
        assert!(is_valid_rtl2832u_pid(RTL2832U_PID_V3));
    }

    #[test]
    fn test_is_valid_rtl2832u_pid_invalid() {
        assert!(!is_valid_rtl2832u_pid(0x1234));
        assert!(!is_valid_rtl2832u_pid(0x0000));
        assert!(!is_valid_rtl2832u_pid(0xFFFF));
    }

    #[test]
    fn test_get_rtl2832u_product_name_v4() {
        assert_eq!(get_rtl2832u_product_name(RTL2832U_PID_V4), "RTL2832U V4");
    }

    #[test]
    fn test_get_rtl2832u_product_name_v3() {
        assert_eq!(get_rtl2832u_product_name(RTL2832U_PID_V3), "RTL2832U V3");
    }

    #[test]
    fn test_get_rtl2832u_product_name_invalid() {
        assert_eq!(
            get_rtl2832u_product_name(0x1234),
            "Unknown RTL2832U variant"
        );
    }
}

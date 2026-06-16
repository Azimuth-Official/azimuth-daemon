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
const RTL2832U_PID: u16 = 0x2832;

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

    let matches = devices.from_vid_pid(RTL2832U_VID, RTL2832U_PID);
    match matches.first() {
        None => {
            info!("No RTL2832U device found (VID_{RTL2832U_VID:04X} PID_{RTL2832U_PID:04X})");
            DriverStatus::DeviceNotFound
        }
        Some(dev) => {
            if let Some(ref driver) = dev.driver {
                info!(driver, "RTL2832U has driver installed");
                DriverStatus::Installed
            } else {
                info!(desc = ?dev.desc, "RTL2832U found without driver");
                DriverStatus::NeedsInstall
            }
        }
    }
}

/// Check driver status and install WinUSB if needed.
/// Returns final status after any installation attempt.
pub fn ensure_driver() -> DriverStatus {
    let status = check_driver_status();
    if status != DriverStatus::NeedsInstall {
        return status;
    }

    info!("Attempting WinUSB driver installation for RTL2832U via libwdi");

    match DriverInstaller::for_device(RTL2832U_VID, RTL2832U_PID)
        .with_driver_type(DriverType::WinUsb)
        .install()
    {
        Ok(dev) => {
            info!(desc = ?dev.desc, "WinUSB driver installed successfully");
            DriverStatus::Installed
        }
        Err(e) => {
            error!(error = ?e, "WinUSB driver installation failed");
            DriverStatus::CheckFailed(format!("install failed: {e:?}"))
        }
    }
}

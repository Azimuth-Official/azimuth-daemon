//! Windows driver detection and the elevated install mode for RTL2832U devices.
//!
//! Detection reads the driver bound to Interface 0 (MI_00) through libwdi.
//! Installation happens ONLY in `azimuth-daemon --install-driver`, which the
//! desktop app launches elevated (UAC) and the installer runs while it is
//! already elevated. The normal, unelevated sidecar never installs anything:
//! it reports `driver_missing` / `wrong_driver` and lets the app ask for admin
//! approval.
//!
//! libwdi's `wdi_prepare_driver` generates a self-signed certificate and
//! catalog for the WinUSB package, and `wdi_install_driver` adds that
//! certificate to the machine's trusted publishers: documented in the release
//! notes.

use std::time::{Duration, Instant};

use tracing::{info, warn};
use wdi_rs::{
    create_list, install_driver, prepare_driver, CreateListOptions, Device, DriverType,
    InstallDriverOptions, PrepareDriverOptions,
};

use super::driver_state::{
    classify_nodes, is_known_device, DriverInfo, DriverStateKind, InstallDeviceReport,
    InstallResult, UsbNode,
};

fn list_devices() -> Result<Vec<Device>, String> {
    create_list(CreateListOptions { list_all: true, list_hubs: false, trim_whitespaces: true })
        .map(|l| l.iter().collect())
        .map_err(|e| format!("{e:?}"))
}

fn to_nodes(devices: &[Device]) -> Vec<UsbNode> {
    devices
        .iter()
        .filter(|d| is_known_device(d.vid, d.pid))
        .map(|d| UsbNode {
            vid: d.vid,
            pid: d.pid,
            mi: if d.is_composite { Some(d.mi) } else { None },
            is_composite: d.is_composite,
            driver: d.driver.clone(),
        })
        .collect()
}

/// Current state of Interface 0 of every attached supported RTL2832U.
pub fn query_driver_info() -> DriverInfo {
    match list_devices() {
        Ok(devices) => classify_nodes(&to_nodes(&devices)),
        Err(e) => {
            warn!(error = %e, "libwdi device enumeration failed");
            DriverInfo::new(DriverStateKind::CheckFailed, format!("Device enumeration failed: {e}"))
        }
    }
}

/// True when the process token is an administrator's (elevated).
pub fn is_elevated() -> bool {
    // SAFETY: IsUserAnAdmin has no preconditions.
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// Interface-0 nodes of every attached supported device (optionally narrowed
/// to one VID:PID) that still need WinUSB.
fn targets(devices: &[Device], vid: Option<u16>, pid: Option<u16>, interface: u8) -> Vec<Device> {
    devices
        .iter()
        .filter(|d| is_known_device(d.vid, d.pid))
        .filter(|d| vid.is_none_or(|v| d.vid == v) && pid.is_none_or(|p| d.pid == p))
        .filter(|d| if d.is_composite { d.mi == interface } else { true })
        .cloned()
        .collect()
}

fn bound_is_usable(d: &Device) -> bool {
    d.driver
        .as_deref()
        .map(super::driver_state::bound_driver_name)
        .is_some_and(|(_, usable)| usable)
}

fn work_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("azimuth-wdi-{}", std::process::id()))
}

/// Bind WinUSB to the given interface of one device, replacing whatever
/// driver is there. Needs elevation.
fn install_one(device: &Device) -> Result<(), String> {
    let dir = work_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let dir_s = dir.to_string_lossy().to_string();
    let inf = format!("rtl2832u_if{}.inf", device.mi);
    let prep = PrepareDriverOptions { driver_type: DriverType::WinUsb, ..Default::default() };
    let result = prepare_driver(device, &dir_s, &inf, &prep)
        .and_then(|()| install_driver(device, &dir_s, &inf, &InstallDriverOptions::default()))
        .map_err(|e| format!("{e:?}"));
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// `--install-driver`: returns the overall result and a per-device report.
pub fn install_cli(
    vid: Option<u16>,
    pid: Option<u16>,
    interface: u8,
) -> (InstallResult, Vec<InstallDeviceReport>) {
    let devices = match list_devices() {
        Ok(d) => d,
        Err(e) => return (InstallResult::Other(format!("device enumeration failed: {e}")), vec![]),
    };
    let wanted = targets(&devices, vid, pid, interface);
    if wanted.is_empty() {
        return (InstallResult::NoDevice, vec![]);
    }
    let pending: Vec<&Device> = wanted.iter().filter(|d| !bound_is_usable(d)).collect();
    let before_info = classify_nodes(&to_nodes(&devices));
    if pending.is_empty() {
        let reports = wanted
            .iter()
            .map(|d| InstallDeviceReport::new(d.vid, d.pid, interface, &before_info, &before_info))
            .collect();
        return (InstallResult::AlreadyOk, reports);
    }
    if !is_elevated() {
        return (InstallResult::NeedsElevation, vec![]);
    }

    let mut first_error: Option<String> = None;
    for d in &pending {
        info!(vid = d.vid, pid = d.pid, mi = d.mi, "Binding WinUSB");
        if let Err(e) = install_one(d) {
            warn!(error = %e, "WinUSB install failed for a device");
            if e.contains("NeedsAdmin") || e.contains("Access") {
                return (InstallResult::NeedsElevation, vec![]);
            }
            first_error.get_or_insert(e);
        }
    }

    // Re-enumeration: the new binding appears a few seconds later.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut after = devices.clone();
    loop {
        if let Ok(now) = list_devices() {
            after = now;
        }
        let done = targets(&after, vid, pid, interface).iter().all(bound_is_usable);
        if done || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let after_info = classify_nodes(&to_nodes(&after));
    let reports: Vec<InstallDeviceReport> = pending
        .iter()
        .map(|d| InstallDeviceReport::new(d.vid, d.pid, interface, &before_info, &after_info))
        .collect();
    let all_ok = targets(&after, vid, pid, interface).iter().all(bound_is_usable);
    if all_ok {
        (InstallResult::Ok, reports)
    } else {
        (
            InstallResult::InstallFailed(
                first_error.unwrap_or_else(|| "the device did not re-enumerate with WinUSB".to_string()),
            ),
            reports,
        )
    }
}

//! RTL2832U driver state, shared by `/status`, `/sdr/test`, `/sdr/list` and the
//! elevated `--install-driver` mode.
//!
//! Everything here is plain logic with no OS calls, so it is unit-tested on
//! every platform. The Windows enumeration lives in `driver_install`.

use serde::Serialize;

/// Every VID:PID rtl-sdr-rs 0.3.3 accepts (`KNOWN_DEVICES` in
/// `src/device/constants.rs`; the table is private there, so it is mirrored
/// here). Re-check on any rtl-sdr-rs upgrade.
pub const KNOWN_DEVICES: [(u16, u16, &str); 42] = [
    (0x0bda, 0x2832, "Generic RTL2832U"),
    (0x0bda, 0x2838, "Generic RTL2832U OEM"),
    (0x0413, 0x6680, "DigitalNow Quad DVB-T PCI-E card"),
    (0x0413, 0x6f0f, "Leadtek WinFast DTV Dongle mini D"),
    (0x0458, 0x707f, "Genius TVGo DVB-T03 USB dongle (Ver. B)"),
    (0x0ccd, 0x00a9, "Terratec Cinergy T Stick Black (rev 1)"),
    (0x0ccd, 0x00b3, "Terratec NOXON DAB/DAB+ USB dongle (rev 1)"),
    (0x0ccd, 0x00b4, "Terratec Deutschlandradio DAB Stick"),
    (0x0ccd, 0x00b5, "Terratec NOXON DAB Stick - Radio Energy"),
    (0x0ccd, 0x00b7, "Terratec Media Broadcast DAB Stick"),
    (0x0ccd, 0x00b8, "Terratec BR DAB Stick"),
    (0x0ccd, 0x00b9, "Terratec WDR DAB Stick"),
    (0x0ccd, 0x00c0, "Terratec MuellerVerlag DAB Stick"),
    (0x0ccd, 0x00c6, "Terratec Fraunhofer DAB Stick"),
    (0x0ccd, 0x00d3, "Terratec Cinergy T Stick RC (Rev.3)"),
    (0x0ccd, 0x00d7, "Terratec T Stick PLUS"),
    (0x0ccd, 0x00e0, "Terratec NOXON DAB/DAB+ USB dongle (rev 2)"),
    (0x1554, 0x5020, "PixelView PV-DT235U(RN)"),
    (0x15f4, 0x0131, "Astrometa DVB-T/DVB-T2"),
    (0x15f4, 0x0133, "HanfTek DAB+FM+DVB-T"),
    (0x185b, 0x0620, "Compro Videomate U620F"),
    (0x185b, 0x0650, "Compro Videomate U650F"),
    (0x185b, 0x0680, "Compro Videomate U680F"),
    (0x1b80, 0xd393, "GIGABYTE GT-U7300"),
    (0x1b80, 0xd394, "DIKOM USB-DVBT HD"),
    (0x1b80, 0xd395, "Peak 102569AGPK"),
    (0x1b80, 0xd397, "KWorld KW-UB450-T USB DVB-T Pico TV"),
    (0x1b80, 0xd398, "Zaapa ZT-MINDVBZP"),
    (0x1b80, 0xd39d, "SVEON STV20 DVB-T USB & FM"),
    (0x1b80, 0xd3a4, "Twintech UT-40"),
    (0x1b80, 0xd3a8, "ASUS U3100MINI_PLUS_V2"),
    (0x1b80, 0xd3af, "SVEON STV27 DVB-T USB & FM"),
    (0x1b80, 0xd3b0, "SVEON STV21 DVB-T USB & FM"),
    (0x1d19, 0x1101, "Dexatek DK DVB-T Dongle (Logilink VG0002A)"),
    (0x1d19, 0x1102, "Dexatek DK DVB-T Dongle (MSI DigiVox mini II V3.0)"),
    (0x1d19, 0x1103, "Dexatek Technology Ltd. DK 5217 DVB-T Dongle"),
    (0x1d19, 0x1104, "MSI DigiVox Micro HD"),
    (0x1f4d, 0xa803, "Sweex DVB-T USB"),
    (0x1f4d, 0xb803, "GTek T803"),
    (0x1f4d, 0xc803, "Lifeview LV5TDeluxe"),
    (0x1f4d, 0xd286, "MyGica TD312"),
    (0x1f4d, 0xd803, "PROlectrix DV107669"),
];

pub fn is_known_device(vid: u16, pid: u16) -> bool {
    KNOWN_DEVICES.iter().any(|(v, p, _)| *v == vid && *p == pid)
}

/// Marketing-neutral name for the table entry. Blog V3 and V4 both enumerate
/// as 0BDA:2838, so the PID alone never says which model it is.
pub fn device_label(vid: u16, pid: u16) -> String {
    match KNOWN_DEVICES.iter().find(|(v, p, _)| *v == vid && *p == pid) {
        Some((0x0bda, 0x2838, _)) | Some((0x0bda, 0x2832, _)) => "RTL-SDR (RTL2832U)".to_string(),
        Some((_, _, d)) => (*d).to_string(),
        None => format!("USB device {vid:04X}:{pid:04X}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverStateKind {
    Ok,
    DriverMissing,
    WrongDriver,
    /// Reserved for the desktop app after an install attempt exit code 3.
    NeedsAdmin,
    NotFound,
    NotApplicable,
    CheckFailed,
}

/// `/status.driver`: state of Interface 0 (MI_00) of the attached RTL2832U.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DriverInfo {
    pub state: DriverStateKind,
    pub interface: u8,
    pub vid: Option<String>,
    pub pid: Option<String>,
    pub bound_driver: Option<String>,
    pub message: String,
}

impl DriverInfo {
    pub fn new(state: DriverStateKind, message: impl Into<String>) -> Self {
        Self {
            state,
            interface: 0,
            vid: None,
            pid: None,
            bound_driver: None,
            message: message.into(),
        }
    }

    pub fn not_applicable() -> Self {
        Self::new(
            DriverStateKind::NotApplicable,
            "No driver installation is needed on this platform.",
        )
    }

    /// The legacy `driver_status` string: same values the old `{:?}` of the
    /// Windows-only enum produced, so existing consumers keep working.
    pub fn legacy_status(&self) -> String {
        match self.state {
            DriverStateKind::Ok => "Installed".to_string(),
            DriverStateKind::DriverMissing | DriverStateKind::WrongDriver | DriverStateKind::NeedsAdmin => {
                "NeedsInstall".to_string()
            }
            DriverStateKind::NotFound => "DeviceNotFound".to_string(),
            DriverStateKind::NotApplicable => "not_applicable".to_string(),
            DriverStateKind::CheckFailed => format!("CheckFailed({:?})", self.message),
        }
    }
}

/// One USB device node as enumerated by the OS. A composite device (the
/// RTL2832U is one: V2 shows two "Bulk-In, Interface" entries) appears as a
/// parent plus one child per interface (`mi` = Some).
#[derive(Debug, Clone, PartialEq)]
pub struct UsbNode {
    pub vid: u16,
    pub pid: u16,
    pub mi: Option<u8>,
    pub is_composite: bool,
    pub driver: Option<String>,
}

/// Canonical name for a bound service, and whether it is one we can use.
pub fn bound_driver_name(service: &str) -> (String, bool) {
    match service.to_ascii_lowercase().as_str() {
        "winusb" => ("WinUSB".to_string(), true),
        "libusbk" => ("libusbK".to_string(), true),
        "libusb0" => ("libusb0".to_string(), true),
        _ => (service.to_string(), false),
    }
}

fn classify_device(vid: u16, pid: u16, interface0: Option<&UsbNode>) -> DriverInfo {
    let mut info = DriverInfo::new(DriverStateKind::DriverMissing, "");
    info.vid = Some(format!("{vid:04x}"));
    info.pid = Some(format!("{pid:04x}"));
    match interface0.and_then(|n| n.driver.as_deref()).filter(|d| !d.is_empty()) {
        None => {
            info.message = format!(
                "Windows has no driver for {} (Interface 0).",
                device_label(vid, pid)
            );
        }
        Some(svc) => {
            let (name, usable) = bound_driver_name(svc);
            info.bound_driver = Some(name.clone());
            if usable {
                info.state = DriverStateKind::Ok;
                info.message = format!("Interface 0 uses {name}.");
            } else {
                info.state = DriverStateKind::WrongDriver;
                info.message = format!("Interface 0 uses {name}; Azimuth needs WinUSB.");
            }
        }
    }
    info
}

/// Judge Interface 0 of every attached RTL2832U. The composite PARENT (bound
/// to `usbccgp`) is not Interface 0 and is never judged, which is also what
/// fixes the old `.first()` pick of whichever node enumerated first.
/// With several dongles the most actionable result wins.
pub fn classify_nodes(nodes: &[UsbNode]) -> DriverInfo {
    let mut devices: Vec<(u16, u16)> = Vec::new();
    for n in nodes.iter().filter(|n| is_known_device(n.vid, n.pid)) {
        if !devices.contains(&(n.vid, n.pid)) {
            devices.push((n.vid, n.pid));
        }
    }
    if devices.is_empty() {
        return DriverInfo::new(DriverStateKind::NotFound, "No RTL-SDR is connected.");
    }
    let mut best: Option<DriverInfo> = None;
    for (vid, pid) in devices {
        let same: Vec<&UsbNode> = nodes.iter().filter(|n| n.vid == vid && n.pid == pid).collect();
        let composite = same.iter().any(|n| n.is_composite || n.mi.is_some());
        let iface0 = if composite {
            same.iter().find(|n| n.mi == Some(0)).copied()
        } else {
            same.first().copied()
        };
        let info = classify_device(vid, pid, iface0);
        let rank = |i: &DriverInfo| match i.state {
            DriverStateKind::WrongDriver => 2,
            DriverStateKind::DriverMissing => 1,
            _ => 0,
        };
        if best.as_ref().is_none_or(|b| rank(&info) > rank(b)) {
            best = Some(info);
        }
    }
    best.expect("at least one device")
}

// ---------------------------------------------------------------------------
// --install-driver result reporting (contract C6)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum InstallResult {
    Ok,
    AlreadyOk,
    NoDevice,
    NeedsElevation,
    InstallFailed(String),
    NotApplicable,
    Other(String),
}

impl InstallResult {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Ok | Self::AlreadyOk => 0,
            Self::NoDevice => 2,
            Self::NeedsElevation => 3,
            Self::InstallFailed(_) => 4,
            Self::NotApplicable | Self::Other(_) => 5,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::AlreadyOk => "already_ok",
            Self::NoDevice => "no_device",
            Self::NeedsElevation => "needs_elevation",
            Self::InstallFailed(_) => "install_failed",
            Self::NotApplicable => "not_applicable",
            Self::Other(_) => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstallDeviceReport {
    pub vid: String,
    pub pid: String,
    pub interface: u8,
    pub before: String,
    pub after: String,
}

impl InstallDeviceReport {
    pub fn new(vid: u16, pid: u16, interface: u8, before: &DriverInfo, after: &DriverInfo) -> Self {
        let describe = |i: &DriverInfo| match (&i.bound_driver, i.state) {
            (Some(d), _) => d.clone(),
            (None, DriverStateKind::DriverMissing) => "none".to_string(),
            (None, s) => format!("{s:?}").to_ascii_lowercase(),
        };
        Self {
            vid: format!("{vid:04x}"),
            pid: format!("{pid:04x}"),
            interface,
            before: describe(before),
            after: describe(after),
        }
    }
}

/// The single JSON line `--install-driver` prints on stdout.
pub fn install_report_json(result: &InstallResult, devices: &[InstallDeviceReport]) -> String {
    let mut v = serde_json::json!({ "result": result.label(), "devices": devices });
    match result {
        InstallResult::InstallFailed(m) | InstallResult::Other(m) => {
            v["detail"] = serde_json::Value::String(m.clone());
        }
        _ => {}
    }
    v.to_string()
}

// ---------------------------------------------------------------------------
// /sdr/test classification (contract C5)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum OpenFailure {
    Access,
    Busy,
    NotFound,
    Other(String),
}

/// rtl-sdr-rs reports open failures as `RtlsdrErr("Error: <rusb::Error:?>")`,
/// so the rusb variant name is the only code available.
pub fn parse_open_error(text: &str) -> OpenFailure {
    if text.contains("Access") {
        OpenFailure::Access
    } else if text.contains("Busy") {
        OpenFailure::Busy
    } else if text.contains("NoDevice") || text.contains("NotFound") || text.contains("No device found") {
        OpenFailure::NotFound
    } else {
        let code = text
            .rsplit(':')
            .next()
            .unwrap_or(text)
            .trim()
            .trim_matches('"')
            .to_string();
        OpenFailure::Other(if code.is_empty() { "unknown".to_string() } else { code })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SdrTestState {
    Ok,
    DriverMissing,
    WrongDriver,
    InUse,
    NotFound,
    UsbError,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SdrTestVerdict {
    pub state: SdrTestState,
    pub detail: String,
    pub tuner: Option<String>,
}

/// Decide the `/sdr/test` outcome from the driver state and an open attempt.
/// `access_means_in_use`: on Windows a second open of an exclusive WinUSB
/// device fails with "Access"; on Linux "Access" is a udev permission problem.
pub fn classify_test(
    driver: &DriverInfo,
    open: Result<String, OpenFailure>,
    access_means_in_use: bool,
) -> SdrTestVerdict {
    let verdict = |state, detail: String| SdrTestVerdict { state, detail, tuner: None };
    match driver.state {
        DriverStateKind::NotFound => {
            return verdict(SdrTestState::NotFound, driver.message.clone());
        }
        DriverStateKind::DriverMissing | DriverStateKind::NeedsAdmin => {
            return verdict(SdrTestState::DriverMissing, driver.message.clone());
        }
        DriverStateKind::WrongDriver => {
            return verdict(SdrTestState::WrongDriver, driver.message.clone());
        }
        _ => {}
    }
    match open {
        Ok(tuner) => SdrTestVerdict {
            state: SdrTestState::Ok,
            detail: format!("Opened the RTL-SDR (tuner {tuner})."),
            tuner: Some(tuner),
        },
        Err(OpenFailure::Busy) => verdict(
            SdrTestState::InUse,
            "The device is busy: another program holds it.".to_string(),
        ),
        Err(OpenFailure::Access) if access_means_in_use => verdict(
            SdrTestState::InUse,
            "Access to the device was refused: another program is using it.".to_string(),
        ),
        Err(OpenFailure::Access) => verdict(
            SdrTestState::UsbError,
            "access_denied: the operating system refused access to the USB device (permissions).".to_string(),
        ),
        Err(OpenFailure::NotFound) => verdict(
            SdrTestState::NotFound,
            "The RTL-SDR disappeared while opening it.".to_string(),
        ),
        Err(OpenFailure::Other(code)) => verdict(SdrTestState::UsbError, format!("usb_error: {code}")),
    }
}

// ---------------------------------------------------------------------------
// QA fault injection: compiled ONLY into test builds
// ---------------------------------------------------------------------------

/// Present in the binary only when built with `--features qa-fault-injection`,
/// so a release build can be proven free of the injection path by searching
/// for this string.
#[cfg(feature = "qa-fault-injection")]
pub const QA_FAULT_MARKER: &str = "AZ_QA_FAULT_INJECTION_ACTIVE";

/// Parse a fault spec such as `wrong_driver:usbser` or `driver_missing`.
#[cfg(any(test, feature = "qa-fault-injection"))]
pub fn parse_driver_fault(spec: &str) -> Option<DriverInfo> {
    let (kind, arg) = match spec.split_once(':') {
        Some((k, a)) => (k, Some(a)),
        None => (spec, None),
    };
    let mut info = match kind {
        "wrong_driver" => {
            let svc = arg.unwrap_or("usbser");
            let mut i = classify_device(
                0x0bda,
                0x2838,
                Some(&UsbNode { vid: 0x0bda, pid: 0x2838, mi: Some(0), is_composite: true, driver: Some(svc.to_string()) }),
            );
            i.state = DriverStateKind::WrongDriver;
            i
        }
        "driver_missing" => classify_device(0x0bda, 0x2838, None),
        _ => return None,
    };
    info.message = format!("[QA fault injection] {}", info.message);
    Some(info)
}

#[cfg(feature = "qa-fault-injection")]
pub fn driver_fault_from_env() -> Option<DriverInfo> {
    std::hint::black_box(QA_FAULT_MARKER);
    std::env::var("AZIMUTH_QA_DRIVER_FAULT").ok().and_then(|s| parse_driver_fault(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(mi: Option<u8>, composite: bool, driver: Option<&str>) -> UsbNode {
        UsbNode { vid: 0x0bda, pid: 0x2838, mi, is_composite: composite, driver: driver.map(str::to_string) }
    }

    #[test]
    fn known_device_table_matches_rtl_sdr_rs_0_3_3() {
        assert_eq!(KNOWN_DEVICES.len(), 42);
        let mut seen = std::collections::HashSet::new();
        for (v, p, _) in KNOWN_DEVICES {
            assert!(seen.insert((v, p)), "duplicate {v:04x}:{p:04x}");
        }
        assert!(is_known_device(0x0bda, 0x2832) && is_known_device(0x0bda, 0x2838));
        assert!(is_known_device(0x1d19, 0x1104));
        assert!(!is_known_device(0x04e8, 0x6860));
    }

    #[test]
    fn composite_parent_usbccgp_is_not_judged_interface0_missing_driver() {
        // V2: two "Bulk-In, Interface" entries with no driver, parent on usbccgp.
        let nodes = [node(None, true, Some("usbccgp")), node(Some(0), true, None), node(Some(1), true, None)];
        let i = classify_nodes(&nodes);
        assert_eq!(i.state, DriverStateKind::DriverMissing);
        assert_eq!((i.vid.as_deref(), i.pid.as_deref()), (Some("0bda"), Some("2838")));
        assert_eq!(i.interface, 0);
    }

    #[test]
    fn interface0_on_winusb_is_ok_even_if_interface1_is_unbound() {
        // The old code looked at whichever node came first.
        let nodes = [node(Some(1), true, None), node(Some(0), true, Some("WINUSB"))];
        let i = classify_nodes(&nodes);
        assert_eq!(i.state, DriverStateKind::Ok);
        assert_eq!(i.bound_driver.as_deref(), Some("WinUSB"));
    }

    #[test]
    fn other_usable_libusb_variants_are_ok() {
        for (svc, name) in [("libusbK", "libusbK"), ("libusb0", "libusb0"), ("LIBUSBK", "libusbK")] {
            let i = classify_nodes(&[node(Some(0), true, Some(svc))]);
            assert_eq!(i.state, DriverStateKind::Ok, "{svc}");
            assert_eq!(i.bound_driver.as_deref(), Some(name));
        }
    }

    #[test]
    fn a_non_winusb_driver_is_wrong_driver_not_installed() {
        // HEAD counted ANY bound driver as Installed.
        let i = classify_nodes(&[node(Some(0), true, Some("usbser"))]);
        assert_eq!(i.state, DriverStateKind::WrongDriver);
        assert_eq!(i.bound_driver.as_deref(), Some("usbser"));
        assert!(i.message.contains("WinUSB"));
    }

    #[test]
    fn non_composite_device_is_judged_directly() {
        assert_eq!(classify_nodes(&[node(None, false, Some("WinUSB"))]).state, DriverStateKind::Ok);
        assert_eq!(classify_nodes(&[node(None, false, None)]).state, DriverStateKind::DriverMissing);
    }

    #[test]
    fn composite_without_an_interface0_entry_is_missing() {
        let i = classify_nodes(&[node(None, true, Some("usbccgp")), node(Some(1), true, Some("WinUSB"))]);
        assert_eq!(i.state, DriverStateKind::DriverMissing);
    }

    #[test]
    fn unknown_vid_pid_and_empty_list_are_not_found() {
        assert_eq!(classify_nodes(&[]).state, DriverStateKind::NotFound);
        let other = UsbNode { vid: 0x04e8, pid: 0x6860, mi: None, is_composite: false, driver: None };
        assert_eq!(classify_nodes(&[other]).state, DriverStateKind::NotFound);
    }

    #[test]
    fn a_supported_non_default_pid_is_detected() {
        let n = UsbNode { vid: 0x1d19, pid: 0x1104, mi: None, is_composite: false, driver: None };
        let i = classify_nodes(&[n]);
        assert_eq!(i.state, DriverStateKind::DriverMissing);
        assert_eq!(i.pid.as_deref(), Some("1104"));
    }

    #[test]
    fn worst_device_wins_with_two_dongles() {
        let good = node(Some(0), true, Some("WinUSB"));
        let bad = UsbNode { vid: 0x0bda, pid: 0x2832, mi: Some(0), is_composite: true, driver: Some("usbser".into()) };
        assert_eq!(classify_nodes(&[good, bad]).state, DriverStateKind::WrongDriver);
    }

    #[test]
    fn legacy_status_strings_are_preserved() {
        let ok = classify_nodes(&[node(Some(0), true, Some("WinUSB"))]);
        assert_eq!(ok.legacy_status(), "Installed");
        assert_eq!(classify_nodes(&[node(Some(0), true, None)]).legacy_status(), "NeedsInstall");
        assert_eq!(classify_nodes(&[]).legacy_status(), "DeviceNotFound");
    }

    #[test]
    fn install_exit_codes_follow_the_contract() {
        assert_eq!(InstallResult::Ok.exit_code(), 0);
        assert_eq!(InstallResult::AlreadyOk.exit_code(), 0);
        assert_eq!(InstallResult::NoDevice.exit_code(), 2);
        assert_eq!(InstallResult::NeedsElevation.exit_code(), 3);
        assert_eq!(InstallResult::InstallFailed("x".into()).exit_code(), 4);
        assert_eq!(InstallResult::NotApplicable.exit_code(), 5);
        assert_eq!(InstallResult::Other("x".into()).exit_code(), 5);
    }

    #[test]
    fn install_report_is_one_json_line_with_the_contract_shape() {
        let before = classify_nodes(&[node(Some(0), true, None)]);
        let after = classify_nodes(&[node(Some(0), true, Some("WinUSB"))]);
        let rep = InstallDeviceReport::new(0x0bda, 0x2838, 0, &before, &after);
        let line = install_report_json(&InstallResult::Ok, &[rep]);
        assert!(!line.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["result"], "ok");
        assert_eq!(v["devices"][0]["vid"], "0bda");
        assert_eq!(v["devices"][0]["pid"], "2838");
        assert_eq!(v["devices"][0]["interface"], 0);
        assert_eq!(v["devices"][0]["before"], "none");
        assert_eq!(v["devices"][0]["after"], "WinUSB");
    }

    #[test]
    fn open_error_text_maps_to_failure_kinds() {
        assert_eq!(parse_open_error("open device 0: Error: Access"), OpenFailure::Access);
        assert_eq!(parse_open_error("Error: Busy"), OpenFailure::Busy);
        assert_eq!(parse_open_error("Error: NoDevice"), OpenFailure::NotFound);
        assert_eq!(parse_open_error("No device found at index 3"), OpenFailure::NotFound);
        assert_eq!(parse_open_error("Error: Io"), OpenFailure::Other("Io".to_string()));
    }

    fn ok_driver() -> DriverInfo {
        classify_nodes(&[node(Some(0), true, Some("WinUSB"))])
    }

    #[test]
    fn test_categories_cover_the_contract() {
        let missing = classify_nodes(&[node(Some(0), true, None)]);
        let wrong = classify_nodes(&[node(Some(0), true, Some("usbser"))]);
        let none = classify_nodes(&[]);
        assert_eq!(classify_test(&missing, Ok("R828D".into()), true).state, SdrTestState::DriverMissing);
        assert_eq!(classify_test(&wrong, Ok("R828D".into()), true).state, SdrTestState::WrongDriver);
        assert_eq!(classify_test(&none, Err(OpenFailure::NotFound), true).state, SdrTestState::NotFound);
        let ok = classify_test(&ok_driver(), Ok("R828D".into()), true);
        assert_eq!((ok.state, ok.tuner.as_deref()), (SdrTestState::Ok, Some("R828D")));
        assert_eq!(classify_test(&ok_driver(), Err(OpenFailure::Busy), true).state, SdrTestState::InUse);
        assert_eq!(classify_test(&ok_driver(), Err(OpenFailure::Access), true).state, SdrTestState::InUse);
        assert_eq!(classify_test(&ok_driver(), Err(OpenFailure::Access), false).state, SdrTestState::UsbError);
        let usb = classify_test(&ok_driver(), Err(OpenFailure::Other("Io".into())), true);
        assert_eq!(usb.state, SdrTestState::UsbError);
        assert!(usb.detail.contains("Io"));
    }

    #[test]
    fn a_driverless_device_is_never_reported_as_in_use() {
        // The R2 bug: every failure read "Device unavailable - it may be in use".
        let missing = classify_nodes(&[node(Some(0), true, None)]);
        let v = classify_test(&missing, Err(OpenFailure::Access), true);
        assert_eq!(v.state, SdrTestState::DriverMissing);
    }

    #[test]
    fn fault_spec_parsing() {
        let w = parse_driver_fault("wrong_driver:usbser").unwrap();
        assert_eq!(w.state, DriverStateKind::WrongDriver);
        assert_eq!(w.bound_driver.as_deref(), Some("usbser"));
        assert_eq!(parse_driver_fault("driver_missing").unwrap().state, DriverStateKind::DriverMissing);
        assert!(parse_driver_fault("bogus").is_none());
    }

    #[test]
    fn fault_injection_feature_is_never_a_default_feature() {
        let manifest = include_str!("../../Cargo.toml");
        let default_line = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("default ="))
            .expect("default features line");
        assert!(!default_line.contains("qa-fault-injection"), "{default_line}");
        assert!(manifest.contains("qa-fault-injection = []"));
    }
}

//! The USB side of an RTL2832U: open + full init, stall-tolerant tuning, reset.
//!
//! An RTL2832U can STALL any control transfer at any time ("Pipe error"). That
//! was seen on a generic 0bda:2832 + R820T with rtl-sdr-rs AND with librtlsdr;
//! librtlsdr logs it and carries on, rtl-sdr-rs reports every stall as an error
//! (and `panic!`s when a stall hits its tuner probe during init). Everything the
//! daemon does to a dongle goes through here, so a hardware run of the
//! `tune_stress` example (which includes this file) exercises exactly the
//! daemon's code path.
//!
//! No `unwrap`/`expect`/`panic` on any USB result: every failure is a value.
// Shared with the `tune_stress` example, which uses only part of it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rtl_sdr_rs::error::RtlsdrError;
use rtl_sdr_rs::{RtlSdr, TunerGain};
use tracing::{debug, info, warn};

use super::driver_state::{self, OpenFailure};
use super::stall;

/// Tunes that hit a stall and succeeded after being re-issued.
pub static TUNE_STALLS_RECOVERED: AtomicU64 = AtomicU64::new(0);
/// Every stalled tune re-issue (including those that never recovered).
pub static TUNE_STALL_RETRIES_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Init attempts that stalled and were answered with a USB reset + redo.
pub static INIT_STALL_RESETS: AtomicU64 = AtomicU64::new(0);
/// Reset+reopen cycles run for a channel whose tune kept stalling.
pub static TUNE_LADDER_CYCLES_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Opens that exhausted the whole init ladder.
pub static INIT_FAILURES: AtomicU64 = AtomicU64::new(0);
/// Last reason the device could not be brought up; cleared on success.
pub static DEVICE_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Another process holds the dongle (set when a USB reset or open is refused
/// with Access/Busy; cleared by the next successful open). While set, the
/// daemon neither resets nor steals the device; it reports `in_use` and retries
/// the open.
pub static HELD_BY_OTHER: AtomicBool = AtomicBool::new(false);

/// Windows/WinUSB refuses a second claimer with Access; on Linux Access is a
/// udev permission problem and says nothing about another holder.
pub const ACCESS_MEANS_IN_USE: bool = cfg!(windows);

pub fn held_by_other() -> bool {
    HELD_BY_OTHER.load(Ordering::Relaxed)
}

const IN_USE_PREFIX: &str = "device in use: ";

pub fn in_use_error() -> RtlsdrError {
    RtlsdrError::RtlsdrErr(format!("{IN_USE_PREFIX}another program holds the RTL-SDR"))
}

pub fn is_in_use(e: &RtlsdrError) -> bool {
    matches!(e, RtlsdrError::RtlsdrErr(m) if m.starts_with(IN_USE_PREFIX))
}

pub fn is_in_use_anyhow(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<RtlsdrError>().is_some_and(is_in_use))
}

/// A refused USB reset/open that means "someone else has the device": Busy, or
/// Access where Access means that (`access_means_in_use`).
pub fn reset_failure_means_held(e: &anyhow::Error, access_means_in_use: bool) -> bool {
    e.chain().any(|c| match c.downcast_ref::<rusb::Error>() {
        Some(rusb::Error::Busy) => true,
        Some(rusb::Error::Access) => access_means_in_use,
        _ => false,
    })
}

/// Run a USB reset ONLY while nobody else is known to hold the device. A known
/// holder short-circuits to the in-use error without touching the bus; a reset
/// refused as held sets `flag` and returns the in-use error. Any other failure
/// is returned unchanged.
pub fn guarded_reset_with(
    flag: &AtomicBool,
    access_means_in_use: bool,
    reset: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if flag.load(Ordering::Relaxed) {
        return Err(anyhow::Error::new(in_use_error()));
    }
    match reset() {
        Err(e) if reset_failure_means_held(&e, access_means_in_use) => {
            flag.store(true, Ordering::Relaxed);
            warn!(error = %e, "USB reset refused: another program holds the RTL-SDR; not resetting or reopening it");
            Err(anyhow::Error::new(in_use_error()))
        }
        other => other,
    }
}

fn guarded_reset(index: usize) -> anyhow::Result<()> {
    guarded_reset_with(&HELD_BY_OTHER, ACCESS_MEANS_IN_USE, || usb_reset(index))
}

/// Recovery's reset (`recovery::reset_usb_bus_device`), with the same holder guard.
pub fn guarded_usb_reset(reset: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
    guarded_reset_with(&HELD_BY_OTHER, ACCESS_MEANS_IN_USE, reset)
}

pub fn device_error() -> Option<String> {
    DEVICE_ERROR.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn set_device_error(v: Option<String>) {
    *DEVICE_ERROR.lock().unwrap_or_else(|p| p.into_inner()) = v;
}

/// Tuner gain setting: automatic, or a fixed gain in tenths of a dB.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gain {
    Auto,
    ManualTenthsDb(i32),
}

impl Gain {
    fn tuner_gain(self) -> TunerGain {
        match self {
            Gain::Auto => TunerGain::Auto,
            Gain::ManualTenthsDb(t) => TunerGain::Manual(t),
        }
    }
}

/// Everything needed to bring a dongle from closed to ready to tune.
#[derive(Debug, Clone, Copy)]
pub struct InitParams {
    pub index: u32,
    pub sample_rate: u32,
    pub gain: Gain,
    pub freq_correction_ppm: i32,
}

/// A stall: the USB errors an RTL2832U produces when it stalls a control
/// transfer (`Pipe`) or loses the transaction (`Io`), or a panic that rtl-sdr-rs
/// raised while probing the tuner (its probe swallows the stall that caused it).
pub fn is_rtl_stall(e: &RtlsdrError) -> bool {
    match e {
        RtlsdrError::Usb(rusb::Error::Pipe) | RtlsdrError::Usb(rusb::Error::Io) => true,
        RtlsdrError::RtlsdrErr(m) => m.starts_with(PANIC_PREFIX),
        _ => false,
    }
}

pub fn is_usb_stall(e: &anyhow::Error) -> bool {
    if let Some(r) = e.downcast_ref::<RtlsdrError>() {
        return is_rtl_stall(r);
    }
    e.downcast_ref::<rusb::Error>()
        .is_some_and(|u| matches!(u, rusb::Error::Pipe | rusb::Error::Io))
}

const PANIC_PREFIX: &str = "init panicked: ";

/// Linux: detach the kernel driver (`dvb_usb_rtl28xxu`) that claims the RTL2832U
/// — especially over usbip — before rtl-sdr-rs opens it, or `open()` fails with
/// "open device N". Uses a PLAIN `detach_kernel_driver` via rusb, NOT
/// `set_auto_detach` (auto mode re-attaches the kernel driver when the handle is
/// released, handing the device straight back before rtl-sdr-rs opens it). The
/// handle is dropped without claiming and without reattach, so the detach
/// persists past close. No-op elsewhere (Windows uses WinUSB, no detach needed).
#[cfg(target_os = "linux")]
fn detach_kernel_drivers() {
    let Ok(devs) = rusb::devices() else { return };
    for device in devs.iter() {
        let Ok(desc) = device.device_descriptor() else { continue };
        if super::driver_state::is_known_device(desc.vendor_id(), desc.product_id()) {
            if let Ok(handle) = device.open() {
                match handle.detach_kernel_driver(0) {
                    Ok(()) => info!("kernel driver detached for RTL2832U (usbip-safe open)"),
                    Err(rusb::Error::NotFound) => {} // no kernel driver attached: fine
                    Err(e) => warn!(error = %e, "detach_kernel_driver failed (continuing)"),
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn detach_kernel_drivers() {}

/// `libusb_reset_device` on the Nth supported dongle, through a separate handle
/// (the caller must have dropped its own). Re-enumeration is the caller's wait.
pub fn usb_reset(device_index: usize) -> anyhow::Result<()> {
    let mut matched = 0usize;
    for device in rusb::devices()?.iter() {
        let desc = device.device_descriptor()?;
        if super::driver_state::is_known_device(desc.vendor_id(), desc.product_id()) {
            if matched == device_index {
                info!(
                    device_index,
                    bus = device.bus_number(),
                    addr = device.address(),
                    "Resetting USB bus for RTL-SDR device"
                );
                device.open()?.reset()?;
                info!(device_index, "USB bus reset completed");
                return Ok(());
            }
            matched += 1;
        }
    }
    anyhow::bail!("No RTL-SDR device at index {}", device_index);
}

/// Open device `index` (kernel driver detached first). A panic inside
/// rtl-sdr-rs's init, which is what a stalled tuner probe turns into, becomes an
/// error value instead of taking the process down.
pub fn open_catching(index: u32) -> Result<RtlSdr, RtlsdrError> {
    detach_kernel_drivers();
    match std::panic::catch_unwind(|| RtlSdr::open_with_index(index as usize)) {
        Ok(r) => r,
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(RtlsdrError::RtlsdrErr(format!("{PANIC_PREFIX}{what}")))
        }
    }
}

/// ONE open + full init attempt. A panic inside rtl-sdr-rs's init is converted
/// into an error value (it is what a stalled tuner probe turns into).
fn open_initialized_once(p: &InitParams) -> Result<RtlSdr, RtlsdrError> {
    let mut dev = open_catching(p.index)?;
    dev.set_sample_rate(p.sample_rate)?;
    dev.set_freq_correction(p.freq_correction_ppm)?;
    dev.set_tuner_gain(p.gain.tuner_gain())?;
    dev.reset_buffer()?;
    Ok(dev)
}

/// Open and fully initialise a dongle under the stall ladder: on any stall, drop
/// the device, USB-reset, wait (0.5/1/2/4/8 s) and redo the whole sequence, up
/// to five attempts. A final failure is recorded in `DEVICE_ERROR` (reported by
/// `/status`); nothing here ever panics, and the caller keeps running.
pub fn open_with_ladder(p: &InitParams) -> Result<RtlSdr, RtlsdrError> {
    let result = stall::init_ladder(
        stall::INIT_ATTEMPTS,
        |e| is_rtl_stall(e) && !held_by_other(),
        || {
            INIT_STALL_RESETS.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = guarded_reset(p.index as usize) {
                if !is_in_use_anyhow(&e) {
                    warn!(index = p.index, error = %e, "USB reset before re-init failed");
                }
            }
        },
        std::thread::sleep,
        |attempt| warn!(index = p.index, attempt, "RTL-SDR stalled during open/init; resetting and redoing the init"),
        || open_initialized_once(p),
    );
    // A refused open (Busy, or Access on Windows) or a stall whose reset was
    // refused means another program holds the dongle.
    let result = result.map_err(|e| {
        let refused = match driver_state::parse_open_error(&e.to_string()) {
            OpenFailure::Busy => true,
            OpenFailure::Access => ACCESS_MEANS_IN_USE,
            _ => false,
        };
        if is_in_use(&e) || held_by_other() || refused {
            HELD_BY_OTHER.store(true, Ordering::Relaxed);
            in_use_error()
        } else {
            e
        }
    });
    match &result {
        Ok(_) => {
            HELD_BY_OTHER.store(false, Ordering::Relaxed);
            set_device_error(None)
        }
        Err(e) if is_in_use(e) => set_device_error(Some(format!("device {} is in use by another program", p.index))),
        Err(e) => {
            INIT_FAILURES.fetch_add(1, Ordering::Relaxed);
            set_device_error(Some(format!("device {} did not initialise: {e}", p.index)));
        }
    }
    result
}

/// Recovery's first level: drop the old handle (the caller did), USB-reset
/// FIRST, wait for re-enumeration, then the full ladder open.
pub fn reset_and_reopen(p: &InitParams) -> Result<RtlSdr, RtlsdrError> {
    std::thread::sleep(Duration::from_millis(200));
    match guarded_reset(p.index as usize) {
        Err(e) if is_in_use_anyhow(&e) => return Err(in_use_error()),
        Err(e) => warn!(index = p.index, error = %e, "USB reset before reopen failed; reopening anyway"),
        Ok(()) => {}
    }
    std::thread::sleep(Duration::from_millis(500));
    open_with_ladder(p)
}

/// Tune, re-issuing the whole tune (repeater switching included) when a control
/// transfer stalls. Only a stall that outlasts the retries is returned.
pub fn tune(dev: &mut RtlSdr, freq_hz: u32) -> Result<(), RtlsdrError> {
    let stalled = std::cell::Cell::new(0u32);
    let result = stall::retry_stalls(
        stall::TUNE_STALL_RETRIES,
        is_rtl_stall,
        std::thread::sleep,
        |attempt| {
            stalled.set(attempt);
            TUNE_STALL_RETRIES_TOTAL.fetch_add(1, Ordering::Relaxed);
            debug!(freq_hz, attempt, "USB control stall while tuning; re-issuing the tune");
        },
        || dev.set_center_freq(freq_hz),
    );
    if result.is_ok() && stalled.get() > 0 {
        TUNE_STALLS_RECOVERED.fetch_add(1, Ordering::Relaxed);
    }
    result
}

/// One channel's tune under the full ladder: an in-place retry (cheap, rarely
/// useful), then up to two USB-reset + full-init cycles that retry the SAME tune
/// before the channel counts as failed. `slot` is the open device; None means it
/// was lost and is reopened first.
pub fn tune_with_ladder(slot: &mut Option<RtlSdr>, p: &InitParams, freq_hz: u32) -> Result<(), RtlsdrError> {
    let cycled = std::cell::Cell::new(0u32);
    let result = stall::tune_ladder(
        slot,
        stall::TUNE_LADDER_CYCLES,
        is_rtl_stall,
        |d| tune(d, freq_hz),
        || reset_and_reopen(p),
        |c| {
            cycled.set(c);
            TUNE_LADDER_CYCLES_TOTAL.fetch_add(1, Ordering::Relaxed);
            warn!(index = p.index, freq_hz, cycle = c, "Tune still stalling; USB reset, reinit, same tune again");
        },
    );
    if result.is_ok() && cycled.get() > 0 {
        TUNE_STALLS_RECOVERED.fetch_add(1, Ordering::Relaxed);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_classification() {
        assert!(is_rtl_stall(&RtlsdrError::Usb(rusb::Error::Pipe)));
        assert!(is_rtl_stall(&RtlsdrError::Usb(rusb::Error::Io)));
        assert!(!is_rtl_stall(&RtlsdrError::Usb(rusb::Error::NoDevice)));
        assert!(!is_rtl_stall(&RtlsdrError::Usb(rusb::Error::Access)));
        assert!(!is_rtl_stall(&RtlsdrError::RtlsdrErr("No device found at index 0".into())));
        // A panic out of rtl-sdr-rs's tuner probe is a stalled init.
        assert!(is_rtl_stall(&RtlsdrError::RtlsdrErr(format!(
            "{PANIC_PREFIX}Failed to find tuner, aborting"
        ))));
    }

    #[test]
    fn anyhow_stalls_are_recognised_through_context() {
        let e = anyhow::Error::new(RtlsdrError::Usb(rusb::Error::Pipe)).context("set_center_freq(1 Hz)");
        assert!(is_usb_stall(&e));
        assert!(!is_usb_stall(&anyhow::anyhow!("something else")));
    }

    #[test]
    fn device_error_is_recorded_and_cleared() {
        set_device_error(Some("x".into()));
        assert_eq!(device_error().as_deref(), Some("x"));
        set_device_error(None);
        assert_eq!(device_error(), None);
    }

    #[test]
    fn refused_reset_means_held_not_a_stall_and_is_never_retried() {
        let flag = AtomicBool::new(false);
        let resets = std::cell::Cell::new(0);
        let refuse = || {
            resets.set(resets.get() + 1);
            Err(anyhow::Error::new(rusb::Error::Access).context("reset"))
        };
        // Windows: Access on reset = held by another process.
        let e = guarded_reset_with(&flag, true, refuse).unwrap_err();
        assert!(is_in_use_anyhow(&e));
        assert!(flag.load(Ordering::Relaxed));
        assert_eq!(resets.get(), 1);
        // Held now: no further reset is even attempted.
        let e = guarded_reset_with(&flag, true, refuse).unwrap_err();
        assert!(is_in_use_anyhow(&e));
        assert_eq!(resets.get(), 1, "no USB reset of a device another process holds");
        // The in-use error is not a stall, so the stall ladder never retries it.
        assert!(!is_rtl_stall(&in_use_error()));
    }

    #[test]
    fn access_on_linux_is_a_permission_problem_and_flaky_usb_keeps_recovery() {
        let flag = AtomicBool::new(false);
        let e = guarded_reset_with(&flag, false, || Err(anyhow::Error::new(rusb::Error::Access))).unwrap_err();
        assert!(!is_in_use_anyhow(&e));
        assert!(!flag.load(Ordering::Relaxed));
        // Pipe/Io/NoDevice are ordinary USB trouble, never "in use".
        for err in [rusb::Error::Pipe, rusb::Error::Io, rusb::Error::NoDevice] {
            let e = guarded_reset_with(&flag, true, || Err(anyhow::Error::new(err))).unwrap_err();
            assert!(!is_in_use_anyhow(&e));
        }
        assert!(!flag.load(Ordering::Relaxed));
        assert!(guarded_reset_with(&flag, true, || Ok(())).is_ok());
        let busy = guarded_reset_with(&flag, false, || Err(anyhow::Error::new(rusb::Error::Busy))).unwrap_err();
        assert!(is_in_use_anyhow(&busy));
    }
}

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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rtl_sdr_rs::error::RtlsdrError;
use rtl_sdr_rs::{RtlSdr, TunerGain};
use tracing::{debug, info, warn};

use super::stall;

/// Tunes that hit a stall and succeeded after being re-issued.
pub static TUNE_STALLS_RECOVERED: AtomicU64 = AtomicU64::new(0);
/// Every stalled tune re-issue (including those that never recovered).
pub static TUNE_STALL_RETRIES_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Init attempts that stalled and were answered with a USB reset + redo.
pub static INIT_STALL_RESETS: AtomicU64 = AtomicU64::new(0);
/// Opens that exhausted the whole init ladder.
pub static INIT_FAILURES: AtomicU64 = AtomicU64::new(0);
/// Last reason the device could not be brought up; cleared on success.
pub static DEVICE_ERROR: Mutex<Option<String>> = Mutex::new(None);

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
        is_rtl_stall,
        || {
            INIT_STALL_RESETS.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = usb_reset(p.index as usize) {
                warn!(index = p.index, error = %e, "USB reset before re-init failed");
            }
        },
        std::thread::sleep,
        |attempt| warn!(index = p.index, attempt, "RTL-SDR stalled during open/init; resetting and redoing the init"),
        || open_initialized_once(p),
    );
    match &result {
        Ok(_) => set_device_error(None),
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
    if let Err(e) = usb_reset(p.index as usize) {
        warn!(index = p.index, error = %e, "USB reset before reopen failed; reopening anyway");
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
}

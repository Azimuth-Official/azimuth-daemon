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
use super::holders::HolderState;
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

/// This daemon has completed a sweep without a stall since its own start: it is
/// the OWNER of the dongle and never yields to a newcomer. Cleared by a fresh
/// start (`replace_sdr`), set by `sweep_finished`.
pub static OWNER: AtomicBool = AtomicBool::new(false);

/// What a stall may do to the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallPlan {
    /// Nobody else is running: the ordinary USB reset + reopen.
    Reset,
    /// A known holder process runs but we own the dongle: reopen our own handle,
    /// NEVER reset the bus (it would hit the newcomer's/holder's session too).
    ReopenNoReset,
    /// A known holder process runs and we have not streamed yet: we are the
    /// newcomer, so yield (`in_use`), no reset.
    InUse,
}

/// Tie-break between two daemons: the one that has streamed since its own open
/// owns the dongle; the newcomer yields. Never marks the owner in_use merely
/// because another process exists. `Unknown` holder status is treated like
/// `Running` for every reset decision (never reset on a guess).
pub fn stall_plan(owner: bool, holder: HolderState) -> StallPlan {
    match (holder, owner) {
        (HolderState::NotRunning, _) => StallPlan::Reset,
        (_, true) => StallPlan::ReopenNoReset,
        (_, false) => StallPlan::InUse,
    }
}

fn current_stall_plan() -> StallPlan {
    stall_plan(OWNER.load(Ordering::Relaxed), super::holders::holder_state())
}

/// Stall events seen so far (tune re-issues, init resets, ladder cycles).
pub fn stall_total() -> u64 {
    TUNE_STALL_RETRIES_TOTAL.load(Ordering::Relaxed)
        + INIT_STALL_RESETS.load(Ordering::Relaxed)
        + TUNE_LADDER_CYCLES_TOTAL.load(Ordering::Relaxed)
}

/// (discard_batch, become_owner) for a finished sweep. Held, or a newcomer
/// next to a running/unknown holder, or an owner that stalled next to a running
/// holder: the batch was contended and is dropped. A newcomer NEVER becomes
/// owner while a holder runs or is unknown. A clean sweep that did its tuning
/// I/O (productive or not, so quiet RF counts) makes this daemon the owner.
pub fn sweep_verdict(held: bool, stalled: bool, holder: HolderState, owner: bool, tuned: bool) -> (bool, bool) {
    if held || (!owner && holder != HolderState::NotRunning) {
        return (true, false);
    }
    (stalled && holder == HolderState::Running, !owner && !stalled && tuned)
}

/// Apply the verdict to `owner`; true = drop this sweep's batch.
pub fn finish_sweep(owner: &AtomicBool, held: bool, stalled: bool, holder: HolderState, tuned: bool) -> bool {
    let (discard, become_owner) = sweep_verdict(held, stalled, holder, owner.load(Ordering::Relaxed), tuned);
    if become_owner {
        owner.store(true, Ordering::Relaxed);
    }
    discard
}

/// Live version: a newcomer always checks for a holder; an owner only after a stall.
pub fn sweep_finished(stalls_before: u64, tuned: bool) -> bool {
    let stalled = stall_total() != stalls_before;
    let holder = if OWNER.load(Ordering::Relaxed) && !stalled {
        HolderState::NotRunning
    } else {
        super::holders::holder_state()
    };
    finish_sweep(&OWNER, held_by_other(), stalled, holder, tuned)
}

/// A fresh start (`replace_sdr`): not the owner, nothing known to hold the dongle.
pub fn fresh_start_with(owner: &AtomicBool, held: &AtomicBool) {
    owner.store(false, Ordering::Relaxed);
    held.store(false, Ordering::Relaxed);
}

pub fn fresh_start() {
    fresh_start_with(&OWNER, &HELD_BY_OTHER);
}

/// A newcomer must not do ANY I/O while a recognised program runs (or status is
/// unknown): mark the dongle held and tell the caller to leave it alone.
pub fn newcomer_must_yield(owner: bool, holder: HolderState) -> bool {
    !owner && holder != HolderState::NotRunning
}

/// Open-time ownership: a daemon whose open found no other program running
/// (`NotRunning`) and produced at least one open device owns the dongle at once.
/// A second daemon sees it `Running` and yields before any I/O; if both race past
/// the check, the exclusive USB claim makes one open fail. Without this, an early
/// stall left both daemons non-owners next to each other: mutual `in_use` forever.
pub fn owner_after_open(holder_at_open: HolderState, opened_devices: usize) -> bool {
    holder_at_open == HolderState::NotRunning && opened_devices > 0
}

/// What `replace_sdr` does before opening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReopenPlan {
    /// Open the dongle now.
    Open,
    /// Another program runs (or status is unknown) and we are a newcomer: no I/O, report in_use.
    Yield,
}

/// A daemon that already owned the dongle keeps the right to re-open it after
/// losing its handle: the other daemon seen as `Running` may itself be parked
/// in_use holding nothing, and yielding to it would park both forever.
pub fn reopen_plan(was_owner: bool, holder: HolderState) -> ReopenPlan {
    if newcomer_must_yield(was_owner, holder) {
        ReopenPlan::Yield
    } else {
        ReopenPlan::Open
    }
}

/// Ownership after a re-open attempt: a former owner stays owner only if it got
/// a device and nobody refused it as held; otherwise a third program took it.
pub fn owner_after_reopen(was_owner: bool, holder_at_open: HolderState, opened_devices: usize, held: bool) -> bool {
    if was_owner {
        opened_devices > 0 && !held
    } else {
        owner_after_open(holder_at_open, opened_devices)
    }
}

/// Run a USB reset ONLY when `plan` allows it. A known holder process means no
/// reset ever: the newcomer yields (flag set, in-use error), the owner just
/// reopens its own handle. A known held dongle short-circuits without touching
/// the bus. For unknown holders a reset refused as held sets `flag`. Any other
/// failure is returned unchanged.
pub fn guarded_reset_with(
    flag: &AtomicBool,
    access_means_in_use: bool,
    plan: impl FnOnce() -> StallPlan,
    reset: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if flag.load(Ordering::Relaxed) {
        return Err(anyhow::Error::new(in_use_error()));
    }
    match plan() {
        StallPlan::ReopenNoReset => return Ok(()),
        StallPlan::InUse => {
            flag.store(true, Ordering::Relaxed);
            warn!("Stall while another RTL-SDR program is running and this daemon has not streamed yet: yielding (in_use), no USB reset");
            return Err(anyhow::Error::new(in_use_error()));
        }
        StallPlan::Reset => {}
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
    guarded_reset_with(&HELD_BY_OTHER, ACCESS_MEANS_IN_USE, current_stall_plan, || usb_reset(index))
}

/// Recovery's reset (`recovery::reset_usb_bus_device`), with the same holder guard.
pub fn guarded_usb_reset(reset: impl FnOnce() -> anyhow::Result<()>) -> anyhow::Result<()> {
    guarded_reset_with(&HELD_BY_OTHER, ACCESS_MEANS_IN_USE, current_stall_plan, reset)
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
        let e = guarded_reset_with(&flag, true, || StallPlan::Reset, refuse).unwrap_err();
        assert!(is_in_use_anyhow(&e));
        assert!(flag.load(Ordering::Relaxed));
        assert_eq!(resets.get(), 1);
        // Held now: no further reset is even attempted.
        let e = guarded_reset_with(&flag, true, || StallPlan::Reset, refuse).unwrap_err();
        assert!(is_in_use_anyhow(&e));
        assert_eq!(resets.get(), 1, "no USB reset of a device another process holds");
        // The in-use error is not a stall, so the stall ladder never retries it.
        assert!(!is_rtl_stall(&in_use_error()));
    }

    #[test]
    fn access_on_linux_is_a_permission_problem_and_flaky_usb_keeps_recovery() {
        let flag = AtomicBool::new(false);
        let e = guarded_reset_with(&flag, false, || StallPlan::Reset, || Err(anyhow::Error::new(rusb::Error::Access))).unwrap_err();
        assert!(!is_in_use_anyhow(&e));
        assert!(!flag.load(Ordering::Relaxed));
        // Pipe/Io/NoDevice are ordinary USB trouble, never "in use".
        for err in [rusb::Error::Pipe, rusb::Error::Io, rusb::Error::NoDevice] {
            let e = guarded_reset_with(&flag, true, || StallPlan::Reset, || Err(anyhow::Error::new(err))).unwrap_err();
            assert!(!is_in_use_anyhow(&e));
        }
        assert!(!flag.load(Ordering::Relaxed));
        assert!(guarded_reset_with(&flag, true, || StallPlan::Reset, || Ok(())).is_ok());
        let busy = guarded_reset_with(&flag, false, || StallPlan::Reset, || Err(anyhow::Error::new(rusb::Error::Busy))).unwrap_err();
        assert!(is_in_use_anyhow(&busy));
    }

    fn counting_reset(n: &std::cell::Cell<u32>) -> impl FnOnce() -> anyhow::Result<()> + '_ {
        move || {
            n.set(n.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn newcomer_with_a_known_holder_yields_on_first_stall_with_zero_resets() {
        let (flag, n) = (AtomicBool::new(false), std::cell::Cell::new(0));
        let plan = || stall_plan(false, HolderState::Running);
        let e = guarded_reset_with(&flag, true, plan, counting_reset(&n)).unwrap_err();
        assert!(is_in_use_anyhow(&e) && flag.load(Ordering::Relaxed));
        assert_eq!(n.get(), 0, "no USB reset while a known holder runs");
    }

    #[test]
    fn owner_after_open_only_for_an_uncontended_open_with_a_device() {
        use HolderState::*;
        assert!(owner_after_open(NotRunning, 1));
        assert!(!owner_after_open(NotRunning, 0));
        assert!(!owner_after_open(Running, 1));
        assert!(!owner_after_open(Unknown, 1));
    }

    #[test]
    fn uncontended_opener_survives_a_stall_after_a_second_daemon_appears() {
        // Livelock: A opens alone, B appears (Running), A stalls before any sweep finished.
        let (owner, flag, n) = (AtomicBool::new(false), AtomicBool::new(false), std::cell::Cell::new(0));
        owner.store(owner_after_open(HolderState::NotRunning, 1), Ordering::Relaxed);
        let plan = || stall_plan(owner.load(Ordering::Relaxed), HolderState::Running);
        assert_eq!(plan(), StallPlan::ReopenNoReset);
        assert!(guarded_reset_with(&flag, true, plan, counting_reset(&n)).is_ok());
        assert!(!flag.load(Ordering::Relaxed), "must not park itself in_use");
        assert_eq!(n.get(), 0, "no reset next to a running holder");
    }

    #[test]
    fn reopen_plan_lets_a_former_owner_open_next_to_a_parked_daemon() {
        use HolderState::*;
        assert_eq!(reopen_plan(true, Running), ReopenPlan::Open);
        assert_eq!(reopen_plan(true, Unknown), ReopenPlan::Open);
        assert_eq!(reopen_plan(false, Running), ReopenPlan::Yield);
        assert_eq!(reopen_plan(false, Unknown), ReopenPlan::Yield);
        assert_eq!(reopen_plan(false, NotRunning), ReopenPlan::Open);
    }

    #[test]
    fn owner_after_reopen_keeps_ownership_only_with_a_device_and_not_held() {
        use HolderState::*;
        assert!(owner_after_reopen(true, Running, 1, false));
        assert!(!owner_after_reopen(true, Running, 0, false));
        assert!(!owner_after_reopen(true, Running, 1, true));
        assert!(owner_after_reopen(false, NotRunning, 1, false));
        assert!(!owner_after_reopen(false, Running, 1, false));
    }

    #[test]
    fn owner_that_lost_its_handle_reopens_instead_of_yielding_to_a_parked_daemon() {
        // A opens uncontended (owner); B appears (Running); A loses its device and re-detects.
        let owner = AtomicBool::new(false);
        owner.store(owner_after_open(HolderState::NotRunning, 1), Ordering::Relaxed);
        let was_owner = owner.load(Ordering::Relaxed);
        fresh_start_with(&owner, &AtomicBool::new(false)); // what replace_sdr does first
        assert_eq!(reopen_plan(was_owner, HolderState::Running), ReopenPlan::Open);
    }

    #[test]
    fn owner_keeps_the_dongle_reopens_but_never_resets_or_yields() {
        let (flag, n) = (AtomicBool::new(false), std::cell::Cell::new(0));
        let plan = || stall_plan(true, HolderState::Running);
        assert!(guarded_reset_with(&flag, true, plan, counting_reset(&n)).is_ok());
        assert_eq!(n.get(), 0);
        assert!(!flag.load(Ordering::Relaxed), "owner is never in_use merely because another process exists");
    }

    #[test]
    fn no_holder_process_keeps_the_ordinary_reset_recovery() {
        let (flag, n) = (AtomicBool::new(false), std::cell::Cell::new(0));
        for owner in [false, true] {
            assert!(guarded_reset_with(&flag, true, || stall_plan(owner, HolderState::NotRunning), counting_reset(&n)).is_ok());
        }
        assert_eq!(n.get(), 2);
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn unknown_holder_status_never_resets() {
        let (flag, n) = (AtomicBool::new(false), std::cell::Cell::new(0));
        let e = guarded_reset_with(&flag, true, || stall_plan(false, HolderState::Unknown), counting_reset(&n)).unwrap_err();
        assert!(is_in_use_anyhow(&e));
        assert!(guarded_reset_with(&AtomicBool::new(false), true, || stall_plan(true, HolderState::Unknown), counting_reset(&n)).is_ok());
        assert_eq!(n.get(), 0);
    }

    #[test]
    fn no_stall_next_to_a_running_holder_is_discarded_and_never_confers_ownership() {
        use HolderState::*;
        let owner = AtomicBool::new(false);
        assert!(finish_sweep(&owner, false, false, Running, true), "newcomer batch dropped");
        assert!(finish_sweep(&owner, false, false, Unknown, true));
        assert!(!owner.load(Ordering::Relaxed));
        assert!(newcomer_must_yield(false, Running) && newcomer_must_yield(false, Unknown));
        assert!(!newcomer_must_yield(true, Running) && !newcomer_must_yield(false, NotRunning));
    }

    #[test]
    fn quiet_rf_clean_sweep_makes_the_owner_and_an_owner_keeps_clean_batches() {
        use HolderState::*;
        let owner = AtomicBool::new(false);
        assert!(!finish_sweep(&owner, false, false, NotRunning, true), "tuned, nothing found: still clean");
        assert!(owner.load(Ordering::Relaxed));
        // Owner next to a holder: clean batch kept, stalled batch dropped, still owner.
        assert!(!finish_sweep(&owner, false, false, Running, true));
        assert!(finish_sweep(&owner, false, true, Running, true));
        assert!(owner.load(Ordering::Relaxed));
        // A plain stall with no holder keeps the batch and does not confer ownership.
        let fresh = AtomicBool::new(false);
        assert!(!finish_sweep(&fresh, false, true, NotRunning, true));
        assert!(!fresh.load(Ordering::Relaxed));
        // Held always drops.
        assert!(finish_sweep(&owner, true, false, NotRunning, true));
    }

    #[test]
    fn a_fresh_start_clears_held_and_owner_and_flags_are_independent_per_dongle() {
        let (o1, h1, o2, h2) = (AtomicBool::new(true), AtomicBool::new(true), AtomicBool::new(true), AtomicBool::new(true));
        fresh_start_with(&o1, &h1);
        assert!(!o1.load(Ordering::Relaxed) && !h1.load(Ordering::Relaxed));
        // Production keeps ONE process-wide pair (single-dongle scope); the helpers
        // take the flags, so a per-index array can be dropped in without change.
        assert!(o2.load(Ordering::Relaxed) && h2.load(Ordering::Relaxed));
    }
}

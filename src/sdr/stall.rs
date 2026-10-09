//! Tolerance for RTL2832U control-transfer stalls.
//!
//! Some dongles intermittently STALL a USB control transfer ("Pipe error").
//! librtlsdr logs it and carries on; rtl-sdr-rs turns every stall into a hard
//! error, so one stall used to abandon a whole channel and the recovery then
//! reopened the handle WITHOUT a USB reset, leaving the device wedged. These
//! helpers re-issue the operation and reset the device first on open.
//!
//! Plain generic logic with no USB calls, so it is unit-tested here and shared
//! (via `#[path]`) with the `tune_stress` hardware repro.

use std::time::Duration;

/// Re-issues of a stalled tune before the error is handed to the recovery ladder.
pub const TUNE_STALL_RETRIES: u32 = 3;
/// USB resets tried while opening a device that stalls its first control write.
pub const OPEN_RESET_ATTEMPTS: u32 = 2;

/// Wait before re-issue number `attempt` (1-based): 20, 40, 60 ms.
pub fn stall_backoff(attempt: u32) -> Duration {
    Duration::from_millis(20 * attempt as u64)
}

/// Run `op`, re-issuing it up to `retries` times while it fails with a stall.
/// A non-stall error is returned at once. `on_stall(attempt)` is called for
/// every stall that is retried (for counters and logging).
pub fn retry_stalls<T, E>(
    retries: u32,
    is_stall: impl Fn(&E) -> bool,
    mut sleep: impl FnMut(Duration),
    mut on_stall: impl FnMut(u32),
    mut op: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut attempt = 0;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if attempt < retries && is_stall(&e) => {
                attempt += 1;
                on_stall(attempt);
                sleep(stall_backoff(attempt));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Open a device the way librtlsdr does: if the open fails with a stall, reset
/// the USB device and try again (at most `max_resets` times).
pub fn open_with_reset<T, E>(
    max_resets: u32,
    is_stall: impl Fn(&E) -> bool,
    mut reset: impl FnMut(),
    mut sleep: impl FnMut(Duration),
    mut open: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut resets = 0;
    loop {
        match open() {
            Ok(v) => return Ok(v),
            Err(e) if resets < max_resets && is_stall(&e) => {
                resets += 1;
                reset();
                // The device re-enumerates after a reset.
                sleep(Duration::from_millis(500));
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Debug, PartialEq)]
    enum E {
        Stall,
        Other,
    }
    fn is_stall(e: &E) -> bool {
        *e == E::Stall
    }

    #[test]
    fn a_stall_is_retried_and_the_tune_then_succeeds() {
        let calls = Cell::new(0);
        let slept = RefCell::new(Vec::new());
        let seen = RefCell::new(Vec::new());
        let r = retry_stalls(
            TUNE_STALL_RETRIES,
            is_stall,
            |d| slept.borrow_mut().push(d),
            |a| seen.borrow_mut().push(a),
            || {
                calls.set(calls.get() + 1);
                if calls.get() <= 2 { Err(E::Stall) } else { Ok(calls.get()) }
            },
        );
        assert_eq!(r, Ok(3));
        assert_eq!(*seen.borrow(), vec![1, 2], "both stalls were counted");
        assert_eq!(
            *slept.borrow(),
            vec![Duration::from_millis(20), Duration::from_millis(40)]
        );
    }

    #[test]
    fn a_stall_that_never_clears_is_returned_after_the_retry_budget() {
        let calls = Cell::new(0);
        let r: Result<(), E> = retry_stalls(3, is_stall, |_| {}, |_| {}, || {
            calls.set(calls.get() + 1);
            Err(E::Stall)
        });
        assert_eq!(r, Err(E::Stall));
        assert_eq!(calls.get(), 4, "one try plus three re-issues");
    }

    #[test]
    fn other_errors_are_never_retried() {
        let calls = Cell::new(0);
        let r: Result<(), E> = retry_stalls(3, is_stall, |_| {}, |_| {}, || {
            calls.set(calls.get() + 1);
            Err(E::Other)
        });
        assert_eq!(r, Err(E::Other));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn first_try_success_costs_nothing() {
        let r: Result<u8, E> = retry_stalls(3, is_stall, |_| panic!("no sleep"), |_| panic!("no stall"), || Ok(7));
        assert_eq!(r, Ok(7));
    }

    #[test]
    fn open_resets_the_device_after_a_stall_and_reopens() {
        let (opens, resets) = (Cell::new(0), Cell::new(0));
        let r = open_with_reset(
            OPEN_RESET_ATTEMPTS,
            is_stall,
            || resets.set(resets.get() + 1),
            |d| assert_eq!(d, Duration::from_millis(500)),
            || {
                opens.set(opens.get() + 1);
                if opens.get() == 1 { Err(E::Stall) } else { Ok("open") }
            },
        );
        assert_eq!(r, Ok("open"));
        assert_eq!((opens.get(), resets.get()), (2, 1));
    }

    #[test]
    fn open_gives_up_after_the_reset_budget_and_never_resets_for_other_errors() {
        let resets = Cell::new(0);
        let r: Result<(), E> = open_with_reset(2, is_stall, || resets.set(resets.get() + 1), |_| {}, || Err(E::Stall));
        assert_eq!((r, resets.get()), (Err(E::Stall), 2));
        resets.set(0);
        let r: Result<(), E> = open_with_reset(2, is_stall, || resets.set(resets.get() + 1), |_| {}, || Err(E::Other));
        assert_eq!((r, resets.get()), (Err(E::Other), 0));
    }
}

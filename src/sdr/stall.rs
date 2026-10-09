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
/// Full open+init attempts before the device is reported as errored.
pub const INIT_ATTEMPTS: u32 = 5;

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

/// Wait after a failed init attempt number `attempt` (1-based): 0.5, 1, 2, 4, 8 s.
pub fn init_backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << attempt.saturating_sub(1).min(5))
}

/// The WHOLE open+init sequence under the stall ladder: any stall (at open, at
/// set_sample_rate, tuner init, gain, ...) closes the device (the caller's
/// `init` owns and drops it), USB-resets, waits and redoes everything, at most
/// `attempts` times in total. `on_stall(attempt)` is called for every retried
/// stall; non-stall errors end the ladder at once.
pub fn init_ladder<T, E>(
    attempts: u32,
    is_stall: impl Fn(&E) -> bool,
    mut reset: impl FnMut(),
    mut sleep: impl FnMut(Duration),
    mut on_stall: impl FnMut(u32),
    mut init: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match init() {
            Ok(v) => return Ok(v),
            Err(e) if attempt < attempts && is_stall(&e) => {
                on_stall(attempt);
                reset();
                sleep(init_backoff(attempt));
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
    fn init_ladder_resets_waits_and_redoes_the_whole_init() {
        let (inits, resets) = (Cell::new(0), Cell::new(0));
        let slept = RefCell::new(Vec::new());
        let r = init_ladder(
            INIT_ATTEMPTS,
            is_stall,
            || resets.set(resets.get() + 1),
            |d| slept.borrow_mut().push(d),
            |_| {},
            || {
                inits.set(inits.get() + 1);
                if inits.get() <= 2 { Err(E::Stall) } else { Ok("ready") }
            },
        );
        assert_eq!(r, Ok("ready"));
        assert_eq!((inits.get(), resets.get()), (3, 2));
        assert_eq!(*slept.borrow(), vec![Duration::from_millis(500), Duration::from_millis(1000)]);
    }

    #[test]
    fn init_backoff_is_half_a_second_doubling_to_eight() {
        let secs: Vec<f32> = (1..=5).map(|a| init_backoff(a).as_secs_f32()).collect();
        assert_eq!(secs, vec![0.5, 1.0, 2.0, 4.0, 8.0]);
    }

    #[test]
    fn init_ladder_gives_up_after_five_attempts_and_ignores_other_errors() {
        let (inits, resets) = (Cell::new(0), Cell::new(0));
        let r: Result<(), E> = init_ladder(5, is_stall, || resets.set(resets.get() + 1), |_| {}, |_| {}, || {
            inits.set(inits.get() + 1);
            Err(E::Stall)
        });
        assert_eq!((r, inits.get(), resets.get()), (Err(E::Stall), 5, 4));
        let (inits, resets) = (Cell::new(0), Cell::new(0));
        let r: Result<(), E> = init_ladder(5, is_stall, || resets.set(resets.get() + 1), |_| {}, |_| {}, || {
            inits.set(inits.get() + 1);
            Err(E::Other)
        });
        assert_eq!((r, inits.get(), resets.get()), (Err(E::Other), 1, 0));
    }
}

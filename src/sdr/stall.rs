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

/// In-place re-issues of a stalled tune. Hardware showed an in-place retry never
/// clears this stall (0 of 135), so one cheap attempt only, then the ladder.
pub const TUNE_STALL_RETRIES: u32 = 1;
/// Reset+reopen cycles tried for ONE channel before it counts as failed.
pub const TUNE_LADDER_CYCLES: u32 = 2;
/// Full open+init attempts before the device is reported as errored.
pub const INIT_ATTEMPTS: u32 = 5;

/// Wait before re-issue number `attempt` (1-based): 20, 40, 60 ms ...
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

/// One channel's tune under the full ladder. `slot` holds the open device (None
/// = lost). On a stall the device is dropped, `reopen` (USB reset + full init)
/// brings it back and the SAME tune is retried, at most `cycles` times; only
/// then is the stall returned. A device that is already gone is reopened first,
/// so one failed channel never strands the rest of the sweep.
pub fn tune_ladder<D, E>(
    slot: &mut Option<D>,
    cycles: u32,
    is_stall: impl Fn(&E) -> bool,
    mut tune: impl FnMut(&mut D) -> Result<(), E>,
    mut reopen: impl FnMut() -> Result<D, E>,
    mut on_cycle: impl FnMut(u32),
) -> Result<(), E> {
    let mut cycle = 0;
    loop {
        if slot.is_none() {
            *slot = Some(reopen()?);
        }
        let result = match slot.as_mut() {
            Some(d) => tune(d),
            None => continue, // unreachable: just filled above
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) if cycle < cycles && is_stall(&e) => {
                cycle += 1;
                on_cycle(cycle);
                *slot = None; // drop the stalled handle before the reset
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
            3, // the generic helper, independent of the production budget
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

    #[test]
    fn production_budget_is_one_cheap_retry_then_two_ladder_cycles() {
        assert_eq!((TUNE_STALL_RETRIES, TUNE_LADDER_CYCLES, INIT_ATTEMPTS), (1, 2, 5));
    }

    #[test]
    fn tune_ladder_resets_reopens_and_retries_the_same_tune() {
        let mut slot = Some(0u32); // handle generation
        let (tunes, reopens, cycles) = (Cell::new(0), Cell::new(0), RefCell::new(Vec::new()));
        let r = tune_ladder(
            &mut slot,
            TUNE_LADDER_CYCLES,
            is_stall,
            |gen| {
                tunes.set(tunes.get() + 1);
                // Only a freshly reopened handle (generation >= 1) can tune.
                if *gen >= 1 { Ok(()) } else { Err(E::Stall) }
            },
            || {
                reopens.set(reopens.get() + 1);
                Ok(reopens.get())
            },
            |c| cycles.borrow_mut().push(c),
        );
        assert_eq!(r, Ok(()));
        assert_eq!((tunes.get(), reopens.get()), (2, 1));
        assert_eq!(*cycles.borrow(), vec![1]);
        assert_eq!(slot, Some(1));
    }

    #[test]
    fn tune_ladder_gives_up_after_two_cycles_and_a_lost_device_heals_on_the_next_call() {
        let mut slot = Some(0u32);
        let reopens = Cell::new(0);
        let r = tune_ladder(&mut slot, 2, is_stall, |_| Err(E::Stall), || {
            reopens.set(reopens.get() + 1);
            Ok(9)
        }, |_| {});
        assert_eq!((r, reopens.get()), (Err(E::Stall), 2));
        // Reopen fails: the error comes back, the slot stays empty ...
        let mut slot: Option<u32> = Some(0);
        let r = tune_ladder(&mut slot, 2, is_stall, |_| Err(E::Stall), || Err(E::Other), |_| {});
        assert_eq!(r, Err(E::Other));
        assert!(slot.is_none());
        // ... and the next channel reopens first, then tunes.
        let r = tune_ladder(&mut slot, 2, is_stall, |_| Ok(()), || Ok(5), |_| {});
        assert_eq!(r, Ok(()));
        assert_eq!(slot, Some(5));
    }

    #[test]
    fn tune_ladder_never_resets_for_a_non_stall_error() {
        let mut slot = Some(0u32);
        let reopens = Cell::new(0);
        let r = tune_ladder(&mut slot, 2, is_stall, |_| Err(E::Other), || {
            reopens.set(reopens.get() + 1);
            Ok(1)
        }, |_| {});
        assert_eq!((r, reopens.get(), slot), (Err(E::Other), 0, Some(0)));
    }
}

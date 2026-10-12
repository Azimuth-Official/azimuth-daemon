//! RED tests for G4 (PLL lock counting on the real tune path). Child module of `scanner` so it can reach
//! private items; it adds no production logic.
//!
//! G4 wished-for API (assumed, minimal; the implementer adds it, these tests fail to compile until then):
//!
//! ```ignore
//! // crate::sdr (src/sdr/mod.rs)
//! pub trait TunerOps {
//!     fn set_frequency(&mut self, freq_hz: u64) -> anyhow::Result<()>; // SdrDevice::set_center_freq
//!     fn has_lock(&mut self) -> bool;                                   // tuner PLL lock readback
//!     fn last_tuned_freq_hz(&self) -> Option<u64>;
//!     fn set_last_tuned_freq_hz(&mut self, freq_hz: u64);
//! }
//! // Scanner (src/sdr/scanner.rs): the real tune path, `tune_with_recovery` delegating to it.
//! fn tune_with_recovery_on(&mut self, tuner: &mut dyn TunerOps, freq_hz: u64) -> Option<()>
//! // BandSweep gains three `u32` fields, serialised in /status `sweep.<band>`:
//! //   pll_lock_failures: every has_lock()==false observed after a settle on the real tune path
//! //   settle_waits:      every post-tune settle wait (retune_delay) performed, including retries
//! //   tune_retries:      every re-tune attempted after a failed lock
//! ```
//!
//! Relations asserted (not literal retry budgets, so the implementer may choose the ladder length, >= 1 retry):
//! attempts = set_frequency calls; a locked tune needs one settle; every failed lock is counted once, is
//! followed by a retry unless the budget is exhausted, and every attempt does its own settle wait.

use super::*;
use crate::sdr::TunerOps;

/// Tuner double: `set_frequency` always succeeds; `has_lock` answers from a script, then `true`.
struct FakeTuner {
    lock_script: Vec<bool>,
    tunes: u32,
    lock_checks: u32,
    last: Option<u64>,
}

impl FakeTuner {
    fn new(lock_script: &[bool]) -> Self {
        FakeTuner { lock_script: lock_script.to_vec(), tunes: 0, lock_checks: 0, last: None }
    }
}

impl TunerOps for FakeTuner {
    fn set_frequency(&mut self, freq_hz: u64) -> anyhow::Result<()> {
        self.tunes += 1;
        self.last = Some(freq_hz);
        Ok(())
    }
    fn has_lock(&mut self) -> bool {
        let i = self.lock_checks as usize;
        self.lock_checks += 1;
        // Past the end of the script the tuner locks.
        self.lock_script.get(i).copied().unwrap_or(true)
    }
    fn last_tuned_freq_hz(&self) -> Option<u64> {
        self.last
    }
    fn set_last_tuned_freq_hz(&mut self, freq_hz: u64) {
        self.last = Some(freq_hz);
    }
}

fn fast_scanner() -> Scanner {
    let mut s = Scanner::new(&ScanningConfig::default(), -30.0);
    s.retune_delay_ms = 1;
    s
}

#[test]
fn a_locked_tune_counts_one_settle_and_no_failure() {
    let mut s = fast_scanner();
    let mut t = FakeTuner::new(&[]); // empty script: always locked
    s.begin_band(BandKind::Fm, 1);
    assert_eq!(s.tune_with_recovery_on(&mut t, 100_300_000), Some(()));
    s.finish_band(BandKind::Fm, 0, 0);
    let fm = s.sweep_report().fm;
    assert_eq!(t.tunes, 1);
    assert_eq!((fm.pll_lock_failures, fm.settle_waits, fm.tune_retries), (0, 1, 0));
}

#[test]
fn one_lost_lock_is_counted_settled_and_retried() {
    let mut s = fast_scanner();
    let mut t = FakeTuner::new(&[false]); // first check: not locked; afterwards locked
    s.begin_band(BandKind::Fm, 1);
    assert_eq!(s.tune_with_recovery_on(&mut t, 100_300_000), Some(()), "a retry after one lost lock must succeed");
    s.finish_band(BandKind::Fm, 1, 0);
    let fm = s.sweep_report().fm;
    assert_eq!(t.tunes, 2, "the failed lock is retried by re-tuning");
    assert_eq!(fm.pll_lock_failures, 1);
    assert_eq!(fm.tune_retries, 1);
    assert_eq!(fm.settle_waits, 2, "one settle per attempt");
    assert_eq!(fm.tune_failed, 0);
}

#[test]
fn a_tuner_that_never_locks_fails_the_tune_and_every_failure_is_counted() {
    let mut s = fast_scanner();
    let mut t = FakeTuner::new(&[false; 64]); // does not lock within any plausible retry budget
    s.begin_band(BandKind::Lte, 1);
    assert_eq!(s.tune_with_recovery_on(&mut t, 800_000_000), None);
    assert_eq!(s.tune_fail_count, 1, "a final lock failure counts as a failed tune");
    s.finish_band(BandKind::Lte, 0, 1);
    let r = s.sweep_report();
    let attempts = t.tunes;
    assert!(attempts >= 2, "the real path must retry at least once, saw {attempts} attempt(s)");
    assert_eq!(r.lte.pll_lock_failures, attempts, "every lock failure counted, none lost");
    assert_eq!(r.lte.settle_waits, attempts, "every attempt waits its settle");
    assert_eq!(r.lte.tune_retries, attempts - 1);
    assert_eq!(r.lte.tune_failed, 1);
    // Counts are per band: nothing leaked into the others.
    assert_eq!((r.fm.pll_lock_failures, r.dtv.pll_lock_failures), (0, 0));
}

#[test]
fn counters_restart_with_each_band_sweep() {
    let mut s = fast_scanner();
    let mut bad = FakeTuner::new(&[false]);
    s.begin_band(BandKind::Fm, 1);
    let _ = s.tune_with_recovery_on(&mut bad, 100_300_000);
    s.finish_band(BandKind::Fm, 0, 0);
    assert_eq!(s.sweep_report().fm.pll_lock_failures, 1);
    s.begin_band(BandKind::Fm, 1);
    let mut good = FakeTuner::new(&[]);
    let _ = s.tune_with_recovery_on(&mut good, 100_500_000);
    s.finish_band(BandKind::Fm, 0, 0);
    let fm = s.sweep_report().fm;
    assert_eq!((fm.pll_lock_failures, fm.settle_waits, fm.tune_retries), (0, 1, 0));
}

#[test]
fn pll_counts_serialise_in_every_band_of_the_sweep_report() {
    let mut s = fast_scanner();
    let mut t = FakeTuner::new(&[false]);
    s.begin_band(BandKind::Dtv, 1);
    let _ = s.tune_with_recovery_on(&mut t, 602_000_000);
    s.finish_band(BandKind::Dtv, 0, 0);
    let v = serde_json::to_value(s.sweep_report()).unwrap();
    for band in ["fm", "lte", "dtv"] {
        for key in ["pll_lock_failures", "settle_waits", "tune_retries"] {
            assert!(v[band][key].is_u64(), "{band}.{key}");
        }
    }
    assert_eq!(v["dtv"]["pll_lock_failures"], 1);
    assert_eq!(v["dtv"]["tune_retries"], 1);
    assert_eq!(v["dtv"]["settle_waits"], 2);
}

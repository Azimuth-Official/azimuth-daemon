//! G4 RED test: the PLL-lock counts of the sweep appear in the `/status` sweep JSON.
//! Child module of `status_server` so it can call the private `to_info`; no production logic.
//! Needs `BandSweep::{pll_lock_failures, settle_waits, tune_retries}: u32` (see `sdr/scanner/tests_dsp.rs`).

use super::*;

#[test]
fn status_sweep_carries_pll_lock_settle_and_retry_counts() {
    let mut st = StatusState::new("n".to_string(), "tier1_rtlsdr".to_string());
    let handle = Arc::new(std::sync::Mutex::new(crate::sdr::scanner::SweepReport::default()));
    {
        let mut h = handle.lock().unwrap();
        h.fm.pll_lock_failures = 3;
        h.fm.settle_waits = 9;
        h.fm.tune_retries = 2;
        h.dtv.pll_lock_failures = 1;
    }
    st.sweep_handle = Some(Arc::clone(&handle));
    let v = serde_json::to_value(st.to_info(true, false)).unwrap();
    for band in ["fm", "lte", "dtv"] {
        for k in ["pll_lock_failures", "settle_waits", "tune_retries"] {
            assert!(v["sweep"][band][k].is_u64(), "sweep.{band}.{k}");
        }
    }
    assert_eq!(v["sweep"]["fm"]["pll_lock_failures"], 3);
    assert_eq!(v["sweep"]["fm"]["settle_waits"], 9);
    assert_eq!(v["sweep"]["fm"]["tune_retries"], 2);
    assert_eq!(v["sweep"]["dtv"]["pll_lock_failures"], 1);
    assert_eq!(v["sweep"]["lte"]["pll_lock_failures"], 0);
    // Live: a later scanner update shows without rebuilding the state.
    handle.lock().unwrap().fm.pll_lock_failures = 4;
    let v = serde_json::to_value(st.to_info(true, false)).unwrap();
    assert_eq!(v["sweep"]["fm"]["pll_lock_failures"], 4);
}

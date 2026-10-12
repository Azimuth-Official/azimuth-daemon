//! G2b: the DVB channel dwell covers a whole maximum T2 frame before a P1-less cyclic-prefix detection
//! is labelled DVB-T, and a P1-less T2-only mode emits nothing. Child module of `scanner` (private items).

use super::*;
use crate::sdr::dtv::dvb::{FftMode, GuardInterval};
use crate::sdr::dtv::testgen::{dvb_capture, negative_capture, Negative};
use crate::sdr::dtv::testgen_t2::{t2_capture, T2Mode, T2Params};

fn dvb_scanner() -> Scanner {
    let mut c = ScanningConfig::default();
    c.dtv.standard = "DVB-T2-694".to_string();
    c.dtv.channel_mhz = 8;
    c.dtv.sweep_start = 470_000_000;
    c.dtv.sweep_end = 694_000_000;
    c.dtv.sweep_step = 8_000_000;
    let mut s = Scanner::new(&c, -30.0);
    s.dtv_plan = DtvPlan::from_config(&s.dtv_config);
    s
}

/// Runs a dwell whose further captures come from `queue`; returns (observations, further captures asked for).
fn dwell(s: &mut Scanner, first: &[u8], queue: Vec<Vec<u8>>) -> (Vec<ScanResult>, usize) {
    let mut asked = 0usize;
    let mut q = queue.into_iter();
    let r = s.scan_dvb_dwell(first, 2_048_000, 602_000_000, -40.0, |_| {
        asked += 1;
        q.next()
    });
    (r, asked)
}

fn t2(seed: u64, fft: FftMode, gi: GuardInterval, with_p1: bool) -> Vec<u8> {
    let mut p = T2Params::new(T2Mode { fft, gi, extended: false });
    p.with_p1 = with_p1;
    t2_capture(seed, p, 12.0, 30_000.0, false)
}

#[test]
fn a_detected_p1_in_the_first_capture_needs_no_further_captures() {
    let mut s = dvb_scanner();
    let first = t2(1, FftMode::K8, GuardInterval::G1_8, true);
    let (r, asked) = dwell(&mut s, &first, Vec::new());
    assert_eq!(asked, 0);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].signal_type, "dtv_dvbt2");
}

#[test]
fn nothing_detected_asks_for_no_further_captures() {
    let mut s = dvb_scanner();
    let (r, asked) = dwell(&mut s, &negative_capture(9, Negative::Awgn), Vec::new());
    assert!(r.is_empty());
    assert_eq!(asked, 0);
}

#[test]
fn a_p1_in_a_later_capture_makes_a_shared_mode_channel_t2() {
    let mut s = dvb_scanner();
    let first = t2(2, FftMode::K8, GuardInterval::G1_8, false); // frame body, P1 outside this capture
    let mut later: Vec<Vec<u8>> = (0..DVB_P1_EXTRA_CAPTURES as u64).map(|i| t2(30 + i, FftMode::K8, GuardInterval::G1_8, false)).collect();
    later[1] = t2(4, FftMode::K8, GuardInterval::G1_8, true);
    let (r, asked) = dwell(&mut s, &first, later);
    assert_eq!(asked, DVB_P1_EXTRA_CAPTURES, "the dwell's captures are taken back to back before any search, so the air time is contiguous");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].signal_type, "dtv_dvbt2");
    assert_eq!(r[0].raw_data.as_ref().unwrap()["fft_mode"], "8K");
}

#[test]
fn a_shared_mode_without_any_p1_over_the_whole_window_is_dvbt() {
    let mut s = dvb_scanner();
    let first = dvb_capture(5, FftMode::K8, GuardInterval::G1_8, 8, 12.0, 60_000.0, false);
    let later: Vec<Vec<u8>> = (0..DVB_P1_EXTRA_CAPTURES as u64).map(|i| dvb_capture(10 + i, FftMode::K8, GuardInterval::G1_8, 8, 12.0, 60_000.0, false)).collect();
    let (r, asked) = dwell(&mut s, &first, later);
    assert_eq!(asked, DVB_P1_EXTRA_CAPTURES, "first + extras must span a full maximum T2 frame (250 ms) with margin");
    assert!((1 + DVB_P1_EXTRA_CAPTURES) as f64 * 0.064 >= 0.250 + 0.064);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].signal_type, "dtv_dvbt");
    assert_eq!(s.sweep.dtv.t2_unconfirmed, 0);
}

#[test]
fn a_t2_only_mode_without_any_p1_emits_nothing_and_is_counted_unconfirmed() {
    let mut s = dvb_scanner();
    let first = t2(6, FftMode::K32, GuardInterval::G19_128, false);
    let later: Vec<Vec<u8>> = (0..DVB_P1_EXTRA_CAPTURES as u64).map(|i| t2(20 + i, FftMode::K32, GuardInterval::G19_128, false)).collect();
    let (r, asked) = dwell(&mut s, &first, later);
    assert_eq!(asked, DVB_P1_EXTRA_CAPTURES);
    assert!(r.is_empty());
    assert_eq!(s.sweep.dtv.t2_unconfirmed, 1);
}

#[test]
fn a_failed_further_capture_ends_the_dwell_without_inventing_a_label() {
    let mut s = dvb_scanner();
    let first = t2(7, FftMode::K16, GuardInterval::G1_16, false);
    let (r, asked) = dwell(&mut s, &first, Vec::new());
    assert_eq!(asked, 1);
    assert!(r.is_empty());
}

#[test]
fn a_p1_in_a_later_capture_rescues_a_t2_only_mode() {
    let mut s = dvb_scanner();
    let first = t2(8, FftMode::K32, GuardInterval::G19_128, false);
    let (r, _) = dwell(&mut s, &first, vec![t2(9, FftMode::K32, GuardInterval::G19_128, true)]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].signal_type, "dtv_dvbt2");
    assert_eq!(s.sweep.dtv.t2_unconfirmed, 0);
}

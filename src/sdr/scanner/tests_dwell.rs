//! G2b: the DVB channel dwell reads ONE contiguous window of a whole maximum T2 frame (plus margin) before
//! a P1-less cyclic-prefix detection is labelled DVB-T, and a P1-less T2-only mode emits nothing. An
//! incomplete window gives no label at all. Child module of `scanner` (private items).

use super::*;
use crate::sdr::dtv::dvb::{self, FftMode, GuardInterval};
use crate::sdr::dtv::testgen::{negative_capture, Negative};
use crate::sdr::dtv::testgen_t2::{t2_capture, T2Mode, T2Params};

const SHARED: (FftMode, GuardInterval) = (FftMode::K8, GuardInterval::G1_8);
const T2_ONLY: (FftMode, GuardInterval) = (FftMode::K32, GuardInterval::G19_128);

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

fn t2(seed: u64, mode: (FftMode, GuardInterval), with_p1: bool) -> Vec<u8> {
    let mut p = T2Params::new(T2Mode { fft: mode.0, gi: mode.1, extended: false });
    p.with_p1 = with_p1;
    t2_capture(seed, p, 12.0, 30_000.0, false)
}

/// A full dwell window: six P1-less captures end to end, with (optionally) one P1 spliced in so that it
/// STARTS at sample `p1_at` of the window.
fn window(mode: (FftMode, GuardInterval), p1_at: Option<usize>) -> Vec<u8> {
    let mut w = Vec::new();
    for i in 0..DVB_WINDOW_CAPTURES as u64 {
        w.extend(t2(100 + i, mode, false));
    }
    assert_eq!(w.len(), DVB_WINDOW_BYTES);
    if let Some(pos) = p1_at {
        let src = t2(200, mode, true);
        let c = dvb::find_p1(&src, 2_048_000, 8).expect("source P1").start.round() as usize;
        let (lo, hi) = (c - 3000, c + 3000);
        assert!(hi <= src.len() / 2);
        let dst = pos - 3000;
        w[2 * dst..2 * dst + 2 * (hi - lo)].copy_from_slice(&src[2 * lo..2 * hi]);
    }
    w
}

/// Runs a dwell whose window read is `read`; returns (observations, whether the window was read).
fn dwell(s: &mut Scanner, first: &[u8], read: Option<Vec<u8>>, bump_recovery: bool) -> (Vec<ScanResult>, bool) {
    let mut called = false;
    let r = s.scan_dvb_dwell(first, 2_048_000, 602_000_000, -40.0, |sc| {
        called = true;
        if bump_recovery {
            sc.recoveries_run += 1;
        }
        read
    });
    (r, called)
}

#[test]
fn the_window_is_one_read_that_covers_a_whole_maximum_t2_frame_with_margin() {
    // 250 ms frame + 64 ms capture length, at 2.048 MS/s and 2 bytes per sample.
    let need = (0.250 + 0.064) * 2_048_000.0 * 2.0;
    assert!(DVB_WINDOW_BYTES as f64 >= need, "window {} bytes", DVB_WINDOW_BYTES);
    assert!(DVB_WINDOW_BYTES as f64 / (2.0 * 2_048_000.0) < 0.5, "and no longer than needed");
}

#[test]
fn every_p1_length_interval_of_the_window_lies_whole_inside_one_slice() {
    let total = DVB_WINDOW_BYTES / 2;
    let slices = dvb::window_slices(total);
    assert_eq!(slices.first().unwrap().start, 0);
    assert_eq!(slices.last().unwrap().end, total);
    // Control: the slices are genuinely several, overlapping ones.
    assert!(slices.len() >= 6 && slices[1].start < slices[0].end);
    for start in (0..=total - dvb::WINDOW_SLICE_OVERLAP).step_by(7) {
        let end = start + dvb::WINDOW_SLICE_OVERLAP; // 2048 samples: 4.5 P1 lengths
        assert!(slices.iter().any(|r| r.start <= start && end <= r.end), "interval {start}..{end} straddles every slice");
    }
}

#[test]
fn a_p1_in_the_old_gaps_and_straddling_slice_boundaries_is_found() {
    let mut s = dvb_scanner();
    let first = t2(1, SHARED, false);
    let slices = dvb::window_slices(DVB_WINDOW_BYTES / 2);
    // Starts: just after the first old capture ended (the old gap), exactly on and either side of every slice
    // boundary (P1 straddling), and near the window end.
    let mut at = vec![131_072 + 300, 262_144 + 100, 393_216 - 200, DVB_WINDOW_BYTES / 2 - 3_500];
    for r in &slices[1..] {
        at.extend([r.start + 100, r.start - 300, r.start + 459 - 300]);
    }
    at.extend(slices.iter().map(|r| r.end - 459 / 2));
    for pos in at {
        if pos < 3000 || pos + 3000 > DVB_WINDOW_BYTES / 2 {
            continue;
        }
        let (r, called) = dwell(&mut s, &first, Some(window(SHARED, Some(pos))), false);
        assert!(called);
        assert_eq!(r.len(), 1, "P1 at {pos}");
        assert_eq!(r[0].signal_type, "dtv_dvbt2", "P1 at sample {pos} of the window not found");
    }
    assert_eq!((s.sweep.dtv.t2_unconfirmed, s.sweep.dtv.dvb_dwell_incomplete), (0, 0));
}

#[test]
fn a_p1_in_the_first_capture_needs_no_window() {
    let mut s = dvb_scanner();
    let (r, called) = dwell(&mut s, &t2(2, SHARED, true), None, false);
    assert!(!called);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].signal_type, "dtv_dvbt2");
}

#[test]
fn nothing_detected_reads_no_window() {
    let mut s = dvb_scanner();
    let (r, called) = dwell(&mut s, &negative_capture(9, Negative::Awgn), None, false);
    assert!(r.is_empty() && !called);
}

#[test]
fn a_shared_mode_with_a_complete_p1_less_window_is_dvbt() {
    let mut s = dvb_scanner();
    let (r, called) = dwell(&mut s, &t2(3, SHARED, false), Some(window(SHARED, None)), false);
    assert!(called);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].signal_type, "dtv_dvbt");
    assert_eq!((s.sweep.dtv.t2_unconfirmed, s.sweep.dtv.dvb_dwell_incomplete), (0, 0));
}

#[test]
fn a_t2_only_mode_with_a_complete_p1_less_window_emits_nothing_and_is_counted_unconfirmed_once() {
    let mut s = dvb_scanner();
    let (r, _) = dwell(&mut s, &t2(4, T2_ONLY, false), Some(window(T2_ONLY, None)), false);
    assert!(r.is_empty());
    assert_eq!((s.sweep.dtv.t2_unconfirmed, s.sweep.dtv.dvb_dwell_incomplete), (1, 0));
}

#[test]
fn discovery_does_not_count_t2_unconfirmed_so_the_dwell_counts_it_exactly_once() {
    let mut s = dvb_scanner();
    let first = t2(5, T2_ONLY, false);
    let mut result = DiscoveryResult { fm_stations: Vec::new(), lte_cells: Vec::new(), dtv_channels: Vec::new() };
    let cap = first.clone();
    s.dvb_sweep_run(&[(37, 602_000_000)], 8, &mut result, move |_, _| Some((cap.clone(), 2_048_000)));
    assert_eq!(result.dtv_channels.len(), 1, "control: the channel is found");
    assert_eq!(s.sweep.dtv.t2_unconfirmed, 0, "discovery must not count it");
    let (r, _) = dwell(&mut s, &first, Some(window(T2_ONLY, None)), false);
    assert!(r.is_empty());
    assert_eq!(s.sweep.dtv.t2_unconfirmed, 1);
}

#[test]
fn an_incomplete_window_without_a_p1_gives_no_label_and_no_unconfirmed_count() {
    for (mode, first_seed) in [(SHARED, 6u64), (T2_ONLY, 7)] {
        let full = window(mode, None);
        let cases: [(&str, Option<Vec<u8>>, bool); 3] = [
            ("failed read", None, false),
            ("short read", Some(full[..DVB_WINDOW_BYTES - 2 * CAPTURE_BYTES].to_vec()), false),
            ("recovery during the dwell", Some(full), true),
        ];
        for (what, read, bump) in cases {
            let mut s = dvb_scanner();
            let (r, called) = dwell(&mut s, &t2(first_seed, mode, false), read, bump);
            assert!(called);
            assert!(r.is_empty(), "{what} {mode:?}: no label from an incomplete window");
            assert_eq!((s.sweep.dtv.t2_unconfirmed, s.sweep.dtv.dvb_dwell_incomplete), (0, 1), "{what} {mode:?}");
        }
    }
}

#[test]
fn a_recovery_voids_even_a_window_that_holds_a_p1() {
    let mut s = dvb_scanner();
    let (r, _) = dwell(&mut s, &t2(8, SHARED, false), Some(window(SHARED, Some(200_000))), true);
    assert!(r.is_empty());
    assert_eq!(s.sweep.dtv.dvb_dwell_incomplete, 1);
}

#[test]
fn a_p1_in_a_short_window_still_makes_the_channel_t2() {
    let mut s = dvb_scanner();
    let full = window(T2_ONLY, Some(50_000));
    let (r, _) = dwell(&mut s, &t2(9, T2_ONLY, false), Some(full[..3 * CAPTURE_BYTES].to_vec()), false);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].signal_type, "dtv_dvbt2");
    assert_eq!((s.sweep.dtv.t2_unconfirmed, s.sweep.dtv.dvb_dwell_incomplete), (0, 0));
}

#[test]
fn the_incomplete_counter_serialises_and_restarts_with_each_band_sweep() {
    let mut s = dvb_scanner();
    s.sweep.dtv.dvb_dwell_incomplete = 3;
    let v = serde_json::to_value(s.sweep_report()).unwrap();
    assert_eq!(v["dtv"]["dvb_dwell_incomplete"], 3);
    assert!(v["fm"]["dvb_dwell_incomplete"].is_u64() && v["dtv"]["t2_unconfirmed"].is_u64());
    s.begin_band(BandKind::Dtv, 1);
    assert_eq!(s.sweep_report().dtv.dvb_dwell_incomplete, 0);
}

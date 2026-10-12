//! RED tests for DVB-T2 detection by P1 (EN 302 755 clause 9.8) on SYNTHETIC captures (test only).
//!
//! Acceptance bar (G2b), matching or beating the DVB-T/T2 cyclic-prefix bar of `validation.rs`:
//! * every synthetic cell detected (reduced: every one of the 51 T2 mode combinations at 8 dB),
//! * 99.8 percent detection at -3 dB in-band SNR,
//! * 0 false positives and 0 T2 labels on AWGN / CW / FM / ATSC,
//! * `dtv_dvbt2` ONLY for a detected P1 (also for shared modes such as 8K 1/8, 8K 1/4, 2K 1/32),
//!   `dtv_dvbt` for DVB-T, and the DVB-T vs DVB-T2 label correct in at least 99 percent of cases.
//!
//! Reduced runs (normal suite):
//!   `cargo test --release --bin azimuth-daemon t2_reduced`
//! Full ignored runs (ROC matrix, 500-trial negatives):
//!   `cargo test --release --bin azimuth-daemon t2_full -- --ignored --nocapture`

use super::dvb::*;
use super::testgen::*;
use super::testgen_t2::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const T2: &str = "dtv_dvbt2";
const T1: &str = "dtv_dvbt";

fn parallel<T: Send, F: Fn(usize) -> T + Sync>(jobs: usize, f: F) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(12);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= jobs {
                    break;
                }
                let r = f(i);
                out.lock().unwrap().push((i, r));
            });
        }
    });
    let mut v = out.into_inner().unwrap();
    v.sort_by_key(|(i, _)| *i);
    v.into_iter().map(|(_, r)| r).collect()
}

#[derive(Debug, Clone)]
struct Outcome {
    detected: bool,
    right_mode: bool,
    label: Option<&'static str>,
    rho: f64,
}

fn mode_seed(m: T2Mode, trial: usize, salt: u64) -> u64 {
    salt ^ ((m.fft.n() as u64) << 20) ^ (((m.gi.ratio() * 4096.0) as u64) << 8) ^ (u64::from(m.extended) << 5) ^ (trial as u64 * 7919)
}

fn t2_trial(m: T2Mode, trial: usize, snr_db: f64, salt: u64, with_p1: bool) -> Outcome {
    let seed = mode_seed(m, trial, salt);
    let mut rng = Rng::new(seed ^ 0xABCD);
    let cfo = random_cfo(&mut rng);
    let clip = rng.uniform() < 0.25;
    let mut p = T2Params::new(m);
    p.with_p1 = with_p1;
    let iq = t2_capture(seed, p, snr_db, cfo, clip);
    match detect_dvb(&iq, 2_048_000, 8) {
        Some(d) => Outcome { detected: true, right_mode: d.mode == m.fft && d.gi == m.gi, label: Some(d.signal_type), rho: d.rho },
        None => Outcome { detected: false, right_mode: false, label: None, rho: 0.0 },
    }
}

fn run_t2(modes: &[T2Mode], trials: usize, snr_db: f64, salt: u64) -> Vec<(T2Mode, Vec<Outcome>)> {
    let res = parallel(modes.len() * trials, |j| t2_trial(modes[j / trials], j % trials, snr_db, salt, true));
    let mut out: Vec<(T2Mode, Vec<Outcome>)> = modes.iter().map(|m| (*m, Vec::new())).collect();
    for (j, r) in res.into_iter().enumerate() {
        out[j / trials].1.push(r);
    }
    out
}

fn describe(m: T2Mode, o: &Outcome) -> String {
    format!("{} detected={} right_mode={} label={:?} rho={:.3}", m.label(), o.detected, o.right_mode, o.label, o.rho)
}

/// DVB-T cells that exist in EN 300 744: 2K/8K with guard 1/32..1/4 at 8/7/6 MHz.
fn dvbt_cells() -> Vec<(u32, FftMode, GuardInterval)> {
    let mut v = Vec::new();
    for bw in [8u32, 7, 6] {
        for mode in [FftMode::K2, FftMode::K8] {
            for gi in [GuardInterval::G1_32, GuardInterval::G1_16, GuardInterval::G1_8, GuardInterval::G1_4] {
                v.push((bw, mode, gi));
            }
        }
    }
    v
}

fn all_dvb_cp_cells() -> Vec<(u32, FftMode, GuardInterval)> {
    let mut v = Vec::new();
    for bw in [8u32, 7, 6] {
        for mode in FftMode::ALL {
            for &gi in allowed_guards(mode) {
                v.push((bw, mode, gi));
            }
        }
    }
    v
}

fn dvbt_trial(bw: u32, mode: FftMode, gi: GuardInterval, seed: u64) -> Option<DvbDetection> {
    let mut rng = Rng::new(seed ^ 0x77);
    let cfo = random_cfo(&mut rng);
    let iq = dvb_capture(seed, mode, gi, bw, 8.0, cfo, false);
    detect_dvb(&iq, 2_048_000, bw)
}

#[test]
fn t2_reduced_positive_matrix_all_51_modes_detected_and_labelled_t2() {
    let modes = t2_all_modes();
    assert_eq!(modes.len(), 51);
    let res = run_t2(&modes, 2, 8.0, 0x4A11);
    let mut bad = Vec::new();
    for (m, outs) in &res {
        for o in outs {
            if !(o.detected && o.right_mode && o.label == Some(T2)) {
                bad.push(describe(*m, o));
            }
        }
    }
    assert!(bad.is_empty(), "T2 captures not detected as dtv_dvbt2 with the right FFT/GI: {:#?}", bad);
}

#[test]
fn t2_reduced_shared_modes_are_labelled_t2_not_dvbt() {
    // 8K 1/8, 8K 1/4, 2K 1/32 (and 8K/2K neighbours) exist in DVB-T too; with a P1 they are DVB-T2.
    let mut modes = t2_shared_modes();
    modes.push(T2Mode { fft: FftMode::K32, gi: GuardInterval::G1_16, extended: false });
    let res = run_t2(&modes, 2, 10.0, 0x5B22);
    let mut bad = Vec::new();
    for (m, outs) in &res {
        for o in outs {
            if o.label != Some(T2) {
                bad.push(describe(*m, o));
            }
        }
    }
    assert!(bad.is_empty(), "shared-mode T2 captures not labelled dtv_dvbt2: {:#?}", bad);
}

#[test]
fn t2_reduced_roc_detection_at_minus_3_db_meets_the_g2_bar() {
    let modes = t2_all_modes();
    let trials = 3;
    let res = run_t2(&modes, trials, -3.0, 0x6C33);
    let n = modes.len() * trials;
    let misses: Vec<String> = res.iter().flat_map(|(m, o)| o.iter().filter(|x| !(x.detected && x.right_mode && x.label == Some(T2))).map(|x| describe(*m, x))).collect();
    // G2 bar: 99.8 percent. misses / n <= 0.2 percent.
    assert!(misses.len() * 1000 <= n * 2, "{} of {} below the bar: {:#?}", misses.len(), n, misses);
}

#[test]
fn t2_reduced_label_accuracy_over_the_synthetic_set_is_at_least_99_percent() {
    let mut correct = 0usize;
    let mut total = 0usize;
    let mut wrong = Vec::new();
    // T2 side: all 51 modes, 1 trial each, expected dtv_dvbt2.
    for (m, outs) in run_t2(&t2_all_modes(), 1, 8.0, 0x7D44) {
        for o in outs {
            total += 1;
            if o.label == Some(T2) {
                correct += 1;
            } else {
                wrong.push(format!("T2 {}", describe(m, &o)));
            }
        }
    }
    // DVB-T side: 24 shared cells, expected dtv_dvbt.
    let cells = dvbt_cells();
    let res = parallel(cells.len(), |i| dvbt_trial(cells[i].0, cells[i].1, cells[i].2, 0x7D45 + i as u64));
    for (c, d) in cells.iter().zip(res) {
        total += 1;
        if d.as_ref().map(|d| d.signal_type) == Some(T1) {
            correct += 1;
        } else {
            wrong.push(format!("DVB-T {}MHz {} {} -> {:?}", c.0, c.1.label(), c.2.label(), d.map(|d| d.signal_type)));
        }
    }
    assert!(correct * 100 >= total * 99, "label accuracy {correct}/{total}: {:#?}", wrong);
}

#[test]
fn t2_reduced_dvbt_cells_are_labelled_dvbt_and_never_t2() {
    // Every existing DVB-T cell (2K/8K, guard 1/32..1/4, 8/7/6 MHz) is still detected and labelled dtv_dvbt.
    let cells = dvbt_cells();
    let res = parallel(cells.len(), |i| dvbt_trial(cells[i].0, cells[i].1, cells[i].2, 0x8E55 + i as u64));
    let mut bad = Vec::new();
    for (c, d) in cells.iter().zip(&res) {
        match d {
            Some(d) if d.signal_type == T1 && d.mode == c.1 && d.gi == c.2 => {}
            other => bad.push(format!("{}MHz {} {} -> {:?}", c.0, c.1.label(), c.2.label(), other.as_ref().map(|d| (d.signal_type, d.fft_mode, d.guard_interval)))),
        }
    }
    assert!(bad.is_empty(), "DVB-T cells not labelled dtv_dvbt: {:#?}", bad);
}

#[test]
fn t2_reduced_cp_only_captures_never_yield_dvbt2_without_a_p1() {
    // The existing 93 CP-only cells (testgen::dvb_capture, no preamble): a detection may stay, but with no P1
    // there is no evidence of T2, so `dtv_dvbt2` must never be produced (T2-only FFT/GI inference is not enough).
    let cells = all_dvb_cp_cells();
    let res = parallel(cells.len(), |i| dvbt_trial(cells[i].0, cells[i].1, cells[i].2, 0x9F66 + i as u64));
    let bad: Vec<String> = cells
        .iter()
        .zip(&res)
        .filter(|(_, d)| d.as_ref().map(|d| d.signal_type) == Some(T2))
        .map(|(c, _)| format!("{}MHz {} {}", c.0, c.1.label(), c.2.label()))
        .collect();
    assert!(bad.is_empty(), "dtv_dvbt2 emitted for CP-only captures without P1: {:#?}", bad);
}

#[test]
fn t2_reduced_t2_stream_without_its_p1_is_never_labelled_t2() {
    // Same frame stream as the positive matrix but the P1 slots are replaced by white noise of equal power.
    let modes = t2_all_modes();
    let res = parallel(modes.len(), |i| t2_trial(modes[i], 0, 10.0, 0xA077, false));
    let bad: Vec<String> = modes.iter().zip(&res).filter(|(_, o)| o.label == Some(T2)).map(|(m, o)| describe(*m, o)).collect();
    assert!(bad.is_empty(), "dtv_dvbt2 without a P1: {:#?}", bad);
}

#[test]
fn t2_reduced_mid_window_p1_is_detected_and_labelled_t2() {
    // P1 in the middle of the capture window shifts the symbol grid under the CP fold (the hard, realistic
    // case): the P1 itself must carry the detection and the T2 label.
    let mut modes = t2_shared_modes();
    modes.push(T2Mode { fft: FftMode::K32, gi: GuardInterval::G1_16, extended: false });
    modes.push(T2Mode { fft: FftMode::K16, gi: GuardInterval::G19_256, extended: true });
    modes.push(T2Mode { fft: FftMode::K1, gi: GuardInterval::G1_8, extended: false });
    let res = parallel(modes.len(), |i| {
        let m = modes[i];
        let seed = mode_seed(m, 0, 0xE0AA);
        let mut p = T2Params::new(m);
        p.p1_at_frac = 0.5;
        let iq = t2_capture(seed, p, 8.0, 31_000.0, false);
        detect_dvb(&iq, 2_048_000, 8).map(|d| (d.signal_type, d.mode == m.fft && d.gi == m.gi))
    });
    let bad: Vec<String> = modes.iter().zip(&res).filter(|(_, r)| **r != Some((T2, true))).map(|(m, r)| format!("{} -> {:?}", m.label(), r)).collect();
    assert!(bad.is_empty(), "mid-window P1 not detected as dtv_dvbt2: {:#?}", bad);
}

fn neg_run(trials: usize, bws: &[u32]) -> (usize, usize) {
    let (mut detections, mut t2_labels) = (0usize, 0usize);
    for kind in [Negative::Awgn, Negative::Cw, Negative::Fm, Negative::Atsc] {
        for &bw in bws {
            let res = parallel(trials, |t| {
                let iq = negative_capture(0xB188 ^ (t as u64 * 104_729) ^ ((kind as u64) << 50) ^ u64::from(bw), kind);
                detect_dvb(&iq, 2_048_000, bw).map(|d| d.signal_type)
            });
            detections += res.iter().filter(|r| r.is_some()).count();
            t2_labels += res.iter().filter(|r| **r == Some(T2)).count();
        }
    }
    (detections, t2_labels)
}

#[test]
fn t2_reduced_negative_controls_have_no_detections_and_no_t2_labels() {
    let (det, t2) = neg_run(4, &[8, 7, 6]);
    assert_eq!((det, t2), (0, 0), "false detections / false dtv_dvbt2 on AWGN, CW, FM, ATSC");
}

#[test]
#[ignore]
fn t2_full_positive_roc_matrix_at_minus_3_and_3_db() {
    let modes = t2_all_modes();
    for (snr, trials, min_pct_x10) in [(-3.0, 20usize, 998usize), (3.0, 20, 1000)] {
        let res = run_t2(&modes, trials, snr, 0xC299);
        let n = modes.len() * trials;
        let ok = res.iter().flat_map(|(_, o)| o.iter()).filter(|x| x.detected && x.right_mode && x.label == Some(T2)).count();
        println!("SNR {snr:>4} dB: {ok}/{n} detected with right FFT/GI and labelled dtv_dvbt2");
        for (m, o) in &res {
            let hits = o.iter().filter(|x| x.detected && x.right_mode && x.label == Some(T2)).count();
            if hits < trials {
                println!("  {}: {}/{}", m.label(), hits, trials);
            }
        }
        assert!(ok * 1000 >= n * min_pct_x10, "SNR {snr} dB: {ok}/{n} below {}.{} percent", min_pct_x10 / 10, min_pct_x10 % 10);
    }
}

#[test]
#[ignore]
fn t2_full_negative_controls_500_trials() {
    let (det, t2) = neg_run(500, &[8, 7, 6]);
    println!("negatives: detections={det} dtv_dvbt2 labels={t2}");
    assert_eq!((det, t2), (0, 0));
}

#[test]
#[ignore]
fn t2_full_label_accuracy_over_the_synthetic_set() {
    let modes = t2_all_modes();
    let res = run_t2(&modes, 20, 3.0, 0xD3AA);
    let cells = dvbt_cells();
    let dvbt = parallel(cells.len() * 20, |j| dvbt_trial(cells[j / 20].0, cells[j / 20].1, cells[j / 20].2, 0xD3BB + j as u64));
    let t2_ok = res.iter().flat_map(|(_, o)| o.iter()).filter(|x| x.label == Some(T2)).count();
    let t1_ok = dvbt.iter().filter(|d| d.as_ref().map(|d| d.signal_type) == Some(T1)).count();
    let total = modes.len() * 20 + dvbt.len();
    println!("label accuracy: T2 {t2_ok}/{}, DVB-T {t1_ok}/{}", modes.len() * 20, dvbt.len());
    assert!((t2_ok + t1_ok) * 100 >= total * 99);
}

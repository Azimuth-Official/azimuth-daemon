//! Validation of the DVB detector on SYNTHETIC captures (test only).
//!
//! Default `cargo test` runs a reduced matrix. The full matrices from the
//! acceptance spec (200 trials per cell, 500 trials per negative control) are
//! `#[ignore]` tests; run them with
//! `cargo test --release --bin azimuth-daemon dvb_full -- --ignored --nocapture`.
//!
//! Threshold provenance (ROC survey: `dvb_roc_survey`, ignored): see the table
//! printed by that test and copied into the module docs of `dvb.rs`.

use super::dvb::*;
use super::testgen::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy)]
struct Cell {
    bw: u32,
    mode: FftMode,
    gi: GuardInterval,
}

fn all_cells() -> Vec<Cell> {
    let mut v = Vec::new();
    for bw in [8u32, 7, 6] {
        for mode in FftMode::ALL {
            for &gi in allowed_guards(mode) {
                v.push(Cell { bw, mode, gi });
            }
        }
    }
    v
}

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

struct PosTrial {
    hit: bool,
    detected_any: bool,
    true_rho: f64,
    best_rho: f64,
    best_contrast: f64,
    margin: f64,
}

fn positive_trial(cell: Cell, trial: usize, snr_db: f64, salt: u64) -> PosTrial {
    let seed = salt ^ ((cell.bw as u64) << 40) ^ ((cell.mode.n() as u64) << 20) ^ (((cell.gi.ratio() * 1024.0) as u64) << 8) ^ (trial as u64 * 7919);
    let mut rng = Rng::new(seed ^ 0xABCD);
    let cfo = random_cfo(&mut rng);
    let clip = rng.uniform() < 0.25;
    let iq = dvb_capture(seed, cell.mode, cell.gi, cell.bw, snr_db, cfo, clip);
    let x = iq_u8_to_complex(&iq);
    let scores = analyze(&x, 2_048_000.0, cell.bw);
    let det = decide(&scores, cell.bw);
    let truth = scores.iter().find(|s| s.mode == cell.mode && s.gi == cell.gi);
    let best = scores.iter().copied().fold(None::<HypothesisScore>, |a, b| match a {
        Some(a) if a.rho >= b.rho => Some(a),
        _ => Some(b),
    });
    PosTrial {
        hit: det.as_ref().map(|d| d.mode == cell.mode && d.gi == cell.gi).unwrap_or(false),
        detected_any: det.is_some(),
        true_rho: truth.map(|s| s.rho).unwrap_or(0.0),
        best_rho: best.map(|s| s.rho).unwrap_or(0.0),
        best_contrast: best.map(|s| s.contrast).unwrap_or(0.0),
        margin: best
            .map(|b| b.rho / scores.iter().filter(|s| s.mode != b.mode || s.gi != b.gi).map(|s| s.rho).fold(0.0, f64::max).max(1e-9))
            .unwrap_or(0.0),
    }
}

fn run_positive(trials: usize, snr_db: f64, cells: &[Cell], salt: u64) -> Vec<(Cell, Vec<PosTrial>)> {
    let res = parallel(cells.len() * trials, |j| positive_trial(cells[j / trials], j % trials, snr_db, salt));
    let mut out: Vec<(Cell, Vec<PosTrial>)> = cells.iter().map(|c| (*c, Vec::new())).collect();
    for (j, r) in res.into_iter().enumerate() {
        out[j / trials].1.push(r);
    }
    out
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * q).round() as usize]
}

struct NegTrial {
    detected: bool,
    rho: f64,
    contrast: f64,
    margin: f64,
}

fn negative_trial(kind: Negative, bw: u32, trial: usize, salt: u64) -> NegTrial {
    let iq = negative_capture(salt ^ (trial as u64 * 104_729) ^ ((kind as u64) << 50), kind);
    let x = iq_u8_to_complex(&iq);
    let scores = analyze(&x, 2_048_000.0, bw);
    let best = scores.iter().copied().fold(None::<HypothesisScore>, |a, b| match a {
        Some(a) if a.rho >= b.rho => Some(a),
        _ => Some(b),
    });
    let second = best
        .map(|b| scores.iter().filter(|s| s.mode != b.mode || s.gi != b.gi).map(|s| s.rho).fold(0.0, f64::max))
        .unwrap_or(0.0);
    NegTrial {
        detected: decide(&scores, bw).is_some(),
        rho: best.map(|b| b.rho).unwrap_or(0.0),
        contrast: best.map(|b| b.contrast).unwrap_or(0.0),
        margin: best.map(|b| b.rho / second.max(1e-9)).unwrap_or(0.0),
    }
}

const NEG_KINDS: [(Negative, &str); 4] = [(Negative::Awgn, "AWGN"), (Negative::Cw, "CW"), (Negative::Fm, "FM broadcast"), (Negative::Atsc, "ATSC 8VSB")];

fn run_negative(trials: usize, label: &str) -> usize {
    let mut false_pos_total = 0;
    println!("negative controls ({} trials per class per bandwidth)", trials);
    println!("{:<14} {:>3} {:>7} {:>5} {:>9} {:>9} {:>11} {:>11}", "class", "bw", "trials", "FP", "max rho", "p99 rho", "max contr", "min margin");
    for (kind, name) in NEG_KINDS {
        for bw in [8u32, 7, 6] {
            let res = parallel(trials, |t| negative_trial(kind, bw, t, 0x5EED_0000 ^ u64::from(bw)));
            let fp = res.iter().filter(|r| r.detected).count();
            false_pos_total += fp;
            let mut rhos: Vec<f64> = res.iter().map(|r| r.rho).collect();
            let max_c = res.iter().map(|r| r.contrast).fold(0.0, f64::max);
            let min_margin = res.iter().filter(|r| r.rho >= RHO_MIN).map(|r| r.margin).fold(f64::INFINITY, f64::min);
            println!(
                "{:<14} {:>3} {:>7} {:>5} {:>9.4} {:>9.4} {:>11.1} {:>11.2}",
                name, bw, trials, fp, rhos.iter().cloned().fold(0.0, f64::max), pct(&mut rhos, 0.99), max_c, min_margin
            );
        }
    }
    println!("{label}: total false positives = {false_pos_total}");
    false_pos_total
}

#[test]
fn dvb_reduced_positive_matrix_all_cells() {
    let cells = all_cells();
    assert_eq!(cells.len(), 93);
    let res = run_positive(2, 8.0, &cells, 0x1111);
    let mut misses = Vec::new();
    for (c, trials) in &res {
        for t in trials {
            if !t.hit {
                misses.push(format!("{}MHz {} {} rho_true={:.3} best_rho={:.3}", c.bw, c.mode.label(), c.gi.label(), t.true_rho, t.best_rho));
            }
        }
    }
    assert!(misses.is_empty(), "misses at 8 dB: {:#?}", misses);
}

#[test]
fn dvb_reduced_negative_controls_have_no_false_positives() {
    assert_eq!(run_negative(6, "reduced"), 0);
}

#[test]
fn dvb_field_consistency_fr_and_uk_like_signals() {
    // FR: 8K, GI 1/8, 8 MHz; UK: 8K, GI 1/32, 8 MHz. High in-band SNR as on a
    // strong local transmitter. Field values: FR rho 0.92-0.99 contrast 76-126;
    // UK rho 0.94-0.98 contrast ~45-59.
    for (gi, rho_lo, c_lo, c_hi) in [(GuardInterval::G1_8, 0.90, 60.0, 160.0), (GuardInterval::G1_32, 0.90, 30.0, 90.0)] {
        let mut rhos = Vec::new();
        let mut cons = Vec::new();
        for trial in 0..3 {
            let iq = dvb_capture(900 + trial, FftMode::K8, gi, 8, 30.0, 20_000.0, false);
            let scores = analyze(&iq_u8_to_complex(&iq), 2_048_000.0, 8);
            let det = decide(&scores, 8).expect("strong signal must be detected");
            assert_eq!((det.mode, det.gi), (FftMode::K8, gi));
            rhos.push(det.rho);
            cons.push(det.contrast);
        }
        println!("8K GI {}: rho {:?} contrast {:?}", gi.label(), rhos, cons);
        for (r, c) in rhos.iter().zip(&cons) {
            assert!(*r >= rho_lo && *r <= 1.0, "rho {r}");
            assert!(*c >= c_lo && *c <= c_hi, "contrast {c}");
        }
    }
}

#[test]
#[ignore]
fn dvb_full_positive_matrix_200_trials_at_3db() {
    let cells = all_cells();
    let res = run_positive(200, 3.0, &cells, 0x2222);
    println!("positive matrix: 200 trials per cell, in-band SNR 3 dB, CFO up to +-250 kHz (incl. +-166.667 kHz), IQ imbalance, DC, 25% clipped");
    println!("{:>3} {:<4} {:<7} {:>6} {:>9} {:>10} {:>10}", "bw", "fft", "GI", "hits", "rate", "med rho", "min rho");
    let mut fails = Vec::new();
    for (c, trials) in &res {
        let hits = trials.iter().filter(|t| t.hit).count();
        let mut rho: Vec<f64> = trials.iter().map(|t| t.true_rho).collect();
        let min_rho = rho.iter().cloned().fold(f64::INFINITY, f64::min);
        let med = pct(&mut rho, 0.5);
        println!("{:>3} {:<4} {:<7} {:>3}/{:<3} {:>8.1}% {:>10.3} {:>10.3}", c.bw, c.mode.label(), c.gi.label(), hits, trials.len(), 100.0 * hits as f64 / trials.len() as f64, med, min_rho);
        if hits * 100 < trials.len() * 99 {
            fails.push(format!("{}MHz {} {} {}/{}", c.bw, c.mode.label(), c.gi.label(), hits, trials.len()));
        }
    }
    assert!(fails.is_empty(), "cells below 99%: {:#?}", fails);
}

#[test]
#[ignore]
fn dvb_full_negative_controls_500_trials() {
    assert_eq!(run_negative(500, "full"), 0);
}

#[test]
#[ignore]
fn dvb_roc_survey() {
    let cells = all_cells();
    println!("ROC survey. Thresholds in dvb.rs: RHO_MIN={RHO_MIN} CONTRAST_MIN={CONTRAST_MIN} MARGIN_MIN={MARGIN_MIN}");
    for snr in [-6.0, -3.0, 0.0, 3.0, 10.0] {
        let res = run_positive(20, snr, &cells, 0x3333);
        let mut true_rho = Vec::new();
        let mut contr = Vec::new();
        let mut hit = 0usize;
        let mut any = 0usize;
        let mut n = 0usize;
        let mut margins = Vec::new();
        for (_, ts) in &res {
            for t in ts {
                margins.push(t.margin);
                true_rho.push(t.true_rho);
                contr.push(t.best_contrast);
                hit += t.hit as usize;
                any += t.detected_any as usize;
                n += 1;
            }
        }
        println!(
            "SNR {:>4.1} dB: n={} correct={:.1}% detected={:.1}%  true-hyp rho p1={:.3} p5={:.3} p50={:.3}  contrast p1={:.1} p5={:.1}  margin p1={:.2} min={:.2}",
            snr, n, 100.0 * hit as f64 / n as f64, 100.0 * any as f64 / n as f64, pct(&mut true_rho.clone(), 0.01), pct(&mut true_rho.clone(), 0.05), pct(&mut true_rho, 0.5), pct(&mut contr.clone(), 0.01), pct(&mut contr, 0.05), pct(&mut margins.clone(), 0.01), pct(&mut margins, 0.0)
        );
    }
    run_negative(500, "survey");
}

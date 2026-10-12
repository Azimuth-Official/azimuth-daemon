//! DVB-T2 P1 preamble detection (ETSI EN 302 755 V1.4.1 clause 9.8 and clause 7.2.1).
//!
//! Every T2 frame starts with one P1 symbol: a 1K-FFT symbol of 1024 T (A) carrying 384 active carriers
//! (Table 68), preceded by a copy C of its first 542 samples and followed by a copy B of its last 482
//! samples, both shifted up in frequency by f_SH = 1/(1024 T) (clause 9.8.2.4: C-A-B, 2048 T in all, about
//! 224 us at 8 MHz). The active carriers carry DBPSK over the concatenated S1/S2 signalling sequences
//! (Table 69: {CSS_S1, CSS_S2, CSS_S1}), scrambled with a fixed PRBS (clause 9.8.2.2).
//!
//! The detector works on a 2.048 MS/s capture (sees about 2 MHz of the 7.6 MHz channel, so about 115 of
//! the 384 active carriers) in two stages:
//!
//! 1. **Timing candidates** from the C/B self-correlation: C repeats the start of A (lag 542 T) and B
//!    repeats the end of A (lag 482 T), each with the f_SH rotation. After removing that rotation the
//!    products add coherently over the repeated part whatever the carrier offset (it only adds a constant
//!    phase), so a sliding sum peaks at the P1 start. This stage is unnormalised and pattern-agnostic and
//!    is only used to rank candidate positions.
//! 2. **Coherent confirmation and decoding** at each candidate: the A part is transformed and its
//!    visible active carriers are correlated with the known sign pattern of every (S1, S2) pair, over a grid
//!    of carrier offsets and sub-sample timings. The best normalised correlation `rho` decides; the
//!    (S1, S2) of the best pair is the decoded signalling. The DBPSK chain makes the pattern separable:
//!    the S1 and S2 parts are correlated on their own and combined with their cumulative-parity signs.

use rustfft::{num_complex::Complex, FftPlanner};

/// Stage-2 decision threshold on the normalised correlation (see `p1_survey` in the tests).
pub const P1_RHO_MIN: f64 = 0.53;
/// A candidate that decodes at least this clearly ends the search.
const P1_RHO_ACCEPT: f64 = 0.62;

const P1_C_LEN: f64 = 542.0;
const P1_B_LEN: f64 = 482.0;
const P1_A_LEN: f64 = 1024.0;
const P1_LEN: f64 = 2048.0;
/// Candidate positions kept from stage 1.
const CANDIDATES: usize = 10;
/// Half-width in samples of the exclusion zone around a chosen candidate.
const NMS_RADIUS: usize = 40;
/// Largest |carrier offset| searched, and its step (Hz).
const CFO_MAX_HZ: f64 = 252_000.0;
const CFO_STEP_HZ: f64 = 4_000.0;
/// Points of the delay-domain correlation (one period of the A symbol, 1024 T, is covered).
const DELAY_FFT: usize = 512;
/// Carriers farther than this from the tuner centre are outside the capture passband.
const VISIBLE_HZ: f64 = 0.92e6;
const MIN_VISIBLE: usize = 40;
const NFFT: usize = 2048;

/// Active carrier indices k_P1(0..383), EN 302 755 Table 68 (clause 9.8.2.1).
const ACTIVE: [u16; 384] = [
    44, 45, 47, 51, 54, 59, 62, 64, 65, 66, 70, 75, 78, 80, 81, 82,
    84, 85, 87, 88, 89, 90, 94, 96, 97, 98, 102, 107, 110, 112, 113, 114,
    116, 117, 119, 120, 121, 122, 124, 125, 127, 131, 132, 133, 135, 136, 137, 138,
    142, 144, 145, 146, 148, 149, 151, 152, 153, 154, 158, 160, 161, 162, 166, 171,
    172, 173, 175, 179, 182, 187, 190, 192, 193, 194, 198, 203, 206, 208, 209, 210,
    212, 213, 215, 216, 217, 218, 222, 224, 225, 226, 230, 235, 238, 240, 241, 242,
    244, 245, 247, 248, 249, 250, 252, 253, 255, 259, 260, 261, 263, 264, 265, 266,
    270, 272, 273, 274, 276, 277, 279, 280, 281, 282, 286, 288, 289, 290, 294, 299,
    300, 301, 303, 307, 310, 315, 318, 320, 321, 322, 326, 331, 334, 336, 337, 338,
    340, 341, 343, 344, 345, 346, 350, 352, 353, 354, 358, 363, 364, 365, 367, 371,
    374, 379, 382, 384, 385, 386, 390, 395, 396, 397, 399, 403, 406, 411, 412, 413,
    415, 419, 420, 421, 423, 424, 425, 426, 428, 429, 431, 435, 438, 443, 446, 448,
    449, 450, 454, 459, 462, 464, 465, 466, 468, 469, 471, 472, 473, 474, 478, 480,
    481, 482, 486, 491, 494, 496, 497, 498, 500, 501, 503, 504, 505, 506, 508, 509,
    511, 515, 516, 517, 519, 520, 521, 522, 526, 528, 529, 530, 532, 533, 535, 536,
    537, 538, 542, 544, 545, 546, 550, 555, 558, 560, 561, 562, 564, 565, 567, 568,
    569, 570, 572, 573, 575, 579, 580, 581, 583, 584, 585, 586, 588, 589, 591, 595,
    598, 603, 604, 605, 607, 611, 612, 613, 615, 616, 617, 618, 622, 624, 625, 626,
    628, 629, 631, 632, 633, 634, 636, 637, 639, 643, 644, 645, 647, 648, 649, 650,
    654, 656, 657, 658, 660, 661, 663, 664, 665, 666, 670, 672, 673, 674, 678, 683,
    684, 689, 692, 696, 698, 699, 701, 702, 703, 704, 706, 707, 708, 712, 714, 715,
    717, 718, 719, 720, 722, 723, 725, 726, 727, 729, 733, 734, 735, 736, 738, 739,
    740, 744, 746, 747, 748, 753, 756, 760, 762, 763, 765, 766, 767, 768, 770, 771,
    772, 776, 778, 779, 780, 785, 788, 792, 794, 795, 796, 801, 805, 806, 807, 809,
];

/// S1 sequences (3-bit S1, 64 bits), EN 302 755 Table 69.
const S1_HEX: [&str; 8] = [
    "124721741D482E7B",
    "47127421481D7B2E",
    "217412472E7B1D48",
    "742147127B2E481D",
    "1D482E7B12472174",
    "481D7B2E47127421",
    "2E7B1D4821741247",
    "7B2E481D74214712",
];

/// S2 sequences (4-bit S2 = field 1 followed by field 2, 256 bits), EN 302 755 Table 69.
const S2_HEX: [&str; 16] = [
    "121D4748212E747B1D1248472E217B7412E247B721D174841DED48B82EDE7B8B",
    "4748121D747B212E48471D127B742E2147B712E2748421D148B81DED7B8B2EDE",
    "212E747B121D47482E217B741D12484721D1748412E247B72EDE7B8B1DED48B8",
    "747B212E4748121D7B742E2148471D12748421D147B712E27B8B2EDE48B81DED",
    "1D1248472E217B74121D4748212E747B1DED48B82EDE7B8B12E247B721D17484",
    "48471D127B742E214748121D747B212E48B81DED7B8B2EDE47B712E2748421D1",
    "2E217B741D124847212E747B121D47482EDE7B8B1DED48B821D1748412E247B7",
    "7B742E2148471D12747B212E4748121D7B8B2EDE48B81DED748421D147B712E2",
    "12E247B721D174841DED48B82EDE7B8B121D4748212E747B1D1248472E217B74",
    "47B712E2748421D148B81DED7B8B2EDE4748121D747B212E48471D127B742E21",
    "21D1748412E247B72EDE7B8B1DED48B8212E747B121D47482E217B741D124847",
    "748421D147B712E27B8B2EDE48B81DED747B212E4748121D7B742E2148471D12",
    "1DED48B82EDE7B8B12E247B721D174841D1248472E217B74121D4748212E747B",
    "48B81DED7B8B2EDE47B712E2748421D148471D127B742E214748121D747B212E",
    "2EDE7B8B1DED48B821D1748412E247B72E217B741D124847212E747B121D4748",
    "7B8B2EDE48B81DED748421D147B712E27B742E2148471D12747B212E4748121D",
];

/// S1 values that can start a frame (Table 18): T2 SISO, T2 MISO, non-T2 (FEF), T2-LITE SISO / MISO.
const S1_VALID: [usize; 5] = [0, 1, 2, 3, 4];

/// Result of a P1 detection.
#[derive(Debug, Clone, Copy)]
pub struct P1Detection {
    /// Decoded S1 (3 bits) and S2 (4 bits) signalling fields.
    pub s1: u8,
    pub s2: u8,
    /// Normalised correlation of the visible active carriers with the decoded pattern.
    pub rho: f64,
    /// Carrier offset estimate in Hz (signal minus tuner centre).
    pub cfo_hz: f64,
    /// Estimated first sample of the P1 symbol (start of C) in the capture.
    pub start: f64,
    /// Length of the whole P1 symbol in capture samples (2048 T).
    pub len: f64,
}

fn hex_bits(hex: &str) -> Vec<u8> {
    hex.chars()
        .flat_map(|c| {
            let v = c.to_digit(16).expect("hex digit") as u8;
            (0..4).rev().map(move |b| (v >> b) & 1)
        })
        .collect()
}

/// Scrambling PRBS of clause 9.8.2.2: x^15 + x^14 + 1 started from '100111001000110'.
fn prbs(n: usize) -> Vec<f32> {
    let mut r: [u8; 15] = [1, 0, 0, 1, 1, 1, 0, 0, 1, 0, 0, 0, 1, 1, 0];
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let b = r[13] ^ r[14];
        out.push(if b == 0 { 1.0 } else { -1.0 });
        for i in (1..15).rev() {
            r[i] = r[i - 1];
        }
        r[0] = b;
    }
    out
}

/// Sign pattern of one signalling segment: the DBPSK running sign (starting at +1) times the PRBS,
/// plus the segment's total parity (the sign the following segment starts from).
fn segment_signs(bits: &[u8], scramble: &[f32]) -> (Vec<f32>, f32) {
    let mut d = 1.0f32;
    let signs = bits
        .iter()
        .zip(scramble)
        .map(|(&b, &p)| {
            if b == 1 {
                d = -d;
            }
            d * p
        })
        .collect();
    (signs, d)
}

/// Known sign tables: segment 1a (S1, carriers 0..64), 2 (S2, 64..320) and 1b (S1, 320..384).
struct Tables {
    s1a: Vec<Vec<f32>>,
    s1b: Vec<Vec<f32>>,
    s2: Vec<Vec<f32>>,
    par_s1: Vec<f32>,
    par_s2: Vec<f32>,
}

fn tables() -> Tables {
    let pr = prbs(384);
    let (mut s1a, mut s1b, mut par_s1) = (Vec::new(), Vec::new(), Vec::new());
    for hex in S1_HEX {
        let bits = hex_bits(hex);
        let (a, p) = segment_signs(&bits, &pr[..64]);
        let (b, _) = segment_signs(&bits, &pr[320..384]);
        s1a.push(a);
        s1b.push(b);
        par_s1.push(p);
    }
    let (mut s2, mut par_s2) = (Vec::new(), Vec::new());
    for hex in S2_HEX {
        let bits = hex_bits(hex);
        let (t, p) = segment_signs(&bits, &pr[64..320]);
        s2.push(t);
        par_s2.push(p);
    }
    Tables { s1a, s1b, s2, par_s1, par_s2 }
}

/// `x` advanced by a fractional number of samples (FFT-domain delay), for a sub-sample correlation lag.
fn advance(x: &[Complex<f32>], frac: f64) -> Vec<Complex<f32>> {
    if frac.abs() < 0.02 {
        return x.to_vec();
    }
    let m = x.len();
    let mut planner = FftPlanner::<f32>::new();
    let mut s = x.to_vec();
    planner.plan_fft_forward(m).process(&mut s);
    for (k, v) in s.iter_mut().enumerate() {
        let kk = if k <= m / 2 { k as f64 } else { k as f64 - m as f64 };
        let ph = 2.0 * std::f64::consts::PI * kk * frac / m as f64;
        *v *= Complex::new(ph.cos() as f32, ph.sin() as f32);
    }
    planner.plan_fft_inverse(m).process(&mut s);
    let sc = 1.0 / m as f32;
    s.iter_mut().for_each(|v| *v *= sc);
    s
}

/// Stage 1: candidate P1 start positions (sample index of the start of C), best first.
fn timing_candidates(x: &[Complex<f32>], s: f64) -> Vec<usize> {
    let m = x.len();
    let (lc, lb) = (P1_C_LEN * s, P1_B_LEN * s);
    let (li_c, li_b) = (lc.round() as usize, lb.round() as usize);
    let (kc, kb) = (li_c, li_b);
    let ramp = 2.0 * std::f64::consts::PI / (P1_A_LEN * s);
    let build = |li: usize, frac: f64, sign: f64| -> Vec<Complex<f64>> {
        let y = advance(x, frac);
        let mut pre = Vec::with_capacity(m - li + 1);
        let mut acc = Complex::new(0.0f64, 0.0f64);
        pre.push(acc);
        for n in 0..m - li {
            let a = x[n];
            let b = y[n + li];
            let prod = Complex::new(f64::from(b.re), f64::from(b.im)) * Complex::new(f64::from(a.re), -f64::from(a.im));
            let ph = sign * ramp * n as f64;
            acc += prod * Complex::new(ph.cos(), ph.sin());
            pre.push(acc);
        }
        pre
    };
    // C copies the start of A: x[t + lag] x*[t] carries exp(-j f_SH t); B copies the end of A the other way.
    let pc = build(li_c, lc - li_c as f64, 1.0);
    let pb = build(li_b, lb - li_b as f64, -1.0);
    let b_off = (P1_C_LEN + P1_A_LEN - P1_B_LEN) * s; // start of the A samples B repeats, from the P1 start
    let total = (P1_LEN * s).ceil() as usize;
    if m < total + 64 {
        return Vec::new();
    }
    let mut metric = vec![0.0f64; m - total];
    for (t, v) in metric.iter_mut().enumerate() {
        let tb = (t as f64 + b_off).round() as usize;
        if t + kc >= pc.len() || tb + kb >= pb.len() {
            continue;
        }
        *v = (pc[t + kc] - pc[t]).norm() + (pb[tb + kb] - pb[tb]).norm();
    }
    let mut out = Vec::new();
    for _ in 0..CANDIDATES {
        let (mut best, mut bi) = (0.0f64, usize::MAX);
        for (i, &v) in metric.iter().enumerate() {
            if v > best {
                best = v;
                bi = i;
            }
        }
        if bi == usize::MAX {
            break;
        }
        out.push(bi);
        let lo = bi.saturating_sub(NMS_RADIUS);
        let hi = (bi + NMS_RADIUS).min(metric.len());
        metric[lo..hi].iter_mut().for_each(|v| *v = 0.0);
    }
    out
}

/// Search for a P1 symbol in a prepared (DC-free) capture. `t_us` is the elementary period T of the
/// channel bandwidth (EN 302 755 Table 65). Returns the best candidate if its normalised correlation
/// reaches `rho_min`.
pub fn detect_p1_with(x: &[Complex<f32>], sample_rate: f64, t_us: f64, rho_min: f64) -> Option<P1Detection> {
    let s = sample_rate * t_us * 1e-6; // capture samples per T
    let w = (P1_A_LEN * s).round() as usize;
    if x.len() < 32_768 || w + 2 > NFFT {
        return None;
    }
    let tb = tables();
    let t_sec = t_us * 1e-6;
    // Carrier slot m = k - 426 and its frequency m / (1024 T) relative to the P1 centre.
    let slot: Vec<i64> = ACTIVE.iter().map(|&k| i64::from(k) - 426).collect();
    let nu: Vec<f64> = slot.iter().map(|&m| m as f64 / (P1_A_LEN * t_sec)).collect();
    let n_f = (CFO_MAX_HZ / CFO_STEP_HZ) as i64;
    let bin_hz = sample_rate / NFFT as f64;
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(NFFT);
    let ifft = planner.plan_fft_inverse(DELAY_FFT);
    // A sub-sample delay delta (samples) multiplies carrier slot m by e^{+j 2 pi m delta / (1024 s)}, so the
    // correlation for every delay on a grid of 1024 s / DELAY_FFT samples is one inverse FFT over the slots.
    let delay_step = P1_A_LEN * s / DELAY_FFT as f64;
    let zero = Complex::new(0.0f32, 0.0f32);

    let mut best: Option<P1Detection> = None;
    let mut best_rho2 = 0.0f32;
    for t0 in timing_candidates(x, s) {
        // Candidates come best first; once one decodes clearly the rest cannot matter.
        if best_rho2.sqrt() as f64 >= P1_RHO_ACCEPT {
            break;
        }
        let n0 = (t0 as f64 + P1_C_LEN * s).round() as usize;
        if n0 + w >= x.len() {
            continue;
        }
        let mut win = vec![zero; NFFT];
        win[..w].copy_from_slice(&x[n0..n0 + w]);
        fft.process(&mut win);

        for jf in -n_f..=n_f {
            let f = jf as f64 * CFO_STEP_HZ;
            // Visible carriers (index, spectrum value) at this carrier offset.
            let mut vis: Vec<(usize, Complex<f32>)> = Vec::with_capacity(160);
            let mut energy = 0.0f32;
            for (i, &v) in nu.iter().enumerate() {
                let fr = v + f;
                if fr.abs() > VISIBLE_HZ {
                    continue;
                }
                let bin = (fr / bin_hz).round() as i64;
                let c = win[bin.rem_euclid(NFFT as i64) as usize];
                energy += c.norm_sqr();
                vis.push((i, c));
            }
            if vis.len() < MIN_VISIBLE || energy <= 0.0 {
                continue;
            }
            let norm2 = 1.0 / (vis.len() as f32 * energy);
            // Delay-domain correlation of one signalling segment (carriers lo..hi) against one sign pattern.
            let seg = |signs: &[f32], lo: usize, hi: usize| -> Vec<Complex<f32>> {
                let mut v = vec![zero; DELAY_FFT];
                for &(i, c) in vis.iter().filter(|(i, _)| *i >= lo && *i < hi) {
                    v[slot[i].rem_euclid(DELAY_FFT as i64) as usize] += c * signs[i - lo];
                }
                ifft.process(&mut v);
                v
            };
            let z2: Vec<Vec<Complex<f32>>> = (0..16).map(|s2| seg(&tb.s2[s2], 64, 320)).collect();
            for &s1 in &S1_VALID {
                let z1a = seg(&tb.s1a[s1], 0, 64);
                let z1b = seg(&tb.s1b[s1], 320, 384);
                let pa = tb.par_s1[s1];
                for (s2, z2s) in z2.iter().enumerate() {
                    let pb = pa * tb.par_s2[s2];
                    for j in 0..DELAY_FFT {
                        let rho2 = (z1a[j] + z2s[j] * pa + z1b[j] * pb).norm_sqr() * norm2;
                        if rho2 > best_rho2 {
                            best_rho2 = rho2;
                            let jj = if j <= DELAY_FFT / 2 { j as f64 } else { j as f64 - DELAY_FFT as f64 };
                            best = Some(P1Detection {
                                s1: s1 as u8,
                                s2: s2 as u8,
                                rho: f64::from(rho2.sqrt()),
                                cfo_hz: f,
                                start: n0 as f64 + jj * delay_step - P1_C_LEN * s,
                                len: P1_LEN * s,
                            });
                        }
                    }
                }
            }
        }
    }
    best.filter(|b| b.rho >= rho_min)
}

/// `detect_p1_with` at the production threshold.
pub fn detect_p1(x: &[Complex<f32>], sample_rate: f64, t_us: f64) -> Option<P1Detection> {
    detect_p1_with(x, sample_rate, t_us, P1_RHO_MIN)
}

#[cfg(test)]
mod tests {
    use super::super::iq_u8_to_complex;
    use crate::sdr::dtv::testgen::{negative_capture, random_cfo, Negative, Rng};
    use crate::sdr::dtv::testgen_t2::*;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn parallel<T: Send, F: Fn(usize) -> T + Sync>(jobs: usize, f: F) -> Vec<T> {
        let next = AtomicUsize::new(0);
        let out: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::new());
        std::thread::scope(|sc| {
            for _ in 0..12 {
                sc.spawn(|| loop {
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

    #[test]
    #[ignore]
    fn p1_survey() {
        let modes = t2_all_modes();
        let snr: f64 = std::env::var("P1_SNR").ok().and_then(|v| v.parse().ok()).unwrap_or(-3.0);
        let trials = 4usize;
        let t0 = std::time::Instant::now();
        let res = parallel(modes.len() * trials, |j| {
            let m = modes[j / trials];
            let seed = 1000 + j as u64 * 31;
            let mut rng = Rng::new(seed ^ 0xABCD);
            let cfo = random_cfo(&mut rng);
            let iq = t2_capture(seed, T2Params::new(m), snr, cfo, rng.uniform() < 0.25);
            let x = iq_u8_to_complex(&iq);
            detect_p1_with(&x, 2_048_000.0, 7.0 / 64.0, 0.0).map(|d| (d.rho, d.s1, d.s2, d.cfo_hz - cfo, m.fft.label(), d.start))
        });
        let mut rhos: Vec<f64> = res.iter().map(|r| r.map_or(0.0, |d| d.0)).collect();
        rhos.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("elapsed {:?} for {} captures", t0.elapsed(), res.len());
        println!("T2 SNR {snr}: rho min {:.3} p5 {:.3} median {:.3}", rhos[0], rhos[rhos.len() / 20], rhos[rhos.len() / 2]);
        let bad_s2 = res.iter().zip(0..).filter(|(r, j)| r.is_none_or(|d| d.2 != (s2_field1(modes[j / trials].fft, modes[j / trials].gi) << 1) & 15 || d.1 != 0)).count();
        println!("decoded S1/S2 wrong: {bad_s2}");
        let cfo_err: Vec<f64> = res.iter().filter_map(|r| r.map(|d| d.3.abs())).collect();
        for (j, r) in res.iter().enumerate() {
            let m = modes[j / trials];
            let want = (s2_field1(m.fft, m.gi) << 1) & 15;
            if r.is_none_or(|d| d.0 < 0.55 || d.2 != want || d.1 != 0) {
                println!("  weak: {} j={j} {:?}", m.label(), r);
            }
        }
        println!("max cfo err {:.0} Hz", cfo_err.iter().cloned().fold(0.0, f64::max));
        let mut neg = Vec::new();
        for kind in [Negative::Awgn, Negative::Cw, Negative::Fm, Negative::Atsc] {
            let nneg: usize = std::env::var("P1_NEG").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
            let r = parallel(nneg, |t| {
                let iq = negative_capture(0xB188 ^ (t as u64 * 104_729) ^ ((kind as u64) << 50), kind);
                let x = iq_u8_to_complex(&iq);
                detect_p1_with(&x, 2_048_000.0, 7.0 / 64.0, 0.0).map_or(0.0, |d| d.rho)
            });
            let mx = r.iter().cloned().fold(0.0, f64::max);
            neg.push((format!("{kind:?}"), mx));
        }
        println!("negative max rho: {:?}", neg);
    }

    #[test]
    fn a_p1_is_found_and_its_signalling_decoded() {
        let modes = t2_all_modes();
        for (i, idx) in [0usize, 13, 30, 44].into_iter().enumerate() {
            let m = modes[idx];
            let cfo = [-210_000.0, 35_000.0, 166_667.0, -7_000.0][i];
            let iq = t2_capture(40 + i as u64, T2Params::new(m), 6.0, cfo, false);
            let d = detect_p1(&iq_u8_to_complex(&iq), 2_048_000.0, 7.0 / 64.0).unwrap_or_else(|| panic!("{} not found", m.label()));
            assert_eq!(d.s1, 0, "{}: S1 (T2_SISO)", m.label());
            assert_eq!(d.s2, (s2_field1(m.fft, m.gi) << 1) & 15, "{}: S2", m.label());
            assert!((d.cfo_hz - cfo).abs() < 3_000.0, "{}: cfo {} vs {}", m.label(), d.cfo_hz, cfo);
            assert!(d.rho > 0.6, "{}: rho {}", m.label(), d.rho);
        }
    }

    #[test]
    fn no_p1_in_noise_cp_only_ofdm_or_a_p1_less_t2_stream() {
        let x = iq_u8_to_complex(&negative_capture(5, Negative::Awgn));
        assert!(detect_p1(&x, 2_048_000.0, 7.0 / 64.0).is_none());
        let mode = T2Mode { fft: super::super::FftMode::K8, gi: super::super::GuardInterval::G1_8, extended: false };
        let mut p = T2Params::new(mode);
        p.with_p1 = false;
        let x = iq_u8_to_complex(&t2_capture(6, p, 12.0, 0.0, false));
        assert!(detect_p1(&x, 2_048_000.0, 7.0 / 64.0).is_none(), "white noise in the P1 slot is not a P1");
        // Control: the same stream with its P1 is found.
        let x = iq_u8_to_complex(&t2_capture(6, T2Params::new(mode), 12.0, 0.0, false));
        assert!(detect_p1(&x, 2_048_000.0, 7.0 / 64.0).is_some());
    }

    #[test]
    fn short_or_unknown_input_yields_none() {
        assert!(detect_p1(&[], 2_048_000.0, 7.0 / 64.0).is_none());
        assert!(detect_p1(&vec![Complex::new(0.5f32, 0.1); 20_000], 2_048_000.0, 7.0 / 64.0).is_none());
    }
}

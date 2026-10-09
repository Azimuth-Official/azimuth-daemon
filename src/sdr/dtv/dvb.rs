//! DVB-T / DVB-T2 detection by cyclic-prefix (guard interval) correlation.
//!
//! A DVB OFDM symbol repeats its last `G = g * Tu` samples as a cyclic prefix,
//! so the received signal correlates with itself at lag `Tu` inside every guard
//! interval and (to first order) nowhere else. The detector tests every
//! (FFT size, guard interval) combination the standards allow for the channel
//! bandwidth:
//!
//! 1. lag `L = N * T * fs` samples (fractional; the sub-sample part is applied
//!    with an FFT-domain fractional delay, because the 2.048 MS/s capture is
//!    critically sampled and cubic interpolation would lose the band edges),
//! 2. per-sample product `x[n] * conj(x[n + L])` and power `(|x[n]|^2 + |x[n+L]|^2) / 2`,
//! 3. a sliding sum over the guard length `G`,
//! 4. folding at the symbol period `Ts = L * (1 + g)` (fractional, tracked with
//!    a phase accumulator), coherently over all symbols in the capture. The CFO
//!    phase `exp(-j 2 pi f Tu)` is identical in every symbol, so the coherent
//!    fold needs no frequency correction and works for any carrier offset.
//!
//! `rho` is the normalised magnitude at the best fold phase (the CP correlation
//! coefficient). `contrast` is `rho` divided by the median over fold phases, i.e.
//! how far the CP peak stands above the correlation floor. A continuous wave
//! or an FM carrier correlates at every phase (contrast near 1); noise gives a
//! small `rho`; only a periodic CP structure gives both.
//!
//! Standards used (texts read from the ETSI PDFs):
//! * ETSI EN 300 744 V1.6.2 (DVB-T), clause 4.4 (OFDM frame structure): elementary
//!   period T = 7/64 us (8 MHz), 1/8 us (7 MHz), 7/48 us (6 MHz); Table 4 gives
//!   Tu = 8192 T (8K) and 2048 T (2K); guard interval fractions 1/4, 1/8, 1/16, 1/32.
//! * ETSI EN 302 755 V1.4.1 (DVB-T2), clause 9.5 Table 65 (elementary period per
//!   bandwidth), Table 66 (Tu = N T for 1K..32K), clause 9.7 Table 67 (allowed
//!   guard interval per FFT size; "NA" combinations are not tested).
//!
//! The cyclic-prefix absolute carrier offset is only observable modulo the
//! carrier spacing 1/Tu (the 2 MHz capture sees a flat part of the spectrum), so
//! `cfo_hz` is the fractional carrier offset in [-1/(2 Tu), +1/(2 Tu)).

use rustfft::{num_complex::Complex, FftPlanner};

/// Capture length the detector is validated for (samples at 2.048 MS/s).
pub const CAPTURE_SAMPLES: usize = 131_072;

/// Decision thresholds, from the ROC survey (`validation.rs::dvb_roc_survey`,
/// 93 mode/GI/bandwidth cells x 20 trials per SNR, synthetic RTL-like captures
/// with CFO, IQ imbalance, DC and 25 percent clipped trials):
///
/// | in-band SNR | correct mode+GI | true-hypothesis rho p1 | contrast p1 | margin min |
/// |---|---|---|---|---|
/// | -6 dB | 0.0 % | 0.171 | 6.3 | 1.29 |
/// | -3 dB | 99.8 % | 0.300 | 11.1 | 1.59 |
/// |  0 dB | 100 % | 0.466 | 16.7 | 1.60 |
/// |  3 dB | 100 % | 0.632 | 22.3 | 1.60 |
/// | 10 dB | 100 % | 0.875 | 30.3 | 1.61 |
///
/// Negative controls (500 trials per class per bandwidth, 6000 captures, 0
/// false positives): AWGN max rho 0.138 / max contrast 4.9; CW max rho 1.00 /
/// max contrast 1.4; FM broadcast max rho 0.84 / max contrast 5.6; ATSC 8VSB
/// max rho 0.89 / max contrast 4.9. Noise is rejected by `rho`; every
/// continuous-wave-like source (CW, FM, the 8VSB pilot) is rejected by
/// `contrast`, which stays below 5.6 for all of them while true OFDM stays
/// above 11 down to -3 dB. `CONTRAST_MIN` sits between the two clouds
/// (about 1.8x above the worst negative, 1.1x below the -3 dB positive p1) and
/// `RHO_MIN` is 1.8x the worst AWGN value.
pub const RHO_MIN: f64 = 0.25;
pub const CONTRAST_MIN: f64 = 10.0;
/// Required ratio between the best hypothesis and the best hypothesis that
/// would give a different (FFT size, guard interval) answer.
pub const MARGIN_MIN: f64 = 1.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FftMode {
    K1,
    K2,
    K4,
    K8,
    K16,
    K32,
}

impl FftMode {
    pub const ALL: [FftMode; 6] = [FftMode::K1, FftMode::K2, FftMode::K4, FftMode::K8, FftMode::K16, FftMode::K32];

    pub fn n(self) -> usize {
        match self {
            FftMode::K1 => 1024,
            FftMode::K2 => 2048,
            FftMode::K4 => 4096,
            FftMode::K8 => 8192,
            FftMode::K16 => 16384,
            FftMode::K32 => 32768,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FftMode::K1 => "1K",
            FftMode::K2 => "2K",
            FftMode::K4 => "4K",
            FftMode::K8 => "8K",
            FftMode::K16 => "16K",
            FftMode::K32 => "32K",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardInterval {
    G1_128,
    G1_32,
    G1_16,
    G19_256,
    G1_8,
    G19_128,
    G1_4,
}

impl GuardInterval {
    pub fn ratio(self) -> f64 {
        match self {
            GuardInterval::G1_128 => 1.0 / 128.0,
            GuardInterval::G1_32 => 1.0 / 32.0,
            GuardInterval::G1_16 => 1.0 / 16.0,
            GuardInterval::G19_256 => 19.0 / 256.0,
            GuardInterval::G1_8 => 1.0 / 8.0,
            GuardInterval::G19_128 => 19.0 / 128.0,
            GuardInterval::G1_4 => 1.0 / 4.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GuardInterval::G1_128 => "1/128",
            GuardInterval::G1_32 => "1/32",
            GuardInterval::G1_16 => "1/16",
            GuardInterval::G19_256 => "19/256",
            GuardInterval::G1_8 => "1/8",
            GuardInterval::G19_128 => "19/128",
            GuardInterval::G1_4 => "1/4",
        }
    }
}

use GuardInterval as G;

/// Guard intervals allowed per FFT size: EN 302 755 V1.4.1 Table 67 (clause 9.7).
/// DVB-T (EN 300 744 clause 4.4) 2K/8K with 1/4..1/32 is a subset of this table.
pub fn allowed_guards(mode: FftMode) -> &'static [GuardInterval] {
    match mode {
        FftMode::K32 => &[G::G1_128, G::G1_32, G::G1_16, G::G19_256, G::G1_8, G::G19_128],
        FftMode::K16 | FftMode::K8 => &[G::G1_128, G::G1_32, G::G1_16, G::G19_256, G::G1_8, G::G19_128, G::G1_4],
        FftMode::K4 | FftMode::K2 => &[G::G1_32, G::G1_16, G::G1_8, G::G1_4],
        FftMode::K1 => &[G::G1_16, G::G1_8, G::G1_4],
    }
}

/// Elementary period T in microseconds: EN 300 744 clause 4.4, EN 302 755 Table 65.
pub fn elementary_period_us(bandwidth_mhz: u32) -> Option<f64> {
    match bandwidth_mhz {
        8 => Some(7.0 / 64.0),
        7 => Some(1.0 / 8.0),
        6 => Some(7.0 / 48.0),
        _ => None,
    }
}

/// Signal type per contract C1 and `t2_possible`: true for every `dtv_dvbt2`
/// result (definitely T2) and for a `dtv_dvbt` reading that could also be T2
/// (2K/8K with guard interval 1/4..1/32).
pub fn classify(mode: FftMode, gi: GuardInterval) -> (&'static str, bool) {
    let t2_only_mode = matches!(mode, FftMode::K1 | FftMode::K4 | FftMode::K16 | FftMode::K32);
    let t2_only_gi = matches!(gi, GuardInterval::G1_128 | GuardInterval::G19_256 | GuardInterval::G19_128);
    if t2_only_mode || t2_only_gi {
        ("dtv_dvbt2", true)
    } else {
        ("dtv_dvbt", true)
    }
}

/// One (FFT size, guard interval) hypothesis score.
#[derive(Debug, Clone, Copy)]
pub struct HypothesisScore {
    pub mode: FftMode,
    pub gi: GuardInterval,
    pub rho: f64,
    pub contrast: f64,
    pub cfo_hz: f64,
}

/// Detector output (contract C7).
#[derive(Debug, Clone)]
pub struct DvbDetection {
    pub fft_mode: &'static str,
    pub guard_interval: &'static str,
    pub rho: f64,
    pub contrast: f64,
    pub cfo_hz: f64,
    pub bandwidth_mhz: u32,
    pub t2_possible: bool,
    pub signal_type: &'static str,
    pub mode: FftMode,
    pub gi: GuardInterval,
}

/// Convert RTL-SDR interleaved u8 IQ to centred complex samples (DC removed).
pub fn iq_u8_to_complex(iq_bytes: &[u8]) -> Vec<Complex<f32>> {
    let mut x: Vec<Complex<f32>> = (0..iq_bytes.len() / 2)
        .map(|i| Complex::new((f32::from(iq_bytes[2 * i]) - 127.5) / 127.5, (f32::from(iq_bytes[2 * i + 1]) - 127.5) / 127.5))
        .collect();
    if !x.is_empty() {
        let n = x.len() as f64;
        let (mut sr, mut si) = (0.0f64, 0.0f64);
        for v in &x {
            sr += f64::from(v.re);
            si += f64::from(v.im);
        }
        let (mr, mi) = ((sr / n) as f32, (si / n) as f32);
        for v in &mut x {
            v.re -= mr;
            v.im -= mi;
        }
    }
    x
}

/// Score every allowed hypothesis for `bandwidth_mhz` on a prepared capture.
pub fn analyze(x: &[Complex<f32>], sample_rate: f64, bandwidth_mhz: u32) -> Vec<HypothesisScore> {
    let t_us = match elementary_period_us(bandwidth_mhz) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let m = x.len();
    if m < 32_768 {
        return Vec::new();
    }
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(m);
    let inv = planner.plan_fft_inverse(m);
    let mut spectrum: Option<Vec<Complex<f32>>> = None;
    let mut out = Vec::new();

    for mode in FftMode::ALL {
        let l = mode.n() as f64 * t_us * 1e-6 * sample_rate;
        let li = l.round() as usize;
        let frac = l - li as f64;
        // Need several symbol repeats of data after the lag.
        if m < 5 * li + 128 {
            continue;
        }
        let delayed_owned: Vec<Complex<f32>>;
        let y: &[Complex<f32>] = if frac.abs() < 0.02 {
            x
        } else {
            let spec = spectrum.get_or_insert_with(|| {
                let mut s = x.to_vec();
                fwd.process(&mut s);
                s
            });
            let mut s = spec.clone();
            for (k, v) in s.iter_mut().enumerate() {
                let kk = if k <= m / 2 { k as f64 } else { k as f64 - m as f64 };
                let ph = 2.0 * std::f64::consts::PI * kk * frac / m as f64;
                *v *= Complex::new(ph.cos() as f32, ph.sin() as f32);
            }
            inv.process(&mut s);
            let sc = 1.0 / m as f32;
            for v in &mut s {
                *v *= sc;
            }
            delayed_owned = s;
            &delayed_owned
        };
        let nmax = m - li - 64;
        let mut pre_c = Vec::with_capacity(nmax + 1);
        let mut pre_e = Vec::with_capacity(nmax + 1);
        let (mut ac, mut ae) = (Complex::new(0.0f64, 0.0f64), 0.0f64);
        pre_c.push(ac);
        pre_e.push(ae);
        for n in 32..32 + nmax {
            let a = x[n];
            let b = y[n + li];
            ac += Complex::new(f64::from(a.re), f64::from(a.im)) * Complex::new(f64::from(b.re), -f64::from(b.im));
            ae += 0.5 * f64::from(a.norm_sqr() + b.norm_sqr());
            pre_c.push(ac);
            pre_e.push(ae);
        }
        let tu_s = l / sample_rate;
        for &gi in allowed_guards(mode) {
            let g_len = ((l * gi.ratio()).round() as usize).max(2);
            let ts = l * (1.0 + gi.ratio());
            let nb = ts.round() as usize;
            if nmax <= g_len + 2 * nb {
                continue;
            }
            let mut cf = vec![Complex::new(0.0f64, 0.0f64); nb];
            let mut ef = vec![0.0f64; nb];
            let inv_ts = 1.0 / ts;
            for n in 0..(nmax - g_len) {
                let ph = (n as f64 * inv_ts).fract();
                let idx = ((ph * nb as f64) as usize).min(nb - 1);
                cf[idx] += pre_c[n + g_len] - pre_c[n];
                ef[idx] += pre_e[n + g_len] - pre_e[n];
            }
            let mut mag = vec![0.0f64; nb];
            let (mut best, mut best_i) = (0.0f64, 0usize);
            for i in 0..nb {
                let v = if ef[i] > 0.0 { cf[i].norm() / ef[i] } else { 0.0 };
                mag[i] = v;
                if v > best {
                    best = v;
                    best_i = i;
                }
            }
            let mut sorted = mag.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = sorted[sorted.len() / 2].max(1e-9);
            let cfo = -cf[best_i].arg() / (2.0 * std::f64::consts::PI * tu_s);
            out.push(HypothesisScore { mode, gi, rho: best, contrast: best / median, cfo_hz: cfo });
        }
    }
    out
}

/// Apply the decision rule to a set of scores.
pub fn decide(scores: &[HypothesisScore], bandwidth_mhz: u32) -> Option<DvbDetection> {
    let best = scores.iter().copied().max_by(|a, b| a.rho.partial_cmp(&b.rho).unwrap_or(std::cmp::Ordering::Equal))?;
    if best.rho < RHO_MIN || best.contrast < CONTRAST_MIN {
        return None;
    }
    let second = scores
        .iter()
        .filter(|s| s.mode != best.mode || s.gi != best.gi)
        .map(|s| s.rho)
        .fold(0.0f64, f64::max);
    if second > 0.0 && best.rho / second < MARGIN_MIN {
        return None;
    }
    let (signal_type, t2_possible) = classify(best.mode, best.gi);
    Some(DvbDetection {
        fft_mode: best.mode.label(),
        guard_interval: best.gi.label(),
        rho: best.rho,
        contrast: best.contrast,
        cfo_hz: best.cfo_hz,
        bandwidth_mhz,
        t2_possible,
        signal_type,
        mode: best.mode,
        gi: best.gi,
    })
}

/// Detect DVB-T / DVB-T2 in a raw RTL-SDR capture (interleaved u8 I/Q).
///
/// `bandwidth_mhz` is the channel width on the region's raster (6, 7 or 8).
/// Returns `None` when no cyclic-prefix structure is found.
pub fn detect_dvb(iq_bytes: &[u8], sample_rate: u32, bandwidth_mhz: u32) -> Option<DvbDetection> {
    let x = iq_u8_to_complex(iq_bytes);
    let scores = analyze(&x, f64::from(sample_rate), bandwidth_mhz);
    decide(&scores, bandwidth_mhz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_tables_match_en_302_755_table_67() {
        assert_eq!(allowed_guards(FftMode::K32).len(), 6);
        assert!(!allowed_guards(FftMode::K32).contains(&GuardInterval::G1_4));
        assert_eq!(allowed_guards(FftMode::K16).len(), 7);
        assert_eq!(allowed_guards(FftMode::K8).len(), 7);
        assert_eq!(allowed_guards(FftMode::K1), &[G::G1_16, G::G1_8, G::G1_4]);
        assert!(!allowed_guards(FftMode::K2).contains(&GuardInterval::G1_128));
        assert!(!allowed_guards(FftMode::K4).contains(&GuardInterval::G19_128));
        let total: usize = FftMode::ALL.iter().map(|m| allowed_guards(*m).len()).sum();
        assert_eq!(total, 31);
    }

    #[test]
    fn elementary_periods_match_standards() {
        assert!((elementary_period_us(8).unwrap() - 0.109375).abs() < 1e-12);
        assert!((elementary_period_us(7).unwrap() - 0.125).abs() < 1e-12);
        assert!((elementary_period_us(6).unwrap() - 7.0 / 48.0).abs() < 1e-12);
        assert!(elementary_period_us(5).is_none());
        // EN 300 744 Table 4: Tu = 896 us (8K) and 224 us (2K) at 8 MHz.
        assert!((FftMode::K8.n() as f64 * elementary_period_us(8).unwrap() - 896.0).abs() < 1e-9);
        assert!((FftMode::K2.n() as f64 * elementary_period_us(8).unwrap() - 224.0).abs() < 1e-9);
    }

    #[test]
    fn labeling_follows_contract_c1() {
        assert_eq!(classify(FftMode::K8, G::G1_8), ("dtv_dvbt", true));
        assert_eq!(classify(FftMode::K2, G::G1_4), ("dtv_dvbt", true));
        assert_eq!(classify(FftMode::K8, G::G19_128), ("dtv_dvbt2", true));
        assert_eq!(classify(FftMode::K8, G::G1_128), ("dtv_dvbt2", true));
        assert_eq!(classify(FftMode::K32, G::G1_8), ("dtv_dvbt2", true));
        assert_eq!(classify(FftMode::K1, G::G1_16), ("dtv_dvbt2", true));
        assert_eq!(classify(FftMode::K4, G::G1_32), ("dtv_dvbt2", true));
        assert_eq!(classify(FftMode::K16, G::G1_16), ("dtv_dvbt2", true));
    }

    #[test]
    fn short_or_unknown_bandwidth_input_yields_none() {
        assert!(detect_dvb(&[], 2_048_000, 8).is_none());
        assert!(detect_dvb(&[127u8; 4096], 2_048_000, 8).is_none());
        assert!(detect_dvb(&vec![127u8; 262_144], 2_048_000, 5).is_none());
    }
}

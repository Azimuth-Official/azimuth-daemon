//! TEST-ONLY synthetic DVB-T2 frame generator (never compiled into release builds).
//!
//! Built from the public standard ETSI EN 302 755 V1.4.1 (2015-07) only; no third-party code and no real
//! RF capture is used. What is modelled:
//!
//! * the **P1 symbol** exactly as specified in clause 9.8 (carrier distribution Table 68, S1/S2 patterns
//!   Table 69, DBPSK + PRBS scrambling clause 9.8.2.2, C-A-B waveform clause 9.8.2.4), signalling per
//!   clause 7.2.1 (S1 Table 18, S2 field 1 Table 19(a), S2 field 2 Table 20);
//! * the **frame layout** P1, N_P2 P2 symbols (clause 8.3.1 / Table 51), then data symbols, each with a
//!   cyclic-prefix guard interval (clause 9.7, Table 67) and the carrier counts of clause 9.5 Table 66
//!   (normal and extended carrier mode);
//! * the **elementary period** of clause 9.5 Table 65 (8 MHz: T = 7/64 us).
//!
//! What is NOT modelled: the contents of P2 and data cells are random QPSK on every carrier (no L1
//! signalling, no scattered/continual/edge pilots, no PLP coding, no PAPR reduction). The cyclic-prefix
//! structure and the P1 are what a detector can observe at 2.048 MS/s.
//!
//! Frame length and P1 placement: a real T2 frame lasts up to 250 ms (clause 8.1) while a detector capture
//! (`CAPTURE_LEN` samples at 2.048 MS/s) is 64 ms, so on air a capture holds a P1 only about one time in
//! four. The generator therefore places exactly one P1 inside the capture window, at `T2Params::p1_at_frac`
//! of the window (default 5 percent: the long continuous symbol grid after it keeps the cyclic-prefix fold
//! coherent), or none when `with_p1` is false. Inserting the 2048T P1 shifts the symbol grid, so a P1 in
//! mid-window (`p1_at_frac` 0.5) is the hard case for a CP-only fold. How a detector should treat captures
//! without any P1 is a policy decision and is NOT asserted here beyond "no P1, no `dtv_dvbt2`".

use super::dvb::{allowed_guards, elementary_period_us, FftMode, GuardInterval};
use super::testgen::{capture_from_record, FrontEnd, Rng, C32, CAPTURE_LEN, FS_OUT};
use rustfft::FftPlanner;

/// Same guard padding `testgen::dvb_capture` uses (private there).
const PAD: usize = 512;
/// Default position of the P1 inside the capture window (fraction of its length).
pub const DEFAULT_P1_AT_FRAC: f64 = 0.05;

/// P1 symbol length: 542T (C) + 1024T (A) + 482T (B), clause 9.8.2.4.2.
pub const P1_LEN: usize = 2048;
const P1_C_LEN: usize = 542;
const P1_A_LEN: usize = 1024;

/// Active carrier indices k_P1(0..383), EN 302 755 Table 68 (clause 9.8.2.1).
pub const P1_ACTIVE_CARRIERS: [usize; 384] = [
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

/// S1 patterns indexed by the 3-bit S1 value, EN 302 755 Table 69 (hexadecimal, MSB first).
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

/// S2 patterns indexed by the 4-bit S2 value (S2 field 1 followed by S2 field 2), Table 69.
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

/// One generated T2 configuration (8 MHz channel unless `bandwidth_mhz` says otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct T2Mode {
    pub fft: FftMode,
    pub gi: GuardInterval,
    /// Extended carrier mode (only defined for 8K, 16K, 32K), Table 66.
    pub extended: bool,
}

impl T2Mode {
    pub fn label(self) -> String {
        format!("{} {}{}", self.fft.label(), self.gi.label(), if self.extended { " ext" } else { "" })
    }
}

/// Every (FFT size, guard interval, carrier mode) combination EN 302 755 allows:
/// Table 67 (`allowed_guards`, 31 combinations) with the normal carrier mode, plus the extended
/// carrier mode for 8K, 16K and 32K (Table 66) = 31 + 7 + 7 + 6 = 51.
pub fn t2_all_modes() -> Vec<T2Mode> {
    let mut v = Vec::new();
    for fft in FftMode::ALL {
        for &gi in allowed_guards(fft) {
            v.push(T2Mode { fft, gi, extended: false });
            if matches!(fft, FftMode::K8 | FftMode::K16 | FftMode::K32) {
                v.push(T2Mode { fft, gi, extended: true });
            }
        }
    }
    v
}

/// Modes that exist in DVB-T as well as DVB-T2 (2K/8K with guard 1/32..1/4): a P1-less reading of
/// these cannot be told apart from DVB-T, which is exactly what the detector must now do with the P1.
pub fn t2_shared_modes() -> Vec<T2Mode> {
    t2_all_modes()
        .into_iter()
        .filter(|m| {
            matches!(m.fft, FftMode::K2 | FftMode::K8)
                && matches!(m.gi, GuardInterval::G1_32 | GuardInterval::G1_16 | GuardInterval::G1_8 | GuardInterval::G1_4)
        })
        .collect()
}

/// Number of carriers K_total, clause 9.5 Table 66.
pub fn k_total(fft: FftMode, extended: bool) -> usize {
    match (fft, extended) {
        (FftMode::K1, _) => 853,
        (FftMode::K2, _) => 1705,
        (FftMode::K4, _) => 3409,
        (FftMode::K8, false) => 6817,
        (FftMode::K8, true) => 6913,
        (FftMode::K16, false) => 13633,
        (FftMode::K16, true) => 13921,
        (FftMode::K32, false) => 27265,
        (FftMode::K32, true) => 27841,
    }
}

/// Number of P2 symbols N_P2, clause 8.3.1 Table 51.
pub fn n_p2(fft: FftMode) -> usize {
    match fft {
        FftMode::K1 => 16,
        FftMode::K2 => 8,
        FftMode::K4 => 4,
        FftMode::K8 => 2,
        FftMode::K16 | FftMode::K32 => 1,
    }
}

/// S2 field 1 for the T2-base profile, clause 7.2.1 Table 19(a).
pub fn s2_field1(fft: FftMode, gi: GuardInterval) -> u8 {
    let t2_only_gi = matches!(gi, GuardInterval::G1_128 | GuardInterval::G19_256 | GuardInterval::G19_128);
    match fft {
        FftMode::K2 => 0b000,
        FftMode::K8 if !t2_only_gi => 0b001,
        FftMode::K4 => 0b010,
        FftMode::K1 => 0b011,
        FftMode::K16 => 0b100,
        FftMode::K32 if !t2_only_gi => 0b101,
        FftMode::K8 => 0b110,
        FftMode::K32 => 0b111,
    }
}

fn hex_bits(hex: &str) -> Vec<u8> {
    hex.chars()
        .flat_map(|c| {
            let v = c.to_digit(16).expect("hex digit") as u8;
            (0..4).rev().map(move |b| (v >> b) & 1)
        })
        .collect()
}

/// PRBS of clause 5.2.4 (1 + X^14 + X^15) loaded with `init` (stage 1 first), as used for the P1
/// scrambler with initial state '100111001000110' (clause 9.8.2.2). Output bit = stage 14 xor stage 15,
/// fed back into stage 1.
pub fn p1_prbs(n: usize) -> Vec<u8> {
    let mut r: [u8; 15] = [1, 0, 0, 1, 1, 1, 0, 0, 1, 0, 0, 0, 1, 1, 0];
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let b = r[13] ^ r[14];
        out.push(b);
        for i in (1..15).rev() {
            r[i] = r[i - 1];
        }
        r[0] = b;
    }
    out
}

/// The 384 scrambled DBPSK values MSS_SCR (+1/-1) for signalling fields S1 (3 bits) and S2 (4 bits),
/// clause 9.8.2.2 steps 1-3: {CSS_S1, CSS_S2, CSS_S1}, DBPSK from a dummy +1, then times the PRBS.
pub fn p1_mss_scr(s1: u8, s2: u8) -> Vec<f32> {
    let mut seq = hex_bits(S1_HEX[usize::from(s1 & 7)]);
    seq.extend(hex_bits(S2_HEX[usize::from(s2 & 15)]));
    seq.extend(hex_bits(S1_HEX[usize::from(s1 & 7)]));
    assert_eq!(seq.len(), 384);
    let prbs = p1_prbs(384);
    let mut prev = 1.0f32;
    seq.iter()
        .zip(&prbs)
        .map(|(&b, &p)| {
            if b == 1 {
                prev = -prev;
            }
            prev * if p == 0 { 1.0 } else { -1.0 }
        })
        .collect()
}

fn ifft(buf: &mut [C32]) {
    FftPlanner::<f32>::new().plan_fft_inverse(buf.len()).process(buf);
}

/// The P1 symbol at the native sample rate 1/T: `P1_LEN` = 2048 samples in C-A-B order.
/// `A` is the 1K useful part with the 384 active carriers k_P1(i) at frequency (k - 426)/(1024 T)
/// (clause 9.8.2.4.1); C is the first 542 samples of A and B the last 482 samples of A, both shifted up
/// by f_SH = 1/(1024 T) (clause 9.8.2.4.2). The amplitude is scaled so the P1 sample power equals
/// `target_power` (clause 9.8.2.3 / 9.8.2.4.1 NOTE: "virtually the same power as the rest of the frame").
pub fn p1_symbol(s1: u8, s2: u8, target_power: f32) -> Vec<C32> {
    let mss = p1_mss_scr(s1, s2);
    let mut a = vec![C32::new(0.0, 0.0); P1_A_LEN];
    for (i, &k) in P1_ACTIVE_CARRIERS.iter().enumerate() {
        let bin = (k as isize - 426).rem_euclid(P1_A_LEN as isize) as usize;
        a[bin] = C32::new(mss[i], 0.0);
    }
    ifft(&mut a);
    let p = a.iter().map(|c| c.norm_sqr()).sum::<f32>() / P1_A_LEN as f32;
    let g = (target_power / p).sqrt();
    a.iter_mut().for_each(|c| *c *= g);
    let shift = |n: usize| {
        let ph = 2.0 * std::f64::consts::PI * n as f64 / P1_A_LEN as f64;
        C32::new(ph.cos() as f32, ph.sin() as f32)
    };
    let mut out = Vec::with_capacity(P1_LEN);
    out.extend((0..P1_C_LEN).map(|n| a[n] * shift(n)));
    out.extend_from_slice(&a);
    out.extend((P1_C_LEN..P1_A_LEN).map(|n| a[n] * shift(n)));
    debug_assert_eq!(out.len(), P1_LEN);
    out
}

/// Generator options.
#[derive(Debug, Clone, Copy)]
pub struct T2Params {
    pub mode: T2Mode,
    pub bandwidth_mhz: u32,
    /// Position of the (single) P1 inside the capture window, as a fraction of the window length.
    pub p1_at_frac: f64,
    /// Emit the P1 symbol (false = the same stream with the P1 slot replaced by white noise of equal power,
    /// for negative tests: a P2/data stream with cyclic prefixes but no preamble).
    pub with_p1: bool,
}

impl T2Params {
    pub fn new(mode: T2Mode) -> Self {
        T2Params { mode, bandwidth_mhz: 8, p1_at_frac: DEFAULT_P1_AT_FRAC, with_p1: true }
    }
}

fn ofdm_symbol(rng: &mut Rng, n: usize, g: usize, k: usize, out: &mut Vec<C32>) {
    let mut body = vec![C32::new(0.0, 0.0); n];
    let half = (k - 1) / 2;
    for kk in 0..k {
        let kp = kk as isize - half as isize;
        let bin = kp.rem_euclid(n as isize) as usize;
        let re = if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 };
        let im = if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 };
        body[bin] = C32::new(re, im);
    }
    ifft(&mut body);
    out.extend_from_slice(&body[n - g..]);
    out.extend_from_slice(&body);
}

/// T2 time-domain record at the native rate 1/T with at least `n_samples` samples: data symbols, one P1
/// (or its white-noise stand-in) at `p1_at_frac * n_samples`, then data symbols on a continuous grid.
/// The start is at a random position inside a symbol. Returns (record, native rate).
pub fn t2_record(rng: &mut Rng, p: T2Params, n_samples: usize) -> (Vec<C32>, f64) {
    let t_us = elementary_period_us(p.bandwidth_mhz).expect("bandwidth");
    let fs_n = 1.0 / (t_us * 1e-6);
    let n = p.mode.fft.n();
    let g = (n as f64 * p.mode.gi.ratio()).round() as usize;
    let ts = n + g;
    let k = k_total(p.mode.fft, p.mode.extended);
    // Unnormalised inverse FFT of random QPSK (+-1 +-j) gives sample power 2 K.
    let data_power = 2.0 * k as f32;
    let s2 = (s2_field1(p.mode.fft, p.mode.gi) << 1) & 0b1111; // S2 field 2 = 0: not mixed (Table 20)
    let p1 = p1_symbol(0b000, s2, data_power); // S1 = 000: T2_SISO (Table 18)
    let pre_syms = ((p.p1_at_frac * n_samples as f64) as usize / ts).max(1) + 1;
    let mut rec: Vec<C32> = Vec::with_capacity(n_samples + 4 * ts + P1_LEN);
    for _ in 0..pre_syms {
        ofdm_symbol(rng, n, g, k, &mut rec);
    }
    if p.with_p1 {
        rec.extend_from_slice(&p1);
    } else {
        for _ in 0..P1_LEN {
            let re = if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 };
            let im = if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 };
            rec.push(C32::new(re, im) * (data_power / 2.0).sqrt());
        }
    }
    while rec.len() < n_samples + ts + pre_syms * ts {
        ofdm_symbol(rng, n, g, k, &mut rec);
    }
    // Drop part of the leading symbols: random phase of the capture start inside a symbol, P1 stays in window.
    let off = (rng.next_u64() as usize) % ts;
    (rec[off..].to_vec(), fs_n)
}

/// One DVB-T2 trial capture through the RTL-SDR-like front end (2.048 MS/s, 8-bit).
pub fn t2_capture(seed: u64, p: T2Params, snr_db: f64, cfo_hz: f64, clip: bool) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let t_us = elementary_period_us(p.bandwidth_mhz).unwrap();
    let fs_n = 1.0 / (t_us * 1e-6);
    let need = (CAPTURE_LEN as f64 * fs_n / FS_OUT).ceil() as usize + 2 * PAD + 16;
    let (rec, fs_n) = t2_record(&mut rng, p, need);
    let mut fe = FrontEnd::standard(snr_db, cfo_hz);
    fe.clip = clip;
    capture_from_record(&mut rng, &rec, fs_n, fe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_table_has_51_entries_and_32k_has_no_quarter_guard() {
        let m = t2_all_modes();
        assert_eq!(m.len(), 51);
        assert!(!m.iter().any(|x| x.fft == FftMode::K32 && x.gi == GuardInterval::G1_4));
        assert!(!m.iter().any(|x| x.extended && matches!(x.fft, FftMode::K1 | FftMode::K2 | FftMode::K4)));
        // 2K: 4 guard intervals (normal only); 8K: 4 guard intervals x (normal, extended).
        assert_eq!(t2_shared_modes().len(), 4 + 4 * 2);
    }

    #[test]
    fn prbs_matches_the_standard_example() {
        // EN 302 755 clause 9.8.2.2 EXAMPLE: SCR_SEQ starts {-1, +1, -1, +1}, i.e. PRBS 1,0,1,0.
        assert_eq!(&p1_prbs(4), &[1, 0, 1, 0]);
    }

    #[test]
    fn mss_matches_the_standard_example() {
        // Clause 9.8.2.2 EXAMPLE (S1=000, S2=0000): MSS_SCR = {-1, +1, -1, -1, ...} (first CSS_S1 values).
        let m = p1_mss_scr(0, 0);
        assert_eq!(&m[..4], &[-1.0, 1.0, -1.0, -1.0]);
        assert_eq!(m.len(), 384);
    }

    #[test]
    fn p1_has_c_a_b_structure_with_fsh_shift() {
        let p1 = p1_symbol(0, 0b0010, 1000.0);
        assert_eq!(p1.len(), P1_LEN);
        // C = first 542 samples of A shifted by exp(j 2 pi n / 1024); B likewise for samples 542..1023.
        for n in [0usize, 100, 541] {
            let sh = 2.0 * std::f64::consts::PI * n as f64 / 1024.0;
            let want = p1[P1_C_LEN + n] * C32::new(sh.cos() as f32, sh.sin() as f32);
            assert!((p1[n] - want).norm() < 1e-2, "C[{n}]");
        }
        for n in [542usize, 700, 1023] {
            let sh = 2.0 * std::f64::consts::PI * n as f64 / 1024.0;
            let want = p1[P1_C_LEN + n] * C32::new(sh.cos() as f32, sh.sin() as f32);
            assert!((p1[P1_C_LEN + P1_A_LEN + (n - 542)] - want).norm() < 1e-2, "B[{n}]");
        }
        let power = p1.iter().map(|c| c.norm_sqr()).sum::<f32>() / P1_LEN as f32;
        assert!((power / 1000.0 - 1.0).abs() < 0.2, "power {power}");
    }

    #[test]
    fn t2_record_is_long_enough_at_the_native_rate() {
        let mut rng = Rng::new(3);
        let mode = T2Mode { fft: FftMode::K8, gi: GuardInterval::G1_8, extended: false };
        let (rec, fs) = t2_record(&mut rng, T2Params::new(mode), 300_000);
        assert!((fs - 9.142857e6).abs() < 1e3);
        assert!(rec.len() >= 300_000);
    }
}

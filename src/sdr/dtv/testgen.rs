//! TEST-ONLY synthetic signal generators (never compiled into release builds).
//!
//! Everything here is synthetic: OFDM DVB-T / DVB-T2-like signals, AWGN, CW,
//! multi-station FM and ATSC 8VSB-like signals, passed through an RTL-SDR-like
//! front end (2.048 MS/s, band-limited, IQ imbalance, DC spike, CFO, clipping,
//! 8-bit quantisation). No real RF capture is used or stored.

use super::dvb::{elementary_period_us, FftMode, GuardInterval};
use rustfft::{num_complex::Complex, FftPlanner};
use std::cell::RefCell;

pub const FS_OUT: f64 = 2_048_000.0;
pub const CAPTURE_LEN: usize = 131_072;
const PAD: usize = 512;

pub type C32 = Complex<f32>;

/// xorshift64* PRNG with Box-Muller Gaussian.
pub struct Rng {
    s: u64,
    spare: Option<f64>,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut r = Rng { s: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03, spare: None };
        for _ in 0..4 {
            r.next_u64();
        }
        r
    }
    pub fn next_u64(&mut self) -> u64 {
        self.s ^= self.s >> 12;
        self.s ^= self.s << 25;
        self.s ^= self.s >> 27;
        self.s.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.uniform()
    }
    pub fn gauss(&mut self) -> f64 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        let r = (-2.0 * u1.ln()).sqrt();
        self.spare = Some(r * (2.0 * std::f64::consts::PI * u2).sin());
        r * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

thread_local! {
    static PLANNER: RefCell<FftPlanner<f32>> = RefCell::new(FftPlanner::new());
}

fn fft(buf: &mut [C32], inverse: bool) {
    PLANNER.with(|p| {
        let mut p = p.borrow_mut();
        let plan = if inverse { p.plan_fft_inverse(buf.len()) } else { p.plan_fft_forward(buf.len()) };
        plan.process(buf);
    });
}

/// Front-end impairment switches.
#[derive(Debug, Clone, Copy)]
pub struct FrontEnd {
    /// In-band SNR (signal power / white noise power over the 2.048 MHz capture).
    pub snr_db: f64,
    /// Frequency shift applied to the record (signal centre minus tuner centre).
    pub shift_hz: f64,
    pub iq_imbalance: bool,
    pub dc_spike: bool,
    /// Drive the 8-bit converter close to saturation (a few percent clipped).
    pub clip: bool,
}

impl FrontEnd {
    pub fn standard(snr_db: f64, shift_hz: f64) -> Self {
        FrontEnd { snr_db, shift_hz, iq_imbalance: true, dc_spike: true, clip: false }
    }
}

/// OFDM time-domain record at the native rate 1/T for `n_samples` samples
/// (QPSK on all carriers of Table 66 "normal carrier mode", cyclic prefix = last
/// `gi * N` samples of every symbol). Returns (record, native rate).
pub fn ofdm_record(rng: &mut Rng, mode: FftMode, gi: GuardInterval, bandwidth_mhz: u32, n_samples: usize) -> (Vec<C32>, f64) {
    let t_us = elementary_period_us(bandwidth_mhz).expect("bandwidth");
    let fs_n = 1.0 / (t_us * 1e-6);
    let n = mode.n();
    let g = (n as f64 * gi.ratio()).round() as usize;
    let k_total = match mode {
        FftMode::K1 => 853,
        FftMode::K2 => 1705,
        FftMode::K4 => 3409,
        FftMode::K8 => 6817,
        FftMode::K16 => 13633,
        FftMode::K32 => 27265,
    };
    let ts = n + g;
    let n_sym = n_samples / ts + 3;
    let mut rec = Vec::with_capacity(n_sym * ts);
    let mut body = vec![C32::new(0.0, 0.0); n];
    let half = (k_total - 1) / 2;
    for _ in 0..n_sym {
        body.iter_mut().for_each(|v| *v = C32::new(0.0, 0.0));
        for k in 0..k_total {
            let kp = k as isize - half as isize;
            let bin = if kp >= 0 { kp as usize } else { (n as isize + kp) as usize };
            let re = if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 };
            let im = if rng.next_u64() & 1 == 0 { 1.0 } else { -1.0 };
            body[bin] = C32::new(re, im);
        }
        fft(&mut body, true);
        rec.extend_from_slice(&body[n - g..]);
        rec.extend_from_slice(&body);
    }
    // Random start inside the symbol stream.
    let off = (rng.next_u64() as usize) % ts;
    (rec[off..].to_vec(), fs_n)
}

/// Complex white Gaussian noise record.
pub fn noise_record(rng: &mut Rng, n: usize) -> Vec<C32> {
    (0..n).map(|_| C32::new(rng.gauss() as f32, rng.gauss() as f32)).collect()
}

/// A few CW tones inside the capture window (offset from the tuner centre, Hz).
pub fn cw_record(rng: &mut Rng, fs_n: f64, n: usize, tones: &[(f64, f64)]) -> Vec<C32> {
    let phases: Vec<f64> = tones.iter().map(|_| rng.range(0.0, std::f64::consts::TAU)).collect();
    (0..n)
        .map(|i| {
            let t = i as f64 / fs_n;
            let mut acc = C32::new(0.0, 0.0);
            for (j, (f, a)) in tones.iter().enumerate() {
                let ph = 2.0 * std::f64::consts::PI * f * t + phases[j];
                acc += C32::new((a * ph.cos()) as f32, (a * ph.sin()) as f32);
            }
            acc
        })
        .collect()
}

/// Broadcast-FM-like scene: each station `(offset_hz, amplitude)` is frequency
/// modulated (75 kHz peak deviation) by a random multi-tone "programme" plus a
/// 19 kHz pilot. Offsets are relative to the tuner centre.
pub fn fm_record(rng: &mut Rng, fs_n: f64, n: usize, stations: &[(f64, f64)]) -> Vec<C32> {
    let mut rec = vec![C32::new(0.0, 0.0); n];
    for &(off, amp) in stations {
        let tones: Vec<(f64, f64, f64)> = (0..4).map(|_| (rng.range(200.0, 6000.0), rng.range(0.05, 0.25), rng.range(0.0, std::f64::consts::TAU))).collect();
        let slow = rng.range(0.5, 3.0);
        let mut phase = rng.range(0.0, std::f64::consts::TAU);
        for (i, v) in rec.iter_mut().enumerate() {
            let t = i as f64 / fs_n;
            let mut m = 0.1 * (2.0 * std::f64::consts::PI * 19_000.0 * t).sin();
            for (f, a, p) in &tones {
                m += a * (2.0 * std::f64::consts::PI * f * t + p).sin();
            }
            m *= 0.6 + 0.4 * (2.0 * std::f64::consts::PI * slow * t).sin();
            phase += 2.0 * std::f64::consts::PI * (off + 75_000.0 * m.clamp(-1.0, 1.0)) / fs_n;
            *v += C32::new((amp * phase.cos()) as f32, (amp * phase.sin()) as f32);
        }
    }
    rec
}

/// ATSC 8VSB-like record at the 8VSB symbol rate: random 8-level symbols,
/// single-sideband spectrum 0..5.38 MHz above the pilot, pilot at 0 Hz.
/// Returns (record, native rate); the pilot sits at 0 Hz of the record.
pub fn atsc_record(rng: &mut Rng, n: usize) -> (Vec<C32>, f64) {
    let rs = 10_762_238.0;
    let len = n.next_power_of_two();
    let mut v: Vec<C32> = (0..len).map(|_| C32::new((2 * (rng.next_u64() % 8) as i32 - 7) as f32, 0.0)).collect();
    fft(&mut v, false);
    for (k, x) in v.iter_mut().enumerate() {
        let f = if k < len / 2 { k as f64 } else { k as f64 - len as f64 } * rs / len as f64;
        let edge = 5.38e6;
        let w = if f < 0.0 || f > edge {
            0.0
        } else if f > edge - 0.2e6 {
            0.5 * (1.0 + (std::f64::consts::PI * (f - (edge - 0.2e6)) / 0.2e6).cos())
        } else {
            1.0
        };
        *x *= w as f32;
    }
    fft(&mut v, true);
    let sc = 1.0 / len as f32;
    let rms = (v.iter().map(|c| c.norm_sqr()).sum::<f32>() / len as f32).sqrt() * sc;
    for c in &mut v {
        *c = *c * sc + C32::new(1.25 * rms * 1.2, 0.0); // pilot ~ -7 dB below the data
    }
    v.truncate(n);
    (v, rs)
}

fn hann_sinc(d: f64) -> f64 {
    const HW: f64 = 8.0;
    if d.abs() >= HW {
        return 0.0;
    }
    let s = if d.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d) };
    s * 0.5 * (1.0 + (std::f64::consts::PI * d / HW).cos())
}

/// Run a native-rate record through the RTL-SDR-like front end and return the
/// interleaved u8 capture (CAPTURE_LEN samples at 2.048 MS/s).
pub fn capture_from_record(rng: &mut Rng, rec: &[C32], fs_n: f64, fe: FrontEnd) -> Vec<u8> {
    let need = (CAPTURE_LEN as f64 * fs_n / FS_OUT).ceil() as usize + 2 * PAD + 16;
    assert!(rec.len() >= need, "record too short: {} < {}", rec.len(), need);
    let pn = need.next_power_of_two();
    let mut buf = vec![C32::new(0.0, 0.0); pn];
    for (i, b) in buf.iter_mut().enumerate().take(need) {
        let ph = 2.0 * std::f64::consts::PI * fe.shift_hz * (i as f64 / fs_n);
        *b = rec[i] * C32::new(ph.cos() as f32, ph.sin() as f32);
    }
    fft(&mut buf, false);
    for (k, b) in buf.iter_mut().enumerate() {
        let f = (if k < pn / 2 { k as f64 } else { k as f64 - pn as f64 }) * fs_n / pn as f64;
        let a = f.abs();
        let w = if a <= 0.90e6 {
            1.0
        } else if a >= 1.02e6 {
            0.0
        } else {
            0.5 * (1.0 + (std::f64::consts::PI * (a - 0.90e6) / 0.12e6).cos())
        };
        *b *= w as f32;
    }
    fft(&mut buf, true);
    let sc = 1.0 / pn as f32;
    for b in &mut buf {
        *b *= sc;
    }
    // Resample to 2.048 MS/s with a windowed-sinc interpolator.
    let step = fs_n / FS_OUT;
    let mut y: Vec<C32> = Vec::with_capacity(CAPTURE_LEN);
    for i in 0..CAPTURE_LEN {
        let pos = PAD as f64 + i as f64 * step;
        let c = pos.floor() as isize;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for j in (c - 7)..=(c + 8) {
            let h = hann_sinc(pos - j as f64);
            let s = buf[j as usize];
            re += h * f64::from(s.re);
            im += h * f64::from(s.im);
        }
        y.push(C32::new(re as f32, im as f32));
    }
    let ps = y.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>() / CAPTURE_LEN as f64;
    let ps = if ps > 0.0 { ps } else { 1.0 };
    let sigma = (ps / 10f64.powf(fe.snr_db / 10.0) / 2.0).sqrt();
    for v in &mut y {
        v.re += (rng.gauss() * sigma) as f32;
        v.im += (rng.gauss() * sigma) as f32;
    }
    let rms = (ps + 2.0 * sigma * sigma).sqrt();
    if fe.dc_spike {
        let dc = C32::new((0.25 * rms) as f32, (-0.15 * rms) as f32);
        for v in &mut y {
            *v += dc;
        }
    }
    if fe.iq_imbalance {
        let (g, phi) = (1.04f32, 3.0f32.to_radians());
        for v in &mut y {
            let q = g * (v.im * phi.cos() + v.re * phi.sin());
            v.im = q;
        }
    }
    let comp_sigma = if fe.clip { 60.0 } else { 30.0 };
    let scale = comp_sigma / (rms / std::f64::consts::SQRT_2);
    let mut out = Vec::with_capacity(2 * CAPTURE_LEN);
    for v in &y {
        out.push((127.5 + f64::from(v.re) * scale).round().clamp(0.0, 255.0) as u8);
        out.push((127.5 + f64::from(v.im) * scale).round().clamp(0.0, 255.0) as u8);
    }
    out
}

/// One DVB trial capture. `cfo_hz` is the signal-minus-tuner offset.
pub fn dvb_capture(seed: u64, mode: FftMode, gi: GuardInterval, bandwidth_mhz: u32, snr_db: f64, cfo_hz: f64, clip: bool) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let t_us = elementary_period_us(bandwidth_mhz).unwrap();
    let fs_n = 1.0 / (t_us * 1e-6);
    let need = (CAPTURE_LEN as f64 * fs_n / FS_OUT).ceil() as usize + 2 * PAD + 16;
    let (rec, fs_n) = ofdm_record(&mut rng, mode, gi, bandwidth_mhz, need);
    let mut fe = FrontEnd::standard(snr_db, cfo_hz);
    fe.clip = clip;
    capture_from_record(&mut rng, &rec, fs_n, fe)
}

/// Draw a CFO: 20 percent of trials use the +-166.667 kHz carrier-offset case,
/// the rest are uniform in +-250 kHz.
pub fn random_cfo(rng: &mut Rng) -> f64 {
    if rng.uniform() < 0.2 {
        if rng.next_u64() & 1 == 0 { 166_666.667 } else { -166_666.667 }
    } else {
        rng.range(-250_000.0, 250_000.0)
    }
}

/// Negative-control classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negative {
    Awgn,
    Cw,
    Fm,
    Atsc,
}

pub fn negative_capture(seed: u64, kind: Negative) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let fs_n = 4.096e6 * 2.0;
    let need = (CAPTURE_LEN as f64 * fs_n / FS_OUT).ceil() as usize + 2 * PAD + 16;
    let snr = rng.range(-5.0, 40.0);
    let clip = rng.uniform() < 0.25;
    match kind {
        Negative::Awgn => {
            let rec = noise_record(&mut rng, need);
            let mut fe = FrontEnd::standard(snr, 0.0);
            fe.clip = clip;
            capture_from_record(&mut rng, &rec, fs_n, fe)
        }
        Negative::Cw => {
            let n_tones = 1 + (rng.next_u64() % 3) as usize;
            let tones: Vec<(f64, f64)> = (0..n_tones).map(|_| (rng.range(-0.85e6, 0.85e6), rng.range(0.2, 1.0))).collect();
            let rec = cw_record(&mut rng, fs_n, need, &tones);
            let mut fe = FrontEnd::standard(rng.range(0.0, 50.0), 0.0);
            fe.clip = clip;
            capture_from_record(&mut rng, &rec, fs_n, fe)
        }
        Negative::Fm => {
            let n_st = 3 + (rng.next_u64() % 8) as usize;
            let mut stations = Vec::new();
            for k in 0..n_st {
                let off = ((rng.next_u64() % 21) as f64 - 10.0) * 200_000.0 * 0.45 + 0.0 * k as f64;
                stations.push((off.clamp(-1.8e6, 1.8e6), 10f64.powf(rng.range(-25.0, 0.0) / 20.0)));
            }
            let rec = fm_record(&mut rng, fs_n, need, &stations);
            let mut fe = FrontEnd::standard(rng.range(5.0, 45.0), 0.0);
            fe.clip = clip;
            capture_from_record(&mut rng, &rec, fs_n, fe)
        }
        Negative::Atsc => {
            let need_a = (CAPTURE_LEN as f64 * 10_762_238.0 / FS_OUT).ceil() as usize + 2 * PAD + 16;
            let (rec, fs_a) = atsc_record(&mut rng, need_a);
            // Tuner centre anywhere inside the 6 MHz channel (pilot at 0 Hz, channel centre at +2.69 MHz).
            let u = 2.69e6 + rng.range(-2.0e6, 2.0e6);
            let mut fe = FrontEnd::standard(rng.range(5.0, 40.0), -u);
            fe.clip = clip;
            capture_from_record(&mut rng, &rec, fs_a, fe)
        }
    }
}

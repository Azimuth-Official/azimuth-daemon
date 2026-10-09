//! Channel-relative FM band survey.
//!
//! One 2.048 MS/s capture contains every 200 kHz channel within about +-0.7 MHz
//! of the tuned centre, so a band sweep needs ~1 tune per 8 channels instead of
//! one per channel, and each channel is judged against THAT capture's own noise
//! floor rather than against the whole-capture power (which tuner AGC makes
//! meaningless):
//!
//! 1. one FFT of the capture; channel evidence = spectrum power within +-50 kHz
//!    of the channel centre (rejects the skirt of a strong neighbour); noise floor = low percentile of 5 kHz spectrum groups
//!    (candidate gate: `excess_db`);
//! 2. the channel is extracted in the frequency domain (brick-wall with raised-
//!    cosine edges, no adjacent-channel leakage into the demodulator) and
//!    FM-demodulated; an FM station puts its energy at audio frequencies
//!    (0.1-15 kHz), noise puts it at high discriminator frequencies, so
//!    `quality_db` (audio density over 72-90 kHz density, normalised so noise
//!    reads ~0 dB) separates the two (confirmation gate);
//! 3. confirmed channels get a 19 kHz stereo-pilot measurement and an RDS PI
//!    attempt (extra evidence, never required).
//!
//! Pure functions over raw u8 IQ: no device access, so it is fully testable.

use crate::sdr::rds::extract_rds;
use rustfft::{num_complex::Complex, FftPlanner};

type C32 = Complex<f32>;

/// FM channel width the survey evaluates.
pub const CHANNEL_HZ: f64 = 200_000.0;

#[derive(Debug, Clone)]
pub struct SurveyConfig {
    /// Candidate gate: power within +-50 kHz of the channel centre above the capture noise floor (dB).
    pub min_excess_db: f64,
    /// Confirmation gate: discriminator quality (dB, noise reads about 0).
    pub min_quality_db: f64,
    /// Channel raster spacing (Hz): 200 kHz in the Americas/Korea, 100 kHz elsewhere.
    pub channel_step_hz: f64,
}

impl Default for SurveyConfig {
    fn default() -> Self {
        SurveyConfig { min_excess_db: 6.0, min_quality_db: 10.0, channel_step_hz: 200_000.0 }
    }
}

#[derive(Debug, Clone)]
pub struct FmChannel {
    pub freq_hz: u64,
    /// Channel power relative to full scale (dBFS, per-sample power of the channel band).
    pub power_dbfs: f64,
    pub excess_db: f64,
    pub quality_db: f64,
    /// 19 kHz stereo pilot above its spectral neighbours (dB); None if not measured.
    pub pilot_db: Option<f64>,
    pub rds_pi: Option<u16>,
    /// Rejected as the mirror image of a much stronger channel at -f.
    pub image_of_mirror: bool,
    /// Rejected as the spill of a much stronger neighbouring channel (same audio, same sign).
    pub spill_of_neighbour: bool,
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CapturePlan {
    pub centre_hz: u64,
    pub channels: Vec<u64>,
}

/// Group the channel raster `start..=end` (step `step`) into captures. Each
/// channel appears in exactly one plan; the tuned centre sits between channels so the
/// RTL-SDR DC spike never lands on a channel.
pub fn plan_captures(start: u64, end: u64, step: u64, sample_rate: u32) -> Vec<CapturePlan> {
    if step == 0 || end < start {
        return Vec::new();
    }
    let channels: Vec<u64> = (0..).map(|k| start + k * step).take_while(|f| *f <= end).collect();
    // Usable half-width: 0.8 MHz at 2.048 MS/s (channels end up within +-0.7 MHz), scaled with the sample rate.
    let usable = 0.8e6 * f64::from(sample_rate) / 2_048_000.0;
    let mut n = ((2.0 * usable) / step as f64).floor() as usize;
    n = (n / 2 * 2).max(2);
    channels
        .chunks(n)
        .map(|c| {
            let centre = if c.len() % 2 == 0 {
                (c[0] + c[c.len() - 1]) / 2
            } else {
                c[c.len() / 2] + step / 2
            };
            CapturePlan { centre_hz: centre, channels: c.to_vec() }
        })
        .collect()
}

fn abs_corr(a: &[f32], b: &[f32]) -> f64 {
    let (mut sxy, mut sxx, mut syy) = (0.0f64, 0.0f64, 0.0f64);
    for (u, v) in a.iter().zip(b) {
        sxy += f64::from(*u) * f64::from(*v);
        sxx += f64::from(*u) * f64::from(*u);
        syy += f64::from(*v) * f64::from(*v);
    }
    sxy.abs() / (sxx * syy).sqrt().max(1e-30)
}

fn percentile(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// Evaluate `channels` (absolute Hz) inside one capture centred at `centre_hz`.
pub fn evaluate_capture(iq_bytes: &[u8], sample_rate: u32, centre_hz: u64, channels: &[u64], cfg: &SurveyConfig) -> Vec<FmChannel> {
    let m = iq_bytes.len() / 2;
    if m < 16_384 {
        return Vec::new();
    }
    let mut x: Vec<C32> = (0..m)
        .map(|i| C32::new((f32::from(iq_bytes[2 * i]) - 127.5) / 127.5, (f32::from(iq_bytes[2 * i + 1]) - 127.5) / 127.5))
        .collect();
    let mean = x.iter().fold(C32::new(0.0, 0.0), |a, b| a + *b) / m as f32;
    for v in &mut x {
        *v -= mean;
    }
    let mut planner = FftPlanner::<f32>::new();
    planner.plan_fft_forward(m).process(&mut x);
    let spec = x;
    let bin_hz = f64::from(sample_rate) / m as f64;
    let mf = m as f64;
    let power = |k: usize| f64::from(spec[k % m].norm_sqr()) / (mf * mf);

    // Noise floor: low percentile of 5 kHz groups across +-0.9 MHz (DC excluded).
    let g = ((5_000.0 / bin_hz).round() as usize).max(1);
    let max_k = (0.9e6 / bin_hz) as isize;
    let dc_k = (12_000.0 / bin_hz) as isize;
    let mut groups = Vec::new();
    let mut k = -max_k;
    while k + g as isize <= max_k {
        if k.abs() > dc_k && (k + g as isize).abs() > dc_k {
            let s: f64 = (0..g).map(|j| power((k + j as isize).rem_euclid(m as isize) as usize)).sum();
            groups.push(s / g as f64);
        }
        k += g as isize;
    }
    if groups.is_empty() {
        return Vec::new();
    }
    let floor_bin = percentile(&mut groups, 0.2).max(1e-18);

    let b = (CHANNEL_HZ / bin_hz).round() as usize; // channel bins
    let inner = ((50_000.0 / bin_hz).round() as usize).max(1);
    let fs_b = b as f64 * bin_hz; // extracted complex rate
    let ifft = planner.plan_fft_inverse(b);
    let nd = b - 1;
    let fft_d = planner.plan_fft_forward(nd);
    let hann: Vec<f32> = (0..nd).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (nd - 1) as f64).cos()) as f32).collect();
    let dbin = fs_b / nd as f64;
    let band = |lo: f64, hi: f64| -> std::ops::Range<usize> { (lo / dbin).ceil() as usize..((hi / dbin).floor() as usize + 1) };

    // Frequency-domain channel extraction (brick wall, raised-cosine edges 90-100 kHz),
    // then quadrature discriminator; returns the Hann-windowed, mean-removed output.
    let discriminator = |k0: isize| -> Vec<f32> {
        let mut y = vec![C32::new(0.0, 0.0); b];
        for (j, yv) in y.iter_mut().enumerate() {
            let jj = if j < b / 2 { j as isize } else { j as isize - b as isize };
            let a = (jj as f64 * bin_hz).abs();
            let w = if a <= 90_000.0 { 1.0 } else if a >= 100_000.0 { 0.0 } else { 0.5 * (1.0 + (std::f64::consts::PI * (a - 90_000.0) / 10_000.0).cos()) };
            if w > 0.0 {
                *yv = spec[(k0 + jj).rem_euclid(m as isize) as usize] * w as f32;
            }
        }
        ifft.process(&mut y);
        let d: Vec<f32> = (0..nd)
            .map(|n| {
                let p = y[n + 1] * y[n].conj();
                p.im.atan2(p.re)
            })
            .collect();
        let mean_d = d.iter().sum::<f32>() / nd as f32;
        d.iter().enumerate().map(|(i, v)| (v - mean_d) * hann[i]).collect()
    };
    let inner_of = |k0: isize| -> f64 { (0..2 * inner).map(|j| power((k0 + j as isize - inner as isize).rem_euclid(m as isize) as usize)).sum() };

    let mut out = Vec::new();
    for &f in channels {
        let off = f as f64 - centre_hz as f64;
        let k0 = (off / bin_hz).round() as isize;
        let ch_power: f64 = (0..b).map(|j| power((k0 + j as isize - (b / 2) as isize).rem_euclid(m as isize) as usize)).sum();
        // Candidate evidence is the power within +-50 kHz of the channel centre:
        // a station concentrates there, whereas the skirt of a strong neighbour
        // leaks into the OUTER part of the channel (its energy sits at +-100 kHz).
        let inner_power = inner_of(k0);
        let excess_db = 10.0 * (inner_power / (floor_bin * (2 * inner) as f64)).log10();
        let mut ch = FmChannel {
            freq_hz: f,
            power_dbfs: 10.0 * ch_power.max(1e-18).log10(),
            excess_db,
            quality_db: f64::NAN,
            pilot_db: None,
            rds_pi: None,
            image_of_mirror: false,
            spill_of_neighbour: false,
            confirmed: false,
        };
        if off.abs() < 0.95e6 && excess_db >= cfg.min_excess_db {
            let d_raw = discriminator(k0);
            let mut d: Vec<C32> = d_raw.iter().map(|v| C32::new(*v, 0.0)).collect();
            fft_d.process(&mut d);
            let p: Vec<f64> = d.iter().map(|c| f64::from(c.norm_sqr())).collect();
            let mean_of = |r: std::ops::Range<usize>| -> f64 { let n = r.len().max(1); r.map(|i| p[i]).sum::<f64>() / n as f64 };
            let audio = mean_of(band(100.0, 15_000.0));
            let hi = mean_of(band(72_000.0, 90_000.0)).max(1e-30);
            ch.quality_db = 10.0 * (audio / hi).log10() + QUALITY_NOISE_OFFSET_DB;
            // RTL front ends leak a 25-40 dB weaker mirror image of every strong
            // signal to -f. The image is the SAME audio with inverted sign, so a
            // candidate that is much weaker than the channel at -f AND strongly
            // correlated with it is an image, not a station.
            let mirror_k0 = -k0;
            let mirror_power = inner_of(mirror_k0);
            let image = if mirror_power > inner_power * 10.0 && (off.abs() > 25_000.0) {
                abs_corr(&d_raw, &discriminator(mirror_k0)) >= 0.5
            } else {
                false
            };
            ch.image_of_mirror = image;
            // Adjacent-channel spill: a much stronger channel within 400 kHz
            // leaks into this channel's window with ITS audio (same sign). Independent
            // programmes do not correlate, so a high correlation means spill, not a
            // station. Matters most on a 100 kHz raster, where the neighbour's
            // spectrum overlaps the +-100 kHz extraction window.
            let step_k = (cfg.channel_step_hz / bin_hz).round() as isize;
            // 100 kHz raster: the neighbour's spectrum overlaps this window, so be
            // sensitive and look two steps out. 200 kHz raster: only a 10x stronger
            // adjacent channel with strongly correlated audio counts as spill.
            let dense = cfg.channel_step_hz < 150_000.0;
            let (min_ratio, min_corr, max_m) = if dense { (2.0, 0.3, 2) } else { (10.0, 0.5, 1) };
            let reach = max_m * step_k;
            let mut spill = false;
            if ch.quality_db >= cfg.min_quality_db && !image && step_k > 0 {
                let mut m = 1;
                'outer: while m * step_k <= reach {
                    for nk in [k0 - m * step_k, k0 + m * step_k] {
                        if inner_of(nk) > inner_power * min_ratio
                            && (nk as f64 * bin_hz).abs() < 0.95e6
                            && abs_corr(&d_raw, &discriminator(nk)) >= min_corr
                        {
                            spill = true;
                            break 'outer;
                        }
                    }
                    m += 1;
                }
            }
            // One-sided energy: a station is centred in its window, so its power is
            // balanced either side of the centre; energy piled up against ONE window
            // edge belongs to a neighbour centred outside (the 100 kHz raster case).
            if ch.quality_db >= cfg.min_quality_db && !image && !spill {
                let gap = ((5_000.0 / bin_hz).round() as isize).max(1);
                let half = (b / 2) as isize;
                let side = |from: isize, to: isize| -> f64 { (from..to).map(|j| power((k0 + j).rem_euclid(m as isize) as usize)).sum() };
                let (lo, hi) = (side(-half, -gap), side(gap, half));
                if (hi - lo).abs() / (hi + lo).max(1e-30) >= 0.6 {
                    spill = true;
                }
            }
            ch.spill_of_neighbour = spill;
            if ch.quality_db >= cfg.min_quality_db && !image && !spill {
                ch.confirmed = true;
                let pil = mean_of(band(18_900.0, 19_100.0));
                let nb = (mean_of(band(17_000.0, 18_500.0)) + mean_of(band(19_500.0, 21_000.0))) / 2.0;
                ch.pilot_db = Some(10.0 * (pil / nb.max(1e-30)).log10());
                // RDS PI is best effort: extract_rds takes audio at its own rate.
                ch.rds_pi = extract_rds(&d_raw, fs_b as u32).map(|r| r.pi_code);
            }
        }
        out.push(ch);
    }
    out
}

/// Discriminator-noise calibration: with pure noise in the channel the ratio of
/// 0.1-15 kHz to 72-90 kHz density is about -20 dB (noise density rises with
/// frequency squared), so adding this offset makes "noise only" read ~0 dB.
pub const QUALITY_NOISE_OFFSET_DB: f64 = 20.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdr::dtv::testgen::*;

    #[test]
    fn plan_covers_every_channel_once_and_keeps_dc_off_channels() {
        let plans = plan_captures(87_500_000, 108_000_000, 200_000, 2_048_000);
        let all: Vec<u64> = plans.iter().flat_map(|p| p.channels.clone()).collect();
        let expect: Vec<u64> = (0..).map(|k| 87_500_000 + k * 200_000).take_while(|f| *f <= 108_000_000).collect();
        assert_eq!(all, expect);
        assert_eq!(expect.len(), 103);
        for p in &plans {
            assert!(p.channels.len() <= 8);
            for c in &p.channels {
                assert!(c.abs_diff(p.centre_hz) >= 50_000, "channel {} on DC of capture {}", c, p.centre_hz);
                assert!(c.abs_diff(p.centre_hz) <= 800_000);
            }
        }
        assert_eq!(plans.len(), 13);
    }

    /// Build a scene: stations at (channel index within the 8-channel capture, level dB re strongest).
    fn scene(seed: u64, stations: &[(usize, f64)], snr_total_db: f64) -> (Vec<u8>, Vec<u64>, u64) {
        let centre = 98_000_000u64; // between channels
        let chans: Vec<u64> = (0..8).map(|i| centre - 700_000 + 200_000 * i as u64).collect();
        let mut rng = Rng::new(seed);
        let fs_n = 8_192_000.0;
        let need = (CAPTURE_LEN as f64 * fs_n / FS_OUT).ceil() as usize + 1024 + 16;
        let st: Vec<(f64, f64)> = stations.iter().map(|(i, db)| (chans[*i] as f64 - centre as f64, 10f64.powf(db / 20.0))).collect();
        let rec = fm_record(&mut rng, fs_n, need, &st);
        let iq = capture_from_record(&mut rng, &rec, fs_n, FrontEnd::standard(snr_total_db, 0.0));
        (iq, chans, centre)
    }

    #[test]
    fn stations_are_confirmed_and_empty_channels_are_not() {
        let mut table = Vec::new();
        for seed in 0..12u64 {
            // Strong, medium and weak stations, an empty channel next to a strong one.
            let (iq, chans, centre) = scene(seed, &[(1, 0.0), (3, -10.0), (4, -22.0), (6, -14.0)], 40.0);
            let r = evaluate_capture(&iq, 2_048_000, centre, &chans, &SurveyConfig::default());
            assert_eq!(r.len(), 8);
            for (i, c) in r.iter().enumerate() {
                let want = [1usize, 3, 4, 6].contains(&i);
                table.push(format!("seed {seed} ch{i} want={want} excess={:.1} quality={:.1} confirmed={}", c.excess_db, c.quality_db, c.confirmed));
                assert_eq!(c.confirmed, want, "seed {seed} channel {i}: {c:?}");
            }
        }
        println!("{}", table.join("\n"));
    }

    #[test]
    fn noise_only_captures_confirm_nothing() {
        for seed in 0..40u64 {
            let (iq, chans, centre) = scene(1000 + seed, &[], 10.0);
            let r = evaluate_capture(&iq, 2_048_000, centre, &chans, &SurveyConfig::default());
            assert!(r.iter().all(|c| !c.confirmed), "seed {seed}: {r:?}");
        }
    }

    #[test]
    fn dense_band_with_strong_adjacent_stations() {
        // Random occupancy (adjacent channels allowed), levels spread over 25 dB.
        let mut rng = Rng::new(777);
        let (mut missed, mut false_pos, mut total_st, mut total_empty) = (0, 0, 0, 0);
        let (mut spill_total, mut spill_confirmed) = (0usize, 0usize);
        for seed in 0..30u64 {
            let mut st = Vec::new();
            let mut occupied = [false; 8];
            for (i, occ) in occupied.iter_mut().enumerate() {
                if rng.uniform() < 0.6 {
                    *occ = true;
                    st.push((i, -rng.range(0.0, 25.0)));
                }
            }
            let (iq, chans, centre) = scene(5000 + seed, &st, 45.0);
            let r = evaluate_capture(&iq, 2_048_000, centre, &chans, &SurveyConfig::default());
            for (i, c) in r.iter().enumerate() {
                if occupied[i] {
                    total_st += 1;
                    if !c.confirmed {
                        missed += 1;
                        println!("MISSED seed {seed} ch{i}: {c:?} lvl {:?}", st.iter().find(|(j, _)| *j == i));
                    }
                } else {
                    // An empty channel directly beside a station is physically not
                    // empty: a transmitter's spectral mask allows adjacent-channel
                    // energy only 25-35 dB down, which at this dynamic range is far
                    // above the noise floor (a rtl_power survey would flag it too).
                    // Those are counted separately and not treated as false positives.
                    let beside_station = (i > 0 && occupied[i - 1]) || (i < 7 && occupied[i + 1]);
                    if beside_station {
                        spill_total += 1;
                        spill_confirmed += c.confirmed as usize;
                        continue;
                    }
                    total_empty += 1;
                    if c.confirmed {
                        false_pos += 1;
                        println!("FALSE seed {seed} ch{i}: {c:?}");
                    }
                }
            }
        }
        println!("dense band: stations {total_st} missed {missed}; isolated empty channels {total_empty} false positives {false_pos}; empty channels beside a station {spill_total} (adjacent spill read as occupied: {spill_confirmed})");
        assert_eq!(missed, 0);
        assert_eq!(false_pos, 0);
    }

    #[test]
    fn reference_table_confirmation_rate_vs_channel_snr() {
        println!("channel SNR (dB) | trials | confirmed");
        for ch_snr in [0.0f64, 3.0, 6.0, 9.0, 12.0, 15.0, 20.0] {
            let mut ok = 0;
            let trials = 20;
            for seed in 0..trials {
                let (iq, chans, centre) = scene(9000 + seed as u64, &[(2, 0.0)], ch_snr - 10.1);
                let r = evaluate_capture(&iq, 2_048_000, centre, &chans, &SurveyConfig::default());
                ok += r[2].confirmed as usize;
            }
            println!("{:>16.0} | {:>6} | {:>5}/{}", ch_snr, trials, ok, trials);
            if ch_snr >= 9.0 {
                assert_eq!(ok, trials, "stations at {ch_snr} dB channel SNR must be confirmed");
            }
        }
    }

    /// 16 channels at 100 kHz spacing around 98 MHz (offsets -750..+750 kHz).
    fn scene100(seed: u64, stations: &[(usize, f64)], snr_total_db: f64) -> (Vec<u8>, Vec<u64>, u64) {
        let centre = 98_000_000u64;
        let chans: Vec<u64> = (0..16).map(|i| centre - 750_000 + 100_000 * i as u64).collect();
        let mut rng = Rng::new(seed);
        let fs_n = 8_192_000.0;
        let need = (CAPTURE_LEN as f64 * fs_n / FS_OUT).ceil() as usize + 1024 + 16;
        let st: Vec<(f64, f64)> = stations.iter().map(|(i, db)| (chans[*i] as f64 - centre as f64, 10f64.powf(db / 20.0))).collect();
        let rec = fm_record(&mut rng, fs_n, need, &st);
        let iq = capture_from_record(&mut rng, &rec, fs_n, FrontEnd::standard(snr_total_db, 0.0));
        (iq, chans, centre)
    }

    #[test]
    fn plan_covers_a_100khz_raster_once() {
        let plans = plan_captures(87_500_000, 108_000_000, 100_000, 2_048_000);
        let all: Vec<u64> = plans.iter().flat_map(|p| p.channels.clone()).collect();
        let expect: Vec<u64> = (0..).map(|k| 87_500_000 + k * 100_000).take_while(|f| *f <= 108_000_000).collect();
        assert_eq!(all, expect);
        assert_eq!(expect.len(), 206);
        for p in &plans {
            assert!(p.channels.len() <= 16);
            assert!(p.channels.iter().all(|c| c.abs_diff(p.centre_hz) <= 800_000));
        }
    }

    #[test]
    fn eu_like_even_tenth_stations_are_all_found_on_a_100khz_raster() {
        // Stations on every other 100 kHz channel (200 kHz apart) at EVEN tenths,
        // i.e. the half of the raster a 200 kHz grid from 87.5 MHz never visits.
        // Levels span 12 dB, so each odd channel in between sits beside two stations.
        let cfg = SurveyConfig { channel_step_hz: 100_000.0, ..SurveyConfig::default() };
        let (mut stations, mut found, mut between, mut between_conf) = (0, 0, 0, 0);
        let mut rng = Rng::new(31337);
        for seed in 0..20u64 {

            let occupied: Vec<usize> = (0..16).filter(|i| i % 2 == 1).collect();
            let st: Vec<(usize, f64)> = occupied.iter().map(|&i| (i, -rng.range(0.0, 12.0))).collect();
            let (iq, chans, centre) = scene100(8000 + seed, &st, 45.0);
            let r = evaluate_capture(&iq, 2_048_000, centre, &chans, &cfg);
            for (i, c) in r.iter().enumerate() {
                if i % 2 == 1 {
                    stations += 1;
                    found += c.confirmed as usize;
                    if !c.confirmed {
                        println!("MISSED seed {seed} ch{i}: {c:?}");
                    }
                } else {
                    between += 1;
                    between_conf += c.confirmed as usize;
                }
            }
        }
        println!("100 kHz raster: stations {stations} found {found}; channels between stations {between}, read as stations {between_conf}");
        assert_eq!(found, stations, "every station must be found");
        assert_eq!(between_conf, 0, "spill between two stations must not be reported as a station");
    }

    #[test]
    fn strong_station_does_not_create_a_phantom_neighbour_on_a_100khz_raster() {
        let cfg = SurveyConfig { channel_step_hz: 100_000.0, ..SurveyConfig::default() };
        let mut phantom = 0;
        for seed in 0..20u64 {
            let (iq, chans, centre) = scene100(8500 + seed, &[(5, 0.0)], 45.0);
            let r = evaluate_capture(&iq, 2_048_000, centre, &chans, &cfg);
            assert!(r[5].confirmed, "seed {seed}");
            phantom += r.iter().enumerate().filter(|(i, c)| *i != 5 && c.confirmed).count();
        }
        assert_eq!(phantom, 0);
    }
}

//! Characterisation tests for the ATSC pilot path. The expected numbers were
//! recorded on the base commit (before any DVB work) and must never change:
//! the ATSC detector and its `dtv_atsc` emission stay byte-identical.

use super::extract_atsc_pilot;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f64) / (1u64 << 31) as f64
    }
    fn gauss(&mut self) -> f64 {
        (0..4).map(|_| self.next()).sum::<f64>() - 2.0
    }
}

/// Synthetic ATSC-like capture: pilot tone `pilot_offset_hz` from the tuned
/// centre plus 8VSB-like wideband noise.
fn synthetic_atsc(pilot_offset_hz: f64, pilot_amp: f64, noise_sigma: f64, seed: u64) -> Vec<u8> {
    let fs = 2_048_000.0;
    let mut rng = Lcg(seed);
    let mut out = Vec::with_capacity(262_144);
    for n in 0..131_072usize {
        let ph = 2.0 * std::f64::consts::PI * pilot_offset_hz * n as f64 / fs;
        let i = pilot_amp * ph.cos() + noise_sigma * rng.gauss();
        let q = pilot_amp * ph.sin() + noise_sigma * rng.gauss();
        out.push((127.5 + 127.5 * i).round().clamp(0.0, 255.0) as u8);
        out.push((127.5 + 127.5 * q).round().clamp(0.0, 255.0) as u8);
    }
    out
}

#[test]
fn atsc_pilot_golden_values_with_pilot() {
    // Channel 19 pilot = 500_309_440.559 Hz. The detector maps raw FFT bin k to
    // centre + (k - N/2) * fs / N (no fftshift), so a +100 kHz tone is read as
    // centre - 924 kHz; this characterises that behaviour as shipped.
    let centre = 500_309_441 + 924_000;
    let iq = synthetic_atsc(100_000.0, 0.12, 0.10, 42);
    let r = extract_atsc_pilot(&iq, 2_048_000, centre).expect("pilot must be detected");
    println!("GOLDEN pilot_frequency_hz={:.3} frequency_offset_hz={:.3} pilot_snr_db={:.3} channel_power_dbfs={:.3}", r.pilot_frequency_hz, r.frequency_offset_hz, r.pilot_snr_db, r.channel_power_dbfs);
    assert!(r.pilot_detected);
    assert!((r.pilot_frequency_hz - GOLD_FREQ).abs() < 0.01, "{}", r.pilot_frequency_hz);
    assert!((f64::from(r.frequency_offset_hz) - GOLD_OFFSET).abs() < 0.01, "{}", r.frequency_offset_hz);
    assert!((f64::from(r.pilot_snr_db) - GOLD_SNR).abs() < 0.01, "{}", r.pilot_snr_db);
    assert!((f64::from(r.channel_power_dbfs) - GOLD_POWER).abs() < 0.01, "{}", r.channel_power_dbfs);
}

#[test]
fn atsc_pilot_absent_in_pure_noise() {
    let iq = synthetic_atsc(0.0, 0.0, 0.12, 7);
    assert!(extract_atsc_pilot(&iq, 2_048_000, 500_309_441 + 924_000).is_none());
}

const GOLD_FREQ: f64 = 500309441.000;
const GOLD_OFFSET: f64 = 0.441;
const GOLD_SNR: f64 = 52.090;
const GOLD_POWER: f64 = 31.397;

#![allow(dead_code)]

use rustfft::{num_complex::Complex, FftPlanner};

pub mod dvb;
pub mod p1;
#[cfg(test)]
pub(crate) mod testgen;
#[cfg(test)]
mod validation;
#[cfg(test)]
pub(crate) mod testgen_t2;
#[cfg(test)]
mod validation_t2;
#[cfg(test)]
mod atsc_regression;

/// Result from ATSC DTV pilot tone detection.
///
/// Contains pilot frequency measurement, signal quality, and channel power estimates.
/// Use `pilot_detected` to determine if a valid ATSC pilot was found.
#[derive(Debug, Clone)]
pub struct AtscPilotResult {
    /// True if a valid ATSC pilot tone was detected
    pub pilot_detected: bool,
    /// Measured pilot frequency in Hz (center of detected peak)
    pub pilot_frequency_hz: f64,
    /// Offset from nominal ATSC pilot frequency in Hz
    pub frequency_offset_hz: f32,
    /// Signal-to-noise ratio of pilot relative to local noise floor (dB)
    pub pilot_snr_db: f32,
    /// Overall channel power in dBFS
    pub channel_power_dbfs: f32,
}

// ATSC DTV constants
const ATSC_PILOT_OFFSET_HZ: f64 = 309440.559;
const ATSC_CHANNEL_BW_HZ: u64 = 6_000_000;
const ATSC_UHF_FIRST_CHANNEL: u32 = 14;
const ATSC_UHF_LAST_CHANNEL: u32 = 36;
const PILOT_DETECTION_THRESHOLD_DB: f32 = 15.0;
const PILOT_FREQ_TOLERANCE_HZ: f64 = 1000.0;

/// Compute the lower edge frequency for an ATSC UHF channel.
///
/// ATSC UHF channels 14-36 (post-repack) follow:
/// Channel N lower edge = 470 MHz + (N - 14) * 6 MHz
fn atsc_channel_lower_edge(channel: u32) -> u64 {
    470_000_000 + (channel - ATSC_UHF_FIRST_CHANNEL) as u64 * ATSC_CHANNEL_BW_HZ
}

/// Compute the nominal pilot frequency for an ATSC channel.
fn atsc_pilot_frequency(channel: u32) -> f64 {
    atsc_channel_lower_edge(channel) as f64 + ATSC_PILOT_OFFSET_HZ
}

/// Extract ATSC pilot tone from raw RTL-SDR IQ capture.
///
/// Analyzes an RTL-SDR IQ capture (interleaved u8 I, Q pairs) to detect the presence
/// of an ATSC DTV pilot tone. The ATSC pilot is a stable CW carrier at a well-defined
/// offset from the channel lower edge, making it valuable as a timing reference.
///
/// # Arguments
///
/// * `iq_bytes` - Raw RTL-SDR IQ data (interleaved unsigned 8-bit I, Q pairs).
///   Minimum 4096 bytes required; returns None if shorter.
/// * `sample_rate` - SDR sample rate in Hz (typically 2,048,000 for RTL-SDR).
/// * `center_freq_hz` - RTL-SDR center frequency in Hz.
///
/// # Returns
///
/// `Some(AtscPilotResult)` if a valid ATSC pilot is detected, `None` otherwise.
///
/// # Algorithm
///
/// 1. Convert raw u8 IQ to Complex<f32> with normalization: (byte - 127.5) / 127.5
/// 2. Compute FFT power spectrum
/// 3. Identify strongest spectral peak above local noise floor (±50 bin window)
/// 4. Verify peak is > 15 dB above surrounding noise
/// 5. Check if peak frequency matches any valid ATSC pilot within ±1000 Hz
/// 6. Compute SNR and frequency offset
pub fn extract_atsc_pilot(
    iq_bytes: &[u8],
    sample_rate: u32,
    center_freq_hz: u64,
) -> Option<AtscPilotResult> {
    // Step 1: Validate input length
    if iq_bytes.len() < 4096 {
        return None;
    }

    let n_samples = iq_bytes.len() / 2;
    if n_samples == 0 {
        return None;
    }

    // Step 2: Convert u8 IQ to Complex<f32>
    let mut iq_samples = Vec::with_capacity(n_samples);
    for i in 0..n_samples {
        let i_byte = iq_bytes[2 * i] as f32;
        let q_byte = iq_bytes[2 * i + 1] as f32;
        let i_norm = (i_byte - 127.5) / 127.5;
        let q_norm = (q_byte - 127.5) / 127.5;
        iq_samples.push(Complex {
            re: i_norm,
            im: q_norm,
        });
    }

    // Step 3: Find largest power-of-2 FFT size <= n_samples
    let fft_size = (n_samples as u32).next_power_of_two() / 2;
    if fft_size < 512 {
        return None;
    }
    let fft_size = fft_size as usize;

    // Truncate to FFT size and compute FFT
    let mut fft_input = iq_samples[0..fft_size].to_vec();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    fft.process(&mut fft_input);

    // Step 4: Compute power spectrum and channel power
    let mut power_spectrum = vec![0.0f32; fft_size];
    let mut power_sum = 0.0f64;

    for (i, sample) in fft_input.iter().enumerate() {
        let pwr = (sample.re * sample.re + sample.im * sample.im) as f64;
        power_spectrum[i] = pwr as f32;
        power_sum += pwr;
    }

    let mean_power = power_sum / fft_size as f64;
    if mean_power <= 1e-20 {
        return None;
    }

    let channel_power_dbfs = 10.0 * (mean_power).log10() as f32;

    // Step 5: Find strongest spectral peak above local noise floor
    let mut peak_bin = 0;
    let mut peak_to_noise_ratio = 0.0f32;

    const NOISE_WINDOW_BINS: usize = 50;
    const PEAK_EXCLUSION_BINS: usize = 3;

    for bin in PEAK_EXCLUSION_BINS..(fft_size - PEAK_EXCLUSION_BINS) {
        // Compute local noise floor: mean power of ±50 bins (excluding ±3 around candidate)
        let mut noise_sum = 0.0f64;
        let mut noise_count = 0;

        for offset in -(NOISE_WINDOW_BINS as i32)..=(NOISE_WINDOW_BINS as i32) {
            let check_bin = (bin as i32 + offset) as usize;
            // Skip bins outside FFT range, and skip ±3 around the candidate
            if check_bin >= fft_size || (offset > -4 && offset < 4) {
                continue;
            }
            noise_sum += power_spectrum[check_bin] as f64;
            noise_count += 1;
        }

        if noise_count == 0 {
            continue;
        }

        let local_noise_floor = noise_sum / noise_count as f64;
        if local_noise_floor <= 1e-20 {
            continue;
        }

        let ratio = (power_spectrum[bin] as f64 / local_noise_floor) as f32;

        if ratio > peak_to_noise_ratio {
            peak_to_noise_ratio = ratio;
            peak_bin = bin;
        }
    }

    // Step 6: Verify peak is above detection threshold
    let threshold_linear = 10.0_f32.powf(PILOT_DETECTION_THRESHOLD_DB / 10.0);
    if peak_to_noise_ratio < threshold_linear {
        return None;
    }

    // Step 7: Compute frequency of peak bin
    let center_dc = fft_size as f64 / 2.0;
    let bin_freq_offset =
        (peak_bin as f64 - center_dc) * sample_rate as f64 / fft_size as f64;
    let measured_freq = center_freq_hz as f64 + bin_freq_offset;

    // Step 8: Verify it matches an ATSC pilot frequency
    let mut best_channel = None;
    let mut best_offset = PILOT_FREQ_TOLERANCE_HZ + 1.0;

    for channel in ATSC_UHF_FIRST_CHANNEL..=ATSC_UHF_LAST_CHANNEL {
        let nominal_pilot = atsc_pilot_frequency(channel);
        let offset = (measured_freq - nominal_pilot).abs();
        if offset < best_offset && offset <= PILOT_FREQ_TOLERANCE_HZ {
            best_offset = offset;
            best_channel = Some(channel);
        }
    }

    let best_channel = best_channel?;
    let nominal_pilot_freq = atsc_pilot_frequency(best_channel);

    // Step 9: Compute pilot SNR
    let mut pilot_power = power_spectrum[peak_bin] as f64;

    // Add leakage from adjacent bins
    if peak_bin > 0 {
        pilot_power += power_spectrum[peak_bin - 1] as f64;
    }
    if peak_bin < fft_size - 1 {
        pilot_power += power_spectrum[peak_bin + 1] as f64;
    }

    // Compute noise floor: mean of ±50 bins, excluding ±5 around peak
    let mut noise_sum = 0.0f64;
    let mut noise_count = 0;

    for offset in -(NOISE_WINDOW_BINS as i32)..=(NOISE_WINDOW_BINS as i32) {
        let check_bin = (peak_bin as i32 + offset) as usize;
        if check_bin >= fft_size || (offset > -6 && offset < 6) {
            continue;
        }
        noise_sum += power_spectrum[check_bin] as f64;
        noise_count += 1;
    }

    let noise_power = if noise_count > 0 {
        noise_sum / noise_count as f64
    } else {
        1e-20
    };

    let pilot_snr_db = if noise_power > 1e-20 {
        10.0 * (pilot_power / noise_power).log10() as f32
    } else {
        99.0
    };

    // Step 10: Compute frequency offset
    let frequency_offset_hz = (measured_freq - nominal_pilot_freq) as f32;

    Some(AtscPilotResult {
        pilot_detected: true,
        pilot_frequency_hz: measured_freq,
        frequency_offset_hz,
        pilot_snr_db,
        channel_power_dbfs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atsc_channel_frequencies() {
        // Channel 14 lower edge: 470 MHz
        assert_eq!(atsc_channel_lower_edge(14), 470_000_000);

        // Channel 15 lower edge: 476 MHz
        assert_eq!(atsc_channel_lower_edge(15), 476_000_000);

        // Channel 36 lower edge: 602 MHz
        assert_eq!(atsc_channel_lower_edge(36), 602_000_000);
    }

    #[test]
    fn test_atsc_pilot_frequencies() {
        // Channel 14 pilot: 470 MHz + 309.44059 kHz
        let ch14_pilot = atsc_pilot_frequency(14);
        assert!((ch14_pilot - 470_309_440.559).abs() < 1.0);

        // Channel 19 pilot
        let ch19_pilot = atsc_pilot_frequency(19);
        assert!((ch19_pilot - 500_309_440.559).abs() < 1.0);
    }

    #[test]
    fn test_empty_input() {
        let result = extract_atsc_pilot(&[], 2_048_000, 470_000_000);
        assert!(result.is_none());
    }

    #[test]
    fn test_short_input() {
        let result = extract_atsc_pilot(&[0u8; 2048], 2_048_000, 470_000_000);
        assert!(result.is_none());
    }

    #[test]
    fn test_input_exactly_4096_bytes() {
        // This should attempt processing, but with no signal it likely won't detect a pilot
        let result = extract_atsc_pilot(&[127u8; 4096], 2_048_000, 470_000_000);
        // Result depends on signal content; we just verify it doesn't panic
        let _ = result;
    }
}

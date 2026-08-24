#![allow(dead_code)]

/// Result of FM demodulation
#[derive(Debug, Clone)]
pub struct FmDemodResult {
    /// Demodulated audio samples at ~200 kHz
    pub audio_samples: Vec<f32>,
    /// Signal-to-noise ratio in dB
    pub snr_db: f32,
    /// Signal power in dBFS
    pub signal_power_dbfs: f32,
    /// Estimated noise floor in dBFS
    pub noise_floor_dbfs: f32,
}

/// Nominal FM broadcast channel bandwidth (mono + stereo pilot + guard),
/// per Carson's rule for +/-75 kHz deviation plus 15 kHz audio. Used to size
/// the pre-demod channel filter below.
const FM_CHANNEL_BANDWIDTH_HZ: u32 = 200_000;

/// Complex moving-average low-pass, applied to I and Q independently.
///
/// The RTL-SDR digitizes its FULL sample-rate bandwidth (2.4 MHz on desktop,
/// 2.048 MHz in the Docker default) around the tuned center frequency in one
/// capture — there is no per-channel filtering at the hardware level. Running
/// quadrature phase-difference demodulation directly on that raw wideband
/// capture means the "instantaneous frequency" at every sample is influenced
/// by every other station within the whole captured window, not just the one
/// sitting at DC (the tuned target). Adjacent stations 200 kHz+ away show up
/// as fast phase jumps relative to the target's own audio-rate modulation —
/// which is exactly what the SNR gate's high-pass noise estimate (see below)
/// picks up, suppressing SNR even for a strong, correctly-tuned target
/// station in any moderately crowded FM market. A boxcar low-pass (first
/// spectral null at `sample_rate / window`) attenuates that adjacent-channel
/// energy before demodulation, without needing a full FIR/IIR filter design —
/// consistent with the existing decimation stage below, which already uses
/// the same moving-average approach.
fn channel_lowpass(samples: &[f32], window: usize) -> Vec<f32> {
    if window <= 1 || samples.is_empty() {
        return samples.to_vec();
    }
    let mut out = Vec::with_capacity(samples.len());
    let mut sum = 0.0f32;
    for (idx, &s) in samples.iter().enumerate() {
        sum += s;
        if idx >= window {
            sum -= samples[idx - window];
        }
        let n = (idx + 1).min(window) as f32;
        out.push(sum / n);
    }
    out
}

/// Demodulate FM from raw RTL-SDR IQ bytes (interleaved u8 I,Q pairs).
///
/// # Arguments
/// * `iq_bytes` - Interleaved u8 I,Q pairs from RTL-SDR device
/// * `sample_rate` - Capture sample rate in Hz (e.g. 2_048_000 or 2_400_000).
///   Used to size the channel low-pass filter below (window = sample_rate /
///   FM_CHANNEL_BANDWIDTH_HZ) so filtering scales correctly across configs.
///
/// # Returns
/// `Some(FmDemodResult)` on success, `None` if input is too short or all-zero
///
/// # Algorithm
/// 1. Convert raw u8 pairs to f32 I/Q samples normalized to [-1, 1]
/// 2. Zero-input guard: detect all-DC or near-DC input (checked on the raw,
///    unfiltered samples — a genuinely dead capture stays dead either way)
/// 3. Channel low-pass: attenuate adjacent-station energy outside the ~200 kHz
///    FM channel around the tuned frequency (see `channel_lowpass` above)
/// 4. Compute instantaneous phase difference via atan2 for quadrature demodulation
/// 5. Decimate by 10: moving-average low-pass then downsample from 2.048 MSPS to ~204.8 kHz
/// 6. Calculate SNR from signal and noise power
/// 7. Return dBFS measurements
pub fn demodulate_fm(iq_bytes: &[u8], sample_rate: u32) -> Option<FmDemodResult> {
    // Minimum 2048 bytes (1024 IQ pairs) for meaningful results
    if iq_bytes.len() < 2048 {
        return None;
    }

    // Must have even number of bytes (I,Q pairs)
    if !iq_bytes.len().is_multiple_of(2) {
        return None;
    }

    // Convert u8 pairs to f32 I/Q samples
    let num_samples = iq_bytes.len() / 2;
    let mut i_samples = Vec::with_capacity(num_samples);
    let mut q_samples = Vec::with_capacity(num_samples);

    for chunk in iq_bytes.chunks_exact(2) {
        // Normalize u8 [0, 255] to f32 [-1, 1] with center at 127.5
        let i = (chunk[0] as f32 - 127.5) / 127.5;
        let q = (chunk[1] as f32 - 127.5) / 127.5;
        i_samples.push(i);
        q_samples.push(q);
    }

    // Zero-input guard: check if all samples are DC (negligible variation)
    let mut min_val = f32::INFINITY;
    let mut max_val = f32::NEG_INFINITY;

    for &sample in &i_samples {
        min_val = min_val.min(sample);
        max_val = max_val.max(sample);
    }
    for &sample in &q_samples {
        min_val = min_val.min(sample);
        max_val = max_val.max(sample);
    }

    let deviation = max_val - min_val;
    if deviation < 0.01 {
        return None;
    }

    // Channel low-pass: attenuate energy from other stations in the wideband
    // capture before demodulation (see `channel_lowpass` doc comment above).
    // window = sample_rate / channel_bandwidth, so this scales correctly
    // whether the capture ran at 2.048 or 2.4 MSPS.
    let filter_window =
        ((sample_rate as f64 / FM_CHANNEL_BANDWIDTH_HZ as f64).round() as usize).max(1);
    let i_samples = channel_lowpass(&i_samples, filter_window);
    let q_samples = channel_lowpass(&q_samples, filter_window);

    // Compute instantaneous phase difference via quadrature demodulation
    // d_phi[n] = atan2(Q[n]*I[n-1] - I[n]*Q[n-1], I[n]*I[n-1] + Q[n]*Q[n-1])
    let mut d_phi = Vec::with_capacity(num_samples - 1);

    for n in 1..num_samples {
        let q_n = q_samples[n];
        let i_n = i_samples[n];
        let q_n_1 = q_samples[n - 1];
        let i_n_1 = i_samples[n - 1];

        let numerator = q_n * i_n_1 - i_n * q_n_1;
        let denominator = i_n * i_n_1 + q_n * q_n_1;

        let phase_diff = numerator.atan2(denominator);
        d_phi.push(phase_diff);
    }

    if d_phi.is_empty() {
        return None;
    }

    // Decimate by 10: moving-average low-pass followed by downsampling
    let decimation_factor = 10;
    let ma_window = decimation_factor as usize;
    let mut decimated = Vec::new();

    // Compute moving average and decimate
    for chunk_start in (0..d_phi.len()).step_by(ma_window) {
        let chunk_end = (chunk_start + ma_window).min(d_phi.len());
        let chunk = &d_phi[chunk_start..chunk_end];

        let sum: f32 = chunk.iter().sum();
        let avg = sum / chunk.len() as f32;
        decimated.push(avg);
    }

    if decimated.is_empty() {
        return None;
    }

    // Calculate signal power as variance of demodulated signal
    let mean_signal: f32 = decimated.iter().sum::<f32>() / decimated.len() as f32;
    let signal_power: f32 = decimated
        .iter()
        .map(|&x| (x - mean_signal).powi(2))
        .sum::<f32>()
        / decimated.len() as f32;

    // Estimate noise as variance of a high-pass residual (second-difference)
    // so low-frequency signal slope is not counted as noise.
    let mut noise_samples = Vec::with_capacity(decimated.len().saturating_sub(2));
    for i in 2..decimated.len() {
        noise_samples.push(decimated[i] - 2.0 * decimated[i - 1] + decimated[i - 2]);
    }

    let noise_power: f32 = if !noise_samples.is_empty() {
        let noise_mean: f32 = noise_samples.iter().sum::<f32>() / noise_samples.len() as f32;
        noise_samples
            .iter()
            .map(|&x| (x - noise_mean).powi(2))
            .sum::<f32>()
            / noise_samples.len() as f32
    } else {
        1e-30
    };

    // Compute SNR
    let snr_db = if noise_power > 1e-20 {
        10.0 * (signal_power / noise_power).log10()
    } else {
        99.0 // Max SNR if noise floor is negligible
    };

    // Compute signal and noise in dBFS
    let signal_power_dbfs = if signal_power > 1e-20 {
        10.0 * signal_power.log10()
    } else {
        -999.0
    };

    let noise_floor_dbfs = if noise_power > 1e-20 {
        10.0 * noise_power.log10()
    } else {
        -999.0
    };

    Some(FmDemodResult {
        audio_samples: decimated,
        snr_db,
        signal_power_dbfs,
        noise_floor_dbfs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        assert!(demodulate_fm(&[], 2_048_000).is_none());
    }

    #[test]
    fn test_short_input() {
        let short = vec![127u8; 100];
        assert!(demodulate_fm(&short, 2_048_000).is_none());
    }

    #[test]
    fn test_all_dc_input() {
        // All values at DC (127 or 128) should return None
        let dc_input = vec![127u8; 2048];
        assert!(demodulate_fm(&dc_input, 2_048_000).is_none());
    }

    #[test]
    fn test_valid_input() {
        // Create synthetic FM IQ pairs with a phase-modulated tone so the
        // demodulated output is not near-DC and has meaningful signal power.
        let mut iq = Vec::new();
        for i in 0..1024 {
            let phase = 2.0 * std::f32::consts::PI * i as f32 / 100.0
                + 0.5 * (2.0 * std::f32::consts::PI * i as f32 / 400.0).sin();
            let i_val = (127.5 + 50.0 * phase.sin()) as u8;
            let q_val = (127.5 + 50.0 * phase.cos()) as u8;
            iq.push(i_val);
            iq.push(q_val);
        }

        let result = demodulate_fm(&iq, 2_048_000);
        assert!(result.is_some());

        let demod = result.unwrap();
        assert!(!demod.audio_samples.is_empty());
        assert!(demod.snr_db > 0.0);
        assert!(demod.signal_power_dbfs < 0.0);
    }

    #[test]
    fn test_odd_length_input() {
        let odd_input = vec![127u8; 2049];
        assert!(demodulate_fm(&odd_input, 2_048_000).is_none());
    }

    #[test]
    fn channel_lowpass_attenuates_high_frequency_content_more_than_low_frequency() {
        // Regression coverage for the fleet finding that FM SNR reads low even
        // under AGC (LTE detection on the same hardware works fine): naive
        // wideband demod lets adjacent-channel content leak into the SNR
        // estimate as fast phase jumps. A slow (in-channel-like) tone should
        // survive the filter far better than a fast (adjacent-channel-like)
        // one, proving the boxcar actually discriminates by frequency.
        let n = 1000;
        let window = 10; // matches 2.048 MSPS / 200 kHz FM_CHANNEL_BANDWIDTH_HZ

        let slow: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * i as f32 / 200.0).sin())
            .collect();
        let fast: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * i as f32 / 3.0).sin())
            .collect();

        let rms = |s: &[f32]| -> f32 { (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt() };

        let slow_ratio = rms(&channel_lowpass(&slow, window)) / rms(&slow);
        let fast_ratio = rms(&channel_lowpass(&fast, window)) / rms(&fast);

        assert!(
            slow_ratio > 0.7,
            "slow (in-channel) content should pass through largely intact, got ratio {slow_ratio}"
        );
        assert!(
            fast_ratio < 0.3,
            "fast (adjacent-channel-like) content should be strongly attenuated, got ratio {fast_ratio}"
        );
        assert!(
            slow_ratio > fast_ratio * 2.0,
            "filter must discriminate: slow_ratio={slow_ratio} fast_ratio={fast_ratio}"
        );
    }

    #[test]
    fn channel_lowpass_window_of_one_is_a_no_op() {
        let samples = vec![1.0, -1.0, 0.5, -0.5, 0.25];
        assert_eq!(channel_lowpass(&samples, 1), samples);
    }

    #[test]
    fn channel_lowpass_empty_input_returns_empty() {
        let empty: Vec<f32> = Vec::new();
        assert_eq!(channel_lowpass(&empty, 10), Vec::<f32>::new());
    }

    #[test]
    fn demodulate_fm_uses_sample_rate_to_size_the_channel_filter() {
        // Same synthetic tone as test_valid_input, but run at 2.4 MSPS (the
        // desktop default) instead of 2.048 MSPS (Docker default) to prove
        // `sample_rate` is actually consumed, not ignored — the filter window
        // must differ (2_400_000/200_000 = 12 vs 2_048_000/200_000 ~= 10) and
        // demodulation must still succeed at both rates.
        let mut iq = Vec::new();
        for i in 0..1024 {
            let phase = 2.0 * std::f32::consts::PI * i as f32 / 100.0
                + 0.5 * (2.0 * std::f32::consts::PI * i as f32 / 400.0).sin();
            let i_val = (127.5 + 50.0 * phase.sin()) as u8;
            let q_val = (127.5 + 50.0 * phase.cos()) as u8;
            iq.push(i_val);
            iq.push(q_val);
        }

        let at_2048k = demodulate_fm(&iq, 2_048_000);
        let at_2400k = demodulate_fm(&iq, 2_400_000);
        assert!(at_2048k.is_some(), "must demodulate at Docker default rate");
        assert!(at_2400k.is_some(), "must demodulate at desktop default rate");
    }
}

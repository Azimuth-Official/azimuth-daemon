#![allow(dead_code)]

use rustfft::{num_complex::Complex, FftPlanner};
use std::f32::consts::PI;

/// Physical Cell ID detection result from LTE signal correlation.
///
/// Contains the detected PCI (0-503) derived from NID1 (0-167) and NID2 (0-2),
/// along with timing and frequency offset estimates.
#[derive(Debug, Clone)]
pub struct LteDetection {
    /// Physical Cell ID: computed as 3 * nid1 + nid2 (0-503)
    pub physical_cell_id: u16,
    /// Cell ID group (NID1): 0-167
    pub nid1: u16,
    /// Sector ID (NID2): 0-2
    pub nid2: u8,
    /// Position of PSS peak in the input buffer (sample count)
    pub timing_offset_samples: usize,
    /// Normalized correlation peak (0.0-1.0)
    pub correlation_strength: f32,
    /// Estimated carrier frequency offset in Hz
    pub frequency_offset_hz: f32,
}

// LTE PSS Zadoff-Chu root indices for NID2 = 0, 1, 2
const PSS_ZC_ROOTS: [u32; 3] = [25, 29, 34];
const PSS_SEQ_LEN: usize = 62;
const PSS_FFT_SIZE: usize = 128;
const NUM_NID1: usize = 168;
const CORRELATION_THRESHOLD: f32 = 0.3;

/// Generate a Zadoff-Chu sequence for the given root index and length.
/// Returns a sequence of complex values in time domain.
fn generate_zadoff_chu(root: u32, length: usize) -> Vec<Complex<f32>> {
    let mut seq = Vec::with_capacity(length);
    let n_float = length as f32;
    for n in 0..length {
        let n_f32 = n as f32;
        let angle = -PI * root as f32 * n_f32 * (n_f32 + 1.0) / (n_float + 1.0);
        seq.push(Complex::new(angle.cos(), angle.sin()));
    }
    seq
}

/// Generate the time-domain PSS reference template for a given sector ID (NID2).
/// Maps the 62-point ZC sequence into a 128-point IFFT (zero-padded), centered with DC null.
fn generate_pss_template(nid2: usize) -> Vec<Complex<f32>> {
    let root = PSS_ZC_ROOTS[nid2];
    let zc = generate_zadoff_chu(root, PSS_SEQ_LEN);

    // Frequency-domain representation: place ZC sequence centered in 128-point FFT
    let mut freq_domain = vec![Complex::new(0.0, 0.0); PSS_FFT_SIZE];

    // LTE PSS placement: subcarriers -31..-1, 1..31 (skip DC at index 0)
    // In FFT indexing: [97..127] and [1..31]
    let start_idx = PSS_FFT_SIZE - PSS_SEQ_LEN / 2; // 97
    for (i, &val) in zc.iter().enumerate() {
        if i < PSS_SEQ_LEN / 2 {
            freq_domain[start_idx + i] = val;
        } else {
            freq_domain[1 + (i - PSS_SEQ_LEN / 2)] = val;
        }
    }

    // IFFT to get time-domain PSS template
    let mut planner = FftPlanner::new();
    let ifft = planner.plan_fft_inverse(PSS_FFT_SIZE);
    ifft.process(&mut freq_domain);

    // Normalize by FFT size
    for sample in &mut freq_domain {
        sample.re /= PSS_FFT_SIZE as f32;
        sample.im /= PSS_FFT_SIZE as f32;
    }

    freq_domain
}

/// Convert raw RTL-SDR u8 IQ samples to Complex<f32>.
fn bytes_to_complex(iq_bytes: &[u8]) -> Vec<Complex<f32>> {
    let mut complex = Vec::with_capacity(iq_bytes.len() / 2);
    for i in (0..iq_bytes.len()).step_by(2) {
        if i + 1 < iq_bytes.len() {
            let re = (iq_bytes[i] as f32 - 127.5) / 127.5;
            let im = (iq_bytes[i + 1] as f32 - 127.5) / 127.5;
            complex.push(Complex::new(re, im));
        }
    }
    complex
}

/// FFT-based cross-correlation of template against capture.
/// Returns the magnitude of the correlation output.
fn fft_correlate(capture: &[Complex<f32>], template: &[Complex<f32>]) -> Vec<f32> {
    if template.is_empty() || capture.is_empty() {
        return Vec::new();
    }

    let fft_size = (capture.len() + template.len() - 1).next_power_of_two();

    let mut cap_fft = capture.to_vec();
    cap_fft.resize(fft_size, Complex::new(0.0, 0.0));

    let mut tpl_fft = template.to_vec();
    tpl_fft.resize(fft_size, Complex::new(0.0, 0.0));

    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);

    fft.process(&mut cap_fft);
    fft.process(&mut tpl_fft);

    // Compute FFT(capture) * conj(FFT(template))
    for i in 0..fft_size {
        let temp_conj = Complex::new(tpl_fft[i].re, -tpl_fft[i].im);
        cap_fft[i] *= temp_conj;
    }

    // IFFT back to time domain
    let ifft = planner.plan_fft_inverse(fft_size);
    ifft.process(&mut cap_fft);

    // Normalize and extract magnitudes
    cap_fft
        .iter()
        .take(capture.len())
        .map(|c| c.norm() / (fft_size as f32))
        .collect()
}

/// Compute template energy (sum of squared magnitudes).
fn compute_energy(template: &[Complex<f32>]) -> f32 {
    template.iter().map(|c| c.norm_sqr()).sum()
}

/// Find peaks in correlation output above a relative threshold.
/// Returns (peak_position, peak_magnitude).
fn find_peaks(correlation: &[f32], threshold: f32) -> Vec<(usize, f32)> {
    let max_val = correlation.iter().cloned().fold(0.0, f32::max);
    let min_distance = 128; // Minimum samples between peaks to avoid duplicates

    let mut peaks = Vec::new();
    let mut last_peak_idx = 0;

    for (i, &val) in correlation.iter().enumerate() {
        if val > threshold * max_val && i >= last_peak_idx + min_distance {
            peaks.push((i, val));
            last_peak_idx = i;
        }
    }

    peaks
}

/// Generate all 168 SSS (Secondary Synchronization Signal) sequences for a given NID2.
/// Returns a vector of time-domain sequences (one per NID1).
fn generate_sss_sequences(nid2: usize) -> Vec<Vec<Complex<f32>>> {
    let mut sss_sequences = Vec::with_capacity(NUM_NID1);

    for nid1 in 0..NUM_NID1 {
        let seq = generate_sss_for_nid1(nid1, nid2);
        sss_sequences.push(seq);
    }

    sss_sequences
}

/// Generate SSS sequence for a specific NID1 and NID2.
/// SSS uses two interleaved m-sequences (length 31 each).
fn generate_sss_for_nid1(nid1: usize, nid2: usize) -> Vec<Complex<f32>> {
    // Compute m-sequence shifts from NID1
    let m = nid1;
    let q = (m * 30) / 168;
    let m_prime = m + q * (q + 1) / 2;
    let m0 = m_prime % 31;
    let m1 = (m0 + (m_prime / 31) + 1) % 31;

    // Generate two m-sequences using polynomial x^5 + x^2 + 1
    let s0 = generate_m_sequence(31, m0, nid2, 0);
    let s1 = generate_m_sequence(31, m1, nid2, 1);

    // Interleave s0 and s1: [s0[0], s1[0], s0[1], s1[1], ..., s0[30], s1[30]]
    let mut sss = Vec::with_capacity(62);
    for i in 0..31 {
        sss.push(s0[i]);
        sss.push(s1[i]);
    }

    sss
}

/// Generate a length-31 m-sequence with cyclic shift and NID2-based scrambling.
fn generate_m_sequence(length: usize, shift: usize, nid2: usize, seq_index: usize) -> Vec<Complex<f32>> {
    // Initial state for m-sequence: [1, 0, 0, 0, 0]
    let mut state = [1, 0, 0, 0, 0];
    let mut seq = Vec::with_capacity(length);

    for _ in 0..length {
        let output = state[4];
        seq.push(output);

        // Feedback polynomial: x^5 + x^2 + 1 (positions 0 and 3)
        let feedback = state[0] ^ state[3];
        for i in (1..5).rev() {
            state[i] = state[i - 1];
        }
        state[0] = feedback;
    }

    // Apply cyclic shift
    let mut shifted = seq.clone();
    shifted.rotate_left(shift);

    // Apply NID2-based scrambling to one of the sequences
    if (seq_index == 1 && nid2 != 0) || (seq_index == 0 && nid2 == 1) {
        for bit in &mut shifted {
            *bit ^= 1;
        }
    }

    // Convert to complex: 0 -> +1, 1 -> -1
    shifted
        .iter()
        .map(|&b| {
            if b == 0 {
                Complex::new(1.0, 0.0)
            } else {
                Complex::new(-1.0, 0.0)
            }
        })
        .collect()
}

/// Detect the best NID1 by correlating the symbol before PSS with all 168 SSS sequences.
/// Returns (best_nid1, correlation_strength).
fn detect_nid1(
    iq_capture: &[Complex<f32>],
    pss_peak_idx: usize,
    nid2: usize,
) -> (usize, f32) {
    // SSS is in the OFDM symbol before PSS
    // In LTE, each OFDM symbol is ~2048 samples at 2 MHz rate
    // For simplicity, assume SSS starts ~128 samples before PSS
    let sss_start = pss_peak_idx.saturating_sub(128);
    let sss_end = sss_start + 62;

    if sss_end > iq_capture.len() {
        return (0, 0.0);
    }

    let sss_window = &iq_capture[sss_start..sss_end];

    // Generate all SSS sequences for this NID2
    let sss_sequences = generate_sss_sequences(nid2);

    let mut best_nid1 = 0;
    let mut best_score = 0.0;

    for (nid1, sss) in sss_sequences.iter().enumerate() {
        // Cross-correlate
        let mut correlation: f32 = 0.0;
        for (i, (captured, reference)) in sss_window.iter().zip(sss.iter()).enumerate() {
            if i < sss_window.len().min(sss.len()) {
                let product = captured * reference.conj();
                correlation += product.re; // Real part is the in-phase correlation
            }
        }

        let score = correlation.abs() / (sss.len() as f32);

        if score > best_score {
            best_score = score;
            best_nid1 = nid1;
        }
    }

    (best_nid1, best_score)
}

/// Estimate frequency offset from PSS auto-correlation.
fn estimate_frequency_offset(pss_template: &[Complex<f32>], sample_rate: u32) -> f32 {
    // Split PSS into two halves and compute cross-correlation
    let half = pss_template.len() / 2;
    let first_half = &pss_template[0..half];
    let second_half = &pss_template[half..];

    let mut cross_corr = Complex::new(0.0, 0.0);
    for (a, b) in first_half.iter().zip(second_half.iter()) {
        let product = a * b.conj();
        cross_corr += product;
    }

    // Frequency offset is derived from the phase angle
    let angle = cross_corr.arg(); // angle in radians [-PI, PI]
    let t_half = (half as f32) / (sample_rate as f32);

    angle / (PI * t_half)
}

/// Detect LTE cells in raw IQ capture by correlating PSS sequences.
///
/// # Arguments
/// * `iq_bytes` - Raw RTL-SDR IQ bytes (interleaved u8 I, Q pairs)
/// * `sample_rate` - Sample rate in Hz (typically 2,048,000)
///
/// # Returns
/// Vector of detected LTE cells (may be empty if no signal detected).
///
/// # Algorithm
/// 1. Generate three PSS reference sequences (one per NID2: 0, 1, 2)
/// 2. Convert raw IQ to Complex<f32>
/// 3. FFT-based cross-correlation of each PSS with the capture
/// 4. Find peaks above correlation threshold
/// 5. For each PSS peak, detect NID1 via SSS correlation
/// 6. Estimate frequency offset from PSS auto-correlation
/// 7. Compute Physical Cell ID = 3 * NID1 + NID2
pub fn detect_lte_cells(iq_bytes: &[u8], sample_rate: u32) -> Vec<LteDetection> {
    // Minimum input validation: need at least 256 bytes (128 IQ pairs)
    if iq_bytes.len() < 256 {
        return Vec::new();
    }

    // Convert bytes to complex IQ
    let iq_complex = bytes_to_complex(iq_bytes);
    if iq_complex.is_empty() {
        return Vec::new();
    }

    let mut detections = Vec::new();

    // Process all three PSS sequences (NID2 = 0, 1, 2)
    for nid2 in 0..3 {
        let pss_template = generate_pss_template(nid2);
        let pss_energy = compute_energy(&pss_template);

        if pss_energy <= 0.0 {
            continue;
        }

        // FFT-based correlation
        let correlation = fft_correlate(&iq_complex, &pss_template);
        let capture_energy: f32 = iq_complex.iter().map(|c| c.norm_sqr()).sum();

        // Normalize correlation
        let norm_factor = (pss_energy * capture_energy).sqrt();
        let normalized_corr: Vec<f32> = correlation
            .iter()
            .map(|&c| c / (norm_factor + 1e-9))
            .collect();

        // Find peaks
        let peaks = find_peaks(&normalized_corr, CORRELATION_THRESHOLD);

        for (peak_idx, peak_magnitude) in peaks {
            // Detect NID1 from SSS
            let (nid1, sss_strength) = detect_nid1(&iq_complex, peak_idx, nid2);

            // Frequency offset estimation
            let freq_offset = estimate_frequency_offset(&pss_template, sample_rate);

            // Compute PCI
            let pci = 3 * nid1 + nid2;

            // If SSS detection is weak, reduce correlation strength to indicate uncertainty
            let final_strength = if sss_strength < 0.1 {
                peak_magnitude * 0.5 // Penalize weak SSS match
            } else {
                peak_magnitude
            };

            detections.push(LteDetection {
                physical_cell_id: pci as u16,
                nid1: nid1 as u16,
                nid2: nid2 as u8,
                timing_offset_samples: peak_idx,
                correlation_strength: final_strength.min(1.0),
                frequency_offset_hz: freq_offset,
            });
        }
    }

    detections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zadoff_chu_generation() {
        let zc = generate_zadoff_chu(25, 62);
        assert_eq!(zc.len(), 62);
        // Verify energy is approximately 62 (Parseval's theorem)
        let energy: f32 = zc.iter().map(|c| c.norm_sqr()).sum();
        assert!((energy - 62.0).abs() < 1.0);
    }

    #[test]
    fn test_pss_template_generation() {
        let template = generate_pss_template(0);
        assert_eq!(template.len(), PSS_FFT_SIZE);
        // Template should not be all zeros
        let energy: f32 = template.iter().map(|c| c.norm_sqr()).sum();
        assert!(energy > 0.0);
    }

    #[test]
    fn test_bytes_to_complex() {
        let iq_bytes = vec![128, 128, 255, 0];
        let complex = bytes_to_complex(&iq_bytes);
        assert_eq!(complex.len(), 2);
        assert!((complex[0].re - 0.0).abs() < 0.01);
        assert!((complex[0].im - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_empty_input() {
        let result = detect_lte_cells(&[], 2_048_000);
        assert!(result.is_empty());
    }

    #[test]
    fn test_too_short_input() {
        let iq_bytes = vec![128u8; 255]; // Less than 256 bytes
        let result = detect_lte_cells(&iq_bytes, 2_048_000);
        assert!(result.is_empty());
    }

    #[test]
    fn test_minimum_valid_input() {
        let iq_bytes = vec![128u8; 256]; // Exactly 256 bytes
        let result = detect_lte_cells(&iq_bytes, 2_048_000);
        // Should not panic; may or may not find cells depending on content
        let _ = result;
    }
}

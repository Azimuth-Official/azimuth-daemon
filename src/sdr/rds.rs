#![allow(dead_code)]

/// RDS (Radio Data System) information extracted from FM signal
#[derive(Debug, Clone)]
pub struct RdsData {
    /// Program Identification code (16-bit)
    pub pi_code: u16,
    /// North American callsign derived from PI code
    pub callsign: Option<String>,
    /// Program Type code (PTY, 0-31)
    pub program_type: Option<u8>,
    /// Program Service name (up to 8 characters)
    pub program_service: Option<String>,
}

// RDS constants
const RDS_SUBCARRIER_HZ: f32 = 57000.0;
const RDS_BITRATE: f32 = 1187.5;
const RDS_BLOCK_LEN: usize = 26;
const RDS_OFFSET_A: u32 = 0x0FC;
const RDS_OFFSET_B: u32 = 0x198;
const RDS_OFFSET_C: u32 = 0x168;
const RDS_OFFSET_CP: u32 = 0x350;
const RDS_OFFSET_D: u32 = 0x1B4;
const RDS_GEN_POLY: u32 = 0x5B9;

/// Extract RDS data from demodulated FM audio samples.
///
/// # Arguments
/// * `audio_samples` - Demodulated FM audio at ~204800 Hz
/// * `audio_rate` - Sample rate of audio (typically ~204800 Hz)
///
/// # Returns
/// `Some(RdsData)` with at least PI code on success, `None` if no valid RDS found
///
/// # Algorithm
/// 1. 57 kHz subcarrier recovery via mixing and bandpass filtering
/// 2. BPSK demodulation at 1187.5 bps
/// 3. Differential decoding
/// 4. Block synchronization using syndrome detection
/// 5. PI code extraction and callsign derivation
/// 6. Program Service name accumulation
pub fn extract_rds(audio_samples: &[f32], audio_rate: u32) -> Option<RdsData> {
    if audio_samples.is_empty() || audio_rate == 0 {
        return None;
    }

    // Step 1: 57 kHz subcarrier recovery via mixing
    let (i_57k, q_57k) = mix_subcarrier(audio_samples, audio_rate, RDS_SUBCARRIER_HZ);

    if i_57k.is_empty() || q_57k.is_empty() {
        return None;
    }

    // Step 2: Low-pass filter to isolate 57 kHz ±2.4 kHz
    let cutoff_bw = 2400.0;
    let ma_window = (audio_rate as f32 / cutoff_bw) as usize;

    let i_filtered = moving_average(&i_57k, ma_window);
    let q_filtered = moving_average(&q_57k, ma_window);

    if i_filtered.is_empty() || q_filtered.is_empty() {
        return None;
    }

    // Step 3: Decimate to RDS bitrate (1187.5 bps)
    let decimation = (audio_rate as f32 / RDS_BITRATE) as usize;
    let i_decimated = downsample(&i_filtered, decimation);
    let q_decimated = downsample(&q_filtered, decimation);

    if i_decimated.is_empty() || q_decimated.is_empty() {
        return None;
    }

    // Step 4: BPSK demodulation (integrate I component, take sign)
    let bits = bpsk_demodulate(&i_decimated);

    if bits.len() < 100 {
        return None;
    }

    // Step 5: Differential decoding
    let data_bits = differential_decode(&bits);

    if data_bits.is_empty() {
        return None;
    }

    // Step 6: Block synchronization
    let blocks = find_rds_blocks(&data_bits);

    if blocks.is_empty() {
        return None;
    }

    // Step 7: Extract PI code and other RDS information
    let mut pi_code = None;

    for block in &blocks {
        if block.offset == RDS_OFFSET_A && pi_code.is_none() {
            pi_code = Some(block.data as u16);
        }
    }

    // Require at least 1 PI code for validity (2 is ideal but 1 is acceptable)
    let pi_code = pi_code?;

    // Derive callsign from PI code
    let callsign = derive_callsign(pi_code);

    // Extract program type and service name from group 0A/0B
    let (program_type, program_service) = extract_program_info(&blocks);

    Some(RdsData {
        pi_code,
        callsign,
        program_type,
        program_service,
    })
}

/// Mix audio down from 57 kHz using cos/sin local oscillator
fn mix_subcarrier(audio: &[f32], sample_rate: u32, freq: f32) -> (Vec<f32>, Vec<f32>) {
    let mut i_out = Vec::with_capacity(audio.len());
    let mut q_out = Vec::with_capacity(audio.len());

    let sr = sample_rate as f32;
    let phase_increment = 2.0 * std::f32::consts::PI * freq / sr;

    for (n, &sample) in audio.iter().enumerate() {
        let phase = phase_increment * n as f32;
        i_out.push(sample * phase.cos());
        q_out.push(sample * phase.sin());
    }

    (i_out, q_out)
}

/// Apply moving average low-pass filter
fn moving_average(samples: &[f32], window: usize) -> Vec<f32> {
    if window < 2 || samples.is_empty() {
        return samples.to_vec();
    }

    let mut output = Vec::with_capacity(samples.len());
    let window = window.min(samples.len());

    for chunk_start in (0..samples.len()).step_by(window) {
        let chunk_end = (chunk_start + window).min(samples.len());
        let chunk = &samples[chunk_start..chunk_end];
        let avg = chunk.iter().sum::<f32>() / chunk.len() as f32;
        output.push(avg);
    }

    output
}

/// Downsample by factor (take every Nth sample)
fn downsample(samples: &[f32], factor: usize) -> Vec<f32> {
    if factor < 2 {
        return samples.to_vec();
    }

    samples
        .iter()
        .enumerate()
        .filter(|(i, _)| i % factor == 0)
        .map(|(_, &sample)| sample)
        .collect()
}

/// BPSK demodulation: extract bits from I-component (sign indicates bit)
fn bpsk_demodulate(i_samples: &[f32]) -> Vec<u8> {
    i_samples
        .iter()
        .map(|&i| if i >= 0.0 { 1 } else { 0 })
        .collect()
}

/// Differential decoding: XOR each bit with previous
fn differential_decode(bits: &[u8]) -> Vec<u8> {
    if bits.is_empty() {
        return Vec::new();
    }

    let mut decoded = vec![bits[0]];
    for i in 1..bits.len() {
        decoded.push(bits[i] ^ bits[i - 1]);
    }
    decoded
}

/// RDS block representation
#[derive(Debug, Clone)]
struct RdsBlock {
    data: u32,
    offset: u32,
}

/// Find RDS blocks using syndrome detection
fn find_rds_blocks(bitstream: &[u8]) -> Vec<RdsBlock> {
    const BLOCK_LEN: usize = RDS_BLOCK_LEN;
    let mut blocks = Vec::new();

    if bitstream.len() < BLOCK_LEN {
        return blocks;
    }

    for start in 0..=(bitstream.len() - BLOCK_LEN) {
        let block_bits = &bitstream[start..start + BLOCK_LEN];

        // Convert 26 bits to u32
        let data = bits_to_u32(&block_bits[..16]);
        let syndrome = compute_syndrome(bitstream, start, BLOCK_LEN);

        // Check if syndrome matches any offset
        let offsets = [RDS_OFFSET_A, RDS_OFFSET_B, RDS_OFFSET_C, RDS_OFFSET_CP, RDS_OFFSET_D];
        for &offset in &offsets {
            if syndrome == offset || syndrome == (syndrome ^ offset) {
                blocks.push(RdsBlock { data, offset });
                break;
            }
        }
    }

    blocks
}

/// Convert bit array to u32
fn bits_to_u32(bits: &[u8]) -> u32 {
    let mut value = 0u32;
    for (i, &bit) in bits.iter().enumerate() {
        if bit != 0 {
            value |= 1 << (bits.len() - 1 - i);
        }
    }
    value
}

/// Compute syndrome for RDS block (simplified CRC check)
fn compute_syndrome(bitstream: &[u8], start: usize, len: usize) -> u32 {
    let mut syndrome = 0u32;

    if start + len > bitstream.len() {
        return syndrome;
    }

    for &bit in &bitstream[start..start + len] {
        syndrome = (syndrome << 1) | (bit as u32);
        // Simple XOR-based syndrome (not full polynomial division, but adequate for demo)
        if (syndrome & 0x400) != 0 {
            syndrome ^= RDS_GEN_POLY;
        }
    }

    syndrome & 0x3FF // Return 10-bit syndrome
}

/// Derive North American callsign from PI code using NRSC-4-B algorithm
fn derive_callsign(pi: u16) -> Option<String> {
    let pi_val = pi as u32;

    // W-stations: PI codes typically >= 0x54A8 for eastern US
    // K-stations: PI codes typically >= 0x1000 and < 0x54A8 for western US
    let (prefix, base_val) = if (0x54A8..=0x9FFF).contains(&pi_val) {
        ('W', pi_val - 0x54A8)
    } else if (0x1000..0x54A8).contains(&pi_val) {
        ('K', pi_val - 0x1000)
    } else {
        return None;
    };

    // Decode 3 trailing letters using modular arithmetic
    let l4_idx = (base_val % 26) as u8 + b'A';
    let l3_idx = ((base_val / 26) % 26) as u8 + b'A';
    let l2_idx = ((base_val / (26 * 26)) % 26) as u8 + b'A';

    let mut callsign = String::with_capacity(4);
    callsign.push(prefix);
    callsign.push(l2_idx as char);
    callsign.push(l3_idx as char);
    callsign.push(l4_idx as char);

    Some(callsign)
}

/// Extract Program Type and Program Service name from RDS blocks
fn extract_program_info(blocks: &[RdsBlock]) -> (Option<u8>, Option<String>) {
    let mut program_type = None;
    let mut ps_chars = [None; 8]; // 8 characters max

    for block in blocks {
        // Group type 0A/0B is identified in block B (bits 12-15 contain group type)
        // For simplicity, we infer group 0A if we see blocks with offset pattern

        if block.offset == RDS_OFFSET_B {
            // Bits 12-15 of block B contain group type; bit 11 = group version
            // Bits 0-1 contain PS address
            let _group_info = (block.data >> 11) & 0x1F; // bits 11-15
            let pty = ((block.data >> 16) & 0x1F) as u8; // Rough PTY extraction

            if pty < 32 && program_type.is_none() {
                program_type = Some(pty);
            }

            let ps_addr = (block.data & 0x3) as usize;
            if ps_addr < 4 {
                let char_offset = ps_addr * 2;
                if char_offset + 1 < 8 {
                    // Extract two ASCII characters from the data
                    let ch1 = ((block.data >> 8) & 0xFF) as u8 as char;
                    let ch2 = (block.data & 0xFF) as u8 as char;

                    if ch1.is_ascii_graphic() || ch1 == ' ' {
                        ps_chars[char_offset] = Some(ch1);
                    }
                    if ch2.is_ascii_graphic() || ch2 == ' ' {
                        ps_chars[char_offset + 1] = Some(ch2);
                    }
                }
            }
        }
    }

    // Build PS string from accumulated characters
    let program_service = if ps_chars.iter().any(|c| c.is_some()) {
        let ps_str: String = ps_chars
            .iter()
            .map(|c| c.unwrap_or(' '))
            .collect::<String>()
            .trim_end()
            .to_string();
        if !ps_str.is_empty() {
            Some(ps_str)
        } else {
            None
        }
    } else {
        None
    };

    (program_type, program_service)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        assert!(extract_rds(&[], 204800).is_none());
    }

    #[test]
    fn test_zero_sample_rate() {
        let audio = vec![0.1; 1000];
        assert!(extract_rds(&audio, 0).is_none());
    }

    #[test]
    fn test_bits_to_u32() {
        let bits = vec![1, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let val = bits_to_u32(&bits);
        assert_eq!(val, 0xAA00);
    }

    #[test]
    fn test_moving_average() {
        let input = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let output = moving_average(&input, 2);
        assert!(!output.is_empty());
    }

    #[test]
    fn test_bpsk_demodulate() {
        let i_samples = vec![0.5, -0.3, 0.8, -0.1, 0.2];
        let bits = bpsk_demodulate(&i_samples);
        assert_eq!(bits, vec![1, 0, 1, 0, 1]);
    }

    #[test]
    fn test_differential_decode() {
        let bits = vec![1, 1, 0, 1, 0, 0];
        let decoded = differential_decode(&bits);
        assert_eq!(decoded.len(), 6);
        assert_eq!(decoded[0], 1);
    }

    #[test]
    fn test_derive_callsign_k_station() {
        // K-station example: PI in range 0x1000-0x54A8
        let pi = 0x2000u16;
        let callsign = derive_callsign(pi);
        assert!(callsign.is_some());
        let cs = callsign.unwrap();
        assert!(cs.starts_with('K'));
        assert_eq!(cs.len(), 4);
    }

    #[test]
    fn test_derive_callsign_w_station() {
        // W-station example: PI >= 0x54A8
        let pi = 0x6000u16;
        let callsign = derive_callsign(pi);
        assert!(callsign.is_some());
        let cs = callsign.unwrap();
        assert!(cs.starts_with('W'));
        assert_eq!(cs.len(), 4);
    }

    #[test]
    fn test_derive_callsign_invalid() {
        // PI outside valid ranges
        let pi = 0x0500u16;
        assert!(derive_callsign(pi).is_none());
    }
}

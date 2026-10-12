//! RED tests for G2b (DVB-T2 emitted only on a detected P1). Child module of `scanner` so it can reach private items; it adds no production logic.
//!

use super::*;
use crate::sdr::dtv::dvb::{FftMode, GuardInterval};
use crate::sdr::dtv::testgen::{dvb_capture, negative_capture, Negative};
use crate::sdr::dtv::testgen_t2::{t2_capture, T2Mode, T2Params};

fn dvb_scanner() -> Scanner {
    let mut c = ScanningConfig::default();
    c.dtv.standard = "DVB-T2-694".to_string();
    c.dtv.channel_mhz = 8;
    c.dtv.sweep_start = 470_000_000;
    c.dtv.sweep_end = 694_000_000;
    c.dtv.sweep_step = 8_000_000;
    let mut s = Scanner::new(&c, -30.0);
    s.dtv_plan = DtvPlan::from_config(&s.dtv_config);
    s
}

fn t2_mode(fft: FftMode, gi: GuardInterval, extended: bool) -> T2Params {
    T2Params::new(T2Mode { fft, gi, extended })
}

// --- G2b: detector-only emission ----------------------------------------------------------------

#[test]
fn dvbt2_is_emitted_for_a_detected_p1_including_shared_modes() {
    let s = dvb_scanner();
    // 8K 1/8 and 2K 1/32 also exist in DVB-T; only the P1 makes them DVB-T2.
    for (i, (fft, gi)) in [(FftMode::K8, GuardInterval::G1_8), (FftMode::K8, GuardInterval::G1_4), (FftMode::K2, GuardInterval::G1_32), (FftMode::K32, GuardInterval::G1_16)].into_iter().enumerate() {
        let iq = t2_capture(100 + i as u64, t2_mode(fft, gi, false), 12.0, 40_000.0, false);
        let r = s.scan_dvb_capture(&iq, 2_048_000, 602_000_000, -40.0);
        assert_eq!(r.len(), 1, "{} {}", fft.label(), gi.label());
        assert_eq!(r[0].signal_type, "dtv_dvbt2", "{} {}", fft.label(), gi.label());
        assert_eq!(r[0].source_id.as_deref(), Some("dvb_ch37"));
        let raw = r[0].raw_data.as_ref().unwrap();
        assert_eq!(raw["fft_mode"], fft.label());
        assert_eq!(raw["guard_interval"], gi.label());
    }
}

#[test]
fn cp_only_dvbt_capture_is_dvbt_never_dvbt2() {
    let s = dvb_scanner();
    for (i, (fft, gi)) in [(FftMode::K8, GuardInterval::G1_8), (FftMode::K2, GuardInterval::G1_32), (FftMode::K8, GuardInterval::G1_4)].into_iter().enumerate() {
        let iq = dvb_capture(200 + i as u64, fft, gi, 8, 12.0, 60_000.0, false);
        let r = s.scan_dvb_capture(&iq, 2_048_000, 602_000_000, -40.0);
        assert!(r.iter().all(|x| x.signal_type != "dtv_dvbt2"), "{} {}: {:?}", fft.label(), gi.label(), r.iter().map(|x| &x.signal_type).collect::<Vec<_>>());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].signal_type, "dtv_dvbt");
    }
}

/// CONTRADICTS the existing `scanner::dsp_plan_tests::dvb_capture_becomes_detection_only_observation`
/// (asserts `dtv_dvbt2` for a CP-only 32K 19/128 capture) and `dvb::tests::labeling_follows_contract_c1`.
/// The lead decides how those two are reconciled; this test encodes the G2b requirement.
#[test]
fn gi_or_fft_inference_without_a_p1_never_emits_dvbt2() {
    let s = dvb_scanner();
    // T2-only FFT sizes / guard intervals, CP-only (no P1): inference alone is not a T2 detection.
    for (i, (fft, gi)) in [
        (FftMode::K32, GuardInterval::G19_128),
        (FftMode::K32, GuardInterval::G1_8),
        (FftMode::K16, GuardInterval::G1_16),
        (FftMode::K4, GuardInterval::G1_32),
        (FftMode::K1, GuardInterval::G1_16),
        (FftMode::K8, GuardInterval::G19_128),
        (FftMode::K8, GuardInterval::G1_128),
    ]
    .into_iter()
    .enumerate()
    {
        let iq = dvb_capture(300 + i as u64, fft, gi, 8, 12.0, -120_000.0, false);
        let r = s.scan_dvb_capture(&iq, 2_048_000, 682_000_000, -40.0);
        assert!(r.iter().all(|x| x.signal_type != "dtv_dvbt2"), "{} {} emitted dtv_dvbt2 without a P1", fft.label(), gi.label());
    }
}

#[test]
fn t2_frames_whose_p1_is_missing_never_emit_dvbt2() {
    let s = dvb_scanner();
    for (i, (fft, gi)) in [(FftMode::K8, GuardInterval::G1_8), (FftMode::K32, GuardInterval::G19_128), (FftMode::K16, GuardInterval::G1_16)].into_iter().enumerate() {
        let mut p = t2_mode(fft, gi, false);
        p.with_p1 = false;
        let iq = t2_capture(400 + i as u64, p, 12.0, 0.0, false);
        let r = s.scan_dvb_capture(&iq, 2_048_000, 602_000_000, -40.0);
        assert!(r.iter().all(|x| x.signal_type != "dtv_dvbt2"), "{} {}", fft.label(), gi.label());
    }
}

#[test]
fn power_alone_never_emits_dvbt2_or_any_dvb_observation() {
    // Strong in-band power with no OFDM structure (high reported dBm) must emit nothing.
    let s = dvb_scanner();
    for kind in [Negative::Awgn, Negative::Cw, Negative::Fm, Negative::Atsc] {
        let iq = negative_capture(77, kind);
        assert!(s.scan_dvb_capture(&iq, 2_048_000, 602_000_000, -5.0).is_empty(), "{kind:?}");
    }
}

#[test]
fn production_scanner_code_never_names_dtv_dvbt2_itself() {
    // The only producer of the `dtv_dvbt2` label is the detector (`dvb::detect_dvb`); the scanner passes its
    // `signal_type` through. Scanning the production half of scanner.rs (everything before the first test
    // module) for the literal catches a power-only or inference shortcut added at the scanner layer.
    let src = include_str!("../scanner.rs");
    let prod = &src[..src.find("#[cfg(test)]\nmod ").expect("first test module")];
    assert!(prod.len() > 10_000, "control: production half must be non-trivial");
    assert!(!prod.contains("dtv_dvbt2"), "scanner production code names dtv_dvbt2");
}

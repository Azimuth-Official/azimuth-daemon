//! Real-capture negative control for the DVB-T/T2 detector.
//!
//! Tunes a list of centre frequencies, takes N native captures each (2.048 MS/s,
//! 131072 samples, default gain) with the daemon's own USB code, runs the DVB
//! detector for each channel bandwidth (8/7/6 MHz) on every capture and prints one
//! line per capture. Pass condition on non-DVB spectrum (ATSC, LTE, FM, empty
//! channels): zero DVB labels. Writes nothing to disk, never opens the GPS, and
//! `--help` does not touch the device.
//!
//!   cargo run --release --example dvb_probe -- --help

#![allow(dead_code)] // the included detector module has helpers the probe does not call

use std::time::Instant;

use rtl_sdr_rs::error::RtlsdrError;
use rtl_sdr_rs::RtlSdr;

// The daemon's own USB code, included verbatim (as tune_stress does).
#[path = "../src/sdr/driver_state.rs"]
mod driver_state;
#[path = "../src/sdr/stall.rs"]
mod stall;
#[path = "../src/sdr/holders.rs"]
mod holders;
#[path = "../src/sdr/dongle.rs"]
mod dongle;
// The detector itself (self-contained, no daemon state).
#[path = "../src/sdr/dtv/dvb.rs"]
mod dvb;
use dongle::{Gain, InitParams};

const SAMPLE_RATE: u32 = 2_048_000;
const CAPTURE_BYTES: usize = 262_144;

fn usage(code: i32) -> ! {
    let text = "usage: dvb_probe (--freqs HZ[,HZ...] | --start HZ --end HZ --step HZ) [--captures N]\n\
         \x20 --freqs LIST     centre frequencies in Hz, comma separated\n\
         \x20 --start/--end/--step HZ   a regular list instead (inclusive)\n\
         \x20 --captures N     native captures per frequency (default 3)\n\
         \x20 --bw LIST        bandwidths to test, default 8,7,6\n\
         \x20 --help           this text (does not open the device)\n\
         One line per capture and bandwidth:\n\
         \x20 f=<Hz> cap=<n> bw=<MHz> best=<fft>/<gi> rho=<r> contrast=<c> margin=<m> cfo_hz=<Hz> decision=<dtv_dvbt|dtv_dvbt2|none>\n\
         Final lines: MAX (largest rho and contrast over all captures, and over decisions) and\n\
         SUMMARY (captures, labels). Exit code 0 = no DVB label, 4 = at least one DVB label.\n\
         Output goes to stdout only; nothing is written to disk.";
    if code == 0 {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
    std::process::exit(code)
}

struct Opts {
    freqs: Vec<u32>,
    captures: u32,
    bws: Vec<u32>,
}

fn parse() -> Opts {
    let mut freqs: Vec<u32> = Vec::new();
    let (mut start, mut end, mut step) = (None, None, None);
    let mut captures = 3u32;
    let mut bws = vec![8u32, 7, 6];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || -> String { args.next().unwrap_or_else(|| usage(2)) };
        match a.as_str() {
            "--help" | "-h" => usage(0),
            "--freqs" => freqs = val().split(',').map(|v| v.trim().parse().unwrap_or_else(|_| usage(2))).collect(),
            "--start" => start = Some(val().parse::<u32>().unwrap_or_else(|_| usage(2))),
            "--end" => end = Some(val().parse::<u32>().unwrap_or_else(|_| usage(2))),
            "--step" => step = Some(val().parse::<u32>().unwrap_or_else(|_| usage(2))),
            "--captures" => captures = val().parse().unwrap_or_else(|_| usage(2)),
            "--bw" => bws = val().split(',').map(|v| v.trim().parse().unwrap_or_else(|_| usage(2))).collect(),
            _ => usage(2),
        }
    }
    if let (Some(s), Some(e), Some(st)) = (start, end, step) {
        if st == 0 {
            usage(2);
        }
        let mut f = s;
        while f <= e {
            freqs.push(f);
            f += st;
        }
    }
    if freqs.is_empty() || captures == 0 || bws.iter().any(|b| !matches!(b, 6..=8)) {
        usage(2);
    }
    Opts { freqs, captures, bws }
}

fn main() {
    let o = parse(); // --help / bad arguments exit here, before any device access
    let params = InitParams { index: 0, sample_rate: SAMPLE_RATE, gain: Gain::Auto, freq_correction_ppm: 0 };
    let mut sdr: Option<RtlSdr> = match dongle::open_with_ladder(&params) {
        Ok(s) => Some(s),
        Err(e) => {
            println!("OPEN_FAILED {e}");
            std::process::exit(3);
        }
    };
    let t0 = Instant::now();
    let mut buf = vec![0u8; CAPTURE_BYTES];
    let (mut captures, mut labels, mut failures) = (0u32, 0u32, 0u32);
    let (mut max_rho, mut max_contrast) = (0.0f64, 0.0f64);
    let (mut max_rho_label, mut max_contrast_label) = (0.0f64, 0.0f64);
    for &f in &o.freqs {
        if let Err(e) = dongle::tune_with_ladder(&mut sdr, &params, f) {
            println!("f={f} TUNE_FAILED {e}");
            failures += 1;
            continue;
        }
        for cap in 0..o.captures {
            let r: Result<usize, RtlsdrError> = match sdr.as_mut() {
                Some(d) => {
                    let _ = d.reset_buffer();
                    d.read_sync(&mut buf)
                }
                None => Err(RtlsdrError::RtlsdrErr("no device".to_string())),
            };
            match r {
                Ok(n) if n >= CAPTURE_BYTES => {}
                other => {
                    println!("f={f} cap={cap} CAPTURE_FAILED {other:?}");
                    failures += 1;
                    continue;
                }
            }
            captures += 1;
            let x = dvb::iq_u8_to_complex(&buf);
            for &bw in &o.bws {
                let scores = dvb::analyze(&x, f64::from(SAMPLE_RATE), bw);
                let Some(best) = scores.iter().copied().max_by(|a, b| a.rho.partial_cmp(&b.rho).unwrap_or(std::cmp::Ordering::Equal)) else {
                    continue;
                };
                let second = scores
                    .iter()
                    .filter(|s| s.mode != best.mode || s.gi != best.gi)
                    .map(|s| s.rho)
                    .fold(0.0f64, f64::max);
                let margin = if second > 0.0 { best.rho / second } else { f64::INFINITY };
                let det = dvb::decide(&scores, bw);
                let decision = det.as_ref().map(|d| d.signal_type).unwrap_or("none");
                max_rho = max_rho.max(best.rho);
                max_contrast = max_contrast.max(best.contrast);
                if det.is_some() {
                    labels += 1;
                    max_rho_label = max_rho_label.max(best.rho);
                    max_contrast_label = max_contrast_label.max(best.contrast);
                }
                println!(
                    "f={f} cap={cap} bw={bw} best={}/{} rho={:.3} contrast={:.1} margin={:.2} cfo_hz={:.0} decision={decision}",
                    best.mode.label(),
                    best.gi.label(),
                    best.rho,
                    best.contrast,
                    margin,
                    best.cfo_hz
                );
            }
        }
    }
    println!("MAX rho={max_rho:.3} contrast={max_contrast:.1} (over all captures); among DVB labels: rho={max_rho_label:.3} contrast={max_contrast_label:.1}");
    println!(
        "SUMMARY frequencies={} captures={captures} labels={labels} failures={failures} thresholds: rho>={} contrast>={} margin>={} elapsed_ms={}",
        o.freqs.len(),
        dvb::RHO_MIN,
        dvb::CONTRAST_MIN,
        dvb::MARGIN_MIN,
        t0.elapsed().as_millis()
    );
    std::process::exit(if labels == 0 { 0 } else { 4 });
}

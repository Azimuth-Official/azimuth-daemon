//! Hardware repro for "demod_write_reg failed: Pipe error" during FM sweeps.
//!
//! Tunes an RTL-SDR across a frequency range exactly like the scanner does
//! (set_center_freq, optional settle, optional capture) and prints one line per
//! step, so the failing step, the elapsed time and the USB error are on record.
//! Opens device 0 only, never writes anything but the standard tuning registers,
//! and exits after the first failure unless `--continue` is given.
//!
//! cargo run --example tune_stress -- --help

use std::time::{Duration, Instant};

use rtl_sdr_rs::error::RtlsdrError;
use rtl_sdr_rs::RtlSdr;

// The daemon's own USB code (open + init ladder, stall-tolerant tune, reset),
// included verbatim so a hardware pass here proves the daemon's code path.
#[path = "../src/sdr/driver_state.rs"]
mod driver_state;
#[path = "../src/sdr/stall.rs"]
mod stall;
#[path = "../src/sdr/dongle.rs"]
mod dongle;
use dongle::{Gain, InitParams};
use std::sync::atomic::Ordering;

/// Prints `log` records (rtl-sdr-rs reports USB errors through them).
struct StderrLog;
impl log::Log for StderrLog {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        eprintln!("LOG {} {}: {}", r.level(), r.target(), r.args());
    }
    fn flush(&self) {}
}

struct Opts {
    start: u32,
    end: u32,
    step: u32,
    sample_rate: u32,
    capture_bytes: usize,
    settle_ms: u64,
    pause_ms: u64,
    passes: u32,
    reset_buffer: bool,
    reset_after: bool,
    retry_tune: u32,
    tolerant: bool,
    reset_on_open: bool,
    keep_going: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: tune_stress [--start HZ] [--end HZ] [--step HZ] [--rate HZ]\n\
         \x20 [--capture-bytes N]   0 = tune only, no bulk read (default 262144)\n\
         \x20 [--settle-ms N]       sleep after each tune (default 0)\n\
         \x20 [--pause-ms N]        sleep after each step (default 0)\n\
         \x20 [--passes N]          sweeps to run (default 3)\n\
         \x20 [--no-reset-buffer]   skip reset_buffer() before each read\n\
         \x20 [--reset-after-capture]  FIX CANDIDATE 1: reset_buffer() right after each read\n\
         \x20                         (stops the endpoint FIFO before the next control write)\n\
         \x20 [--retry-tune N]      FIX CANDIDATE 2: retry a failed tune up to N times\n\
         \x20 [--tolerant]         THE DAEMON FIX: re-issue a stalled tune (3x, 20/40/60 ms); if it\n\
         \x20                         still fails, USB-reset, reopen and carry on (never abort)\n\
         \x20 [--reset-on-open]     like librtlsdr: if open stalls, USB-reset the dongle and reopen\n\
         \x20 [--continue]          keep going after a failure"
    );
    std::process::exit(2)
}

fn parse() -> Opts {
    let mut o = Opts {
        start: 87_500_000,
        end: 108_000_000,
        step: 200_000,
        sample_rate: 2_048_000,
        capture_bytes: 262_144,
        settle_ms: 0,
        pause_ms: 0,
        passes: 3,
        reset_buffer: true,
        reset_after: false,
        retry_tune: 0,
        tolerant: false,
        reset_on_open: false,
        keep_going: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut num = || -> u64 { args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()) };
        match a.as_str() {
            "--start" => o.start = num() as u32,
            "--end" => o.end = num() as u32,
            "--step" => o.step = num() as u32,
            "--rate" => o.sample_rate = num() as u32,
            "--capture-bytes" => o.capture_bytes = num() as usize,
            "--settle-ms" => o.settle_ms = num(),
            "--pause-ms" => o.pause_ms = num(),
            "--passes" => o.passes = num() as u32,
            "--no-reset-buffer" => o.reset_buffer = false,
            "--reset-after-capture" => o.reset_after = true,
            "--retry-tune" => o.retry_tune = num() as u32,
            "--tolerant" => o.tolerant = true,
            "--reset-on-open" => o.reset_on_open = true,
            "--continue" => o.keep_going = true,
            _ => usage(),
        }
    }
    o
}

pub fn main() {
    let _ = log::set_logger(&StderrLog);
    log::set_max_level(log::LevelFilter::Info);
    let o = parse();
    let params = InitParams {
        index: 0,
        sample_rate: o.sample_rate,
        gain: Gain::Auto,
        freq_correction_ppm: 0,
    };
    let tolerant = o.tolerant;

    // Baseline: one plain open + init, no ladder. Tolerant: the daemon's ladder.
    let open = |tolerant: bool| -> Result<RtlSdr, RtlsdrError> {
        if tolerant || o.reset_on_open {
            dongle::open_with_ladder(&params)
        } else {
            let mut d = dongle::open_catching(params.index)?;
            d.set_sample_rate(params.sample_rate)?;
            d.set_freq_correction(0)?;
            d.set_tuner_gain(rtl_sdr_rs::TunerGain::Auto)?;
            d.reset_buffer()?;
            Ok(d)
        }
    };
    let mut sdr: Option<RtlSdr> = match open(tolerant) {
        Ok(s) => Some(s),
        Err(e) => {
            println!("OPEN_FAILED {e}");
            std::process::exit(3);
        }
    };
    if let Some(d) = sdr.as_ref() {
        println!("tuner={:?}", d.get_tuner_id().map(str::to_string));
    }

    let t0 = Instant::now();
    let (mut steps, mut failures, mut reopens) = (0u32, 0u32, 0u32);
    let mut buf = vec![0u8; o.capture_bytes];
    let counter = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
    for pass in 0..o.passes {
        let mut f = o.start;
        while f <= o.end {
            steps += 1;
            let t = Instant::now();
            let stalls_before = counter(&dongle::TUNE_STALL_RETRIES_TOTAL);

            // Device lost earlier (reopen failed)? Keep trying: never abort the sweep.
            if sdr.is_none() && tolerant {
                reopens += 1;
                sdr = dongle::reset_and_reopen(&params).ok();
            }

            let (tune, capture) = match sdr.as_mut() {
                None => (Err("no device".to_string()), Err("no device".to_string())),
                Some(d) => {
                    let mut tune = if tolerant {
                        dongle::tune(d, f).map_err(|e| e.to_string())
                    } else {
                        d.set_center_freq(f).map_err(|e| e.to_string())
                    };
                    if tolerant && tune.is_err() {
                        // Persistent stall: the daemon's recovery, USB reset first.
                        println!("STALL_PERSISTENT f={f}: reset and reopen");
                        reopens += 1;
                        drop(sdr.take());
                        sdr = dongle::reset_and_reopen(&params).ok();
                        tune = match sdr.as_mut() {
                            Some(d) => dongle::tune(d, f).map_err(|e| e.to_string()),
                            None => Err("reopen failed".to_string()),
                        };
                    }
                    if o.settle_ms > 0 {
                        std::thread::sleep(Duration::from_millis(o.settle_ms));
                    }
                    let mut capture = Ok(0usize);
                    if tune.is_ok() && o.capture_bytes > 0 {
                        if let Some(d) = sdr.as_mut() {
                            if o.reset_buffer {
                                let _ = d.reset_buffer();
                            }
                            capture = d.read_sync(&mut buf).map_err(|e| e.to_string());
                            if o.reset_after {
                                let _ = d.reset_buffer();
                            }
                        }
                    }
                    (tune, capture)
                }
            };
            let ok = tune.is_ok() && capture.is_ok();
            println!(
                "pass={pass} step={steps} f={f} elapsed_ms={} step_ms={} stall_retries={} tune={} capture={}",
                t0.elapsed().as_millis(),
                t.elapsed().as_millis(),
                counter(&dongle::TUNE_STALL_RETRIES_TOTAL) - stalls_before,
                tune.as_ref().map(|_| "ok".to_string()).unwrap_or_else(|e| format!("ERR({e})")),
                capture.as_ref().map(|n| format!("ok({n})")).unwrap_or_else(|e| format!("ERR({e})")),
            );
            if !ok {
                failures += 1;
                if !o.keep_going && !tolerant {
                    println!("FIRST_FAILURE step={steps} f={f} after_ms={}", t0.elapsed().as_millis());
                    println!("SUMMARY steps={steps} failures={failures}");
                    std::process::exit(1);
                }
            }
            if o.pause_ms > 0 {
                std::thread::sleep(Duration::from_millis(o.pause_ms));
            }
            f += o.step;
        }
    }
    println!(
        "SUMMARY steps={steps} failures={failures} tune_stall_retries={} tune_stalls_recovered={} init_stall_resets={} init_failures={} reopens={reopens} device_error={:?} elapsed_ms={}",
        counter(&dongle::TUNE_STALL_RETRIES_TOTAL),
        counter(&dongle::TUNE_STALLS_RECOVERED),
        counter(&dongle::INIT_STALL_RESETS),
        counter(&dongle::INIT_FAILURES),
        dongle::device_error(),
        t0.elapsed().as_millis()
    );
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

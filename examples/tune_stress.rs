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

use rtl_sdr_rs::{RtlSdr, TunerGain};

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

    let mut sdr = match RtlSdr::open_with_index(0) {
        Ok(s) => s,
        Err(e) => {
            println!("OPEN_FAILED {e}");
            std::process::exit(3);
        }
    };
    println!("tuner={:?}", sdr.get_tuner_id().map(str::to_string));
    for k in ["AZ_RTL_PLL_SLEEP_MS", "AZ_RTL_I2C_EXACT", "AZ_RTL_CHUNK_FIX"] {
        println!("{k}={}", std::env::var(k).unwrap_or_else(|_| "(unset)".to_string()));
    }
    sdr.set_sample_rate(o.sample_rate).expect("set_sample_rate");
    sdr.set_tuner_gain(TunerGain::Auto).expect("set_tuner_gain");
    let _ = sdr.reset_buffer();

    let t0 = Instant::now();
    let (mut steps, mut failures) = (0u32, 0u32);
    let mut buf = vec![0u8; o.capture_bytes];
    for pass in 0..o.passes {
        let mut f = o.start;
        while f <= o.end {
            steps += 1;
            let t = Instant::now();
            let mut tune = sdr.set_center_freq(f);
            for _ in 0..o.retry_tune {
                if tune.is_ok() {
                    break;
                }
                println!("RETRY_TUNE f={f}");
                std::thread::sleep(Duration::from_millis(50));
                tune = sdr.set_center_freq(f);
            }
            if o.settle_ms > 0 {
                std::thread::sleep(Duration::from_millis(o.settle_ms));
            }
            let mut capture = Ok(0usize);
            if tune.is_ok() && o.capture_bytes > 0 {
                if o.reset_buffer {
                    let _ = sdr.reset_buffer();
                }
                capture = sdr.read_sync(&mut buf);
                if o.reset_after {
                    let _ = sdr.reset_buffer();
                }
            }
            let ok = tune.is_ok() && capture.is_ok();
            println!(
                "pass={pass} step={steps} f={f} elapsed_ms={} step_ms={} tune={} capture={}",
                t0.elapsed().as_millis(),
                t.elapsed().as_millis(),
                tune.as_ref().map(|_| "ok".to_string()).unwrap_or_else(|e| format!("ERR({e})")),
                capture.as_ref().map(|n| format!("ok({n})")).unwrap_or_else(|e| format!("ERR({e})")),
            );
            if !ok {
                failures += 1;
                if !o.keep_going {
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
    println!("SUMMARY steps={steps} failures={failures} elapsed_ms={}", t0.elapsed().as_millis());
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

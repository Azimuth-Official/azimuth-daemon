//! End-to-end check that a record from the real `log` facade (how rtl-sdr-rs
//! reports "[R82xx] PLL not locked!") reaches the PLL counter through the
//! tracing-log bridge and is never printed.
//!
//! This installs the process-global subscriber, so it lives in its own test
//! binary (its own process) where nothing else can have installed one first.

// The file under test, exactly as the daemon compiles it.
#[path = "../src/tuner_log.rs"]
mod tuner_log;

/// The one item of the daemon's `sdr` module that `tuner_log` touches.
mod sdr {
    pub static PLL_LOCK_FAILS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
}

use std::sync::{Arc, Mutex};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, reload, EnvFilter};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

#[test]
fn real_log_crate_record_reaches_the_counter_through_the_bridge() {
    let (out, file) = (Capture::default(), Capture::default());
    let filter = EnvFilter::try_new(tuner_log::with_tuner_directive("warn,azimuth_daemon=info")).unwrap();
    let (filter_layer, _handle) = reload::Layer::new(filter);
    assert!(
        tuner_log::assemble(filter_layer, out.clone(), file.clone()).try_init().is_ok(),
        "this binary owns the global subscriber"
    );
    let count = || sdr::PLL_LOCK_FAILS.load(std::sync::atomic::Ordering::Relaxed);
    let before = count();
    log::info!(target: "rtl_sdr_rs::tuners::r82xx", "[R82xx] PLL not locked!");
    log::info!(target: "rtl_sdr_rs::tuners::r82xx", "freq (kHz): 98500");
    assert_eq!(count() - before, 1, "only the PLL record is counted");
    for c in [&out, &file] {
        let text = String::from_utf8_lossy(&c.0.lock().unwrap()).to_string();
        assert!(!text.contains("PLL") && !text.contains("freq (kHz)"), "tuner noise printed: {text}");
    }
}

//! Logging setup with a runtime-switchable level and an on-disk rolling file.
//!
//! Replaces a bare `tracing_subscriber::fmt::init()`, which wrote to stdout only
//! with no `EnvFilter` — so a user troubleshooting a node had nothing to send in
//! and no way to raise verbosity without a rebuild.
//!
//! ## Why the filters are shaped the way they are
//!
//! Both filters start at `warn` and raise ONLY `azimuth_daemon`. That is a
//! security control, not a tidiness preference: `hyper`, `reqwest` and `rustls`
//! log request headers at debug/trace, and this daemon sends
//! `Authorization: Bearer <api key>` on every submit. Pinning third-party
//! targets at `warn` makes that leak unreachable even at
//! `azimuth_daemon=trace`, and `parse_level_directive` rejects any request that
//! would raise a non-`azimuth_daemon` target above `info`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Filter, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, reload, EnvFilter, Layer, Registry};

/// Quiet baseline: this crate at info, everything else at warn.
pub const BASE_FILTER: &str = "warn,azimuth_daemon=info";
/// Verbose troubleshooting: this crate at debug, everything else still warn.
pub const DEBUG_FILTER: &str = "warn,azimuth_daemon=debug";

/// Target of the rtl-sdr-rs tuner driver, whose `log` records carry the
/// "[R82xx] PLL not locked!" line (and chatty per-tune frequency math).
pub const TUNER_LOG_TARGET: &str = "rtl_sdr_rs::tuners::r82xx";
/// Lets those records reach the (silent) counting layer. The output layers drop
/// them again (`QuietTuner`), so nothing from this target is ever printed.
const TUNER_DIRECTIVE: &str = "rtl_sdr_rs::tuners::r82xx=info";

fn with_tuner_directive(directive: &str) -> String {
    format!("{directive},{TUNER_DIRECTIVE}")
}

/// What we need from an event to recognise tuner-driver records. Records that
/// came through the `log` bridge carry the original target in `log.target`.
#[derive(Default)]
struct EventFacts {
    message: String,
    log_target: Option<String>,
}

impl Visit for EventFacts {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "log.target" => self.log_target = Some(value.to_string()),
            "message" => self.message = value.to_string(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

fn facts(event: &Event<'_>) -> Option<EventFacts> {
    let mut f = EventFacts::default();
    event.record(&mut f);
    let target = f.log_target.as_deref().unwrap_or_else(|| event.metadata().target());
    target.starts_with(TUNER_LOG_TARGET).then_some(f)
}

/// Counts "PLL not locked" records and writes nothing.
struct PllCounter;

impl<S: Subscriber> Layer<S> for PllCounter {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if let Some(f) = facts(event) {
            if f.message.contains("PLL not locked") {
                crate::sdr::PLL_LOCK_FAILS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// Output filter: drops tuner-driver records so they never reach stdout or the
/// log file at the default level (they are only counted).
struct QuietTuner;

impl<S: Subscriber> Filter<S> for QuietTuner {
    fn enabled(&self, _meta: &tracing::Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &Event<'_>, _cx: &Context<'_, S>) -> bool {
        facts(event).is_none()
    }
}

/// Total bytes of daemon logs to keep. tracing-appender rotates by time and
/// caps file COUNT, but has no size-based rotation, so a chatty debug session
/// is bounded here instead.
pub const LOG_DIR_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// How long a debug session lasts before reverting on its own. This is the most
/// effective disk control available: it stops "turned debug on in March, disk
/// full in June" without relying on the user remembering.
pub const DEBUG_AUTO_REVERT: Duration = Duration::from_secs(60 * 60);

type ReloadHandle = reload::Handle<EnvFilter, Registry>;

/// Runtime control surface handed to the status server.
pub struct LogHandle {
    reload: ReloadHandle,
    dir: PathBuf,
    _guard: tracing_appender::non_blocking::WorkerGuard,
}

impl LogHandle {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A handle that is not wired to a global subscriber, for unit tests.
    #[cfg(test)]
    pub fn for_tests(dir: PathBuf) -> Arc<Self> {
        let (_layer, reload): (reload::Layer<EnvFilter, Registry>, _) =
            reload::Layer::new(EnvFilter::new(BASE_FILTER));
        let (_writer, guard) = tracing_appender::non_blocking(std::io::sink());
        Arc::new(Self { reload, dir, _guard: guard })
    }

    /// Apply a raw EnvFilter directive string. Returns the applied value.
    pub fn set_filter(&self, directive: &str) -> Result<String, String> {
        let filter = EnvFilter::try_new(with_tuner_directive(directive)).map_err(|e| e.to_string())?;
        self.reload
            .reload(filter)
            .map_err(|e| format!("filter reload failed: {e}"))?;
        Ok(directive.to_string())
    }

    pub fn set_debug(&self, on: bool) -> Result<String, String> {
        self.set_filter(if on { DEBUG_FILTER } else { BASE_FILTER })
    }

    /// Files currently on disk, newest first: (name, bytes).
    pub fn files(&self) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("azimuth-daemon") {
                    return None;
                }
                let len = e.metadata().ok()?.len();
                Some((name, len))
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        out
    }
}

/// Translate a friendly level into a full directive, refusing anything that
/// would raise a third-party target. This is the check that keeps dependency
/// logging (and therefore auth headers) out of reach.
pub fn parse_level_directive(level: Option<&str>, raw_filter: Option<&str>) -> Result<String, String> {
    if let Some(raw) = raw_filter {
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (target, lvl) = match part.split_once('=') {
                Some((t, l)) => (t.trim(), l.trim()),
                // A bare level applies globally — that would raise everything.
                None => ("", part),
            };
            let too_loud = matches!(lvl.to_ascii_lowercase().as_str(), "debug" | "trace");
            let is_ours = target.starts_with("azimuth_daemon");
            if too_loud && !is_ours {
                return Err(format!(
                    "refusing directive '{part}': raising a non-azimuth_daemon target to {lvl} \
                     would expose dependency logging (request headers include the API key)"
                ));
            }
        }
        return Ok(raw.to_string());
    }

    match level.unwrap_or("info").to_ascii_lowercase().as_str() {
        "trace" => Ok("warn,azimuth_daemon=trace".to_string()),
        "debug" => Ok(DEBUG_FILTER.to_string()),
        "info" => Ok(BASE_FILTER.to_string()),
        "warn" => Ok("warn".to_string()),
        "error" => Ok("error".to_string()),
        other => Err(format!("unknown level '{other}'")),
    }
}

/// Where logs live. Derived from the config path the sidecar is always launched
/// with, so desktop/systemd/Docker each land somewhere sensible without special
/// casing. `AZIMUTH_LOG_DIR` overrides (useful for a Docker volume).
pub fn log_dir(config_path: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("AZIMUTH_LOG_DIR") {
        return PathBuf::from(dir);
    }
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

/// Delete oldest files until the directory is under `max_bytes`.
pub fn enforce_size_cap(dir: &Path, max_bytes: u64) {
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("azimuth-daemon") {
                return None;
            }
            let md = e.metadata().ok()?;
            Some((e.path(), md.len(), md.modified().ok()?))
        })
        .collect();

    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max_bytes {
        return;
    }
    files.sort_by_key(|f| f.2); // oldest first
    for (path, len, _) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// The subscriber stack: runtime filter, silent PLL counter, then the stdout and
/// file outputs (both blind to tuner-driver records).
fn assemble<W1, W2>(
    filter_layer: reload::Layer<EnvFilter, Registry>,
    stdout: W1,
    file: W2,
) -> impl Subscriber + Send + Sync
where
    W1: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
    W2: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    Registry::default()
        .with(filter_layer)
        .with(PllCounter)
        .with(fmt::layer().with_writer(stdout).with_target(true).with_filter(QuietTuner))
        .with(fmt::layer().with_writer(file).with_ansi(false).with_filter(QuietTuner))
}

/// Initialise logging. Never fails the process: if the log directory cannot be
/// created (read-only container), it warns once and runs stdout-only, because
/// `docker logs` already covers that case.
pub fn init(config_path: &Path, initial: Option<&str>) -> Arc<LogHandle> {
    let dir = log_dir(config_path);
    let dir_ok = std::fs::create_dir_all(&dir).is_ok();

    let directive = std::env::var("RUST_LOG")
        .ok()
        .or_else(|| initial.map(|s| s.to_string()))
        .unwrap_or_else(|| BASE_FILTER.to_string());
    let filter = EnvFilter::try_new(with_tuner_directive(&directive))
        .unwrap_or_else(|_| EnvFilter::new(with_tuner_directive(BASE_FILTER)));
    let (filter_layer, reload_handle) = reload::Layer::new(filter);

    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("azimuth-daemon")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir);

    // The guard must outlive main or buffered lines are silently dropped, so it
    // is owned by the returned handle rather than bound to a throwaway `_`.
    let (writer, guard) = match appender {
        Ok(a) if dir_ok => tracing_appender::non_blocking(a),
        _ => tracing_appender::non_blocking(std::io::sink()),
    };

    assemble(filter_layer, std::io::stdout, writer).init();

    if !dir_ok {
        tracing::warn!(dir = %dir.display(), "log directory unavailable — logging to stdout only");
    }
    enforce_size_cap(&dir, LOG_DIR_MAX_BYTES);

    Arc::new(LogHandle {
        reload: reload_handle,
        dir,
        _guard: guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_and_debug_filters_pin_third_party_at_warn() {
        // The security property: raising our own crate must never raise hyper/
        // reqwest/rustls, whose debug output includes request headers.
        assert!(BASE_FILTER.starts_with("warn,"));
        assert!(DEBUG_FILTER.starts_with("warn,"));
        assert!(DEBUG_FILTER.contains("azimuth_daemon=debug"));
    }

    #[test]
    fn level_directive_maps_friendly_names() {
        assert_eq!(parse_level_directive(Some("debug"), None).unwrap(), DEBUG_FILTER);
        assert_eq!(parse_level_directive(Some("info"), None).unwrap(), BASE_FILTER);
        assert!(parse_level_directive(Some("bogus"), None).is_err());
    }

    #[test]
    fn level_directive_rejects_raising_a_third_party_target() {
        assert!(parse_level_directive(None, Some("hyper=trace")).is_err());
        assert!(parse_level_directive(None, Some("warn,reqwest=debug")).is_err());
    }

    #[test]
    fn level_directive_rejects_a_bare_global_debug() {
        // A bare level applies to every target, including dependencies.
        assert!(parse_level_directive(None, Some("debug")).is_err());
    }

    #[test]
    fn level_directive_allows_raising_our_own_modules() {
        assert!(parse_level_directive(None, Some("warn,azimuth_daemon::sdr=trace")).is_ok());
    }

    #[test]
    fn log_dir_prefers_the_config_directory() {
        let p = Path::new("C:/somewhere/config.toml");
        assert!(log_dir(p).ends_with("logs"));
    }

    #[test]
    fn size_cap_removes_oldest_until_under_budget() {
        let dir = std::env::temp_dir().join(format!("az-log-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..4 {
            std::fs::write(dir.join(format!("azimuth-daemon.2026-01-0{i}.log")), vec![b'x'; 1024])
                .unwrap();
        }
        enforce_size_cap(&dir, 2048);
        let remaining = std::fs::read_dir(&dir).unwrap().count();
        assert!(remaining <= 2, "expected cap to prune, found {remaining}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- G4: "[R82xx] PLL not locked!" ----------------------------------------

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
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

    /// The PLL counter is process-global; tests that move it take turns.
    static PLL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn text(c: &Capture) -> String {
        String::from_utf8_lossy(&c.0.lock().unwrap()).to_string()
    }

    #[test]
    fn pll_not_locked_is_counted_but_never_printed_at_default_levels() {
        let _turn = PLL_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (out, file) = (Capture::default(), Capture::default());
        let filter = EnvFilter::try_new(with_tuner_directive(BASE_FILTER)).unwrap();
        let (filter_layer, _h) = reload::Layer::new(filter);
        let sub = assemble(filter_layer, out.clone(), file.clone());
        let before = crate::sdr::pll_lock_failures();
        tracing::subscriber::with_default(sub, || {
            // Shape of a record bridged from the `log` crate by tracing-log.
            tracing::info!(log.target = "rtl_sdr_rs::tuners::r82xx", message = "[R82xx] PLL not locked!");
            tracing::info!(log.target = "rtl_sdr_rs::tuners::r82xx", message = "freq (kHz): 98500");
            tracing::info!("daemon line still printed");
        });
        assert_eq!(crate::sdr::pll_lock_failures() - before, 1, "exactly the PLL record is counted");
        for c in [&out, &file] {
            let t = text(c);
            assert!(!t.contains("PLL"), "tuner noise must not be printed: {t}");
            assert!(!t.contains("freq (kHz)"), "{t}");
            assert!(t.contains("daemon line still printed"), "{t}");
        }
    }

    #[test]
    fn real_log_crate_record_reaches_the_counter_through_the_bridge() {
        // End to end with the real `log` facade: the process-global subscriber
        // installs the tracing-log bridge exactly as `init` does.
        let _turn = PLL_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (out, file) = (Capture::default(), Capture::default());
        let filter = EnvFilter::try_new(with_tuner_directive(BASE_FILTER)).unwrap();
        let (filter_layer, _h) = reload::Layer::new(filter);
        // The process-global subscriber can only be set once; if another test in
        // this binary got there first there is nothing to observe here.
        if assemble(filter_layer, out.clone(), file.clone()).try_init().is_err() {
            return;
        }
        let before = crate::sdr::pll_lock_failures();
        log::info!(target: "rtl_sdr_rs::tuners::r82xx", "[R82xx] PLL not locked!");
        assert_eq!(crate::sdr::pll_lock_failures() - before, 1);
        assert!(!text(&out).contains("PLL") && !text(&file).contains("PLL"));
    }
}

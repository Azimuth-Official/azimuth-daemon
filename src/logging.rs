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

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, reload, EnvFilter, Layer, Registry};

/// Quiet baseline: this crate at info, everything else at warn.
pub const BASE_FILTER: &str = "warn,azimuth_daemon=info";
/// Verbose troubleshooting: this crate at debug, everything else still warn.
pub const DEBUG_FILTER: &str = "warn,azimuth_daemon=debug";

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

    /// Apply a raw EnvFilter directive string. Returns the applied value.
    pub fn set_filter(&self, directive: &str) -> Result<String, String> {
        let filter = EnvFilter::try_new(directive).map_err(|e| e.to_string())?;
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
    let filter = EnvFilter::try_new(&directive)
        .unwrap_or_else(|_| EnvFilter::new(BASE_FILTER));
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

    Registry::default()
        .with(filter_layer)
        .with(fmt::layer().with_target(true))
        .with(fmt::layer().with_writer(writer).with_ansi(false).boxed())
        .init();

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
}

//! RTK/NTRIP config persistence.
//!
//! Runtime RTK state (POST /rtk-config) previously lived only in `StatusState`
//! in memory: after a container recreation or daemon restart it reset to
//! enabled:false / provider:null / connected:false, and the operator had to
//! re-enter caster settings in the desktop app every time. This module
//! persists the merged config to disk and restores it at startup, following
//! the same machine-scoped-primary + config-dir-fallback pattern as
//! `identity.rs`:
//!   Windows: %ProgramData%\Azimuth\rtk-config.json
//!   Unix:    /var/lib/azimuth/rtk-config.json
//! Falls back to the config file's directory when the primary location is not
//! writable (logged at WARN).
//!
//! The public `save`/`load`/`restore_into` resolve real machine-scoped paths
//! via `candidate_paths`. The `_to_paths`/`_from_paths` variants are exposed
//! `pub(crate)` purely so tests can exercise the save/load/restore logic
//! against an isolated tempdir without ever touching the real primary
//! directory on the host running the test suite (identity.rs avoids this same
//! hazard by only unit-testing its path-independent `adopt()` helper).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::config::NtripConfig;
use crate::status_server::StatusState;

pub const RTK_CONFIG_FILE: &str = "rtk-config.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRtkConfig {
    pub version: u32,
    pub enabled: bool,
    pub provider: Option<String>,
    pub ntrip: NtripConfig,
}

fn primary_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("ProgramData").map(|p| PathBuf::from(p).join("Azimuth"))
    }
    #[cfg(unix)]
    {
        Some(PathBuf::from("/var/lib/azimuth"))
    }
}

fn candidate_paths(config_path: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = primary_dir() {
        v.push(d.join(RTK_CONFIG_FILE));
    }
    if let Some(parent) = config_path.parent() {
        v.push(parent.join(RTK_CONFIG_FILE));
    }
    v
}

/// Load the persisted RTK config, preferring the machine-scoped location.
/// Production code goes through `restore_into` (which also applies the
/// result); this is kept as public API — mirroring `identity::load` — for
/// callers that need the raw persisted value without mutating state.
#[allow(dead_code)]
pub fn load(config_path: &Path) -> Option<PersistedRtkConfig> {
    load_from_paths(&candidate_paths(config_path))
}

pub(crate) fn load_from_paths(paths: &[PathBuf]) -> Option<PersistedRtkConfig> {
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<PersistedRtkConfig>(&raw) {
                Ok(cfg) => {
                    debug!(path = %path.display(), "Loaded persisted RTK config");
                    return Some(cfg);
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Ignoring unparseable RTK config file")
                }
            },
            Err(_) => continue,
        }
    }
    None
}

/// Persist the RTK config. Tries the machine-scoped dir first, falls back to
/// the config dir (WARN: path divergence risk, same as identity.rs). Always
/// writes — including `enabled: false` — so a disable is durable across a
/// restart rather than only clearing in-memory state.
pub fn save(config_path: &Path, cfg: &PersistedRtkConfig) -> Result<(), String> {
    save_to_paths(&candidate_paths(config_path), cfg)
}

pub(crate) fn save_to_paths(paths: &[PathBuf], cfg: &PersistedRtkConfig) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(cfg).map_err(|e| format!("failed to serialize: {e}"))?;
    let mut last_err = None;
    for (i, path) in paths.iter().enumerate() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(path, &json) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(e) =
                        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    {
                        warn!(path = %path.display(), error = %e,
                            "Could not restrict RTK config file permissions to 0600");
                    }
                }
                if i > 0 {
                    warn!(
                        path = %path.display(),
                        "RTK config saved to FALLBACK location — machine-scoped dir was not \
                         writable; config may not survive a container recreation that resets \
                         this fallback path"
                    );
                } else {
                    debug!(path = %path.display(), "RTK config saved");
                }
                return Ok(());
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Could not write RTK config file, trying fallback");
                last_err = Some(format!("{}: {}", path.display(), e));
            }
        }
    }
    let msg = last_err.unwrap_or_else(|| "no candidate paths available".to_string());
    warn!("RTK config could NOT be persisted anywhere: {msg}");
    Err(msg)
}

/// Restore persisted RTK state into a freshly-created `StatusState` at daemon
/// startup, before the NTRIP supervisor task is spawned. Returns `true` when a
/// persisted config was found and applied (caller falls back to config.toml
/// `[ntrip]` when this returns `false`).
pub fn restore_into(state: &mut StatusState, config_path: &Path) -> bool {
    restore_from_paths(state, &candidate_paths(config_path))
}

pub(crate) fn restore_from_paths(state: &mut StatusState, paths: &[PathBuf]) -> bool {
    match load_from_paths(paths) {
        Some(persisted) => {
            state.rtk_config = Some(persisted.ntrip);
            state.rtk_enabled = persisted.enabled;
            state.rtk_provider = persisted.provider;
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ntrip() -> NtripConfig {
        NtripConfig {
            caster_url: "rtk2go.com".to_string(),
            port: 2101,
            mountpoint: "TESTMOUNT".to_string(),
            username: Some("user".to_string()),
            password: Some("secret".to_string()),
            gga_interval_secs: 10,
            serial_port: None,
            baud_rate: 115200,
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("azd-rtk-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    #[test]
    fn save_and_load_round_trip_via_explicit_paths() {
        let base = tempdir("roundtrip");
        let path = base.join(RTK_CONFIG_FILE);
        let paths = vec![path.clone()];

        let cfg = PersistedRtkConfig {
            version: 1,
            enabled: true,
            provider: Some("rtk2go".to_string()),
            ntrip: sample_ntrip(),
        };

        save_to_paths(&paths, &cfg).expect("save must succeed to an isolated tempdir");
        assert!(path.exists());

        let loaded = load_from_paths(&paths).expect("load must find the saved file");
        assert!(loaded.enabled);
        assert_eq!(loaded.provider.as_deref(), Some("rtk2go"));
        assert_eq!(loaded.ntrip.caster_url, "rtk2go.com");
        assert_eq!(loaded.ntrip.serial_port, None);
        assert_eq!(loaded.ntrip.baud_rate, 115200);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn disabling_rewrites_the_file_with_enabled_false() {
        let base = tempdir("disable");
        let path = base.join(RTK_CONFIG_FILE);
        let paths = vec![path.clone()];

        let mut cfg = PersistedRtkConfig {
            version: 1,
            enabled: true,
            provider: Some("rtk2go".to_string()),
            ntrip: sample_ntrip(),
        };
        save_to_paths(&paths, &cfg).unwrap();
        let loaded = load_from_paths(&paths).unwrap();
        assert!(loaded.enabled, "must start enabled");

        // Durable disable: rewriting with enabled:false must persist, not be
        // silently skipped (that was the old in-memory-only behavior).
        cfg.enabled = false;
        save_to_paths(&paths, &cfg).unwrap();
        let loaded = load_from_paths(&paths).unwrap();
        assert!(!loaded.enabled, "disable must be persisted, not silently dropped");
        // The caster config itself must survive a disable too — the operator
        // shouldn't have to re-enter it to re-enable later.
        assert_eq!(loaded.ntrip.caster_url, "rtk2go.com");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_returns_none_when_no_candidate_paths_exist() {
        let base = tempdir("missing");
        let path = base.join(RTK_CONFIG_FILE);
        assert!(load_from_paths(&[path]).is_none());
    }

    #[test]
    fn load_ignores_unparseable_file() {
        let base = tempdir("badjson");
        std::fs::create_dir_all(&base).unwrap();
        let bad_path = base.join("bad.json");
        std::fs::write(&bad_path, "{not json").unwrap();

        assert!(load_from_paths(&[bad_path]).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_falls_through_to_second_path_when_first_is_missing() {
        let base = tempdir("fallthrough");
        std::fs::create_dir_all(&base).unwrap();
        let missing = base.join("does-not-exist.json");
        let present = base.join("present.json");

        let cfg = PersistedRtkConfig {
            version: 1,
            enabled: true,
            provider: None,
            ntrip: sample_ntrip(),
        };
        save_to_paths(&[present.clone()], &cfg).unwrap();

        let loaded = load_from_paths(&[missing, present]).expect("must fall through to the second path");
        assert_eq!(loaded.ntrip.caster_url, "rtk2go.com");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn candidate_paths_includes_config_dir_fallback() {
        let config_path = Path::new("some").join("dir").join("config.toml");
        let paths = candidate_paths(&config_path);
        let expected_fallback = Path::new("some").join("dir").join(RTK_CONFIG_FILE);
        assert!(
            paths.iter().any(|p| p == &expected_fallback),
            "config-dir fallback must be one of the candidate paths: {paths:?}"
        );
    }

    #[test]
    fn restore_from_paths_applies_persisted_state_and_reports_found() {
        let base = tempdir("restore");
        let path = base.join(RTK_CONFIG_FILE);
        let paths = vec![path.clone()];

        let cfg = PersistedRtkConfig {
            version: 1,
            enabled: true,
            provider: Some("rtk2go".to_string()),
            ntrip: sample_ntrip(),
        };
        save_to_paths(&paths, &cfg).unwrap();

        let mut state = StatusState::new("node-1".to_string(), "tier1_rtlsdr".to_string());
        assert!(!state.rtk_enabled, "sanity: fresh state starts disabled");

        let found = restore_from_paths(&mut state, &paths);

        assert!(found, "restore_from_paths must report it found a persisted config");
        assert!(state.rtk_enabled);
        assert_eq!(state.rtk_provider.as_deref(), Some("rtk2go"));
        assert_eq!(
            state.rtk_config.as_ref().map(|c| c.caster_url.as_str()),
            Some("rtk2go.com")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn restore_from_paths_returns_false_when_nothing_persisted() {
        let base = tempdir("restore-none");
        let path = base.join(RTK_CONFIG_FILE);
        let mut state = StatusState::new("node-1".to_string(), "tier1_rtlsdr".to_string());

        let found = restore_from_paths(&mut state, &[path]);

        assert!(!found);
        assert!(!state.rtk_enabled);
        assert!(state.rtk_config.is_none());
    }
}

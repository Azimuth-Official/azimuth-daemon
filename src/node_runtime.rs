//! Live node lifecycle: config verdicts, hot-apply of a saved config, deferred
//! registration, and observation submission under the live node id.
//!
//! Everything here reads and writes shared state (`StatusState`) instead of
//! values captured once at startup, so a config written by the setup wizard
//! takes effect without restarting the daemon.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::api_client::{self, ApiClient};
use crate::config::{Config, ConfigProblem, LoadError, NodeConfig, ScanningConfig};
use crate::identity;
use crate::observation::{Observation, SubmitObservationsRequest};
use crate::status_server::{self, StatusState};

/// Overall health of the node's configuration, as reported by `/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigState {
    /// Never set up: no config file, no API key, or the location is still (0,0).
    AwaitingSetup,
    Ok,
    /// A genuine problem after setup (unreadable file, out-of-range value, ...).
    Invalid,
}

/// Result of judging a config. `cli_error` keeps the CLI/Docker wording and is
/// only set for `Invalid`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigVerdict {
    pub state: ConfigState,
    pub problems: Vec<ConfigProblem>,
    pub cli_error: Option<String>,
}

/// Fields whose problems mean "not set up yet" rather than "broken".
const SETUP_FIELDS: [&str; 2] = ["api.key", "node.location"];

pub fn evaluate(loaded: Result<&Config, &LoadError>, fallback: &Config) -> ConfigVerdict {
    match loaded {
        Err(LoadError::Missing(_)) => ConfigVerdict {
            state: ConfigState::AwaitingSetup,
            problems: Vec::new(),
            cli_error: None,
        },
        Err(LoadError::Failed(msg)) => ConfigVerdict {
            state: ConfigState::Invalid,
            problems: vec![ConfigProblem {
                field: "config_file".to_string(),
                message: msg.clone(),
            }],
            cli_error: Some(format!("Config load failed: {msg}")),
        },
        Ok(_) => evaluate_config(fallback),
    }
}

/// Judge an in-memory config (loaded from disk or just received over HTTP).
pub fn evaluate_config(config: &Config) -> ConfigVerdict {
    let problems = config.problems();
    if problems.is_empty() {
        return ConfigVerdict { state: ConfigState::Ok, problems, cli_error: None };
    }
    if problems.iter().all(|p| SETUP_FIELDS.contains(&p.field.as_str())) {
        return ConfigVerdict {
            state: ConfigState::AwaitingSetup,
            problems: Vec::new(),
            cli_error: None,
        };
    }
    // CLI/Docker wording: the original validate() strings, each once.
    let cli = config
        .validate()
        .err()
        .map(|errs| format!("Validation failed: {}", errs.join("; ")));
    ConfigVerdict { state: ConfigState::Invalid, problems, cli_error: cli }
}

/// SDR settings a config write can change at runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct SdrParams {
    pub device_index: Option<u32>,
    pub sample_rate: u32,
    pub gain: i32,
    pub dbm_offset: f32,
}

impl SdrParams {
    pub fn from_config(c: &Config) -> Self {
        Self {
            device_index: c.sdr.device_index,
            sample_rate: c.sdr.sample_rate,
            gain: c.sdr.gain,
            dbm_offset: c.sdr.dbm_offset,
        }
    }
}

/// Sent from the HTTP layer to the observation loop when a config is applied.
#[derive(Debug, Clone)]
pub struct ApplyMsg {
    pub scanning: Option<ScanningConfig>,
    pub sdr: SdrParams,
}

/// Hand a new scan plan to the scanner.
pub fn apply_plan(
    scanner: &mut crate::sdr::scanner::Scanner,
    plan: &ScanningConfig,
    new_dbm_offset: f32,
    _old_dbm_offset: f32,
) {
    *scanner = crate::sdr::scanner::Scanner::new(plan, new_dbm_offset);
}

pub fn is_placeholder(node_id: &str) -> bool {
    node_id.is_empty() || node_id == "pending" || node_id.starts_with("placeholder-")
}

/// Deferred / background node registration. At most one attempt chain runs at
/// a time; a config write kicks it immediately instead of waiting out a timer.
pub struct Registrar {
    pub client: Arc<ApiClient>,
    pub state: Arc<RwLock<StatusState>>,
    pub config_path: PathBuf,
    pub fingerprint: String,
    running: AtomicBool,
    /// Delay before attempt N; the last entry repeats.
    delays: Vec<Duration>,
    max_attempts: u32,
}

impl Registrar {
    pub fn new(
        client: Arc<ApiClient>,
        state: Arc<RwLock<StatusState>>,
        config_path: PathBuf,
        fingerprint: String,
    ) -> Self {
        Self {
            client,
            state,
            config_path,
            fingerprint,
            running: AtomicBool::new(false),
            delays: [0, 5, 15, 60, 300].iter().map(|s| Duration::from_secs(*s)).collect(),
            max_attempts: 288, // ~24 h at the 300 s steady state
        }
    }

    #[cfg(test)]
    pub fn with_schedule(mut self, delays: Vec<Duration>, max_attempts: u32) -> Self {
        self.delays = delays;
        self.max_attempts = max_attempts;
        self
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Start registering unless a chain is already running. Returns whether a
    /// new chain was started.
    pub fn kick(self: &Arc<Self>, node: NodeConfig) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.run(node).await;
            me.running.store(false, Ordering::SeqCst);
        });
        true
    }

    async fn set_state(&self, value: String) {
        self.state.write().await.registration_state = value;
    }

    async fn run(&self, node: NodeConfig) {
        for attempt in 0..self.max_attempts {
            let delay = self
                .delays
                .get(attempt as usize)
                .or_else(|| self.delays.last())
                .copied()
                .unwrap_or_default();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            // A later write may have registered us (or reset the key): re-check.
            if !is_placeholder(&self.state.read().await.node_id) {
                self.set_state("registered".to_string()).await;
                return;
            }
            self.set_state("registering".to_string()).await;
            info!(attempt = attempt + 1, "Registering node");
            // Re-read each attempt: an identity file may have been restored.
            let saved = identity::load(&self.config_path).map(|i| i.node_id);
            let result = self
                .client
                .register_node(
                    &node.hardware_type,
                    &node.label,
                    node.location.latitude,
                    node.location.longitude,
                    node.location.altitude_m,
                    Some(&self.fingerprint),
                    saved.as_deref(),
                )
                .await;
            match result {
                Ok(real_id) => {
                    self.finish_registration(&real_id).await;
                    return;
                }
                Err(e) if api_client::is_unauthorized(&e) => {
                    error!(error = %e, "Registration rejected (401) — API key is invalid; retries will not help");
                    let mut s = self.state.write().await;
                    s.registration_state = "failed:unauthorized".to_string();
                    if s.auth_error.is_none() {
                        s.auth_error = Some(status_server::AUTH_ERROR_MSG.to_string());
                    }
                    return;
                }
                Err(e) => {
                    warn!(error = %e, attempt = attempt + 1, "Node registration failed; will retry");
                    self.set_state(format!("failed:{}", short_reason(&e))).await;
                }
            }
        }
        error!("Registration gave up after the retry budget; save the config again to retry");
    }

    /// Persist a freshly issued id and publish it to the live state.
    pub async fn finish_registration(&self, real_id: &str) {
        info!(node_id = %real_id, "Node registration succeeded");
        if let Ok(mut cfg) = Config::load(&self.config_path) {
            cfg.node.id = real_id.to_string();
            if let Err(e) = cfg.save(&self.config_path) {
                warn!(error = %e, "Failed to save node_id after registration");
            }
        }
        identity::save(&self.config_path, real_id);
        {
            let mut s = self.state.write().await;
            s.node_id = real_id.to_string();
            s.registration_state = "registered".to_string();
            s.auth_error = None;
        }
        match self.client.fetch_display_name(real_id).await {
            Some(name) => self.state.write().await.node_display_name = Some(name),
            None => debug!("No display name available yet"),
        }
    }
}

fn short_reason(e: &anyhow::Error) -> String {
    if let Some(h) = e.downcast_ref::<api_client::HttpStatusError>() {
        return format!("http_{}", h.status);
    }
    "network".to_string()
}

/// Submit one batch under the node's LIVE id. Placeholder ids are skipped (the
/// server can only 404 them); a rejected key pauses submission; per-item
/// rejections are logged and never resubmitted.
pub async fn submit_batch(
    state: &Arc<RwLock<StatusState>>,
    client: &ApiClient,
    observations: Vec<Observation>,
) {
    if observations.is_empty() {
        return;
    }
    let (node_id, auth_rejected, config_ok) = {
        let s = state.read().await;
        (s.node_id.clone(), s.auth_error.is_some(), s.config_state == ConfigState::Ok)
    };
    if !config_ok {
        debug!("Skipping observation submit — configuration not applied");
        return;
    }
    if is_placeholder(&node_id) {
        debug!("Skipping observation submit — node not registered yet");
        return;
    }
    // Auth error active: don't burn requests on a rejected key. The heartbeat
    // loop probes for recovery and clears the flag.
    if auth_rejected {
        debug!("Skipping observation submit — API key rejected (401)");
        return;
    }

    let count = observations.len();
    let types: Vec<String> = observations.iter().map(|o| o.signal_type.clone()).collect();
    let request = SubmitObservationsRequest { node_id, observations };
    match client.submit_observations_detailed(&request).await {
        Ok(outcome) => {
            log_rejected(&outcome.rejected, &types);
            let mut s = state.write().await;
            s.observations_submitted += outcome.accepted as u64;
            if outcome.accepted > 0 {
                s.last_accepted_at = Some(chrono::Utc::now());
            }
            info!(submitted = count, accepted = outcome.accepted,
                rejected = outcome.rejected.len(), total = s.observations_submitted, "Batch submitted");
            s.auth_error = None;
        }
        Err(e) => {
            if api_client::is_unauthorized(&e) {
                let mut s = state.write().await;
                if s.auth_error.is_none() {
                    error!("Observation submit 401 — API key rejected; pausing submissions until the key changes");
                    s.auth_error = Some(status_server::AUTH_ERROR_MSG.to_string());
                }
            } else {
                log_rejected(&api_client::rejected_from_error(&e), &types);
                warn!(error = %e, "Observation submit failed; this batch is dropped");
            }
        }
    }
}

fn log_rejected(rejected: &[crate::observation::RejectedObservation], types: &[String]) {
    for r in rejected {
        let kind = types.get(r.index as usize).map(String::as_str).unwrap_or("?");
        warn!(index = r.index, signal_type = kind, reason = %r.reason, "Observation rejected by server");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg(key: &str, lat: f64, lon: f64) -> Config {
        let mut c = Config::default();
        c.api.key = key.to_string();
        c.node.location.latitude = lat;
        c.node.location.longitude = lon;
        c
    }

    #[test]
    fn missing_file_is_awaiting_setup_not_an_error() {
        let err = LoadError::Missing("Failed to read config at X: The system cannot find the file specified. (os error 2)".into());
        let v = evaluate(Err(&err), &Config::default());
        assert_eq!(v.state, ConfigState::AwaitingSetup);
        assert!(v.cli_error.is_none());
        assert!(v.problems.is_empty());
    }

    #[test]
    fn key_present_location_zero_is_awaiting_setup() {
        let v = evaluate_config(&cfg("k", 0.0, 0.0));
        assert_eq!(v.state, ConfigState::AwaitingSetup);
        assert!(v.cli_error.is_none());
    }

    #[test]
    fn empty_key_with_location_is_awaiting_setup() {
        assert_eq!(evaluate_config(&cfg("", 48.8, 2.3)).state, ConfigState::AwaitingSetup);
    }

    #[test]
    fn complete_config_is_ok() {
        let v = evaluate_config(&cfg("k", 48.8566, 2.3522));
        assert_eq!(v.state, ConfigState::Ok);
        assert!(v.problems.is_empty() && v.cli_error.is_none());
    }

    #[test]
    fn out_of_range_latitude_is_invalid_with_field_and_no_cli_advice() {
        let v = evaluate_config(&cfg("", 123.0, 2.3));
        assert_eq!(v.state, ConfigState::Invalid);
        assert!(v.problems.iter().any(|p| p.field == "node.location.latitude"));
        for p in &v.problems {
            assert!(!p.message.contains("azimuth-daemon --setup"), "{p:?}");
        }
        let cli = v.cli_error.unwrap();
        assert_eq!(cli.matches("api.key").count(), 1, "api.key reported once: {cli}");
    }

    #[test]
    fn parse_failure_is_invalid_naming_the_file() {
        let err = LoadError::Failed("Failed to parse config at C:/x/config.toml: bad".into());
        let v = evaluate(Err(&err), &Config::default());
        assert_eq!(v.state, ConfigState::Invalid);
        assert_eq!(v.problems[0].field, "config_file");
        assert!(v.cli_error.unwrap().starts_with("Config load failed:"));
    }

    #[test]
    fn placeholder_detection() {
        assert!(is_placeholder(""));
        assert!(is_placeholder("pending"));
        assert!(is_placeholder("placeholder-1234"));
        assert!(!is_placeholder("0f0e-real"));
    }
}

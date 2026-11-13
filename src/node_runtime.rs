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
use crate::observation::Observation;
use crate::pending;
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

/// Write `node_id` into config.toml by editing the FILE's config, not the
/// in-memory one: the in-memory config carries the region-derived scan plan and
/// any `--region` override, which must not be frozen into the file.
pub fn persist_node_id(config_path: &std::path::Path, node_id: &str) {
    let id = node_id.to_string();
    if let Err(e) = Config::update_on_disk(config_path, |c| c.node.id = id) {
        warn!(error = %e, "Could not save node_id to config; kept in the identity file only");
    }
}

/// Hand a new scan plan to the scanner.
pub fn apply_plan(
    scanner: &mut crate::sdr::scanner::Scanner,
    plan: &ScanningConfig,
    new_dbm_offset: f32,
    old_dbm_offset: f32,
) {
    if (new_dbm_offset - old_dbm_offset).abs() > f32::EPSILON {
        // The scan plan can be swapped in place, but the dBm calibration is
        // fixed at construction: rebuild (this also schedules discovery).
        *scanner = crate::sdr::scanner::Scanner::new(plan, new_dbm_offset);
    } else {
        scanner.apply_plan(plan);
    }
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
    /// Wakes a chain that is sleeping between attempts.
    wake: tokio::sync::Notify,
    /// The newest node settings; every attempt registers with these.
    latest: std::sync::Mutex<Option<NodeConfig>>,
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
            wake: tokio::sync::Notify::new(),
            latest: std::sync::Mutex::new(None),
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

    /// Register with `node`. If a chain is already running (possibly asleep
    /// between attempts) it is woken and uses these newer settings at once.
    /// Returns whether a new chain was started.
    pub fn kick(self: &Arc<Self>, node: NodeConfig) -> bool {
        *self.latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(node);
        if self.running.swap(true, Ordering::SeqCst) {
            self.wake.notify_one();
            return false;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            me.run().await;
            me.running.store(false, Ordering::SeqCst);
        });
        true
    }

    fn latest_node(&self) -> Option<NodeConfig> {
        self.latest.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    async fn set_state(&self, value: String) {
        self.state.write().await.registration_state = value;
    }

    async fn run(&self) {
        for attempt in 0..self.max_attempts {
            let delay = self
                .delays
                .get(attempt as usize)
                .or_else(|| self.delays.last())
                .copied()
                .unwrap_or_default();
            if !delay.is_zero() {
                // A kick with newer settings cuts the wait short.
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = self.wake.notified() => {}
                }
            }
            let Some(node) = self.latest_node() else {
                return;
            };
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
        persist_node_id(&self.config_path, real_id);
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
    submit_batch_at(state, client, observations, std::time::Instant::now()).await
}

/// `submit_batch` with the clock injected (tests drive the retry timing).
pub(crate) async fn submit_batch_at(
    state: &Arc<RwLock<StatusState>>,
    client: &ApiClient,
    observations: Vec<Observation>,
    now: std::time::Instant,
) {
    let (node_id, auth_rejected, config_ok, buffer) = {
        let s = state.read().await;
        (s.node_id.clone(), s.auth_error.is_some(), s.config_state == ConfigState::Ok, Arc::clone(&s.pending))
    };
    if observations.is_empty() && buffer.len() == 0 {
        return;
    }
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

    // New observations join the pending ones (oldest first); a failed upload
    // that is worth retrying stays in the buffer for a later cycle.
    let sent_all = pending::flush(&buffer, client, &node_id, observations, now).await;
    if sent_all.is_empty() {
        debug!("Observation submit waiting — retry delay in force");
        return;
    }
    for sent in sent_all {
        let (count, types) = (sent.count, sent.types);
        match sent.result {
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
                } else if !sent.kept {
                    log_rejected(&api_client::rejected_from_error(&e), &types);
                    warn!(error = %e, "Observation submit failed; this batch is dropped");
                }
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_kick_wakes_a_sleeping_chain_and_registers_with_the_new_settings() {
        // The first register call fails; the chain then sleeps for an hour.
        let calls: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
        let rec = Arc::clone(&calls);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
            let rec = Arc::clone(&rec);
            async move {
                if uri.path() != "/api/nodes/register" {
                    return (axum::http::StatusCode::NOT_FOUND, String::new());
                }
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let mut c = rec.lock().unwrap();
                c.push(v);
                if c.len() == 1 {
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom".to_string())
                } else {
                    (axum::http::StatusCode::OK, r#"{"node_id":"real-9"}"#.to_string())
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let dir = std::env::temp_dir().join(format!("az-kick-{}", std::process::id()));
        let key = Arc::new(std::sync::RwLock::new("k".to_string()));
        let client = Arc::new(ApiClient::new(&base, key));
        let state = Arc::new(RwLock::new(StatusState::new("placeholder-t".into(), "tier1_rtlsdr".into())));
        let reg = Arc::new(
            Registrar::new(client, Arc::clone(&state), dir.join("config.toml"), "fp".into())
                .with_schedule(vec![Duration::ZERO, Duration::from_secs(3600)], 5),
        );
        let mut old = Config::default().node;
        old.label = "old-label".into();
        assert!(reg.kick(old));
        for _ in 0..100 {
            if calls.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await; // now asleep
        let mut new = Config::default().node;
        new.label = "new-label".into();
        assert!(!reg.kick(new), "a chain is running: it is woken, not duplicated");
        for _ in 0..100 {
            if state.read().await.registration_state == "registered" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(state.read().await.registration_state, "registered");
        let c = calls.lock().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["label"], "old-label");
        assert_eq!(c[1]["label"], "new-label", "the second attempt uses the NEW settings");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- G3: per-item rejection handling in submit_batch -----------------------

    /// Requests the fake observations API saw, in order.
    type Bodies = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

    /// Fake API answering every POST with `reply(request_index, body)`.
    async fn fake_obs_api(
        reply: impl Fn(usize, &serde_json::Value) -> (u16, String) + Send + Sync + 'static,
    ) -> (String, Bodies) {
        let seen: Bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (rec, reply) = (Arc::clone(&seen), Arc::new(reply));
        let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
            let (rec, reply) = (Arc::clone(&rec), Arc::clone(&reply));
            async move {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let idx = {
                    let mut g = rec.lock().unwrap();
                    g.push(v.clone());
                    g.len() - 1
                };
                let (code, text) = reply(idx, &v);
                (axum::http::StatusCode::from_u16(code).unwrap(), text)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), seen)
    }

    async fn live_state() -> Arc<RwLock<StatusState>> {
        let state = Arc::new(RwLock::new(StatusState::new("real-node-1".into(), "tier1_rtlsdr".into())));
        state.write().await.config_state = ConfigState::Ok;
        state
    }

    fn client_to(base: &str) -> ApiClient {
        ApiClient::new(base, Arc::new(std::sync::RwLock::new("k".to_string())))
    }

    fn obs_at(signal_type: &str, frequency_hz: u64) -> Observation {
        Observation {
            signal_type: signal_type.to_string(),
            observed_at: chrono::Utc::now(),
            frequency_hz: Some(frequency_hz),
            timestamp_ns: None,
            tdoa_offset_ns: None,
            signal_strength_dbm: None,
            snr_db: None,
            source_id: None,
            raw_data: None,
        }
    }

    fn frequencies(body: &serde_json::Value) -> Vec<u64> {
        body["observations"].as_array().unwrap().iter().filter_map(|o| o["frequency_hz"].as_u64()).collect()
    }

    #[derive(Clone, Default)]
    struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogCapture {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCapture;
        fn make_writer(&'a self) -> LogCapture {
            self.clone()
        }
    }

    /// Thread-local capture of everything `tracing` emits while the guard lives.
    /// The tests using it run on the default current-thread runtime, so every
    /// event of the awaited code lands on this thread.
    fn capture_logs() -> (LogCapture, tracing::subscriber::DefaultGuard) {
        let cap = LogCapture::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        (cap, tracing::subscriber::set_default(sub))
    }

    fn logged(cap: &LogCapture) -> String {
        String::from_utf8_lossy(&cap.0.lock().unwrap()).to_string()
    }

    /// G3a (characterization): a partial-acceptance response logs one line per
    /// rejected item with its index, signal type and the server's reason.
    #[tokio::test]
    async fn g3a_partial_acceptance_logs_one_line_per_rejected_item() {
        let (cap, _guard) = capture_logs();
        let (base, _seen) = fake_obs_api(|_, _| {
            (200, r#"{"accepted":1,"rejected":[{"index":0,"reason":"snr out of range"},{"index":2,"reason":"unknown frequency"}]}"#.to_string())
        })
        .await;
        let state = live_state().await;
        let batch = vec![obs_at("fm_rds", 98_500_000), obs_at("lte_pss", 806_000_000), obs_at("dtv_atsc", 605_000_000)];
        submit_batch(&state, &client_to(&base), batch).await;
        let text = logged(&cap);
        let lines: Vec<&str> = text.lines().filter(|l| l.contains("Observation rejected by server")).collect();
        assert_eq!(lines.len(), 2, "one line per rejected item, got: {text}");
        assert!(
            lines[0].contains("index=0") && lines[0].contains("signal_type=") && lines[0].contains("fm_rds") && lines[0].contains("snr out of range"),
            "{}", lines[0]
        );
        assert!(
            lines[1].contains("index=2") && lines[1].contains("signal_type=") && lines[1].contains("dtv_atsc") && lines[1].contains("unknown frequency"),
            "{}", lines[1]
        );
        assert_eq!(state.read().await.observations_submitted, 1);
    }

    /// G3b (characterization): an item the server rejected is never sent again,
    /// over several submit cycles, and a partially accepted batch is not retried.
    #[tokio::test]
    async fn g3b_rejected_items_are_never_resubmitted() {
        let (base, seen) = fake_obs_api(|i, _| match i {
            // Cycle 1: index 1 (the 99.1 MHz FM item) is rejected.
            0 => (200, r#"{"accepted":2,"rejected":[{"index":1,"reason":"bad station"}]}"#.to_string()),
            _ => (200, r#"{"accepted":2,"rejected":[]}"#.to_string()),
        })
        .await;
        let (state, client) = (live_state().await, client_to(&base));
        const REJECTED_HZ: u64 = 99_100_000;
        submit_batch(&state, &client, vec![obs_at("fm_rds", 98_500_000), obs_at("fm_rds", REJECTED_HZ), obs_at("fm_rds", 100_300_000)]).await;
        submit_batch(&state, &client, vec![obs_at("fm_rds", 101_100_000), obs_at("fm_rds", 102_700_000)]).await;
        submit_batch(&state, &client, vec![obs_at("lte_pss", 806_000_000), obs_at("dtv_atsc", 605_000_000)]).await;
        let bodies = seen.lock().unwrap();
        assert_eq!(bodies.len(), 3, "exactly one request per cycle: no retry of the partial batch");
        assert!(frequencies(&bodies[0]).contains(&REJECTED_HZ), "control: cycle 1 did carry the item");
        for (n, b) in bodies.iter().enumerate().skip(1) {
            assert!(!frequencies(b).contains(&REJECTED_HZ), "rejected item resubmitted in request {n}");
        }
        assert_eq!(state.read().await.observations_submitted, 2 + 2 + 2);
    }

    /// Run one batch against an API that answers `status`/`body`; returns the
    /// state, the requests seen and the captured log.
    async fn one_batch_against(status: u16, body: &'static str) -> (Arc<RwLock<StatusState>>, Bodies, String) {
        let (cap, _guard) = capture_logs();
        let (base, seen) = fake_obs_api(move |_, _| (status, body.to_string())).await;
        let state = live_state().await;
        let batch = vec![obs_at("fm_rds", 98_500_000), obs_at("fm_rds", 99_100_000)];
        submit_batch(&state, &client_to(&base), batch).await;
        // A later cycle must not carry the first batch again.
        submit_batch(&state, &client_to(&base), vec![obs_at("lte_pss", 806_000_000)]).await;
        let text = logged(&cap);
        (state, seen, text)
    }

    /// G3c (RED today): a 204 with no body is a success. `submit_observations_detailed`
    /// json-parses the body, so today it is "submit failed; batch dropped".
    #[tokio::test]
    async fn g3c_204_empty_body_counts_as_accepted_and_is_not_resubmitted() {
        let (state, seen, text) = one_batch_against(204, "").await;
        assert!(!text.contains("batch is dropped") && !text.contains("submit failed"), "204 treated as failure: {text}");
        let s = state.read().await;
        assert!(s.last_accepted_at.is_some(), "a 2xx batch must count as accepted");
        assert_eq!(s.observations_submitted, 2 + 1, "both cycles accepted in full");
        let bodies = seen.lock().unwrap();
        assert_eq!(bodies.len(), 2, "no resubmission");
        assert_eq!(frequencies(&bodies[1]), vec![806_000_000]);
    }

    /// G3c (characterization): 202 carrying the usual body is accepted today.
    #[tokio::test]
    async fn g3c_202_with_the_normal_body_counts_as_accepted() {
        let (state, seen, text) = one_batch_against(202, r#"{"accepted":2,"rejected":[]}"#).await;
        assert!(!text.contains("batch is dropped"), "{text}");
        assert!(state.read().await.last_accepted_at.is_some());
        assert_eq!(seen.lock().unwrap().len(), 2, "no resubmission");
    }

    /// G3c (RED today): a 2xx whose body is not our shape (an async-queue 202)
    /// is still a success; the batch was taken, so it is neither dropped nor retried.
    #[tokio::test]
    async fn g3c_202_with_an_unrecognised_body_counts_as_accepted() {
        let (state, seen, text) = one_batch_against(202, r#"{"status":"queued"}"#).await;
        assert!(!text.contains("batch is dropped") && !text.contains("submit failed"), "202 treated as failure: {text}");
        assert!(state.read().await.last_accepted_at.is_some(), "a 2xx batch must count as accepted");
        assert_eq!(seen.lock().unwrap().len(), 2, "no resubmission");
    }

    // --- Retry buffer: failed uploads are kept and resent ------------------------

    /// Like `fake_obs_api`, but a reply may also carry a `Retry-After` header.
    async fn fake_api_ra(
        reply: impl Fn(usize) -> (u16, Option<u64>, String) + Send + Sync + 'static,
    ) -> (String, Bodies) {
        let seen: Bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (rec, reply) = (Arc::clone(&seen), Arc::new(reply));
        let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
            let (rec, reply) = (Arc::clone(&rec), Arc::clone(&reply));
            async move {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let idx = {
                    let mut g = rec.lock().unwrap();
                    g.push(v);
                    g.len() - 1
                };
                let (code, ra, text) = reply(idx);
                let mut h = axum::http::HeaderMap::new();
                if let Some(s) = ra {
                    h.insert("retry-after", s.to_string().parse().unwrap());
                }
                (axum::http::StatusCode::from_u16(code).unwrap(), h, text)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), seen)
    }

    const OK_BODY: &str = r#"{"accepted":1,"rejected":[]}"#;

    fn at(base: std::time::Instant, secs: u64) -> std::time::Instant {
        base + Duration::from_secs(secs)
    }

    fn keys(body: &serde_json::Value) -> Vec<String> {
        body["observations"].as_array().unwrap().iter().map(|o| o["client_dedupe_key"].as_str().unwrap_or("").to_string()).collect()
    }

    #[tokio::test]
    async fn retry_500_then_200_delivers_both_batches_oldest_first_with_stable_keys() {
        let (base_url, seen) = fake_api_ra(|i| if i == 0 { (500, None, "boom".into()) } else { (200, None, OK_BODY.into()) }).await;
        let (state, client, t0) = (live_state().await, client_to(&base_url), std::time::Instant::now());
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 1), obs_at("fm_rds", 2)], t0).await;
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 3)], at(t0, 31)).await;
        let b = seen.lock().unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(frequencies(&b[1]), vec![1, 2, 3], "pending first, oldest first, then the new one");
        let (k0, k1) = (keys(&b[0]), keys(&b[1]));
        assert!(k0.iter().all(|k| !k.is_empty()), "every observation carries a client_dedupe_key");
        assert_eq!(k0[..], k1[..2], "a resent observation keeps its key");
        assert_ne!(k1[1], k1[2], "keys are unique per observation");
    }

    #[tokio::test]
    async fn retry_after_503_sends_nothing_until_it_elapses() {
        let (base_url, seen) = fake_api_ra(|i| if i == 0 { (503, Some(7), r#"{"retryable":true}"#.into()) } else { (200, None, OK_BODY.into()) }).await;
        let (state, client, t0) = (live_state().await, client_to(&base_url), std::time::Instant::now());
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 1)], t0).await;
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 2)], at(t0, 3)).await;
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 3)], at(t0, 6)).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "no request while Retry-After is in the future");
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 4)], at(t0, 8)).await;
        let b = seen.lock().unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(frequencies(&b[1]), vec![1, 2, 3, 4], "observations that arrived during the wait were kept");
    }

    #[tokio::test]
    async fn retry_429_uses_exponential_backoff_without_retry_after() {
        let (base_url, seen) = fake_api_ra(|i| if i < 2 { (429, None, "slow down".into()) } else { (200, None, OK_BODY.into()) }).await;
        let (state, client, t0) = (live_state().await, client_to(&base_url), std::time::Instant::now());
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 1)], t0).await;
        submit_batch_at(&state, &client, vec![], at(t0, 20)).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "first wait is the 30 s base interval");
        submit_batch_at(&state, &client, vec![], at(t0, 31)).await; // second failure -> 60 s
        submit_batch_at(&state, &client, vec![], at(t0, 31 + 40)).await;
        assert_eq!(seen.lock().unwrap().len(), 2, "second wait doubled");
        submit_batch_at(&state, &client, vec![], at(t0, 31 + 62)).await;
        let b = seen.lock().unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(frequencies(&b[2]), vec![1]);
    }

    #[tokio::test]
    async fn network_error_keeps_the_batch_and_delivers_it_later() {
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}", l.local_addr().unwrap())
        }; // listener dropped: connection refused
        let (live, seen) = fake_api_ra(|_| (200, None, OK_BODY.into())).await;
        let (state, t0) = (live_state().await, std::time::Instant::now());
        submit_batch_at(&state, &client_to(&dead), vec![obs_at("fm_rds", 1), obs_at("fm_rds", 2)], t0).await;
        submit_batch_at(&state, &client_to(&live), vec![obs_at("fm_rds", 3)], at(t0, 31)).await;
        assert_eq!(frequencies(&seen.lock().unwrap()[0]), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn validation_rejection_400_is_not_resent() {
        let (base_url, seen) = fake_api_ra(|i| if i == 0 { (400, None, r#"{"error":"invalid","rejected":[{"index":0,"reason":"bad"}]}"#.into()) } else { (200, None, OK_BODY.into()) }).await;
        let (state, client, t0) = (live_state().await, client_to(&base_url), std::time::Instant::now());
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 1)], t0).await;
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 2)], at(t0, 1)).await;
        let b = seen.lock().unwrap();
        assert_eq!(b.len(), 2, "the next cycle is not delayed by a 400");
        assert_eq!(frequencies(&b[1]), vec![2], "the rejected batch is never resent");
    }

    #[tokio::test]
    async fn unauthorized_401_still_pauses_and_drops_the_batch() {
        let (base_url, seen) = fake_api_ra(|i| if i == 0 { (401, None, "no".into()) } else { (200, None, OK_BODY.into()) }).await;
        let (state, client, t0) = (live_state().await, client_to(&base_url), std::time::Instant::now());
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 1)], t0).await;
        assert!(state.read().await.auth_error.is_some());
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 2)], at(t0, 1)).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "paused: no request while the key is rejected");
        state.write().await.auth_error = None; // what the heartbeat does on recovery
        submit_batch_at(&state, &client, vec![obs_at("fm_rds", 3)], at(t0, 2)).await;
        assert_eq!(frequencies(&seen.lock().unwrap()[1]), vec![3], "the 401 batch is not resent");
    }

    #[tokio::test]
    async fn over_the_cap_the_oldest_are_dropped_and_the_count_is_logged() {
        let (cap, _guard) = capture_logs();
        let (base_url, seen) = fake_api_ra(|i| if i == 0 { (500, None, "boom".into()) } else { (200, None, OK_BODY.into()) }).await;
        let (state, client, t0) = (live_state().await, client_to(&base_url), std::time::Instant::now());
        let big: Vec<Observation> = (1..=5100u64).map(|i| obs_at("fm_rds", i)).collect();
        submit_batch_at(&state, &client, big, t0).await;
        assert!(logged(&cap).contains("dropped=100"), "one warn with the count: {}", logged(&cap));
        for n in 0..80 {
            submit_batch_at(&state, &client, vec![], at(t0, 31 + n)).await;
        }
        let b = seen.lock().unwrap();
        let all: Vec<u64> = b.iter().skip(1).flat_map(frequencies).collect();
        assert_eq!(all.len(), 5000, "exactly the cap was delivered");
        assert_eq!(all.first(), Some(&101), "the oldest 100 were dropped");
        assert_eq!(all.last(), Some(&5100), "the newest were kept");
        assert!(all.windows(2).all(|w| w[0] < w[1]), "order preserved");
        assert!(b.iter().all(|r| r["observations"].as_array().unwrap().len() <= 100), "server batch cap respected");
    }
}

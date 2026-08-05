mod api_client;
mod config;
mod dashboard;
mod gps;
mod identity;
mod observation;
mod regions;
mod sdr;
mod setup;
mod ntrip;
mod status_server;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use api_client::ApiClient;
use config::Config;
use observation::{Observation, SubmitObservationsRequest};
use sdr::SdrManager;
use sdr::scanner::Scanner;
use status_server::{AppState, RecentSignal, StatusState, run_status_server};

fn category_for_signal_type(signal_type: &str) -> &str {
    if signal_type.starts_with("lte_") {
        "lte"
    } else if signal_type.starts_with("fm_") {
        "fm"
    } else if signal_type.starts_with("dtv_") {
        "dtv"
    } else {
        "other"
    }
}

fn update_status_from_observations(state: &mut StatusState, observations: &[Observation]) {
    let count = observations.len() as u64;
    state.signals_found += count;

    for obs in observations {
        match category_for_signal_type(&obs.signal_type) {
            "lte" => state.signal_breakdown.lte += 1,
            "fm" => state.signal_breakdown.fm += 1,
            "dtv" => state.signal_breakdown.dtv += 1,
            _ => {}
        }
    }

    let mut recent: Vec<RecentSignal> = observations
        .iter()
        .map(|obs| RecentSignal {
            id: uuid::Uuid::new_v4().to_string(),
            signal_type: obs.signal_type.clone(),
            frequency_hz: obs.frequency_hz.unwrap_or(0),
            source_id: obs.source_id.clone(),
            snr_db: obs.snr_db,
            status: "Detected".to_string(),
        })
        .collect();

    recent.reverse();
    recent.truncate(20);
    state.recent_signals = recent;
}

#[derive(Parser)]
#[command(name = "azimuth-daemon", about = "Azimuth positioning network daemon")]
struct Cli {
    /// Path to config.toml
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Run interactive first-run setup wizard
    #[arg(long)]
    setup: bool,

    /// API key (for automated setup)
    #[arg(long)]
    api_key: Option<String>,

    /// Node name (for automated setup)
    #[arg(long)]
    node_name: Option<String>,

    /// Latitude (for automated setup)
    #[arg(long)]
    lat: Option<f64>,

    /// Longitude (for automated setup)
    #[arg(long)]
    lon: Option<f64>,

    /// Region code (ISO country, e.g. US, DE, JP) or AUTO — overrides config `region`
    #[arg(long)]
    region: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    // Setup wizard mode
    if cli.setup {
        let args = setup::SetupArgs {
            api_key: cli.api_key,
            node_name: cli.node_name,
            latitude: cli.lat,
            longitude: cli.lon,
            output_path: Some(cli.config.to_string_lossy().into_owned()),
            base_url: None,
        };
        setup::run_setup(args).await?;
        return Ok(());
    }

    // Config load failure must NOT kill the daemon: under NSSM that means a
    // silent restart loop, under the desktop sidecar a dead app. Fall back to
    // defaults and surface the error through /status instead.
    let (mut config, config_load_error) = match Config::load(&cli.config) {
        Ok(c) => {
            info!("Config loaded from {}", cli.config.display());
            (c, None)
        }
        Err(e) => {
            error!(error = %e, path = %cli.config.display(),
                "Config load failed — starting with defaults; run 'azimuth-daemon --setup'");
            (Config::default(), Some(format!("Config load failed: {e}")))
        }
    };

    // CLI --region overrides config.region; resolution then turns the region
    // code into concrete FM/LTE/DTV sweep plans (capability-filtered).
    if let Some(ref region) = cli.region {
        config.region = Some(region.clone());
    }
    config.apply_region();

    // Live API key, shared between ApiClient and the status server so
    // POST /config/write can hot-swap a corrected key without a restart.
    let api_key = Arc::new(std::sync::RwLock::new(config.api.key.clone()));

    // Status server state — create early so we can report config errors via HTTP
    let app_state = Arc::new(AppState {
        status: Arc::new(RwLock::new(StatusState::new(
            "pending".to_string(),
            config.node.hardware_type.clone(),
        ))),
        scanning_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        scanner_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        config_path: cli.config.clone(),
        api_key: Arc::clone(&api_key),
    });

    // Spawn status server early so sidecar can always query it
    let status_bind = config.daemon.status_bind.clone();
    let app_state_clone = Arc::clone(&app_state);
    tokio::spawn(async move {
        run_status_server(&status_bind, app_state_clone).await;
    });

    // Collect config problems and surface them all via /status.
    let mut config_errors: Vec<String> = Vec::new();
    if let Some(e) = config_load_error {
        config_errors.push(e);
    }

    // Config validation — log errors but don't exit hard
    if let Err(errors) = config.validate() {
        error!("Config validation failed ({} errors):", errors.len());
        for e in &errors {
            error!("  * {}", e);
        }
        config_errors.push(format!("Validation failed: {}", errors.join("; ")));
    }

    // Check API key separately — this is critical for operation
    let has_valid_api_key = !config.api.key.is_empty() && config.api.key != "YOUR_API_KEY_HERE";
    if !has_valid_api_key {
        let msg = "api.key is not configured — run 'azimuth-daemon --setup' to configure";
        error!("{}", msg);
        config_errors.push(msg.to_string());
    }

    if !config_errors.is_empty() {
        let mut state = app_state.status.write().await;
        state.config_error = Some(config_errors.join(" | "));
    }

    let client = Arc::new(ApiClient::new(&config.api.url, Arc::clone(&api_key)));

    // Machine-scoped identity + fingerprinted registration keep reinstalls
    // from creating duplicate nodes. Registration goes first: the server
    // dedupes on (user, fingerprint), which is correctly account-scoped. The
    // local identity file is the offline fallback only — it is not bound to an
    // account, so it is trusted only when the API cannot be reached.
    let fingerprint = identity::machine_fingerprint();
    if !config.node.id.is_empty() {
        // Config already has an id — make sure the identity file exists too.
        identity::save(&cli.config, &config.node.id);
    } else if has_valid_api_key {
        info!("No node_id — attempting registration via API");
        // A persisted identity is offered to the server as a reclaim hint. The
        // server honours it only when that node belongs to the authenticated
        // account, so this stays account-scoped while stopping a recreated
        // container from registering a duplicate.
        let saved_identity = identity::load(&cli.config);
        if let Some(saved) = &saved_identity {
            info!(node_id = %saved.node_id, "Offering persisted node identity for reclaim");
        }
        match client
            .register_node(
                &config.node.hardware_type,
                &config.node.label,
                config.node.location.latitude,
                config.node.location.longitude,
                config.node.location.altitude_m,
                Some(&fingerprint),
                saved_identity.as_ref().map(|i| i.node_id.as_str()),
            )
            .await
        {
            Ok(node_id) => {
                config.node.id = node_id;
                if let Err(e) = config.save(&cli.config) {
                    warn!(error = %e, "Failed to save node_id to config");
                }
                identity::save(&cli.config, &config.node.id);
                info!(node_id = %config.node.id, "Node registered");
            }
            Err(e) => {
                if let Some(saved) = saved_identity {
                    warn!(error = %e, node_id = %saved.node_id,
                        "Registration unreachable — adopting persisted node identity; \
                         if this machine changed accounts, re-run 'azimuth-daemon --setup'");
                    config.node.id = saved.node_id;
                    if let Err(e) = config.save(&cli.config) {
                        warn!(error = %e, "Failed to save adopted node_id to config");
                    }
                } else {
                    warn!(error = %e, "Node registration failed — will use placeholder and retry in background");
                    config.node.id = format!("placeholder-{}", uuid::Uuid::new_v4());
                }
            }
        }
    } else {
        // No API key yet (fresh install before setup) — placeholder keeps the
        // daemon alive and visible; heartbeats are suppressed for placeholders.
        config.node.id = format!("placeholder-{}", uuid::Uuid::new_v4());
    }

    let node_id = config.node.id.clone();

    // Update node_id in app_state
    {
        let mut state = app_state.status.write().await;
        state.node_id = node_id.clone();
    }

    // Background registration retry — a placeholder id means the first
    // registration attempt failed (offline install, API blip). Retry until it
    // succeeds so the node eventually joins without a manual restart.
    if node_id.starts_with("placeholder-") && has_valid_api_key {
        let retry_client = Arc::clone(&client);
        let retry_config_path = cli.config.clone();
        let retry_node = config.node.clone();
        let retry_fingerprint = fingerprint.clone();
        let app_state_retry = Arc::clone(&app_state);
        tokio::spawn(async move {
            const RETRY_INTERVAL_SECS: u64 = 300;
            const MAX_RETRIES: u32 = 288; // ~24h
            for attempt in 1..=MAX_RETRIES {
                tokio::time::sleep(Duration::from_secs(RETRY_INTERVAL_SECS)).await;
                info!(attempt, "Retrying node registration");
                match retry_client
                    .register_node(
                        &retry_node.hardware_type,
                        &retry_node.label,
                        retry_node.location.latitude,
                        retry_node.location.longitude,
                        retry_node.location.altitude_m,
                        Some(&retry_fingerprint),
                        // Re-read each attempt: an identity file may have been
                        // restored while the retry loop was running.
                        identity::load(&retry_config_path)
                            .map(|i| i.node_id)
                            .as_deref(),
                    )
                    .await
                {
                    Ok(real_id) => {
                        info!(node_id = %real_id, "Deferred registration succeeded");
                        if let Ok(mut cfg) = Config::load(&retry_config_path) {
                            cfg.node.id = real_id.clone();
                            if let Err(e) = cfg.save(&retry_config_path) {
                                warn!(error = %e, "Failed to save node_id after deferred registration");
                            }
                        }
                        identity::save(&retry_config_path, &real_id);
                        let mut state = app_state_retry.status.write().await;
                        state.node_id = real_id;
                        return;
                    }
                    Err(e) => {
                        warn!(error = %e, attempt, "Deferred registration failed; will retry");
                    }
                }
            }
            error!("Deferred registration exhausted after 24h; restart the daemon to retry");
        });
    }

    // SDR manager — handles driver install on Windows, enumerates all devices
    let mut sdr_manager = SdrManager::new(
        config.sdr.device_index,
        config.sdr.sample_rate,
        config.sdr.gain,
    );
    {
        let mut state = app_state.status.write().await;
        state.sdr_connected = sdr_manager.is_connected();
        state.device_count = sdr_manager.device_count();
        state.sdr_name = sdr_manager
            .device_serials()
            .into_iter()
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "RTL-SDR".to_string());

        #[cfg(target_os = "windows")]
        {
            state.driver_status =
                format!("{:?}", sdr::driver_install::check_driver_status());
        }
    }

    info!(
        devices = sdr_manager.device_count(),
        serials = ?sdr_manager.device_serials(),
        "SDR devices initialized"
    );

    // Spawn heartbeat loop with device info. The node id is re-read from
    // shared state every tick so a deferred registration is picked up, and
    // placeholder ids are never sent (they can only 404).
    let heartbeat_client = Arc::clone(&client);
    let heartbeat_interval = config.daemon.heartbeat_interval_secs;
    let heartbeat_device_count = sdr_manager.device_count();
    let heartbeat_serials = sdr_manager.device_serials();
    let app_state_hb = Arc::clone(&app_state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(heartbeat_interval));
        info!(interval_secs = heartbeat_interval, "Heartbeat loop started");
        loop {
            interval.tick().await;
            let (current_node_id, net_support, rtk_enabled, rtk_provider) = {
                let s = app_state_hb.status.read().await;
                (s.node_id.clone(), s.network_support_enabled, s.rtk_enabled, s.rtk_provider.clone())
            };
            if current_node_id.is_empty() || current_node_id.starts_with("placeholder-") {
                tracing::debug!("Skipping heartbeat — node not yet registered");
                continue;
            }
            match heartbeat_client
                .heartbeat(
                    &current_node_id,
                    heartbeat_device_count,
                    heartbeat_serials.clone(),
                    net_support,
                    rtk_enabled,
                    rtk_provider,
                )
                .await
            {
                Ok(()) => {
                    let mut state = app_state_hb.status.write().await;
                    state.last_heartbeat = Some(chrono::Utc::now());
                    if state.auth_error.is_some() {
                        info!("Heartbeat succeeded — clearing auth error");
                        state.auth_error = None;
                    }
                }
                Err(e) => {
                    if api_client::is_unauthorized(&e) {
                        // 401: surface via /status, then back off hard — the
                        // heartbeat stays the recovery probe, but at 5-minute
                        // cadence instead of hammering with a dead key.
                        {
                            let mut state = app_state_hb.status.write().await;
                            if state.auth_error.is_none() {
                                error!("Heartbeat 401 — API key rejected; slowing to 5-minute probes until the key changes");
                                state.auth_error =
                                    Some(status_server::AUTH_ERROR_MSG.to_string());
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(300)).await;
                    } else {
                        error!(error = %e, "Heartbeat failed");
                    }
                }
            }
        }
    });

    // Initialize RTK config from config.toml if present
    if let Some(ref ntrip_cfg) = config.ntrip {
        let mut s = app_state.status.write().await;
        s.rtk_config = Some(ntrip_cfg.clone());
    }

    // GPS reader task — provides RTCM3 channel and auto-detection
    let (rtcm_tx, rtcm_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let app_state_gps = Arc::clone(&app_state);
    tokio::spawn(async move {
        gps::run_gps_task(Arc::clone(&app_state_gps.status), rtcm_rx).await;
    });

    // Spawn NTRIP client task
    let app_state_ntrip = Arc::clone(&app_state);
    let node_lat = config.node.location.latitude;
    let node_lon = config.node.location.longitude;
    let node_alt = config.node.location.altitude_m;
    let rtcm_tx_ntrip = rtcm_tx.clone();
    tokio::spawn(async move {
        loop {
            let enabled = {
                let s = app_state_ntrip.status.read().await;
                s.rtk_enabled && s.rtk_config.is_some()
            };
            if enabled {
                if let Err(e) = ntrip::run_ntrip_client(
                    Arc::clone(&app_state_ntrip.status),
                    node_lat,
                    node_lon,
                    node_alt,
                    Some(rtcm_tx_ntrip.clone()),
                )
                .await
                {
                    tracing::warn!(error = %e, "NTRIP client error, will retry");
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    // Determine observation mode: scanner vs legacy. The scanner runs whenever
    // a [scanning] config exists AND its `enabled` flag is true (the default —
    // including when [scanning] is absent entirely); `enabled = false` is the
    // only way to opt back into the legacy single-frequency loop.
    if let Some(scanning_config) = config.scanning.as_ref().filter(|s| s.enabled) {
        // Scanner mode — multi-frequency, multi-signal-type
        let mut scanner = Scanner::new(scanning_config, config.sdr.dbm_offset);
        let scan_interval = Duration::from_secs(scanner.interval_seconds() as u64);

        // Auto-discovery sweep on first run if mode is "sweep" or "both"
        if scanner.needs_discovery() {
            info!("Running first-time station discovery sweep");
            let discovery = tokio::task::block_in_place(|| {
                scanner.discover_stations(&mut sdr_manager)
            });
            info!(
                fm = discovery.fm_stations.len(),
                lte = discovery.lte_cells.len(),
                dtv = discovery.dtv_channels.len(),
                "Discovery complete"
            );
        }

        info!("Scanner observation loop started (interval {}s)", scanner.interval_seconds());
        let scanning_enabled = Arc::clone(&app_state.scanning_enabled);
        let scanner_active = Arc::clone(&app_state.scanner_active);
        // Counts consecutive cycles with no open SDR device. Recovery
        // exhaustion (L2/L3 close the handle, reopen fails) leaves the
        // manager empty; without re-detection every later cycle returns no
        // observations while heartbeats keep the node looking healthy.
        let mut cycles_without_sdr: u64 = 0;
        loop {
            tokio::time::sleep(scan_interval).await;

            if !scanning_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            if sdr_manager.device_count() == 0 {
                cycles_without_sdr += 1;
                if cycles_without_sdr == 1 || cycles_without_sdr % 6 == 0 {
                    warn!(
                        cycles = cycles_without_sdr,
                        "No SDR devices open — attempting re-detection"
                    );
                    sdr_manager = SdrManager::new(
                        config.sdr.device_index,
                        config.sdr.sample_rate,
                        config.sdr.gain,
                    );
                    let mut state = app_state.status.write().await;
                    state.sdr_connected = sdr_manager.is_connected();
                    state.device_count = sdr_manager.device_count();
                    if sdr_manager.device_count() > 0 {
                        state.sdr_name = sdr_manager
                            .device_serials()
                            .into_iter()
                            .next()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "RTL-SDR".to_string());
                        drop(state);
                        info!("SDR re-detected — resuming scanning");
                        cycles_without_sdr = 0;
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            } else {
                cycles_without_sdr = 0;
            }

            scanner_active.store(true, std::sync::atomic::Ordering::Relaxed);
            let observations = tokio::task::block_in_place(|| {
                scanner.run_cycle(&mut sdr_manager)
            });
            scanner_active.store(false, std::sync::atomic::Ordering::Relaxed);

            if !observations.is_empty() {
                let mut state = app_state.status.write().await;
                update_status_from_observations(&mut state, &observations);
            }

            if observations.is_empty() {
                continue;
            }

            // Auth error active: don't burn requests on a rejected key. The
            // heartbeat loop probes for recovery and clears the flag.
            if app_state.status.read().await.auth_error.is_some() {
                tracing::debug!("Skipping observation submit — API key rejected (401)");
                continue;
            }

            let count = observations.len();
            let request = SubmitObservationsRequest {
                node_id: node_id.clone(),
                observations,
            };

            match client.submit_observations(&request).await {
                Ok(accepted) => {
                    let mut state = app_state.status.write().await;
                    state.observations_submitted += accepted as u64;
                    info!(submitted = count, accepted, total = state.observations_submitted, "Batch submitted");
                    if state.auth_error.is_some() {
                        state.auth_error = None;
                    }
                }
                Err(e) => {
                    if api_client::is_unauthorized(&e) {
                        let mut state = app_state.status.write().await;
                        if state.auth_error.is_none() {
                            error!("Observation submit 401 — API key rejected; pausing submissions until the key changes");
                            state.auth_error = Some(status_server::AUTH_ERROR_MSG.to_string());
                        }
                    } else {
                        warn!(error = %e, "Observation submit failed — will retry next interval");
                    }
                }
            }
        }
    } else {
        // Legacy mode — single frequency, power measurement only (Stage A behavior)
        let submit_interval = Duration::from_secs(config.daemon.submit_interval_secs);
        info!("Legacy observation loop started (submit every {}s)", config.daemon.submit_interval_secs);

        loop {
            tokio::time::sleep(submit_interval).await;

            let observations = tokio::task::block_in_place(|| {
                let device = match sdr_manager.device_mut(0) {
                    Some(d) => d,
                    None => return Vec::new(),
                };

                // Set frequency for legacy single-freq mode
                if let Err(e) = device.set_center_freq(config.sdr.center_frequency) {
                    warn!(error = %e, "Failed to set center frequency");
                    return Vec::new();
                }

                let iq_bytes = match device.capture_raw(4096) {
                    Ok(b) if b.len() >= 2 => b,
                    Ok(_) => return Vec::new(),
                    Err(e) => {
                        error!(error = %e, "SDR read failed");
                        return Vec::new();
                    }
                };

                let (db_fs, signal_dbm) = sdr::calculate_signal_power(&iq_bytes, config.sdr.dbm_offset);
                info!(
                    samples = iq_bytes.len() / 2,
                    db_fs = format!("{:.1}", db_fs),
                    signal_dbm = format!("{:.1}", signal_dbm),
                    freq_hz = config.sdr.center_frequency,
                    "IQ batch captured (legacy)"
                );

                vec![Observation {
                    signal_type: "fm_rds".to_string(),
                    observed_at: chrono::Utc::now(),
                    frequency_hz: Some(config.sdr.center_frequency),
                    timestamp_ns: None,
                    tdoa_offset_ns: None,
                    signal_strength_dbm: Some(signal_dbm),
                    snr_db: None,
                    source_id: None,
                    raw_data: None,
                }]
            });

            if !observations.is_empty() {
                let mut state = app_state.status.write().await;
                update_status_from_observations(&mut state, &observations);
            }

            if observations.is_empty() {
                continue;
            }

            // Auth error active: don't burn requests on a rejected key. The
            // heartbeat loop probes for recovery and clears the flag.
            if app_state.status.read().await.auth_error.is_some() {
                tracing::debug!("Skipping observation submit — API key rejected (401)");
                continue;
            }

            let request = SubmitObservationsRequest {
                node_id: node_id.clone(),
                observations,
            };

            match client.submit_observations(&request).await {
                Ok(accepted) => {
                    let mut state = app_state.status.write().await;
                    state.observations_submitted += accepted as u64;
                    info!(accepted, total = state.observations_submitted, "Batch submitted");
                    if state.auth_error.is_some() {
                        state.auth_error = None;
                    }
                }
                Err(e) => {
                    if api_client::is_unauthorized(&e) {
                        let mut state = app_state.status.write().await;
                        if state.auth_error.is_none() {
                            error!("Observation submit 401 — API key rejected; pausing submissions until the key changes");
                            state.auth_error = Some(status_server::AUTH_ERROR_MSG.to_string());
                        }
                    } else {
                        warn!(error = %e, "Observation submit failed — will retry next interval");
                    }
                }
            }
        }
    }
}

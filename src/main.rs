mod api_client;
mod config;
mod dashboard;
mod gps;
mod identity;
mod logging;
mod node_runtime;
mod observation;
mod regions;
mod sdr;
mod setup;
mod ntrip;
mod request_guard;
mod rtk;
mod status_server;
mod tuner_log;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use api_client::ApiClient;
use config::Config;
use observation::{category_for_signal_type, Observation};
use sdr::SdrManager;
use sdr::scanner::Scanner;
use node_runtime::{ApplyMsg, ConfigState, Registrar, SdrParams};
use status_server::{AppState, RecentSignal, StatusState, run_status_server};

/// Logged (never surfaced as a config error) while a configured node still has
/// a placeholder id: registration retries run in the background.
const NODE_NOT_REGISTERED_MSG: &str = "Node not registered yet — heartbeats suppressed until \
     registration succeeds; it is retried automatically in the background.";

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
#[command(name = "azimuth-daemon", version, about = "Azimuth positioning network daemon")]
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

    /// Bind WinUSB to Interface 0 of the attached RTL-SDR(s), print one JSON
    /// line and exit (0 ok, 2 no device, 3 needs elevation, 4 install failed,
    /// 5 other / not applicable). Windows only; needs administrator rights.
    #[arg(long)]
    install_driver: bool,

    /// With --install-driver: USB vendor id in hex (default: every supported dongle)
    #[arg(long, value_parser = parse_hex_u16)]
    vid: Option<u16>,

    /// With --install-driver: USB product id in hex
    #[arg(long, value_parser = parse_hex_u16)]
    pid: Option<u16>,

    /// With --install-driver: interface number to bind (default 0)
    #[arg(long, default_value_t = 0)]
    interface: u8,

    /// Log filter, e.g. "warn,azimuth_daemon=debug". RUST_LOG still wins.
    /// Lets a support session start the sidecar verbose without editing config.
    #[arg(long)]
    log_level: Option<String>,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .map_err(|e| format!("expected a hex number like 0bda: {e}"))
}

/// `--install-driver`: stdout carries exactly one JSON line; diagnostics go to
/// stderr so the launching app can parse stdout.
fn run_install_driver_mode(cli: &Cli) -> i32 {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .init();
    let (result, devices) = sdr::run_install_driver(cli.vid, cli.pid, cli.interface);
    println!("{}", sdr::driver_state::install_report_json(&result, &devices));
    result.exit_code()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse first: the log directory is derived from --config, and the desktop
    // sidecar always passes an explicit path. Nothing logs before this point.
    let cli = Cli::parse();
    if cli.install_driver {
        std::process::exit(run_install_driver_mode(&cli));
    }
    let log_handle = logging::init(&cli.config, cli.log_level.as_deref());

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
    // defaults and surface the state through /status instead. A MISSING file is
    // a first run (awaiting_setup), not an error.
    let (mut config, load_error) = match Config::load_checked(&cli.config) {
        Ok(c) => {
            info!("Config loaded from {}", cli.config.display());
            (c, None)
        }
        Err(e @ config::LoadError::Missing(_)) => {
            info!(path = %cli.config.display(), "No config file yet — waiting for setup");
            (Config::default(), Some(e))
        }
        Err(e) => {
            error!(error = %e, path = %cli.config.display(),
                "Config load failed — starting with defaults; run 'azimuth-daemon --setup'");
            (Config::default(), Some(e))
        }
    };

    // CLI --region overrides config.region; resolution then turns the region
    // code into concrete FM/LTE/DTV sweep plans (capability-filtered).
    if let Some(ref region) = cli.region {
        config.region = Some(region.clone());
    }
    let region_resolution = config.apply_region();

    // Judge the config once now; POST /config/write re-judges on every save.
    let verdict = match &load_error {
        Some(e) => node_runtime::evaluate(Err(e), &config),
        None => node_runtime::evaluate_config(&config),
    };
    if !matches!(load_error, Some(config::LoadError::Missing(_))) {
        if let Err(errors) = config.validate() {
            error!("Config validation failed ({} errors):", errors.len());
            for e in &errors {
                error!("  * {}", e);
            }
        }
    }
    if verdict.state == ConfigState::AwaitingSetup {
        warn!("Node is not set up yet (API key and location) — run 'azimuth-daemon --setup' or use the desktop app");
    }
    let has_valid_api_key = !config.api.key.is_empty() && config.api.key != "YOUR_API_KEY_HERE";

    // Live API key, shared between ApiClient and the status server so
    // POST /config/write can hot-swap a corrected key without a restart.
    let api_key = Arc::new(std::sync::RwLock::new(config.api.key.clone()));
    let client = Arc::new(ApiClient::new(&config.api.url, Arc::clone(&api_key)));
    let fingerprint = identity::machine_fingerprint();

    // Status server state — create early so we can report config errors via HTTP
    let status = Arc::new(RwLock::new(StatusState::new(
        "pending".to_string(),
        config.node.hardware_type.clone(),
    )));
    {
        let mut st = status.write().await;
        st.config_state = verdict.state;
        st.config_error = verdict.cli_error.clone();
        st.config_problems = verdict.problems.clone();
        st.config_path_abs = std::path::absolute(&cli.config)
            .unwrap_or_else(|_| cli.config.clone())
            .display()
            .to_string();
        st.region_effective = Some(region_resolution.effective.clone());
        st.region_source = region_resolution.source.to_string();
        st.live_location = (
            config.node.location.latitude,
            config.node.location.longitude,
            config.node.location.altitude_m,
        );
    }
    let sdr: SharedSdr = Arc::new(std::sync::Mutex::new(SdrManager::empty()));
    let (apply_tx, mut apply_rx) = tokio::sync::mpsc::unbounded_channel::<ApplyMsg>();
    let (gps_cfg_tx, gps_cfg_rx) = tokio::sync::watch::channel(config.gps.clone().unwrap_or_default());
    let stop_signal = Arc::new(tokio::sync::Notify::new());
    let registrar = Arc::new(Registrar::new(
        Arc::clone(&client),
        Arc::clone(&status),
        cli.config.clone(),
        fingerprint.clone(),
    ));
    let app_state = Arc::new(AppState {
        status: Arc::clone(&status),
        scanning_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        scanner_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        config_path: cli.config.clone(),
        api_key: Arc::clone(&api_key),
        log: Arc::clone(&log_handle),
        apply_lock: tokio::sync::Mutex::new(()),
        apply_tx,
        gps_cfg_tx,
        registrar: Arc::clone(&registrar),
        sdr: Arc::clone(&sdr),
        stop_signal: Arc::clone(&stop_signal),
    });

    // Spawn status server early so sidecar can always query it
    let status_bind = config.daemon.status_bind.clone();
    let app_state_clone = Arc::clone(&app_state);
    tokio::spawn(async move {
        run_status_server(&status_bind, app_state_clone).await;
    });

    // Machine-scoped identity + fingerprinted registration keep reinstalls
    // from creating duplicate nodes. Registration goes first: the server
    // dedupes on (user, fingerprint), which is correctly account-scoped. The
    // local identity file is the offline fallback only — it is not bound to an
    // account, so it is trusted only when the API cannot be reached.
    let setup_complete = verdict.state == ConfigState::Ok;
    if !config.node.id.is_empty() {
        // Config already has an id — make sure the identity file exists too.
        identity::save(&cli.config, &config.node.id);
    } else if setup_complete && has_valid_api_key {
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
                node_runtime::persist_node_id(&cli.config, &config.node.id);
                identity::save(&cli.config, &config.node.id);
                info!(node_id = %config.node.id, "Node registered");
            }
            Err(e) => {
                // A rejected key is surfaced through the same auth_error the
                // heartbeat loop uses, so the desktop's "API key invalid" UI
                // covers it immediately at startup.
                if api_client::is_unauthorized(&e) {
                    error!(error = %e, "Node registration rejected (401) — API key is invalid");
                    let mut state = app_state.status.write().await;
                    if state.auth_error.is_none() {
                        state.auth_error = Some(status_server::AUTH_ERROR_MSG.to_string());
                    }
                }
                if let Some(saved) = saved_identity {
                    warn!(error = %e, node_id = %saved.node_id,
                        "Registration unreachable — adopting persisted node identity; \
                         if this machine changed accounts, re-run 'azimuth-daemon --setup'");
                    config.node.id = saved.node_id;
                    node_runtime::persist_node_id(&cli.config, &config.node.id);
                } else {
                    warn!(error = %e, "Node registration failed — will use placeholder and retry in background");
                    config.node.id = format!("placeholder-{}", uuid::Uuid::new_v4());
                }
            }
        }
    } else {
        // Not set up yet — the placeholder keeps the daemon alive and visible;
        // heartbeats are suppressed for placeholders and registration starts
        // as soon as POST /config/write delivers a complete config.
        config.node.id = format!("placeholder-{}", uuid::Uuid::new_v4());
    }

    let node_id = config.node.id.clone();

    // Publish the node id and registration state to the live status.
    {
        let mut state = app_state.status.write().await;
        state.node_id = node_id.clone();
        state.registration_state = if !node_runtime::is_placeholder(&node_id) {
            "registered".to_string()
        } else {
            "unregistered".to_string()
        };
    }

    // Placeholder id + complete setup means the first attempt failed (offline
    // install, API blip): keep trying in the background so the node joins
    // without a manual restart. Unauthorized keys stop the chain.
    if node_runtime::is_placeholder(&node_id) && setup_complete && has_valid_api_key {
        registrar.kick(config.node.clone());
    }
    if !node_runtime::is_placeholder(&node_id) {
        let client_dn = Arc::clone(&client);
        let state_dn = Arc::clone(&app_state.status);
        let id_dn = node_id.clone();
        tokio::spawn(async move {
            if let Some(name) = client_dn.fetch_display_name(&id_dn).await {
                state_dn.write().await.node_display_name = Some(name);
            }
        });
    }

    // SDR manager: enumerates and opens all devices. Driver installation is
    // never attempted here (it needs administrator rights): the driver state is
    // reported through /status instead.
    let startup_params = SdrParams::from_config(&config);
    replace_sdr(&app_state.status, &sdr, &startup_params).await;
    let (heartbeat_device_count, heartbeat_serials) = {
        let m = lock_sdr(&sdr);
        (m.device_count(), m.device_serials())
    };
    info!(
        devices = heartbeat_device_count,
        serials = ?heartbeat_serials,
        "SDR devices initialized"
    );
    spawn_driver_refresh(Arc::clone(&app_state.status));

    // Spawn heartbeat loop with device info. The node id is re-read from
    // shared state every tick so a deferred registration is picked up, and
    // placeholder ids are never sent (they can only 404).
    let heartbeat_client = Arc::clone(&client);
    let heartbeat_interval = config.daemon.heartbeat_interval_secs;
    let app_state_hb = Arc::clone(&app_state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(heartbeat_interval));
        info!(interval_secs = heartbeat_interval, "Heartbeat loop started");
        // Ticks between placeholder-id reminders (~5 minutes), bounded so this
        // never turns into an unbounded busy-loop of warnings.
        let placeholder_warn_every_ticks = (300 / heartbeat_interval.max(1)).max(1);
        let mut placeholder_tick_count: u64 = 0;
        loop {
            interval.tick().await;
            let (current_node_id, net_support, rtk_enabled, rtk_provider) = {
                let s = app_state_hb.status.read().await;
                (s.node_id.clone(), s.network_support_enabled, s.rtk_enabled, s.rtk_provider.clone())
            };
            if current_node_id.is_empty() || current_node_id.starts_with("placeholder-") {
                // A placeholder id means heartbeats — and therefore this
                // node's presence on the dashboard — are silently suppressed,
                // while /status and the Docker HEALTHCHECK still report "up".
                // Without a loud, periodic explanation the node just looks
                // invisible with no clue why. Warn immediately, then every
                // ~5 minutes so it surfaces in `docker logs` without
                // spamming every 15s heartbeat tick.
                placeholder_tick_count += 1;
                if placeholder_tick_count == 1
                    || placeholder_tick_count % placeholder_warn_every_ticks == 0
                {
                    let configured = app_state_hb.status.read().await.config_state
                        == ConfigState::Ok;
                    if configured {
                        warn!(node_id = %current_node_id, "{}", NODE_NOT_REGISTERED_MSG);
                    }
                }
                tracing::debug!("Skipping heartbeat — node not yet registered");
                continue;
            }
            placeholder_tick_count = 0;
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

    // Initialize RTK config: prefer a persisted config (survives container
    // recreation / reinstall — see rtk.rs) over config.toml, since only the
    // persisted file carries the enabled/provider state a POST /rtk-config
    // previously lost on every restart. Falls back to config.toml [ntrip]
    // (caster settings only — config.toml has no enabled flag, so `enabled`
    // stays false) when nothing has ever been persisted. Must run before the
    // NTRIP supervisor task below, which reads rtk_enabled/rtk_config.
    {
        let mut s = app_state.status.write().await;
        if rtk::restore_into(&mut s, &cli.config) {
            info!("RTK config restored from persistent storage");
        } else if let Some(ref ntrip_cfg) = config.ntrip {
            s.rtk_config = Some(ntrip_cfg.clone());
        }
    }

    // GPS reader task — provides RTCM3 channel and auto-detection
    let (rtcm_tx, rtcm_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let app_state_gps = Arc::clone(&app_state);
    // The GPS task reads its settings from the watch channel built above from
    // `config.gps.unwrap_or_default()` (safe only because GpsConfig has a
    // hand-written Default: a derived one would give enabled: false and
    // silently kill GPS fleet-wide). POST /config/write updates the channel.
    tokio::spawn(async move {
        gps::run_gps_task(Arc::clone(&app_state_gps.status), rtcm_rx, gps_cfg_rx).await;
    });

    // Spawn NTRIP client task
    let app_state_ntrip = Arc::clone(&app_state);
    let rtcm_tx_ntrip = rtcm_tx.clone();
    tokio::spawn(async move {
        loop {
            let (enabled, (node_lat, node_lon, node_alt)) = {
                let s = app_state_ntrip.status.read().await;
                (s.rtk_enabled && s.rtk_config.is_some(), s.live_location)
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
    let mut sdr_params = SdrParams::from_config(&config);
    if let Some(scanning_config) = config.scanning.as_ref().filter(|s| s.enabled) {
        // Scanner mode — multi-frequency, multi-signal-type
        let mut scanner = Scanner::new(scanning_config, config.sdr.dbm_offset);
        let scan_interval = Duration::from_secs(scanner.interval_seconds() as u64);
        app_state.status.write().await.sweep_handle = Some(scanner.sweep_handle());

        // Nothing is scanned, discovered or submitted until the node is set up:
        // pre-setup defaults (location 0,0, no key) would only produce
        // observations nobody can accept. A config write releases the wait.
        wait_until_configured(
            &app_state,
            &mut apply_rx,
            &mut scanner,
            &sdr,
            &mut sdr_params,
        )
        .await;

        // Auto-discovery sweep on first run if mode is "sweep" or "both".
        // Marked scanner_active so the UI doesn't read "idle" through the
        // multi-minute sweep.
        if scanner.needs_discovery() {
            info!("Running first-time station discovery sweep");
            app_state
                .scanner_active
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let stalls_before = sdr::dongle::stall_total();
            let discovery = tokio::task::block_in_place(|| {
                scanner.discover_stations(&mut lock_sdr(&sdr))
            });
            let tuned = lock_sdr(&sdr).device_count() > 0;
            tokio::task::block_in_place(|| sdr::dongle::sweep_finished(stalls_before, tuned));
            publish_in_use(&app_state.status, &sdr).await;
            app_state
                .scanner_active
                .store(false, std::sync::atomic::Ordering::Relaxed);
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
            // Wake early when a config write arrives instead of sleeping out
            // the whole interval.
            tokio::select! {
                _ = tokio::time::sleep(if sdr::dongle::held_by_other() { scan_interval.min(IN_USE_RETRY) } else { scan_interval }) => {}
                msg = apply_rx.recv() => {
                    if let Some(msg) = msg {
                        apply_config_msg(&app_state, &mut scanner, &sdr, &mut sdr_params, msg).await;
                    }
                }
            }
            while let Ok(msg) = apply_rx.try_recv() {
                apply_config_msg(&app_state, &mut scanner, &sdr, &mut sdr_params, msg).await;
            }

            if !scanning_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            if app_state.status.read().await.config_state != ConfigState::Ok {
                continue;
            }

            if lock_sdr(&sdr).device_count() == 0 {
                cycles_without_sdr += 1;
                // Held by a recognised running program: do not open (init writes
                // would disturb its session); wait for it to exit.
                let held_retry = sdr::dongle::held_by_other()
                    && tokio::task::block_in_place(sdr::holders::holder_state) == sdr::holders::HolderState::NotRunning;
                if cycles_without_sdr == 1 || cycles_without_sdr % 6 == 0 || held_retry {
                    warn!(
                        cycles = cycles_without_sdr,
                        "No SDR devices open — attempting re-detection"
                    );
                    replace_sdr(&app_state.status, &sdr, &sdr_params).await;
                    let found = lock_sdr(&sdr).device_count() > 0;
                    if found {
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

            // Repeat discovery. `needs_discovery()` returns false immediately
            // after a sweep, so this cannot double-run; it fires on the
            // configured interval, or sooner while the FM list is below the
            // low-yield floor. Without this, whatever the single startup sweep
            // happened to find is all the node ever scans.
            if scanner.needs_discovery() {
                info!("Re-running station discovery sweep");
                scanner_active.store(true, std::sync::atomic::Ordering::Relaxed);
                let stalls_before = sdr::dongle::stall_total();
                let discovery = tokio::task::block_in_place(|| {
                    scanner.discover_stations(&mut lock_sdr(&sdr))
                });
                let tuned = lock_sdr(&sdr).device_count() > 0;
                tokio::task::block_in_place(|| sdr::dongle::sweep_finished(stalls_before, tuned));
                publish_in_use(&app_state.status, &sdr).await;
                scanner_active.store(false, std::sync::atomic::Ordering::Relaxed);
                info!(
                    fm = discovery.fm_stations.len(),
                    lte = discovery.lte_cells.len(),
                    dtv = discovery.dtv_channels.len(),
                    "Re-discovery complete"
                );
            }

            scanner_active.store(true, std::sync::atomic::Ordering::Relaxed);
            let stalls_before = sdr::dongle::stall_total();
            let observations = tokio::task::block_in_place(|| {
                scanner.run_cycle(&mut lock_sdr(&sdr))
            });
            scanner_active.store(false, std::sync::atomic::Ordering::Relaxed);
            // A cycle that stalled next to another running SDR program, or ended
            // with the dongle held, was contended: its readings may be mis-tuned.
            let tuned = lock_sdr(&sdr).device_count() > 0;
            let observations = if tokio::task::block_in_place(|| sdr::dongle::sweep_finished(stalls_before, tuned)) {
                warn!(dropped = observations.len(), "Dropping observations captured while the RTL-SDR was contended");
                Vec::new()
            } else {
                observations
            };
            publish_in_use(&app_state.status, &sdr).await;

            {
                let mut state = app_state.status.write().await;
                state.sdr_last_capture_ok_at = Some(chrono::Utc::now());
                if !observations.is_empty() {
                    update_status_from_observations(&mut state, &observations);
                }
            }

            // Submits under the LIVE node id (registration may finish after
            // this loop started).
            node_runtime::submit_batch(&app_state.status, &client, observations).await;
        }
    } else {
        // Legacy mode — single frequency, power measurement only (Stage A behavior)
        let submit_interval = Duration::from_secs(config.daemon.submit_interval_secs);
        info!("Legacy observation loop started (submit every {}s)", config.daemon.submit_interval_secs);

        loop {
            tokio::time::sleep(submit_interval).await;
            // Config writes carry nothing the legacy loop can adopt live.
            while apply_rx.try_recv().is_ok() {}

            if app_state.status.read().await.config_state != ConfigState::Ok {
                continue;
            }

            let observations = tokio::task::block_in_place(|| {
                let mut sdr_guard = lock_sdr(&sdr);
                let device = match sdr_guard.device_mut(0) {
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

            node_runtime::submit_batch(&app_state.status, &client, observations).await;
        }
    }
}

type SharedSdr = Arc<std::sync::Mutex<SdrManager>>;

/// Lock the shared SDR manager, recovering from a poisoned lock (a panic in a
/// scan must not brick `/sdr/test` and every later cycle).
fn lock_sdr(sdr: &SharedSdr) -> std::sync::MutexGuard<'_, SdrManager> {
    sdr.lock().unwrap_or_else(|p| p.into_inner())
}

/// Replace the SDR manager. The old one is swapped for an empty manager FIRST so
/// its handle is closed (a second claim on the same interface would fail), and
/// the blocking open (USB resets and backoff sleeps in the stall ladder) runs in
/// `block_in_place`, never on an async worker.
/// Open-retry period while the dongle is held (capped by the scan interval).
const IN_USE_RETRY: Duration = Duration::from_secs(20);

async fn replace_sdr(status: &Arc<RwLock<StatusState>>, sdr: &SharedSdr, p: &SdrParams) {
    *lock_sdr(sdr) = SdrManager::empty();
    sdr::dongle::fresh_start();
    // A fresh start is a newcomer: with a recognised program running (or the
    // process list unreadable) it does NO I/O on the dongle and reports in_use.
    // The process list is read here, with the SDR mutex NOT held.
    let holder = tokio::task::block_in_place(sdr::holders::holder_state);
    if sdr::dongle::newcomer_must_yield(false, holder) {
        sdr::dongle::HELD_BY_OTHER.store(true, std::sync::atomic::Ordering::Relaxed);
        publish_in_use(status, sdr).await;
        return;
    }
    let fresh = tokio::task::block_in_place(|| SdrManager::new(p.device_index, p.sample_rate, p.gain));
    publish_sdr(status, &fresh).await;
    *lock_sdr(sdr) = fresh;
    refresh_driver_state(status).await;
    publish_in_use(status, sdr).await;
}

/// While another program holds the dongle, release our handle and report
/// `in_use` (with the likely holders) where the desktop reads it
/// (`sdr_connected=false` + `sdr_test_last.state`); once it is free again, drop
/// that stale verdict.
async fn publish_in_use(status: &Arc<RwLock<StatusState>>, sdr: &SharedSdr) {
    if !sdr::dongle::held_by_other() {
        let mut s = status.write().await;
        if s.sdr_connected && s.sdr_test_last.as_ref().is_some_and(|v| v["state"] == "in_use") {
            s.sdr_test_last = None;
        }
        return;
    }
    let empty = SdrManager::empty();
    *lock_sdr(sdr) = empty;
    let holders = tokio::task::spawn_blocking(sdr::holders::likely_holders).await.unwrap_or_default();
    let mut s = status.write().await;
    s.sdr_connected = false;
    s.device_count = 0;
    s.sdr_name = "No device".to_string();
    s.sdr_test_last = Some(serde_json::json!({
        "index": 0, "opened": false, "state": "in_use", "tuner": null, "holders": holders,
        "detail": "Another program is using the RTL-SDR. Close it; Azimuth retries automatically.",
        "vid": null, "pid": null,
    }));
}

/// Copy the manager's device facts into the live status.
async fn publish_sdr(status: &Arc<RwLock<StatusState>>, manager: &SdrManager) {
    let mut state = status.write().await;
    state.sdr_connected = manager.is_connected();
    state.device_count = manager.device_count();
    state.sdr_name = manager
        .device_serials()
        .into_iter()
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if manager.is_connected() { "RTL-SDR".to_string() } else { "No device".to_string() });
}

/// Re-read the driver state (blocking OS calls) into `/status.driver`.
async fn refresh_driver_state(status: &Arc<RwLock<StatusState>>) {
    if let Ok(info) = tokio::task::spawn_blocking(sdr::current_driver_info).await {
        status.write().await.set_driver(info);
    }
}

/// While the driver is not usable, keep re-reading it so a fix made outside
/// the daemon (the elevated install) shows up without a restart.
fn spawn_driver_refresh(status: Arc<RwLock<StatusState>>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let state = status.read().await.driver.state;
            if !matches!(
                state,
                sdr::driver_state::DriverStateKind::Ok | sdr::driver_state::DriverStateKind::NotApplicable
            ) {
                refresh_driver_state(&status).await;
            }
        }
    });
}

/// Block the observation loop until a complete config is in place, adopting
/// any config writes that arrive meanwhile.
async fn wait_until_configured(
    app_state: &Arc<AppState>,
    apply_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ApplyMsg>,
    scanner: &mut Scanner,
    sdr: &SharedSdr,
    sdr_params: &mut SdrParams,
) {
    let mut logged = false;
    loop {
        while let Ok(msg) = apply_rx.try_recv() {
            apply_config_msg(app_state, scanner, sdr, sdr_params, msg).await;
        }
        if app_state.status.read().await.config_state == ConfigState::Ok {
            return;
        }
        if !logged {
            info!("Waiting for setup — scanning starts once the configuration is saved");
            logged = true;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            msg = apply_rx.recv() => {
                if let Some(msg) = msg {
                    apply_config_msg(app_state, scanner, sdr, sdr_params, msg).await;
                }
            }
        }
    }
}

/// Adopt an applied config in the observation loop: the new scan plan and, if
/// the SDR settings changed, a freshly opened device.
async fn apply_config_msg(
    app_state: &Arc<AppState>,
    scanner: &mut Scanner,
    sdr: &SharedSdr,
    sdr_params: &mut SdrParams,
    msg: ApplyMsg,
) {
    if let Some(plan) = msg.scanning.as_ref() {
        node_runtime::apply_plan(scanner, plan, msg.sdr.dbm_offset, sdr_params.dbm_offset);
        // A rebuilt scanner has a new progress handle.
        app_state.status.write().await.sweep_handle = Some(scanner.sweep_handle());
    }
    if msg.sdr.device_index != sdr_params.device_index
        || msg.sdr.sample_rate != sdr_params.sample_rate
        || msg.sdr.gain != sdr_params.gain
    {
        info!("SDR settings changed — reopening the device");
        replace_sdr(&app_state.status, sdr, &msg.sdr).await;
    }
    *sdr_params = msg.sdr;
}

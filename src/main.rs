mod api_client;
mod config;
mod observation;
mod sdr;
mod setup;
mod status_server;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use api_client::ApiClient;
use config::Config;
use observation::SubmitObservationsRequest;
use sdr::SdrManager;
use sdr::scanner::Scanner;
use status_server::{StatusState, run_status_server};

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

    let mut config = Config::load(&cli.config)?;
    info!("Config loaded from {}", cli.config.display());

    // Config validation
    if let Err(errors) = config.validate() {
        eprintln!("ERROR: Config validation failed ({} errors):", errors.len());
        for e in &errors {
            eprintln!("  * {}", e);
        }
        if config.has_placeholder_values() {
            eprintln!("\nRun 'azimuth-daemon --setup' to configure.");
        }
        std::process::exit(1);
    }

    if config.api.key.is_empty() {
        anyhow::bail!("api.key is empty — run 'azimuth-daemon --setup' to configure");
    }

    let client = Arc::new(ApiClient::new(&config.api.url, &config.api.key));

    // Auto-register node if no node_id
    if config.node.id.is_empty() {
        info!("No node_id — registering node via API");
        let node_id = client
            .register_node(
                &config.node.hardware_type,
                &config.node.label,
                config.node.location.latitude,
                config.node.location.longitude,
                config.node.location.altitude_m,
            )
            .await?;
        config.node.id = node_id;
        config.save(&cli.config)?;
        info!(node_id = %config.node.id, "Node registered and saved to config");
    }

    let node_id = config.node.id.clone();

    // Status server state
    let status_state = Arc::new(RwLock::new(StatusState::new(
        node_id.clone(),
        config.node.hardware_type.clone(),
    )));

    // SDR manager — handles driver install on Windows, enumerates all devices
    let mut sdr_manager = SdrManager::new(
        config.sdr.device_index,
        config.sdr.sample_rate,
        config.sdr.gain,
    );
    {
        let mut state = status_state.write().await;
        state.sdr_connected = sdr_manager.is_connected();
        state.device_count = sdr_manager.device_count();

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

    // Spawn status server
    let status_bind = config.daemon.status_bind.clone();
    let status_state_clone = Arc::clone(&status_state);
    tokio::spawn(async move {
        run_status_server(&status_bind, status_state_clone).await;
    });

    // Spawn heartbeat loop with device info
    let heartbeat_client = Arc::clone(&client);
    let heartbeat_node_id = node_id.clone();
    let heartbeat_interval = config.daemon.heartbeat_interval_secs;
    let heartbeat_device_count = sdr_manager.device_count();
    let heartbeat_serials = sdr_manager.device_serials();
    let status_state_hb = Arc::clone(&status_state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(heartbeat_interval));
        info!(interval_secs = heartbeat_interval, "Heartbeat loop started");
        loop {
            interval.tick().await;
            match heartbeat_client
                .heartbeat(
                    &heartbeat_node_id,
                    heartbeat_device_count,
                    heartbeat_serials.clone(),
                )
                .await
            {
                Ok(()) => {
                    let mut state = status_state_hb.write().await;
                    state.last_heartbeat = Some(chrono::Utc::now());
                }
                Err(e) => error!(error = %e, "Heartbeat failed"),
            }
        }
    });

    // Determine observation mode: scanner vs legacy
    if let Some(ref scanning_config) = config.scanning {
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
        loop {
            tokio::time::sleep(scan_interval).await;

            let observations = tokio::task::block_in_place(|| {
                scanner.run_cycle(&mut sdr_manager)
            });

            if observations.is_empty() {
                continue;
            }

            let count = observations.len();
            let request = SubmitObservationsRequest {
                node_id: node_id.clone(),
                observations,
            };

            match client.submit_observations(&request).await {
                Ok(accepted) => {
                    let mut state = status_state.write().await;
                    state.observations_submitted += accepted as u64;
                    info!(submitted = count, accepted, total = state.observations_submitted, "Batch submitted");
                }
                Err(e) => {
                    warn!(error = %e, "Observation submit failed — will retry next interval");
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

                vec![observation::Observation {
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

            if observations.is_empty() {
                continue;
            }

            let request = SubmitObservationsRequest {
                node_id: node_id.clone(),
                observations,
            };

            match client.submit_observations(&request).await {
                Ok(accepted) => {
                    let mut state = status_state.write().await;
                    state.observations_submitted += accepted as u64;
                    info!(accepted, total = state.observations_submitted, "Batch submitted");
                }
                Err(e) => {
                    warn!(error = %e, "Observation submit failed — will retry next interval");
                }
            }
        }
    }
}

mod api_client;
mod config;
mod observation;
mod sdr;
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
use sdr::SdrCapture;
use status_server::{StatusState, run_status_server};

#[derive(Parser)]
#[command(name = "azimuth-daemon", about = "Azimuth positioning network daemon")]
struct Cli {
    /// Path to config.toml
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let mut config = Config::load(&cli.config)?;
    info!("Config loaded from {}", cli.config.display());

    if config.api.key.is_empty() {
        anyhow::bail!("api.key is empty in config — register via POST /api/auth/register first, then paste the key into config.toml");
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

    // SDR capture — handles driver install on Windows if needed
    let sdr = SdrCapture::new(
        config.sdr.device_index,
        config.sdr.sample_rate,
        config.sdr.center_frequency,
        config.sdr.gain,
    );
    {
        let mut state = status_state.write().await;
        state.sdr_connected = sdr.is_connected();

        #[cfg(target_os = "windows")]
        {
            state.driver_status =
                sdr::driver_install::check_driver_status().to_string();
        }
    }

    // Spawn status server
    let status_bind = config.daemon.status_bind.clone();
    let status_state_clone = Arc::clone(&status_state);
    tokio::spawn(async move {
        run_status_server(&status_bind, status_state_clone).await;
    });

    // Spawn heartbeat loop
    let heartbeat_client = Arc::clone(&client);
    let heartbeat_node_id = node_id.clone();
    let heartbeat_interval = config.daemon.heartbeat_interval_secs;
    let status_state_hb = Arc::clone(&status_state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(heartbeat_interval));
        info!(interval_secs = heartbeat_interval, "Heartbeat loop started");
        loop {
            interval.tick().await;
            match heartbeat_client.heartbeat(&heartbeat_node_id).await {
                Ok(()) => {
                    let mut state = status_state_hb.write().await;
                    state.last_heartbeat = Some(chrono::Utc::now());
                }
                Err(e) => error!(error = %e, "Heartbeat failed"),
            }
        }
    });

    // Main observation loop
    let submit_interval = Duration::from_secs(config.daemon.submit_interval_secs);
    info!("Observation loop started (submit every {}s)", config.daemon.submit_interval_secs);

    loop {
        tokio::time::sleep(submit_interval).await;

        let observations = tokio::task::block_in_place(|| sdr.capture_batch());
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

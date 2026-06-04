use axum::{Json, Router, routing::get};
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::info;

#[derive(Debug, Clone, Serialize)]
pub struct StatusInfo {
    pub node_id: String,
    pub hardware_type: String,
    pub uptime_secs: u64,
    pub observations_submitted: u64,
    pub last_heartbeat: Option<String>,
    pub sdr_connected: bool,
}

pub struct StatusState {
    pub node_id: String,
    pub hardware_type: String,
    pub start_time: Instant,
    pub observations_submitted: u64,
    pub last_heartbeat: Option<chrono::DateTime<chrono::Utc>>,
    pub sdr_connected: bool,
}

impl StatusState {
    pub fn new(node_id: String, hardware_type: String) -> Self {
        Self {
            node_id,
            hardware_type,
            start_time: Instant::now(),
            observations_submitted: 0,
            last_heartbeat: None,
            sdr_connected: false,
        }
    }

    fn to_info(&self) -> StatusInfo {
        StatusInfo {
            node_id: self.node_id.clone(),
            hardware_type: self.hardware_type.clone(),
            uptime_secs: self.start_time.elapsed().as_secs(),
            observations_submitted: self.observations_submitted,
            last_heartbeat: self.last_heartbeat.map(|t| t.to_rfc3339()),
            sdr_connected: self.sdr_connected,
        }
    }
}

pub async fn run_status_server(bind_addr: &str, state: Arc<RwLock<StatusState>>) {
    let app = Router::new().route(
        "/status",
        get(move || {
            let state = Arc::clone(&state);
            async move {
                let s = state.read().await;
                Json(s.to_info())
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("Failed to bind status server");
    info!(addr = bind_addr, "Status server listening");
    axum::serve(listener, app).await.expect("Status server error");
}

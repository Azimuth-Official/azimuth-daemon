use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::observation::{SubmitObservationsRequest, SubmitObservationsResponse};

pub struct ApiClient {
    client: Client,
    base_url: String,
    api_key: String,
}

#[derive(Debug, Serialize)]
struct RegisterNodeRequest {
    hardware_type: String,
    label: String,
    latitude: f64,
    longitude: f64,
    altitude_m: f64,
}

#[derive(Debug, Deserialize)]
struct RegisterNodeResponse {
    node_id: String,
}

#[derive(Debug, Serialize)]
struct HeartbeatRequest {
    device_count: usize,
    device_serials: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    network_support_enabled: bool,
}

impl ApiClient {
    pub fn new(base_url: &str, api_key: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }

    pub async fn register_node(
        &self,
        hardware_type: &str,
        label: &str,
        lat: f64,
        lon: f64,
        alt: f64,
    ) -> anyhow::Result<String> {
        let url = format!("{}/api/nodes/register", self.base_url);
        let body = RegisterNodeRequest {
            hardware_type: hardware_type.to_string(),
            label: label.to_string(),
            latitude: lat,
            longitude: lon,
            altitude_m: alt,
        };

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Node registration failed ({}): {}", status, text);
        }

        let data: RegisterNodeResponse = resp.json().await?;
        info!(node_id = %data.node_id, "Node registered");
        Ok(data.node_id)
    }

    pub async fn heartbeat(
        &self,
        node_id: &str,
        device_count: usize,
        device_serials: Vec<String>,
        network_support_enabled: bool,
    ) -> anyhow::Result<()> {
        let url = format!("{}/api/nodes/{}/heartbeat", self.base_url, node_id);
        let body = HeartbeatRequest {
            device_count,
            device_serials,
            network_support_enabled,
        };
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            debug!("Heartbeat sent");
            Ok(())
        } else {
            let status = resp.status();
            error!(%status, "Heartbeat failed");
            anyhow::bail!("Heartbeat failed: {}", status)
        }
    }

    pub async fn submit_observations(
        &self,
        request: &SubmitObservationsRequest,
    ) -> anyhow::Result<u32> {
        let url = format!("{}/api/observations", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Observation submit failed ({}): {}", status, text);
        }

        let data: SubmitObservationsResponse = resp.json().await?;
        debug!(accepted = data.accepted, "Observations submitted");
        Ok(data.accepted)
    }
}

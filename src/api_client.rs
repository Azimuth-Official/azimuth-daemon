use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::observation::{SubmitObservationsRequest, SubmitObservationsResponse};

/// Non-2xx API response with the status code preserved so callers can react
/// to specific codes (401 → surface an auth error) instead of parsing strings.
#[derive(Debug)]
pub struct HttpStatusError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.body)
    }
}

impl std::error::Error for HttpStatusError {}

/// True when the error chain carries an HTTP 401 from the API.
pub fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpStatusError>()
        .map_or(false, |s| s.status == 401)
}

pub struct ApiClient {
    client: Client,
    base_url: String,
    // Shared with AppState: POST /config/write swaps the key in-place so a
    // corrected key takes effect without a daemon restart.
    api_key: std::sync::Arc<std::sync::RwLock<String>>,
}

#[derive(Debug, Serialize)]
struct RegisterNodeRequest {
    hardware_type: String,
    label: String,
    latitude: f64,
    longitude: f64,
    altitude_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    machine_fingerprint: Option<String>,
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    rtk_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rtk_provider: Option<String>,
    // Merged into nodes.metadata server-side so the dashboard can show which
    // daemon build a node is running.
    metadata: HeartbeatMetadata,
}

#[derive(Debug, Serialize)]
struct HeartbeatMetadata {
    daemon_version: &'static str,
}

impl ApiClient {
    pub fn new(base_url: &str, api_key: std::sync::Arc<std::sync::RwLock<String>>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    fn bearer(&self) -> String {
        let key = self
            .api_key
            .read()
            .map(|k| k.clone())
            .unwrap_or_default();
        format!("Bearer {}", key)
    }

    pub async fn register_node(
        &self,
        hardware_type: &str,
        label: &str,
        lat: f64,
        lon: f64,
        alt: f64,
        machine_fingerprint: Option<&str>,
    ) -> anyhow::Result<String> {
        let url = format!("{}/api/nodes/register", self.base_url);
        let body = RegisterNodeRequest {
            hardware_type: hardware_type.to_string(),
            label: label.to_string(),
            latitude: lat,
            longitude: lon,
            altitude_m: alt,
            machine_fingerprint: machine_fingerprint.map(|s| s.to_string()),
        };

        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.bearer())
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(HttpStatusError {
                status,
                body: format!("Node registration failed: {}", text),
            }));
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
        rtk_enabled: bool,
        rtk_provider: Option<String>,
    ) -> anyhow::Result<()> {
        let url = format!("{}/api/nodes/{}/heartbeat", self.base_url, node_id);
        let body = HeartbeatRequest {
            device_count,
            device_serials,
            network_support_enabled,
            rtk_enabled,
            rtk_provider,
            metadata: HeartbeatMetadata {
                daemon_version: env!("CARGO_PKG_VERSION"),
            },
        };
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.bearer())
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            debug!("Heartbeat sent");
            Ok(())
        } else {
            let status = resp.status();
            error!(%status, "Heartbeat failed");
            Err(anyhow::Error::new(HttpStatusError {
                status: status.as_u16(),
                body: "Heartbeat failed".to_string(),
            }))
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
            .header("Authorization", self.bearer())
            .json(request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(HttpStatusError {
                status,
                body: format!("Observation submit failed: {}", text),
            }));
        }

        let data: SubmitObservationsResponse = resp.json().await?;
        debug!(accepted = data.accepted, "Observations submitted");
        Ok(data.accepted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local axum server answering every route with a fixed status.
    async fn spawn_fixed_status(status: u16) -> String {
        let code = axum::http::StatusCode::from_u16(status).unwrap();
        let app = axum::Router::new().fallback(move || async move {
            if code.is_success() {
                (code, r#"{"node_id":"n","accepted":0}"#.to_string())
            } else {
                (code, "denied".to_string())
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{}", addr)
    }

    fn client_for(base: &str) -> ApiClient {
        let key = std::sync::Arc::new(std::sync::RwLock::new("test-key".to_string()));
        ApiClient::new(base, key)
    }

    #[test]
    fn heartbeat_body_reports_daemon_version() {
        let body = HeartbeatRequest {
            device_count: 1,
            device_serials: vec!["SDR-0000".to_string()],
            network_support_enabled: false,
            rtk_enabled: false,
            rtk_provider: None,
            metadata: HeartbeatMetadata {
                daemon_version: env!("CARGO_PKG_VERSION"),
            },
        };
        let json = serde_json::to_value(&body).expect("heartbeat body must serialize");
        assert_eq!(
            json["metadata"]["daemon_version"],
            serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
            "heartbeat must report the crate version so the dashboard can display it"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_401_is_detectable() {
        let base = spawn_fixed_status(401).await;
        let err = client_for(&base)
            .heartbeat("node-1", 1, vec![], false, false, None)
            .await
            .expect_err("401 must be an error");
        assert!(
            is_unauthorized(&err),
            "401 must be detectable via is_unauthorized, got: {err:#}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_500_is_not_unauthorized() {
        let base = spawn_fixed_status(500).await;
        let err = client_for(&base)
            .heartbeat("node-1", 1, vec![], false, false, None)
            .await
            .expect_err("500 must be an error");
        assert!(!is_unauthorized(&err), "500 must NOT read as unauthorized");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heartbeat_200_is_ok() {
        let base = spawn_fixed_status(200).await;
        client_for(&base)
            .heartbeat("node-1", 1, vec![], false, false, None)
            .await
            .expect("200 must be Ok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn submit_401_is_detectable() {
        let base = spawn_fixed_status(401).await;
        let req = SubmitObservationsRequest {
            node_id: "node-1".to_string(),
            observations: vec![],
        };
        let err = client_for(&base)
            .submit_observations(&req)
            .await
            .expect_err("401 must be an error");
        assert!(
            is_unauthorized(&err),
            "submit 401 must be detectable, got: {err:#}"
        );
    }
}

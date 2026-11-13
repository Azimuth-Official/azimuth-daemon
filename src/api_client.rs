use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::observation::{
    RejectedBody, RejectedObservation, SubmitObservationsRequest, SubmitObservationsResponse,
    SubmitOutcome,
};

/// Non-2xx API response with the status code preserved so callers can react
/// to specific codes (401 → surface an auth error) instead of parsing strings.
#[derive(Debug)]
pub struct HttpStatusError {
    pub status: u16,
    pub body: String,
    /// `Retry-After` in seconds from now (delta-seconds or HTTP-date form) when the server sent one.
    pub retry_after_secs: Option<u64>,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, truncated(&self.body, MAX_ERROR_DISPLAY_BYTES))
    }
}

/// Most bytes of a server response body kept in an error's text.
const MAX_ERROR_DISPLAY_BYTES: usize = 512;
/// Most bytes read from an error response body (the rest is never read).
const MAX_ERROR_BODY_BYTES: usize = 4096;
/// Most bytes read from a success response body.
const MAX_OK_BODY_BYTES: usize = 64 * 1024;

fn truncated(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Reads at most `max` bytes of the body (lossy UTF-8) and stops; a hostile
/// server cannot make the daemon buffer an unbounded body.
async fn read_limited(mut resp: reqwest::Response, max: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < max {
        match resp.chunk().await {
            Ok(Some(c)) => buf.extend_from_slice(&c),
            _ => break,
        }
    }
    buf.truncate(max);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Control characters (newlines, escape sequences, NUL ...) become spaces, so a
/// response body cannot forge log lines or drive a terminal.
fn clean_for_log(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

impl std::error::Error for HttpStatusError {}

/// `Retry-After` value in seconds from now: delta-seconds or an HTTP-date.
pub fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<u64> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(secs);
    }
    // IMF-fixdate ("Sun, 06 Nov 1994 08:49:37 GMT"); a date in the past means "now".
    let when = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    Some(when.with_timezone(&chrono::Utc).signed_duration_since(now).num_seconds().max(0) as u64)
}

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
    /// Previously issued node id from this machine's identity file. The server
    /// reclaims it when it belongs to the authenticated account, which keeps a
    /// recreated container from registering a duplicate node.
    #[serde(skip_serializing_if = "Option::is_none")]
    node_id: Option<String>,
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
        known_node_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let url = format!("{}/api/nodes/register", self.base_url);
        let body = RegisterNodeRequest {
            hardware_type: hardware_type.to_string(),
            label: label.to_string(),
            latitude: lat,
            longitude: lon,
            altitude_m: alt,
            machine_fingerprint: machine_fingerprint.map(|s| s.to_string()),
            node_id: known_node_id.map(|s| s.to_string()),
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
                retry_after_secs: None,
            }));
        }

        let data: RegisterNodeResponse = resp.json().await?;
        info!(node_id = %data.node_id, "Node registered");
        Ok(data.node_id)
    }

    /// The node's display (animal) name from `GET /api/nodes/mine`, if the
    /// server provides one. Best effort: any failure is `None`.
    pub async fn fetch_display_name(&self, node_id: &str) -> Option<String> {
        let url = format!("{}/api/nodes/mine", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", self.bearer())
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        display_name_from_nodes(&v, node_id)
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
                retry_after_secs: None,
            }))
        }
    }

    // The daemon binary submits through `submit_observations_detailed`; this
    // wrapper remains for the Android library build (android_bridge).
    #[allow(dead_code)]
    pub async fn submit_observations(
        &self,
        request: &SubmitObservationsRequest,
    ) -> anyhow::Result<u32> {
        self.submit_observations_detailed(request).await.map(|o| o.accepted)
    }

    /// Like `submit_observations`, but also returns the server's per-item
    /// rejections (`rejected: [{index, reason}]`). Old servers omit the field.
    pub async fn submit_observations_detailed(
        &self,
        request: &SubmitObservationsRequest,
    ) -> anyhow::Result<SubmitOutcome> {
        self.submit_body(request, request.observations.len()).await
    }

    /// POST an observations body holding `count` items (the shape of
    /// `SubmitObservationsRequest`, possibly with extra per-item fields).
    pub async fn submit_body<T: Serialize>(&self, body: &T, count: usize) -> anyhow::Result<SubmitOutcome> {
        let url = format!("{}/api/observations", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("Authorization", self.bearer())
            .json(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| parse_retry_after(v, chrono::Utc::now()));
            let text = clean_for_log(&read_limited(resp, MAX_ERROR_BODY_BYTES).await);
            return Err(anyhow::Error::new(HttpStatusError {
                status,
                body: format!("Observation submit failed: {}", text),
                retry_after_secs,
            }));
        }

        // Any 2xx means the server took the batch. The body is optional: a 204,
        // an async-queue 202 or an unknown shape counts every item as accepted.
        let text = read_limited(resp, MAX_OK_BODY_BYTES).await;
        let outcome = match serde_json::from_str::<SubmitObservationsResponse>(&text) {
            Ok(data) => SubmitOutcome { accepted: data.accepted, rejected: data.rejected },
            Err(_) => {
                let rejected = serde_json::from_str::<RejectedBody>(&text).map(|b| b.rejected).unwrap_or_default();
                let accepted = (count as u32).saturating_sub(rejected.len() as u32);
                SubmitOutcome { accepted, rejected }
            }
        };
        debug!(accepted = outcome.accepted, "Observations submitted");
        Ok(outcome)
    }
}

/// Find `node_id` in a nodes/mine payload (bare array or `{nodes:[...]}`) and
/// return its `display_name`, falling back to `animal_name`.
pub fn display_name_from_nodes(v: &serde_json::Value, node_id: &str) -> Option<String> {
    let list = v.as_array().or_else(|| v.get("nodes").and_then(|n| n.as_array()))?;
    let node = list
        .iter()
        .find(|n| n.get("id").and_then(|i| i.as_str()) == Some(node_id))?;
    ["display_name", "animal_name"]
        .iter()
        .filter_map(|k| node.get(*k).and_then(|x| x.as_str()))
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

/// Per-item rejections carried by an all-invalid 400 response body.
pub fn rejected_from_error(err: &anyhow::Error) -> Vec<RejectedObservation> {
    let Some(e) = err.downcast_ref::<HttpStatusError>() else {
        return Vec::new();
    };
    // The body is "Observation submit failed: <json>"; find the JSON object.
    let Some(start) = e.body.find('{') else {
        return Vec::new();
    };
    serde_json::from_str::<RejectedBody>(&e.body[start..])
        .map(|b| b.rejected)
        .unwrap_or_default()
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

    /// Local server answering POST with a fixed status and JSON body.
    async fn spawn_body(status: u16, body: &'static str) -> String {
        let code = axum::http::StatusCode::from_u16(status).unwrap();
        let app = axum::Router::new().fallback(move || async move { (code, body) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{}", addr)
    }

    fn empty_req() -> SubmitObservationsRequest {
        SubmitObservationsRequest { node_id: "n".to_string(), observations: vec![] }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn partial_acceptance_surfaces_rejected_items() {
        let base = spawn_body(
            201,
            r#"{"accepted":2,"duplicates":0,"skipped":0,"points_earned":1.5,"rejected":[{"index":2,"reason":"invalid signal_type"}]}"#,
        )
        .await;
        let out = client_for(&base).submit_observations_detailed(&empty_req()).await.unwrap();
        assert_eq!(out.accepted, 2, "accepted keeps its meaning");
        assert_eq!(
            out.rejected,
            vec![RejectedObservation { index: 2, reason: "invalid signal_type".to_string() }]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_response_without_rejected_still_parses() {
        let base = spawn_body(200, r#"{"accepted":3}"#).await;
        let out = client_for(&base).submit_observations_detailed(&empty_req()).await.unwrap();
        assert_eq!(out.accepted, 3);
        assert!(out.rejected.is_empty());
        assert_eq!(client_for(&base).submit_observations(&empty_req()).await.unwrap(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_invalid_400_exposes_rejected_items() {
        let base = spawn_body(
            400,
            r#"{"error":"Observation 0: invalid signal_type","rejected":[{"index":0,"reason":"invalid signal_type"}]}"#,
        )
        .await;
        let err = client_for(&base)
            .submit_observations_detailed(&empty_req())
            .await
            .expect_err("400 is an error");
        assert!(!is_unauthorized(&err));
        let rejected = rejected_from_error(&err);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].index, 0);
        assert_eq!(rejected[0].reason, "invalid signal_type");
    }
}

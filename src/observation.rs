use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub signal_type: String,
    pub observed_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_hz: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_ns: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tdoa_offset_ns: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_strength_dbm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_data: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub struct SubmitObservationsRequest {
    pub node_id: String,
    pub observations: Vec<Observation>,
}

#[derive(Debug, Deserialize)]
pub struct SubmitObservationsResponse {
    pub accepted: u32,
}

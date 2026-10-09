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

/// One item the server refused inside an otherwise accepted batch.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RejectedObservation {
    pub index: u32,
    pub reason: String,
}

/// Result of a submit: how many were stored plus per-item rejections.
#[derive(Debug, Clone, PartialEq)]
pub struct SubmitOutcome {
    pub accepted: u32,
    pub rejected: Vec<RejectedObservation>,
}

/// Maps a full signal type (`fm_rds`, `lte_pss`, `dtv_atsc`, `dtv_dvbt`, ...) to its
/// scoring category. Bare category names pass through unchanged.
pub fn category_for_signal_type(signal_type: &str) -> &str {
    if signal_type == "lte" || signal_type.starts_with("lte_") {
        "lte"
    } else if signal_type == "fm" || signal_type.starts_with("fm_") {
        "fm"
    } else if signal_type == "dtv" || signal_type.starts_with("dtv_") {
        "dtv"
    } else {
        "other"
    }
}

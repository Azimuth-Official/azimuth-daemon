//! Bounded in-memory buffer for observations whose upload failed with a
//! retryable error (network, timeout, HTTP 5xx, HTTP 429). Shared by every
//! submit path (`node_runtime::submit_batch` and the Android bridge) through
//! [`flush`], so the keep-and-resend rules live in one place.
//!
//! Each observation gets a `client_dedupe_key` (UUID) when it enters the
//! buffer and keeps it across resends, so a batch whose response was lost after
//! the server committed it is stored once (server: unique on node + key).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::warn;

use crate::api_client::{ApiClient, HttpStatusError};
use crate::observation::{Observation, SubmitOutcome};

/// Most observations kept for resend; over this the OLDEST are dropped.
pub const MAX_PENDING_OBSERVATIONS: usize = 5000;
/// Longest an observation is kept for resend.
pub const MAX_PENDING_AGE: Duration = Duration::from_secs(30 * 60);
/// Observations per request: the server's default `max_observations_per_batch`
/// (a larger batch is answered 413 and would be lost).
pub const MAX_REQUEST_OBSERVATIONS: usize = 100;
/// First wait after a failure without `Retry-After` (the default submit interval).
pub const BASE_BACKOFF: Duration = Duration::from_secs(30);
/// Longest wait after a failure, also the cap on a server `Retry-After`.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug)]
struct Entry {
    obs: Observation,
    key: String,
    queued_at: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    items: VecDeque<Entry>,
    not_before: Option<Instant>,
    failures: u32,
}

#[derive(Debug, Default)]
pub struct PendingBuffer(Mutex<Inner>);

impl PendingBuffer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// Queue new observations (newest last), then enforce the age and size
    /// caps by dropping the oldest. Returns how many were dropped.
    fn push(&self, obs: Vec<Observation>, now: Instant) -> usize {
        let mut g = self.lock();
        for o in obs {
            g.items.push_back(Entry { obs: o, key: uuid::Uuid::new_v4().to_string(), queued_at: now });
        }
        let mut dropped = 0;
        while g.items.front().is_some_and(|e| now.saturating_duration_since(e.queued_at) > MAX_PENDING_AGE) {
            g.items.pop_front();
            dropped += 1;
        }
        while g.items.len() > MAX_PENDING_OBSERVATIONS {
            g.items.pop_front();
            dropped += 1;
        }
        dropped
    }

    /// The oldest request-sized chunk, or `None` when empty or still waiting.
    fn chunk(&self, now: Instant) -> Option<Vec<(Observation, String)>> {
        let g = self.lock();
        if g.items.is_empty() || g.not_before.is_some_and(|t| now < t) {
            return None;
        }
        Some(g.items.iter().take(MAX_REQUEST_OBSERVATIONS).map(|e| (e.obs.clone(), e.key.clone())).collect())
    }

    /// The chunk was taken by the server (or must never be resent): remove it.
    fn done(&self, keys: &[&str]) {
        let mut g = self.lock();
        while g.items.front().is_some_and(|e| keys.contains(&e.key.as_str())) {
            g.items.pop_front();
        }
        g.failures = 0;
        g.not_before = None;
    }

    /// Keep the chunk; wait `server_wait` (Retry-After) or an exponential backoff.
    fn failed(&self, server_wait: Option<Duration>, now: Instant) {
        let mut g = self.lock();
        g.failures = g.failures.saturating_add(1);
        let backoff = BASE_BACKOFF.saturating_mul(1 << (g.failures - 1).min(8)).min(MAX_BACKOFF);
        g.not_before = Some(now + server_wait.map_or(backoff, |d| d.min(MAX_BACKOFF)));
    }
}

/// `Some(wait)` when the failure is worth retrying: network/timeout errors,
/// 5xx and 429 (`wait` = the server's `Retry-After`). `None` for everything else
/// (400-class rejections, 401): those batches are not kept.
fn retry_wait(e: &anyhow::Error) -> Option<Option<Duration>> {
    match e.downcast_ref::<HttpStatusError>() {
        None => Some(None),
        Some(h) if h.status == 429 || (500..600).contains(&h.status) => Some(h.retry_after_secs.map(Duration::from_secs)),
        Some(_) => None,
    }
}

/// Short status label for logs: the HTTP code, or "network".
fn status_label(e: &anyhow::Error) -> String {
    e.downcast_ref::<HttpStatusError>().map_or("network".to_string(), |h| h.status.to_string())
}

#[derive(Serialize)]
struct KeyedObservation<'a> {
    #[serde(flatten)]
    obs: &'a Observation,
    client_dedupe_key: &'a str,
}

#[derive(Serialize)]
struct KeyedRequest<'a> {
    node_id: &'a str,
    observations: Vec<KeyedObservation<'a>>,
}

/// What one flush attempt sent and how it ended.
pub struct Flushed {
    pub count: usize,
    pub types: Vec<String>,
    pub result: anyhow::Result<SubmitOutcome>,
    /// The batch stays buffered for a later cycle (retryable failure).
    pub kept: bool,
    /// Observations moved to the parked set by this request.
    pub parked: usize,
}

/// Queue `new` observations and send the oldest pending chunk (at most
/// [`MAX_REQUEST_OBSERVATIONS`]), if no wait is in force. `None` = no request
/// was made. Buffer effects: success or a non-retryable failure removes the
/// chunk, a retryable failure keeps it and sets the next-attempt time.
pub async fn flush(
    buf: &PendingBuffer,
    client: &ApiClient,
    node_id: &str,
    new: Vec<Observation>,
    now: Instant,
) -> Vec<Flushed> {
    let dropped = buf.push(new, now);
    if dropped > 0 {
        warn!(dropped, cap = MAX_PENDING_OBSERVATIONS, "Pending observations over the size or age cap; oldest dropped");
    }
    let Some(chunk) = buf.chunk(now) else { return Vec::new() };
    let count = chunk.len();
    let types = chunk.iter().map(|(o, _)| o.signal_type.clone()).collect();
    let body = KeyedRequest {
        node_id,
        observations: chunk.iter().map(|(o, k)| KeyedObservation { obs: o, client_dedupe_key: k }).collect(),
    };
    let started = Instant::now();
    let result = client.submit_body(&body, count).await;
    let kept = match result.as_ref().err().and_then(retry_wait) {
        Some(wait) => {
            buf.failed(wait, now + started.elapsed());
            let e = result.as_ref().err().map(status_label).unwrap_or_default();
            warn!(status = %e, count, pending = buf.len(), "Observation submit failed; batch kept for retry");
            true
        }
        None => {
            buf.done(&chunk.iter().map(|(_, k)| k.as_str()).collect::<Vec<_>>());
            false
        }
    };
    vec![Flushed { count, types, result, kept, parked: 0 }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(n: u64) -> Observation {
        Observation {
            signal_type: "fm_rds".into(),
            observed_at: chrono::Utc::now(),
            frequency_hz: Some(n),
            timestamp_ns: None,
            tdoa_offset_ns: None,
            signal_strength_dbm: None,
            snr_db: None,
            source_id: None,
            raw_data: None,
        }
    }

    #[test]
    fn age_cap_drops_only_entries_older_than_the_limit() {
        let buf = PendingBuffer::default();
        let t0 = Instant::now();
        assert_eq!(buf.push(vec![obs(1), obs(2)], t0), 0);
        assert_eq!(buf.push(vec![obs(3)], t0 + MAX_PENDING_AGE), 0, "exactly at the limit is kept");
        assert_eq!(buf.push(vec![obs(4)], t0 + MAX_PENDING_AGE + Duration::from_secs(1)), 2);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn size_cap_drops_the_oldest_and_keeps_the_newest() {
        let buf = PendingBuffer::default();
        let t0 = Instant::now();
        let dropped = buf.push((0..MAX_PENDING_OBSERVATIONS as u64 + 7).map(obs).collect(), t0);
        assert_eq!(dropped, 7);
        let chunk = buf.chunk(t0).unwrap();
        assert_eq!(chunk.len(), MAX_REQUEST_OBSERVATIONS);
        assert_eq!(chunk[0].0.frequency_hz, Some(7));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_and_retry_after_is_capped() {
        let buf = PendingBuffer::default();
        let t0 = Instant::now();
        buf.push(vec![obs(1)], t0);
        for secs in [30, 60, 120, 240, 300, 300] {
            buf.failed(None, t0);
            assert!(buf.chunk(t0 + Duration::from_secs(secs - 1)).is_none(), "{secs}");
            assert!(buf.chunk(t0 + Duration::from_secs(secs)).is_some(), "{secs}");
        }
        buf.failed(Some(Duration::from_secs(86_400)), t0);
        assert!(buf.chunk(t0 + MAX_BACKOFF).is_some());
    }

    #[test]
    fn only_network_5xx_and_429_are_retryable() {
        let http = |status| anyhow::Error::new(HttpStatusError { status, body: String::new(), retry_after_secs: None });
        for s in [500, 502, 503, 504, 429] {
            assert!(retry_wait(&http(s)).is_some(), "{s}");
        }
        for s in [400, 401, 403, 404, 413, 422] {
            assert!(retry_wait(&http(s)).is_none(), "{s}");
        }
        assert!(retry_wait(&anyhow::anyhow!("connection refused")).is_some());
    }
}

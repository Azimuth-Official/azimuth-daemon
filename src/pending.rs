//! Bounded in-memory buffer for observations whose upload failed with a
//! retryable error (network, timeout, HTTP 5xx/408/425/429). Shared by every
//! submit path (`node_runtime::submit_batch` and the Android bridge) through
//! [`flush`], so the keep-and-resend rules live in one place.
//!
//! Each observation gets a `client_dedupe_key` (UUID) when it enters the
//! buffer and keeps it across resends, so a batch whose response was lost after
//! the server committed it is stored once (server: unique on node + key).
//!
//! Data leaves the buffer in exactly three ways: a 2xx answer (`delivered`),
//! a bound being hit (`evicted`, oldest first, counted and logged), or the
//! parked set overflowing (`parked_evicted`, oldest first, counted and logged).
//! A request the server will never accept (non-retryable 4xx) moves the items
//! to the bounded parked set; it is never deleted silently.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::warn;

use crate::api_client::{ApiClient, HttpStatusError};
use crate::observation::{Observation, SubmitOutcome};

/// Most observations kept for resend; over this the OLDEST are evicted.
pub const MAX_PENDING_OBSERVATIONS: usize = 5000;
/// Most bytes (serialized size) kept for resend; over this the OLDEST are evicted.
pub const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
/// Longest an observation is kept for resend.
pub const MAX_PENDING_AGE: Duration = Duration::from_secs(30 * 60);
/// Observations per request at the start: the server's default
/// `max_observations_per_batch`. A 413 lowers it for this process (see `max_req`).
pub const MAX_REQUEST_OBSERVATIONS: usize = 100;
/// Observations / bytes kept in the parked set; over this the OLDEST are evicted.
pub const PARKED_MAX_ITEMS: usize = 1000;
pub const PARKED_MAX_BYTES: usize = 2 * 1024 * 1024;
/// First wait after a failure without `Retry-After`; it doubles per consecutive
/// failure of the same chunk (5, 10, 20 ... up to [`MAX_BACKOFF`]) and starts over
/// after a 2xx. A 30 s base (tried first) throttled delivery to about a third of a
/// 20 observations/s production rate under 18 % transient faults.
pub const BASE_BACKOFF: Duration = Duration::from_secs(5);
/// Longest exponential backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Shortest wait after any retryable failure; also the floor for `Retry-After`.
pub const MIN_BACKOFF: Duration = Duration::from_secs(5);
/// Longest `Retry-After` honoured.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
/// From this many consecutive failures of one chunk, a short `Retry-After` no
/// longer shortens the exponential backoff (a hostile server cannot hammer us).
pub const RA_TRUST_FAILURES: u32 = 4;
/// Consecutive failures of one chunk (with other chunks waiting) before it is
/// moved behind the others.
pub const POISON_AFTER: u32 = 3;
/// Requests one `flush` may make, and the wall time it may spend.
pub const DRAIN_MAX_REQUESTS: usize = 8;
pub const DRAIN_BUDGET: Duration = Duration::from_secs(20);
/// Successful requests at a lowered size before the size doubles again.
const RECOVER_AFTER: u32 = 5;
/// Lowest factor jitter applies to a backoff.
const JITTER_MIN: f64 = 0.8;

/// Counters of what the buffer holds and has done (visible, never silent).
/// Conservation: `pending + parked + delivered + evicted_total + parked_evicted_total == submitted`.
#[allow(dead_code)] // read by the tests; the counters are also logged on every park/eviction
#[derive(Debug, Default, Clone)]
pub struct PendingStats {
    pub pending: usize,
    pub pending_bytes: usize,
    pub parked: usize,
    pub submitted: usize,
    pub delivered: usize,
    pub parked_total: usize,
    pub evicted_total: usize,
    pub parked_evicted_total: usize,
    pub rotated_total: usize,
}

#[derive(Debug)]
struct Entry {
    obs: Observation,
    key: String,
    queued_at: Instant,
    /// Arrival order; the oldest data has the lowest value even after a rotation.
    seq: u64,
    bytes: usize,
    /// Chunks never span two groups (a rotated block stays together).
    group: u64,
    /// Consecutive failed attempts of the chunk this entry heads.
    fails: u32,
    /// A rotated chunk is not offered again before this time.
    cool_until: Option<Instant>,
}

#[derive(Debug)]
struct Inner {
    items: VecDeque<Entry>,
    parked: VecDeque<Entry>,
    pending_bytes: usize,
    parked_bytes: usize,
    not_before: Option<Instant>,
    /// Current request size; lowered by a 413, restored after a run of successes.
    max_req: usize,
    ok_streak: u32,
    /// Request size before the current run of 413 splits began.
    pre_split: Option<usize>,
    /// A `flush` is in progress (the buffer has one sender).
    flushing: bool,
    next_seq: u64,
    next_group: u64,
    /// First key and length of the chunk last handed out.
    cur: Option<(String, usize)>,
    submitted: usize,
    delivered: usize,
    parked_total: usize,
    evicted_total: usize,
    parked_evicted_total: usize,
    rotated_total: usize,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            items: VecDeque::new(),
            parked: VecDeque::new(),
            pending_bytes: 0,
            parked_bytes: 0,
            not_before: None,
            max_req: MAX_REQUEST_OBSERVATIONS,
            ok_streak: 0,
            pre_split: None,
            flushing: false,
            next_seq: 0,
            next_group: 1,
            cur: None,
            submitted: 0,
            delivered: 0,
            parked_total: 0,
            evicted_total: 0,
            parked_evicted_total: 0,
            rotated_total: 0,
        }
    }
}

impl Inner {
    /// Index of the entry holding `keys[0]`, if the whole run is still queued.
    fn locate(&self, keys: &[&str]) -> Option<usize> {
        let first = keys.first()?;
        let p = self.items.iter().position(|e| e.key == *first)?;
        (p + keys.len() <= self.items.len()).then_some(p)
    }

    /// Remove `len` entries from `p`, keeping the byte total right.
    fn take(&mut self, p: usize, len: usize) -> Vec<Entry> {
        let v: Vec<Entry> = self.items.drain(p..p + len).collect();
        self.pending_bytes -= v.iter().map(|e| e.bytes).sum::<usize>();
        v
    }

    /// Start index and length of the chunk a failure refers to.
    fn current(&self) -> Option<(usize, usize)> {
        if let Some((k, len)) = &self.cur {
            if let Some(p) = self.items.iter().position(|e| e.key == *k) {
                return Some((p, (*len).min(self.items.len() - p)));
            }
        }
        let first = self.items.front()?;
        let n = self.items.iter().take_while(|e| e.group == first.group).count();
        Some((0, n.min(self.max_req)))
    }

    /// Evict by age, then (oldest first) until the item and byte bounds hold.
    /// Returns (total, by_age, by_count, by_bytes).
    fn enforce_bounds(&mut self, now: Instant) -> (usize, usize, usize, usize) {
        let n = self.items.len();
        let mut remove = vec![false; n];
        let (mut left_count, mut left_bytes) = (n, self.pending_bytes);
        let (mut by_age, mut by_count, mut by_bytes) = (0, 0, 0);
        for (i, e) in self.items.iter().enumerate() {
            if now.saturating_duration_since(e.queued_at) > MAX_PENDING_AGE {
                remove[i] = true;
                by_age += 1;
                left_count -= 1;
                left_bytes -= e.bytes;
            }
        }
        if left_count > MAX_PENDING_OBSERVATIONS || left_bytes > MAX_PENDING_BYTES {
            let mut order: Vec<usize> = (0..n).filter(|i| !remove[*i]).collect();
            order.sort_by_key(|i| self.items[*i].seq);
            for i in order {
                if left_count <= MAX_PENDING_OBSERVATIONS && left_bytes <= MAX_PENDING_BYTES {
                    break;
                }
                if left_count > MAX_PENDING_OBSERVATIONS {
                    by_count += 1;
                } else {
                    by_bytes += 1;
                }
                remove[i] = true;
                left_count -= 1;
                left_bytes -= self.items[i].bytes;
            }
        }
        let total = by_age + by_count + by_bytes;
        if total == 0 {
            return (0, 0, 0, 0);
        }
        // The chunk in flight stays the current one when its head survives (its length
        // shrinks by the members evicted); otherwise a failure falls back to the front chunk.
        let cur = self.cur.take().and_then(|(k, len)| {
            let p = self.items.iter().position(|e| e.key == k)?;
            (!remove[p]).then(|| (k, (p..(p + len).min(n)).filter(|i| !remove[*i]).count()))
        });
        // Rebuild; a removed chunk head hands its failure state to the next entry of its group.
        let old = std::mem::take(&mut self.items);
        let mut carry: Option<(u64, u32, Option<Instant>)> = None;
        for (i, mut e) in old.into_iter().enumerate() {
            if remove[i] {
                if e.fails > 0 {
                    carry = Some((e.group, e.fails, e.cool_until));
                }
                continue;
            }
            if let Some((g, f, c)) = carry.take() {
                if g == e.group && e.fails == 0 {
                    e.fails = f;
                    e.cool_until = c;
                }
            }
            self.items.push_back(e);
        }
        self.pending_bytes = left_bytes;
        self.evicted_total += total;
        self.cur = cur;
        (total, by_age, by_count, by_bytes)
    }
}

#[derive(Debug, Default)]
pub struct PendingBuffer(Mutex<Inner>);

fn entry_bytes(o: &Observation, key: &str) -> usize {
    serde_json::to_string(o).map_or(256, |s| s.len()) + key.len() + 32
}

impl PendingBuffer {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    #[allow(dead_code)]
    pub fn stats(&self) -> PendingStats {
        let g = self.lock();
        PendingStats {
            pending: g.items.len(),
            pending_bytes: g.pending_bytes,
            parked: g.parked.len(),
            submitted: g.submitted,
            delivered: g.delivered,
            parked_total: g.parked_total,
            evicted_total: g.evicted_total,
            parked_evicted_total: g.parked_evicted_total,
            rotated_total: g.rotated_total,
        }
    }

    /// Queue new observations (newest last), then enforce the age, count and
    /// byte bounds by evicting the oldest. One warn per call that evicts.
    /// Returns how many were evicted.
    fn push(&self, obs: Vec<Observation>, now: Instant) -> usize {
        let mut g = self.lock();
        for o in obs {
            let key = uuid::Uuid::new_v4().to_string();
            let bytes = entry_bytes(&o, &key);
            let seq = g.next_seq;
            g.next_seq += 1;
            g.submitted += 1;
            g.pending_bytes += bytes;
            g.items.push_back(Entry { obs: o, key, queued_at: now, seq, bytes, group: 0, fails: 0, cool_until: None });
        }
        let (dropped, age, count, bytes) = g.enforce_bounds(now);
        if dropped > 0 {
            warn!(
                dropped, by_age = age, by_count = count, by_bytes = bytes,
                evicted_total = g.evicted_total, max_items = MAX_PENDING_OBSERVATIONS, max_bytes = MAX_PENDING_BYTES,
                "Pending observations over a bound; oldest dropped"
            );
        }
        dropped
    }

    /// The next request-sized chunk (oldest ready group first), or `None` when
    /// empty or every chunk is still waiting.
    fn chunk(&self, now: Instant) -> Option<Vec<(Observation, String)>> {
        let mut g = self.lock();
        if g.items.is_empty() || g.not_before.is_some_and(|t| now < t) {
            return None;
        }
        let n = g.items.len();
        let mut i = 0;
        while i < n {
            let gid = g.items[i].group;
            let mut j = i;
            while j < n && g.items[j].group == gid {
                j += 1;
            }
            if !g.items[i].cool_until.is_some_and(|t| now < t) {
                let take = (j - i).min(g.max_req);
                let out: Vec<_> = g.items.iter().skip(i).take(take).map(|e| (e.obs.clone(), e.key.clone())).collect();
                g.cur = Some((g.items[i].key.clone(), take));
                return Some(out);
            }
            i = j;
        }
        None
    }

    /// The server took the chunk: remove it.
    fn done(&self, keys: &[&str]) {
        let mut g = self.lock();
        if let Some(p) = g.locate(keys) {
            g.take(p, keys.len());
            g.delivered += keys.len();
            if g.max_req < MAX_REQUEST_OBSERVATIONS {
                g.ok_streak += 1;
                if g.ok_streak >= RECOVER_AFTER {
                    g.max_req = (g.max_req * 2).min(MAX_REQUEST_OBSERVATIONS);
                    g.ok_streak = 0;
                    if g.max_req >= MAX_REQUEST_OBSERVATIONS {
                        g.pre_split = None;
                    }
                }
            }
        }
        g.cur = None;
        g.not_before = None;
    }

    /// The server will never accept the chunk: move it to the bounded parked
    /// set (kept, counted, one warn). Returns how many items were parked.
    fn park(&self, keys: &[&str], status: u16) -> usize {
        let mut g = self.lock();
        let Some(p) = g.locate(keys) else { return 0 };
        let moved = g.take(p, keys.len());
        let n = moved.len();
        for mut e in moved {
            e.fails = 0;
            e.cool_until = None;
            e.group = 0;
            g.parked_bytes += e.bytes;
            g.parked.push_back(e);
        }
        g.parked_total += n;
        g.cur = None;
        warn!(status, class = %format!("{}xx", status / 100), items = n, parked = g.parked.len(), parked_total = g.parked_total,
            "Observations parked: held in memory, NOT resent automatically, lost at restart");
        let mut evicted = 0;
        while g.parked.len() > PARKED_MAX_ITEMS || g.parked_bytes > PARKED_MAX_BYTES {
            let Some(e) = g.parked.pop_front() else { break };
            g.parked_bytes -= e.bytes;
            evicted += 1;
        }
        if evicted > 0 {
            g.parked_evicted_total += evicted;
            warn!(dropped = evicted, parked_evicted_total = g.parked_evicted_total, max_items = PARKED_MAX_ITEMS,
                "Parked observations over bound; oldest dropped");
        }
        n
    }

    /// No request before `until` (never shortens a wait already in force).
    fn hold(&self, until: Instant) {
        let mut g = self.lock();
        g.not_before = Some(g.not_before.map_or(until, |t| t.max(until)));
    }

    /// A 413 for `len` items: halve the request size (down to 1).
    fn shrink(&self, len: usize) {
        let mut g = self.lock();
        if g.pre_split.is_none() {
            g.pre_split = Some(g.max_req);
        }
        g.max_req = (len / 2).max(1);
        g.ok_streak = 0;
        g.cur = None;
    }

    /// A single item answered 413 on its own: the size was never the problem,
    /// so go back to the size in use before the splits began.
    fn unshrink(&self) {
        let mut g = self.lock();
        if let Some(n) = g.pre_split.take() {
            g.max_req = n;
        }
        g.ok_streak = 0;
    }

    /// Keep the chunk after a retryable failure (no jitter; see [`failed_jit`]).
    #[cfg(test)]
    fn failed(&self, server_wait: Option<Duration>, now: Instant) {
        self.failed_jit(server_wait, now, 1.0)
    }

    /// Keep the chunk last handed out and decide when to try again: a
    /// `Retry-After` clamped to [`MIN_BACKOFF`, `MAX_RETRY_AFTER`] (until
    /// `RA_TRUST_FAILURES` consecutive failures, then never below the exponential
    /// backoff), else exponential backoff times `factor` (jitter), never below
    /// `MIN_BACKOFF`. From `POISON_AFTER` failures, with other chunks waiting,
    /// the chunk moves behind them (order inside it unchanged) and cools down
    /// on its own, so the rest keeps draining.
    fn failed_jit(&self, server_wait: Option<Duration>, now: Instant, factor: f64) {
        let mut g = self.lock();
        let Some((s, len)) = g.current() else { return };
        g.items[s].fails = g.items[s].fails.saturating_add(1);
        let n = g.items[s].fails;
        let backoff = BASE_BACKOFF.saturating_mul(1u32 << (n - 1).min(8)).min(MAX_BACKOFF);
        let jittered = backoff.mul_f64(factor.clamp(JITTER_MIN, 1.0)).max(MIN_BACKOFF);
        let wait = match server_wait {
            Some(ra) if n < RA_TRUST_FAILURES => ra.clamp(MIN_BACKOFF, MAX_RETRY_AFTER),
            Some(ra) => ra.clamp(MIN_BACKOFF, MAX_RETRY_AFTER).max(jittered),
            None => jittered,
        };
        g.cur = None;
        if n >= POISON_AFTER && g.items.len() > len {
            let mut block = g.take(s, len);
            let gid = g.next_group;
            g.next_group += 1;
            for e in block.iter_mut() {
                e.group = gid;
            }
            block[0].cool_until = Some(now + wait);
            g.pending_bytes += block.iter().map(|e| e.bytes).sum::<usize>();
            g.items.extend(block);
            g.rotated_total += 1;
            // The failure that triggered the rotation still sets a wait for the whole
            // queue: the server's Retry-After, else the minimum backoff.
            let global = if server_wait.is_some() { wait } else { MIN_BACKOFF };
            g.not_before = Some(now + global);
            warn!(failures = n, items = len, wait_s = wait.as_secs(),
                "Chunk failed repeatedly; moved behind the others (kept), the rest keeps draining");
        } else {
            g.not_before = Some(now + wait);
        }
    }
}

/// Backoff jitter: a factor in [0.8, 1.0], from the random bytes of a UUID v4
/// (the `uuid` crate is already a dependency; no new crate).
fn jitter_factor() -> f64 {
    let id = uuid::Uuid::new_v4();
    let b = id.as_bytes();
    let x = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    JITTER_MIN + (1.0 - JITTER_MIN) * (x as f64 / u32::MAX as f64)
}

/// `Some(wait)` when the failure is worth retrying: network/timeout errors,
/// 5xx, 408, 425 and 429 (`wait` = the server's `Retry-After`). `None` for
/// everything else: those batches are parked (413 is first split, see [`flush`]).
fn retry_wait(e: &anyhow::Error) -> Option<Option<Duration>> {
    match e.downcast_ref::<HttpStatusError>() {
        None => Some(None),
        Some(h) if matches!(h.status, 408 | 425 | 429) || (500..600).contains(&h.status) => Some(h.retry_after_secs.map(Duration::from_secs)),
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

/// Held for the duration of a `flush`; dropping it (also when the future is
/// cancelled) lets the next call send.
struct FlushGuard<'a>(&'a PendingBuffer);

impl<'a> FlushGuard<'a> {
    fn enter(buf: &'a PendingBuffer) -> Option<Self> {
        let mut g = buf.lock();
        if g.flushing {
            return None;
        }
        g.flushing = true;
        Some(FlushGuard(buf))
    }
}

impl Drop for FlushGuard<'_> {
    fn drop(&mut self) {
        self.0.lock().flushing = false;
    }
}

/// What one request of a flush sent and how it ended.
pub struct Flushed {
    pub count: usize,
    pub types: Vec<String>,
    pub result: anyhow::Result<SubmitOutcome>,
    /// The batch stays buffered for a later attempt (retryable failure, or a 413 being split).
    #[allow(dead_code)] // callers rely on the buffer's own warn lines for kept batches
    pub kept: bool,
    /// Observations moved to the parked set by this request.
    pub parked: usize,
}

/// Queue `new` observations and send pending chunks, oldest first, while no
/// wait is in force: at most [`DRAIN_MAX_REQUESTS`] requests and [`DRAIN_BUDGET`]
/// of time per call. Empty result = no request was made. Effects per answer:
/// 2xx removes the chunk; a retryable failure keeps it, sets the wait and ends
/// the call; 413 halves the request size and resends at once (a single item is
/// parked); any other 4xx parks the chunk (a 401 also ends the call). Every
/// request, a 413 split included, counts toward the budget, and a call that
/// ends on a 413 holds the buffer for [`MIN_BACKOFF`].
///
/// One sender per buffer: a call made while another is in progress only queues
/// `new` and returns empty (the running call sends it).
pub async fn flush(
    buf: &PendingBuffer,
    client: &ApiClient,
    node_id: &str,
    new: Vec<Observation>,
    now: Instant,
) -> Vec<Flushed> {
    buf.push(new, now);
    let Some(_guard) = FlushGuard::enter(buf) else {
        warn!("flush re-entered; the new observations are queued, this call sends nothing");
        return Vec::new();
    };
    let started = Instant::now();
    let mut out = Vec::new();
    let (mut requests, mut steps) = (0usize, 0usize);
    let mut last_413 = false;
    while requests < DRAIN_MAX_REQUESTS && steps < 64 && started.elapsed() < DRAIN_BUDGET {
        steps += 1;
        last_413 = false;
        let Some(chunk) = buf.chunk(now + started.elapsed()) else { break };
        let count = chunk.len();
        let keys: Vec<&str> = chunk.iter().map(|(_, k)| k.as_str()).collect();
        let types = chunk.iter().map(|(o, _)| o.signal_type.clone()).collect();
        let body = KeyedRequest {
            node_id,
            observations: chunk.iter().map(|(o, k)| KeyedObservation { obs: o, client_dedupe_key: k }).collect(),
        };
        let result = client.submit_body(&body, count).await;
        let at = now + started.elapsed();
        let (mut kept, mut parked, mut stop) = (false, 0, false);
        match result.as_ref() {
            Ok(_) => {
                buf.done(&keys);
                requests += 1;
            }
            Err(e) => match retry_wait(e) {
                Some(wait) => {
                    buf.failed_jit(wait, at, jitter_factor());
                    warn!(status = %status_label(e), count, pending = buf.len(), "Observation submit failed; batch kept for retry");
                    kept = true;
                    requests += 1;
                }
                None => {
                    let status = e.downcast_ref::<HttpStatusError>().map_or(0, |h| h.status);
                    if status == 413 && count > 1 {
                        buf.shrink(count);
                        warn!(count, new_size = (count / 2).max(1), "Server answered 413; resending in smaller pieces");
                        kept = true;
                        last_413 = true;
                        requests += 1;
                    } else {
                        parked = buf.park(&keys, status);
                        if status == 413 {
                            buf.unshrink();
                            last_413 = true;
                        }
                        stop = crate::api_client::is_unauthorized(e);
                        requests += 1;
                    }
                }
            },
        }
        out.push(Flushed { count, types, result, kept, parked });
        if stop {
            break;
        }
    }
    if last_413 {
        buf.hold(now + started.elapsed() + MIN_BACKOFF);
    }
    out
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
        for secs in [5, 10, 20, 40, 80, 160, 300, 300] {
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

#[cfg(test)]
#[path = "pending_tests.rs"]
mod n8_tests;

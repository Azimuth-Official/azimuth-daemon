//! N8 hardening tests for the observation retry buffer. Each test drives the real
//! `pending::flush` against a local fake API with a synthetic clock.

use super::*;
use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue, StatusCode};

type Seen = Arc<Mutex<Vec<serde_json::Value>>>;

#[derive(Clone)]
struct Reply {
    code: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

fn ok_for(v: &serde_json::Value) -> Reply {
    let n = v["observations"].as_array().map_or(0, |a| a.len());
    Reply { code: 200, headers: vec![], body: format!(r#"{{"accepted":{n},"rejected":[]}}"#) }
}

fn status(code: u16) -> Reply {
    Reply { code, headers: vec![], body: "{}".into() }
}

fn status_ra(code: u16, ra: &str) -> Reply {
    Reply { code, headers: vec![("retry-after", ra.to_string())], body: "{}".into() }
}

async fn fake(reply: impl Fn(usize, &serde_json::Value) -> Reply + Send + Sync + 'static) -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let (rec, reply) = (Arc::clone(&seen), Arc::new(reply));
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let (rec, reply) = (Arc::clone(&rec), Arc::clone(&reply));
        async move {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let idx = {
                let mut g = rec.lock().unwrap();
                g.push(v.clone());
                g.len() - 1
            };
            let r = reply(idx, &v);
            let mut h = axum::http::HeaderMap::new();
            for (k, val) in &r.headers {
                h.insert(HeaderName::from_static(k), HeaderValue::from_str(val).unwrap());
            }
            (StatusCode::from_u16(r.code).unwrap(), h, r.body)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), seen)
}

fn client_to(base: &str) -> ApiClient {
    ApiClient::new(base, Arc::new(std::sync::RwLock::new("k".to_string())))
}

fn obs_n(n: u64) -> Observation {
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

fn many(range: std::ops::Range<u64>) -> Vec<Observation> {
    range.map(obs_n).collect()
}

fn big_obs(n: u64, kb: usize) -> Observation {
    let mut o = obs_n(n);
    let mut m = std::collections::HashMap::new();
    m.insert("blob".to_string(), serde_json::Value::String("x".repeat(kb * 1024)));
    o.raw_data = Some(m);
    o
}

fn freqs(v: &serde_json::Value) -> Vec<u64> {
    v["observations"].as_array().unwrap().iter().filter_map(|o| o["frequency_hz"].as_u64()).collect()
}

fn secs(base: Instant, s: u64) -> Instant {
    base + Duration::from_secs(s)
}

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogCapture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> LogCapture {
        self.clone()
    }
}

fn capture_logs() -> (LogCapture, tracing::subscriber::DefaultGuard) {
    let cap = LogCapture::default();
    let sub = tracing_subscriber::fmt().with_writer(cap.clone()).with_ansi(false).with_max_level(tracing::Level::INFO).finish();
    (cap, tracing::subscriber::set_default(sub))
}

fn lines_with(cap: &LogCapture, needle: &str) -> Vec<String> {
    String::from_utf8_lossy(&cap.0.lock().unwrap()).lines().filter(|l| l.contains(needle)).map(str::to_string).collect()
}

fn accounted(s: &PendingStats) -> usize {
    s.pending + s.parked + s.delivered + s.evicted_total + s.parked_evicted_total
}

// ---- item 1: drain rate ---------------------------------------------------------------------

#[tokio::test]
async fn item1_one_flush_drains_several_chunks() {
    let (base, seen) = fake(|_, v| ok_for(v)).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    let sent = flush(&buf, &client, "n1", many(0..350), t0).await;
    assert_eq!(buf.len(), 0, "350 items fit the per-cycle drain budget");
    assert_eq!(seen.lock().unwrap().len(), 4);
    assert_eq!(sent.len(), 4);
}

#[tokio::test]
async fn item1_drain_is_bounded_per_cycle() {
    let (base, seen) = fake(|_, v| ok_for(v)).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..2000), t0).await;
    assert_eq!(seen.lock().unwrap().len(), DRAIN_MAX_REQUESTS, "never more than the per-cycle request budget");
    assert_eq!(buf.len(), 2000 - 100 * DRAIN_MAX_REQUESTS);
}

// ---- item 2: 413 split ----------------------------------------------------------------------

#[tokio::test]
async fn item2_413_halves_the_chunk_and_everything_is_delivered() {
    let (base, seen) = fake(|_, v| if v["observations"].as_array().unwrap().len() > 30 { status(413) } else { ok_for(v) }).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    for c in 0..6 {
        flush(&buf, &client, "n1", if c == 0 { many(0..100) } else { vec![] }, secs(t0, c * 31)).await;
    }
    let stored: Vec<u64> = seen.lock().unwrap().iter().filter(|v| v["observations"].as_array().unwrap().len() <= 30).flat_map(freqs).collect();
    assert_eq!(stored, (0..100).collect::<Vec<_>>(), "all items delivered, in order, in pieces of at most 30");
    let s = buf.stats();
    assert_eq!((s.parked, s.pending), (0, 0));
}

#[tokio::test]
async fn item2_single_item_413_is_parked_and_the_rest_flows() {
    let (base, seen) = fake(|_, v| if freqs(v).contains(&7) { status(413) } else { ok_for(v) }).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    for c in 0..4 {
        flush(&buf, &client, "n1", if c == 0 { many(0..10) } else { vec![] }, secs(t0, c * 31)).await;
    }
    let s = buf.stats();
    assert_eq!((s.pending, s.parked, s.delivered), (0, 1, 9), "{s:?}");
    let sent_ok: usize = seen.lock().unwrap().iter().filter(|v| !freqs(v).contains(&7)).map(|v| freqs(v).len()).sum();
    assert_eq!(sent_ok, 9);
}

#[tokio::test]
async fn item2_a_poison_item_does_not_leave_the_request_size_lowered() {
    let (base, seen) = fake(|_, v| if freqs(v).contains(&7) { status(413) } else { ok_for(v) }).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..10), t0).await; // the split ends with item 7 parked
    assert_eq!(buf.stats().parked, 1);
    seen.lock().unwrap().clear();
    flush(&buf, &client, "n1", many(100..300), secs(t0, 10)).await;
    let sizes: Vec<usize> = seen.lock().unwrap().iter().map(|v| freqs(v).len()).collect();
    assert_eq!(sizes[0], 100, "the 413 was one item's fault, not the server cap: full-size requests again ({sizes:?})");
}

#[tokio::test]
async fn item2_a_lowered_request_size_recovers_after_a_run_of_successes() {
    let (base, seen) = fake(|_, v| if v["observations"].as_array().unwrap().len() > 10 { status(413) } else { ok_for(v) }).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    for c in 0..12u64 {
        flush(&buf, &client, "n1", many(c * 100..c * 100 + 100), secs(t0, c)).await;
    }
    let sizes: Vec<usize> = seen.lock().unwrap().iter().map(|v| v["observations"].as_array().unwrap().len()).collect();
    assert!(sizes.iter().filter(|s| **s > 10).count() >= 2, "it probes the larger size again after a run of successes: {sizes:?}");
    assert_eq!(buf.stats().parked, 0);
}

// ---- item 3: byte bound, counted eviction, exactly one warn per event ----------------------

#[tokio::test]
async fn item3_byte_bound_evicts_the_oldest_counts_and_warns_once() {
    let (cap, _g) = capture_logs();
    let (buf, t0) = (PendingBuffer::default(), Instant::now());
    let n = 1200u64; // 1200 x 10 KiB = 12 MiB > MAX_PENDING_BYTES
    let dropped = buf.push((0..n).map(|i| big_obs(i, 10)).collect(), t0);
    let s = buf.stats();
    assert!(dropped > 0 && s.evicted_total == dropped, "{s:?}");
    assert!(s.pending_bytes <= MAX_PENDING_BYTES, "{s:?}");
    assert_eq!(buf.chunk(t0).unwrap()[0].0.frequency_hz, Some(dropped as u64), "the OLDEST were evicted");
    assert_eq!(lines_with(&cap, "dropped=").len(), 1, "one warn per eviction event");
}

#[tokio::test]
async fn item3_count_eviction_is_counted_and_warns_once() {
    let (cap, _g) = capture_logs();
    let (buf, t0) = (PendingBuffer::default(), Instant::now());
    assert_eq!(buf.push(many(0..MAX_PENDING_OBSERVATIONS as u64 + 7), t0), 7);
    assert_eq!(buf.stats().evicted_total, 7);
    assert_eq!(lines_with(&cap, "dropped=7").len(), 1);
    assert_eq!(buf.push(many(0..3), t0), 3);
    assert_eq!(buf.stats().evicted_total, 10);
    assert_eq!(lines_with(&cap, "dropped=").len(), 2, "second event, second warn");
}

#[tokio::test]
async fn item3_age_eviction_is_counted_and_warns_once() {
    let (cap, _g) = capture_logs();
    let (buf, t0) = (PendingBuffer::default(), Instant::now());
    buf.push(many(0..5), t0);
    assert_eq!(buf.push(many(5..6), t0 + MAX_PENDING_AGE + Duration::from_secs(1)), 5);
    assert_eq!(buf.stats().evicted_total, 5);
    assert_eq!(lines_with(&cap, "dropped=5").len(), 1);
}

#[tokio::test]
async fn item3_parked_set_bound_counts_and_warns_once_per_event() {
    let (cap, _g) = capture_logs();
    let (base, _seen) = fake(|_, _| status(400)).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..1200), t0).await;
    flush(&buf, &client, "n1", vec![], secs(t0, 1)).await;
    let s = buf.stats();
    assert_eq!(s.parked, PARKED_MAX_ITEMS, "{s:?}");
    assert_eq!(s.parked_evicted_total, 200, "{s:?}");
    assert_eq!(lines_with(&cap, "Parked observations over bound").len(), 2, "two park operations overflowed");
    assert_eq!(accounted(&s), 1200);
}

// ---- item 4: retry classes and Retry-After forms -------------------------------------------

#[tokio::test]
async fn item4_408_and_425_are_kept_and_resent() {
    for code in [408u16, 425] {
        let (base, seen) = fake(move |i, v| if i == 0 { status(code) } else { ok_for(v) }).await;
        let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
        flush(&buf, &client, "n1", many(0..2), t0).await;
        assert_eq!((buf.len(), buf.stats().parked), (2, 0), "{code}: kept, not parked");
        flush(&buf, &client, "n1", vec![], secs(t0, 40)).await;
        assert_eq!(buf.len(), 0, "{code}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn item4_http_date_retry_after_is_honoured() {
    let when = (chrono::Utc::now() + chrono::Duration::seconds(90)).format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let (base, seen) = fake(move |i, v| if i == 0 { status_ra(503, &when) } else { ok_for(v) }).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..1), t0).await;
    flush(&buf, &client, "n1", vec![], secs(t0, 40)).await;
    assert_eq!(seen.lock().unwrap().len(), 1, "40 s in, the 90 s date is still in the future");
    flush(&buf, &client, "n1", vec![], secs(t0, 100)).await;
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn item4_retry_after_parser_reads_both_forms() {
    let now = chrono::DateTime::parse_from_rfc2822("Sun, 06 Nov 1994 08:49:37 GMT").unwrap().with_timezone(&chrono::Utc);
    assert_eq!(crate::api_client::parse_retry_after("120", now), Some(120));
    assert_eq!(crate::api_client::parse_retry_after(" 7 ", now), Some(7));
    assert_eq!(crate::api_client::parse_retry_after("Sun, 06 Nov 1994 08:50:37 GMT", now), Some(60));
    assert_eq!(crate::api_client::parse_retry_after("Sun, 06 Nov 1994 08:00:00 GMT", now), Some(0), "a date in the past");
    assert_eq!(crate::api_client::parse_retry_after("soon", now), None);
    assert_eq!(crate::api_client::parse_retry_after("-5", now), None);
}

// ---- item 5: head-of-line blocking ----------------------------------------------------------

#[tokio::test]
async fn item5_a_poisoned_chunk_is_moved_behind_the_others_and_kept() {
    let (base, seen) = fake(|_, v| if freqs(v).first() == Some(&0) { status(503) } else { ok_for(v) }).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..300), t0).await;
    let mut now_s = 0;
    while now_s < 400 {
        now_s += 10;
        flush(&buf, &client, "n1", vec![], secs(t0, now_s)).await;
    }
    let s = buf.stats();
    assert_eq!((s.delivered, s.pending, s.parked), (200, 100, 0), "the two healthy chunks went through: {s:?}");
    assert!(s.rotated_total >= 1, "{s:?}");
    let poison_tries = seen.lock().unwrap().iter().filter(|v| freqs(v).first() == Some(&0)).count();
    assert!(poison_tries <= 8, "no tight loop on the poisoned chunk: {poison_tries} tries in 400 s");
}

#[tokio::test]
async fn item5_rotation_keeps_chunk_order_and_does_not_reset_the_backoff() {
    let buf = PendingBuffer::default();
    let t0 = Instant::now();
    buf.push(many(0..300), t0);
    // three failures of the first chunk in a row
    for k in 0..POISON_AFTER {
        let c = buf.chunk(secs(t0, 1000 * k as u64)).expect("chunk offered after its wait");
        assert_eq!(c[0].0.frequency_hz, Some(0));
        buf.failed_jit(None, secs(t0, 1000 * k as u64), 1.0);
    }
    let now = secs(t0, 1000 * (POISON_AFTER as u64 - 1)); // the moment of the rotation
    let second = buf.chunk(now + MIN_BACKOFF).expect("the healthy chunk is next, after the minimum wait");
    assert_eq!(second[0].0.frequency_hz, Some(100), "the poisoned chunk went behind the others");
    // drain the healthy chunks
    let keys: Vec<String> = second.iter().map(|(_, k)| k.clone()).collect();
    buf.done(&keys.iter().map(String::as_str).collect::<Vec<_>>());
    let third = buf.chunk(now).unwrap();
    assert_eq!(third[0].0.frequency_hz, Some(200));
    let keys: Vec<String> = third.iter().map(|(_, k)| k.clone()).collect();
    buf.done(&keys.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(buf.chunk(now).is_none(), "the rotated chunk is still cooling down, not retried at once");
    let back = buf.chunk(now + MAX_BACKOFF).expect("offered again after its backoff");
    assert_eq!(back.iter().filter_map(|(o, _)| o.frequency_hz).collect::<Vec<_>>(), (0..100).collect::<Vec<_>>(), "order inside the chunk unchanged");
    buf.failed_jit(None, now + MAX_BACKOFF, 1.0);
    assert!(buf.chunk(now + MAX_BACKOFF + Duration::from_secs(30)).is_none(), "the failure count continued (4th failure = 40 s), it did not restart at the 5 s base");
}

// ---- item 6: hostile server -----------------------------------------------------------------

#[tokio::test]
async fn item6_retry_after_zero_cannot_make_the_client_hammer() {
    let (base, seen) = fake(|_, _| status_ra(503, "0")).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..1), t0).await;
    for s in 1..600 {
        flush(&buf, &client, "n1", vec![], secs(t0, s)).await;
    }
    let n = seen.lock().unwrap().len();
    assert!(n <= 12, "{n} requests in 10 minutes against a server that always says Retry-After: 0");
    assert!(n >= 4, "but it keeps trying: {n}");
    assert_eq!(buf.len(), 1, "and the item is kept");
}

#[tokio::test]
async fn item6_first_retry_after_zero_is_floored() {
    let (base, seen) = fake(|_, _| status_ra(503, "0")).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..1), t0).await;
    flush(&buf, &client, "n1", vec![], secs(t0, 2)).await;
    assert_eq!(seen.lock().unwrap().len(), 1, "2 s later is under the {MIN_BACKOFF:?} floor");
}

// ---- item 7: jitter -------------------------------------------------------------------------

#[test]
fn item7_jitter_lowers_a_backoff_by_at_most_a_fifth() {
    // fourth consecutive failure: the unjittered wait is 40 s
    for (f, floor_s) in [(1.0, 40u64), (0.8, 32)] {
        let buf = PendingBuffer::default();
        let t0 = Instant::now();
        buf.push(many(0..1), t0);
        for k in 0..3u64 {
            buf.chunk(secs(t0, 1000 * k)).expect("offered");
            buf.failed_jit(None, secs(t0, 1000 * k), 1.0);
        }
        let t = secs(t0, 5000);
        buf.chunk(t).expect("offered");
        buf.failed_jit(None, t, f);
        assert!(buf.chunk(t + Duration::from_secs(floor_s - 1)).is_none(), "{f}");
        assert!(buf.chunk(t + Duration::from_secs(floor_s)).is_some(), "{f}");
    }
}

#[test]
fn item7_production_jitter_varies_and_stays_in_range() {
    let v: Vec<f64> = (0..200).map(|_| jitter_factor()).collect();
    assert!(v.iter().all(|f| (0.8..=1.0).contains(f)), "{v:?}");
    assert!(v.iter().any(|f| (*f - v[0]).abs() > 1e-6), "all 200 samples identical");
}

// ---- park policy ---------------------------------------------------------------------------

#[tokio::test]
async fn park_non_retryable_4xx_are_parked_visibly_never_resent() {
    for code in [400u16, 401, 403, 404, 409, 422, 418] {
        let (cap, _g) = capture_logs();
        let (base, seen) = fake(move |i, v| if i == 0 { status(code) } else { ok_for(v) }).await;
        let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
        flush(&buf, &client, "n1", many(0..3), t0).await;
        let s = buf.stats();
        assert_eq!((s.pending, s.parked, s.parked_total), (0, 3, 3), "{code}: {s:?}");
        let l = lines_with(&cap, "Observations parked");
        assert_eq!(l.len(), 1, "{code}: exactly one park warn");
        assert!(l[0].contains(&code.to_string()) && l[0].contains("4xx") && l[0].contains("items=3"), "{code}: {}", l[0]);
        assert!(!l[0].contains("fm_rds"), "no payload in the log");
        flush(&buf, &client, "n1", many(10..11), secs(t0, 1)).await;
        assert!(seen.lock().unwrap().iter().skip(1).all(|v| !freqs(v).contains(&0)), "{code}: the parked batch is never resent");
    }
}

#[tokio::test]
async fn conservation_every_status_class_accounts_for_every_item() {
    #[derive(Clone, Copy)]
    enum Case {
        Status(u16),
        Network,
    }
    let cases = [200u16, 400, 401, 403, 404, 408, 409, 413, 422, 425, 429, 500, 503].map(Case::Status).into_iter().chain([Case::Network]);
    for case in cases {
        let (base, _seen) = match case {
            Case::Status(c) => fake(move |_, v| if c == 200 { ok_for(v) } else { status(c) }).await,
            Case::Network => {
                let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                (format!("http://{}", l.local_addr().unwrap()), Arc::new(Mutex::new(vec![])))
            }
        };
        let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
        let mut submitted = 0;
        for c in 0..40u64 {
            let new = if c % 5 == 0 { many(submitted..submitted + 120) } else { vec![] };
            submitted += new.len() as u64;
            flush(&buf, &client, "n1", new, secs(t0, c * 20)).await;
            let s = buf.stats();
            assert_eq!(accounted(&s) as u64, submitted, "cycle {c}: {s:?}");
        }
        let s = buf.stats();
        if let Case::Status(200) = case {
            assert_eq!(s.delivered as u64, submitted, "{s:?}");
        }
        if let Case::Status(c) = case {
            if c != 200 && !(c == 408 || c == 425 || c == 429 || c >= 500) {
                assert_eq!(s.delivered, 0, "status {c} never counts as delivered");
            }
        }
    }
}

// ---- harness: retry with server dedupe applied and disabled --------------------------------

/// A server that stores observations. With `dedupe` it applies the production rule
/// (unique on node_id + client_dedupe_key, `ON CONFLICT DO NOTHING`).
#[derive(Default)]
struct Store {
    rows: Vec<(String, Option<String>)>,
}

fn store_batch(store: &Mutex<Store>, v: &serde_json::Value, dedupe: bool) {
    let node = v["node_id"].as_str().unwrap().to_string();
    let mut g = store.lock().unwrap();
    for o in v["observations"].as_array().unwrap() {
        let key = o["client_dedupe_key"].as_str().map(str::to_string);
        if dedupe && key.is_some() && g.rows.iter().any(|(n, k)| *n == node && *k == key) {
            continue;
        }
        g.rows.push((node.clone(), key));
    }
}

async fn run_harness(dedupe: bool, commit_before_failing: bool) -> (usize, usize, usize) {
    let store = Arc::new(Mutex::new(Store::default()));
    let st = Arc::clone(&store);
    let (base, _seen) = fake(move |i, v| match i {
        0 => {
            if commit_before_failing {
                store_batch(&st, v, dedupe);
            }
            status(500)
        }
        1 => {
            if commit_before_failing {
                store_batch(&st, v, dedupe);
            }
            status_ra(503, "9")
        }
        _ => {
            store_batch(&st, v, dedupe);
            Reply { code: 201, headers: vec![], body: r#"{"accepted":5,"rejected":[]}"#.into() }
        }
    })
    .await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..5), t0).await;
    flush(&buf, &client, "n1", vec![], secs(t0, 40)).await;
    flush(&buf, &client, "n1", vec![], secs(t0, 60)).await;
    let rows = store.lock().unwrap().rows.clone();
    let unique: std::collections::HashSet<_> = rows.iter().cloned().collect();
    let lost = 5usize.saturating_sub(unique.len());
    let dup = rows.len() - unique.len();
    (unique.len(), lost, dup)
}

#[tokio::test]
async fn harness_500_then_503_retry_after_then_201_stores_the_batch_once() {
    for dedupe in [true, false] {
        let (stored, lost, dup) = run_harness(dedupe, false).await;
        println!("HARNESS dedupe={dedupe} stored={stored} lost={lost} dup={dup}");
        assert_eq!((stored, lost, dup), (5, 0, 0), "dedupe={dedupe}: the client does not rely on the server rule when failures did not commit");
    }
}

#[tokio::test]
async fn harness_responses_lost_after_commit_are_covered_by_the_server_rule() {
    let (stored, lost, dup) = run_harness(true, true).await;
    println!("HARNESS lost-response dedupe=true stored={stored} lost={lost} dup={dup}");
    assert_eq!((stored, lost, dup), (5, 0, 0));
    let (stored, lost, dup) = run_harness(false, true).await;
    println!("HARNESS lost-response dedupe=false stored={stored} lost={lost} dup={dup}");
    assert_eq!((stored, lost), (5, 0), "never lost");
    assert_eq!(dup, 10, "without the server rule a committed-then-failed batch is stored again; the stable key is what lets the server collapse it");
}

// ---- round 2: hostile-server hardening -----------------------------------------------------

fn status_body(code: u16, body: String) -> Reply {
    Reply { code, headers: vec![], body }
}

#[tokio::test]
async fn r2_413_to_everything_is_bounded_per_flush_and_then_waits() {
    let (base, seen) = fake(|_, _| status(413)).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..300), t0).await;
    let n = seen.lock().unwrap().len();
    assert!(n <= DRAIN_MAX_REQUESTS, "{n} requests in one flush against a server that answers 413 to everything");
    flush(&buf, &client, "n1", vec![], secs(t0, 1)).await;
    assert_eq!(seen.lock().unwrap().len(), n, "no request within 1 s of the last 413");
    flush(&buf, &client, "n1", vec![], secs(t0, 7)).await;
    assert!(seen.lock().unwrap().len() > n, "it keeps trying after the {MIN_BACKOFF:?} wait");
    let s = buf.stats();
    assert_eq!(accounted(&s), 300, "{s:?}");
}

#[tokio::test]
async fn r2_a_rotation_honours_the_retry_after_that_triggered_it() {
    let (base, seen) = fake(|_, v| if freqs(v).first() == Some(&0) { status_ra(503, "120") } else { ok_for(v) }).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..300), t0).await;
    flush(&buf, &client, "n1", vec![], secs(t0, 121)).await;
    let before = seen.lock().unwrap().len();
    flush(&buf, &client, "n1", vec![], secs(t0, 242)).await; // third failure: the chunk is rotated
    assert_eq!(seen.lock().unwrap().len(), before + 1, "no extra request right after a 503 with Retry-After: 120");
    flush(&buf, &client, "n1", vec![], secs(t0, 242 + 60)).await;
    assert_eq!(seen.lock().unwrap().len(), before + 1, "still inside the 120 s the server asked for");
    flush(&buf, &client, "n1", vec![], secs(t0, 242 + 121)).await;
    assert!(seen.lock().unwrap().len() > before + 1, "after it, the other chunks go");
}

#[tokio::test]
async fn r2_a_huge_hostile_body_is_bounded_and_clean_in_the_error() {
    let body = format!("line1\nline2\x1b[31m\r\x07{}", "x".repeat(10 * 1024 * 1024));
    let (base, _seen) = fake(move |_, _| status_body(500, body.clone())).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    let sent = flush(&buf, &client, "n1", many(0..1), t0).await;
    let e = sent[0].result.as_ref().err().expect("error").to_string();
    assert!(e.len() <= 600, "Display is bounded: {} bytes", e.len());
    assert!(!e.chars().any(|c| c.is_control()), "no control characters: {e:?}");
    assert!(e.contains("line1"), "{e:?}");
    let h = sent[0].result.as_ref().err().unwrap().downcast_ref::<HttpStatusError>().unwrap();
    assert!(h.body.len() <= 4200 && !h.body.chars().any(|c| c.is_control()), "stored body bounded and clean: {}", h.body.len());
}

#[tokio::test]
async fn r2_flush_is_single_caller_a_second_concurrent_call_only_queues() {
    let (base, seen) = fake(|_, v| ok_for(v)).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    let (a, b) = tokio::join!(flush(&buf, &client, "n1", many(0..100), t0), flush(&buf, &client, "n1", many(100..101), t0));
    assert!(b.is_empty(), "the re-entrant call made no request");
    assert!(!a.is_empty());
    let all: Vec<u64> = seen.lock().unwrap().iter().flat_map(freqs).collect();
    assert_eq!(all, (0..101).collect::<Vec<_>>(), "every observation sent exactly once, in order");
    assert_eq!(buf.len(), 0);
}

#[tokio::test]
async fn r2_park_line_says_held_in_memory_and_not_resent() {
    let (cap, _g) = capture_logs();
    let (base, _seen) = fake(|_, _| status(400)).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..2), t0).await;
    let l = lines_with(&cap, "Observations parked");
    assert_eq!(l.len(), 1);
    assert!(l[0].contains("held in memory") && l[0].contains("NOT resent") && l[0].contains("restart"), "{}", l[0]);
}

// ---- round 2 addendum: backoff ladder 5, 10, 20 ... 300 s --------------------------------------

#[test]
fn r2_backoff_ladder_starts_at_the_floor_and_doubles_to_the_cap() {
    let buf = PendingBuffer::default();
    let t0 = Instant::now();
    buf.push(many(0..1), t0);
    let mut t = 0u64;
    for wait in [5u64, 10, 20, 40, 80, 160, 300, 300, 300] {
        assert!(buf.chunk(secs(t0, t)).is_some(), "offered at t={t}");
        buf.failed_jit(None, secs(t0, t), 1.0);
        assert!(buf.chunk(secs(t0, t + wait - 1)).is_none(), "wait {wait}");
        assert!(buf.chunk(secs(t0, t + wait)).is_some(), "wait {wait}");
        t += wait;
    }
}

#[test]
fn r2_jitter_never_goes_under_the_floor() {
    let buf = PendingBuffer::default();
    let t0 = Instant::now();
    buf.push(many(0..1), t0);
    buf.failed_jit(None, t0, 0.8);
    assert!(buf.chunk(secs(t0, 4)).is_none());
    assert!(buf.chunk(secs(t0, 5)).is_some());
}

#[test]
fn r2_the_ladder_restarts_after_a_delivery() {
    let buf = PendingBuffer::default();
    let t0 = Instant::now();
    buf.push(many(0..1), t0);
    for k in 0..4 {
        buf.chunk(secs(t0, 1000 * k)).expect("offered");
        buf.failed_jit(None, secs(t0, 1000 * k), 1.0);
    }
    let c = buf.chunk(secs(t0, 5000)).unwrap(); // the fifth attempt succeeds
    buf.done(&c.iter().map(|(_, k)| k.as_str()).collect::<Vec<_>>());
    buf.push(many(1..2), secs(t0, 5000));
    buf.chunk(secs(t0, 5000)).unwrap();
    buf.failed_jit(None, secs(t0, 5000), 1.0);
    assert!(buf.chunk(secs(t0, 5004)).is_none());
    assert!(buf.chunk(secs(t0, 5005)).is_some(), "first failure after a 2xx waits the floor again, not 80 s");
}

#[tokio::test]
async fn r2_transient_faults_cost_seconds_not_half_a_minute() {
    // 503 for the first two requests, then 200: the batch is delivered within 20 s of the first send.
    let (base, seen) = fake(|i, v| if i < 2 { status(503) } else { ok_for(v) }).await;
    let (buf, client, t0) = (PendingBuffer::default(), client_to(&base), Instant::now());
    flush(&buf, &client, "n1", many(0..3), t0).await;
    flush(&buf, &client, "n1", vec![], secs(t0, 6)).await;
    flush(&buf, &client, "n1", vec![], secs(t0, 17)).await;
    assert_eq!(buf.len(), 0, "{} requests", seen.lock().unwrap().len());
}

// ---- Z4 review follow-ups (pending.rs data-affecting SHOULDs) ----

fn z4_keys(buf: &PendingBuffer) -> Vec<String> {
    buf.lock().items.iter().map(|e| e.key.clone()).collect()
}

/// D1: whatever pushes, evictions and rotations happen while a chunk is in flight,
/// `done(chunk)` may remove only the keys of that chunk, never an unsent neighbour.
#[test]
fn z4_d1_done_removes_only_the_keys_of_the_chunk() {
    let mut total = (0usize, 0usize, 0usize);
    for seed in 1..=40u64 {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut rnd = move |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        let buf = PendingBuffer::default();
        let mut now = Instant::now();
        let mut n = 0u64;
        let mut inflight: Option<Vec<String>> = None;
        for _ in 0..4000 {
            match rnd(6) {
                0 | 1 => {
                    let k = 1 + rnd(30);
                    buf.push((n..n + k).map(obs_n).collect(), now);
                    n += k;
                }
                2 => {
                    now += Duration::from_secs(rnd(400));
                    buf.push(vec![obs_n(n)], now);
                    n += 1;
                }
                3 => {
                    if inflight.is_none() {
                        inflight = buf.chunk(now).map(|c| c.into_iter().map(|(_, k)| k).collect());
                    }
                }
                4 => {
                    // a failed attempt of the current chunk (may rotate it behind the others)
                    if inflight.take().is_some() {
                        buf.failed(None, now);
                    }
                    now += Duration::from_secs(rnd(120));
                }
                _ => {
                    if let Some(keys) = inflight.take() {
                        let before = z4_keys(&buf);
                        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
                        buf.done(&refs);
                        let after = z4_keys(&buf);
                        for r in before.iter().filter(|k| !after.contains(k)) {
                            assert!(keys.contains(r), "seed {seed}: done() removed an unsent entry {r}");
                        }
                    }
                }
            }
        }
        // Control: the run really exercised delivery, age/count eviction and rotation.
        let s = buf.stats();
        total = (total.0 + s.delivered, total.1 + s.evicted_total, total.2 + s.rotated_total);
    }
    assert!(total.0 > 0 && total.1 > 0 && total.2 > 0, "scenario too weak: {total:?}");
}

fn z4_fails(buf: &PendingBuffer, key: &str) -> u32 {
    buf.lock().items.iter().find(|e| e.key == key).map_or(u32::MAX, |e| e.fails)
}

/// D3: an eviction between `chunk()` and `failed()` must not move the failure onto the
/// front chunk; the chunk that was actually sent (here not the front one) takes it.
#[test]
fn z4_d3_failure_after_an_eviction_is_charged_to_the_sent_chunk() {
    let buf = PendingBuffer::default();
    let t0 = Instant::now();
    let mut now = t0 + Duration::from_secs(20 * 60);
    buf.push(vec![obs_n(1)], now);
    buf.shrink(2); // one observation per request
    // P alone fails five times (no rotation without other chunks waiting).
    let p = buf.chunk(now).unwrap()[0].1.clone();
    buf.failed(None, now);
    for _ in 0..4 {
        now += Duration::from_secs(400);
        assert_eq!(buf.chunk(now).unwrap()[0].1, p);
        buf.failed(None, now);
    }
    // Q arrives; P's sixth failure rotates it behind Q with a long cooldown.
    buf.push(vec![obs_n(2)], now);
    now += Duration::from_secs(400);
    assert_eq!(buf.chunk(now).unwrap()[0].1, p);
    buf.failed(None, now);
    // Q fails three times, the third rotates it behind P; P is still cooling.
    let mut q = String::new();
    for gap in [6u64, 6, 11] {
        now += Duration::from_secs(gap);
        let c = buf.chunk(now).expect("Q ready");
        q = c[0].1.clone();
        assert_ne!(q, p);
        buf.failed(None, now);
    }
    now += Duration::from_secs(21);
    // Now P is cooling (long) and Q is ready; V (old) sits behind them.
    buf.push(vec![obs_n(3)], t0);
    let sent = buf.chunk(now).expect("a ready chunk");
    assert_eq!(sent[0].1, q, "Q is the ready chunk although P is in front");
    let (p_before, q_before) = (z4_fails(&buf, &p), z4_fails(&buf, &q));
    // a later push evicts the old V by age, which clears the in-flight marker
    let evicted = buf.push(vec![obs_n(4)], t0 + Duration::from_secs(31 * 60));
    assert!(evicted >= 1, "the old entry must be age-evicted");
    buf.failed(None, now);
    assert_eq!(z4_fails(&buf, &p), p_before, "front chunk P was not the one sent");
    assert_eq!(z4_fails(&buf, &q), q_before + 1, "the sent chunk Q takes the failure");
}

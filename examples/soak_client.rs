//! DEV ONLY (never part of a release tarball): drives the daemon's real retry
//! code (`pending.rs` + `api_client.rs`, included verbatim like `tune_stress`
//! includes the SDR code) against `mock_ingest`, and prints the soak verdict.
//!
//! The release daemon only produces observations from an SDR, so a soak on a
//! host without hardware runs this driver in its place: same flush, same
//! buffer, same HTTP client, unique ids, no SDR.
//!
//! cargo run --release --example soak_client -- --help

#![allow(dead_code)]

#[path = "../src/observation.rs"]
mod observation;
#[path = "../src/api_client.rs"]
mod api_client;
#[path = "../src/pending.rs"]
mod pending;

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn num<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    arg(args, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn obs(id: u64) -> observation::Observation {
    observation::Observation {
        signal_type: "fm_rds".into(),
        observed_at: chrono::Utc::now(),
        frequency_hz: Some(id),
        timestamp_ns: None,
        tdoa_offset_ns: None,
        signal_strength_dbm: None,
        snr_db: None,
        source_id: None,
        raw_data: None,
    }
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.iter().any(|x| x == "--help") {
        println!("soak_client --url http://mock:8080 [--duration-secs 1800] [--per-cycle 20] [--cycle-ms 1000]\n            [--drain-secs 600] [--node-id soak-node]");
        return;
    }
    let url = arg(&a, "--url").expect("--url is required");
    let (duration, per_cycle, cycle_ms, drain) = (
        num(&a, "--duration-secs", 1800u64),
        num(&a, "--per-cycle", 20u64),
        num(&a, "--cycle-ms", 1000u64),
        num(&a, "--drain-secs", 600u64),
    );
    let node = arg(&a, "--node-id").unwrap_or_else(|| "soak-node".into());
    tracing_subscriber::fmt().with_env_filter("warn").with_target(false).init();

    let client = api_client::ApiClient::new(&url, Arc::new(RwLock::new("soak".to_string())));
    let buf = pending::PendingBuffer::default();
    let start = Instant::now();
    let mut next_id = 0u64;
    let (mut max_pending, mut first_eviction): (usize, Option<u64>) = (0, None);
    loop {
        let e = start.elapsed().as_secs();
        let generating = e < duration;
        let new: Vec<_> = if generating { (next_id..next_id + per_cycle).map(obs).collect() } else { vec![] };
        next_id += new.len() as u64;
        pending::flush(&buf, &client, &node, new, Instant::now()).await;
        let st = buf.stats();
        max_pending = max_pending.max(st.pending);
        if first_eviction.is_none() && st.evicted_total > 0 {
            first_eviction = Some(e);
        }
        if !generating && (buf.len() == 0 || e > duration + drain) {
            break;
        }
        if e % 60 == 0 && e > 0 {
            let s = buf.stats();
            println!("progress t={e}s sent={next_id} pending={} parked={} delivered={}", s.pending, s.parked, s.delivered);
        }
        tokio::time::sleep(Duration::from_millis(cycle_ms)).await;
    }
    let s = buf.stats();
    let server: serde_json::Value = reqwest::get(format!("{url}/stats?n={next_id}")).await.expect("stats").json().await.expect("stats json");
    let stored = server["stored_unique"].as_i64().unwrap_or(-1);
    let dup = server["dup_rows"].as_i64().unwrap_or(-1);
    let lost = next_id as i64 - stored - (s.parked + s.evicted_total + s.parked_evicted_total + s.pending) as i64;
    println!("SOAK sent={next_id} stored={stored} lost={lost} dup={dup} parked={}", s.parked);
    println!(
        "SOAK-DETAIL pending={} delivered={} parked_total={} evicted={} parked_evicted={} rotated={} server_requests={} by_status={}",
        s.pending, s.delivered, s.parked_total, s.evicted_total, s.parked_evicted_total, s.rotated_total, server["requests"], server["by_status"]
    );
    let total = start.elapsed().as_secs_f64().max(1.0);
    println!(
        "SOAK-RATE produced_per_s={:.1} delivered_per_s={:.1} max_pending={} first_eviction_t={}",
        next_id as f64 / (duration.max(1) as f64),
        s.delivered as f64 / total,
        max_pending,
        first_eviction.map_or("none".to_string(), |t| format!("{t}s"))
    );
    // Exit 0 only when nothing is unaccounted for, nothing is pending, and the server stored nothing twice.
    if s.pending > 0 {
        println!("SOAK-NOTE not drained: {} observations still pending at the end", s.pending);
    }
    std::process::exit(if lost == 0 && dup == 0 && s.pending == 0 { 0 } else { 1 });
}

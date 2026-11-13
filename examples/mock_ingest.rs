//! DEV ONLY (never part of a release tarball): a mock of the observations
//! ingest route for the retry soak.
//!
//! * `POST /api/observations` stores observations. The dedupe rule is the
//!   production one: unique on (node_id, client_dedupe_key), `ON CONFLICT DO
//!   NOTHING`; `--dedupe off` disables it.
//! * Seeded random faults: 500, 503 (Retry-After in four shapes: 0, delta,
//!   HTTP-date, none), 429, 408, 413 above `--cap` items; `--lost-rate` commits
//!   the batch and then answers 503 (a response lost after the commit);
//!   `--poison-mod K` makes any request holding an id with `id % K == 7` fail
//!   (413 when it holds several items, 422 when it holds one).
//! * `GET /stats?n=N` reports what was stored for ids 0..N (the id is the
//!   observation's `frequency_hz`); `GET /health` answers 200.
//!
//! cargo run --release --example mock_ingest -- --help

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{body::Bytes, Router};

struct Cfg {
    bind: String,
    seed: u64,
    p500: f64,
    p503: f64,
    p429: f64,
    p408: f64,
    cap: usize,
    dedupe: bool,
    lost: f64,
    poison_mod: u64,
}

#[derive(Default)]
struct Db {
    rng: u64,
    keys: HashSet<(String, String)>,
    ids: HashMap<u64, u32>,
    rows: u64,
    requests: u64,
    by_status: HashMap<u16, u64>,
}

struct App {
    cfg: Cfg,
    db: Mutex<Db>,
}

fn next(rng: &mut u64) -> f64 {
    // xorshift64*
    let mut x = *rng;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *rng = x;
    (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn num<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    arg(args, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn reply(db: &mut Db, code: u16, headers: HeaderMap, body: String) -> axum::response::Response {
    *db.by_status.entry(code).or_default() += 1;
    (StatusCode::from_u16(code).unwrap(), headers, body).into_response()
}

async fn ingest(State(app): State<Arc<App>>, body: Bytes) -> axum::response::Response {
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let mut db = app.db.lock().unwrap();
    db.requests += 1;
    let (Some(node), Some(obs)) = (v["node_id"].as_str(), v["observations"].as_array()) else {
        return reply(&mut db, 400, HeaderMap::new(), r#"{"error":"invalid"}"#.into());
    };
    let n = obs.len();
    let ids: Vec<u64> = obs.iter().filter_map(|o| o["frequency_hz"].as_u64()).collect();
    if app.cfg.poison_mod > 0 && ids.iter().any(|i| i % app.cfg.poison_mod == 7) {
        let code = if n > 1 { 413 } else { 422 };
        return reply(&mut db, code, HeaderMap::new(), r#"{"error":"poison"}"#.into());
    }
    if n > app.cfg.cap {
        let b = format!(r#"{{"error":"too many","max_observations_per_batch":{}}}"#, app.cfg.cap);
        return reply(&mut db, 413, HeaderMap::new(), b);
    }
    let r = next(&mut db.rng);
    let mut edge = app.cfg.p500;
    if r < edge {
        return reply(&mut db, 500, HeaderMap::new(), "boom".into());
    }
    edge += app.cfg.p503;
    if r < edge {
        let mut h = HeaderMap::new();
        match (next(&mut db.rng) * 4.0) as u32 {
            0 => drop(h.insert("retry-after", HeaderValue::from_static("0"))),
            1 => drop(h.insert("retry-after", HeaderValue::from_static("2"))),
            2 => {
                let d = (chrono::Utc::now() + chrono::Duration::seconds(3)).format("%a, %d %b %Y %H:%M:%S GMT").to_string();
                h.insert("retry-after", HeaderValue::from_str(&d).unwrap());
            }
            _ => {}
        }
        return reply(&mut db, 503, h, "unavailable".into());
    }
    edge += app.cfg.p429;
    if r < edge {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("1"));
        return reply(&mut db, 429, h, "slow down".into());
    }
    edge += app.cfg.p408;
    if r < edge {
        return reply(&mut db, 408, HeaderMap::new(), "timeout".into());
    }
    // Commit.
    for o in obs {
        let key = o["client_dedupe_key"].as_str().map(str::to_string);
        if let (true, Some(k)) = (app.cfg.dedupe, key.clone()) {
            if !db.keys.insert((node.to_string(), k)) {
                continue;
            }
        }
        db.rows += 1;
        if let Some(id) = o["frequency_hz"].as_u64() {
            *db.ids.entry(id).or_default() += 1;
        }
    }
    if next(&mut db.rng) < app.cfg.lost {
        return reply(&mut db, 503, HeaderMap::new(), "committed, response lost".into());
    }
    reply(&mut db, 201, HeaderMap::new(), format!(r#"{{"accepted":{n},"rejected":[]}}"#))
}

async fn stats(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> axum::response::Response {
    let n: u64 = q.get("n").and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
    let db = app.db.lock().unwrap();
    let stored = db.ids.iter().filter(|(id, c)| **id < n && **c > 0).count();
    let dup: u64 = db.ids.values().map(|c| c.saturating_sub(1) as u64).sum();
    let by: std::collections::BTreeMap<String, u64> = db.by_status.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    axum::Json(serde_json::json!({
        "stored_unique": stored, "dup_rows": dup, "rows": db.rows, "requests": db.requests, "by_status": by,
    }))
    .into_response()
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.iter().any(|x| x == "--help") {
        println!("mock_ingest [--bind 0.0.0.0:8080] [--seed 1] [--p500 0.05] [--p503 0.05] [--p429 0.03] [--p408 0.02]\n            [--cap 100] [--dedupe on|off] [--lost-rate 0.03] [--poison-mod 0]");
        return;
    }
    let cfg = Cfg {
        bind: arg(&a, "--bind").unwrap_or_else(|| "0.0.0.0:8080".into()),
        seed: num(&a, "--seed", 1u64),
        p500: num(&a, "--p500", 0.05),
        p503: num(&a, "--p503", 0.05),
        p429: num(&a, "--p429", 0.03),
        p408: num(&a, "--p408", 0.02),
        cap: num(&a, "--cap", 100usize),
        dedupe: arg(&a, "--dedupe").map_or(true, |v| v != "off"),
        lost: num(&a, "--lost-rate", 0.03),
        poison_mod: num(&a, "--poison-mod", 0u64),
    };
    let db = Db { rng: cfg.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1, ..Default::default() };
    let bind = cfg.bind.clone();
    let app = Arc::new(App { cfg, db: Mutex::new(db) });
    let router = Router::new()
        .route("/api/observations", post(ingest))
        .route("/stats", get(stats))
        .route("/health", get(|| async { "ok" }))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("mock_ingest listening on {bind}");
    axum::serve(listener, router).await.unwrap();
}

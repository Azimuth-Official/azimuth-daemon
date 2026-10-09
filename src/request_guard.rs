//! Protection of the local control API against drive-by web pages.
//!
//! The status server listens on loopback, but a page in the user's browser can
//! still send requests to `127.0.0.1:8420` (CSRF), and a page on a hostile DNS
//! name that is rebound to 127.0.0.1 can do so same-origin (DNS rebinding). Every
//! state-changing request (anything but GET/HEAD/OPTIONS) must therefore have:
//!
//! 1. a `Host` header naming this server: `127.0.0.1`, `localhost` or `[::1]` on
//!    the bound port (plus anything listed in `AZIMUTH_ALLOWED_HOSTS`);
//! 2. no `Origin` header, or an exact match of the desktop webview
//!    (`tauri://localhost`, `http(s)://tauri.localhost`) or of the server itself
//!    (`http://{127.0.0.1|localhost|[::1]}:<bound port>`);
//! 3. a loopback peer, unless the server is bound to a non-loopback address
//!    (the Docker case, where the peer is the bridge gateway).
//!
//! The desktop app calls the daemon from Rust (reqwest: a loopback `Host`, no
//! `Origin`) and the local dashboard is served same-origin, so both pass.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[derive(Debug, Clone)]
pub struct GuardPolicy {
    pub port: u16,
    /// Bound to a non-loopback address (Docker): non-loopback peers are expected
    /// and the host port may be remapped, so any port is accepted on a loopback
    /// host name. Still no foreign host names.
    pub non_loopback_bind: bool,
    /// Extra full `Host` values (lowercase, with port) from `AZIMUTH_ALLOWED_HOSTS`.
    pub extra_hosts: Vec<String>,
}

impl GuardPolicy {
    pub fn from_bind(bind_addr: &str) -> Self {
        let (host, port) = match split_host_port(bind_addr) {
            Some((h, Some(p))) => (h, p),
            _ => (String::new(), 8420),
        };
        let loopback_bind = host == "127.0.0.1" || host == "localhost" || host == "[::1]";
        let extra_hosts = std::env::var("AZIMUTH_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        Self { port, non_loopback_bind: !loopback_bind, extra_hosts }
    }

    fn port_ok(&self, port: Option<u16>) -> bool {
        match port {
            Some(p) => p == self.port || self.non_loopback_bind,
            None => false,
        }
    }

    /// Is this `Host` header value this server?
    pub fn host_allowed(&self, host_header: &str) -> bool {
        let lower = host_header.trim().to_ascii_lowercase();
        if self.extra_hosts.contains(&lower) {
            return true;
        }
        match split_host_port(&lower) {
            Some((h, port)) => is_loopback_name(&h) && self.port_ok(port),
            None => false,
        }
    }

    /// Is this `Origin` header value the desktop webview or this server itself?
    pub fn origin_allowed(&self, origin: &str) -> bool {
        let lower = origin.trim().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
        ) {
            return true;
        }
        let Some(rest) = lower.strip_prefix("http://") else {
            return false;
        };
        match split_host_port(rest) {
            Some((h, port)) => is_loopback_name(&h) && self.port_ok(port),
            None => false,
        }
    }
}

fn is_loopback_name(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// Split `host`, `host:port` or `[v6]:port`. Anything that is not a plain
/// authority (path, query, fragment, userinfo, whitespace) is rejected, so
/// `127.0.0.1.evil.tld`, `127.0.0.1@evil.tld` and `127.0.0.1:8420/x` never match.
fn split_host_port(s: &str) -> Option<(String, Option<u16>)> {
    if s.is_empty() || s.chars().any(|c| matches!(c, '/' | '?' | '#' | '@' | '\\') || c.is_whitespace()) {
        return None;
    }
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = format!("[{}]", &rest[..end]);
        let after = &rest[end + 1..];
        return match after {
            "" => Some((host, None)),
            _ => {
                let p = after.strip_prefix(':')?;
                Some((host, Some(p.parse().ok()?)))
            }
        };
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && !h.is_empty() => Some((h.to_string(), Some(p.parse().ok()?))),
        Some(_) => None,
        None => Some((s.to_string(), None)),
    }
}

fn refuse(why: &'static str) -> Response {
    (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "forbidden", "reason": why}))).into_response()
}

/// Middleware: apply the policy to every state-changing request.
pub async fn guard_mutations(
    State(policy): State<Arc<GuardPolicy>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| policy.host_allowed(h));
    if !host_ok {
        return refuse("host not allowed");
    }
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        if !origin.to_str().is_ok_and(|o| policy.origin_allowed(o)) {
            return refuse("origin not allowed");
        }
    }
    if !peer.ip().is_loopback() && !policy.non_loopback_bind {
        return refuse("loopback callers only");
    }
    next.run(req).await
}

/// Explicit CORS: only the allowed origins, the two methods and the one header
/// the API uses. Replaces `CorsLayer::permissive()`.
pub fn cors_layer(policy: Arc<GuardPolicy>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            origin.to_str().is_ok_and(|o| policy.origin_allowed(o))
        }))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use axum::Router;

    fn policy() -> GuardPolicy {
        GuardPolicy { port: 8420, non_loopback_bind: false, extra_hosts: vec![] }
    }

    #[test]
    fn host_header_must_name_this_server() {
        let p = policy();
        for ok in ["127.0.0.1:8420", "localhost:8420", "LOCALHOST:8420", "[::1]:8420"] {
            assert!(p.host_allowed(ok), "{ok}");
        }
        for bad in [
            "evil.example:8420", // DNS rebinding
            "127.0.0.1.evil.tld:8420",
            "127.0.0.1",           // no port
            "127.0.0.1:9999",      // other port
            "127.0.0.1:8420.evil.tld",
            "127.0.0.1@evil.tld:8420",
            "",
        ] {
            assert!(!p.host_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn origin_is_an_exact_match_not_a_prefix() {
        let p = policy();
        for ok in [
            "tauri://localhost",
            "http://tauri.localhost",
            "https://tauri.localhost",
            "http://127.0.0.1:8420",
            "http://localhost:8420",
            "http://[::1]:8420",
        ] {
            assert!(p.origin_allowed(ok), "{ok}");
        }
        for bad in [
            "http://127.0.0.1.evil.tld",
            "http://127.0.0.1.evil.tld:8420",
            "http://127.0.0.1:8420.evil.tld",
            "http://localhost.evil.com",
            "http://localhost:8420/",
            "http://127.0.0.1:9999",
            "https://127.0.0.1:8420",
            "http://tauri.localhost.evil.tld",
            "tauri://localhost.evil.tld",
            "https://evil.example",
            "null",
            "",
        ] {
            assert!(!p.origin_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn a_non_loopback_bind_accepts_a_remapped_port_but_never_a_foreign_name() {
        let p = GuardPolicy::from_bind("0.0.0.0:8420");
        assert!(p.non_loopback_bind);
        assert!(p.host_allowed("127.0.0.1:8421"), "docker -p 8421:8420");
        assert!(!p.host_allowed("evil.example:8420"));
        assert!(p.origin_allowed("http://localhost:8421"));
        assert!(!p.origin_allowed("http://evil.example:8420"));
        let q = GuardPolicy::from_bind("127.0.0.1:8420");
        assert!(!q.non_loopback_bind);
        assert_eq!(q.port, 8420);
    }

    /// A tiny server carrying the real middleware and CORS layer.
    async fn serve() -> (String, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let policy = Arc::new(GuardPolicy { port, non_loopback_bind: false, extra_hosts: vec![] });
        let app = Router::new()
            .route("/read", get(|| async { "r" }))
            .route("/write", post(|| async { "w" }))
            .layer(axum::middleware::from_fn_with_state(Arc::clone(&policy), guard_mutations))
            .layer(cors_layer(policy));
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .ok();
        });
        (format!("http://127.0.0.1:{port}"), port)
    }

    async fn post_with(base: &str, headers: &[(&str, String)]) -> reqwest::StatusCode {
        let mut r = reqwest::Client::new().post(format!("{base}/write"));
        for (k, v) in headers {
            r = r.header(*k, v);
        }
        r.send().await.unwrap().status()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_middleware_blocks_hostile_requests_and_passes_the_desktop_app() {
        let (base, port) = serve().await;
        // The desktop app (reqwest): loopback Host, no Origin.
        assert_eq!(post_with(&base, &[]).await, 200);
        // Desktop webview and the dashboard itself.
        assert_eq!(post_with(&base, &[("origin", "tauri://localhost".into())]).await, 200);
        assert_eq!(post_with(&base, &[("origin", format!("http://127.0.0.1:{port}"))]).await, 200);
        // Evil origin, prefix-bypass origins.
        for evil in [
            "https://evil.example".to_string(),
            "http://127.0.0.1.evil.tld".to_string(),
            format!("http://127.0.0.1:{port}.evil.tld"),
            "http://localhost.evil.com".to_string(),
            "null".to_string(),
        ] {
            assert_eq!(post_with(&base, &[("origin", evil.clone())]).await, 403, "{evil}");
        }
        // DNS rebinding: right peer, wrong Host.
        assert_eq!(post_with(&base, &[("host", format!("evil.example:{port}"))]).await, 403);
        assert_eq!(post_with(&base, &[("host", "127.0.0.1".into())]).await, 403);
        // Reads are not guarded.
        let r = reqwest::Client::new()
            .get(format!("{base}/read"))
            .header("host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cors_headers_are_only_granted_to_allowed_origins() {
        let (base, port) = serve().await;
        let c = reqwest::Client::new();
        let allowed = c
            .get(format!("{base}/read"))
            .header("origin", "tauri://localhost")
            .send()
            .await
            .unwrap();
        assert_eq!(
            allowed.headers().get("access-control-allow-origin").map(|v| v.to_str().unwrap()),
            Some("tauri://localhost")
        );
        for evil in ["https://evil.example".to_string(), format!("http://127.0.0.1:{port}.evil.tld")] {
            let r = c.get(format!("{base}/read")).header("origin", evil.clone()).send().await.unwrap();
            assert!(r.headers().get("access-control-allow-origin").is_none(), "{evil}");
        }
        // Preflight from an evil origin is not approved either.
        let pre = c
            .request(reqwest::Method::OPTIONS, format!("{base}/write"))
            .header("origin", "https://evil.example")
            .header("access-control-request-method", "POST")
            .send()
            .await
            .unwrap();
        assert!(pre.headers().get("access-control-allow-origin").is_none());
    }
}

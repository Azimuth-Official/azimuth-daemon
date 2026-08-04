use axum::response::Html;

/// Self-contained local dashboard served at GET / on the loopback status
/// server. The page is embedded at compile time so the daemon stays a single
/// binary with no runtime asset lookup.
pub async fn dashboard_handler() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

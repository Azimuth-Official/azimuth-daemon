use axum::response::Html;

/// Self-contained local dashboard served at GET / on the loopback status
/// server. The page is embedded at compile time so the daemon stays a single
/// binary with no runtime asset lookup.
pub async fn dashboard_handler() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BANDS: [&str; 3] = ["fm", "lte", "dtv"];
    /// The /status `sweep.<band>` keys the page must show (status_server.rs `sweep`):
    /// last sweep start and end, channels evaluated / total, stations found and
    /// whether the sweep is complete (`in_progress` false).
    const KEYS: [&str; 6] = ["started_at", "completed_at", "evaluated", "total", "found", "in_progress"];

    /// The page the daemon serves at GET /.
    async fn served_page() -> String {
        dashboard_handler().await.0.to_string()
    }

    /// D2 (RED today: the page has no sweep section). Approach: `cargo test` has
    /// no JS engine, so this is a static contract check on the served HTML.
    /// 1. every band/key pair has a display element with id `sweep_<band>_<key>`;
    /// 2. the page script reads the `/status` `sweep` object and names every key,
    ///    and addresses the elements through the same `sweep_` id prefix.
    #[tokio::test]
    async fn d2_page_binds_every_sweep_field_for_every_band() {
        let html = served_page().await;
        for band in BANDS {
            for key in KEYS {
                let id = format!("sweep_{band}_{key}");
                assert!(html.contains(&format!("id=\"{id}\"")), "no element with id {id}");
            }
        }
        let script = html
            .split("<script>")
            .nth(1)
            .and_then(|rest| rest.split("</script>").next())
            .expect("inline script");
        assert!(script.contains(".sweep"), "script never reads the /status sweep object");
        assert!(script.contains("\"sweep_\"") || script.contains("'sweep_'") || script.contains("`sweep_"),
            "script does not address the sweep_<band>_<key> elements");
        for band in BANDS {
            assert!(script.contains(band), "script never mentions band {band}");
        }
        for key in KEYS {
            assert!(script.contains(key), "script never binds sweep key {key}");
        }
    }

    /// Control for the check above: the matcher does see ids and keys that are
    /// really on the page (an id the page already has; a key it does not).
    #[tokio::test]
    async fn d2_matcher_control_sees_existing_ids_and_misses_absent_ones() {
        let html = served_page().await;
        assert!(html.contains("id=\"scan_active\""));
        assert!(!html.contains("id=\"sweep_nonexistent_key\""));
    }
}

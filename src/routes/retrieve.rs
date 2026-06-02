use axum::{
    extract::{Form, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use indexmap::IndexMap;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

use crate::routes::message_response;
use crate::state::AppState;

#[derive(Serialize)]
struct SeenDeviceRow {
    serial: String,
    hostname: String,
    ipv4: String,
    ipv6: String,
}

#[derive(Serialize)]
struct RetrieveFormCtx {
    devices: Vec<SeenDeviceRow>,
    device_count: usize,
    /// Total seen_assets before the freshness filter — shown so the operator
    /// can tell when devices are being hidden because they're stale.
    total_count: usize,
    max_age_label: String,
    quicksearch_table_id: &'static str,
}

#[derive(Serialize)]
struct NamedItem {
    serial: String,
    reason: String,
}

#[derive(Serialize)]
struct RetrieveResultCtx {
    retrieved_count: usize,
    failed_count: usize,
    skipped_count: usize,
    retrieved: Vec<String>,
    failed: Vec<NamedItem>,
    skipped: Vec<NamedItem>,
}

// ── Form struct ───────────────────────────────────────────────────────────────

// No form fields needed — credentials come from stored device credentials.

// ── Git helper ────────────────────────────────────────────────────────────────

fn ensure_git_repo(path: &Path, branch: &str) -> anyhow::Result<()> {
    if path.join(".git").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path)?;
    let status = std::process::Command::new("git")
        .args(["init", "-b", branch])
        .current_dir(path)
        .output()?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        anyhow::bail!("git init failed: {}", stderr);
    }
    // Create initial commit so the branch HEAD exists
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(path)
        .output()?;
    Ok(())
}

// ── Freshness filter ──────────────────────────────────────────────────────────

/// Returns the cutoff timestamp: any device whose last_seen is before this
/// is considered stale and hidden from /retrieve. `None` means the filter
/// is disabled (max_age_secs == 0).
fn freshness_cutoff(max_age_secs: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    if max_age_secs == 0 {
        return None;
    }
    Some(chrono::Utc::now() - chrono::Duration::seconds(max_age_secs as i64))
}

/// A device is "fresh" if either the filter is disabled (`cutoff is None`)
/// or its most-recent last_seen timestamp is at or after the cutoff.
fn is_fresh(
    d: &aycallhome::Device,
    cutoff: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    let Some(cutoff) = cutoff else { return true; };
    match d.last_seen() {
        Some(ts) => ts >= cutoff,
        None => false,
    }
}

fn format_max_age(max_age_secs: u64) -> String {
    if max_age_secs == 0 {
        return "any age".to_string();
    }
    if max_age_secs % 60 == 0 {
        format!("last {} min", max_age_secs / 60)
    } else {
        format!("last {} s", max_age_secs)
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn retrieve_page(State(state): State<AppState>) -> Response {
    let devices = state.seen_assets.read().await;
    let total_count = devices.len();
    let max_age_secs = state.config.retrieve_max_age_secs;
    let cutoff = freshness_cutoff(max_age_secs);
    let rows: Vec<SeenDeviceRow> = devices
        .iter()
        .filter(|(_, d)| is_fresh(d, cutoff))
        .map(|(serial, d)| SeenDeviceRow {
            serial: serial.clone(),
            hostname: d.hostname.clone().unwrap_or_else(|| "-".to_string()),
            ipv4: d.last_ipv4.clone().unwrap_or_else(|| "-".to_string()),
            ipv6: d.last_ipv6.clone().unwrap_or_else(|| "-".to_string()),
        })
        .collect();
    drop(devices);

    let device_count = rows.len();
    let ctx = RetrieveFormCtx {
        devices: rows,
        device_count,
        total_count,
        max_age_label: format_max_age(max_age_secs),
        quicksearch_table_id: "retrieveTable",
    };
    let html = state
        .templates
        .render_page(
            &state.templates.retrieve_form,
            "Retrieve Current Configs",
            "",
            &ctx,
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

pub async fn retrieve_configs(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    // Checkboxes are named `select_<serial>`; we keep only the serials whose
    // box was checked. Unchecked boxes don't appear in the form payload at
    // all (HTML form semantics), so presence == checked.
    let selected: HashSet<String> = form
        .keys()
        .filter_map(|k| k.strip_prefix("select_").map(|s| s.to_string()))
        .collect();
    if selected.is_empty() {
        return message_response(
            &state,
            "No devices selected",
            "Pick one or more devices from the list before retrieving.",
            Some(("/retrieve", "Back")),
        )
        .into_response();
    }

    let creds = state.get_device_credentials().await;

    // 1. Check current_configs_path is configured and exists
    let configs_path = match &state.config.current_configs_path {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                message_response(
                    &state,
                    "Not Configured",
                    "current_configs_path is not configured.",
                    Some(("/retrieve", "Back")),
                ),
            )
                .into_response();
        }
    };

    // Check that the path either exists or can be created
    if configs_path.exists() && !configs_path.is_dir() {
        let msg = format!(
            "current_configs_path {} exists but is not a directory.",
            configs_path.display()
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            message_response(&state, "Configuration Error", &msg, Some(("/retrieve", "Back"))),
        )
            .into_response();
    }

    // 3. Ensure git repo
    let branch = state.config.current_branch.clone();
    if let Err(e) = ensure_git_repo(&configs_path, &branch) {
        warn!(error = %e, path = %configs_path.display(), "Failed to initialise git repo");
        let msg = format!(
            "Could not initialise git repo at {}: {}",
            configs_path.display(),
            e
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            message_response(&state, "Git Init Failed", &msg, Some(("/retrieve", "Back"))),
        )
            .into_response();
    }

    // 4. Open the repo
    let repo = match aycfgapply::git_ops::open_repo(&configs_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to open git repo");
            let msg = format!("Could not open git repo: {e}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                message_response(&state, "Git Error", &msg, Some(("/retrieve", "Back"))),
            )
                .into_response();
        }
    };

    // 5. Convert seen assets to aycfgapply::devices::Device, filtering to
    //    only the serials the operator checked AND that are fresh enough.
    //    The freshness filter is also applied here (not only in the form)
    //    so a stale POST body — e.g. a tab left open for hours — can't
    //    sneak past the UI.
    let cutoff = freshness_cutoff(state.config.retrieve_max_age_secs);
    let seen = state.seen_assets.read().await;
    let device_map: IndexMap<String, aycfgapply::devices::Device> = seen
        .iter()
        .filter(|(serial, d)| selected.contains(*serial) && is_fresh(d, cutoff))
        .map(|(serial, d)| {
            let aycfg_device = aycfgapply::devices::Device {
                serial: d.serial.clone(),
                hostname: d.hostname.clone(),
                last_ipv4: d.last_ipv4.clone(),
                last_ipv6: d.last_ipv6.clone(),
                last_seen_ts: None,
            };
            (serial.clone(), aycfg_device)
        })
        .collect();
    drop(seen);

    if device_map.is_empty() {
        return message_response(
            &state,
            "No matching devices",
            "None of the selected serials are present in the current seen-assets list.",
            Some(("/retrieve", "Back")),
        )
        .into_response();
    }

    info!(
        device_count = device_map.len(),
        branch = %branch,
        "Starting config retrieval"
    );

    // 6. Build InitConfig
    let init_config = aycfgapply::init::InitConfig {
        branch: branch.clone(),
        username: creds.username.clone(),
        password: creds.password.clone(),
        connection_type: aycfgapply::cli::ConnectionType::Ssh,
        connect_timeout: Duration::from_secs(30),
        cmd_timeout: Duration::from_secs(60),
        batch_size: 10,
        prefer_ipv4: true,
        only_devices: None,
        skip_devices: None,
    };

    // 7. Run retrieval
    let connector = crate::jumphost_connector::JumphostConnector::from_credentials(&creds);
    let summary = match aycfgapply::init::run_init(&init_config, &connector, &device_map, &repo).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "run_init returned error");
            let msg = format!("run_init returned an error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                message_response(&state, "Retrieval Failed", &msg, Some(("/retrieve", "Back"))),
            )
                .into_response();
        }
    };

    let ctx = RetrieveResultCtx {
        retrieved_count: summary.retrieved.len(),
        failed_count: summary.failed.len(),
        skipped_count: summary.skipped.len(),
        retrieved: summary.retrieved.clone(),
        failed: summary
            .failed
            .iter()
            .map(|(s, r)| NamedItem {
                serial: s.clone(),
                reason: r.clone(),
            })
            .collect(),
        skipped: summary
            .skipped
            .iter()
            .map(|(s, r)| NamedItem {
                serial: s.clone(),
                reason: r.clone(),
            })
            .collect(),
    };

    let html = state
        .templates
        .render_page(&state.templates.retrieve_result, "Retrieval Complete", "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new().route("/retrieve", get(retrieve_page).post(retrieve_configs))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
    };
    use clap::Parser;
    use indexmap::IndexMap;
    use tower::ServiceExt;

    use crate::auth::htpasswd::HtpasswdStore;
    use crate::config::AppConfig;
    use crate::state::AppState;

    fn make_test_config() -> AppConfig {
        AppConfig::try_parse_from(["aynmsgui", "--htpasswd-file", "/dev/null"])
            .expect("test config parse")
    }

    fn make_test_htpasswd() -> HtpasswdStore {
        HtpasswdStore::from_str("")
    }

    fn build_test_app() -> axum::Router {
        let state = AppState::new(
            make_test_config(),
            make_test_htpasswd(),
            None,
            IndexMap::new(),
        );
        routes().with_state(state)
    }

    // ── Test 1: GET /retrieve returns 200 with form ───────────────────────────

    #[tokio::test]
    async fn test_retrieve_page_returns_form() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/retrieve")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("<form"), "expected form element");
        assert!(
            body.contains("Retrieve Current Configs"),
            "expected submit button text"
        );
    }

    // ── Test 2: POST with no selection returns a friendly message ──────────────

    #[tokio::test]
    async fn test_retrieve_no_selection_returns_message() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/retrieve")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("No devices selected"),
            "expected 'No devices selected' message, got: {body}"
        );
    }

    // ── Test 3: POST with a selection but bad path returns 503 ────────────────

    #[tokio::test]
    async fn test_retrieve_not_configured() {
        let config = AppConfig::try_parse_from([
            "aynmsgui",
            "--htpasswd-file",
            "/dev/null",
            "--current-configs-path",
            "/nonexistent/path/that/does/not/exist",
        ])
        .expect("test config parse");

        // Seed seen_assets with one device so the selection matches a real entry.
        let mut seen = IndexMap::new();
        seen.insert(
            "SERIAL1".to_string(),
            aycallhome::Device {
                serial: "SERIAL1".to_string(),
                version: None,
                hostname: Some("host1".to_string()),
                model: None,
                token: None,
                last_ipv4: Some("10.0.0.1".to_string()),
                last_ipv6: None,
                last_seen_ipv4: None,
                last_seen_ipv6: None,
                first_seen: None,
            },
        );

        let state = AppState::new(config, make_test_htpasswd(), None, seen);
        let app = routes().with_state(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/retrieve")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("select_SERIAL1=on"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // After the form check passes, the path-not-dir check is skipped
        // (path doesn't exist), but ensure_git_repo will try to create it
        // and fail under sandboxing → SERVICE_UNAVAILABLE.
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── Test 4: GET hides devices last-seen beyond the freshness window ───────

    #[tokio::test]
    async fn test_retrieve_freshness_filter_hides_stale_devices() {
        let now = chrono::Utc::now();
        let mut seen = IndexMap::new();
        seen.insert(
            "FRESH".to_string(),
            aycallhome::Device {
                serial: "FRESH".to_string(),
                version: None,
                hostname: Some("fresh-host".to_string()),
                model: None,
                token: None,
                last_ipv4: Some("10.0.0.1".to_string()),
                last_ipv6: None,
                last_seen_ipv4: Some(now), // just seen
                last_seen_ipv6: None,
                first_seen: None,
            },
        );
        seen.insert(
            "STALE".to_string(),
            aycallhome::Device {
                serial: "STALE".to_string(),
                version: None,
                hostname: Some("stale-host".to_string()),
                model: None,
                token: None,
                last_ipv4: Some("10.0.0.2".to_string()),
                last_ipv6: None,
                last_seen_ipv4: Some(now - chrono::Duration::hours(2)), // 2h ago, well past 30min
                last_seen_ipv6: None,
                first_seen: None,
            },
        );
        let state = AppState::new(make_test_config(), make_test_htpasswd(), None, seen);
        let app = routes().with_state(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/retrieve")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("fresh-host"), "FRESH device should be shown");
        assert!(!body.contains("stale-host"), "STALE device should be hidden");
        assert!(body.contains("1 of 2"), "header should show filtered count");
    }

    // ── Test 5: POST with a selection that matches no seen asset is rejected ──

    #[tokio::test]
    async fn test_retrieve_unknown_selection_returns_message() {
        let app = build_test_app(); // empty seen_assets
        let req = Request::builder()
            .method(Method::POST)
            .uri("/retrieve")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("select_GHOST=on"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("No matching devices"),
            "expected 'No matching devices' message, got: {body}"
        );
    }
}

use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use indexmap::IndexMap;
use serde::Serialize;
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

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn retrieve_page(State(state): State<AppState>) -> Response {
    let devices = state.seen_assets.read().await;
    let device_count = devices.len();
    let rows: Vec<SeenDeviceRow> = devices
        .iter()
        .map(|(serial, d)| SeenDeviceRow {
            serial: serial.clone(),
            hostname: d.hostname.clone().unwrap_or_else(|| "-".to_string()),
            ipv4: d.last_ipv4.clone().unwrap_or_else(|| "-".to_string()),
            ipv6: d.last_ipv6.clone().unwrap_or_else(|| "-".to_string()),
        })
        .collect();
    drop(devices);

    let ctx = RetrieveFormCtx { devices: rows, device_count };
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
) -> Response {
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

    // 5. Convert seen assets to aycfgapply::devices::Device
    let seen = state.seen_assets.read().await;
    let device_map: IndexMap<String, aycfgapply::devices::Device> = seen
        .iter()
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

    // ── Test 2: POST when current_configs_path doesn't exist returns 503 ──────

    #[tokio::test]
    async fn test_retrieve_not_configured() {
        // Build a config that points current_configs_path to a nonexistent path
        let config = AppConfig::try_parse_from([
            "aynmsgui",
            "--htpasswd-file",
            "/dev/null",
            "--current-configs-path",
            "/nonexistent/path/that/does/not/exist",
        ])
        .expect("test config parse");

        let state = AppState::new(config, make_test_htpasswd(), None, IndexMap::new());
        let app = routes().with_state(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/retrieve")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

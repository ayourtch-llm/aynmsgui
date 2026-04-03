use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use indexmap::IndexMap;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

use crate::state::AppState;

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

// ── HTML helper ───────────────────────────────────────────────────────────────

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn retrieve_page(State(state): State<AppState>) -> Response {
    let devices = state.seen_assets.read().await;
    let mut device_rows = String::new();
    for (serial, d) in devices.iter() {
        let hostname = d.hostname.as_deref().unwrap_or("-");
        let ipv4 = d.last_ipv4.as_deref().unwrap_or("-");
        let ipv6 = d.last_ipv6.as_deref().unwrap_or("-");
        device_rows.push_str(&format!(
            "<tr><td>{serial}</td><td>{hostname}</td><td>{ipv4}</td><td>{ipv6}</td></tr>",
            serial = html_escape(serial),
            hostname = html_escape(hostname),
            ipv4 = html_escape(ipv4),
            ipv6 = html_escape(ipv6),
        ));
    }
    let device_count = devices.len();
    drop(devices);

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Retrieve Current Configs</title></head>
<body>
<h1>Retrieve Current Configs</h1>
<p>SSH into all seen assets and commit their running configs to the current-configs repo.</p>
<form method="POST" action="/retrieve">
  <button type="submit">Retrieve Current Configs</button>
</form>
<h2>Seen Assets ({device_count})</h2>
<table border="1" cellpadding="4">
<tr><th>Serial</th><th>Hostname</th><th>IPv4</th><th>IPv6</th></tr>
{device_rows}
</table>
<p><a href="/">Back to Dashboard</a></p>
</body>
</html>"#,
        device_count = device_count,
        device_rows = device_rows,
    );

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
                Html(
                    r#"<!DOCTYPE html>
<html><body>
<h1>Not Configured</h1>
<p>current_configs_path is not configured.</p>
<a href="/retrieve">Back</a>
</body></html>"#
                        .to_string(),
                ),
            )
                .into_response();
        }
    };

    // Check that the path either exists or can be created
    if configs_path.exists() && !configs_path.is_dir() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Configuration Error</h1>
<p>current_configs_path <code>{}</code> exists but is not a directory.</p>
<a href="/retrieve">Back</a>
</body></html>"#,
                html_escape(&configs_path.display().to_string()),
            )),
        )
            .into_response();
    }

    // 3. Ensure git repo
    let branch = state.config.current_branch.clone();
    if let Err(e) = ensure_git_repo(&configs_path, &branch) {
        warn!(error = %e, path = %configs_path.display(), "Failed to initialise git repo");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Git Init Failed</h1>
<p>Could not initialise git repo at <code>{path}</code>: {error}</p>
<a href="/retrieve">Back</a>
</body></html>"#,
                path = html_escape(&configs_path.display().to_string()),
                error = html_escape(&e.to_string()),
            )),
        )
            .into_response();
    }

    // 4. Open the repo
    let repo = match aycfgapply::git_ops::open_repo(&configs_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Failed to open git repo");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(format!(
                    r#"<!DOCTYPE html>
<html><body>
<h1>Git Error</h1>
<p>Could not open git repo: {error}</p>
<a href="/retrieve">Back</a>
</body></html>"#,
                    error = html_escape(&e.to_string()),
                )),
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
    let connector = aycfgapply::cisco_connector::CiscoIosConnector;
    let summary = match aycfgapply::init::run_init(&init_config, &connector, &device_map, &repo).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "run_init returned error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!(
                    r#"<!DOCTYPE html>
<html><body>
<h1>Retrieval Failed</h1>
<p>run_init returned an error: {error}</p>
<a href="/retrieve">Back</a>
</body></html>"#,
                    error = html_escape(&e),
                )),
            )
                .into_response();
        }
    };

    // 8. Build result page
    let retrieved_count = summary.retrieved.len();
    let failed_count = summary.failed.len();
    let skipped_count = summary.skipped.len();

    let mut failed_html = String::new();
    for (serial, reason) in &summary.failed {
        failed_html.push_str(&format!(
            "<li><strong>{serial}</strong>: {reason}</li>",
            serial = html_escape(serial),
            reason = html_escape(reason),
        ));
    }

    let mut skipped_html = String::new();
    for (serial, reason) in &summary.skipped {
        skipped_html.push_str(&format!(
            "<li><strong>{serial}</strong>: {reason}</li>",
            serial = html_escape(serial),
            reason = html_escape(reason),
        ));
    }

    let mut retrieved_html = String::new();
    for serial in &summary.retrieved {
        retrieved_html.push_str(&format!(
            "<li>{serial}</li>",
            serial = html_escape(serial),
        ));
    }

    Html(format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Retrieval Complete</title></head>
<body>
<h1>Retrieval Complete</h1>
<p>Retrieved: <strong>{retrieved_count}</strong> | Failed: <strong>{failed_count}</strong> | Skipped: <strong>{skipped_count}</strong></p>
<h2>Retrieved ({retrieved_count})</h2>
<ul>{retrieved_html}</ul>
<h2>Failed ({failed_count})</h2>
<ul>{failed_html}</ul>
<h2>Skipped ({skipped_count})</h2>
<ul>{skipped_html}</ul>
<p><a href="/retrieve">Retrieve Again</a> | <a href="/diff">View Diffs</a> | <a href="/">Dashboard</a></p>
</body>
</html>"#,
        retrieved_count = retrieved_count,
        failed_count = failed_count,
        skipped_count = skipped_count,
        retrieved_html = retrieved_html,
        failed_html = failed_html,
        skipped_html = skipped_html,
    ))
    .into_response()
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

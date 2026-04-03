use axum::{
    extract::{Form, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::state::AppState;

// ── Form struct ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ExtractForm {
    ip: String,
    username: String,
    password: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn extract_page(State(state): State<AppState>) -> Response {
    let default_username = state
        .config
        .device_username
        .as_deref()
        .unwrap_or("")
        .to_string();

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Extract Config</title></head>
<body>
<h1>Extract Config</h1>
<p>Connect to a Cisco IOS device via SSH to collect show command outputs and run the aycfggen extraction pipeline.</p>
<form method="POST" action="/extract">
  <label for="ip">IP Address:</label><br>
  <input type="text" id="ip" name="ip" required placeholder="192.168.1.1"><br><br>
  <label for="username">Username:</label><br>
  <input type="text" id="username" name="username" value="{default_username}" required><br><br>
  <label for="password">Password:</label><br>
  <input type="password" id="password" name="password" required><br><br>
  <button type="submit">Extract Config</button>
</form>
<p><a href="/devices">Back to Devices</a></p>
</body>
</html>"#
    );

    Html(html).into_response()
}

pub async fn extract_device(
    State(state): State<AppState>,
    Form(form): Form<ExtractForm>,
) -> Response {
    // Validate inputs
    if form.ip.trim().is_empty()
        || form.username.trim().is_empty()
        || form.password.trim().is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            Html(
                r#"<!DOCTYPE html>
<html><body>
<h1>Extract Error</h1>
<p>IP address, username, and password are all required.</p>
<a href="/extract">Try again</a>
</body></html>"#
                    .to_string(),
            ),
        )
            .into_response();
    }

    let base_dir = match &state.config.cfggen_base_dir {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(
                    r#"<html><body><h1>Error</h1><p>cfggen base directory is not configured.</p></body></html>"#
                        .to_string(),
                ),
            )
                .into_response();
        }
    };

    if !base_dir.exists() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Not Configured</h1>
<p>cfggen base directory does not exist: <code>{dir}</code></p>
<a href="/extract">Try again</a>
</body></html>"#,
                dir = html_escape(&base_dir.display().to_string()),
            )),
        )
            .into_response();
    }

    let ip = form.ip.trim().to_string();
    let target = format!("{}:22", ip);
    info!(ip = %ip, "Starting config extraction via SSH");

    // Connect to device via SSH
    let mut conn = match ayclic::CiscoIosConn::with_timeouts(
        &target,
        ayclic::ConnectionType::Ssh,
        &form.username,
        &form.password,
        std::time::Duration::from_secs(15),
        std::time::Duration::from_secs(30),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(ip = %ip, error = %e, "SSH connection failed");
            return Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Connection Failed</h1>
<p>Could not connect to <strong>{ip}</strong> via SSH:</p>
<pre>{error}</pre>
<a href="/extract">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
            ))
            .into_response();
        }
    };

    // Run each show command and collect output with section headers
    let commands = [
        "show version",
        "show inventory",
        "show ip interface brief",
        "show interfaces status",
        "show running-config",
    ];

    let mut collected = String::new();
    let mut show_version_output = None;

    for cmd in &commands {
        match conn.run_cmd(cmd).await {
            Ok(output) => {
                collected.push_str(&format!("!!! aycfgextract: {} !!!\n{}\n", cmd, output));
                if *cmd == "show version" {
                    show_version_output = Some(output);
                }
            }
            Err(e) => {
                warn!(ip = %ip, cmd = %cmd, error = %e, "Failed to run command");
                let _ = conn.disconnect().await;
                return Html(format!(
                    r#"<!DOCTYPE html>
<html><body>
<h1>Command Failed</h1>
<p>Connected to <strong>{ip}</strong> but '{cmd}' failed:</p>
<pre>{error}</pre>
<a href="/extract">Try again</a>
</body></html>"#,
                    ip = html_escape(&ip),
                    cmd = html_escape(cmd),
                    error = html_escape(&format!("{e}")),
                ))
                .into_response();
            }
        }
    }

    let _ = conn.disconnect().await;

    // Register the device in known_devices from show version output
    if let Some(ref sv_output) = show_version_output {
        if let Some(sv_info) = aycfggen::show_parsers::parse_show_version(sv_output) {
            if !sv_info.serial_number.is_empty() {
                state.register_known_device(
                    &sv_info.serial_number,
                    &ip,
                    if sv_info.hostname.is_empty() { None } else { Some(sv_info.hostname.as_str()) },
                    if sv_info.platform.is_empty() { None } else { Some(sv_info.platform.as_str()) },
                    if sv_info.software_image.is_empty() { None } else { Some(sv_info.software_image.as_str()) },
                ).await;
            }
        }
    }

    // Save collected output to temp file
    let saved_path = std::path::PathBuf::from(format!("/tmp/aynmsgui-extract-{}.txt", ip));
    if let Err(e) = std::fs::write(&saved_path, &collected) {
        warn!(ip = %ip, error = %e, "Failed to write temp file");
        return Html(format!(
            r#"<!DOCTYPE html>
<html><body>
<h1>File Write Failed</h1>
<p>Could not save collected output for <strong>{ip}</strong>:</p>
<pre>{error}</pre>
<a href="/extract">Try again</a>
</body></html>"#,
            ip = html_escape(&ip),
            error = html_escape(&format!("{e}")),
        ))
        .into_response();
    }

    // Build ResolvedExtractDirs from cfggen_base_dir
    let dirs = aycfggen::extract_cli::ResolvedExtractDirs {
        hardware_templates: base_dir.join("hardware-templates"),
        logical_devices: base_dir.join("logical-devices"),
        services: base_dir.join("services"),
        config_templates: base_dir.join("config-templates"),
        config_elements: base_dir.join("config-elements"),
        configs: base_dir.join("configs"),
    };

    // Ensure all 6 directories exist
    let subdirs = [
        &dirs.hardware_templates,
        &dirs.logical_devices,
        &dirs.services,
        &dirs.config_templates,
        &dirs.config_elements,
        &dirs.configs,
    ];
    for subdir in &subdirs {
        if let Err(e) = std::fs::create_dir_all(subdir) {
            warn!(ip = %ip, dir = ?subdir, error = %e, "Failed to create cfggen subdirectory");
            return Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Directory Error</h1>
<p>Could not create directory <code>{dir}</code>:</p>
<pre>{error}</pre>
<a href="/extract">Try again</a>
</body></html>"#,
                dir = html_escape(&subdir.display().to_string()),
                error = html_escape(&format!("{e}")),
            ))
            .into_response();
        }
    }

    // Run extraction (synchronous) via spawn_blocking
    let result = tokio::task::spawn_blocking(move || {
        aycfggen::extract_cli::run_extract_offline(
            &saved_path,
            &dirs,
            None,
            false,
            false,
            &[],
        )
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!(ip = %ip, "Config extraction completed successfully");
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Extraction Complete</h1>
<p>Successfully extracted config from <strong>{ip}</strong>.</p>
<p><a href="/devices">Back to Devices</a></p>
</body></html>"#,
                ip = html_escape(&ip),
            ))
            .into_response()
        }
        Ok(Err(e)) => {
            warn!(ip = %ip, error = %e, "Extraction pipeline failed");
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Extraction Failed</h1>
<p>SSH collection succeeded but the extraction pipeline failed for <strong>{ip}</strong>:</p>
<pre>{error}</pre>
<a href="/extract">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
            ))
            .into_response()
        }
        Err(e) => {
            warn!(ip = %ip, error = %e, "spawn_blocking panicked during extraction");
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Internal Error</h1>
<p>Extraction task panicked for <strong>{ip}</strong>:</p>
<pre>{error}</pre>
<a href="/extract">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
            ))
            .into_response()
        }
    }
}

/// HTML-escape a string to prevent XSS from device-supplied data.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/extract", get(extract_page).post(extract_device))
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

    fn make_test_config_no_cfggen() -> AppConfig {
        AppConfig::try_parse_from([
            "aynmsgui",
            "--htpasswd-file",
            "/dev/null",
            "--cfggen-base-dir",
            "/nonexistent/cfggen",
        ])
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

    fn build_test_app_no_cfggen() -> axum::Router {
        let state = AppState::new(
            make_test_config_no_cfggen(),
            make_test_htpasswd(),
            None,
            IndexMap::new(),
        );
        routes().with_state(state)
    }

    #[tokio::test]
    async fn test_extract_page_returns_form() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/extract")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("<form"), "expected form element");
        assert!(body.contains("name=\"ip\""), "expected ip field");
        assert!(body.contains("name=\"username\""), "expected username field");
        assert!(body.contains("name=\"password\""), "expected password field");
    }

    #[tokio::test]
    async fn test_extract_empty_ip_returns_error() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/extract")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("ip=&username=admin&password=secret"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_extract_not_configured() {
        let app = build_test_app_no_cfggen();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/extract")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("ip=192.168.1.1&username=admin&password=secret"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

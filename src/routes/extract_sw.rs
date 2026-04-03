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
pub(crate) struct ExtractSwForm {
    ip: String,
    username: String,
    password: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn extract_sw_page(State(state): State<AppState>) -> Response {
    let default_username = state
        .config
        .device_username
        .as_deref()
        .unwrap_or("")
        .to_string();

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Extract Software Image</title></head>
<body>
<h1>Extract Software Image</h1>
<p>Connect to a Cisco IOS device via SSH, identify the running image, and download it
to the local software-images directory via HTTP copy.</p>
<form method="POST" action="/extract-sw">
  <label for="ip">IP Address:</label><br>
  <input type="text" id="ip" name="ip" required placeholder="192.168.1.1"><br><br>
  <label for="username">Username:</label><br>
  <input type="text" id="username" name="username" value="{default_username}" required><br><br>
  <label for="password">Password:</label><br>
  <input type="password" id="password" name="password" required><br><br>
  <button type="submit">Extract Software Image</button>
</form>
<p><a href="/software">Back to Software</a> | <a href="/">Dashboard</a></p>
</body>
</html>"#
    );

    Html(html).into_response()
}

pub async fn extract_sw_device(
    State(state): State<AppState>,
    Form(form): Form<ExtractSwForm>,
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
<h1>Error</h1>
<p>IP address, username, and password are all required.</p>
<a href="/extract-sw">Try again</a>
</body></html>"#
                    .to_string(),
            ),
        )
            .into_response();
    }

    // Determine output directory: cfggen_base_dir/software-images
    let sw_dir = match &state.config.cfggen_base_dir {
        Some(base) if base.exists() => base.join("software-images"),
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(
                    "<html><body><h1>Error</h1><p>cfggen base directory is not configured or does not exist.</p></body></html>"
                        .to_string(),
                ),
            )
                .into_response();
        }
    };

    if let Err(e) = std::fs::create_dir_all(&sw_dir) {
        warn!(error = %e, "Failed to create software-images directory");
        return Html(format!(
            r#"<!DOCTYPE html>
<html><body>
<h1>Error</h1>
<p>Could not create software-images directory:</p>
<pre>{}</pre>
<a href="/extract-sw">Try again</a>
</body></html>"#,
            html_escape(&format!("{e}")),
        ))
        .into_response();
    }

    let ip = form.ip.trim().to_string();
    let target = crate::state::ssh_target(&ip, 22);
    info!(ip = %ip, "Starting software image extraction via SSH");

    // Connect to device
    let mut conn = match ayclic::CiscoIosConn::with_timeouts(
        &target,
        ayclic::ConnectionType::Ssh,
        &form.username,
        &form.password,
        std::time::Duration::from_secs(15),
        std::time::Duration::from_secs(120), // longer timeout for image transfer
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
<a href="/extract-sw">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
            ))
            .into_response();
        }
    };

    // Run show version to register device in seen_assets before extraction
    if let Ok(sv_output) = conn.run_cmd("show version").await {
        if let Some(sv_info) = aycfggen::show_parsers::parse_show_version(&sv_output) {
            if !sv_info.serial_number.is_empty() {
                state.register_seen_asset(
                    &sv_info.serial_number,
                    &ip,
                    if sv_info.hostname.is_empty() { None } else { Some(sv_info.hostname.as_str()) },
                    if sv_info.platform.is_empty() { None } else { Some(sv_info.platform.as_str()) },
                    if sv_info.software_image.is_empty() { None } else { Some(sv_info.software_image.as_str()) },
                ).await;
            }
        }
    }

    // Use ayiosupdate-lib extract_from_device with ExtractionKind::Image
    let request = ayiosupdate_lib::extract::ExtractionRequest {
        kind: ayiosupdate_lib::extract::ExtractionKind::Image,
        output_dir: sw_dir.clone(),
        timeout_secs: 600, // 10 minutes for large images
    };

    let result = ayiosupdate_lib::extract::extract_from_device(
        &mut conn,
        &ip,
        request,
    )
    .await;

    let _ = conn.disconnect().await;

    match result {
        Ok(extraction) => {
            info!(
                ip = %ip,
                filename = %extraction.filename,
                bytes = extraction.bytes,
                md5 = ?extraction.md5,
                "Software image extracted successfully"
            );

            let size_mb = extraction.bytes as f64 / 1_048_576.0;
            let md5_display = extraction.md5.as_deref().unwrap_or("not verified");

            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Software Image Extracted</h1>
<p>Successfully extracted software image from <strong>{ip}</strong>.</p>
<table>
<tr><th>Filename</th><td>{filename}</td></tr>
<tr><th>Size</th><td>{size:.1} MB ({bytes} bytes)</td></tr>
<tr><th>MD5</th><td><code>{md5}</code></td></tr>
<tr><th>Saved to</th><td><code>{path}</code></td></tr>
</table>
<p><a href="/software">Back to Software</a> | <a href="/extract-sw">Extract Another</a> | <a href="/">Dashboard</a></p>
</body></html>"#,
                ip = html_escape(&ip),
                filename = html_escape(&extraction.filename),
                size = size_mb,
                bytes = extraction.bytes,
                md5 = html_escape(md5_display),
                path = html_escape(&extraction.local_path.display().to_string()),
            ))
            .into_response()
        }
        Err(e) => {
            warn!(ip = %ip, error = %e, "Software image extraction failed");
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Extraction Failed</h1>
<p>Could not extract software image from <strong>{ip}</strong>:</p>
<pre>{error}</pre>
<a href="/extract-sw">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
            ))
            .into_response()
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/extract-sw", get(extract_sw_page).post(extract_sw_device))
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

    fn build_test_app() -> axum::Router {
        let state = AppState::new(
            make_test_config(),
            HtpasswdStore::from_str(""),
            None,
            IndexMap::new(),
        );
        routes().with_state(state)
    }

    #[tokio::test]
    async fn test_extract_sw_page_returns_form() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/extract-sw")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("<form"), "expected form");
        assert!(body.contains("name=\"ip\""), "expected ip field");
        assert!(body.contains("name=\"username\""), "expected username field");
        assert!(body.contains("name=\"password\""), "expected password field");
    }

    #[tokio::test]
    async fn test_extract_sw_empty_ip_returns_error() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/extract-sw")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("ip=&username=admin&password=secret"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

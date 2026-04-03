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
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn extract_sw_page(State(_state): State<AppState>) -> Response {
    let html = r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Extract Software Image</title></head>
<body>
<h1>Extract Software Image</h1>
<p>Connect to a Cisco IOS device via SSH, identify the running image, and download it
to the local software-images directory via HTTP copy.</p>
<form method="POST" action="/extract-sw">
  <label for="ip">IP Address:</label><br>
  <input type="text" id="ip" name="ip" required placeholder="192.168.1.1"><br><br>
  <button type="submit">Extract Software Image</button>
</form>
<p><a href="/software">Back to Software</a> | <a href="/">Dashboard</a></p>
</body>
</html>"#;

    Html(html).into_response()
}

pub async fn extract_sw_device(
    State(state): State<AppState>,
    Form(form): Form<ExtractSwForm>,
) -> Response {
    // Validate inputs
    if form.ip.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html(
                r#"<!DOCTYPE html>
<html><body>
<h1>Error</h1>
<p>IP address is required.</p>
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
    info!(ip = %ip, "Starting software image extraction via SSH");

    // Create SSE operation
    let (op_id, tx) = state.operations.write().await.create_operation();
    info!(ip = %ip, op_id = %op_id, "Extraction operation created");

    // Spawn the extraction in a background task
    let ops = state.operations.clone();
    let extract_state = state.clone();
    let spawned_op_id = op_id.clone();
    let extract_ip = ip.clone();

    tokio::spawn(async move {
        let send = |event_type: &str, data: &str| {
            let _ = tx.send(crate::sse::SseEvent {
                event_type: event_type.to_string(),
                data: data.to_string(),
            });
        };

        send("progress", &format!("Connecting to {}...", extract_ip));

        // Connect to device
        let mut conn = match extract_state.connect_to_device(
            &extract_ip,
            std::time::Duration::from_secs(15),
            std::time::Duration::from_secs(1200), // must match extraction timeout for copy command
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                send("error", &format!("SSH connection failed: {}", e));
                ops.write().await.remove_operation(&spawned_op_id);
                return;
            }
        };

        send("progress", "Connected. Running show version...");

        // Register device in seen_assets
        if let Ok(sv_output) = conn.run_cmd("show version").await {
            if let Some(sv_info) = aycfggen::show_parsers::parse_show_version(&sv_output) {
                if !sv_info.serial_number.is_empty() {
                    extract_state.register_seen_asset(
                        &sv_info.serial_number,
                        &extract_ip,
                        if sv_info.hostname.is_empty() { None } else { Some(sv_info.hostname.as_str()) },
                        if sv_info.platform.is_empty() { None } else { Some(sv_info.platform.as_str()) },
                        if sv_info.software_image.is_empty() { None } else { Some(sv_info.software_image.as_str()) },
                    ).await;
                    send("progress", &format!("Device: {} ({})", sv_info.hostname, sv_info.serial_number));
                }
            }
        }

        send("progress", "Starting software image extraction (this may take several minutes)...");

        let request = ayiosupdate_lib::extract::ExtractionRequest {
            kind: ayiosupdate_lib::extract::ExtractionKind::Image,
            output_dir: sw_dir,
            timeout_secs: 1200, // 20 minutes for large images
        };

        let result = ayiosupdate_lib::extract::extract_from_device(
            &mut conn,
            &extract_ip,
            request,
        )
        .await;

        let _ = conn.disconnect().await;

        match result {
            Ok(extraction) => {
                let size_mb = extraction.bytes as f64 / 1_048_576.0;
                let md5_display = extraction.md5.as_deref().unwrap_or("not verified");
                info!(
                    ip = %extract_ip,
                    filename = %extraction.filename,
                    bytes = extraction.bytes,
                    md5 = ?extraction.md5,
                    "Software image extracted successfully"
                );
                send("complete", &format!(
                    "Extracted {} ({:.1} MB, MD5: {})",
                    extraction.filename, size_mb, md5_display
                ));
            }
            Err(e) => {
                warn!(ip = %extract_ip, error = %e, "Software image extraction failed");
                send("error", &format!("Extraction failed: {}", e));
            }
        }

        ops.write().await.remove_operation(&spawned_op_id);
    });

    // Return page with SSE progress
    Html(format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Extracting Software Image</title></head>
<body>
<h1>Extracting Software Image</h1>
<p>Device: <strong>{ip}</strong></p>
<p>Operation ID: {op_id}</p>
<div id="progress"></div>
<script>
const evtSource = new EventSource("/software/upgrade/{op_id}/progress");
const div = document.getElementById("progress");
evtSource.addEventListener("progress", function(e) {{
    div.innerHTML += "<p>" + e.data + "</p>";
}});
evtSource.addEventListener("complete", function(e) {{
    div.innerHTML += "<p style='color:green'><strong>" + e.data + "</strong></p>";
    div.innerHTML += "<p><a href='/software'>Back to Software</a> | <a href='/extract-sw'>Extract Another</a></p>";
    evtSource.close();
}});
evtSource.addEventListener("error", function(e) {{
    if (e.data) {{
        div.innerHTML += "<p style='color:red'><strong>Error:</strong> " + e.data + "</p>";
    }}
    div.innerHTML += "<p><a href='/extract-sw'>Try again</a></p>";
    evtSource.close();
}});
</script>
<p><a href="/extract-sw">Back</a> | <a href="/">Dashboard</a></p>
</body>
</html>"#,
        ip = html_escape(&ip),
        op_id = html_escape(&op_id),
    ))
    .into_response()
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
    }

    #[tokio::test]
    async fn test_extract_sw_empty_ip_returns_error() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/extract-sw")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("ip="))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

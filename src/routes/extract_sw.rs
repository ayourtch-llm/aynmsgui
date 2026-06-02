use axum::{
    extract::{Form, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::routes::{message_response, message_response_with_html};
use crate::state::AppState;

// ── Form struct ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ExtractSwForm {
    ip: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn extract_sw_page(State(state): State<AppState>) -> Response {
    #[derive(Serialize)]
    struct Empty {}
    let html = state
        .templates
        .render_page(
            &state.templates.extract_sw_form,
            "Extract Software Image",
            "",
            &Empty {},
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
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
            message_response(
                &state,
                "Error",
                "IP address is required.",
                Some(("/extract-sw", "Try again")),
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
                message_response(
                    &state,
                    "Error",
                    "cfggen base directory is not configured or does not exist.",
                    None,
                ),
            )
                .into_response();
        }
    };

    if let Err(e) = std::fs::create_dir_all(&sw_dir) {
        warn!(error = %e, "Failed to create software-images directory");
        let body = format!(
            "<p>Could not create software-images directory:</p><pre>{}</pre>",
            html_escape(&format!("{e}")),
        );
        return message_response_with_html(
            &state,
            "Error",
            &body,
            Some(("/extract-sw", "Try again")),
        );
    }

    let ip = form.ip.trim().to_string();
    info!(ip = %ip, "Starting software image extraction via SSH");

    // Create SSE operation
    let (op_id, tx) = state.operations.write().await.create_operation_with_info("extract-sw", &ip);
    info!(ip = %ip, op_id = %op_id, "Extraction operation created");

    // Spawn the extraction in a background task
    let ops = state.operations.clone();
    let extract_state = state.clone();
    let spawned_op_id = op_id.clone();
    let extract_ip = ip.clone();

    tokio::spawn(async move {
        let send_progress = |data: &str| {
            let _ = tx.send(crate::sse::SseEvent {
                event_type: "progress".to_string(),
                data: data.to_string(),
            });
        };

        send_progress(&format!("Connecting to {}...", extract_ip));

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
                let msg = format!("SSH connection failed: {}", e);
                let _ = tx.send(crate::sse::SseEvent { event_type: "error".to_string(), data: msg.clone() });
                ops.write().await.fail_operation(&spawned_op_id, &msg);
                return;
            }
        };

        send_progress("Connected. Running show version...");

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
                    send_progress(&format!("Device: {} ({})", sv_info.hostname, sv_info.serial_number));
                }
            }
        }

        send_progress("Starting software image extraction (this may take several minutes)...");

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
                let msg = format!("Extracted {} ({:.1} MB, MD5: {})", extraction.filename, size_mb, md5_display);
                let _ = tx.send(crate::sse::SseEvent { event_type: "complete".to_string(), data: msg.clone() });
                ops.write().await.complete_operation(&spawned_op_id, &msg);
            }
            Err(e) => {
                warn!(ip = %extract_ip, error = %e, "Software image extraction failed");
                let msg = format!("Extraction failed: {}", e);
                let _ = tx.send(crate::sse::SseEvent { event_type: "error".to_string(), data: msg.clone() });
                ops.write().await.fail_operation(&spawned_op_id, &msg);
            }
        }
    });

    // Return page with SSE progress
    let details = format!("<p>Device: <strong>{}</strong></p>", html_escape(&ip));
    Html(crate::sse::sse_progress_page("Extracting Software Image", &details, &op_id))
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

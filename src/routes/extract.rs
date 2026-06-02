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
pub(crate) struct ExtractForm {
    ip: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn extract_page(State(state): State<AppState>) -> Response {
    #[derive(Serialize)]
    struct Empty {}
    let html = state
        .templates
        .render_page(&state.templates.extract_form, "Extract Config", "", &Empty {})
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

pub async fn extract_device(
    State(state): State<AppState>,
    Form(form): Form<ExtractForm>,
) -> Response {
    // Validate inputs
    if form.ip.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            message_response(
                &state,
                "Extract Error",
                "IP address is required.",
                Some(("/extract", "Try again")),
            ),
        )
            .into_response();
    }

    let base_dir = match &state.config.cfggen_base_dir {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                message_response(
                    &state,
                    "Error",
                    "cfggen base directory is not configured.",
                    None,
                ),
            )
                .into_response();
        }
    };

    if !base_dir.exists() {
        let msg = format!(
            "cfggen base directory does not exist: {}",
            base_dir.display()
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            message_response(&state, "Not Configured", &msg, Some(("/extract", "Try again"))),
        )
            .into_response();
    }

    let ip = form.ip.trim().to_string();
    info!(ip = %ip, "Starting config extraction via SSH");

    // Connect to device via SSH (direct or via jumphost)
    let mut conn = match state.connect_to_device(
        &ip,
        std::time::Duration::from_secs(15),
        std::time::Duration::from_secs(30),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(ip = %ip, error = %e, "SSH connection failed");
            let body = format!(
                "<p>Could not connect to <strong>{}</strong> via SSH:</p><pre>{}</pre>",
                html_escape(&ip),
                html_escape(&format!("{e}")),
            );
            return message_response_with_html(
                &state,
                "Connection Failed",
                &body,
                Some(("/extract", "Try again")),
            );
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
                // Normalize running-config to strip noise lines
                let clean_output = if *cmd == "show running-config" {
                    aycfgapply::normalize::normalize_config(&output)
                } else {
                    output.clone()
                };
                collected.push_str(&format!("!!! aycfgextract: {} !!!\n{}\n", cmd, clean_output));
                if *cmd == "show version" {
                    show_version_output = Some(output);
                }
            }
            Err(e) => {
                warn!(ip = %ip, cmd = %cmd, error = %e, "Failed to run command");
                let _ = conn.disconnect().await;
                let body = format!(
                    "<p>Connected to <strong>{}</strong> but '{}' failed:</p><pre>{}</pre>",
                    html_escape(&ip),
                    html_escape(cmd),
                    html_escape(&format!("{e}")),
                );
                return message_response_with_html(
                    &state,
                    "Command Failed",
                    &body,
                    Some(("/extract", "Try again")),
                );
            }
        }
    }

    let _ = conn.disconnect().await;

    // Register the device in seen_assets from show version output
    if let Some(ref sv_output) = show_version_output {
        if let Some(sv_info) = aycfggen::show_parsers::parse_show_version(sv_output) {
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

    // Save collected output to temp file
    let saved_path = std::path::PathBuf::from(format!("/tmp/aynmsgui-extract-{}.txt", ip));
    if let Err(e) = std::fs::write(&saved_path, &collected) {
        warn!(ip = %ip, error = %e, "Failed to write temp file");
        let body = format!(
            "<p>Could not save collected output for <strong>{}</strong>:</p><pre>{}</pre>",
            html_escape(&ip),
            html_escape(&format!("{e}")),
        );
        return message_response_with_html(
            &state,
            "File Write Failed",
            &body,
            Some(("/extract", "Try again")),
        );
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
            let body = format!(
                "<p>Could not create directory <code>{}</code>:</p><pre>{}</pre>",
                html_escape(&subdir.display().to_string()),
                html_escape(&format!("{e}")),
            );
            return message_response_with_html(
                &state,
                "Directory Error",
                &body,
                Some(("/extract", "Try again")),
            );
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
            let msg = format!("Successfully extracted config from {}.", ip);
            message_response(
                &state,
                "Extraction Complete",
                &msg,
                Some(("/devices", "Back to Devices")),
            )
        }
        Ok(Err(e)) => {
            warn!(ip = %ip, error = %e, "Extraction pipeline failed");
            let body = format!(
                "<p>SSH collection succeeded but the extraction pipeline failed for <strong>{}</strong>:</p><pre>{}</pre>",
                html_escape(&ip),
                html_escape(&format!("{e}")),
            );
            message_response_with_html(
                &state,
                "Extraction Failed",
                &body,
                Some(("/extract", "Try again")),
            )
        }
        Err(e) => {
            warn!(ip = %ip, error = %e, "spawn_blocking panicked during extraction");
            let body = format!(
                "<p>Extraction task panicked for <strong>{}</strong>:</p><pre>{}</pre>",
                html_escape(&ip),
                html_escape(&format!("{e}")),
            );
            message_response_with_html(
                &state,
                "Internal Error",
                &body,
                Some(("/extract", "Try again")),
            )
        }
    }
}

/// HTML-escape a string for embedding inside <pre>/<code> blocks where
/// device-supplied text might include angle brackets.
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
    }

    #[tokio::test]
    async fn test_extract_empty_ip_returns_error() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/extract")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("ip="))
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
            .body(Body::from("ip=192.168.1.1"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

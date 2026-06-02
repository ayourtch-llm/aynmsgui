use axum::{
    extract::{Form, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::routes::devices::{load_all_device_configs, serial_to_device_names};
use crate::routes::{message_response, message_response_with_html};
use crate::state::AppState;

// ── Form struct ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ApplyForm {
    safety_minutes: Option<u32>,
}

#[derive(Serialize)]
struct ApplyConfirmCtx {
    name: String,
    logical_device: String,
    hostname: String,
    ip: String,
    delta: String,
}

#[derive(Serialize)]
struct ApplyResultCtx {
    serial: String,
    ip: String,
    verification_ok: bool,
    remaining_delta: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// GET /apply/{name} — show the delta and a confirmation form
pub async fn apply_page(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let (target_path, current_path) = match (
        &state.config.target_configs_path,
        &state.config.current_configs_path,
    ) {
        (Some(t), Some(c)) if t.exists() => (t, c),
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                message_response(&state, "Apply", "Config paths not configured", None),
            )
                .into_response();
        }
    };

    let target_file = target_path.join(format!("{}.cfg", name));
    let current_file = current_path.join(format!("{}.cfg", name));

    let target_config = match std::fs::read_to_string(&target_file) {
        Ok(c) => aycfgapply::normalize::normalize_target_config(&c),
        Err(_) => {
            let msg = format!("Target config for '{}' not found", name);
            return (
                StatusCode::NOT_FOUND,
                message_response(&state, "Not Found", &msg, Some(("/diff", "Back to Diffs"))),
            )
                .into_response();
        }
    };

    let current_config = match std::fs::read_to_string(&current_file) {
        Ok(c) => aycfgapply::normalize::normalize_config(&c),
        Err(_) => String::new(),
    };

    let delta = aycicdiff::generate_delta(&current_config, &target_config, None);

    if delta.trim().is_empty() {
        let msg = format!("Device {} is already at the target configuration.", name);
        return message_response(&state, "No Changes", &msg, Some(("/diff", "Back to Diffs")));
    }

    // Look up logical device name for this serial
    let logical_device = state
        .config
        .cfggen_base_dir
        .as_ref()
        .and_then(|base| {
            let map = serial_to_device_names(&load_all_device_configs(base));
            map.get(&name).map(|names| names.join(", "))
        })
        .unwrap_or_else(|| "-".to_string());

    // Look up device IP from seen_assets
    let devices = state.seen_assets.read().await;
    let device = devices.get(&name);
    let device_ip = device
        .and_then(|d| d.last_ipv4.as_deref().or(d.last_ipv6.as_deref()))
        .unwrap_or("unknown")
        .to_string();
    let device_hostname = device
        .and_then(|d| d.hostname.as_deref())
        .unwrap_or("-")
        .to_string();
    drop(devices);

    let ctx = ApplyConfirmCtx {
        name: name.clone(),
        logical_device,
        hostname: device_hostname,
        ip: device_ip,
        delta,
    };
    let title = format!("Apply Config: {name}");
    let html = state
        .templates
        .render_page(&state.templates.apply_confirm, &title, "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

/// POST /apply/{name} — apply the delta to the device
pub async fn apply_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<ApplyForm>,
) -> Response {
    let creds = state.get_device_credentials().await;

    let (target_path, current_path) = match (
        &state.config.target_configs_path,
        &state.config.current_configs_path,
    ) {
        (Some(t), Some(c)) if t.exists() => (t, c),
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                message_response(&state, "Apply", "Config paths not configured", None),
            )
                .into_response();
        }
    };

    // Read and normalize configs
    let target_file = target_path.join(format!("{}.cfg", name));
    let current_file = current_path.join(format!("{}.cfg", name));

    let target_config = match std::fs::read_to_string(&target_file) {
        Ok(c) => aycfgapply::normalize::normalize_target_config(&c),
        Err(_) => {
            let msg = format!("Target config for '{}' not found", name);
            return (
                StatusCode::NOT_FOUND,
                message_response(&state, "Not Found", &msg, Some(("/diff", "Back to Diffs"))),
            )
                .into_response();
        }
    };

    let current_config = match std::fs::read_to_string(&current_file) {
        Ok(c) => aycfgapply::normalize::normalize_config(&c),
        Err(_) => String::new(),
    };

    let delta = aycicdiff::generate_delta(&current_config, &target_config, None);

    if delta.trim().is_empty() {
        let msg = format!("Device {} is already at the target configuration.", name);
        return message_response(&state, "No Changes", &msg, Some(("/diff", "Back to Diffs")));
    }

    // Find device IP
    let device_ip = {
        let devices = state.seen_assets.read().await;
        devices.get(&name)
            .and_then(|d| d.last_ipv4.clone().or(d.last_ipv6.clone()))
    };

    let ip = match device_ip {
        Some(ip) => ip,
        None => {
            let msg = format!(
                "Device {} has no known IP address. Import or extract it first.",
                name
            );
            return message_response(&state, "No IP Address", &msg, Some(("/diff", "Back to Diffs")));
        }
    };

    let safety = match form.safety_minutes {
        Some(m) if m > 0 => aycfgapply::connector::ChangeSafety::DelayedReload { minutes: m },
        _ => aycfgapply::connector::ChangeSafety::None,
    };

    info!(serial = %name, ip = %ip, safety = ?safety, "Applying config via atomic update");

    let change = aycfgapply::apply::DeviceChange {
        serial: name.clone(),
        ip: ip.clone(),
        hostname: None,
        target_config: target_config.clone(),
        current_config,
        show_version: None,
        delta: delta.clone(),
    };

    let connector = crate::jumphost_connector::JumphostConnector::from_credentials(&creds);
    let result = aycfgapply::apply::apply_device_change(
        &connector,
        &change,
        safety,
        aycfgapply::cli::ConnectionType::Ssh,
        &creds.username,
        &creds.password,
        std::time::Duration::from_secs(15),
        std::time::Duration::from_secs(120),
    ).await;

    match result {
        aycfgapply::apply::ApplyResult::Applied { serial, post_config, mismatch: _ } => {
            // Save post-apply config as the new current config
            if let Err(e) = std::fs::write(&current_file, &post_config) {
                warn!(error = %e, "Failed to save post-apply config");
            }

            // Do our own verification: re-run delta on normalized post-apply config.
            // If delta is empty, the target changes were successfully applied.
            // (The built-in mismatch check does strict string equality which
            // false-positives on default commands, crypto keys, etc.)
            let norm_post = aycfgapply::normalize::normalize_config(&post_config);
            let remaining_delta = aycicdiff::generate_delta(&norm_post, &target_config, None);
            let effectively_applied = remaining_delta.trim().is_empty();

            info!(serial = %serial, effectively_applied = effectively_applied, "Config applied");

            let ctx = ApplyResultCtx {
                serial,
                ip,
                verification_ok: effectively_applied,
                remaining_delta: remaining_delta.trim().to_string(),
            };
            let html = state
                .templates
                .render_page(&state.templates.apply_result, "Config Applied", "", &ctx)
                .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
            Html(html).into_response()
        }
        aycfgapply::apply::ApplyResult::Skipped { serial, reason } => {
            let msg = format!("Device {} was skipped: {}", serial, reason);
            message_response(&state, "Skipped", &msg, Some(("/diff", "Back to Diffs")))
        }
        aycfgapply::apply::ApplyResult::Failed { serial, error } => {
            warn!(serial = %serial, error = %error, "Config apply failed");
            let body = format!(
                "<p>Failed to apply config to <strong>{}</strong> ({}):</p><pre>{}</pre>\
                 <p><a href=\"/apply/{}\">Try Again</a></p>",
                html_escape(&serial),
                html_escape(&ip),
                html_escape(&error),
                html_escape(&serial),
            );
            message_response_with_html(
                &state,
                "Apply Failed",
                &body,
                Some(("/diff", "Back to Diffs")),
            )
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
        .route("/apply/{name}", get(apply_page).post(apply_config))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::{Method, Request, StatusCode}};
    use clap::Parser;
    use indexmap::IndexMap;
    use tower::ServiceExt;
    use crate::auth::htpasswd::HtpasswdStore;
    use crate::config::AppConfig;
    use crate::state::AppState;

    fn make_test_config() -> AppConfig {
        AppConfig::try_parse_from([
            "aynmsgui", "--htpasswd-file", "/dev/null",
            "--target-configs-path", "/nonexistent/target",
            "--current-configs-path", "/nonexistent/current",
        ]).expect("test config parse")
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
    async fn test_apply_page_not_configured() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/apply/FOC123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // target path doesn't exist, so 503 or 404
        assert!(
            resp.status() == StatusCode::SERVICE_UNAVAILABLE || resp.status() == StatusCode::NOT_FOUND,
        );
    }

    #[tokio::test]
    async fn test_apply_post_not_configured() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/apply/FOC123")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("safety_minutes=5"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // target path doesn't exist, so 503 or 404
        assert!(
            resp.status() == StatusCode::SERVICE_UNAVAILABLE || resp.status() == StatusCode::NOT_FOUND,
        );
    }
}

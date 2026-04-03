use axum::{
    extract::{Form, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::routes::devices::{load_all_device_configs, serial_to_device_names};
use crate::state::AppState;

// ── Form struct ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ApplyForm {
    safety_minutes: Option<u32>,
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
            return (StatusCode::SERVICE_UNAVAILABLE, Html(
                "<html><body><p>Config paths not configured</p></body></html>".to_string(),
            )).into_response();
        }
    };

    let target_file = target_path.join(format!("{}.cfg", name));
    let current_file = current_path.join(format!("{}.cfg", name));

    let target_config = match std::fs::read_to_string(&target_file) {
        Ok(c) => aycfgapply::normalize::normalize_target_config(&c),
        Err(_) => {
            return (StatusCode::NOT_FOUND, Html(format!(
                "<html><body><p>Target config for '{}' not found</p></body></html>", name
            ))).into_response();
        }
    };

    let current_config = match std::fs::read_to_string(&current_file) {
        Ok(c) => aycfgapply::normalize::normalize_config(&c),
        Err(_) => String::new(),
    };

    let delta = aycicdiff::generate_delta(&current_config, &target_config, None);

    if delta.trim().is_empty() {
        return Html(format!(
            r#"<!DOCTYPE html>
<html><body>
<h1>No Changes</h1>
<p>Device <strong>{name}</strong> is already at the target configuration.</p>
<p><a href="/diff">Back to Diffs</a></p>
</body></html>"#,
            name = html_escape(&name),
        )).into_response();
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
        .unwrap_or("unknown");
    let device_hostname = device.and_then(|d| d.hostname.as_deref()).unwrap_or("-");

    Html(format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Apply Config: {name}</title></head>
<body>
<h1>Apply Config: {name}</h1>
<table>
<tr><th>Serial</th><td>{name}</td></tr>
<tr><th>Logical Device</th><td>{logical_device}</td></tr>
<tr><th>Hostname</th><td>{hostname}</td></tr>
<tr><th>IP</th><td>{ip}</td></tr>
</table>
<h2>Delta to apply</h2>
<pre style="background:#f5f5f5; padding:1rem; border:1px solid #ccc; overflow-x:auto;">{delta}</pre>
<h2>Confirm Apply</h2>
<form method="POST" action="/apply/{name}">
  <label for="safety_minutes">Safety reload (minutes, 0 = none):</label><br>
  <input type="number" id="safety_minutes" name="safety_minutes" value="5" min="0" max="60"><br><br>
  <button type="submit" style="background:#d9534f; color:white; padding:0.5rem 1rem; border:none; cursor:pointer;">
    Apply Changes via Atomic Update
  </button>
</form>
<p><a href="/diff">Back to Diffs</a></p>
</body>
</html>"#,
        name = html_escape(&name),
        logical_device = html_escape(&logical_device),
        hostname = html_escape(device_hostname),
        ip = html_escape(device_ip),
        delta = html_escape(&delta),
    )).into_response()
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
            return (StatusCode::SERVICE_UNAVAILABLE, Html(
                "<html><body><p>Config paths not configured</p></body></html>".to_string(),
            )).into_response();
        }
    };

    // Read and normalize configs
    let target_file = target_path.join(format!("{}.cfg", name));
    let current_file = current_path.join(format!("{}.cfg", name));

    let target_config = match std::fs::read_to_string(&target_file) {
        Ok(c) => aycfgapply::normalize::normalize_target_config(&c),
        Err(_) => {
            return (StatusCode::NOT_FOUND, Html(format!(
                "<html><body><p>Target config for '{}' not found</p></body></html>", name
            ))).into_response();
        }
    };

    let current_config = match std::fs::read_to_string(&current_file) {
        Ok(c) => aycfgapply::normalize::normalize_config(&c),
        Err(_) => String::new(),
    };

    let delta = aycicdiff::generate_delta(&current_config, &target_config, None);

    if delta.trim().is_empty() {
        return Html(format!(
            r#"<!DOCTYPE html>
<html><body>
<h1>No Changes</h1>
<p>Device <strong>{name}</strong> is already at the target configuration.</p>
<p><a href="/diff">Back to Diffs</a></p>
</body></html>"#,
            name = html_escape(&name),
        )).into_response();
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
            return Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>No IP Address</h1>
<p>Device <strong>{name}</strong> has no known IP address. Import or extract it first.</p>
<p><a href="/diff">Back to Diffs</a></p>
</body></html>"#,
                name = html_escape(&name),
            )).into_response();
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

    let connector = aycfgapply::cisco_connector::CiscoIosConnector;
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

            let status_msg = if effectively_applied {
                "<p style='color:green'>Post-apply verification passed — no remaining delta.</p>"
            } else {
                &format!(
                    "<p style='color:orange'>Warning: Post-apply delta is not empty. \
                     Remaining changes:</p><pre style='background:#fff3cd; padding:0.5rem;'>{}</pre>",
                    html_escape(remaining_delta.trim())
                )
            };

            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Config Applied</h1>
<p>Successfully applied config to <strong>{serial}</strong> ({ip}) via atomic update.</p>
{status_msg}
<p><a href="/diff">Back to Diffs</a> | <a href="/diff/{serial}">View Updated Diff</a></p>
</body></html>"#,
                serial = html_escape(&serial),
                ip = html_escape(&ip),
                status_msg = status_msg,
            )).into_response()
        }
        aycfgapply::apply::ApplyResult::Skipped { serial, reason } => {
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Skipped</h1>
<p>Device <strong>{serial}</strong> was skipped: {reason}</p>
<p><a href="/diff">Back to Diffs</a></p>
</body></html>"#,
                serial = html_escape(&serial),
                reason = html_escape(&reason),
            )).into_response()
        }
        aycfgapply::apply::ApplyResult::Failed { serial, error } => {
            warn!(serial = %serial, error = %error, "Config apply failed");
            Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Apply Failed</h1>
<p>Failed to apply config to <strong>{serial}</strong> ({ip}):</p>
<pre>{error}</pre>
<p><a href="/apply/{serial}">Try Again</a> | <a href="/diff">Back to Diffs</a></p>
</body></html>"#,
                serial = html_escape(&serial),
                ip = html_escape(&ip),
                error = html_escape(&error),
            )).into_response()
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

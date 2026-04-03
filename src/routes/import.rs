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
pub(crate) struct ImportForm {
    ip: String,
    username: String,
    password: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn import_page(State(state): State<AppState>) -> Response {
    let default_username = state
        .config
        .device_username
        .as_deref()
        .unwrap_or("")
        .to_string();

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Import Device</title></head>
<body>
<h1>Import Device</h1>
<p>Connect to a Cisco IOS device via SSH to discover and register it in the asset inventory.</p>
<form method="POST" action="/import">
  <label for="ip">IP Address:</label><br>
  <input type="text" id="ip" name="ip" required placeholder="192.168.1.1"><br><br>
  <label for="username">Username:</label><br>
  <input type="text" id="username" name="username" value="{default_username}" required><br><br>
  <label for="password">Password:</label><br>
  <input type="password" id="password" name="password" required><br><br>
  <button type="submit">Import Device</button>
</form>
<p><a href="/assets">Back to Assets</a></p>
</body>
</html>"#
    );

    Html(html).into_response()
}

pub async fn import_device(
    State(state): State<AppState>,
    Form(form): Form<ImportForm>,
) -> Response {
    // Validate inputs
    if form.ip.trim().is_empty() || form.username.trim().is_empty() || form.password.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html(
                r#"<!DOCTYPE html>
<html><body>
<h1>Import Error</h1>
<p>IP address, username, and password are all required.</p>
<a href="/import">Try again</a>
</body></html>"#
                    .to_string(),
            ),
        )
            .into_response();
    }

    let inv_path = match &state.asset_inventory_path {
        Some(p) => p.as_ref().clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html(
                    "<html><body><h1>Error</h1><p>Asset inventory is not configured.</p></body></html>"
                        .to_string(),
                ),
            )
                .into_response();
        }
    };

    let ip = form.ip.trim().to_string();
    let target = crate::state::ssh_target(&ip, 22);
    info!(ip = %ip, "Starting device import via SSH");

    // 1. Connect to device via SSH
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
<a href="/import">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
            ))
            .into_response();
        }
    };

    // 2. Run show commands
    let show_version = match conn.run_cmd("show version").await {
        Ok(output) => output,
        Err(e) => {
            warn!(ip = %ip, error = %e, "Failed to run 'show version'");
            let _ = conn.disconnect().await;
            return Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Command Failed</h1>
<p>Connected to <strong>{ip}</strong> but 'show version' failed:</p>
<pre>{error}</pre>
<a href="/import">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
            ))
            .into_response();
        }
    };

    let show_inventory = match conn.run_cmd("show inventory").await {
        Ok(output) => output,
        Err(e) => {
            warn!(ip = %ip, error = %e, "Failed to run 'show inventory'");
            let _ = conn.disconnect().await;
            return Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Command Failed</h1>
<p>Connected to <strong>{ip}</strong> but 'show inventory' failed:</p>
<pre>{error}</pre>
<a href="/import">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
            ))
            .into_response();
        }
    };

    let _ = conn.disconnect().await;

    // 3. Parse outputs into DeviceMetadata
    let metadata_list = match ayciam::build_metadata(&show_version, &show_inventory, "ANY") {
        Ok(list) => list,
        Err(e) => {
            warn!(ip = %ip, error = %e, "Failed to parse device metadata");
            return Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Parse Failed</h1>
<p>Connected to <strong>{ip}</strong> and ran commands, but could not parse device metadata:</p>
<pre>{error}</pre>
<h2>show version output</h2>
<pre>{sv}</pre>
<h2>show inventory output</h2>
<pre>{si}</pre>
<a href="/import">Try again</a>
</body></html>"#,
                ip = html_escape(&ip),
                error = html_escape(&format!("{e}")),
                sv = html_escape(&show_version),
                si = html_escape(&show_inventory),
            ))
            .into_response();
        }
    };

    if metadata_list.is_empty() {
        return Html(format!(
            r#"<!DOCTYPE html>
<html><body>
<h1>No Devices Found</h1>
<p>Connected to <strong>{ip}</strong> but no device metadata could be extracted.</p>
<a href="/import">Try again</a>
</body></html>"#,
            ip = html_escape(&ip),
        ))
        .into_response();
    }

    // 4. Register each discovered device via ayciam (idempotent, proper S-tags, dedup)
    let mut results_html = String::new();
    let mut registered_count = 0;

    for metadata in &metadata_list {
        match ayciam::ensure_registered(&inv_path, metadata, "aynmsgui").await {
            Ok(record) => {
                info!(
                    serial = %record.serial_number,
                    asset_tag = %record.asset_tag,
                    sku = %record.sku,
                    ip = %ip,
                    "Imported device"
                );

                // Update known devices so /retrieve and /software can find this device
                state.register_known_device(
                    &record.serial_number,
                    &ip,
                    None, // hostname not available from metadata
                    record.platform.as_deref(),
                    None,
                ).await;
                registered_count += 1;
                results_html.push_str(&format!(
                    r#"<div style="border:1px solid #ccc; padding:1rem; margin:0.5rem 0;">
<h3>Device: {serial}</h3>
<table>
<tr><th>Asset Tag</th><td>{tag}</td></tr>
<tr><th>Serial</th><td>{serial}</td></tr>
<tr><th>SKU</th><td>{sku}</td></tr>
<tr><th>Platform</th><td>{platform}</td></tr>
<tr><th>Vendor</th><td>{vendor}</td></tr>
<tr><th>MACs</th><td>{macs}</td></tr>
</table>
</div>"#,
                    tag = html_escape(&record.asset_tag),
                    serial = html_escape(&record.serial_number),
                    sku = html_escape(&record.sku),
                    platform = html_escape(record.platform.as_deref().unwrap_or("-")),
                    vendor = html_escape(&record.vendor),
                    macs = html_escape(&record.mac_addresses.join(", ")),
                ));
            }
            Err(e) => {
                warn!(serial = %metadata.serial_number, error = %e, "Failed to register device");
                results_html.push_str(&format!(
                    "<p style='color:red'>Failed to register {}: {}</p>",
                    html_escape(&metadata.serial_number),
                    html_escape(&format!("{e}")),
                ));
            }
        }
    }

    // 5. Invalidate asset cache so the new records show up
    if let Some(cache) = &state.asset_cache {
        cache.invalidate();
    }

    Html(format!(
        r#"<!DOCTYPE html>
<html><body>
<h1>Import Complete</h1>
<p>Discovered {total} device(s) at <strong>{ip}</strong>, registered {registered}.</p>
{results}
<p><a href="/assets">View Assets</a> | <a href="/import">Import Another</a></p>
</body></html>"#,
        total = metadata_list.len(),
        ip = html_escape(&ip),
        registered = registered_count,
        results = results_html,
    ))
    .into_response()
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
        .route("/import", get(import_page).post(import_device))
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

    #[tokio::test]
    async fn test_import_page_returns_form() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/import")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("<form"), "expected form element");
        assert!(body.contains("name=\"ip\""), "expected ip field");
        assert!(body.contains("name=\"username\""), "expected username field");
        assert!(body.contains("name=\"password\""), "expected password field");
    }

    #[tokio::test]
    async fn test_import_empty_ip_returns_error() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/import")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("ip=&username=admin&password=secret"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

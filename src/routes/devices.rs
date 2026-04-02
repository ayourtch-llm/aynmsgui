use axum::{
    extract::{Form, Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::Serialize;
use std::collections::HashMap;
use tracing::{debug, warn};

use crate::state::AppState;

// ── View models ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DeviceListItem {
    pub name: String,
    pub hostname: Option<String>,
    pub role: Option<String>,
}

#[derive(Serialize)]
pub struct DeviceDetailView {
    pub name: String,
    pub config_json: String,
    pub raw_config: serde_json::Value,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn list_devices(State(state): State<AppState>) -> Response {
    let base_dir = match &state.config.cfggen_base_dir {
        Some(d) if d.join("logical-devices").exists() => d,
        _ => {
            let html = "<html><body><p>Logical devices not configured</p></body></html>";
            return Html(html).into_response();
        }
    };

    let devices_dir = base_dir.join("logical-devices");
    debug!(path = %devices_dir.display(), "Listing logical devices");

    let entries = match std::fs::read_dir(&devices_dir) {
        Ok(e) => e,
        Err(err) => {
            warn!(path = %devices_dir.display(), error = %err, "Failed to read logical-devices directory");
            let html = format!(
                "<html><body><p>Failed to read logical devices directory: {}</p></body></html>",
                err
            );
            return Html(html).into_response();
        }
    };

    let mut items: Vec<DeviceListItem> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!(error = %err, "Failed to read directory entry");
                continue;
            }
        };

        let path = entry.path();

        // Support flat JSON files: logical-devices/switch-01.json
        if path.is_file() {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let (hostname, role) = read_device_fields(&path);
            items.push(DeviceListItem { name, hostname, role });

        // Support directory-based layout: logical-devices/switch1/config.json
        } else if path.is_dir() {
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let config_path = path.join("config.json");
            let (hostname, role) = read_device_fields(&config_path);
            items.push(DeviceListItem { name, hostname, role });
        }
    }

    items.sort_by(|a, b| a.name.cmp(&b.name));

    let rows: String = items
        .iter()
        .map(|item| {
            format!(
                "<tr><td><a href=\"/devices/{name}\">{name}</a></td><td>{hostname}</td><td>{role}</td></tr>",
                name = item.name,
                hostname = item.hostname.as_deref().unwrap_or("-"),
                role = item.role.as_deref().unwrap_or("-"),
            )
        })
        .collect();

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Logical Devices</title></head>
<body>
<h1>Logical Devices</h1>
<table>
<tr><th>Name</th><th>Hostname</th><th>Role</th></tr>
{rows}
</table>
</body>
</html>"#
    );

    Html(html).into_response()
}

pub async fn device_detail(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let base_dir = match &state.config.cfggen_base_dir {
        Some(d) if d.join("logical-devices").exists() => d,
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html("<html><body><p>Logical devices not configured</p></body></html>"),
            )
                .into_response();
        }
    };

    debug!(name = %name, "Looking up logical device detail");

    // Try flat file first: logical-devices/{name}.json
    let flat_path = base_dir.join("logical-devices").join(format!("{}.json", name));
    // Also try directory layout: logical-devices/{name}/config.json
    let dir_path = base_dir.join("logical-devices").join(&name).join("config.json");

    let (json_path, exists) = if flat_path.exists() {
        (flat_path, true)
    } else if dir_path.exists() {
        (dir_path, true)
    } else {
        (flat_path, false)
    };

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<html><body><p>Device '{}' not found</p></body></html>",
                name
            )),
        )
            .into_response();
    }

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to read device config");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!(
                    "<html><body><p>Failed to read device config: {}</p></body></html>",
                    err
                )),
            )
                .into_response();
        }
    };

    let raw_config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to parse device config JSON");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!(
                    "<html><body><p>Failed to parse device config: {}</p></body></html>",
                    err
                )),
            )
                .into_response();
        }
    };

    let config_json = serde_json::to_string_pretty(&raw_config).unwrap_or_else(|_| content.clone());

    let detail = DeviceDetailView {
        name: name.clone(),
        config_json: config_json.clone(),
        raw_config,
    };

    let html = render_device_detail(&detail);
    Html(html).into_response()
}

fn render_device_detail(d: &DeviceDetailView) -> String {
    let hostname = d
        .raw_config
        .get("hostname")
        .or_else(|| d.raw_config.get("vars").and_then(|v| v.get("hostname")))
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    let role = d
        .raw_config
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("-");

    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Device {name}</title></head>
<body>
<h1>Logical Device: {name}</h1>
<table>
<tr><th>Name</th><td>{name}</td></tr>
<tr><th>Hostname</th><td>{hostname}</td></tr>
<tr><th>Role</th><td>{role}</td></tr>
</table>
<h2>Configuration</h2>
<pre>{config_json}</pre>
</body>
</html>"#,
        name = d.name,
        hostname = hostname,
        role = role,
        config_json = html_escape(&d.config_json),
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Read hostname and role from a JSON file path.
/// Returns (hostname, role) — both are Option<String>.
fn read_device_fields(path: &std::path::Path) -> (Option<String>, Option<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "Failed to read device file");
            return (None, None);
        }
    };

    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "Failed to parse device JSON");
            return (None, None);
        }
    };

    // hostname: top-level "hostname" key, or inside "vars"
    let hostname = val
        .get("hostname")
        .or_else(|| val.get("vars").and_then(|v| v.get("hostname")))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let role = val
        .get("role")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    (hostname, role)
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn update_ports(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let base_dir = match &state.config.cfggen_base_dir {
        Some(d) if d.join("logical-devices").exists() => d,
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Html("<html><body><p>Logical devices not configured</p></body></html>"),
            )
                .into_response();
        }
    };

    debug!(name = %name, "Updating ports for logical device");

    // Try flat file first: logical-devices/{name}.json
    let flat_path = base_dir.join("logical-devices").join(format!("{}.json", name));
    // Also try directory layout: logical-devices/{name}/config.json
    let dir_path = base_dir.join("logical-devices").join(&name).join("config.json");

    let (json_path, exists) = if flat_path.exists() {
        (flat_path, true)
    } else if dir_path.exists() {
        (dir_path, true)
    } else {
        (flat_path, false)
    };

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<html><body><p>Device '{}' not found</p></body></html>",
                name
            )),
        )
            .into_response();
    }

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to read device config");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!(
                    "<html><body><p>Failed to read device config: {}</p></body></html>",
                    err
                )),
            )
                .into_response();
        }
    };

    let mut config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to parse device config JSON");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!(
                    "<html><body><p>Failed to parse device config: {}</p></body></html>",
                    err
                )),
            )
                .into_response();
        }
    };

    // Ensure ports is an object; create it if absent
    if config.get("ports").is_none() {
        config["ports"] = serde_json::Value::Object(serde_json::Map::new());
    }

    let ports = config["ports"].as_object_mut().expect("ports is an object");

    for (key, value) in &form {
        if let Some(port_name) = key.strip_prefix("port_") {
            debug!(port = %port_name, service = %value, "Updating port service assignment");
            ports.insert(port_name.to_string(), serde_json::Value::String(value.clone()));
        }
    }

    let updated = match serde_json::to_string_pretty(&config) {
        Ok(s) => s,
        Err(err) => {
            warn!(error = %err, "Failed to serialise updated device config");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!(
                    "<html><body><p>Failed to serialise device config: {}</p></body></html>",
                    err
                )),
            )
                .into_response();
        }
    };

    if let Err(err) = std::fs::write(&json_path, &updated) {
        warn!(path = %json_path.display(), error = %err, "Failed to write updated device config");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!(
                "<html><body><p>Failed to write device config: {}</p></body></html>",
                err
            )),
        )
            .into_response();
    }

    // 302 redirect back to device detail page
    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("/devices/{}", name))],
    )
        .into_response()
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/devices", get(list_devices))
        .route("/devices/{name}", get(device_detail))
        .route("/devices/{name}/ports", post(update_ports))
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

    fn make_test_config_no_cfggen() -> AppConfig {
        AppConfig::try_parse_from([
            "aynmsgui", "--htpasswd-file", "/dev/null",
            "--cfggen-base-dir", "/nonexistent/cfggen",
        ])
            .expect("test config parse")
    }

    fn make_test_config_with_cfggen(base_dir: &std::path::Path) -> AppConfig {
        AppConfig::try_parse_from([
            "aynmsgui",
            "--htpasswd-file",
            "/dev/null",
            "--cfggen-base-dir",
            base_dir.to_str().expect("valid utf8 path"),
        ])
        .expect("test config parse")
    }

    fn make_test_htpasswd() -> HtpasswdStore {
        HtpasswdStore::from_str("")
    }

    fn build_app_no_cfggen() -> axum::Router {
        let state = AppState::new(
            make_test_config_no_cfggen(),
            make_test_htpasswd(),
            None,
            IndexMap::new(),
        );
        routes().with_state(state)
    }

    fn build_app_with_cfggen(base_dir: &std::path::Path) -> axum::Router {
        let state = AppState::new(
            make_test_config_with_cfggen(base_dir),
            make_test_htpasswd(),
            None,
            IndexMap::new(),
        );
        routes().with_state(state)
    }

    // ── Test 1: list_devices not configured ───────────────────────────────────

    #[tokio::test]
    async fn test_devices_list_not_configured() {
        let app = build_app_no_cfggen();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/devices")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(
            body.contains("not configured"),
            "expected 'not configured' in body, got: {}",
            body
        );
    }

    // ── Test 2: list_devices with data ────────────────────────────────────────

    #[tokio::test]
    async fn test_devices_list_with_data() {
        let dir = tempfile::TempDir::new().unwrap();
        let devices_dir = dir.path().join("logical-devices");
        std::fs::create_dir_all(&devices_dir).unwrap();

        // Create a flat JSON file for switch-01
        let device_json = r#"{"hostname": "sw01", "role": "access"}"#;
        std::fs::write(devices_dir.join("switch-01.json"), device_json).unwrap();

        let app = build_app_with_cfggen(dir.path());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/devices")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(
            body.contains("switch-01"),
            "expected device name 'switch-01' in body, got: {}",
            body
        );
        assert!(
            body.contains("sw01"),
            "expected hostname 'sw01' in body, got: {}",
            body
        );
    }

    // ── Test 3: device_detail found ───────────────────────────────────────────

    #[tokio::test]
    async fn test_device_detail_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let devices_dir = dir.path().join("logical-devices");
        std::fs::create_dir_all(&devices_dir).unwrap();

        let device_json = r#"{"hostname": "sw01", "role": "access", "config-template": "switch.conf"}"#;
        std::fs::write(devices_dir.join("switch-01.json"), device_json).unwrap();

        let app = build_app_with_cfggen(dir.path());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/devices/switch-01")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(
            body.contains("switch-01"),
            "expected device name in body, got: {}",
            body
        );
        assert!(
            body.contains("switch.conf") || body.contains("config-template"),
            "expected JSON content in body, got: {}",
            body
        );
    }

    // ── Test 4: device_detail not found ──────────────────────────────────────

    #[tokio::test]
    async fn test_device_detail_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let devices_dir = dir.path().join("logical-devices");
        std::fs::create_dir_all(&devices_dir).unwrap();

        let app = build_app_with_cfggen(dir.path());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/devices/nonexistent")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "expected 404 for unknown device"
        );
    }

    // ── Test 5: update_ports success ─────────────────────────────────────────

    #[tokio::test]
    async fn test_update_ports_success() {
        let dir = tempfile::TempDir::new().unwrap();
        let devices_dir = dir.path().join("logical-devices");
        std::fs::create_dir_all(&devices_dir).unwrap();

        let initial_json = r#"{
  "hostname": "sw01",
  "role": "access",
  "ports": {
    "Gi0/1": "old-service",
    "Gi0/2": "old-service2",
    "Gi0/3": "unused"
  }
}"#;
        let device_file = devices_dir.join("switch-01.json");
        std::fs::write(&device_file, initial_json).unwrap();

        let app = build_app_with_cfggen(dir.path());

        let body = "port_Gi0%2F1=uplink&port_Gi0%2F2=access-vlan20";

        let req = Request::builder()
            .method(Method::POST)
            .uri("/devices/switch-01/ports")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FOUND,
            "expected 302 redirect, got {}",
            resp.status()
        );

        // Verify the redirect target
        let location = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(location, "/devices/switch-01");

        // Verify file was updated
        let updated_content = std::fs::read_to_string(&device_file).unwrap();
        let updated: serde_json::Value = serde_json::from_str(&updated_content).unwrap();
        let ports = updated["ports"].as_object().unwrap();

        assert_eq!(
            ports.get("Gi0/1").and_then(|v| v.as_str()),
            Some("uplink"),
            "Gi0/1 should be updated to 'uplink'"
        );
        assert_eq!(
            ports.get("Gi0/2").and_then(|v| v.as_str()),
            Some("access-vlan20"),
            "Gi0/2 should be updated to 'access-vlan20'"
        );
        // Gi0/3 was not in the form — it should remain unchanged
        assert_eq!(
            ports.get("Gi0/3").and_then(|v| v.as_str()),
            Some("unused"),
            "Gi0/3 should remain 'unused'"
        );
    }

    // ── Test 6: update_ports not configured ──────────────────────────────────

    #[tokio::test]
    async fn test_update_ports_not_configured() {
        let app = build_app_no_cfggen();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/devices/switch-01/ports")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::from("port_Gi0%2F1=uplink"))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "expected 503 when cfggen_base_dir is not set"
        );
    }

    // ── Test 7: update_ports device not found ────────────────────────────────

    #[tokio::test]
    async fn test_update_ports_device_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let devices_dir = dir.path().join("logical-devices");
        std::fs::create_dir_all(&devices_dir).unwrap();
        // No device file is written — directory is empty

        let app = build_app_with_cfggen(dir.path());

        let req = Request::builder()
            .method(Method::POST)
            .uri("/devices/nonexistent/ports")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::from("port_Gi0%2F1=uplink"))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "expected 404 for unknown device"
        );
    }
}

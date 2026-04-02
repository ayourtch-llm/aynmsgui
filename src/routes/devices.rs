use axum::{
    extract::{Form, Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::Serialize;
use std::collections::HashMap;
use tracing::{debug, info, warn};

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

    // Load available services from cfggen services directory
    let available_services = load_available_services(base_dir);

    let detail = DeviceDetailView {
        name: name.clone(),
        config_json: config_json.clone(),
        raw_config,
    };

    let html = render_device_detail(&detail, &available_services);
    Html(html).into_response()
}

fn render_device_detail(d: &DeviceDetailView, available_services: &[String]) -> String {
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

    // Build port assignment table with editable service dropdowns
    let mut port_rows = String::new();
    if let Some(modules) = d.raw_config.get("modules").and_then(|m| m.as_array()) {
        for (mod_idx, module_val) in modules.iter().enumerate() {
            if module_val.is_null() {
                continue;
            }
            let sku = module_val.get("SKU").and_then(|v| v.as_str()).unwrap_or("-");
            if let Some(ports) = module_val.get("ports").and_then(|p| p.as_array()) {
                for port in ports {
                    let port_name = port.get("name").and_then(|v| v.as_str()).unwrap_or("-");
                    let current_service = port.get("service").and_then(|v| v.as_str()).unwrap_or("");

                    let options: String = available_services.iter().map(|svc| {
                        let selected = if svc == current_service { " selected" } else { "" };
                        format!("<option value=\"{svc}\"{selected}>{svc}</option>",
                            svc = html_escape(svc), selected = selected)
                    }).collect();

                    // If current service isn't in the list, add it as selected
                    let extra = if !current_service.is_empty() && !available_services.iter().any(|s| s == current_service) {
                        format!("<option value=\"{svc}\" selected>{svc}</option>",
                            svc = html_escape(current_service))
                    } else {
                        String::new()
                    };

                    port_rows.push_str(&format!(
                        "<tr><td>{mod_idx}</td><td>{sku}</td><td>{port_name}</td><td>\
                         <select name=\"port_{mod_idx}_{port_name}\">{extra}{options}</select></td></tr>\n",
                        mod_idx = mod_idx,
                        sku = html_escape(sku),
                        port_name = html_escape(port_name),
                        extra = extra,
                        options = options,
                    ));
                }
            }
        }
    }

    let ports_section = if port_rows.is_empty() {
        "<p>No port assignments found in this device config.</p>".to_string()
    } else {
        format!(
            r#"<form method="POST" action="/devices/{name}/ports">
<table>
<tr><th>Module</th><th>SKU</th><th>Port</th><th>Service</th></tr>
{port_rows}
</table>
<button type="submit">Save Port Assignments</button>
</form>"#,
            name = d.name,
            port_rows = port_rows,
        )
    };

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
<h2>Port Assignments</h2>
{ports_section}
<h2>Raw Configuration</h2>
<details><summary>Show JSON</summary>
<pre>{config_json}</pre>
</details>
<p><a href="/devices">Back to Devices</a></p>
</body>
</html>"#,
        name = d.name,
        hostname = hostname,
        role = role,
        ports_section = ports_section,
        config_json = html_escape(&d.config_json),
    )
}

/// Compile a logical device config into a .cfg file in target-configs.
fn compile_device_config(
    device_name: &str,
    cfggen_base: &std::path::Path,
    app_config: &crate::config::AppConfig,
) -> anyhow::Result<()> {
    use aycfggen::fs_sources::{
        FsHardwareTemplateSource, FsLogicalDeviceSource, FsServiceSource,
        FsConfigTemplateSource, FsConfigElementSource, FsSoftwareImageSource,
    };
    use aycfggen::compile::compile_device;

    let hw_source = FsHardwareTemplateSource::new(cfggen_base.join("hardware-templates"));
    let device_source = FsLogicalDeviceSource::new(cfggen_base.join("logical-devices"));
    let service_source = FsServiceSource::new(cfggen_base.join("services"));
    let template_source = FsConfigTemplateSource::new(cfggen_base.join("config-templates"));
    let element_source = FsConfigElementSource::new(cfggen_base.join("config-elements"));
    let image_source = FsSoftwareImageSource::new(cfggen_base.join("software-images"));

    let compiled = compile_device(
        device_name,
        &device_source,
        &hw_source,
        &service_source,
        &template_source,
        &element_source,
        &image_source,
    )?;

    // Write to target-configs directory
    if let Some(ref target_dir) = app_config.target_configs_path {
        std::fs::create_dir_all(target_dir)?;
        let cfg_path = target_dir.join(format!("{}.cfg", device_name));
        std::fs::write(&cfg_path, compiled)?;
        tracing::info!(path = %cfg_path.display(), "Wrote compiled config");
    }

    Ok(())
}

/// Load available service names from cfggen services directory.
fn load_available_services(base_dir: &std::path::Path) -> Vec<String> {
    let services_dir = base_dir.join("services");
    let mut services = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&services_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    services.push(name.to_string());
                }
            }
        }
    }
    services.sort();
    services
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

    // Update port service assignments in modules[].ports[].service
    // Form fields are named "port_{mod_idx}_{port_name}" with the service as value
    if let Some(modules) = config.get_mut("modules").and_then(|m| m.as_array_mut()) {
        for (key, new_service) in &form {
            if let Some(rest) = key.strip_prefix("port_") {
                // Parse "mod_idx_port_name" — split on first underscore
                if let Some((idx_str, port_name)) = rest.split_once('_') {
                    if let Ok(mod_idx) = idx_str.parse::<usize>() {
                        if let Some(module) = modules.get_mut(mod_idx) {
                            if let Some(ports) = module.get_mut("ports").and_then(|p| p.as_array_mut()) {
                                for port in ports.iter_mut() {
                                    if port.get("name").and_then(|n| n.as_str()) == Some(port_name) {
                                        debug!(module = mod_idx, port = %port_name, service = %new_service, "Updating port service");
                                        port["service"] = serde_json::Value::String(new_service.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
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

    info!(name = %name, "Port assignments saved, compiling config");

    // Compile the device config to produce a .cfg file in target-configs
    let compile_result = compile_device_config(&name, base_dir, &state.config);
    match &compile_result {
        Ok(()) => info!(name = %name, "Config compiled successfully"),
        Err(e) => warn!(name = %name, error = %e, "Config compilation failed (port changes saved but .cfg not updated)"),
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

        // Real aycfggen structure: modules[].ports[].{name, service}
        let initial_json = r#"{
  "config-template": "test.conf",
  "modules": [
    {
      "SKU": "C9300-48P",
      "ports": [
        {"name": "Gi1/0/1", "service": "old-service"},
        {"name": "Gi1/0/2", "service": "old-service2"},
        {"name": "Gi1/0/3", "service": "unused"}
      ]
    }
  ]
}"#;
        let device_file = devices_dir.join("switch-01.json");
        std::fs::write(&device_file, initial_json).unwrap();

        let app = build_app_with_cfggen(dir.path());

        // Form fields: port_{mod_idx}_{port_name}=new_service
        let body = "port_0_Gi1%2F0%2F1=uplink&port_0_Gi1%2F0%2F2=access-vlan20";

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

        let location = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(location, "/devices/switch-01");

        // Verify file was updated
        let updated_content = std::fs::read_to_string(&device_file).unwrap();
        let updated: serde_json::Value = serde_json::from_str(&updated_content).unwrap();
        let ports = updated["modules"][0]["ports"].as_array().unwrap();

        let svc = |idx: usize| ports[idx].get("service").and_then(|v| v.as_str());
        assert_eq!(svc(0), Some("uplink"), "Gi1/0/1 should be updated to 'uplink'");
        assert_eq!(svc(1), Some("access-vlan20"), "Gi1/0/2 should be updated to 'access-vlan20'");
        assert_eq!(svc(2), Some("unused"), "Gi1/0/3 should remain 'unused'");
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

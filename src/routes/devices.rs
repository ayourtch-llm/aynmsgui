use axum::{
    extract::{Form, Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
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

/// A candidate asset that could be assigned to a logical device (same SKU, recently called home).
pub struct AssignCandidate {
    pub serial: String,
    pub hostname: String,
    pub last_seen_label: String,
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

    // Load available services and software images from cfggen directories
    let available_services = load_available_services(base_dir);
    let available_images = load_available_software_images(base_dir);

    // Build assign candidates if device has no serial
    let candidates = if first_module_serial(&raw_config).is_none() {
        build_assign_candidates(&raw_config, &state).await
    } else {
        vec![]
    };

    let detail = DeviceDetailView {
        name: name.clone(),
        config_json: config_json.clone(),
        raw_config,
    };

    let html = render_device_detail(&detail, &available_services, &available_images, &candidates);
    Html(html).into_response()
}

fn render_device_detail(d: &DeviceDetailView, available_services: &[String], available_images: &[String], candidates: &[AssignCandidate]) -> String {
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

    let current_image = d
        .raw_config
        .get("software-image")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let image_options: String = available_images
        .iter()
        .map(|img| {
            let selected = if img == current_image { " selected" } else { "" };
            format!(
                "<option value=\"{img}\"{selected}>{img}</option>",
                img = html_escape(img),
                selected = selected,
            )
        })
        .collect();

    // If current image isn't in the list, add it as selected
    let image_extra = if !current_image.is_empty()
        && !available_images.iter().any(|i| i == current_image)
    {
        format!(
            "<option value=\"{img}\" selected>{img}</option>",
            img = html_escape(current_image),
        )
    } else {
        String::new()
    };

    let software_image_field = format!(
        r#"<form method="POST" action="/devices/{name}/software-image" style="display:inline">
<select name="software_image">{image_extra}{image_options}</select>
<button type="submit">Update</button>
</form>"#,
        name = d.name,
        image_extra = image_extra,
        image_options = image_options,
    );

    let serial = first_module_serial(&d.raw_config);
    let serial_display = match &serial {
        Some(s) => s.as_str(),
        None => "-",
    };

    let serial_action = if serial.is_some() {
        format!(
            r#" <form method="POST" action="/devices/{name}/unassign-serial" style="display:inline">
<button type="submit" onclick="return confirm('Remove serial from this device?')">Unassign Serial</button>
</form>"#,
            name = d.name,
        )
    } else if !candidates.is_empty() {
        let options: String = candidates
            .iter()
            .map(|c| {
                format!(
                    "<option value=\"{serial}\">{serial} — {hostname} — {last_seen}</option>",
                    serial = html_escape(&c.serial),
                    hostname = html_escape(&c.hostname),
                    last_seen = html_escape(&c.last_seen_label),
                )
            })
            .collect();
        format!(
            r#" <form method="POST" action="/devices/{name}/assign-serial" style="display:inline">
<select name="serial" id="assign-serial-select" onchange="document.getElementById('assign-serial-btn').disabled = (this.value === '')">
<option value="" selected>-</option>
{options}
</select>
<button type="submit" id="assign-serial-btn" disabled onclick="return confirm('Assign this serial to the device?')">Assign Serial</button>
</form>"#,
            name = d.name,
            options = options,
        )
    } else {
        String::new()
    };

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

    // Show upgrade section only when serial is assigned and a software image is set
    let upgrade_section = if serial.is_some() && !current_image.is_empty() {
        format!(
            r#"<h2>Software Upgrade</h2>
<form method="POST" action="/devices/{name}/upgrade">
  <button type="submit" onclick="return confirm('Start software upgrade to {image}?')"
    style="background:#d9534f; color:white; padding:0.5rem 1rem; border:none; cursor:pointer;">
    Upgrade to {image}
  </button>
</form>"#,
            name = d.name,
            image = html_escape(current_image),
        )
    } else {
        String::new()
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Device {name}</title></head>
<body>
<h1>Logical Device: {name}</h1>
<table>
<tr><th>Name</th><td>{name} <form method="POST" action="/devices/{name}/rename" style="display:inline">
<input type="text" name="new_name" size="20" placeholder="New name">
<button type="submit" onclick="return confirm('Rename device to the new name?')">Rename</button>
</form></td></tr>
<tr><th>Hostname</th><td>{hostname}</td></tr>
<tr><th>Role</th><td>{role}</td></tr>
<tr><th>Serial</th><td>{serial}{serial_action}</td></tr>
<tr><th>Software Image</th><td>{software_image_field}</td></tr>
</table>
{upgrade_section}
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
        serial = html_escape(serial_display),
        serial_action = serial_action,
        software_image_field = software_image_field,
        upgrade_section = upgrade_section,
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

    // Always write preview config keyed by device name
    if let Some(ref preview_dir) = app_config.target_configs_preview_path {
        std::fs::create_dir_all(preview_dir)?;
        let preview_path = preview_dir.join(format!("{}.cfg", device_name));
        std::fs::write(&preview_path, &compiled)?;
        tracing::info!(path = %preview_path.display(), "Wrote preview config");
    }

    // If device has a serial, also write serial-keyed config to target-configs
    let serial = {
        // Read the device config JSON to extract first module serial
        let logical_devices_dir = cfggen_base.join("logical-devices");
        let flat_path = logical_devices_dir.join(format!("{}.json", device_name));
        let dir_path = logical_devices_dir.join(device_name).join("config.json");
        let json_path = if flat_path.exists() { flat_path } else { dir_path };
        std::fs::read_to_string(&json_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|val| first_module_serial(&val))
    };

    if let Some(ref target_dir) = app_config.target_configs_path {
        if let Some(ref serial) = serial {
            std::fs::create_dir_all(target_dir)?;
            let cfg_path = target_dir.join(format!("{}.cfg", serial));
            std::fs::write(&cfg_path, &compiled)?;
            tracing::info!(path = %cfg_path.display(), serial = %serial, "Wrote serial-keyed target config");
        }
    }

    Ok(())
}

/// Load available software image filenames from cfggen software-images directory.
fn load_available_software_images(base_dir: &std::path::Path) -> Vec<String> {
    let images_dir = base_dir.join("software-images");
    let mut images = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&images_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    images.push(name.to_string());
                }
            }
        }
    }
    images.sort();
    images
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

/// Load all logical device configs from the cfggen base directory.
///
/// Returns an `IndexMap` keyed by device name, with the parsed JSON `Value` as
/// the value.  Supports both flat files (`logical-devices/{name}.json`) and
/// directory layouts (`logical-devices/{name}/config.json`).
pub fn load_all_device_configs(
    cfggen_base_dir: &std::path::Path,
) -> IndexMap<String, serde_json::Value> {
    let devices_dir = cfggen_base_dir.join("logical-devices");
    let mut configs: IndexMap<String, serde_json::Value> = IndexMap::new();

    let entries = match std::fs::read_dir(&devices_dir) {
        Ok(e) => e,
        Err(err) => {
            warn!(path = %devices_dir.display(), error = %err, "Failed to read logical-devices directory");
            return configs;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();

        let (name, json_path) = if path.is_file() {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            (name, path)
        } else if path.is_dir() {
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            (name, path.join("config.json"))
        } else {
            continue;
        };

        let content = match std::fs::read_to_string(&json_path) {
            Ok(c) => c,
            Err(err) => {
                warn!(path = %json_path.display(), error = %err, "Failed to read device config");
                continue;
            }
        };

        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(val) => {
                configs.insert(name, val);
            }
            Err(err) => {
                warn!(path = %json_path.display(), error = %err, "Failed to parse device config JSON");
            }
        }
    }

    configs
}

/// Build a mapping from module serial number → list of logical device names
/// that reference that serial in their `modules[].serial` field.
pub fn serial_to_device_names(
    configs: &IndexMap<String, serde_json::Value>,
) -> IndexMap<String, Vec<String>> {
    let mut map: IndexMap<String, Vec<String>> = IndexMap::new();

    for (device_name, config) in configs {
        if let Some(modules) = config.get("modules").and_then(|m| m.as_array()) {
            for module in modules {
                if let Some(serial) = module.get("serial").and_then(|s| s.as_str()) {
                    map.entry(serial.to_string())
                        .or_default()
                        .push(device_name.clone());
                }
            }
        }
    }

    map
}

/// Extract the SKU from the first module in a device config JSON.
fn first_module_sku(config: &serde_json::Value) -> Option<String> {
    config
        .get("modules")
        .and_then(|m| m.as_array())
        .and_then(|modules| modules.first())
        .and_then(|module| module.get("SKU"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

/// Build a list of candidate assets that match the device's SKU and have
/// recently called home. Sorted by last-seen time descending (IPv6 first).
async fn build_assign_candidates(
    raw_config: &serde_json::Value,
    state: &AppState,
) -> Vec<AssignCandidate> {
    let sku = match first_module_sku(raw_config) {
        Some(s) => s,
        None => return vec![],
    };

    // Read all asset records to find those matching the SKU
    let inv_path = match &state.asset_inventory_path {
        Some(p) => p.as_ref().clone(),
        None => return vec![],
    };
    let all_records: Vec<ayciam::AssetRecord> = match std::fs::read_to_string(&inv_path) {
        Ok(content) => content
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                serde_json::from_str(line).ok()
            })
            .collect(),
        Err(_) => return vec![],
    };

    let matching_serials: Vec<String> = all_records
        .iter()
        .filter(|r| r.sku == sku)
        .map(|r| r.serial_number.clone())
        .collect();

    if matching_serials.is_empty() {
        return vec![];
    }

    // Collect serials already used by other device configs
    let used_serials = state
        .config
        .cfggen_base_dir
        .as_ref()
        .map(|base| serial_to_device_names(&load_all_device_configs(base)))
        .unwrap_or_default();

    let seen_assets = state.seen_assets.read().await;

    let mut sorted: Vec<_> = matching_serials
        .into_iter()
        .filter(|serial| !used_serials.contains_key(serial))
        .filter_map(|serial| {
            let device = seen_assets.get(&serial)?;
            // Must have called home at least once
            let last_seen = match (device.last_seen_ipv6, device.last_seen_ipv4) {
                (Some(v6), _) => v6,
                (_, Some(v4)) => v4,
                _ => return None,
            };
            let hostname = device.hostname.as_deref().unwrap_or("-").to_string();
            let ip = device
                .last_ipv6
                .as_deref()
                .or(device.last_ipv4.as_deref())
                .unwrap_or("-");
            Some((serial, hostname, ip.to_string(), last_seen, device.last_seen_ipv6.is_some()))
        })
        .collect();

    // Sort: IPv6-reachable first, then by last-seen descending
    sorted.sort_by(|a, b| b.4.cmp(&a.4).then_with(|| b.3.cmp(&a.3)));

    sorted
        .into_iter()
        .map(|(serial, hostname, ip, last_seen, _)| AssignCandidate {
            serial,
            hostname,
            last_seen_label: format!("{} ({})", last_seen.format("%Y-%m-%d %H:%M"), ip),
        })
        .collect()
}

/// Extract the serial number from the first module in a device config JSON.
pub fn first_module_serial(config: &serde_json::Value) -> Option<String> {
    config
        .get("modules")
        .and_then(|m| m.as_array())
        .and_then(|modules| modules.first())
        .and_then(|module| module.get("serial"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
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

/// POST /devices/{name}/unassign-serial — clear the serial from all modules.
pub async fn unassign_serial(
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

    // Locate the device config file
    let flat_path = base_dir.join("logical-devices").join(format!("{}.json", name));
    let dir_path = base_dir.join("logical-devices").join(&name).join("config.json");
    let json_path = if flat_path.exists() {
        flat_path
    } else if dir_path.exists() {
        dir_path
    } else {
        return (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<html><body><p>Device '{}' not found</p></body></html>",
                name
            )),
        )
            .into_response();
    };

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

    // Clear serial from all modules
    if let Some(modules) = config.get_mut("modules").and_then(|m| m.as_array_mut()) {
        for module in modules.iter_mut() {
            if module.get("serial").is_some() {
                module["serial"] = serde_json::Value::Null;
                debug!(name = %name, "Cleared serial from module");
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

    info!(name = %name, "Serial unassigned from device, recompiling config");

    let compile_result = compile_device_config(&name, base_dir, &state.config);
    match &compile_result {
        Ok(()) => info!(name = %name, "Config compiled successfully after serial unassign"),
        Err(e) => warn!(name = %name, error = %e, "Config compilation failed after serial unassign"),
    }

    // Redirect back to device detail
    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("/devices/{}", name))],
    )
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct AssignSerialForm {
    serial: String,
}

/// POST /devices/{name}/assign-serial — set modules[0].serial to the chosen value.
pub async fn assign_serial(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<AssignSerialForm>,
) -> Response {
    if form.serial.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<html><body><p>No serial selected</p></body></html>"),
        )
            .into_response();
    }

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

    let flat_path = base_dir.join("logical-devices").join(format!("{}.json", name));
    let dir_path = base_dir.join("logical-devices").join(&name).join("config.json");
    let json_path = if flat_path.exists() {
        flat_path
    } else if dir_path.exists() {
        dir_path
    } else {
        return (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<html><body><p>Device '{}' not found</p></body></html>",
                name
            )),
        )
            .into_response();
    };

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

    // Set serial on the first module
    if let Some(modules) = config.get_mut("modules").and_then(|m| m.as_array_mut()) {
        if let Some(first_module) = modules.first_mut() {
            first_module["serial"] = serde_json::Value::String(form.serial.trim().to_string());
            debug!(name = %name, serial = %form.serial, "Assigned serial to first module");
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

    info!(name = %name, serial = %form.serial, "Serial assigned to device, recompiling config");

    let compile_result = compile_device_config(&name, base_dir, &state.config);
    match &compile_result {
        Ok(()) => info!(name = %name, "Config compiled successfully after serial assign"),
        Err(e) => warn!(name = %name, error = %e, "Config compilation failed after serial assign"),
    }

    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("/devices/{}", name))],
    )
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct SoftwareImageForm {
    software_image: String,
}

/// POST /devices/{name}/software-image — update the software-image field.
pub async fn update_software_image(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<SoftwareImageForm>,
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

    let flat_path = base_dir.join("logical-devices").join(format!("{}.json", name));
    let dir_path = base_dir.join("logical-devices").join(&name).join("config.json");
    let json_path = if flat_path.exists() {
        flat_path
    } else if dir_path.exists() {
        dir_path
    } else {
        return (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<html><body><p>Device '{}' not found</p></body></html>",
                name
            )),
        )
            .into_response();
    };

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

    config["software-image"] = serde_json::Value::String(form.software_image.trim().to_string());

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

    info!(name = %name, image = %form.software_image, "Software image updated, recompiling config");

    let compile_result = compile_device_config(&name, base_dir, &state.config);
    match &compile_result {
        Ok(()) => info!(name = %name, "Config compiled successfully after software image update"),
        Err(e) => warn!(name = %name, error = %e, "Config compilation failed after software image update"),
    }

    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("/devices/{}", name))],
    )
        .into_response()
}

// Upgrade form has no fields — credentials come from stored device credentials.

/// SSE-compatible progress callback that sends events to a broadcast channel.
struct SseProgressCallback {
    tx: tokio::sync::broadcast::Sender<crate::sse::SseEvent>,
}

#[async_trait::async_trait]
impl ayiosupdate_lib::upgrade::UpgradeProgressCallback for SseProgressCallback {
    async fn on_progress(&self, progress: &ayiosupdate_lib::upgrade::UpgradeProgress) {
        let data = format!("{:?}", progress);
        let _ = self.tx.send(crate::sse::SseEvent {
            event_type: "progress".to_string(),
            data,
        });
    }
}

/// POST /devices/{name}/upgrade — start a software upgrade via SSE.
pub async fn start_upgrade(
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

    // Load device config to get serial and software-image
    let configs = load_all_device_configs(base_dir);
    let config = match configs.get(&name) {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Html(format!(
                    "<html><body><p>Device '{}' not found</p></body></html>",
                    name
                )),
            )
                .into_response();
        }
    };

    let serial = match first_module_serial(&config) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<html><body><p>No serial assigned to this device</p></body></html>"),
            )
                .into_response();
        }
    };

    let image_name = match config.get("software-image").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<html><body><p>No software image configured for this device</p></body></html>"),
            )
                .into_response();
        }
    };

    // Resolve the image file path
    let image_path = base_dir.join("software-images").join(&image_name);
    if !image_path.exists() {
        return (
            StatusCode::BAD_REQUEST,
            Html(format!(
                "<html><body><p>Software image file not found: {}</p></body></html>",
                html_escape(&image_name),
            )),
        )
            .into_response();
    }

    // Look up device IP from seen_assets
    let device_ip = {
        let assets = state.seen_assets.read().await;
        assets
            .get(&serial)
            .and_then(|d| d.last_ipv4.clone().or(d.last_ipv6.clone()))
    };

    let ip = match device_ip {
        Some(ip) => ip,
        None => {
            return Html(format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>No IP Address</h1>
<p>Device <strong>{serial}</strong> has no known IP address.</p>
<p><a href="/devices/{name}">Back</a></p>
</body></html>"#,
                serial = html_escape(&serial),
                name = html_escape(&name),
            ))
            .into_response();
        }
    };

    // Create SSE operation
    let (op_id, tx) = state.operations.write().await.create_operation_with_info("upgrade", &name);
    info!(device = %name, serial = %serial, op_id = %op_id, image = %image_name, "Starting software upgrade");

    // Spawn the upgrade task
    let ops = state.operations.clone();
    let spawned_op_id = op_id.clone();
    let upgrade_state = state.clone();
    let upgrade_ip = ip.clone();
    let ssh_target = crate::state::ssh_target(&ip, 22);

    tokio::spawn(async move {
        let creds = upgrade_state.get_device_credentials().await;
        let username = creds.username.clone();
        let password = creds.password.clone();

        // Connect via SSH (direct or via jumphost)
        // Read timeout must be long enough for image transfer + verify /md5
        let mut conn = match upgrade_state.connect_to_device(
            &upgrade_ip,
            std::time::Duration::from_secs(15),
            std::time::Duration::from_secs(1200), // 20 minutes for large image transfers
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

        let request = ayiosupdate_lib::upgrade::UpgradeRequest {
            image_path,
            expected_md5: None,
            delete_existing: false,
            cleanup_after: true,
            timeout_secs: 1200, // 20 minutes for large image transfers
        };

        let progress_cb = SseProgressCallback { tx: tx.clone() };

        match ayiosupdate_lib::upgrade::upgrade_classic_ios(
            &mut conn,
            &ssh_target,
            &username,
            &password,
            request,
            &progress_cb,
        )
        .await
        {
            Ok(result) => {
                let msg = format!("{} -> {}", result.old_version, result.new_version);
                let _ = tx.send(crate::sse::SseEvent {
                    event_type: "complete".to_string(),
                    data: serde_json::json!({
                        "status": "success",
                        "old_version": result.old_version,
                        "new_version": result.new_version,
                    })
                    .to_string(),
                });
                ops.write().await.complete_operation(&spawned_op_id, &msg);
            }
            Err(e) => {
                let msg = format!("Upgrade failed: {}", e);
                let _ = tx.send(crate::sse::SseEvent { event_type: "error".to_string(), data: msg.clone() });
                ops.write().await.fail_operation(&spawned_op_id, &msg);
            }
        }

        let _ = conn.disconnect().await;
    });

    // Return page with SSE progress
    let details = format!(
        "<p>Device: <strong>{}</strong> (serial: {})<br>Image: <strong>{}</strong></p>",
        html_escape(&name), html_escape(&serial), html_escape(&image_name),
    );
    Html(crate::sse::sse_progress_page("Software Upgrade", &details, &op_id))
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct RenameForm {
    new_name: String,
}

/// POST /devices/{name}/rename — rename a logical device.
pub async fn rename_device(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<RenameForm>,
) -> Response {
    let new_name = form.new_name.trim().to_string();
    if new_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<html><body><p>New name cannot be empty</p></body></html>"),
        )
            .into_response();
    }

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

    let ld_dir = base_dir.join("logical-devices");

    // Find the source (directory or flat JSON)
    let src_dir = ld_dir.join(&name);
    let src_json = ld_dir.join(format!("{}.json", name));
    let dst_dir = ld_dir.join(&new_name);
    let dst_json = ld_dir.join(format!("{}.json", new_name));

    if dst_dir.exists() || dst_json.exists() {
        return (
            StatusCode::CONFLICT,
            Html(format!(
                "<html><body><p>A device named '{}' already exists</p></body></html>",
                new_name
            )),
        )
            .into_response();
    }

    if src_dir.exists() {
        if let Err(e) = std::fs::rename(&src_dir, &dst_dir) {
            warn!(error = %e, from = %name, to = %new_name, "Failed to rename device directory");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!(
                    "<html><body><p>Failed to rename: {}</p></body></html>",
                    e
                )),
            )
                .into_response();
        }
    } else if src_json.exists() {
        if let Err(e) = std::fs::rename(&src_json, &dst_json) {
            warn!(error = %e, from = %name, to = %new_name, "Failed to rename device JSON");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!(
                    "<html><body><p>Failed to rename: {}</p></body></html>",
                    e
                )),
            )
                .into_response();
        }
    } else {
        return (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<html><body><p>Device '{}' not found</p></body></html>",
                name
            )),
        )
            .into_response();
    }

    info!(from = %name, to = %new_name, "Renamed logical device");

    // Recompile the config under the new name
    let compile_result = compile_device_config(&new_name, base_dir, &state.config);
    match &compile_result {
        Ok(()) => info!(name = %new_name, "Config compiled after rename"),
        Err(e) => warn!(name = %new_name, error = %e, "Config compilation failed after rename"),
    }

    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("/devices/{}", new_name))],
    )
        .into_response()
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/devices", get(list_devices))
        .route("/devices/{name}", get(device_detail))
        .route("/devices/{name}/ports", post(update_ports))
        .route("/devices/{name}/unassign-serial", post(unassign_serial))
        .route("/devices/{name}/assign-serial", post(assign_serial))
        .route("/devices/{name}/software-image", post(update_software_image))
        .route("/devices/{name}/upgrade", post(start_upgrade))
        .route("/devices/{name}/rename", post(rename_device))
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

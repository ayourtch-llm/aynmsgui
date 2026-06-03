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

use crate::routes::message_response;
use crate::state::AppState;

// ── View models ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct DeviceListRow {
    name: String,
    hostname: String,
    role: String,
    description: String,
}

#[derive(Serialize)]
struct DevicesListCtx {
    rows: Vec<DeviceListRow>,
    quicksearch_table_id: &'static str,
}

/// A candidate asset that could be assigned to a logical device (same SKU, recently called home).
pub struct AssignCandidate {
    pub serial: String,
    pub hostname: String,
    pub last_seen_label: String,
}

#[derive(Serialize)]
struct CandidateCtx {
    serial: String,
    hostname: String,
    last_seen: String,
}

#[derive(Serialize)]
struct OptionCtx {
    name: String,
    selected: bool,
}

#[derive(Serialize)]
struct PortRowCtx {
    mod_idx: usize,
    sku: String,
    port_name: String,
    field_name: String,
    has_extra: bool,
    extra_value: String,
    options: Vec<OptionCtx>,
}

#[derive(Serialize)]
struct DeviceDetailCtx {
    name: String,
    hostname: String,
    role: String,
    description: String,
    serial: String,
    has_serial: bool,
    has_candidates: bool,
    candidates: Vec<CandidateCtx>,
    has_image_extra: bool,
    image_extra: String,
    image_options: Vec<OptionCtx>,
    has_upgrade_section: bool,
    current_image: String,
    has_ports: bool,
    port_rows: Vec<PortRowCtx>,
    config_json: String,
}

// ── Local error helpers ──────────────────────────────────────────────────────

fn not_configured(state: &AppState) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        message_response(state, "Devices", "Logical devices not configured", None),
    )
        .into_response()
}

fn device_not_found(state: &AppState, name: &str) -> Response {
    let msg = format!("Device '{}' not found", name);
    (
        StatusCode::NOT_FOUND,
        message_response(state, "Not Found", &msg, Some(("/devices", "Back to Devices"))),
    )
        .into_response()
}

fn internal_error(state: &AppState, what: &str, err: impl std::fmt::Display) -> Response {
    let msg = format!("Failed to {what}: {err}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        message_response(state, "Error", &msg, None),
    )
        .into_response()
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn list_devices(State(state): State<AppState>) -> Response {
    let base_dir = match &state.config.cfggen_base_dir {
        Some(d) if d.join("logical-devices").exists() => d,
        _ => {
            return message_response(&state, "Logical Devices", "Logical devices not configured", None);
        }
    };

    let devices_dir = base_dir.join("logical-devices");
    debug!(path = %devices_dir.display(), "Listing logical devices");

    let entries = match std::fs::read_dir(&devices_dir) {
        Ok(e) => e,
        Err(err) => {
            warn!(path = %devices_dir.display(), error = %err, "Failed to read logical-devices directory");
            let msg = format!("Failed to read logical devices directory: {err}");
            return message_response(&state, "Logical Devices", &msg, None);
        }
    };

    let mut rows: Vec<DeviceListRow> = Vec::new();

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
        let (name_opt, fields) = if path.is_file() {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path.file_stem().and_then(|s| s.to_str()).map(String::from);
            (name, read_device_fields(&path))
        } else if path.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).map(String::from);
            let config_path = path.join("config.json");
            (name, read_device_fields(&config_path))
        } else {
            continue;
        };

        if let Some(name) = name_opt {
            rows.push(DeviceListRow {
                name,
                hostname: fields.hostname.unwrap_or_else(|| "-".to_string()),
                role: fields.role.unwrap_or_else(|| "-".to_string()),
                description: fields.description.unwrap_or_else(|| "-".to_string()),
            });
        }
    }

    rows.sort_by(|a, b| a.name.cmp(&b.name));

    let html = state
        .templates
        .render_page(
            &state.templates.devices_list,
            "Logical Devices",
            "",
            &DevicesListCtx {
                rows,
                quicksearch_table_id: "devices-table",
            },
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

pub async fn device_detail(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let base_dir = match &state.config.cfggen_base_dir {
        Some(d) if d.join("logical-devices").exists() => d,
        _ => return not_configured(&state),
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
        return device_not_found(&state, &name);
    }

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to read device config");
            return internal_error(&state, "read device config", err);
        }
    };

    let raw_config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to parse device config JSON");
            return internal_error(&state, "parse device config", err);
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

    let ctx = build_device_detail_ctx(&name, &raw_config, &config_json, &available_services, &available_images, &candidates);

    let title = format!("Device {}", name);
    let html = state
        .templates
        .render_page(&state.templates.device_detail, &title, "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

/// Build the context for the device_detail template from the raw JSON config.
fn build_device_detail_ctx(
    name: &str,
    raw_config: &serde_json::Value,
    config_json: &str,
    available_services: &[String],
    available_images: &[String],
    candidates: &[AssignCandidate],
) -> DeviceDetailCtx {
    let hostname = raw_config
        .get("hostname")
        .or_else(|| raw_config.get("vars").and_then(|v| v.get("hostname")))
        .and_then(|v| v.as_str())
        .unwrap_or("-")
        .to_string();

    let role = raw_config
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("-")
        .to_string();

    let description = raw_config
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("-")
        .to_string();

    let current_image = raw_config
        .get("software-image")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let image_options: Vec<OptionCtx> = available_images
        .iter()
        .map(|img| OptionCtx {
            name: img.clone(),
            selected: img == &current_image,
        })
        .collect();
    let has_image_extra =
        !current_image.is_empty() && !available_images.iter().any(|i| i == &current_image);

    let serial = first_module_serial(raw_config);
    let has_serial = serial.is_some();
    let serial_display = serial.clone().unwrap_or_else(|| "-".to_string());
    let candidates_ctx: Vec<CandidateCtx> = candidates
        .iter()
        .map(|c| CandidateCtx {
            serial: c.serial.clone(),
            hostname: c.hostname.clone(),
            last_seen: c.last_seen_label.clone(),
        })
        .collect();

    // Build port assignment rows
    let mut port_rows: Vec<PortRowCtx> = Vec::new();
    if let Some(modules) = raw_config.get("modules").and_then(|m| m.as_array()) {
        for (mod_idx, module_val) in modules.iter().enumerate() {
            if module_val.is_null() {
                continue;
            }
            let sku = module_val.get("SKU").and_then(|v| v.as_str()).unwrap_or("-").to_string();
            if let Some(ports) = module_val.get("ports").and_then(|p| p.as_array()) {
                for port in ports {
                    let port_name = port.get("name").and_then(|v| v.as_str()).unwrap_or("-").to_string();
                    let current_service = port.get("service").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let options: Vec<OptionCtx> = available_services
                        .iter()
                        .map(|svc| OptionCtx {
                            name: svc.clone(),
                            selected: svc == &current_service,
                        })
                        .collect();
                    let has_extra = !current_service.is_empty()
                        && !available_services.iter().any(|s| s == &current_service);
                    port_rows.push(PortRowCtx {
                        field_name: format!("port_{mod_idx}_{port_name}"),
                        mod_idx,
                        sku: sku.clone(),
                        port_name,
                        has_extra,
                        extra_value: current_service,
                        options,
                    });
                }
            }
        }
    }

    DeviceDetailCtx {
        name: name.to_string(),
        hostname,
        role,
        description,
        serial: serial_display,
        has_serial,
        has_candidates: !candidates_ctx.is_empty(),
        candidates: candidates_ctx,
        has_image_extra,
        image_extra: current_image.clone(),
        image_options,
        has_upgrade_section: has_serial && !current_image.is_empty(),
        current_image,
        has_ports: !port_rows.is_empty(),
        port_rows,
        config_json: config_json.to_string(),
    }
}

/// Compile a logical device config into a .cfg file in target-configs.
pub(crate) fn compile_device_config(
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
                    // It's common for stack members under one logical device
                    // to all carry the master's serial (e.g. AD6-X015 has
                    // both a stub module and a real C9300 module both
                    // serial=FCW2216G054), which made /diff render
                    // "AD6-X015, AD6-X015" in the Logical Device column.
                    // Dedupe so each device appears at most once per serial.
                    let entry = map.entry(serial.to_string()).or_default();
                    if !entry.iter().any(|n| n == device_name) {
                        entry.push(device_name.clone());
                    }
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

/// Fields read from a logical device JSON for list-page display.
#[derive(Default)]
struct DeviceFields {
    hostname: Option<String>,
    role: Option<String>,
    /// Free-text description (set by switches-poll from LocationDetail).
    description: Option<String>,
}

/// Read display fields from a logical device JSON.
fn read_device_fields(path: &std::path::Path) -> DeviceFields {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "Failed to read device file");
            return DeviceFields::default();
        }
    };

    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "Failed to parse device JSON");
            return DeviceFields::default();
        }
    };

    DeviceFields {
        // hostname: top-level "hostname" key, or inside "vars"
        hostname: val
            .get("hostname")
            .or_else(|| val.get("vars").and_then(|v| v.get("hostname")))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        role: val.get("role").and_then(|v| v.as_str()).map(|s| s.to_string()),
        description: val
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn update_ports(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let base_dir = match &state.config.cfggen_base_dir {
        Some(d) if d.join("logical-devices").exists() => d,
        _ => return not_configured(&state),
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
        return device_not_found(&state, &name);
    }

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to read device config");
            return internal_error(&state, "read device config", err);
        }
    };

    let mut config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to parse device config JSON");
            return internal_error(&state, "parse device config", err);
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
            return internal_error(&state, "serialise device config", err);
        }
    };

    if let Err(err) = std::fs::write(&json_path, &updated) {
        warn!(path = %json_path.display(), error = %err, "Failed to write updated device config");
        return internal_error(&state, "write device config", err);
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
        _ => return not_configured(&state),
    };

    // Locate the device config file
    let flat_path = base_dir.join("logical-devices").join(format!("{}.json", name));
    let dir_path = base_dir.join("logical-devices").join(&name).join("config.json");
    let json_path = if flat_path.exists() {
        flat_path
    } else if dir_path.exists() {
        dir_path
    } else {
        return device_not_found(&state, &name);
    };

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to read device config");
            return internal_error(&state, "read device config", err);
        }
    };

    let mut config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to parse device config JSON");
            return internal_error(&state, "parse device config", err);
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
            return internal_error(&state, "serialise device config", err);
        }
    };

    if let Err(err) = std::fs::write(&json_path, &updated) {
        warn!(path = %json_path.display(), error = %err, "Failed to write updated device config");
        return internal_error(&state, "write device config", err);
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
            message_response(&state, "Assign Serial", "No serial selected", None),
        )
            .into_response();
    }

    let base_dir = match &state.config.cfggen_base_dir {
        Some(d) if d.join("logical-devices").exists() => d,
        _ => return not_configured(&state),
    };

    let flat_path = base_dir.join("logical-devices").join(format!("{}.json", name));
    let dir_path = base_dir.join("logical-devices").join(&name).join("config.json");
    let json_path = if flat_path.exists() {
        flat_path
    } else if dir_path.exists() {
        dir_path
    } else {
        return device_not_found(&state, &name);
    };

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to read device config");
            return internal_error(&state, "read device config", err);
        }
    };

    let mut config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to parse device config JSON");
            return internal_error(&state, "parse device config", err);
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
            return internal_error(&state, "serialise device config", err);
        }
    };

    if let Err(err) = std::fs::write(&json_path, &updated) {
        warn!(path = %json_path.display(), error = %err, "Failed to write updated device config");
        return internal_error(&state, "write device config", err);
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
        _ => return not_configured(&state),
    };

    let flat_path = base_dir.join("logical-devices").join(format!("{}.json", name));
    let dir_path = base_dir.join("logical-devices").join(&name).join("config.json");
    let json_path = if flat_path.exists() {
        flat_path
    } else if dir_path.exists() {
        dir_path
    } else {
        return device_not_found(&state, &name);
    };

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to read device config");
            return internal_error(&state, "read device config", err);
        }
    };

    let mut config: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            warn!(path = %json_path.display(), error = %err, "Failed to parse device config JSON");
            return internal_error(&state, "parse device config", err);
        }
    };

    config["software-image"] = serde_json::Value::String(form.software_image.trim().to_string());

    let updated = match serde_json::to_string_pretty(&config) {
        Ok(s) => s,
        Err(err) => {
            warn!(error = %err, "Failed to serialise updated device config");
            return internal_error(&state, "serialise device config", err);
        }
    };

    if let Err(err) = std::fs::write(&json_path, &updated) {
        warn!(path = %json_path.display(), error = %err, "Failed to write updated device config");
        return internal_error(&state, "write device config", err);
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
        _ => return not_configured(&state),
    };

    // Load device config to get serial and software-image
    let configs = load_all_device_configs(base_dir);
    let config = match configs.get(&name) {
        Some(c) => c.clone(),
        None => return device_not_found(&state, &name),
    };

    let serial = match first_module_serial(&config) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                message_response(&state, "Upgrade", "No serial assigned to this device", None),
            )
                .into_response();
        }
    };

    let image_name = match config.get("software-image").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                message_response(&state, "Upgrade", "No software image configured for this device", None),
            )
                .into_response();
        }
    };

    // Resolve the image file path
    let image_path = base_dir.join("software-images").join(&image_name);
    if !image_path.exists() {
        let msg = format!("Software image file not found: {}", image_name);
        return (
            StatusCode::BAD_REQUEST,
            message_response(&state, "Upgrade", &msg, None),
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
            let msg = format!("Device {} has no known IP address.", serial);
            let back = format!("/devices/{name}");
            return message_response(&state, "No IP Address", &msg, Some((&back, "Back")));
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
            message_response(&state, "Rename", "New name cannot be empty", None),
        )
            .into_response();
    }

    let base_dir = match &state.config.cfggen_base_dir {
        Some(d) if d.join("logical-devices").exists() => d,
        _ => return not_configured(&state),
    };

    let ld_dir = base_dir.join("logical-devices");

    // Find the source (directory or flat JSON)
    let src_dir = ld_dir.join(&name);
    let src_json = ld_dir.join(format!("{}.json", name));
    let dst_dir = ld_dir.join(&new_name);
    let dst_json = ld_dir.join(format!("{}.json", new_name));

    if dst_dir.exists() || dst_json.exists() {
        let msg = format!("A device named '{}' already exists", new_name);
        return (
            StatusCode::CONFLICT,
            message_response(&state, "Conflict", &msg, Some(("/devices", "Back"))),
        )
            .into_response();
    }

    if src_dir.exists() {
        if let Err(e) = std::fs::rename(&src_dir, &dst_dir) {
            warn!(error = %e, from = %name, to = %new_name, "Failed to rename device directory");
            return internal_error(&state, "rename", e);
        }
    } else if src_json.exists() {
        if let Err(e) = std::fs::rename(&src_json, &dst_json) {
            warn!(error = %e, from = %name, to = %new_name, "Failed to rename device JSON");
            return internal_error(&state, "rename", e);
        }
    } else {
        return device_not_found(&state, &name);
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

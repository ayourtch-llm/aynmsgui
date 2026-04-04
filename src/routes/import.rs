use axum::{
    extract::{Form, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use regex::Regex;
use serde::Deserialize;
use tracing::{info, warn};

use crate::state::AppState;

// ── Form struct ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ImportForm {
    ip: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn import_page(State(_state): State<AppState>) -> Response {
    let html = r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Import Device</title></head>
<body>
<h1>Import Device</h1>
<p>Connect to a Cisco IOS device via SSH to discover and register it in the asset inventory.</p>
<form method="POST" action="/import">
  <label for="ip">IP Address:</label><br>
  <input type="text" id="ip" name="ip" required placeholder="192.168.1.1"><br><br>
  <button type="submit">Import Device</button>
</form>
<p><a href="/assets">Back to Assets</a></p>
</body>
</html>"#;

    Html(html).into_response()
}

pub async fn import_device(
    State(state): State<AppState>,
    Form(form): Form<ImportForm>,
) -> Response {
    // Validate inputs
    if form.ip.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html(
                r#"<!DOCTYPE html>
<html><body>
<h1>Import Error</h1>
<p>IP address is required.</p>
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
    info!(ip = %ip, "Starting device import via SSH");

    // 1. Connect to device via SSH (direct or via jumphost)
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

    // Also collect commands needed for config extraction pipeline
    let show_ip_brief = conn.run_cmd("show ip interface brief").await.unwrap_or_default();
    let show_intf_status = conn.run_cmd("show interfaces status").await.unwrap_or_default();
    let show_running_raw = conn.run_cmd("show running-config").await.unwrap_or_default();
    // Strip noise lines (Load for five secs, Time source, Building configuration, etc.)
    let show_running = aycfgapply::normalize::normalize_config(&show_running_raw);

    // Log discovered interfaces for debugging
    let iface_entries = aycfggen::show_parsers::parse_show_ip_interface_brief(&show_ip_brief);
    let iface_names: Vec<&str> = iface_entries.iter().map(|e| e.name.as_str()).collect();
    info!(ip = %ip, count = iface_entries.len(), interfaces = ?iface_names,
        "Discovered interfaces from show ip interface brief");
    let running_ifaces: Vec<&str> = show_running.lines()
        .filter(|l| l.starts_with("interface "))
        .map(|l| l.strip_prefix("interface ").unwrap_or(l).trim())
        .collect();
    info!(ip = %ip, count = running_ifaces.len(), interfaces = ?running_ifaces,
        "Interfaces in running-config");

    // Collect chassis MAC (fallback for IOS routers without base MAC in show version)
    // Try "show diag" first, then "show diag all eeprom detail | inc Chassis MAC"
    let show_diag = conn.run_cmd("show diag").await.unwrap_or_default();
    let mut fallback_mac = ayciam::parse_chassis_mac(&show_diag);
    if fallback_mac.is_none() {
        let show_diag_eeprom = conn.run_cmd("show diag all eeprom detail | inc Chassis MAC").await.unwrap_or_default();
        fallback_mac = ayciam::parse_chassis_mac(&show_diag_eeprom);
        if fallback_mac.is_none() && !show_diag_eeprom.trim().is_empty() {
            info!(ip = %ip, output = %show_diag_eeprom.trim(), "show diag eeprom output (no MAC parsed)");
        }
        if fallback_mac.is_none() {
            info!(ip = %ip, "No chassis MAC found in show diag or show diag eeprom");
        }
    }

    let _ = conn.disconnect().await;

    // 3. Parse outputs into DeviceMetadata
    let metadata_list = match ayciam::build_metadata_with_fallback_mac(
        &show_version,
        &show_inventory,
        "ANY",
        fallback_mac.as_deref(),
    ) {
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

                // Update seen assets so /retrieve and /software can find this device
                state.register_seen_asset(
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

    // 5. Run config extraction pipeline + auto-name logical device
    if let Some(sv_info) = aycfggen::show_parsers::parse_show_version(&show_version) {
        let hostname = &sv_info.hostname;

        // Register seen asset with hostname
        if !sv_info.serial_number.is_empty() {
            state.register_seen_asset(
                &sv_info.serial_number,
                &ip,
                if hostname.is_empty() { None } else { Some(hostname.as_str()) },
                if sv_info.platform.is_empty() { None } else { Some(sv_info.platform.as_str()) },
                if sv_info.software_image.is_empty() { None } else { Some(sv_info.software_image.as_str()) },
            ).await;

            // Save normalized running-config to current-configs/{serial}.cfg
            if !show_running.is_empty() {
                if let Some(ref current_dir) = state.config.current_configs_path {
                    let _ = std::fs::create_dir_all(current_dir);
                    let cfg_path = current_dir.join(format!("{}.cfg", sv_info.serial_number));
                    match std::fs::write(&cfg_path, &show_running) {
                        Ok(()) => {
                            info!(serial = %sv_info.serial_number, path = %cfg_path.display(),
                                "Saved current config during import");
                            results_html.push_str(&format!(
                                "<p>Current config saved to <code>{}</code></p>",
                                html_escape(&cfg_path.display().to_string()),
                            ));
                        }
                        Err(e) => {
                            warn!(error = %e, path = %cfg_path.display(), "Failed to save current config");
                        }
                    }
                }
            }
        }

        // Determine logical device name: if hostname matches the naming convention,
        // use the prefix; otherwise use the full hostname.
        let hostname_re = Regex::new(r"^([A-Za-z0-9]+-X?[0-9]{3})-(S[0-9]{4,5})$").unwrap();
        let logical_name = if let Some(caps) = hostname_re.captures(hostname) {
            let name = caps[1].to_string();
            let asset_tag = &caps[2];
            let tag_matches = state.asset_cache.as_ref()
                .map(|cache| !cache.lookup_by_asset_tag(asset_tag).is_empty())
                .unwrap_or(false);
            if tag_matches {
                info!(hostname = %hostname, logical_name = %name, asset_tag = %asset_tag,
                    "Hostname matches naming convention, auto-naming logical device");
                results_html.push_str(&format!(
                    "<p>Auto-detected logical device name: <strong>{}</strong> (asset tag {} matches)</p>",
                    html_escape(&name), html_escape(asset_tag),
                ));
                name
            } else {
                hostname.to_string()
            }
        } else {
            hostname.to_string()
        };

        // Run the extraction pipeline with all collected show commands
        if let Some(ref base_dir) = state.config.cfggen_base_dir {
            let ld_dir = base_dir.join("logical-devices");
            let collected = format!(
                "!!! aycfgextract: show version !!!\n{}\n\
                 !!! aycfgextract: show inventory !!!\n{}\n\
                 !!! aycfgextract: show ip interface brief !!!\n{}\n\
                 !!! aycfgextract: show interfaces status !!!\n{}\n\
                 !!! aycfgextract: show running-config !!!\n{}\n",
                show_version, show_inventory, show_ip_brief, show_intf_status, show_running
            );
            let saved_path = std::path::PathBuf::from(format!("/tmp/aynmsgui-import-{}.txt", ip));
            if let Ok(()) = std::fs::write(&saved_path, &collected) {
                let dirs = aycfggen::extract_cli::ResolvedExtractDirs {
                    hardware_templates: base_dir.join("hardware-templates"),
                    logical_devices: ld_dir.clone(),
                    services: base_dir.join("services"),
                    config_templates: base_dir.join("config-templates"),
                    config_elements: base_dir.join("config-elements"),
                    configs: base_dir.join("configs"),
                };

                for subdir in [&dirs.hardware_templates, &dirs.logical_devices, &dirs.services,
                              &dirs.config_templates, &dirs.config_elements, &dirs.configs] {
                    let _ = std::fs::create_dir_all(subdir);
                }

                let saved = saved_path.clone();
                let extract_result = tokio::task::spawn_blocking(move || {
                    aycfggen::extract_cli::run_extract_offline(
                        &saved, &dirs, None, false, false, &[],
                    )
                })
                .await;

                match extract_result {
                    Ok(Ok(())) => {
                        // Extraction may create config under the hostname or serial.
                        // Find which one it created, then rename to logical_name.
                        let serial = sv_info.serial_number.as_str();
                        let candidates = [hostname.as_str(), serial];

                        let mut source_found = None;
                        for candidate in &candidates {
                            let cand_dir = ld_dir.join(candidate);
                            let cand_json = ld_dir.join(format!("{}.json", candidate));
                            if cand_dir.exists() || cand_json.exists() {
                                if *candidate != logical_name {
                                    source_found = Some(candidate.to_string());
                                }
                                break;
                            }
                        }

                        if let Some(source_name) = source_found {
                            let full_dir = ld_dir.join(&source_name);
                            let full_json = ld_dir.join(format!("{}.json", source_name));
                            let target_dir = ld_dir.join(&logical_name);
                            let target_json = ld_dir.join(format!("{}.json", logical_name));

                            if full_dir.exists() {
                                if target_dir.exists() {
                                    // Target exists — merge by removing old and renaming new
                                    let _ = std::fs::remove_dir_all(&target_dir);
                                }
                                if let Err(e) = std::fs::rename(&full_dir, &target_dir) {
                                    warn!(error = %e, "Failed to rename logical device directory");
                                } else {
                                    info!(from = %source_name, to = %logical_name, "Renamed logical device directory");
                                }
                            } else if full_json.exists() {
                                if target_json.exists() {
                                    let _ = std::fs::remove_file(&target_json);
                                }
                                if let Err(e) = std::fs::rename(&full_json, &target_json) {
                                    warn!(error = %e, "Failed to rename logical device JSON");
                                } else {
                                    info!(from = %source_name, to = %logical_name, "Renamed logical device JSON");
                                }
                            }
                        }

                        // Compile the extracted config to produce target .cfg files
                        match crate::routes::devices::compile_device_config(&logical_name, base_dir, &state.config) {
                            Ok(()) => {
                                info!(name = %logical_name, "Config compiled after import");
                                results_html.push_str(&format!(
                                    "<p style='color:green'>Config extracted and compiled for logical device <strong>{}</strong>.</p>",
                                    html_escape(&logical_name),
                                ));
                            }
                            Err(e) => {
                                warn!(name = %logical_name, error = %e, "Config compilation failed after import");
                                results_html.push_str(&format!(
                                    "<p style='color:green'>Config extracted for logical device <strong>{}</strong>.</p>\
                                     <p style='color:orange'>Compilation failed: {}</p>",
                                    html_escape(&logical_name),
                                    html_escape(&format!("{e}")),
                                ));
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(logical_name = %logical_name, error = %e, "Extraction pipeline failed");
                        results_html.push_str(&format!(
                            "<p style='color:orange'>Config extraction failed: {}</p>",
                            html_escape(&format!("{e}")),
                        ));
                    }
                    Err(e) => {
                        warn!(error = %e, "spawn_blocking panicked during extraction");
                    }
                }
            }
        }
    }

    // 6. Invalidate asset cache so the new records show up
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
    }

    #[tokio::test]
    async fn test_import_empty_ip_returns_error() {
        let app = build_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/import")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from("ip="))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

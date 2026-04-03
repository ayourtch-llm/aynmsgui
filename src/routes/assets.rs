use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use indexmap::IndexMap;
use serde::Serialize;
use tracing::debug;

use crate::routes::devices::{load_all_device_configs, serial_to_device_names};
use crate::state::AppState;

// ── View models ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct AssetView {
    pub serial: String,
    pub asset_tag: String,
    pub vendor: String,
    pub sku: String,
    pub platform: String,
    pub hostname: Option<String>,
    pub last_ipv4: Option<String>,
    pub last_ipv6: Option<String>,
    pub last_seen: Option<String>,
}

#[derive(Serialize)]
pub struct AssetDetailView {
    pub serial: String,
    pub asset_tag: String,
    pub vendor: String,
    pub flavor: String,
    pub sku: String,
    pub platform: String,
    pub mac_addresses: Vec<String>,
    pub modules: Vec<ModuleView>,
    pub hostname: Option<String>,
    pub model: Option<String>,
    pub last_ipv4: Option<String>,
    pub last_ipv6: Option<String>,
    pub last_seen_ipv4: Option<String>,
    pub last_seen_ipv6: Option<String>,
    pub first_seen: Option<String>,
    pub registered_at: String,
    /// HTML-formatted links to logical devices that reference this serial.
    pub logical_devices_html: String,
}

#[derive(Serialize)]
pub struct ModuleView {
    pub sku: String,
    pub serial: String,
    pub mac_address: Option<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Read all AssetRecords from a JSONL file (one JSON object per line).
/// Blank lines are skipped. Malformed lines are logged and skipped.
fn read_all_records(path: &std::path::Path) -> Vec<ayciam::AssetRecord> {
    match std::fs::read_to_string(path) {
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "Failed to read inventory file");
            vec![]
        }
        Ok(content) => {
            let mut records = Vec::new();
            for (lineno, line) in content.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<ayciam::AssetRecord>(line) {
                    Ok(r) => records.push(r),
                    Err(e) => {
                        tracing::warn!(
                            lineno = lineno + 1,
                            error = %e,
                            "Skipping malformed JSONL line in inventory"
                        );
                    }
                }
            }
            records
        }
    }
}

/// Format logical device names as HTML links, or "-" if none.
fn format_device_links(serial_map: &IndexMap<String, Vec<String>>, serial: &str) -> String {
    match serial_map.get(serial) {
        Some(names) if !names.is_empty() => names
            .iter()
            .map(|n| format!("<a href=\"/devices/{name}\">{name}</a>", name = n))
            .collect::<Vec<_>>()
            .join(", "),
        _ => "-".to_string(),
    }
}

/// Format an IP address with a hover tooltip showing the last-seen timestamp.
fn format_ip_with_timestamp(ip: Option<&str>, last_seen: Option<&str>) -> String {
    match ip {
        Some(addr) => {
            let title = match last_seen {
                Some(ts) => format!(" title=\"Last seen: {}\"", ts),
                None => String::new(),
            };
            format!("<span{}>{}</span>", title, addr)
        }
        None => "-".to_string(),
    }
}

/// Render the asset list table rows from filtered records.
fn render_asset_rows(
    records: &[ayciam::AssetRecord],
    seen_assets: &indexmap::IndexMap<String, aycallhome::Device>,
    serial_map: &IndexMap<String, Vec<String>>,
) -> String {
    records
        .iter()
        .map(|r| {
            let device = seen_assets.get(&r.serial_number);
            let hostname = device
                .and_then(|d| d.hostname.as_deref())
                .unwrap_or("-");
            let last_ipv4 = format_ip_with_timestamp(
                device.and_then(|d| d.last_ipv4.as_deref()),
                device
                    .and_then(|d| d.last_seen_ipv4)
                    .map(|t| t.to_rfc3339())
                    .as_deref(),
            );
            let last_ipv6 = format_ip_with_timestamp(
                device.and_then(|d| d.last_ipv6.as_deref()),
                device
                    .and_then(|d| d.last_seen_ipv6)
                    .map(|t| t.to_rfc3339())
                    .as_deref(),
            );
            let logical_devices = format_device_links(serial_map, &r.serial_number);
            format!(
                "<tr><td><a href=\"/assets/{serial}\">{serial}</a></td>\
                 <td>{asset_tag}</td><td>{vendor}</td><td>{sku}</td>\
                 <td>{platform}</td><td>{hostname}</td><td>{last_ipv4}</td><td>{last_ipv6}</td>\
                 <td>{logical_devices}</td></tr>",
                serial = r.serial_number,
                asset_tag = r.asset_tag,
                vendor = r.vendor,
                sku = r.sku,
                platform = r.platform.as_deref().unwrap_or("-"),
                hostname = hostname,
                last_ipv4 = last_ipv4,
                last_ipv6 = last_ipv6,
                logical_devices = logical_devices,
            )
        })
        .collect()
}

/// Render the full asset list page HTML.
fn render_asset_list_page(title: &str, heading: &str, rows: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>{title}</title></head>
<body>
<h1>{heading}</h1>
<table>
<tr><th>Serial</th><th>Asset Tag</th><th>Vendor</th><th>SKU</th><th>Platform</th><th>Hostname</th><th>Last IPv4</th><th>Last IPv6</th><th>Logical Device</th></tr>
{rows}
</table>
</body>
</html>"#,
        title = title,
        heading = heading,
        rows = rows,
    )
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn list_assets(State(state): State<AppState>) -> Response {
    let (Some(_cache), Some(inv_path)) = (&state.asset_cache, &state.asset_inventory_path) else {
        let html = "<html><body><p>Asset inventory not configured</p></body></html>";
        return Html(html).into_response();
    };

    debug!(path = %inv_path.display(), "Loading all assets for list view");

    let records = read_all_records(inv_path);
    let seen_assets = state.seen_assets.read().await;

    let serial_map = state
        .config
        .cfggen_base_dir
        .as_ref()
        .map(|base| serial_to_device_names(&load_all_device_configs(base)))
        .unwrap_or_default();

    let rows = render_asset_rows(&records, &seen_assets, &serial_map);
    let html = render_asset_list_page("Assets", "Asset Inventory", &rows);
    Html(html).into_response()
}

pub async fn list_seen_assets(State(state): State<AppState>) -> Response {
    let (Some(_cache), Some(inv_path)) = (&state.asset_cache, &state.asset_inventory_path) else {
        let html = "<html><body><p>Asset inventory not configured</p></body></html>";
        return Html(html).into_response();
    };

    debug!(path = %inv_path.display(), "Loading seen assets for list view");

    let records = read_all_records(inv_path);
    let seen_assets = state.seen_assets.read().await;

    // Filter to only assets that have been seen
    let seen_records: Vec<ayciam::AssetRecord> = records
        .into_iter()
        .filter(|r| seen_assets.contains_key(&r.serial_number))
        .collect();

    let serial_map = state
        .config
        .cfggen_base_dir
        .as_ref()
        .map(|base| serial_to_device_names(&load_all_device_configs(base)))
        .unwrap_or_default();

    let rows = render_asset_rows(&seen_records, &seen_assets, &serial_map);
    let html = render_asset_list_page("Seen Assets", "Seen Assets", &rows);
    Html(html).into_response()
}

pub async fn asset_detail(
    State(state): State<AppState>,
    Path(serial): Path<String>,
) -> Response {
    let Some(cache) = &state.asset_cache else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Html("<html><body><p>Asset inventory not configured</p></body></html>"),
        )
            .into_response();
    };

    debug!(serial = %serial, "Looking up asset detail");

    let records = cache.lookup_by_serial(&serial);
    let Some(record) = records.into_iter().next() else {
        return (
            StatusCode::NOT_FOUND,
            Html(format!(
                "<html><body><p>Asset {} not found</p></body></html>",
                serial
            )),
        )
            .into_response();
    };

    let seen_assets = state.seen_assets.read().await;
    let device = seen_assets.get(&record.serial_number);

    // Build serial → logical device name(s) mapping
    let serial_map = state
        .config
        .cfggen_base_dir
        .as_ref()
        .map(|base| serial_to_device_names(&load_all_device_configs(base)))
        .unwrap_or_default();

    let modules: Vec<ModuleView> = record
        .modules
        .iter()
        .map(|m| ModuleView {
            sku: m.sku.clone(),
            serial: m.serial_number.clone(),
            mac_address: m.mac_address.clone(),
        })
        .collect();

    let detail = AssetDetailView {
        serial: record.serial_number.clone(),
        asset_tag: record.asset_tag.clone(),
        vendor: record.vendor.clone(),
        flavor: record.flavor.clone(),
        sku: record.sku.clone(),
        platform: record.platform.clone().unwrap_or_default(),
        mac_addresses: record.mac_addresses.clone(),
        modules,
        hostname: device.and_then(|d| d.hostname.clone()),
        model: device.and_then(|d| d.model.clone()),
        last_ipv4: device.and_then(|d| d.last_ipv4.clone()),
        last_ipv6: device.and_then(|d| d.last_ipv6.clone()),
        last_seen_ipv4: device
            .and_then(|d| d.last_seen_ipv4)
            .map(|t| t.to_rfc3339()),
        last_seen_ipv6: device
            .and_then(|d| d.last_seen_ipv6)
            .map(|t| t.to_rfc3339()),
        first_seen: device
            .and_then(|d| d.first_seen)
            .map(|t| t.to_rfc3339()),
        registered_at: record.registered_at.clone(),
        logical_devices_html: format_device_links(&serial_map, &record.serial_number),
    };

    let html = render_asset_detail(&detail);
    Html(html).into_response()
}

fn render_asset_detail(d: &AssetDetailView) -> String {
    let modules_html: String = d
        .modules
        .iter()
        .map(|m| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                m.sku,
                m.serial,
                m.mac_address.as_deref().unwrap_or("-")
            )
        })
        .collect();

    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Asset {serial}</title></head>
<body>
<h1>Asset Detail: {serial}</h1>
<table>
<tr><th>Serial</th><td>{serial}</td></tr>
<tr><th>Asset Tag</th><td>{asset_tag}</td></tr>
<tr><th>Vendor</th><td>{vendor}</td></tr>
<tr><th>Flavor</th><td>{flavor}</td></tr>
<tr><th>SKU</th><td>{sku}</td></tr>
<tr><th>Platform</th><td>{platform}</td></tr>
<tr><th>Registered At</th><td>{registered_at}</td></tr>
<tr><th>Hostname</th><td>{hostname}</td></tr>
<tr><th>Model</th><td>{model}</td></tr>
<tr><th>Last IPv4</th><td>{last_ipv4}</td></tr>
<tr><th>Last IPv6</th><td>{last_ipv6}</td></tr>
<tr><th>Logical Device(s)</th><td>{logical_devices}</td></tr>
</table>
<h2>Modules</h2>
<table>
<tr><th>SKU</th><th>Serial</th><th>MAC</th></tr>
{modules}
</table>
</body>
</html>"#,
        serial = d.serial,
        asset_tag = d.asset_tag,
        vendor = d.vendor,
        flavor = d.flavor,
        sku = d.sku,
        platform = d.platform,
        registered_at = d.registered_at,
        hostname = d.hostname.as_deref().unwrap_or("-"),
        model = d.model.as_deref().unwrap_or("-"),
        last_ipv4 = format_ip_with_timestamp(d.last_ipv4.as_deref(), d.last_seen_ipv4.as_deref()),
        last_ipv6 = format_ip_with_timestamp(d.last_ipv6.as_deref(), d.last_seen_ipv6.as_deref()),
        logical_devices = d.logical_devices_html,
        modules = modules_html,
    )
}

// ── Routes ───────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/assets", get(list_assets))
        .route("/assets/{serial}", get(asset_detail))
        .route("/seen", get(list_seen_assets))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
    };
    use clap::Parser;
    use indexmap::IndexMap;
    use std::path::PathBuf;
    use tower::ServiceExt;

    use aycallhome::Device;
    use ayciam::{AssetCache, AssetRecord, ModuleRecord};

    use crate::auth::htpasswd::HtpasswdStore;
    use crate::config::AppConfig;
    use crate::state::AppState;

    fn make_test_config() -> AppConfig {
        AppConfig::try_parse_from(["aynmsgui", "--htpasswd-file", "/dev/null"])
            .expect("test config parse")
    }

    fn make_test_htpasswd() -> HtpasswdStore {
        // A dummy store — we don't need to verify credentials in asset tests.
        HtpasswdStore::from_str("")
    }

    fn make_asset_record(serial: &str) -> AssetRecord {
        AssetRecord {
            asset_tag: format!("TAG-{}", serial),
            vendor: "Cisco".to_string(),
            flavor: "router".to_string(),
            mac_addresses: vec!["AA:BB:CC:DD:EE:FF".to_string()],
            radio_mac_addresses: vec![],
            sku: "C9300-48P".to_string(),
            serial_number: serial.to_string(),
            platform: Some("C9300".to_string()),
            owner: "test-owner".to_string(),
            registered_at: "2024-01-01T00:00:00Z".to_string(),
            modules: vec![ModuleRecord {
                sku: "MOD-SKU".to_string(),
                serial_number: "MOD-SN-001".to_string(),
                mac_address: Some("AA:BB:CC:DD:EE:00".to_string()),
            }],
        }
    }

    fn make_test_device(serial: &str) -> Device {
        Device {
            serial: serial.to_string(),
            version: Some("17.03".to_string()),
            hostname: Some(format!("router-{}", serial)),
            model: Some("C9300".to_string()),
            token: None,
            last_ipv4: Some("10.0.0.1".to_string()),
            last_ipv6: None,
            last_seen_ipv4: None,
            last_seen_ipv6: None,
            first_seen: None,
        }
    }

    fn write_jsonl(path: &std::path::Path, records: &[AssetRecord]) {
        let content: String = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, content).unwrap();
    }

    fn build_test_app_no_cache() -> axum::Router {
        let state = AppState::new(
            make_test_config(),
            make_test_htpasswd(),
            None,
            IndexMap::new(),
        );
        routes().with_state(state)
    }

    fn build_test_app_with_cache(
        inv_path: PathBuf,
        cache: AssetCache,
        seen_assets: IndexMap<String, Device>,
    ) -> axum::Router {
        let state = AppState::new(
            make_test_config(),
            make_test_htpasswd(),
            Some((cache, inv_path)),
            seen_assets,
        );
        routes().with_state(state)
    }

    // ── Test 1: list not configured ──────────────────────────────────────────

    #[tokio::test]
    async fn test_assets_list_not_configured() {
        let app = build_test_app_no_cache();

        let req = Request::builder()
            .method(Method::GET)
            .uri("/assets")
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

    // ── Test 2: list with data ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_assets_list_with_data() {
        let dir = tempfile::TempDir::new().unwrap();
        let inv_path = dir.path().join("inventory.jsonl");

        let records = vec![
            make_asset_record("SN-ALPHA"),
            make_asset_record("SN-BETA"),
        ];
        write_jsonl(&inv_path, &records);

        let cache = AssetCache::new(inv_path.clone()).unwrap();
        let app = build_test_app_with_cache(inv_path, cache, IndexMap::new());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/assets")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(
            body.contains("SN-ALPHA"),
            "expected SN-ALPHA in body, got: {}",
            body
        );
        assert!(
            body.contains("SN-BETA"),
            "expected SN-BETA in body, got: {}",
            body
        );
    }

    // ── Test 3: asset detail found ───────────────────────────────────────────

    #[tokio::test]
    async fn test_asset_detail_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let inv_path = dir.path().join("inventory.jsonl");

        let records = vec![make_asset_record("SN-DETAIL-001")];
        write_jsonl(&inv_path, &records);

        let cache = AssetCache::new(inv_path.clone()).unwrap();

        let mut seen_assets = IndexMap::new();
        seen_assets.insert(
            "SN-DETAIL-001".to_string(),
            make_test_device("SN-DETAIL-001"),
        );

        let app = build_test_app_with_cache(inv_path, cache, seen_assets);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/assets/SN-DETAIL-001")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(
            body.contains("SN-DETAIL-001"),
            "expected serial in body, got: {}",
            body
        );
        assert!(
            body.contains("TAG-SN-DETAIL-001"),
            "expected asset_tag in body, got: {}",
            body
        );
        assert!(
            body.contains("router-SN-DETAIL-001"),
            "expected hostname from callhome in body, got: {}",
            body
        );
    }

    // ── Test 4: asset detail not found ───────────────────────────────────────

    #[tokio::test]
    async fn test_asset_detail_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let inv_path = dir.path().join("inventory.jsonl");

        let records = vec![make_asset_record("SN-EXISTS")];
        write_jsonl(&inv_path, &records);

        let cache = AssetCache::new(inv_path.clone()).unwrap();
        let app = build_test_app_with_cache(inv_path, cache, IndexMap::new());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/assets/SN-UNKNOWN-XYZ")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "expected 404 for unknown serial"
        );
    }
}

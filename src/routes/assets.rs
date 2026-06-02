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
use crate::routes::message_response;
use crate::state::AppState;

// ── View models ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AssetListRow {
    serial: String,
    asset_tag: String,
    vendor: String,
    sku: String,
    platform: String,
    hostname: String,
    has_ipv4: bool,
    last_ipv4: String,
    last_seen_ipv4: String,
    has_ipv6: bool,
    last_ipv6: String,
    last_seen_ipv6: String,
    /// Pre-rendered HTML for the device links cell (raw, embedded via {{{}}}).
    logical_devices_html: String,
}

#[derive(Serialize)]
struct AssetListCtx {
    heading: String,
    rows: Vec<AssetListRow>,
}

#[derive(Serialize)]
struct ModuleView {
    sku: String,
    serial: String,
    mac_address: String,
}

#[derive(Serialize)]
struct AssetDetailCtx {
    serial: String,
    asset_tag: String,
    vendor: String,
    flavor: String,
    sku: String,
    platform: String,
    registered_at: String,
    hostname: String,
    model: String,
    has_ipv4: bool,
    last_ipv4: String,
    last_seen_ipv4: String,
    last_seen_ipv4_display: String,
    has_ipv6: bool,
    last_ipv6: String,
    last_seen_ipv6: String,
    last_seen_ipv6_display: String,
    /// Pre-rendered HTML for the logical device links cell.
    logical_devices_html: String,
    modules: Vec<ModuleView>,
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
/// Names are alphanumeric + dashes by convention; emit as-is (no escaping needed).
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

fn build_rows(
    records: &[ayciam::AssetRecord],
    seen_assets: &indexmap::IndexMap<String, aycallhome::Device>,
    serial_map: &IndexMap<String, Vec<String>>,
) -> Vec<AssetListRow> {
    records
        .iter()
        .map(|r| {
            let device = seen_assets.get(&r.serial_number);
            let last_seen_ipv4 = device
                .and_then(|d| d.last_seen_ipv4)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            let last_seen_ipv6 = device
                .and_then(|d| d.last_seen_ipv6)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            let last_ipv4 = device.and_then(|d| d.last_ipv4.clone());
            let last_ipv6 = device.and_then(|d| d.last_ipv6.clone());
            AssetListRow {
                serial: r.serial_number.clone(),
                asset_tag: r.asset_tag.clone(),
                vendor: r.vendor.clone(),
                sku: r.sku.clone(),
                platform: r.platform.clone().unwrap_or_else(|| "-".to_string()),
                hostname: device
                    .and_then(|d| d.hostname.clone())
                    .unwrap_or_else(|| "-".to_string()),
                has_ipv4: last_ipv4.is_some(),
                last_ipv4: last_ipv4.unwrap_or_default(),
                last_seen_ipv4,
                has_ipv6: last_ipv6.is_some(),
                last_ipv6: last_ipv6.unwrap_or_default(),
                last_seen_ipv6,
                logical_devices_html: format_device_links(serial_map, &r.serial_number),
            }
        })
        .collect()
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn list_assets(State(state): State<AppState>) -> Response {
    let (Some(_cache), Some(inv_path)) = (&state.asset_cache, &state.asset_inventory_path) else {
        return message_response(&state, "Assets", "Asset inventory not configured", None);
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

    let rows = build_rows(&records, &seen_assets, &serial_map);
    let ctx = AssetListCtx {
        heading: "Asset Inventory".to_string(),
        rows,
    };
    let html = state
        .templates
        .render_page(&state.templates.assets_list, "Assets", "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

pub async fn list_seen_assets(State(state): State<AppState>) -> Response {
    let (Some(_cache), Some(inv_path)) = (&state.asset_cache, &state.asset_inventory_path) else {
        return message_response(&state, "Seen Assets", "Asset inventory not configured", None);
    };

    debug!(path = %inv_path.display(), "Loading seen assets for list view");

    let records = read_all_records(inv_path);
    let seen_assets = state.seen_assets.read().await;

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

    let rows = build_rows(&seen_records, &seen_assets, &serial_map);
    let ctx = AssetListCtx {
        heading: "Seen Assets".to_string(),
        rows,
    };
    let html = state
        .templates
        .render_page(&state.templates.assets_list, "Seen Assets", "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

pub async fn asset_detail(
    State(state): State<AppState>,
    Path(serial): Path<String>,
) -> Response {
    let Some(cache) = &state.asset_cache else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            message_response(&state, "Assets", "Asset inventory not configured", None),
        )
            .into_response();
    };

    debug!(serial = %serial, "Looking up asset detail");

    let records = cache.lookup_by_serial(&serial);
    let Some(record) = records.into_iter().next() else {
        let msg = format!("Asset {} not found", serial);
        return (
            StatusCode::NOT_FOUND,
            message_response(&state, "Not Found", &msg, Some(("/assets", "Back to Assets"))),
        )
            .into_response();
    };

    let seen_assets = state.seen_assets.read().await;
    let device = seen_assets.get(&record.serial_number);

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
            mac_address: m.mac_address.clone().unwrap_or_else(|| "-".to_string()),
        })
        .collect();

    let last_ipv4 = device.and_then(|d| d.last_ipv4.clone());
    let last_ipv6 = device.and_then(|d| d.last_ipv6.clone());
    let last_seen_ipv4 = device
        .and_then(|d| d.last_seen_ipv4)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default();
    let last_seen_ipv6 = device
        .and_then(|d| d.last_seen_ipv6)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default();

    let ctx = AssetDetailCtx {
        serial: record.serial_number.clone(),
        asset_tag: record.asset_tag.clone(),
        vendor: record.vendor.clone(),
        flavor: record.flavor.clone(),
        sku: record.sku.clone(),
        platform: record.platform.clone().unwrap_or_default(),
        registered_at: record.registered_at.clone(),
        hostname: device
            .and_then(|d| d.hostname.clone())
            .unwrap_or_else(|| "-".to_string()),
        model: device
            .and_then(|d| d.model.clone())
            .unwrap_or_else(|| "-".to_string()),
        has_ipv4: last_ipv4.is_some(),
        last_ipv4: last_ipv4.unwrap_or_default(),
        last_seen_ipv4_display: if last_seen_ipv4.is_empty() {
            "-".to_string()
        } else {
            last_seen_ipv4.clone()
        },
        last_seen_ipv4,
        has_ipv6: last_ipv6.is_some(),
        last_ipv6: last_ipv6.unwrap_or_default(),
        last_seen_ipv6_display: if last_seen_ipv6.is_empty() {
            "-".to_string()
        } else {
            last_seen_ipv6.clone()
        },
        last_seen_ipv6,
        logical_devices_html: format_device_links(&serial_map, &record.serial_number),
        modules,
    };

    let title = format!("Asset {}", record.serial_number);
    let html = state
        .templates
        .render_page(&state.templates.asset_detail, &title, "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
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

use axum::{
    extract::State,
    response::Html,
    routing::get,
    Router,
};
use serde::Serialize;
use tracing::debug;

use crate::state::AppState;

#[derive(Serialize)]
struct DashboardCtx {
    asset_count: String,
    seen_count: usize,
    device_count: String,
    assignment_count: usize,
    config_count: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

pub async fn dashboard(State(state): State<AppState>) -> Html<String> {
    // Count assets from JSONL inventory
    let asset_count = if let Some(ref path) = state.asset_inventory_path {
        std::fs::read_to_string(path.as_ref())
            .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
            .to_string()
    } else {
        "N/A".to_string()
    };

    // Count seen assets
    let seen_count = state.seen_assets.read().await.len();

    // Count logical devices (JSON files in cfggen_base_dir/logical-devices/)
    let device_count = if let Some(ref dir) = state.config.cfggen_base_dir {
        std::fs::read_dir(dir.join("logical-devices"))
            .map(|entries| entries.filter_map(|e| e.ok()).count())
            .unwrap_or(0)
            .to_string()
    } else {
        "N/A".to_string()
    };

    // Count assignments
    let assignment_count = state.assignments.read().await.all_assignments().len();

    // Count target .cfg files (proxy for pending diffs)
    let config_count = if let Some(ref dir) = state.config.target_configs_path {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .map_or(false, |ext| ext == "cfg")
                    })
                    .count()
            })
            .unwrap_or(0)
            .to_string()
    } else {
        "N/A".to_string()
    };

    debug!(
        assets = %asset_count,
        seen_assets = seen_count,
        logical_devices = %device_count,
        assignments = assignment_count,
        pending_diffs = %config_count,
        "Dashboard counts"
    );

    let ctx = DashboardCtx {
        asset_count,
        seen_count,
        device_count,
        assignment_count,
        config_count,
    };

    let html = state
        .templates
        .render_page(&state.templates.dashboard, "Dashboard", "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html)
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new().route("/", get(dashboard))
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

    use ayciam::{AssetCache, AssetRecord, ModuleRecord};

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

    fn write_jsonl(path: &std::path::Path, records: &[AssetRecord]) {
        let content: String = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, content).unwrap();
    }

    async fn get_body(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        (status, body)
    }

    // ── Test 1: minimal config — all optional features absent ─────────────────

    #[tokio::test]
    async fn test_dashboard_minimal() {
        let state = AppState::new(
            make_test_config(),
            make_test_htpasswd(),
            None,
            IndexMap::new(),
        );
        let app = routes().with_state(state);

        let (status, body) = get_body(app, "/").await;
        assert_eq!(status, StatusCode::OK);

        // N/A should appear for unconfigured optional features
        assert!(
            body.contains("N/A"),
            "expected 'N/A' for unconfigured features, got: {}",
            body
        );

        // Links to the respective pages must be present
        assert!(body.contains("href=\"/assets\""), "missing /assets link");
        assert!(body.contains("href=\"/devices\""), "missing /devices link");
        assert!(body.contains("href=\"/assignments\""), "missing /assignments link");
        assert!(body.contains("href=\"/diff\""), "missing /diff link");
    }

    // ── Test 2: configured features show real counts ───────────────────────────

    #[tokio::test]
    async fn test_dashboard_with_data() {
        let dir = tempfile::TempDir::new().unwrap();

        // ── Asset inventory: 3 records ────────────────────────────────────────
        let inv_path = dir.path().join("inventory.jsonl");
        let records = vec![
            make_asset_record("SN-001"),
            make_asset_record("SN-002"),
            make_asset_record("SN-003"),
        ];
        write_jsonl(&inv_path, &records);
        let cache = AssetCache::new(inv_path.clone()).unwrap();

        // ── Logical devices directory: 2 JSON files ───────────────────────────
        let logical_devices_dir = dir.path().join("cfggen").join("logical-devices");
        std::fs::create_dir_all(&logical_devices_dir).unwrap();
        std::fs::write(
            logical_devices_dir.join("router-a.json"),
            r#"{"hostname":"router-a"}"#,
        )
        .unwrap();
        std::fs::write(
            logical_devices_dir.join("router-b.json"),
            r#"{"hostname":"router-b"}"#,
        )
        .unwrap();

        // ── Target configs directory: 2 .cfg files ────────────────────────────
        let target_dir = dir.path().join("target-configs");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("router-a.cfg"), "hostname router-a").unwrap();
        std::fs::write(target_dir.join("router-b.cfg"), "hostname router-b").unwrap();
        // Non-.cfg file — should NOT be counted
        std::fs::write(target_dir.join("readme.txt"), "ignore me").unwrap();

        // ── Known devices: 1 ─────────────────────────────────────────────────
        let mut seen_assets = IndexMap::new();
        seen_assets.insert(
            "SN-001".to_string(),
            aycallhome::Device {
                serial: "SN-001".to_string(),
                version: None,
                hostname: Some("router-sn001".to_string()),
                model: None,
                token: None,
                last_ipv4: None,
                last_ipv6: None,
                last_seen_ipv4: None,
                last_seen_ipv6: None,
                first_seen: None,
            },
        );

        // ── Build config with all optional paths set ──────────────────────────
        let config = AppConfig::try_parse_from([
            "aynmsgui",
            "--htpasswd-file",
            "/dev/null",
            "--cfggen-base-dir",
            dir.path().join("cfggen").to_str().unwrap(),
            "--target-configs-path",
            target_dir.to_str().unwrap(),
        ])
        .expect("test config parse");

        let state = AppState::new(config, make_test_htpasswd(), Some((cache, inv_path)), seen_assets);

        // Add an assignment so we can verify the count
        {
            let mut guard = state.assignments.write().await;
            guard.assign("SN-001", "router-a").unwrap();
            guard.assign("SN-002", "router-b").unwrap();
        }

        let app = routes().with_state(state);
        let (status, body) = get_body(app, "/").await;
        assert_eq!(status, StatusCode::OK);

        // Asset count: 3
        assert!(
            body.contains(">3<"),
            "expected asset count 3 in body, got: {}",
            body
        );

        // Known device count: 1
        assert!(
            body.contains(">1<"),
            "expected seen asset count 1 in body, got: {}",
            body
        );

        // Logical device count: 2
        assert!(
            body.contains(">2<"),
            "expected logical device count 2 in body, got: {}",
            body
        );

        // Assignment count: 2
        // Already checked >2< above; verify the page has no "N/A"
        // (all optional features are configured)
        assert!(
            !body.contains("N/A"),
            "expected no 'N/A' when all features configured, got: {}",
            body
        );

        // Links still present
        assert!(body.contains("href=\"/assets\""), "missing /assets link");
        assert!(body.contains("href=\"/diff\""), "missing /diff link");
    }
}

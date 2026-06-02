use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures::Stream;
use serde::Serialize;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as TokioStreamExt;
use axum::response::sse::{Event, Sse};
use tracing::{info, warn};

use crate::sse::SseEvent;
use crate::state::AppState;

#[derive(Serialize)]
struct SoftwareRow {
    serial: String,
    hostname: String,
    version: String,
}

#[derive(Serialize)]
struct SoftwareCtx {
    rows: Vec<SoftwareRow>,
}

#[derive(Serialize)]
struct UpgradeStartedCtx {
    serial: String,
    op_id: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn list_software(State(state): State<AppState>) -> Response {
    let devices = state.seen_assets.read().await;

    let rows: Vec<SoftwareRow> = devices
        .values()
        .map(|dev| SoftwareRow {
            serial: dev.serial.clone(),
            hostname: dev.hostname.clone().unwrap_or_else(|| "-".to_string()),
            version: dev.version.clone().unwrap_or_else(|| "-".to_string()),
        })
        .collect();

    let html = state
        .templates
        .render_page(
            &state.templates.software,
            "Software Versions",
            "",
            &SoftwareCtx { rows },
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

async fn start_upgrade(
    State(state): State<AppState>,
    Path(serial): Path<String>,
) -> Response {
    // Verify device exists
    {
        let devices = state.seen_assets.read().await;
        if !devices.contains_key(&serial) {
            warn!(serial = %serial, "Upgrade requested for unknown serial");
            return StatusCode::NOT_FOUND.into_response();
        }
    }

    // Create operation
    let (op_id, tx) = {
        let mut tracker = state.operations.write().await;
        tracker.create_operation()
    };

    info!(serial = %serial, op_id = %op_id, "Starting software upgrade operation");

    // Clone refs for the spawned task
    let ops = state.operations.clone();
    let serial_clone = serial.clone();
    let op_id_clone = op_id.clone();

    tokio::spawn(async move {
        // In production: call ayiosupdate_lib::upgrade::upgrade_classic_ios()
        // For now: simulate progress
        let _ = tx.send(SseEvent {
            event_type: "progress".to_string(),
            data: serde_json::json!({"phase": "starting", "serial": serial_clone}).to_string(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = tx.send(SseEvent {
            event_type: "complete".to_string(),
            data: serde_json::json!({"status": "placeholder", "serial": serial_clone}).to_string(),
        });
        // Clean up
        let mut tracker = ops.write().await;
        tracker.remove_operation(&op_id_clone);
    });

    let html = state
        .templates
        .render_page(
            &state.templates.upgrade_started,
            "Upgrade Started",
            "",
            &UpgradeStartedCtx {
                serial: serial.clone(),
                op_id: op_id.clone(),
            },
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

async fn upgrade_progress(
    State(state): State<AppState>,
    Path(op_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, axum::BoxError>>>, StatusCode> {
    let tracker = state.operations.read().await;
    let rx = tracker.subscribe(&op_id).ok_or(StatusCode::NOT_FOUND)?;
    drop(tracker);

    let stream = BroadcastStream::new(rx).filter_map(|result| {
        match result {
            Ok(event) => Some(Ok(Event::default()
                .event(event.event_type)
                .data(event.data))),
            Err(_) => None, // channel closed or lagged
        }
    });

    Ok(Sse::new(stream))
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/software", get(list_software))
        .route("/software/upgrade/{serial}", post(start_upgrade))
        .route("/software/upgrade/{op_id}/progress", get(upgrade_progress))
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

    fn make_device(serial: &str, version: Option<&str>) -> aycallhome::Device {
        aycallhome::Device {
            serial: serial.to_string(),
            version: version.map(|s| s.to_string()),
            hostname: None,
            model: None,
            token: None,
            last_ipv4: None,
            last_ipv6: None,
            last_seen_ipv4: None,
            last_seen_ipv6: None,
            first_seen: None,
        }
    }

    fn build_app_with_devices(devices: IndexMap<String, aycallhome::Device>) -> axum::Router {
        let state = AppState::new(
            make_test_config(),
            make_test_htpasswd(),
            None,
            devices,
        );
        routes().with_state(state)
    }

    // ── Test 1: GET /software returns 200 with HTML ───────────────────────────

    #[tokio::test]
    async fn test_software_list() {
        let mut devices = IndexMap::new();
        devices.insert(
            "SN-001".to_string(),
            make_device("SN-001", Some("15.2(4)M")),
        );

        let app = build_app_with_devices(devices);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/software")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(
            body.contains("SN-001"),
            "expected serial SN-001 in body, got: {}",
            body
        );
        assert!(
            body.contains("15.2(4)M"),
            "expected version in body, got: {}",
            body
        );
    }

    // ── Test 2: POST /software/upgrade/SN-001 returns 200 with operation ID ──

    #[tokio::test]
    async fn test_start_upgrade_creates_operation() {
        let mut devices = IndexMap::new();
        devices.insert(
            "SN-001".to_string(),
            make_device("SN-001", Some("15.2(4)M")),
        );

        let app = build_app_with_devices(devices);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/software/upgrade/SN-001")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "expected 200 when starting upgrade for known device"
        );

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();

        // The response should contain an operation ID (a UUID)
        assert!(
            body.contains("Operation ID"),
            "expected 'Operation ID' in body, got: {}",
            body
        );
        // Should also reference the serial
        assert!(
            body.contains("SN-001"),
            "expected serial in body, got: {}",
            body
        );
    }

    // ── Test 3: POST /software/upgrade/UNKNOWN returns 404 ───────────────────

    #[tokio::test]
    async fn test_start_upgrade_unknown_serial() {
        let app = build_app_with_devices(IndexMap::new());

        let req = Request::builder()
            .method(Method::POST)
            .uri("/software/upgrade/UNKNOWN")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "expected 404 for unknown device serial"
        );
    }

    // ── Test 4: SSE progress endpoint streams events ──────────────────────────

    #[tokio::test]
    async fn test_sse_progress_streams_events() {
        let mut devices = IndexMap::new();
        devices.insert(
            "SN-001".to_string(),
            make_device("SN-001", Some("15.2(4)M")),
        );

        // We need a shared state so the start_upgrade and upgrade_progress
        // handlers share the same OperationTracker.
        let state = AppState::new(
            make_test_config(),
            make_test_htpasswd(),
            None,
            devices,
        );
        let app = routes().with_state(state.clone());

        // Step 1: Start the upgrade to get an operation ID
        let req = Request::builder()
            .method(Method::POST)
            .uri("/software/upgrade/SN-001")
            .body(Body::empty())
            .unwrap();

        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();

        // Extract the operation ID — it appears after "Operation ID: " in the HTML
        let op_id = {
            let marker = "Operation ID: ";
            let start = body.find(marker).expect("should find 'Operation ID: ' in body") + marker.len();
            let rest = &body[start..];
            let end = rest.find('<').expect("should find closing '<' after op_id");
            rest[..end].trim().to_string()
        };

        assert!(!op_id.is_empty(), "operation ID should be non-empty");

        // Step 2: Subscribe to the SSE progress endpoint
        let sse_url = format!("/software/upgrade/{}/progress", op_id);
        let req = Request::builder()
            .method(Method::GET)
            .uri(&sse_url)
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Step 3: Read the SSE stream with a timeout
        // The spawned task sends a "progress" event, sleeps 100ms, then sends "complete"
        // and drops the sender. BroadcastStream will end when the sender is dropped.
        let body_future = axum::body::to_bytes(resp.into_body(), usize::MAX);
        let bytes = tokio::time::timeout(std::time::Duration::from_secs(5), body_future)
            .await
            .expect("SSE stream should complete within 5 seconds")
            .expect("body read should succeed");

        let body = std::str::from_utf8(&bytes).unwrap();

        // SSE format: "event: progress\n" or "event: complete\n"
        assert!(
            body.contains("event:progress") || body.contains("event: progress")
                || body.contains("event:complete") || body.contains("event: complete"),
            "expected SSE event in body, got: {:?}",
            body
        );
    }

    // ── Test 5: SSE progress for unknown op_id returns 404 ───────────────────

    #[tokio::test]
    async fn test_sse_progress_unknown_op_id() {
        let app = build_app_with_devices(IndexMap::new());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/software/upgrade/nonexistent-op-id/progress")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "expected 404 for unknown operation ID"
        );
    }
}

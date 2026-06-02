use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Router,
};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

use crate::routes::message_response;
use crate::sse::SseEvent;
use crate::state::AppState;

// ── Handlers ─────────────────────────────────────────────────────────────────

/// POST /provision/{name}
///
/// Starts a provisioning operation for a logical device. Returns 503 if
/// cfggen_base_dir is not configured, 404 if the device config file does not
/// exist, or 200 with the operation ID in the body.
pub async fn start_provision(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    // 1. Verify cfggen_base_dir is configured and exists
    let base_dir = match &state.config.cfggen_base_dir {
        Some(p) if p.join("logical-devices").exists() => p.clone(),
        _ => {
            warn!("start_provision called but cfggen_base_dir is not configured");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                message_response(&state, "Provision", "cfggen_base_dir is not configured", None),
            )
                .into_response();
        }
    };

    // 2. Verify the device config exists
    let logical_devices_dir = base_dir.join("logical-devices");
    let flat_path = logical_devices_dir.join(format!("{}.json", name));
    let dir_path = logical_devices_dir.join(&name).join("config.json");

    if !flat_path.exists() && !dir_path.exists() {
        warn!(device = %name, "Device config not found for provisioning");
        let msg = format!("Device config for '{}' not found", name);
        return (
            StatusCode::NOT_FOUND,
            message_response(&state, "Not Found", &msg, Some(("/devices", "Back to Devices"))),
        )
            .into_response();
    }

    // 3. Create operation
    let (op_id, tx) = state.operations.write().await.create_operation();
    info!(device = %name, op_id = %op_id, "Starting provisioning operation");

    // 4. Spawn task that sends simulated progress events
    let ops = state.operations.clone();
    let device_name = name.clone();
    let spawned_op_id = op_id.clone();

    tokio::spawn(async move {
        let phases = ["compiling", "connecting", "uploading", "verifying"];
        for phase in &phases {
            let _ = tx.send(SseEvent {
                event_type: "progress".to_string(),
                data: serde_json::json!({"phase": phase, "device": device_name}).to_string(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = tx.send(SseEvent {
            event_type: "complete".to_string(),
            data: serde_json::json!({"status": "success", "device": device_name}).to_string(),
        });
        // Clean up
        let mut tracker = ops.write().await;
        tracker.remove_operation(&spawned_op_id);
    });

    // 5. Return operation ID
    debug!(device = %name, op_id = %op_id, "Provisioning operation started");
    let msg = format!("Provisioning started for '{}'. Operation ID: {}", name, op_id);
    message_response(
        &state,
        "Provisioning Started",
        &msg,
        Some((&format!("/provision/{op_id}/progress"), "Watch progress")),
    )
}

/// GET /provision/{op_id}/progress
///
/// SSE endpoint: subscribes to the broadcast channel for the given operation
/// and streams events to the client. Returns 404 if the operation is not found.
pub async fn provision_progress(
    State(state): State<AppState>,
    Path(op_id): Path<String>,
) -> Response {
    let rx = match state.operations.read().await.subscribe(&op_id) {
        Some(rx) => rx,
        None => {
            warn!(op_id = %op_id, "SSE subscribe: operation not found");
            let msg = format!("Operation '{}' not found", op_id);
            return (
                StatusCode::NOT_FOUND,
                message_response(&state, "Not Found", &msg, None),
            )
                .into_response();
        }
    };

    debug!(op_id = %op_id, "SSE client connected for provisioning progress");

    let stream = BroadcastStream::new(rx)
        .filter_map(|result| {
            result.ok().map(|sse_event| {
                let event = Event::default()
                    .event(sse_event.event_type)
                    .data(sse_event.data);
                Ok::<Event, std::convert::Infallible>(event)
            })
        });

    Sse::new(stream).into_response()
}

// ── Routes ───────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/provision/{name}", post(start_provision))
        .route("/provision/{op_id}/progress", get(provision_progress))
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

    fn make_htpasswd() -> HtpasswdStore {
        HtpasswdStore::from_str("")
    }

    fn make_config_no_cfggen() -> AppConfig {
        AppConfig::try_parse_from(&[
            "aynmsgui", "--htpasswd-file", "/dev/null",
            "--cfggen-base-dir", "/nonexistent/cfggen",
        ])
            .expect("test config parse")
    }

    fn make_config_with_cfggen(base_dir: &std::path::Path) -> AppConfig {
        let base_str = base_dir.to_str().unwrap().to_string();
        AppConfig::try_parse_from(&[
            "aynmsgui",
            "--htpasswd-file",
            "/dev/null",
            "--cfggen-base-dir",
            &base_str,
        ])
        .expect("test config parse")
    }

    fn build_app(config: AppConfig) -> axum::Router {
        let state = AppState::new(config, make_htpasswd(), None, IndexMap::new());
        routes().with_state(state)
    }

    fn build_app_with_state(config: AppConfig) -> (axum::Router, AppState) {
        let state = AppState::new(config, make_htpasswd(), None, IndexMap::new());
        let app = routes().with_state(state.clone());
        (app, state)
    }

    async fn send_request(
        app: axum::Router,
        method: Method,
        uri: &str,
    ) -> (StatusCode, String) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap().to_string();
        (status, body)
    }

    // ── Test 1: 503 when cfggen_base_dir is not configured ────────────────────

    #[tokio::test]
    async fn test_provision_not_configured() {
        let config = make_config_no_cfggen();
        let app = build_app(config);

        let (status, body) = send_request(app, Method::POST, "/provision/switch-01").await;

        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "expected 503, got: {} body: {}",
            status,
            body
        );
        assert!(
            body.contains("cfggen_base_dir is not configured"),
            "expected error message in body, got: {}",
            body
        );
    }

    // ── Test 2: 404 when device config does not exist ─────────────────────────

    #[tokio::test]
    async fn test_provision_device_not_found() {
        let base_dir = tempfile::TempDir::new().unwrap();
        // Create logical-devices dir but don't put any device file in it
        std::fs::create_dir_all(base_dir.path().join("logical-devices")).unwrap();

        let config = make_config_with_cfggen(base_dir.path());
        let app = build_app(config);

        let (status, body) =
            send_request(app, Method::POST, "/provision/switch-01").await;

        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "expected 404, got: {} body: {}",
            status,
            body
        );
        assert!(
            body.contains("switch-01"),
            "expected device name in body, got: {}",
            body
        );
    }

    // ── Test 3: 200 with operation ID when device config exists ───────────────

    #[tokio::test]
    async fn test_provision_starts_operation() {
        let base_dir = tempfile::TempDir::new().unwrap();
        let ld_dir = base_dir.path().join("logical-devices");
        std::fs::create_dir_all(&ld_dir).unwrap();
        std::fs::write(
            ld_dir.join("switch-01.json"),
            r#"{"name": "switch-01"}"#,
        )
        .unwrap();

        let config = make_config_with_cfggen(base_dir.path());
        let app = build_app(config);

        let (status, body) =
            send_request(app, Method::POST, "/provision/switch-01").await;

        assert_eq!(
            status,
            StatusCode::OK,
            "expected 200, got: {} body: {}",
            status,
            body
        );
        assert!(
            body.contains("switch-01"),
            "expected device name in body, got: {}",
            body
        );
        // Body should contain an operation ID (a UUID-like string)
        assert!(
            body.contains("Operation ID:"),
            "expected 'Operation ID:' in body, got: {}",
            body
        );
    }

    // ── Test 4: SSE endpoint streams events ───────────────────────────────────

    #[tokio::test]
    async fn test_provision_sse_streams_events() {
        let base_dir = tempfile::TempDir::new().unwrap();
        let ld_dir = base_dir.path().join("logical-devices");
        std::fs::create_dir_all(&ld_dir).unwrap();
        std::fs::write(
            ld_dir.join("switch-01.json"),
            r#"{"name": "switch-01"}"#,
        )
        .unwrap();

        let config = make_config_with_cfggen(base_dir.path());
        let (app, state) = build_app_with_state(config);

        // POST to start provisioning
        let post_req = Request::builder()
            .method(Method::POST)
            .uri("/provision/switch-01")
            .body(Body::empty())
            .unwrap();
        let post_resp = app.clone().oneshot(post_req).await.unwrap();
        assert_eq!(post_resp.status(), StatusCode::OK);

        let post_bytes = axum::body::to_bytes(post_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let post_body = std::str::from_utf8(&post_bytes).unwrap();

        // Extract operation ID from response body
        let op_id = post_body
            .split("Operation ID:")
            .nth(1)
            .expect("'Operation ID:' not in response")
            .split('<')
            .next()
            .expect("could not find end of op_id")
            .trim()
            .to_string();

        assert!(!op_id.is_empty(), "operation ID should not be empty");

        // Subscribe to SSE BEFORE the spawned task can finish
        // We subscribe via state directly to get a receiver
        let mut rx = state
            .operations
            .read()
            .await
            .subscribe(&op_id)
            .expect("operation should still exist");

        // Collect events with a timeout
        let mut events: Vec<SseEvent> = Vec::new();
        let timeout = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            async {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            let is_complete = ev.event_type == "complete";
                            events.push(ev);
                            if is_complete {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            },
        )
        .await;

        assert!(timeout.is_ok(), "timed out waiting for SSE events");

        // Verify we got progress events and a complete event
        let event_types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(
            event_types.contains(&"progress"),
            "expected at least one 'progress' event, got: {:?}",
            event_types
        );
        assert!(
            event_types.contains(&"complete"),
            "expected a 'complete' event, got: {:?}",
            event_types
        );

        // Verify all events mention the device
        for ev in &events {
            assert!(
                ev.data.contains("switch-01"),
                "expected 'switch-01' in event data, got: {}",
                ev.data
            );
        }

        // Verify all four provisioning phases were emitted
        let progress_events: Vec<&SseEvent> =
            events.iter().filter(|e| e.event_type == "progress").collect();
        let phases_seen: Vec<&str> = progress_events
            .iter()
            .filter_map(|e| {
                if e.data.contains("compiling") {
                    Some("compiling")
                } else if e.data.contains("connecting") {
                    Some("connecting")
                } else if e.data.contains("uploading") {
                    Some("uploading")
                } else if e.data.contains("verifying") {
                    Some("verifying")
                } else {
                    None
                }
            })
            .collect();
        for expected_phase in &["compiling", "connecting", "uploading", "verifying"] {
            assert!(
                phases_seen.contains(expected_phase),
                "expected phase '{}' in events, phases seen: {:?}",
                expected_phase,
                phases_seen
            );
        }
    }

    // ── Test 5: SSE 404 for unknown operation ID ──────────────────────────────

    #[tokio::test]
    async fn test_provision_progress_not_found() {
        let config = make_config_no_cfggen();
        let app = build_app(config);

        let (status, _body) =
            send_request(app, Method::GET, "/provision/nonexistent-op/progress").await;

        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "expected 404 for unknown operation"
        );
    }
}

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::get,
    Router,
};
use serde::Serialize;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{debug, warn};

use crate::sse::OperationStatus;
use crate::state::AppState;

#[derive(Serialize)]
struct OpRow {
    id: String,
    op_type: String,
    device: String,
    status_style: &'static str,
    status_text: &'static str,
    started: String,
    duration: String,
    last_message: String,
    running: bool,
}

#[derive(Serialize)]
struct OpsCtx {
    ops: Vec<OpRow>,
    empty: bool,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /operations — list all active and recent operations.
pub async fn list_operations(State(state): State<AppState>) -> Response {
    let tracker = state.operations.read().await;
    let ops = tracker.list_operations();

    let now = chrono::Utc::now();
    let rows: Vec<OpRow> = ops
        .iter()
        .map(|op| {
            let (status_style, status_text) = match op.status {
                OperationStatus::Running => ("color: #2980b9; font-weight: bold", "Running"),
                OperationStatus::Complete => ("color: green", "Complete"),
                OperationStatus::Error => ("color: red", "Error"),
            };
            let duration = match op.finished_at {
                Some(end) => format!("{}s", (end - op.started_at).num_seconds()),
                None => format!("{}s...", (now - op.started_at).num_seconds()),
            };
            OpRow {
                id: op.id.clone(),
                op_type: op.op_type.clone(),
                device: op.device.clone(),
                status_style,
                status_text,
                started: op.started_at.format("%H:%M:%S").to_string(),
                duration,
                last_message: op.last_message.clone(),
                running: op.status == OperationStatus::Running,
            }
        })
        .collect();

    let ctx = OpsCtx {
        empty: rows.is_empty(),
        ops: rows,
    };

    let html = state
        .templates
        .render_page(&state.templates.operations, "Operations", "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

/// GET /operations/{op_id} — view a single operation with SSE progress.
pub async fn operation_detail(
    State(state): State<AppState>,
    Path(op_id): Path<String>,
) -> Response {
    let tracker = state.operations.read().await;
    let ops = tracker.list_operations();
    let op = ops.iter().find(|o| o.id == op_id);

    match op {
        Some(op) => {
            let details = format!(
                "<p>Type: <strong>{}</strong> | Device: <strong>{}</strong></p>",
                html_escape(&op.op_type),
                html_escape(&op.device),
            );
            let html = crate::sse::sse_progress_page(
                &format!("Operation: {}", op.op_type),
                &details,
                &op_id,
            );
            Html(html).into_response()
        }
        None => {
            (StatusCode::NOT_FOUND, Html(format!(
                "<html><body><p>Operation '{}' not found</p><p><a href=\"/operations\">Back</a></p></body></html>",
                html_escape(&op_id),
            ))).into_response()
        }
    }
}

/// GET /operations/{op_id}/stream — SSE event stream for an operation.
pub async fn operation_stream(
    State(state): State<AppState>,
    Path(op_id): Path<String>,
) -> Response {
    let rx = match state.operations.read().await.subscribe(&op_id) {
        Some(rx) => rx,
        None => {
            warn!(op_id = %op_id, "SSE subscribe: operation not found");
            return (StatusCode::NOT_FOUND, Html(format!(
                "<html><body><p>Operation '{}' not found</p></body></html>", op_id
            ))).into_response();
        }
    };

    debug!(op_id = %op_id, "SSE client connected for operation progress");

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
        .route("/operations", get(list_operations))
        .route("/operations/{op_id}", get(operation_detail))
        .route("/operations/{op_id}/stream", get(operation_stream))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

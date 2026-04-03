use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::get,
    Router,
};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{debug, warn};

use crate::sse::OperationStatus;
use crate::state::AppState;

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /operations — list all active and recent operations.
pub async fn list_operations(State(state): State<AppState>) -> Response {
    let tracker = state.operations.read().await;
    let ops = tracker.list_operations();

    let rows: String = if ops.is_empty() {
        "<tr><td colspan=\"6\">No operations</td></tr>".to_string()
    } else {
        ops.iter().map(|op| {
            let status_style = match op.status {
                OperationStatus::Running => "color: #2980b9; font-weight: bold",
                OperationStatus::Complete => "color: green",
                OperationStatus::Error => "color: red",
            };
            let status_text = match op.status {
                OperationStatus::Running => "Running",
                OperationStatus::Complete => "Complete",
                OperationStatus::Error => "Error",
            };
            let duration = match op.finished_at {
                Some(end) => {
                    let d = end - op.started_at;
                    format!("{}s", d.num_seconds())
                }
                None => {
                    let d = chrono::Utc::now() - op.started_at;
                    format!("{}s...", d.num_seconds())
                }
            };
            let view_link = if op.status == OperationStatus::Running {
                format!("<a href=\"/operations/{id}\">Watch</a>", id = op.id)
            } else {
                String::new()
            };
            format!(
                "<tr>\
                 <td>{op_type}</td>\
                 <td>{device}</td>\
                 <td style=\"{status_style}\">{status_text}</td>\
                 <td>{started}</td>\
                 <td>{duration}</td>\
                 <td>{last_msg}</td>\
                 <td>{view_link}</td>\
                 </tr>",
                op_type = html_escape(&op.op_type),
                device = html_escape(&op.device),
                status_style = status_style,
                status_text = status_text,
                started = op.started_at.format("%H:%M:%S"),
                duration = duration,
                last_msg = html_escape(&op.last_message),
                view_link = view_link,
            )
        }).collect()
    };

    let content = format!(
        r#"<h1>Operations</h1>
<table>
<thead><tr><th>Type</th><th>Device</th><th>Status</th><th>Started</th><th>Duration</th><th>Last Message</th><th></th></tr></thead>
<tbody>
{rows}
</tbody>
</table>
<p><a href="/">Dashboard</a></p>"#,
        rows = rows,
    );

    let html = crate::routes::page_html("Operations", "", &content);
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

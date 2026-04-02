use axum::{
    extract::{Form, Path, State},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use tracing::info;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Form structs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AssignForm {
    serial: String,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/assignments", get(list_assignments))
        .route("/assignments/{name}/assign", post(assign_device))
        .route("/assignments/{name}/unassign", post(unassign_device))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_assignments(State(state): State<AppState>) -> Html<String> {
    let guard = state.assignments.read().await;
    let assignments = guard.all_assignments();

    info!(count = assignments.len(), "Serving assignments list");

    if assignments.is_empty() {
        return Html(
            r#"<!DOCTYPE html>
<html><body>
<h1>Assignments</h1>
<p>No assignments</p>
</body></html>"#
                .to_string(),
        );
    }

    let mut rows = String::new();
    for (serial, device) in assignments {
        rows.push_str(&format!(
            r#"<tr>
  <td><a href="/assets/{serial}">{serial}</a></td>
  <td><a href="/devices/{device}">{device}</a></td>
</tr>
"#,
            serial = html_escape(serial),
            device = html_escape(device),
        ));
    }

    Html(format!(
        r#"<!DOCTYPE html>
<html><body>
<h1>Assignments</h1>
<table>
<thead><tr><th>Serial Number</th><th>Logical Device</th></tr></thead>
<tbody>
{rows}</tbody>
</table>
</body></html>"#
    ))
}

// ---------------------------------------------------------------------------
// Mutation handlers
// ---------------------------------------------------------------------------

async fn assign_device(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<AssignForm>,
) -> axum::response::Response {
    let mut guard = state.assignments.write().await;
    match guard.assign(&form.serial, &name) {
        Ok(()) => {
            info!(serial = %form.serial, device = %name, "Assigned serial to device via HTTP");
            if let Err(e) = guard.save() {
                tracing::warn!(error = %e, "Failed to save assignments after assign");
            }
            Redirect::to("/assignments").into_response()
        }
        Err(msg) => {
            tracing::warn!(error = %msg, serial = %form.serial, device = %name, "Assign conflict");
            let body = format!(
                r#"<!DOCTYPE html>
<html><body>
<h1>Conflict</h1>
<p>{}</p>
</body></html>"#,
                html_escape(&msg)
            );
            (axum::http::StatusCode::CONFLICT, Html(body)).into_response()
        }
    }
}

async fn unassign_device(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Redirect {
    let mut guard = state.assignments.write().await;
    if let Some(serial) = guard.get_serial_for_device(&name).map(str::to_owned) {
        info!(serial = %serial, device = %name, "Unassigning serial from device via HTTP");
        guard.unassign(&serial);
    }
    if let Err(e) = guard.save() {
        tracing::warn!(error = %e, "Failed to save assignments after unassign");
    }
    Redirect::to("/assignments")
}

/// Minimal HTML escaping for untrusted strings.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use clap::Parser;
    use indexmap::IndexMap;
    use tower::ServiceExt;

    use crate::auth::htpasswd::HtpasswdStore;
    use crate::config::AppConfig;

    fn make_test_state() -> AppState {
        // Use a unique temp path for assignments so save() calls in tests
        // do not pollute the default assignments.json and break other tests.
        let tmp_path = std::env::temp_dir().join(format!(
            "aynmsgui_route_test_assignments_{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let tmp_path_str = tmp_path.to_str().unwrap().to_owned();
        let config = AppConfig::try_parse_from([
            "aynmsgui",
            "--htpasswd-file",
            "/dev/null",
            "--assignments-file",
            &tmp_path_str,
        ])
        .expect("test config parse");
        let htpasswd = HtpasswdStore::from_str("");
        AppState::new(config, htpasswd, None, IndexMap::new())
    }

    fn build_app(state: AppState) -> Router {
        routes().with_state(state)
    }

    async fn body_string(body: Body) -> String {
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn test_list_assignments_empty() {
        let state = make_test_state();
        let app = build_app(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/assignments")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response.into_body()).await;
        assert!(
            body.contains("No assignments"),
            "Expected 'No assignments' in body, got: {body}"
        );
    }

    #[tokio::test]
    async fn test_assign_success() {
        let state = make_test_state();
        let app = build_app(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/assignments/router-a/assign")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("serial=SN-001"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        // Verify the assignment persists in state.
        let guard = state.assignments.read().await;
        assert_eq!(guard.get_device_for_serial("SN-001"), Some("router-a"));
    }

    #[tokio::test]
    async fn test_assign_conflict() {
        let state = make_test_state();
        {
            let mut guard = state.assignments.write().await;
            guard.assign("SN-001", "router-a").unwrap();
        }

        let app = build_app(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/assignments/router-b/assign")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("serial=SN-001"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_unassign_success() {
        let state = make_test_state();
        {
            let mut guard = state.assignments.write().await;
            guard.assign("SN-001", "router-a").unwrap();
        }

        let app = build_app(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/assignments/router-a/unassign")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        // Verify the assignment was removed from state.
        let guard = state.assignments.read().await;
        assert_eq!(guard.get_device_for_serial("SN-001"), None);
        assert_eq!(guard.get_serial_for_device("router-a"), None);
    }

    #[tokio::test]
    async fn test_list_assignments_with_data() {
        let state = make_test_state();
        {
            let mut guard = state.assignments.write().await;
            guard.assign("SN-001", "router-a").unwrap();
        }

        let app = build_app(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/assignments")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response.into_body()).await;
        assert!(
            body.contains("SN-001"),
            "Expected serial 'SN-001' in body, got: {body}"
        );
        assert!(
            body.contains("router-a"),
            "Expected device 'router-a' in body, got: {body}"
        );
    }
}

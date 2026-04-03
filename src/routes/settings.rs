use axum::{
    extract::{Form, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use tracing::info;

use crate::state::{AppState, DeviceCredentials};

// ── Form struct ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct CredentialsForm {
    username: String,
    password: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn credentials_page(State(state): State<AppState>) -> Response {
    let creds = state.get_device_credentials().await;

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Device Credentials</title></head>
<body>
<h1>Device Credentials</h1>
<p>These credentials are used for all device connections (SSH).</p>
<form method="POST" action="/settings/credentials">
  <label for="username">Username:</label><br>
  <input type="text" id="username" name="username" value="{username}" required><br><br>
  <label for="password">Password:</label><br>
  <input type="password" id="password" name="password" value="{password}" required><br><br>
  <button type="submit">Save Credentials</button>
</form>
<p><a href="/">Back to Dashboard</a></p>
</body>
</html>"#,
        username = html_escape(&creds.username),
        password = html_escape(&creds.password),
    );

    Html(html).into_response()
}

pub async fn update_credentials(
    State(state): State<AppState>,
    Form(form): Form<CredentialsForm>,
) -> Response {
    let creds = DeviceCredentials {
        username: form.username.trim().to_string(),
        password: form.password.trim().to_string(),
    };

    info!(username = %creds.username, "Device credentials updated");
    state.update_device_credentials(creds).await;

    (
        StatusCode::FOUND,
        [(header::LOCATION, "/settings/credentials".to_string())],
    )
        .into_response()
}

// ── Routes ───────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings/credentials", get(credentials_page).post(update_credentials))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

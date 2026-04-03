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
    jumphost_address: String,
    jumphost_username: String,
    jumphost_password: String,
    jumphost_command: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn credentials_page(State(state): State<AppState>) -> Response {
    let creds = state.get_device_credentials().await;

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Device Connection Settings</title></head>
<body>
<h1>Device Connection Settings</h1>
<form method="POST" action="/settings/credentials">
<h2>Device Credentials</h2>
<p>These credentials are used to authenticate to network devices via SSH.</p>
  <label for="username">Username:</label><br>
  <input type="text" id="username" name="username" value="{username}" required><br><br>
  <label for="password">Password:</label><br>
  <input type="password" id="password" name="password" value="{password}" required><br><br>
<h2>Jumphost (optional)</h2>
<p>When configured, connections go through the jumphost first. The command template
is run on the jumphost shell to reach the target device.<br>
Placeholders: <code>{{username}}</code> = device username, <code>{{target_ip}}</code> = device IP.</p>
  <label for="jumphost_address">Jumphost Address (host or host:port):</label><br>
  <input type="text" id="jumphost_address" name="jumphost_address" value="{jumphost_address}" placeholder="10.1.1.1"><br><br>
  <label for="jumphost_username">Jumphost Username:</label><br>
  <input type="text" id="jumphost_username" name="jumphost_username" value="{jumphost_username}"><br><br>
  <label for="jumphost_password">Jumphost Password:</label><br>
  <input type="password" id="jumphost_password" name="jumphost_password" value="{jumphost_password}"><br><br>
  <label for="jumphost_command">SSH Command Template:</label><br>
  <input type="text" id="jumphost_command" name="jumphost_command" value="{jumphost_command}" size="60"
    placeholder="ssh -b 10.100.252.5 {{username}}@{{target_ip}}"><br><br>
  <button type="submit">Save Settings</button>
</form>
<p><a href="/">Back to Dashboard</a></p>
</body>
</html>"#,
        username = html_escape(&creds.username),
        password = html_escape(&creds.password),
        jumphost_address = html_escape(&creds.jumphost_address),
        jumphost_username = html_escape(&creds.jumphost_username),
        jumphost_password = html_escape(&creds.jumphost_password),
        jumphost_command = html_escape(&creds.jumphost_command),
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
        jumphost_address: form.jumphost_address.trim().to_string(),
        jumphost_username: form.jumphost_username.trim().to_string(),
        jumphost_password: form.jumphost_password.trim().to_string(),
        jumphost_command: form.jumphost_command.trim().to_string(),
    };

    info!(username = %creds.username, jumphost = %creds.jumphost_address, "Device connection settings updated");
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

use axum::{
    extract::{Form, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::state::{AppState, DeviceCredentials};

#[derive(Serialize)]
struct CredentialsCtx {
    username: String,
    password: String,
    jumphost_enabled: bool,
    jumphost_address: String,
    jumphost_username: String,
    jumphost_password: String,
    jumphost_command: String,
}

// ── Form struct ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct CredentialsForm {
    username: String,
    password: String,
    #[serde(default)]
    jumphost_enabled: Option<String>,
    jumphost_address: String,
    jumphost_username: String,
    jumphost_password: String,
    jumphost_command: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn credentials_page(State(state): State<AppState>) -> Response {
    let creds = state.get_device_credentials().await;
    let ctx = CredentialsCtx {
        username: creds.username.clone(),
        password: creds.password.clone(),
        jumphost_enabled: creds.jumphost_enabled,
        jumphost_address: creds.jumphost_address.clone(),
        jumphost_username: creds.jumphost_username.clone(),
        jumphost_password: creds.jumphost_password.clone(),
        jumphost_command: creds.jumphost_command.clone(),
    };
    let html = state
        .templates
        .render_page(
            &state.templates.settings_credentials,
            "Device Connection Settings",
            "",
            &ctx,
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

pub async fn update_credentials(
    State(state): State<AppState>,
    Form(form): Form<CredentialsForm>,
) -> Response {
    let creds = DeviceCredentials {
        username: form.username.trim().to_string(),
        password: form.password.trim().to_string(),
        jumphost_enabled: form.jumphost_enabled.as_deref() == Some("on"),
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

pub mod apply;
pub mod assets;
pub mod assignments;
pub mod config_diff;
pub mod dashboard;
pub mod devices;
pub mod extract;
pub mod extract_sw;
pub mod import;
pub mod login;
pub mod provision;
pub mod retrieve;
pub mod settings;
pub mod software;

use axum::{middleware, Router};
use tower_http::services::ServeDir;
use crate::auth::middleware::auth_middleware;
use crate::state::AppState;

/// Wrap page content in the common site layout with header, nav, and CSS.
/// Pass empty string for username if not available.
pub fn page_html(title: &str, username: &str, content: &str) -> String {
    let user_display = if username.is_empty() { "user" } else { username };
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>{title} - aynmsgui</title>
<link rel="stylesheet" type="text/css" href="/static/css/site.css" />
</head>
<body>
<div class="header">
<div class="header-inner">
  <a class="header-logo" href="/">aynmsgui</a>
  <div class="loginDisplay">
    Logged in as <strong>{user_display}</strong>
    [ <a href="/logout">Log Out</a> ]
  </div>
</div>
<div>
<ul class="menubar">
  <li><a href="/">Dashboard</a></li>
  <li class="dropdown">
    <a href="/assets">Assets</a>
    <div class="dropdown-content">
      <a href="/assets">All Assets</a>
      <a href="/seen">Seen Assets</a>
    </div>
  </li>
  <li class="dropdown">
    <a href="/devices">Devices</a>
    <div class="dropdown-content">
      <a href="/devices">Logical Devices</a>
      <a href="/assignments">Assignments</a>
    </div>
  </li>
  <li class="dropdown">
    <a href="/diff">Config</a>
    <div class="dropdown-content">
      <a href="/diff">Config Diffs</a>
      <a href="/retrieve">Retrieve Configs</a>
    </div>
  </li>
  <li class="dropdown">
    <a href="/software">Software</a>
    <div class="dropdown-content">
      <a href="/software">Versions</a>
      <a href="/extract-sw">Extract Image</a>
    </div>
  </li>
  <li class="dropdown">
    <a href="/import">Actions</a>
    <div class="dropdown-content">
      <a href="/import">Import Device</a>
      <a href="/extract">Extract Config</a>
      <a href="/extract-sw">Extract Software</a>
      <a href="/retrieve">Retrieve Configs</a>
    </div>
  </li>
  <li><a href="/settings/credentials">Settings</a></li>
</ul>
</div>
</div>
<div class="main">
{content}
</div>
</body>
</html>"#,
        title = title,
        user_display = user_display,
        content = content,
    )
}

pub fn build_router(state: AppState) -> Router {
    let sessions = state.sessions.clone();

    // Public routes (no auth required)
    let public_routes = Router::new()
        .merge(login::routes());

    // Protected routes (auth required)
    let protected_routes = Router::new()
        .merge(dashboard::routes())
        .merge(assets::routes())
        .merge(config_diff::routes())
        .merge(devices::routes())
        .merge(assignments::routes())
        .merge(software::routes())
        .merge(provision::routes())
        .merge(import::routes())
        .merge(extract::routes())
        .merge(extract_sw::routes())
        .merge(apply::routes())
        .merge(retrieve::routes())
        .merge(settings::routes())
        .layer(middleware::from_fn_with_state(sessions, auth_middleware));

    Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state)
}

pub mod assets;
pub mod assignments;
pub mod config_diff;
pub mod dashboard;
pub mod devices;
pub mod extract;
pub mod import;
pub mod login;
pub mod provision;
pub mod software;

use axum::{middleware, Router};
use crate::auth::middleware::auth_middleware;
use crate::state::AppState;

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
        .layer(middleware::from_fn_with_state(sessions, auth_middleware));

    Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .with_state(state)
}

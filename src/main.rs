mod auth;
pub mod assignments;
mod config;
mod error;
mod routes;
mod sse;
mod state;

use clap::Parser;
use config::AppConfig;
use auth::htpasswd::HtpasswdStore;
use state::AppState;
use tracing::info;

#[tokio::main]
async fn main() {
    let cfg = AppConfig::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let htpasswd = HtpasswdStore::from_file(&cfg.htpasswd_file)
        .unwrap_or_else(|e| panic!("Failed to load htpasswd file {:?}: {e}", cfg.htpasswd_file));

    let state = AppState::new(cfg.clone(), htpasswd, None, indexmap::IndexMap::new());

    // Background session cleanup
    let sessions = state.sessions.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let mut guard = sessions.write().await;
            guard.cleanup_expired();
            tracing::debug!("Cleaned up expired sessions");
        }
    });

    let app = routes::build_router(state);

    let bind_addr = format!("{}:{}", cfg.listen_addr, cfg.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind to {bind_addr}: {e}"));

    info!("Listening on {bind_addr}");

    axum::serve(listener, app)
        .await
        .expect("server error");
}

mod auth;
pub mod assignments;
mod config;
mod error;
mod routes;
mod sse;
mod state;

use std::path::Path;
use clap::Parser;
use config::AppConfig;
use auth::htpasswd::HtpasswdStore;
use state::AppState;
use tracing::{info, warn};

/// Ensure a directory exists, creating it (and parents) if needed.
fn ensure_dir(path: &Path) {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .unwrap_or_else(|e| warn!(path = %path.display(), error = %e, "Failed to create directory"));
        info!(path = %path.display(), "Created directory");
    }
}

/// Ensure a file exists, creating it with default content if missing.
fn ensure_file(path: &Path, default_content: &str) {
    if let Some(parent) = path.parent() {
        ensure_dir(parent);
    }
    if !path.exists() {
        std::fs::write(path, default_content)
            .unwrap_or_else(|e| warn!(path = %path.display(), error = %e, "Failed to create file"));
        info!(path = %path.display(), "Created file with defaults");
    }
}

/// Create the data/ directory structure and seed any missing files.
fn ensure_data_dirs(cfg: &AppConfig) {
    // htpasswd file — seed with a default admin:admin account (bcrypt)
    ensure_file(
        &cfg.htpasswd_file,
        // admin:admin (bcrypt cost 10)
        "admin:$2y$10$YourHashHere\n# Replace with: htpasswd -cB data/htpasswd <username>\n",
    );

    // Asset inventory
    if let Some(ref path) = cfg.inventory_path {
        ensure_file(path, "");
    }

    // Cfggen directories
    if let Some(ref dir) = cfg.cfggen_base_dir {
        ensure_dir(&dir.join("logical-devices"));
    }

    // Config directories
    if let Some(ref dir) = cfg.target_configs_path {
        ensure_dir(dir);
    }
    if let Some(ref dir) = cfg.current_configs_path {
        ensure_dir(dir);
    }

    // Changes directory
    if let Some(ref dir) = cfg.changes_dir {
        ensure_dir(dir);
    }

    // Images directory
    if let Some(ref dir) = cfg.images_dir {
        ensure_dir(dir);
    }
}

#[tokio::main]
async fn main() {
    let cfg = AppConfig::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Create data directory structure if needed
    ensure_data_dirs(&cfg);

    let htpasswd = HtpasswdStore::from_file(&cfg.htpasswd_file)
        .unwrap_or_else(|e| panic!("Failed to load htpasswd file {:?}: {e}", cfg.htpasswd_file));

    // Load asset inventory if configured
    let asset_cache = if let Some(ref path) = cfg.inventory_path {
        if path.exists() && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false) {
            match ayciam::AssetCache::new(path.clone()) {
                Ok(cache) => {
                    info!(path = %path.display(), "Loaded asset inventory");
                    Some((cache, path.clone()))
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Failed to load asset inventory");
                    None
                }
            }
        } else {
            info!(path = %path.display(), "Asset inventory file is empty, skipping");
            None
        }
    } else {
        None
    };

    // Load known devices from callhome address map URLs
    let known_devices = if !cfg.address_map_urls.is_empty() {
        let mut all_devices = indexmap::IndexMap::new();
        for url in &cfg.address_map_urls {
            match aycallhome::try_load_devices_ordered(url).await {
                Ok(devices) => {
                    info!(url = %url, count = devices.len(), "Loaded devices from address map");
                    all_devices.extend(devices);
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "Failed to load devices from address map");
                }
            }
        }
        all_devices
    } else {
        indexmap::IndexMap::new()
    };

    let state = AppState::new(cfg.clone(), htpasswd, asset_cache, known_devices);

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

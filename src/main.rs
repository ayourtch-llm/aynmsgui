mod auth;
pub mod assignments;
mod config;
mod error;
pub mod jumphost_connector;
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

/// Ensure a directory is a git repo, initializing if needed.
fn ensure_git_repo(path: &Path, branch: &str) {
    ensure_dir(path);
    if path.join(".git").exists() {
        return;
    }
    info!(path = %path.display(), branch = %branch, "Initializing git repo");
    let output = std::process::Command::new("git")
        .args(["init", "-b", branch])
        .current_dir(path)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            // Configure user for commits
            let _ = std::process::Command::new("git")
                .args(["config", "user.email", "aynmsgui@localhost"])
                .current_dir(path)
                .output();
            let _ = std::process::Command::new("git")
                .args(["config", "user.name", "aynmsgui"])
                .current_dir(path)
                .output();
            // Create initial commit
            let _ = std::process::Command::new("git")
                .args(["commit", "--allow-empty", "-m", "init"])
                .current_dir(path)
                .output();
            info!(path = %path.display(), "Git repo initialized");
        }
        Ok(o) => warn!(path = %path.display(), stderr = %String::from_utf8_lossy(&o.stderr), "git init failed"),
        Err(e) => warn!(path = %path.display(), error = %e, "Failed to run git init"),
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

    // Config directories — init as git repos for aycfgapply
    if let Some(ref dir) = cfg.target_configs_path {
        ensure_git_repo(dir, &cfg.target_branch);
    }
    if let Some(ref dir) = cfg.target_configs_preview_path {
        ensure_dir(dir);
    }
    if let Some(ref dir) = cfg.current_configs_path {
        ensure_git_repo(dir, &cfg.current_branch);
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

    // Load seen assets: first from local file, then merge from callhome URLs
    let mut seen_assets = indexmap::IndexMap::new();

    // Load from local seen_assets.json (persisted from previous sessions)
    if cfg.seen_assets_file.exists() {
        match std::fs::read_to_string(&cfg.seen_assets_file) {
            Ok(content) if !content.trim().is_empty() => {
                match serde_json::from_str::<Vec<aycallhome::Device>>(&content) {
                    Ok(devices) => {
                        info!(count = devices.len(), path = %cfg.seen_assets_file.display(), "Loaded seen assets from file");
                        for d in devices {
                            seen_assets.insert(d.serial.clone(), d);
                        }
                    }
                    Err(e) => {
                        warn!(path = %cfg.seen_assets_file.display(), error = %e, "Failed to parse seen assets file");
                    }
                }
            }
            _ => {}
        }
    }

    // Merge from callhome address map URLs (if configured)
    for url in &cfg.address_map_urls {
        match aycallhome::try_load_devices_ordered(url).await {
            Ok(devices) => {
                info!(url = %url, count = devices.len(), "Loaded devices from address map");
                seen_assets.extend(devices);
            }
            Err(e) => {
                warn!(url = %url, error = %e, "Failed to load devices from address map");
            }
        }
    }

    let state = AppState::new(cfg.clone(), htpasswd, asset_cache, seen_assets);

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

    // Background seen-assets refresh (file + callhome URLs)
    let refresh_state = state.clone();
    let refresh_secs = cfg.address_map_refresh_secs;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
        interval.tick().await; // skip the immediate first tick (already loaded above)
        loop {
            interval.tick().await;
            refresh_state.refresh_seen_assets().await;
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

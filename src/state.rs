use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::assignments::AssignmentMap;
use crate::auth::htpasswd::HtpasswdStore;
use crate::auth::session::SessionStore;
use crate::config::AppConfig;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub htpasswd: Arc<HtpasswdStore>,
    pub sessions: Arc<RwLock<SessionStore>>,
    pub asset_cache: Option<Arc<ayciam::AssetCache>>,
    /// Path to the JSONL inventory file; present when asset_cache is Some.
    pub asset_inventory_path: Option<Arc<PathBuf>>,
    pub known_devices: Arc<RwLock<indexmap::IndexMap<String, aycallhome::Device>>>,
    pub assignments: Arc<RwLock<AssignmentMap>>,
    pub operations: Arc<RwLock<crate::sse::OperationTracker>>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        htpasswd: HtpasswdStore,
        asset_cache: Option<(ayciam::AssetCache, PathBuf)>,
        known_devices: indexmap::IndexMap<String, aycallhome::Device>,
    ) -> Self {
        let (cache_opt, path_opt) = match asset_cache {
            Some((cache, path)) => (Some(Arc::new(cache)), Some(Arc::new(path))),
            None => (None, None),
        };
        let assignments = AssignmentMap::from_file(&config.assignments_file)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to load assignments, starting empty");
                AssignmentMap::new()
            });
        Self {
            config: Arc::new(config),
            htpasswd: Arc::new(htpasswd),
            sessions: Arc::new(RwLock::new(SessionStore::new())),
            asset_cache: cache_opt,
            asset_inventory_path: path_opt,
            known_devices: Arc::new(RwLock::new(known_devices)),
            assignments: Arc::new(RwLock::new(assignments)),
            operations: Arc::new(RwLock::new(crate::sse::OperationTracker::new())),
        }
    }
}

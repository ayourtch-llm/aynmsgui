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
    /// Register or update a known device by serial + IP address.
    /// Called after successful SSH connections during import/extract operations.
    pub async fn register_known_device(
        &self,
        serial: &str,
        ip: &str,
        hostname: Option<&str>,
        model: Option<&str>,
        version: Option<&str>,
    ) {
        let mut devices = self.known_devices.write().await;
        let device = devices.entry(serial.to_string()).or_insert_with(|| {
            aycallhome::Device {
                serial: serial.to_string(),
                version: None,
                hostname: None,
                model: None,
                token: None,
                last_ipv4: None,
                last_ipv6: None,
                last_seen_ipv4: None,
                last_seen_ipv6: None,
                first_seen: Some(chrono::Utc::now()),
            }
        });
        device.last_ipv4 = Some(ip.to_string());
        device.last_seen_ipv4 = Some(chrono::Utc::now());
        if let Some(h) = hostname {
            device.hostname = Some(h.to_string());
        }
        if let Some(m) = model {
            device.model = Some(m.to_string());
        }
        if let Some(v) = version {
            device.version = Some(v.to_string());
        }
        tracing::info!(serial = %serial, ip = %ip, "Registered/updated known device");

        // Persist to disk
        let path = &self.config.known_devices_file;
        let devices_vec: Vec<&aycallhome::Device> = devices.values().collect();
        match serde_json::to_string_pretty(&devices_vec) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, &json) {
                    tracing::warn!(path = %path.display(), error = %e, "Failed to save known devices");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize known devices");
            }
        }
    }

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

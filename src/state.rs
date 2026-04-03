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
    pub seen_assets: Arc<RwLock<indexmap::IndexMap<String, aycallhome::Device>>>,
    pub assignments: Arc<RwLock<AssignmentMap>>,
    pub operations: Arc<RwLock<crate::sse::OperationTracker>>,
}

/// Format an IP address + port as an SSH target string.
/// IPv6 addresses are wrapped in brackets: `[::1]:22`
/// IPv4 addresses are used as-is: `10.0.0.1:22`
pub fn ssh_target(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("[{}]:{}", ip, port)
    } else {
        format!("{}:{}", ip, port)
    }
}

impl AppState {
    /// Register or update a seen asset by serial + IP address.
    /// Called after successful SSH connections during import/extract operations.
    pub async fn register_seen_asset(
        &self,
        serial: &str,
        ip: &str,
        hostname: Option<&str>,
        model: Option<&str>,
        version: Option<&str>,
    ) {
        let mut devices = self.seen_assets.write().await;
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
        if ip.contains(':') {
            // IPv6
            device.last_ipv6 = Some(ip.to_string());
            device.last_seen_ipv6 = Some(chrono::Utc::now());
        } else {
            // IPv4
            device.last_ipv4 = Some(ip.to_string());
            device.last_seen_ipv4 = Some(chrono::Utc::now());
        }
        if let Some(h) = hostname {
            device.hostname = Some(h.to_string());
        }
        if let Some(m) = model {
            device.model = Some(m.to_string());
        }
        if let Some(v) = version {
            device.version = Some(v.to_string());
        }
        tracing::info!(serial = %serial, ip = %ip, "Registered/updated seen asset");

        // Persist to disk
        let path = &self.config.seen_assets_file;
        let devices_vec: Vec<&aycallhome::Device> = devices.values().collect();
        match serde_json::to_string_pretty(&devices_vec) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, &json) {
                    tracing::warn!(path = %path.display(), error = %e, "Failed to save seen assets");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize seen assets");
            }
        }
    }

    /// Refresh seen assets from the on-disk file and callhome URLs.
    /// Merges new/updated entries into the in-memory map without losing
    /// entries that only exist in memory.
    pub async fn refresh_seen_assets(&self) {
        let mut assets = self.seen_assets.write().await;

        // 1. Re-read from local file
        let path = &self.config.seen_assets_file;
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) if !content.trim().is_empty() => {
                    match serde_json::from_str::<Vec<aycallhome::Device>>(&content) {
                        Ok(devices) => {
                            for d in devices {
                                let entry = assets.entry(d.serial.clone()).or_insert(d.clone());
                                // Update fields if the file version is newer
                                // (compare last_seen timestamps)
                                let file_latest = d.last_seen_ipv6.or(d.last_seen_ipv4);
                                let mem_latest = entry.last_seen_ipv6.or(entry.last_seen_ipv4);
                                if file_latest > mem_latest {
                                    *entry = d;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "Failed to parse seen assets file during refresh");
                        }
                    }
                }
                _ => {}
            }
        }

        // 2. Merge from callhome URLs
        for url in &self.config.address_map_urls {
            match aycallhome::try_load_devices_ordered(url).await {
                Ok(devices) => {
                    tracing::debug!(url = %url, count = devices.len(), "Refreshed devices from address map");
                    for (serial, d) in devices {
                        let entry = assets.entry(serial).or_insert(d.clone());
                        let remote_latest = d.last_seen_ipv6.or(d.last_seen_ipv4);
                        let mem_latest = entry.last_seen_ipv6.or(entry.last_seen_ipv4);
                        if remote_latest > mem_latest {
                            *entry = d;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(url = %url, error = %e, "Failed to refresh devices from address map");
                }
            }
        }

        // 3. Persist merged result back to disk
        let devices_vec: Vec<&aycallhome::Device> = assets.values().collect();
        match serde_json::to_string_pretty(&devices_vec) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, &json) {
                    tracing::warn!(path = %path.display(), error = %e, "Failed to save seen assets after refresh");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize seen assets after refresh");
            }
        }

        tracing::debug!(count = assets.len(), "Seen assets refresh complete");
    }

    pub fn new(
        config: AppConfig,
        htpasswd: HtpasswdStore,
        asset_cache: Option<(ayciam::AssetCache, PathBuf)>,
        seen_assets: indexmap::IndexMap<String, aycallhome::Device>,
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
        let sessions = SessionStore::with_persistence(&config.user_sessions_dir);
        Self {
            config: Arc::new(config),
            htpasswd: Arc::new(htpasswd),
            sessions: Arc::new(RwLock::new(sessions)),
            asset_cache: cache_opt,
            asset_inventory_path: path_opt,
            seen_assets: Arc::new(RwLock::new(seen_assets)),
            assignments: Arc::new(RwLock::new(assignments)),
            operations: Arc::new(RwLock::new(crate::sse::OperationTracker::new())),
        }
    }
}

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use crate::assignments::AssignmentMap;
use crate::auth::htpasswd::HtpasswdStore;
use crate::auth::session::SessionStore;
use crate::config::AppConfig;

/// Credentials and connection settings for reaching network devices via SSH.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCredentials {
    pub username: String,
    pub password: String,
    /// Jumphost settings (all optional — when address is empty, direct connection is used).
    #[serde(default)]
    pub jumphost_address: String,
    #[serde(default)]
    pub jumphost_username: String,
    #[serde(default)]
    pub jumphost_password: String,
    /// Command template to run on the jumphost to reach the target device.
    /// Placeholders: {username} = device username, {target_ip} = device IP.
    /// Example: "ssh -b 10.100.252.5 {username}@{target_ip}"
    #[serde(default)]
    pub jumphost_command: String,
}

impl DeviceCredentials {
    /// Returns true if jumphost is configured.
    pub fn has_jumphost(&self) -> bool {
        !self.jumphost_address.is_empty() && !self.jumphost_command.is_empty()
    }

    /// Build the SSH command to run on the jumphost for a given target IP.
    pub fn jumphost_ssh_command(&self, target_ip: &str) -> String {
        self.jumphost_command
            .replace("{username}", &self.username)
            .replace("{target_ip}", target_ip)
    }
}

impl DeviceCredentials {
    /// Load from a JSON file. Falls back to config CLI args, then empty defaults.
    pub fn load(path: &Path, config: &AppConfig) -> Self {
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(creds) = serde_json::from_str::<DeviceCredentials>(&content) {
                    info!(path = %path.display(), "Loaded device credentials from file");
                    return creds;
                }
                warn!(path = %path.display(), "Failed to parse device credentials file");
            }
        }

        // Fall back to CLI/env args
        let creds = DeviceCredentials {
            username: config.device_username.clone().unwrap_or_default(),
            password: config.device_password.clone().unwrap_or_default(),
            jumphost_address: String::new(),
            jumphost_username: String::new(),
            jumphost_password: String::new(),
            jumphost_command: String::new(),
        };

        // Persist the initial credentials
        if let Err(e) = creds.save(path) {
            warn!(path = %path.display(), error = %e, "Failed to save initial device credentials");
        }

        creds
    }

    /// Save to a JSON file.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        info!(path = %path.display(), "Saved device credentials");
        Ok(())
    }
}

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
    pub device_credentials: Arc<RwLock<DeviceCredentials>>,
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
        let device_credentials = DeviceCredentials::load(&config.device_credentials_file, &config);
        Self {
            config: Arc::new(config),
            htpasswd: Arc::new(htpasswd),
            sessions: Arc::new(RwLock::new(sessions)),
            asset_cache: cache_opt,
            asset_inventory_path: path_opt,
            seen_assets: Arc::new(RwLock::new(seen_assets)),
            assignments: Arc::new(RwLock::new(assignments)),
            operations: Arc::new(RwLock::new(crate::sse::OperationTracker::new())),
            device_credentials: Arc::new(RwLock::new(device_credentials)),
        }
    }

    /// Get a snapshot of the current device credentials.
    pub async fn get_device_credentials(&self) -> DeviceCredentials {
        self.device_credentials.read().await.clone()
    }

    /// Update device credentials and persist to disk.
    pub async fn update_device_credentials(&self, creds: DeviceCredentials) {
        if let Err(e) = creds.save(&self.config.device_credentials_file) {
            tracing::warn!(error = %e, "Failed to save device credentials");
        }
        *self.device_credentials.write().await = creds;
    }

    /// Connect to a device, either directly or via jumphost if configured.
    ///
    /// `target_ip` is the device IP address (without port).
    pub async fn connect_to_device(
        &self,
        target_ip: &str,
        timeout: std::time::Duration,
        read_timeout: std::time::Duration,
    ) -> Result<ayclic::CiscoIosConn, ayclic::CiscoIosError> {
        let creds = self.get_device_credentials().await;

        if creds.has_jumphost() {
            let ssh_command = creds.jumphost_ssh_command(target_ip);
            info!(
                jumphost = %creds.jumphost_address,
                command = %ssh_command,
                "Connecting via jumphost"
            );

            let jump_target = ssh_target(&creds.jumphost_address, 22);
            let jump_addr: std::net::SocketAddr = jump_target.parse()
                .map_err(|e| ayclic::CiscoIosError::InvalidConnectionType(
                    format!("invalid jumphost address '{}': {}", jump_target, e),
                ))?;

            // TextFSMPlus template for the jumphost hop:
            // 1. Wait for jumphost prompt
            // 2. Send the SSH command
            // 3. Wait for device password prompt
            // 4. Send device password
            // 5. Wait for device IOS prompt
            // 6. Send terminal length 0
            let jumphost_template = format!(
                r#"Value Preset DevicePassword ()

Start
  ^.*[\$#>]\s* -> Send "{ssh_command}" WaitPassword

WaitPassword
  ^.*[Pp]assword:\s* -> Send ${{DevicePassword}} WaitPrompt
  ^.*refused.* -> Error "connection refused"
  ^.*denied.* -> Error "permission denied"
  ^.*[Nn]o route.* -> Error "no route to host"

WaitPrompt
  ^.*# -> Send "terminal length 0" TermLen
  ^.*> -> Send "terminal length 0" TermLen
  ^.*refused.* -> Error "connection refused"
  ^.*denied.* -> Error "permission denied"

TermLen
  ^.*# -> Done
  ^.*> -> Done
"#,
                ssh_command = ssh_command,
            );

            let hops = vec![
                ayclic::Hop::Transport(ayclic::TransportSpec::Ssh {
                    target: jump_addr,
                    auth: ayclic::SshAuth::Password {
                        username: creds.jumphost_username.clone(),
                        password: creds.jumphost_password.clone(),
                    },
                    source: None,
                }),
                ayclic::Hop::Interactive(
                    aytextfsmplus::TextFSMPlus::from_str(&jumphost_template)
                        .with_preset("DevicePassword", &creds.password),
                ),
            ];

            let path = ayclic::ConnectionPath::new(hops).with_timeout(timeout);
            ayclic::CiscoIosConn::from_path(
                path,
                &ssh_target(target_ip, 22),
                &aytextfsmplus::NoVars,
                &aytextfsmplus::NoFuncs,
            )
            .await
        } else {
            let target = ssh_target(target_ip, 22);
            ayclic::CiscoIosConn::with_timeouts(
                &target,
                ayclic::ConnectionType::Ssh,
                &creds.username,
                &creds.password,
                timeout,
                read_timeout,
            )
            .await
        }
    }
}

//! Background poller that fetches a switches-list JSON and merges
//! Reachable=true entries into the shared `seen_assets` map, and also
//! syncs each entry's LocationDetail into the matching logical device
//! config as a `description` field (regardless of reachability — the
//! location text doesn't depend on whether the box is up right now).

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct SwitchEntry {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "Hostname")]
    hostname: Option<String>,
    #[serde(rename = "IPAddress")]
    ip_address: Option<String>,
    #[serde(rename = "Reachable")]
    reachable: Option<bool>,
    #[serde(rename = "SKU")]
    sku: Option<String>,
    #[serde(rename = "LocationDetail")]
    location_detail: Option<String>,
}

/// Fetch the switches list and merge reachable entries into `state.seen_assets`.
/// Returns (matched_existing, inserted_new, skipped_unreachable, descriptions_updated) on success.
pub async fn poll_once(
    state: &AppState,
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(usize, usize, usize, usize)> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {status}: {body}");
    }

    let entries: Vec<SwitchEntry> = resp.json().await?;
    let now = chrono::Utc::now();

    // 1) Update descriptions on matching logical devices from LocationDetail.
    //    Does not depend on reachability — location text describes the device
    //    even if it's currently down.
    let mut descriptions_updated = 0usize;
    if let Some(base_dir) = state.config.cfggen_base_dir.as_ref() {
        let ld_dir = base_dir.join("logical-devices");
        for entry in &entries {
            let name = match entry.name.as_deref() {
                Some(n) if !n.trim().is_empty() => n.trim(),
                _ => continue,
            };
            let location = match entry.location_detail.as_deref() {
                Some(l) if !l.trim().is_empty() => l.trim(),
                _ => continue,
            };
            if update_device_description(&ld_dir, name, location) {
                descriptions_updated += 1;
            }
        }
    }

    let mut matched = 0usize;
    let mut inserted = 0usize;
    let mut skipped = 0usize;
    let mut assets = state.seen_assets.write().await;

    for entry in &entries {
        if !entry.reachable.unwrap_or(false) {
            skipped += 1;
            continue;
        }
        let host = match entry.hostname.as_deref() {
            Some(h) if !h.trim().is_empty() => h.trim(),
            _ => continue,
        };
        let ip = match entry.ip_address.as_deref() {
            Some(i) if !i.trim().is_empty() => i.trim(),
            _ => continue,
        };

        // Look for an existing entry whose hostname matches (case-insensitive).
        let existing_key = assets
            .iter()
            .find(|(_, d)| {
                d.hostname
                    .as_deref()
                    .map(|h| h.eq_ignore_ascii_case(host))
                    .unwrap_or(false)
            })
            .map(|(k, _)| k.clone());

        if let Some(key) = existing_key {
            let device = assets.get_mut(&key).expect("key just looked up");
            apply_ip(device, ip, now);
            if device.model.is_none() {
                device.model = entry.sku.clone().filter(|s| !s.is_empty());
            }
            matched += 1;
            continue;
        }

        // No match — insert a new entry keyed by Hostname as synthetic serial.
        let mut device = aycallhome::Device {
            serial: host.to_string(),
            version: None,
            hostname: Some(host.to_string()),
            model: entry.sku.clone().filter(|s| !s.is_empty()),
            token: None,
            last_ipv4: None,
            last_ipv6: None,
            last_seen_ipv4: None,
            last_seen_ipv6: None,
            first_seen: Some(now),
        };
        apply_ip(&mut device, ip, now);
        assets.insert(host.to_string(), device);
        inserted += 1;
        debug!(hostname = %host, ip = %ip, "switches-poll: new reachable switch added");
    }

    if matched > 0 || inserted > 0 {
        let path = &state.config.seen_assets_file;
        let devices_vec: Vec<&aycallhome::Device> = assets.values().collect();
        match serde_json::to_string_pretty(&devices_vec) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, &json) {
                    warn!(path = %path.display(), error = %e, "Failed to save seen assets after switches poll");
                }
            }
            Err(e) => warn!(error = %e, "Failed to serialize seen assets after switches poll"),
        }
    }

    Ok((matched, inserted, skipped, descriptions_updated))
}

/// Set `description` on the logical-device config JSON if it differs from
/// the current value. Returns true if the file was rewritten.
fn update_device_description(ld_dir: &Path, name: &str, description: &str) -> bool {
    // Support both flat and directory layouts (matches devices.rs convention).
    let flat = ld_dir.join(format!("{}.json", name));
    let dir_layout = ld_dir.join(name).join("config.json");
    let json_path = if flat.exists() {
        flat
    } else if dir_layout.exists() {
        dir_layout
    } else {
        return false;
    };

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path = %json_path.display(), error = %e, "switches-poll: failed to read device JSON");
            return false;
        }
    };
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!(path = %json_path.display(), error = %e, "switches-poll: failed to parse device JSON");
            return false;
        }
    };

    // Skip if already set to the same value — avoid disk churn + needless mtime bumps.
    if value.get("description").and_then(|v| v.as_str()) == Some(description) {
        return false;
    }
    value["description"] = serde_json::Value::String(description.to_string());

    let updated = match serde_json::to_string_pretty(&value) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "switches-poll: failed to serialize device JSON");
            return false;
        }
    };
    if let Err(e) = std::fs::write(&json_path, &updated) {
        warn!(path = %json_path.display(), error = %e, "switches-poll: failed to write device JSON");
        return false;
    }
    debug!(name, description, "switches-poll: updated logical device description");
    true
}

fn apply_ip(device: &mut aycallhome::Device, ip: &str, now: chrono::DateTime<chrono::Utc>) {
    if ip.contains(':') {
        device.last_ipv6 = Some(ip.to_string());
        device.last_seen_ipv6 = Some(now);
    } else {
        device.last_ipv4 = Some(ip.to_string());
        device.last_seen_ipv4 = Some(now);
    }
}

/// Spawn the switches-list poller as a background task. No-op if `url` is empty.
pub fn spawn(state: AppState, url: String, interval_secs: u64, insecure: bool) {
    if url.is_empty() {
        return;
    }
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(insecure)
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to build switches-poll HTTP client; poller not started");
            return;
        }
    };

    info!(url = %url, interval_secs, insecure, "Starting switches-list poller");

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match poll_once(&state, &client, &url).await {
                Ok((matched, inserted, skipped, descriptions_updated)) => {
                    info!(
                        matched, inserted, skipped, descriptions_updated,
                        "switches-list poll complete"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "switches-list poll failed");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_ip_ipv4() {
        let now = chrono::Utc::now();
        let mut d = aycallhome::Device {
            serial: "X".into(),
            version: None,
            hostname: None,
            model: None,
            token: None,
            last_ipv4: None,
            last_ipv6: None,
            last_seen_ipv4: None,
            last_seen_ipv6: None,
            first_seen: None,
        };
        apply_ip(&mut d, "10.1.2.3", now);
        assert_eq!(d.last_ipv4.as_deref(), Some("10.1.2.3"));
        assert!(d.last_seen_ipv4.is_some());
    }

    #[test]
    fn test_deserialize_sample() {
        let json = r#"[{"Name":"AD6-X025","IPAddress":"10.1.23.5","Reachable":true,"WasReachable":true,"Hostname":"AD6-X025-S1076","AssetTag":"S1076","LocationDetail":"Palais Vrijwilligers","SKU":"C9200CX-12P-2X2G","Returned":false}]"#;
        let entries: Vec<SwitchEntry> = serde_json::from_str(json).expect("parses");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name.as_deref(), Some("AD6-X025"));
        assert_eq!(entries[0].hostname.as_deref(), Some("AD6-X025-S1076"));
        assert_eq!(entries[0].ip_address.as_deref(), Some("10.1.23.5"));
        assert_eq!(entries[0].reachable, Some(true));
        assert_eq!(entries[0].sku.as_deref(), Some("C9200CX-12P-2X2G"));
        assert_eq!(entries[0].location_detail.as_deref(), Some("Palais Vrijwilligers"));
    }

    #[test]
    fn test_update_device_description_writes_and_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let ld_dir = dir.path().to_path_buf();
        // Flat-file layout: logical-devices/foo.json
        let path = ld_dir.join("foo.json");
        std::fs::write(&path, r#"{"hostname":"foo-host","role":"access"}"#).unwrap();

        // First call: should write.
        assert!(update_device_description(&ld_dir, "foo", "NOC rack 3"));
        let content = std::fs::read_to_string(&path).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(val.get("description").and_then(|v| v.as_str()), Some("NOC rack 3"));
        // Other fields preserved.
        assert_eq!(val.get("hostname").and_then(|v| v.as_str()), Some("foo-host"));
        assert_eq!(val.get("role").and_then(|v| v.as_str()), Some("access"));

        // Second call with same value: should be a no-op.
        assert!(!update_device_description(&ld_dir, "foo", "NOC rack 3"));

        // Change: should write again.
        assert!(update_device_description(&ld_dir, "foo", "NOC rack 4"));
        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(val.get("description").and_then(|v| v.as_str()), Some("NOC rack 4"));
    }

    #[test]
    fn test_update_device_description_missing_device_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        // No file exists for "ghost"
        assert!(!update_device_description(dir.path(), "ghost", "anywhere"));
    }
}

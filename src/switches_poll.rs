//! Background poller that fetches a switches-list JSON and merges
//! Reachable=true entries into the shared `seen_assets` map.
//!
//! The endpoint returns an array of `{ Hostname, IPAddress, Reachable, SKU, ... }`.
//! Only entries with `Reachable: true` are acted on. Matching is by `Hostname`
//! against `Device.hostname` (case-insensitive). Unknown reachable switches
//! are inserted as new entries keyed by `Hostname` as a synthetic serial.

use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct SwitchEntry {
    #[serde(rename = "Hostname")]
    hostname: Option<String>,
    #[serde(rename = "IPAddress")]
    ip_address: Option<String>,
    #[serde(rename = "Reachable")]
    reachable: Option<bool>,
    #[serde(rename = "SKU")]
    sku: Option<String>,
}

/// Fetch the switches list and merge reachable entries into `state.seen_assets`.
/// Returns (matched_existing, inserted_new, skipped_unreachable) on success.
pub async fn poll_once(
    state: &AppState,
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(usize, usize, usize)> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {status}: {body}");
    }

    let entries: Vec<SwitchEntry> = resp.json().await?;
    let now = chrono::Utc::now();

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

    Ok((matched, inserted, skipped))
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
                Ok((matched, inserted, skipped)) => {
                    info!(matched, inserted, skipped, "switches-list poll complete");
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
        let json = r#"[{"Name":"AD6-X025","IPAddress":"10.1.23.5","Reachable":true,"WasReachable":true,"Hostname":"AD6-X025-S1076","AssetTag":"S1076","LocationDetail":"x","SKU":"C9200CX-12P-2X2G","Returned":false}]"#;
        let entries: Vec<SwitchEntry> = serde_json::from_str(json).expect("parses");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hostname.as_deref(), Some("AD6-X025-S1076"));
        assert_eq!(entries[0].ip_address.as_deref(), Some("10.1.23.5"));
        assert_eq!(entries[0].reachable, Some(true));
        assert_eq!(entries[0].sku.as_deref(), Some("C9200CX-12P-2X2G"));
    }
}

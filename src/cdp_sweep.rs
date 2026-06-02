//! Background poller that fetches a CDP-neighbors sweep JSON and merges
//! discovered neighbor IPs into the shared `seen_assets` map.
//!
//! Each row in the sweep names a local switch (`Local_*`) and one CDP
//! neighbor (`Remote_*`). The local switch has no IP in the payload, so the
//! poller only acts on the remote side. Matching is by hostname (FQDN suffix
//! stripped, case-insensitive). When a CDP-visible device is not yet in
//! `seen_assets`, a new entry is inserted, keyed by the canonical hostname
//! as a synthetic serial.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::state::AppState;

/// One CDP-neighbors-sweep row: a single adjacency from a local switch port
/// to one of its CDP neighbors. Public because the topology endpoint reads
/// the cached snapshot too.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CdpEntry {
    #[serde(rename = "Local_Asset_Inventory")]
    pub local_asset_inventory: Option<String>,
    #[serde(rename = "Local_Host")]
    pub local_host: Option<String>,
    #[serde(rename = "Local_Interface")]
    pub local_interface: Option<String>,
    #[serde(rename = "Remote_Host")]
    pub remote_host: Option<String>,
    #[serde(rename = "Remote_IPAddress")]
    pub remote_ipaddress: Option<String>,
    #[serde(rename = "Remote_Interface")]
    pub remote_interface: Option<String>,
    #[serde(rename = "Remote_Platform")]
    pub remote_platform: Option<String>,
    #[serde(rename = "Remote_Version")]
    pub remote_version: Option<String>,
}

/// A cached snapshot of the last CDP sweep poll, stored on AppState so the
/// topology endpoint can render without re-fetching.
#[derive(Debug, Clone)]
pub struct CdpSnapshot {
    pub entries: Vec<CdpEntry>,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// Canonicalize a hostname:
///   - If the name's last segment looks like a TLD (2–6 lowercase ASCII
///     letters), treat it as an FQDN and strip to the first segment.
///     e.g. "AD6-X014-S1249.a3s.alpehuzes.nl" -> "AD6-X014-S1249"
///   - Otherwise keep the whole name intact, so MAC-style identifiers
///     like "AP38B8.1234.2345" (where dots are part of the name, not a
///     domain suffix) don't collapse into "AP38B8".
pub fn canonical_hostname(host: &str) -> &str {
    let host = host.trim();
    if host.is_empty() {
        return host;
    }
    if let Some(idx) = host.rfind('.') {
        let last = &host[idx + 1..];
        let looks_like_tld = !last.is_empty()
            && last.len() <= 6
            && last.chars().all(|c| c.is_ascii_lowercase());
        if looks_like_tld {
            return host.split('.').next().unwrap_or(host);
        }
    }
    host
}

/// Fetch the sweep and merge results into `state.seen_assets`.
/// Returns (matched_existing, inserted_new) on success. If
/// `update_assets` is false, the topology snapshot is still refreshed
/// but seen_assets is left untouched (both counts return 0).
pub async fn poll_once(
    state: &AppState,
    client: &reqwest::Client,
    url: &str,
    cookie: &str,
    update_assets: bool,
) -> anyhow::Result<(usize, usize)> {
    let mut req = client.get(url);
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {status}: {body}");
    }

    let entries: Vec<CdpEntry> = resp.json().await?;
    let now = chrono::Utc::now();

    // Cache the raw entries so /topology/json can render without re-fetching.
    *state.cdp_snapshot.write().await = Some(CdpSnapshot {
        entries: entries.clone(),
        fetched_at: now,
    });

    if !update_assets {
        return Ok((0, 0));
    }

    let mut matched = 0usize;
    let mut inserted = 0usize;
    let mut assets = state.seen_assets.write().await;

    for entry in &entries {
        let host = match entry.remote_host.as_deref() {
            Some(h) if !h.trim().is_empty() => h,
            _ => continue,
        };
        let ip = match entry.remote_ipaddress.as_deref() {
            Some(i) if !i.trim().is_empty() => i.trim(),
            _ => continue,
        };
        let canon = canonical_hostname(host);
        if canon.is_empty() {
            continue;
        }

        // Look for an existing entry whose hostname matches (case-insensitive).
        let existing_key = assets
            .iter()
            .find(|(_, d)| {
                d.hostname
                    .as_deref()
                    .map(|h| canonical_hostname(h).eq_ignore_ascii_case(canon))
                    .unwrap_or(false)
            })
            .map(|(k, _)| k.clone());

        if let Some(key) = existing_key {
            let device = assets.get_mut(&key).expect("key just looked up");
            apply_ip(device, ip, now);
            matched += 1;
            continue;
        }

        // No match — insert a new entry keyed by the canonical hostname.
        let mut device = aycallhome::Device {
            serial: canon.to_string(),
            version: entry.remote_version.clone().filter(|s| !s.is_empty()),
            hostname: Some(canon.to_string()),
            model: entry.remote_platform.clone().filter(|s| !s.is_empty()),
            token: None,
            last_ipv4: None,
            last_ipv6: None,
            last_seen_ipv4: None,
            last_seen_ipv6: None,
            first_seen: Some(now),
        };
        apply_ip(&mut device, ip, now);
        assets.insert(canon.to_string(), device);
        inserted += 1;
        debug!(hostname = %canon, ip = %ip, "CDP-discovered new device added to seen_assets");
    }

    if matched > 0 || inserted > 0 {
        let path = &state.config.seen_assets_file;
        let devices_vec: Vec<&aycallhome::Device> = assets.values().collect();
        match serde_json::to_string_pretty(&devices_vec) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, &json) {
                    warn!(path = %path.display(), error = %e, "Failed to save seen assets after CDP sweep");
                }
            }
            Err(e) => warn!(error = %e, "Failed to serialize seen assets after CDP sweep"),
        }
    }

    Ok((matched, inserted))
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

/// Spawn the CDP sweep poller as a background task. No-op if `url` is empty.
pub fn spawn(
    state: AppState,
    url: String,
    cookie: String,
    interval_secs: u64,
    insecure: bool,
    update_assets: bool,
) {
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
            warn!(error = %e, "Failed to build CDP sweep HTTP client; poller not started");
            return;
        }
    };

    info!(url = %url, interval_secs, insecure, update_assets, "Starting CDP sweep poller");

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match poll_once(&state, &client, &url, &cookie, update_assets).await {
                Ok((matched, inserted)) => {
                    info!(matched, inserted, "CDP sweep poll complete");
                }
                Err(e) => {
                    warn!(error = %e, "CDP sweep poll failed");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonical_hostname_strips_fqdn() {
        assert_eq!(canonical_hostname("AD6-X014-S1249.a3s.alpehuzes.nl"), "AD6-X014-S1249");
        assert_eq!(canonical_hostname("router1"), "router1");
        assert_eq!(canonical_hostname(""), "");
        // FQDN with 3-letter TLD
        assert_eq!(canonical_hostname("box.example.com"), "box");
        // 2-letter TLD
        assert_eq!(canonical_hostname("host.uk"), "host");
    }

    #[test]
    fn test_canonical_hostname_preserves_mac_style_names() {
        // AP / phone / camera hostnames where dots are part of the
        // identifier, not a domain suffix — last segment is digits, not a TLD.
        assert_eq!(canonical_hostname("AP38B8.1234.2345"), "AP38B8.1234.2345");
        assert_eq!(canonical_hostname("SEP000.AAAA.BBBB"), "SEP000.AAAA.BBBB");
        // Mixed-case last segment also shouldn't be treated as TLD.
        assert_eq!(canonical_hostname("AP38B8.1234.AAAA"), "AP38B8.1234.AAAA");
    }

    #[test]
    fn test_apply_ip_ipv4_vs_ipv6() {
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
        assert!(d.last_ipv6.is_none());
        apply_ip(&mut d, "2001:db8::1", now);
        assert_eq!(d.last_ipv6.as_deref(), Some("2001:db8::1"));
    }
}

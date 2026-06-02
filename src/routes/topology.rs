//! Topology graph endpoint.
//!
//! `/topology/json` returns the cached CDP-sweep snapshot as a graph:
//! one node per unique host (local or remote), one edge per CDP adjacency
//! (source = local switch that sees the neighbor, target = neighbor).
//!
//! A node is "managed" if its canonical hostname matches a logical-device
//! name or any logical-device's `hostname`/`vars.hostname` field. The page
//! renders managed nodes in gray (matching the previous graphviz output).

use std::collections::HashSet;

use axum::{
    extract::State,
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use indexmap::IndexMap;
use serde::Serialize;

use crate::cdp_sweep::canonical_hostname;
use crate::routes::devices::load_all_device_configs;
use crate::state::AppState;

#[derive(Serialize)]
struct NodeView {
    /// Canonical hostname — used as the graph id and the primary label line.
    id: String,
    /// Display name (= id for now; future-proof for separate UI label).
    label: String,
    /// True if this node matches a logical device we manage.
    managed: bool,
    /// Optional secondary lines (description, role, IP, platform, version).
    description: Option<String>,
    role: Option<String>,
    ip: Option<String>,
    platform: Option<String>,
    version: Option<String>,
    /// If managed, link to the device detail page.
    href: Option<String>,
}

#[derive(Serialize)]
struct EdgeView {
    /// Stable id so client-side diff-merges can match across polls.
    id: String,
    source: String,
    target: String,
    /// Port on the source (local) side that the adjacency was observed on.
    sport: Option<String>,
    /// Port on the target (remote) side.
    tport: Option<String>,
}

#[derive(Serialize)]
struct TopologyResponse {
    /// ISO 8601 timestamp of the underlying CDP poll; null if no poll yet.
    fetched_at: Option<String>,
    node_count: usize,
    edge_count: usize,
    nodes: Vec<NodeView>,
    edges: Vec<EdgeView>,
}

/// Build the set of canonical-hostname strings that count as "managed".
/// Currently: each logical-device file's name (canonicalized) plus its
/// `hostname` / `vars.hostname` field if set.
fn managed_set(state: &AppState) -> HashSet<String> {
    let mut set = HashSet::new();
    let Some(base) = state.config.cfggen_base_dir.as_ref() else {
        return set;
    };
    let configs = load_all_device_configs(base);
    for (name, cfg) in &configs {
        set.insert(canonical_hostname(name).to_ascii_lowercase());
        let hostname = cfg
            .get("hostname")
            .or_else(|| cfg.get("vars").and_then(|v| v.get("hostname")))
            .and_then(|v| v.as_str());
        if let Some(h) = hostname {
            set.insert(canonical_hostname(h).to_ascii_lowercase());
        }
    }
    set
}

/// Add a node to the map if not present; refresh its `managed`/aux fields
/// if a later mention provides more info.
fn upsert_node(
    map: &mut IndexMap<String, NodeView>,
    managed: &HashSet<String>,
    canonical: &str,
    platform: Option<&str>,
    version: Option<&str>,
    ip: Option<&str>,
) {
    let id = canonical.to_string();
    let entry = map.entry(id.clone()).or_insert_with(|| {
        let is_managed = managed.contains(&canonical.to_ascii_lowercase());
        NodeView {
            id: id.clone(),
            label: id.clone(),
            managed: is_managed,
            description: None,
            role: None,
            ip: None,
            platform: None,
            version: None,
            href: if is_managed {
                Some(format!("/devices/{}", canonical_device_name(canonical)))
            } else {
                None
            },
        }
    });

    // Backfill any missing aux fields from this mention.
    if entry.platform.is_none() {
        entry.platform = platform.filter(|s| !s.is_empty()).map(String::from);
    }
    if entry.version.is_none() {
        entry.version = version.filter(|s| !s.is_empty()).map(|v| {
            // Versions can be multi-line; keep the first line for display.
            v.lines().next().unwrap_or(v).trim().to_string()
        });
    }
    if entry.ip.is_none() {
        entry.ip = ip.filter(|s| !s.is_empty()).map(String::from);
    }
}

/// For a managed node, the logical-device URL uses the prefix-style name
/// (e.g. "AD6-X013" rather than full hostname "AD6-X013-S1147"). For now
/// we just use the canonical form — devices.rs accepts either.
fn canonical_device_name(canonical: &str) -> &str {
    canonical
}

async fn topology_json(State(state): State<AppState>) -> Response {
    let snap = state.cdp_snapshot.read().await;
    let (entries, fetched_at) = match snap.as_ref() {
        Some(s) => (s.entries.clone(), Some(s.fetched_at.to_rfc3339())),
        None => (Vec::new(), None),
    };
    drop(snap);

    let managed = managed_set(&state);
    let mut nodes: IndexMap<String, NodeView> = IndexMap::new();
    let mut edges: Vec<EdgeView> = Vec::new();

    // Optional: enrich managed nodes' description/role/IP from logical-device
    // configs and seen_assets. Walked outside the loop.
    let device_configs = state
        .config
        .cfggen_base_dir
        .as_ref()
        .map(|base| load_all_device_configs(base))
        .unwrap_or_default();
    let seen = state.seen_assets.read().await;

    for entry in &entries {
        // Source = local switch (the one that SAW the neighbor).
        let local = entry
            .local_asset_inventory
            .as_deref()
            .or(entry.local_host.as_deref())
            .map(|s| canonical_hostname(s).to_string())
            .filter(|s| !s.is_empty());
        // Target = remote neighbor.
        let remote = entry
            .remote_host
            .as_deref()
            .map(|s| canonical_hostname(s).to_string())
            .filter(|s| !s.is_empty());

        let (Some(src), Some(dst)) = (local, remote) else {
            continue;
        };

        upsert_node(&mut nodes, &managed, &src, None, None, None);
        upsert_node(
            &mut nodes,
            &managed,
            &dst,
            entry.remote_platform.as_deref(),
            entry.remote_version.as_deref(),
            entry.remote_ipaddress.as_deref(),
        );

        let edge_id = format!(
            "{}|{}->{}|{}",
            src,
            entry.local_interface.as_deref().unwrap_or(""),
            dst,
            entry.remote_interface.as_deref().unwrap_or(""),
        );
        edges.push(EdgeView {
            id: edge_id,
            source: src,
            target: dst,
            sport: entry.local_interface.clone().filter(|s| !s.is_empty()),
            tport: entry.remote_interface.clone().filter(|s| !s.is_empty()),
        });
    }

    // Enrich managed nodes with description/role from their logical-device JSON,
    // plus IP from seen_assets if available.
    for node in nodes.values_mut() {
        if !node.managed {
            continue;
        }
        // Try the canonical name as a device name first, then fall back to
        // hostname-match against any device's `hostname` field.
        let cfg = device_configs.get(&node.id).or_else(|| {
            device_configs.values().find(|cfg| {
                let h = cfg
                    .get("hostname")
                    .or_else(|| cfg.get("vars").and_then(|v| v.get("hostname")))
                    .and_then(|v| v.as_str());
                h.map(|h| canonical_hostname(h).eq_ignore_ascii_case(&node.id))
                    .unwrap_or(false)
            })
        });
        if let Some(c) = cfg {
            if node.description.is_none() {
                node.description = c
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            if node.role.is_none() {
                node.role = c.get("role").and_then(|v| v.as_str()).map(|s| s.to_string());
            }
        }
        if node.ip.is_none() {
            // Find a seen_assets entry whose hostname matches (case-insensitive).
            let ip = seen.values().find_map(|d| {
                let h = d.hostname.as_deref()?;
                if canonical_hostname(h).eq_ignore_ascii_case(&node.id) {
                    d.last_ipv4.clone().or_else(|| d.last_ipv6.clone())
                } else {
                    None
                }
            });
            if let Some(ip) = ip {
                node.ip = Some(ip);
            }
        }
    }

    let node_vec: Vec<NodeView> = nodes.into_values().collect();
    let resp = TopologyResponse {
        fetched_at,
        node_count: node_vec.len(),
        edge_count: edges.len(),
        nodes: node_vec,
        edges,
    };

    Json(resp).into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/topology/json", get(topology_json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp_sweep::CdpSnapshot;
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
    };
    use clap::Parser;
    use indexmap::IndexMap;
    use tower::ServiceExt;

    use crate::auth::htpasswd::HtpasswdStore;
    use crate::config::AppConfig;

    fn make_state() -> AppState {
        let cfg = AppConfig::try_parse_from(["aynmsgui", "--htpasswd-file", "/dev/null"])
            .expect("parse");
        AppState::new(cfg, HtpasswdStore::from_str(""), None, IndexMap::new())
    }

    #[tokio::test]
    async fn test_topology_json_empty_when_no_snapshot() {
        let state = make_state();
        let app = routes().with_state(state);
        let req = Request::builder()
            .method(Method::GET)
            .uri("/topology/json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["node_count"], 0);
        assert_eq!(body["edge_count"], 0);
        assert!(body["fetched_at"].is_null());
    }

    #[tokio::test]
    async fn test_topology_json_builds_nodes_and_edges() {
        let state = make_state();
        // Inject a synthetic CDP snapshot
        *state.cdp_snapshot.write().await = Some(CdpSnapshot {
            fetched_at: chrono::Utc::now(),
            entries: vec![crate::cdp_sweep::CdpEntry {
                local_asset_inventory: Some("SW-LOCAL".to_string()),
                local_host: Some("SW-LOCAL".to_string()),
                local_interface: Some("Gi1/0/1".to_string()),
                remote_host: Some("REMOTE-PEER.example.com".to_string()),
                remote_ipaddress: Some("10.0.0.5".to_string()),
                remote_interface: Some("Gi0/0".to_string()),
                remote_platform: Some("cisco C9200CX".to_string()),
                remote_version: Some("17.15.4".to_string()),
            }],
        });

        let app = routes().with_state(state);
        let req = Request::builder()
            .method(Method::GET)
            .uri("/topology/json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["node_count"], 2);
        assert_eq!(body["edge_count"], 1);
        let nodes = body["nodes"].as_array().unwrap();
        let remote = nodes
            .iter()
            .find(|n| n["id"] == "REMOTE-PEER")
            .expect("remote node present, FQDN suffix stripped");
        assert_eq!(remote["ip"], "10.0.0.5");
        assert_eq!(remote["platform"], "cisco C9200CX");
        let edge = &body["edges"][0];
        assert_eq!(edge["source"], "SW-LOCAL");
        assert_eq!(edge["target"], "REMOTE-PEER");
        assert_eq!(edge["sport"], "Gi1/0/1");
        assert_eq!(edge["tport"], "Gi0/0");
    }
}

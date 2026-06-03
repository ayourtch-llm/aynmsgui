use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Serialize;
use tracing::{debug, warn};

use crate::routes::devices::{load_all_device_configs, serial_to_device_names};
use crate::state::AppState;

// ── View models ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DiffLine {
    pub text: String,
    /// True iff the line begins with "no " — Cisco's negation. We tint
    /// these red and tint everything else green so the eye can scan a
    /// preview the way it scans a unified diff.
    pub is_remove: bool,
}

#[derive(Serialize)]
pub struct DiffOverviewItem {
    pub name: String,
    pub device_name: String,
    /// Live hostname from seen_assets, falling back to the configured
    /// hostname from the logical-device JSON when seen_assets has nothing.
    /// "-" if neither source has anything.
    pub hostname: String,
    pub has_diff: bool,
    /// The preview slice of the delta (up to PREVIEW_LINES), classified
    /// per line. The full delta is on the diff detail page.
    pub diff_lines: Vec<DiffLine>,
    /// Number of additional lines that the preview did NOT include.
    /// Surfaced under the preview as "… +N more lines".
    pub diff_more: usize,
    pub has_more: bool,
    pub diff_total: usize,
    pub diff_added: usize,
    pub diff_removed: usize,
    pub status_class: &'static str,
    pub status_text: &'static str,
    /// True if the matching seen_assets entry's last_seen is older than the
    /// freshness window (or missing entirely). The per-row Retrieve button
    /// is disabled in this case — same reason /retrieve disables it.
    pub retrieve_disabled: bool,
    pub retrieve_reason: String,
    /// True when the logical device's JSON has more than one module with
    /// a non-empty SKU AND those modules carry distinct serials — i.e. a
    /// real stack (multiple chassis sharing one config plane). The
    /// template uses this to render a "stack: N chassis" badge.
    pub is_stack: bool,
    /// Number of chassis in the stack (1 for a single switch, ≥2 for a
    /// stack). Always ≥1 when the device has any real (non-stub) module.
    pub chassis_count: usize,
    /// Serials of every non-stub module, comma-separated for the badge
    /// tooltip. Empty when chassis_count <= 1.
    pub chassis_serials: String,
}

/// How many delta lines to embed in the overview preview before
/// "… +N more lines" kicks in. Combined with the .diff-preview
/// max-height in CSS this gives a compact but informative snapshot.
const PREVIEW_LINES: usize = 10;

#[derive(Serialize)]
struct DiffOverviewCtx {
    items: Vec<DiffOverviewItem>,
    quicksearch_table_id: &'static str,
}

#[derive(Serialize)]
pub struct DiffDetailView {
    pub name: String,
    pub device_name: String,
    pub delta: String,
    pub target_config: String,
    pub current_config: String,
    /// Side-by-side rendering: every line, paired between the current
    /// (left) and target (right) configs, with `kind` ∈
    /// {"equal", "modify", "delete", "insert", "gap"}. Empty when
    /// either side is empty so the template can show a "no current
    /// retrieved yet" placeholder instead.
    pub diff_rows: Vec<DiffRow>,
    pub has_diff_rows: bool,
    pub equal_count: usize,
    pub change_count: usize,
}

#[derive(Serialize)]
pub struct DiffRow {
    pub left: String,
    pub right: String,
    /// Empty string when there's no line number (gap on that side).
    pub left_no: String,
    pub right_no: String,
    pub kind: &'static str,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn diff_overview(State(state): State<AppState>) -> Response {
    let (target_path, current_path) = match (
        &state.config.target_configs_path,
        &state.config.current_configs_path,
    ) {
        (Some(t), Some(c)) if t.exists() && c.exists() => (t, c),
        _ => {
            let html = state
                .templates
                .render_message(
                    "Config Diff",
                    Some("Config diff not configured: target_configs_path or current_configs_path is not set"),
                    None,
                    None,
                )
                .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
            return Html(html).into_response();
        }
    };

    debug!(
        target_path = %target_path.display(),
        current_path = %current_path.display(),
        "Loading config diff overview"
    );

    let entries = match std::fs::read_dir(target_path) {
        Ok(e) => e,
        Err(err) => {
            warn!(path = %target_path.display(), error = %err, "Failed to read target configs directory");
            let msg = format!("Failed to read target configs directory: {err}");
            let html = state
                .templates
                .render_message("Config Diff", Some(&msg), None, None)
                .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
            return Html(html).into_response();
        }
    };

    // Load logical-device configs once; both the serial→name map and the
    // per-row hostname lookup pull from this.
    let device_configs = state
        .config
        .cfggen_base_dir
        .as_ref()
        .map(|base| load_all_device_configs(base))
        .unwrap_or_default();
    let serial_map = serial_to_device_names(&device_configs);

    // Per-row Retrieve button needs to know whether the device is fresh.
    // Compute the freshness cutoff once and snapshot seen_assets.
    let max_age_secs = state.config.retrieve_max_age_secs;
    let freshness_cutoff = if max_age_secs == 0 {
        None
    } else {
        Some(chrono::Utc::now() - chrono::Duration::seconds(max_age_secs as i64))
    };
    let seen = state.seen_assets.read().await;

    let mut items: Vec<DiffOverviewItem> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!(error = %err, "Error reading directory entry");
                continue;
            }
        };

        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        // Only process .cfg files
        let Some(name) = file_name_str.strip_suffix(".cfg") else {
            continue;
        };
        let name = name.to_string();

        let target_file = target_path.join(&*file_name_str);
        let current_file = current_path.join(&*file_name_str);

        let target_config = match std::fs::read_to_string(&target_file) {
            Ok(c) => aycfgapply::normalize::normalize_target_config(&c),
            Err(err) => {
                warn!(path = %target_file.display(), error = %err, "Failed to read target config");
                continue;
            }
        };

        let current_config = match std::fs::read_to_string(&current_file) {
            Ok(c) => aycfgapply::normalize::normalize_config(&c),
            Err(_) => {
                // No current config — treat as empty (device has nothing applied yet)
                debug!(name = %name, "No current config found, treating as empty");
                String::new()
            }
        };

        // Short-circuit identical normalized configs without parsing.
        // Both sides funnel through aycfgapply's normalize_body, so byte
        // equality guarantees an empty delta — and skips the expensive
        // aycicdiff parse + classify for unchanged devices.
        let (has_diff, diff_lines, diff_more, diff_total, diff_added, diff_removed) =
            if target_config == current_config {
                (false, Vec::new(), 0usize, 0usize, 0usize, 0usize)
            } else {
                let delta = aycicdiff::generate_delta(&current_config, &target_config, None);
                let all: Vec<&str> = delta.lines().collect();
                let total = all.len();
                let removed = all
                    .iter()
                    .filter(|l| l.trim_start().starts_with("no "))
                    .count();
                let added = total.saturating_sub(removed);
                let has_diff = !delta.trim().is_empty();
                let preview_lines: Vec<DiffLine> = all
                    .iter()
                    .take(PREVIEW_LINES)
                    .map(|l| DiffLine {
                        text: (*l).to_string(),
                        is_remove: l.trim_start().starts_with("no "),
                    })
                    .collect();
                let more = total.saturating_sub(preview_lines.len());
                (has_diff, preview_lines, more, total, added, removed)
            };

        let logical_names = serial_map.get(&name);
        let device_name = logical_names
            .map(|names| names.join(", "))
            .unwrap_or_else(|| "-".to_string());

        // Hostname: live seen_assets hostname first; if that's missing,
        // fall back to the hostname configured in any of the matching
        // logical-device JSONs (root `hostname` or `vars.hostname`).
        let hostname = seen
            .get(&name)
            .and_then(|d| d.hostname.clone().filter(|s| !s.is_empty()))
            .or_else(|| {
                logical_names.and_then(|names| {
                    names.iter().find_map(|ln| {
                        let cfg = device_configs.get(ln)?;
                        cfg.get("hostname")
                            .and_then(|v| v.as_str())
                            .or_else(|| {
                                cfg.get("vars")
                                    .and_then(|v| v.get("hostname"))
                                    .and_then(|v| v.as_str())
                            })
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                    })
                })
            })
            .unwrap_or_else(|| "-".to_string());

        // Stack detection. Walk any matching logical-device's modules and
        // collect the serials of non-stub entries (those with a non-empty
        // SKU). When >1 distinct serial → real stack. Single switch with
        // a stub slot-0 carrying the chassis serial is NOT a stack —
        // that's just an extraction-time placeholder shared with the
        // real module.
        let (is_stack, chassis_count, chassis_serials) = logical_names
            .and_then(|names| names.first())
            .and_then(|ln| device_configs.get(ln))
            .map(|cfg| {
                let mut serials: Vec<String> = Vec::new();
                let mut seen_set: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                if let Some(modules) = cfg.get("modules").and_then(|m| m.as_array()) {
                    for module in modules {
                        let sku = module.get("SKU").and_then(|v| v.as_str()).unwrap_or("");
                        if sku.is_empty() {
                            continue; // stub
                        }
                        if let Some(s) = module
                            .get("serial")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            if seen_set.insert(s.to_string()) {
                                serials.push(s.to_string());
                            }
                        }
                    }
                }
                (serials.len() > 1, serials.len(), serials.join(", "))
            })
            .unwrap_or((false, 0, String::new()));

        // Retrieve-button gating: disabled if the device isn't in
        // seen_assets at all, or its last_seen is past the freshness
        // cutoff. The reason is surfaced as a button tooltip.
        let (retrieve_disabled, retrieve_reason) = match seen.get(&name) {
            None => (true, "Not in seen-assets — never reported".to_string()),
            Some(d) => match (freshness_cutoff, d.last_seen()) {
                (Some(cutoff), Some(ts)) if ts < cutoff => {
                    (true, format!("Last seen {} (older than freshness window)", ts.to_rfc3339()))
                }
                (Some(_), None) => (true, "No last-seen timestamp on this device".to_string()),
                _ => (false, String::new()),
            },
        };

        items.push(DiffOverviewItem {
            name,
            device_name,
            hostname,
            has_diff,
            diff_lines,
            diff_more,
            has_more: diff_more > 0,
            diff_total,
            diff_added,
            diff_removed,
            status_class: if has_diff { "has-diff" } else { "no-diff" },
            status_text: if has_diff { "Changes" } else { "No changes" },
            retrieve_disabled,
            retrieve_reason,
            is_stack,
            chassis_count,
            chassis_serials,
        });
    }
    drop(seen);

    // Sort by name for stable output
    items.sort_by(|a, b| a.name.cmp(&b.name));

    let html = state
        .templates
        .render_page(
            &state.templates.diff_overview,
            "Config Diff Overview",
            "",
            &DiffOverviewCtx {
                items,
                quicksearch_table_id: "diff-table",
            },
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));

    Html(html).into_response()
}

pub async fn diff_detail(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let (target_path, current_path) = match (
        &state.config.target_configs_path,
        &state.config.current_configs_path,
    ) {
        (Some(t), Some(c)) if t.exists() => (t, c),
        _ => {
            let html = state
                .templates
                .render_message("Config Diff", Some("Config diff not configured"), None, None)
                .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
            return Html(html).into_response();
        }
    };

    debug!(name = %name, "Loading config diff detail");

    let file_name = format!("{}.cfg", name);
    let target_file = target_path.join(&file_name);
    let current_file = current_path.join(&file_name);

    let target_config = match std::fs::read_to_string(&target_file) {
        Ok(c) => aycfgapply::normalize::normalize_target_config(&c),
        Err(err) => {
            warn!(path = %target_file.display(), error = %err, "Target config not found");
            let msg = format!("Config '{}' not found", name);
            let html = state
                .templates
                .render_message("Not Found", Some(&msg), None, Some(("/diff", "Back to Diffs")))
                .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
            return (StatusCode::NOT_FOUND, Html(html)).into_response();
        }
    };

    let current_config = match std::fs::read_to_string(&current_file) {
        Ok(c) => aycfgapply::normalize::normalize_config(&c),
        Err(_) => {
            debug!(name = %name, "No current config found for detail view, using empty");
            String::new()
        }
    };

    let delta = if target_config == current_config {
        String::new()
    } else {
        aycicdiff::generate_delta(&current_config, &target_config, None)
    };

    // Look up logical device name for this serial
    let device_name = state
        .config
        .cfggen_base_dir
        .as_ref()
        .and_then(|base| {
            let map = serial_to_device_names(&load_all_device_configs(base));
            map.get(&name).map(|names| names.join(", "))
        })
        .unwrap_or_else(|| "-".to_string());

    let (diff_rows, equal_count, change_count) =
        compute_side_by_side(&current_config, &target_config);
    let has_diff_rows = !diff_rows.is_empty() && (!current_config.is_empty() || !target_config.is_empty());

    let view = DiffDetailView {
        name: name.clone(),
        device_name,
        delta,
        target_config,
        current_config,
        diff_rows,
        has_diff_rows,
        equal_count,
        change_count,
    };

    let title = format!("Config Diff: {name}");
    let html = state
        .templates
        .render_page(&state.templates.diff_detail, &title, "", &view)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

// ── Side-by-side diff computation ────────────────────────────────────────────

/// Build a paired list of rows for the side-by-side view. Equal lines
/// occupy both sides at the same row; deletions/insertions are paired
/// up when adjacent (so back-to-back `delete X` / `insert Y` becomes a
/// single "modify" row), and lone deletes/inserts become rows with a
/// gap on the opposite side.
///
/// Returns (rows, equal_count, change_count).
fn compute_side_by_side(current: &str, target: &str) -> (Vec<DiffRow>, usize, usize) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(current, target);
    let changes: Vec<_> = diff
        .iter_all_changes()
        .map(|c| (c.tag(), c.old_index(), c.new_index(), c.value().to_string()))
        .collect();

    let mut rows: Vec<DiffRow> = Vec::new();
    let mut equal_count = 0usize;
    let mut change_count = 0usize;
    let mut i = 0;
    while i < changes.len() {
        match changes[i].0 {
            ChangeTag::Equal => {
                let (_, oi, ni, ref v) = changes[i];
                rows.push(DiffRow {
                    left: trim_trailing_newline(v).to_string(),
                    right: trim_trailing_newline(v).to_string(),
                    left_no: oi.map(|n| (n + 1).to_string()).unwrap_or_default(),
                    right_no: ni.map(|n| (n + 1).to_string()).unwrap_or_default(),
                    kind: "equal",
                });
                equal_count += 1;
                i += 1;
            }
            ChangeTag::Delete | ChangeTag::Insert => {
                // Collect run of deletes then run of inserts so adjacent
                // pairs can be shown side-by-side as "modify" rows.
                let mut deletes: Vec<(Option<usize>, String)> = Vec::new();
                while i < changes.len() && changes[i].0 == ChangeTag::Delete {
                    let (_, oi, _, ref v) = changes[i];
                    deletes.push((oi, trim_trailing_newline(v).to_string()));
                    i += 1;
                }
                let mut inserts: Vec<(Option<usize>, String)> = Vec::new();
                while i < changes.len() && changes[i].0 == ChangeTag::Insert {
                    let (_, _, ni, ref v) = changes[i];
                    inserts.push((ni, trim_trailing_newline(v).to_string()));
                    i += 1;
                }
                let max = deletes.len().max(inserts.len());
                for k in 0..max {
                    let left = deletes.get(k).cloned();
                    let right = inserts.get(k).cloned();
                    let (left_text, left_no) = left
                        .map(|(oi, v)| (v, oi.map(|n| (n + 1).to_string()).unwrap_or_default()))
                        .unwrap_or((String::new(), String::new()));
                    let (right_text, right_no) = right
                        .map(|(ni, v)| (v, ni.map(|n| (n + 1).to_string()).unwrap_or_default()))
                        .unwrap_or((String::new(), String::new()));
                    let kind = if !left_text.is_empty() && !right_text.is_empty() {
                        "modify"
                    } else if !left_text.is_empty() {
                        "delete"
                    } else {
                        "insert"
                    };
                    rows.push(DiffRow {
                        left: left_text,
                        right: right_text,
                        left_no,
                        right_no,
                        kind,
                    });
                    change_count += 1;
                }
            }
        }
    }
    (rows, equal_count, change_count)
}

fn trim_trailing_newline(s: &str) -> &str {
    s.strip_suffix('\n').unwrap_or(s)
}

// ── Routes ───────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/diff", get(diff_overview))
        .route("/diff/{name}", get(diff_detail))
        .route("/diff/{name}/recompile", axum::routing::post(recompile_one))
        .route("/diff/recompile-all", axum::routing::post(recompile_all))
}

// ── Recompile actions ────────────────────────────────────────────────────────

/// POST /diff/{name}/recompile — re-runs compile_device_config for the
/// device backing the .cfg whose serial is `name`, refreshing the target
/// config so the page reflects current cfggen state. Redirects back to
/// the diff overview.
pub async fn recompile_one(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let Some(cfggen_base) = state.config.cfggen_base_dir.as_ref().filter(|p| p.exists()) else {
        return axum::response::Redirect::to("/diff").into_response();
    };
    let device_name = serial_to_logical_name(cfggen_base, &name).unwrap_or(name.clone());
    if let Err(e) = crate::routes::devices::compile_device_config(
        &device_name,
        cfggen_base,
        &state.config,
    ) {
        warn!(device = %device_name, error = %e, "Recompile failed");
    }
    axum::response::Redirect::to("/diff").into_response()
}

/// POST /diff/recompile-all — recompiles every logical-device target
/// config. Useful after a structural cfggen change.
pub async fn recompile_all(State(state): State<AppState>) -> Response {
    let Some(cfggen_base) = state.config.cfggen_base_dir.as_ref().filter(|p| p.exists()) else {
        return axum::response::Redirect::to("/diff").into_response();
    };
    let logical_dir = cfggen_base.join("logical-devices");
    if let Ok(entries) = std::fs::read_dir(&logical_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = if path.is_dir() {
                if !path.join("config.json").exists() {
                    continue;
                }
                path.file_name().and_then(|s| s.to_str()).map(|s| s.to_string())
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                path.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
            } else {
                None
            };
            if let Some(name) = name {
                if let Err(e) = crate::routes::devices::compile_device_config(
                    &name,
                    cfggen_base,
                    &state.config,
                ) {
                    warn!(device = %name, error = %e, "Recompile failed");
                }
            }
        }
    }
    axum::response::Redirect::to("/diff").into_response()
}

fn serial_to_logical_name(cfggen_base: &std::path::Path, raw: &str) -> Option<String> {
    let logical_dir = cfggen_base.join("logical-devices");
    if logical_dir.join(format!("{}.json", raw)).exists()
        || logical_dir.join(raw).join("config.json").exists()
    {
        return Some(raw.to_string());
    }
    let configs = load_all_device_configs(cfggen_base);
    let map = serial_to_device_names(&configs);
    map.get(raw).and_then(|n| n.first().cloned())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
    };
    use clap::Parser;
    use indexmap::IndexMap;
    use tower::ServiceExt;

    use crate::auth::htpasswd::HtpasswdStore;
    use crate::config::AppConfig;
    use crate::state::AppState;

    fn make_htpasswd() -> HtpasswdStore {
        HtpasswdStore::from_str("")
    }

    fn make_config_with_paths(
        target: Option<&std::path::Path>,
        current: Option<&std::path::Path>,
    ) -> AppConfig {
        let mut args = vec!["aynmsgui", "--htpasswd-file", "/dev/null"];
        let target_str;
        let current_str;

        // Override defaults: use provided paths, or nonexistent paths for "not configured"
        target_str = target.map(|p| p.to_str().unwrap().to_string())
            .unwrap_or_else(|| "/nonexistent/target".to_string());
        args.push("--target-configs-path");
        args.push(&target_str);

        current_str = current.map(|p| p.to_str().unwrap().to_string())
            .unwrap_or_else(|| "/nonexistent/current".to_string());
        args.push("--current-configs-path");
        args.push(&current_str);

        AppConfig::try_parse_from(&args).expect("test config parse")
    }

    fn build_app(config: AppConfig) -> axum::Router {
        let state = AppState::new(config, make_htpasswd(), None, IndexMap::new());
        routes().with_state(state)
    }

    async fn get_body(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap().to_string();
        (status, body)
    }

    // ── Test 1: not configured ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_diff_not_configured() {
        let config = make_config_with_paths(None, None);
        let app = build_app(config);

        let (status, body) = get_body(app, "/diff").await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("not configured"),
            "expected 'not configured' in body, got: {}",
            body
        );
    }

    // ── Test 2: overview with changes ─────────────────────────────────────────

    #[tokio::test]
    async fn test_diff_overview_with_changes() {
        let target_dir = tempfile::TempDir::new().unwrap();
        let current_dir = tempfile::TempDir::new().unwrap();

        std::fs::write(
            target_dir.path().join("switch-01.cfg"),
            "hostname sw01\ninterface Gi0/1\n description UPLINK\n",
        )
        .unwrap();
        std::fs::write(
            current_dir.path().join("switch-01.cfg"),
            "hostname sw01\ninterface Gi0/1\n description OLD\n",
        )
        .unwrap();

        let config = make_config_with_paths(Some(target_dir.path()), Some(current_dir.path()));
        let app = build_app(config);

        let (status, body) = get_body(app, "/diff").await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("switch-01"),
            "expected 'switch-01' in body, got: {}",
            body
        );
        assert!(
            body.contains("Changes"),
            "expected 'Changes' in body, got: {}",
            body
        );
    }

    // ── Test 3: overview no changes ───────────────────────────────────────────

    #[tokio::test]
    async fn test_diff_overview_no_changes() {
        let target_dir = tempfile::TempDir::new().unwrap();
        let current_dir = tempfile::TempDir::new().unwrap();

        let cfg = "hostname sw01\ninterface Gi0/1\n description UPLINK\n";
        std::fs::write(target_dir.path().join("switch-01.cfg"), cfg).unwrap();
        std::fs::write(current_dir.path().join("switch-01.cfg"), cfg).unwrap();

        let config = make_config_with_paths(Some(target_dir.path()), Some(current_dir.path()));
        let app = build_app(config);

        let (status, body) = get_body(app, "/diff").await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("switch-01"),
            "expected 'switch-01' in body, got: {}",
            body
        );
        assert!(
            body.contains("No changes"),
            "expected 'No changes' in body, got: {}",
            body
        );
    }

    // ── Test 4: diff detail found ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_diff_detail_found() {
        let target_dir = tempfile::TempDir::new().unwrap();
        let current_dir = tempfile::TempDir::new().unwrap();

        std::fs::write(
            target_dir.path().join("switch-01.cfg"),
            "hostname sw01\ninterface Gi0/1\n description UPLINK\n",
        )
        .unwrap();
        std::fs::write(
            current_dir.path().join("switch-01.cfg"),
            "hostname sw01\ninterface Gi0/1\n description OLD\n",
        )
        .unwrap();

        let config = make_config_with_paths(Some(target_dir.path()), Some(current_dir.path()));
        let app = build_app(config);

        let (status, body) = get_body(app, "/diff/switch-01").await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("switch-01"),
            "expected 'switch-01' in body, got: {}",
            body
        );
        // The delta should mention the interface or description change
        assert!(
            body.contains("UPLINK") || body.contains("description") || body.contains("interface"),
            "expected delta content in body, got: {}",
            body
        );
    }

    // ── Test 5: diff detail not found ─────────────────────────────────────────

    #[tokio::test]
    async fn test_diff_detail_not_found() {
        let target_dir = tempfile::TempDir::new().unwrap();
        let current_dir = tempfile::TempDir::new().unwrap();

        let config = make_config_with_paths(Some(target_dir.path()), Some(current_dir.path()));
        let app = build_app(config);

        let (status, _body) = get_body(app, "/diff/nonexistent").await;

        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "expected 404 for nonexistent config"
        );
    }
}

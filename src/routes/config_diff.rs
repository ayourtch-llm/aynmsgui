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
pub struct DiffOverviewItem {
    pub name: String,
    pub device_name: String,
    pub has_diff: bool,
    pub diff_preview: String,
    pub status_class: &'static str,
    pub status_text: &'static str,
}

#[derive(Serialize)]
struct DiffOverviewCtx {
    items: Vec<DiffOverviewItem>,
}

#[derive(Serialize)]
pub struct DiffDetailView {
    pub name: String,
    pub device_name: String,
    pub delta: String,
    pub target_config: String,
    pub current_config: String,
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

    // Build serial → logical device name(s) mapping
    let serial_map = state
        .config
        .cfggen_base_dir
        .as_ref()
        .map(|base| serial_to_device_names(&load_all_device_configs(base)))
        .unwrap_or_default();

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
        let (has_diff, diff_preview) = if target_config == current_config {
            (false, "No changes".to_string())
        } else {
            let delta = aycicdiff::generate_delta(&current_config, &target_config, None);
            let has_diff = !delta.trim().is_empty();
            let preview = if has_diff {
                delta.lines().take(3).collect::<Vec<_>>().join("\n")
            } else {
                "No changes".to_string()
            };
            (has_diff, preview)
        };

        let device_name = serial_map
            .get(&name)
            .map(|names| names.join(", "))
            .unwrap_or_else(|| "-".to_string());

        items.push(DiffOverviewItem {
            name,
            device_name,
            has_diff,
            diff_preview,
            status_class: if has_diff { "has-diff" } else { "no-diff" },
            status_text: if has_diff { "Changes" } else { "No changes" },
        });
    }

    // Sort by name for stable output
    items.sort_by(|a, b| a.name.cmp(&b.name));

    let html = state
        .templates
        .render_page(
            &state.templates.diff_overview,
            "Config Diff Overview",
            "",
            &DiffOverviewCtx { items },
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

    let view = DiffDetailView {
        name: name.clone(),
        device_name,
        delta,
        target_config,
        current_config,
    };

    let title = format!("Config Diff: {name}");
    let html = state
        .templates
        .render_page(&state.templates.diff_detail, &title, "", &view)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

// ── Routes ───────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/diff", get(diff_overview))
        .route("/diff/{name}", get(diff_detail))
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

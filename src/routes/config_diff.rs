use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Serialize;
use tracing::{debug, warn};

use crate::state::AppState;

// ── View models ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DiffOverviewItem {
    pub name: String,
    pub has_diff: bool,
    pub diff_preview: String,
}

#[derive(Serialize)]
pub struct DiffDetailView {
    pub name: String,
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
            let html = "<html><body><p>Config diff not configured: \
                        target_configs_path or current_configs_path is not set</p></body></html>";
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
            let html = format!(
                "<html><body><p>Failed to read target configs directory: {}</p></body></html>",
                err
            );
            return Html(html).into_response();
        }
    };

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

        let delta = aycicdiff::generate_delta(&current_config, &target_config, None);
        let has_diff = !delta.trim().is_empty();

        let diff_preview = if has_diff {
            delta
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            "No changes".to_string()
        };

        items.push(DiffOverviewItem {
            name,
            has_diff,
            diff_preview,
        });
    }

    // Sort by name for stable output
    items.sort_by(|a, b| a.name.cmp(&b.name));

    let rows: String = items
        .iter()
        .map(|item| {
            let status_class = if item.has_diff { "has-diff" } else { "no-diff" };
            let status_text = if item.has_diff { "Changes" } else { "No changes" };
            let action = if item.has_diff {
                format!("<a href=\"/apply/{name}\">Apply</a>", name = item.name)
            } else {
                String::new()
            };
            format!(
                "<tr class=\"{status_class}\">\
                 <td><a href=\"/diff/{name}\">{name}</a></td>\
                 <td>{status_text}</td>\
                 <td><pre>{preview}</pre></td>\
                 <td>{action}</td>\
                 </tr>",
                status_class = status_class,
                name = item.name,
                status_text = status_text,
                preview = html_escape(&item.diff_preview),
                action = action,
            )
        })
        .collect();

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Config Diff Overview</title></head>
<body>
<h1>Config Diff Overview</h1>
<table>
<tr><th>Device</th><th>Status</th><th>Preview</th><th>Action</th></tr>
{rows}
</table>
</body>
</html>"#,
        rows = rows,
    );

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
            let html = "<html><body><p>Config diff not configured</p></body></html>";
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
            return (
                StatusCode::NOT_FOUND,
                Html(format!(
                    "<html><body><p>Config '{}' not found</p></body></html>",
                    name
                )),
            )
                .into_response();
        }
    };

    let current_config = match std::fs::read_to_string(&current_file) {
        Ok(c) => aycfgapply::normalize::normalize_config(&c),
        Err(_) => {
            debug!(name = %name, "No current config found for detail view, using empty");
            String::new()
        }
    };

    let delta = aycicdiff::generate_delta(&current_config, &target_config, None);

    let view = DiffDetailView {
        name: name.clone(),
        delta: delta.clone(),
        target_config: target_config.clone(),
        current_config: current_config.clone(),
    };

    let html = render_diff_detail(&view);
    Html(html).into_response()
}

fn render_diff_detail(view: &DiffDetailView) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Config Diff: {name}</title></head>
<body>
<h1>Config Diff: {name}</h1>
<h2>Delta (commands to apply)</h2>
<pre>{delta}</pre>
<h2>Target Config</h2>
<pre>{target}</pre>
<h2>Current Config</h2>
<pre>{current}</pre>
</body>
</html>"#,
        name = view.name,
        delta = html_escape(&view.delta),
        target = html_escape(&view.target_config),
        current = html_escape(&view.current_config),
    )
}

/// Minimal HTML escaping for embedding config text inside <pre> blocks.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

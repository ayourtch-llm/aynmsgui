//! Per-device config reconcile workflow.
//!
//! `GET /diff/{name}/reconcile` runs `aycfggen::compile_device_traced` to
//! get both the compiled target config AND a per-line provenance map. It
//! then computes the same delta as the regular diff page and joins each
//! delta line to its target-side source — which service, which template
//! line, which prologue/epilogue — so the operator can see at a glance
//! which template change would absorb each hunk.
//!
//! This page is read-only in the first pass; the write actions (swap a
//! port's service, promote a hunk to a new service, edit a template line)
//! are tracked by task #32 in the reconcile work list.
//!
//! Delta-line ↔ target-line matching is by trimmed text first, then
//! disambiguated by the active context (current `interface X` block, etc).
//! The pipeline never invents provenance: if a line can't be located in
//! the target with confidence, we say "unknown" rather than guess.

use std::collections::HashMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use aycfggen::provenance::{LineProv, ProvSource};
use serde::Serialize;
use tracing::{debug, warn};

use crate::state::AppState;

// ── View models ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ReconcileLineCtx {
    /// The delta line itself, leading "no " preserved for clarity.
    text: String,
    /// Indentation level, used only for visual hint in the template.
    indent: usize,
    /// "add" if this line should be applied to the device,
    /// "remove" if the line begins with "no " (delta wants to negate it).
    direction: &'static str,
    /// Human-readable source label ("service access-vlan6:Port36",
    /// "template AD6-X001-...conf:42", "structural PORTS-START", etc).
    /// Empty if no provenance was found (typical for "remove" lines —
    /// the line we're removing is in current, not in target).
    source_label: String,
    /// CSS class hint: "src-template", "src-service-port", "src-svi",
    /// "src-element", "src-structural", "src-prologue", "src-epilogue",
    /// "src-unknown", "src-removed-from-current".
    source_class: &'static str,
    /// Optional secondary detail (e.g., the service name when the kind
    /// is PortService, so the template can render "→ swap to ..." actions).
    detail: String,
}

#[derive(Serialize)]
struct ReconcileCtx {
    name: String,
    device_name: String,
    hostname: String,
    has_delta: bool,
    /// Number of distinct port services referenced from this delta.
    /// Helps the operator gauge how concentrated the changes are.
    affected_services: usize,
    /// Number of delta lines we could attribute to a concrete source.
    resolved_count: usize,
    /// Number of delta lines we couldn't attribute.
    unresolved_count: usize,
    /// Number of delta lines (total).
    total_count: usize,
    /// List of delta lines + provenance.
    lines: Vec<ReconcileLineCtx>,
}

// ── Handler ──────────────────────────────────────────────────────────────────

pub async fn reconcile_detail(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Reconcile not configured", "cfggen_base_dir is unset", None),
    };
    let current_path = match &state.config.current_configs_path {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Reconcile not configured", "current_configs_path is unset", None),
    };

    debug!(name = %name, "Loading reconcile page");

    // Run the traced compile.
    use aycfggen::compile_traced::compile_device_traced;
    use aycfggen::fs_sources::{
        FsConfigElementSource, FsConfigTemplateSource, FsHardwareTemplateSource,
        FsLogicalDeviceSource, FsServiceSource, FsSoftwareImageSource,
    };
    let device_source = FsLogicalDeviceSource::new(cfggen_base.join("logical-devices"));
    let hw_source = FsHardwareTemplateSource::new(cfggen_base.join("hardware-templates"));
    let service_source = FsServiceSource::new(cfggen_base.join("services"));
    let template_source = FsConfigTemplateSource::new(cfggen_base.join("config-templates"));
    let element_source = FsConfigElementSource::new(cfggen_base.join("config-elements"));
    let image_source = FsSoftwareImageSource::new(cfggen_base.join("software-images"));

    let (target_text_raw, target_provs_raw) = match compile_device_traced(
        &name,
        &device_source,
        &hw_source,
        &service_source,
        &template_source,
        &element_source,
        &image_source,
    ) {
        Ok(pair) => pair,
        Err(err) => {
            warn!(name = %name, error = %err, "compile_device_traced failed");
            let msg = format!("Could not compile target config: {err}");
            return message(&state, "Compile Failed", &msg, Some(("/diff", "Back")));
        }
    };

    // Normalize both target and current the same way the regular diff page
    // does. Then filter the provenance vector in lockstep so prov[i] still
    // matches target_lines[i] after blank-line / `end` removal.
    let (target_text, target_provs) = normalize_with_provs(&target_text_raw, &target_provs_raw);

    let current_file = current_path.join(format!("{}.cfg", name));
    let current_text = match std::fs::read_to_string(&current_file) {
        Ok(c) => aycfgapply::normalize::normalize_config(&c),
        Err(_) => String::new(),
    };

    let delta = if target_text == current_text {
        String::new()
    } else {
        aycicdiff::generate_delta(&current_text, &target_text, None)
    };

    // Build a lookup index from trimmed text → all (line_idx, &LineProv).
    // A trimmed line can map to many targets (same port-config "switchport
    // mode access" appears under every access-VLAN service), so we keep the
    // full list and disambiguate by current interface context as we walk
    // the delta.
    let mut by_text: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in target_provs.iter().enumerate() {
        by_text
            .entry(p.text.trim().to_string())
            .or_default()
            .push(i);
    }

    let mut lines: Vec<ReconcileLineCtx> = Vec::new();
    let mut current_iface_ctx: Option<String> = None;
    let mut resolved = 0usize;
    let mut unresolved = 0usize;
    let mut affected_services_set = std::collections::HashSet::new();

    for raw_line in delta.lines() {
        let line = raw_line.to_string();
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let is_remove = trimmed.starts_with("no ");
        let direction = if is_remove { "remove" } else { "add" };

        // Track the active interface block (delta's own context). The
        // delta typically opens with `interface X` then indented changes,
        // closed by `exit`. We use that to disambiguate matches inside the
        // target — a `switchport mode access` line inside the delta's
        // `interface Gi1/0/12` block should match the target's Gi1/0/12
        // block, not Gi1/0/1.
        if trimmed.starts_with("interface ") {
            current_iface_ctx = Some(trimmed.trim().to_string());
        } else if trimmed.trim() == "exit" || trimmed.trim() == "!" {
            current_iface_ctx = None;
        }

        // For removes, the line we're negating ("switchport mode trunk")
        // is in current, not target — no provenance to find.
        let search_text = if is_remove {
            trimmed[3..].trim().to_string()
        } else {
            trimmed.trim().to_string()
        };

        let (label, class, detail) = if is_remove {
            (
                "currently on device, not in target".to_string(),
                "src-removed-from-current",
                String::new(),
            )
        } else {
            // Look up by trimmed text, then disambiguate by interface ctx.
            let candidate_indices = by_text.get(&search_text);
            match candidate_indices {
                None => {
                    ("(unresolved)".to_string(), "src-unknown", String::new())
                }
                Some(indices) => {
                    // Filter by context: if we're inside an interface block,
                    // prefer matches whose preceding `interface X` header
                    // matches our current_iface_ctx.
                    let mut chosen: Option<usize> = None;
                    if let Some(ctx) = &current_iface_ctx {
                        for &idx in indices {
                            if line_is_in_interface_block(&target_provs, idx, ctx) {
                                chosen = Some(idx);
                                break;
                            }
                        }
                    }
                    if chosen.is_none() && indices.len() == 1 {
                        chosen = Some(indices[0]);
                    }
                    match chosen {
                        Some(idx) => {
                            let (lbl, cls, det) = label_for(&target_provs[idx].source);
                            if let ProvSource::PortService { service, .. } =
                                &target_provs[idx].source
                            {
                                affected_services_set.insert(service.clone());
                            }
                            (lbl, cls, det)
                        }
                        None => {
                            // Ambiguous — show count.
                            let count = indices.len();
                            (
                                format!("{count} candidates (ambiguous)"),
                                "src-unknown",
                                String::new(),
                            )
                        }
                    }
                }
            }
        };

        if class == "src-unknown" || class == "src-removed-from-current" {
            unresolved += 1;
        } else {
            resolved += 1;
        }

        lines.push(ReconcileLineCtx {
            text: line,
            indent,
            direction,
            source_label: label,
            source_class: class,
            detail,
        });
    }

    let device_name = lookup_logical_device_name(&cfggen_base, &name);
    let hostname = lookup_hostname(&state, &cfggen_base, &name).await;

    let ctx = ReconcileCtx {
        name: name.clone(),
        device_name,
        hostname,
        has_delta: !lines.is_empty(),
        affected_services: affected_services_set.len(),
        resolved_count: resolved,
        unresolved_count: unresolved,
        total_count: lines.len(),
        lines,
    };

    let title = format!("Reconcile: {name}");
    let html = state
        .templates
        .render_page(&state.templates.reconcile, &title, "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Normalize the compiled config (drop blanks / trailing `end`) and filter
/// the provenance vector in lockstep so they stay aligned line-for-line.
fn normalize_with_provs(text: &str, provs: &[LineProv]) -> (String, Vec<LineProv>) {
    let raw_lines: Vec<&str> = text.lines().collect();
    if raw_lines.len() != provs.len() {
        // Shouldn't happen — compile_device_traced guarantees alignment.
        // Fall back to a coarse normalize without provs.
        let normalized = aycfgapply::normalize::normalize_target_config(text);
        let blank_provs: Vec<LineProv> = normalized
            .lines()
            .map(|l| LineProv {
                text: l.to_string(),
                source: ProvSource::Unknown {
                    hint: "input misalignment".to_string(),
                },
            })
            .collect();
        return (normalized, blank_provs);
    }

    // Find last non-blank, non-`end` line (mirrors normalize_body).
    let mut end_idx = raw_lines.len();
    let mut saw_end = false;
    for i in (0..raw_lines.len()).rev() {
        let trimmed = raw_lines[i].trim_end();
        if trimmed.is_empty() {
            end_idx = i;
            continue;
        }
        if !saw_end && trimmed == "end" {
            saw_end = true;
            end_idx = i;
            continue;
        }
        end_idx = i + 1;
        break;
    }

    let mut out_text = String::new();
    let mut out_provs: Vec<LineProv> = Vec::new();
    for i in 0..end_idx {
        let stripped = raw_lines[i].trim_end();
        if stripped.is_empty() {
            continue;
        }
        out_text.push_str(stripped);
        out_text.push('\n');
        let mut p = provs[i].clone();
        p.text = stripped.to_string();
        out_provs.push(p);
    }
    // normalize_target_config joins with \n (no trailing newline).
    if out_text.ends_with('\n') {
        out_text.pop();
    }
    (out_text, out_provs)
}

/// Walk backwards from `idx` looking for the nearest `interface X` line; if
/// it matches `wanted_iface_line`, return true.
fn line_is_in_interface_block(provs: &[LineProv], idx: usize, wanted_iface_line: &str) -> bool {
    for i in (0..=idx).rev() {
        let trimmed = provs[i].text.trim();
        if trimmed.starts_with("interface ") {
            return trimmed == wanted_iface_line;
        }
        // A non-indented line that isn't `interface ...` ends the
        // sub-mode, so we stop searching.
        if !provs[i].text.starts_with(' ') && !provs[i].text.starts_with('\t') {
            return false;
        }
    }
    false
}

/// Map a `ProvSource` to a (display_label, css_class, secondary_detail)
/// tuple. Kept centralized so the template can stay dumb.
fn label_for(src: &ProvSource) -> (String, &'static str, String) {
    match src {
        ProvSource::Template { path, line } => {
            (format!("template {path}:{line}"), "src-template", path.clone())
        }
        ProvSource::TemplateVarExpanded { path, line } => (
            format!("template {path}:{line} (var-expanded)"),
            "src-template",
            path.clone(),
        ),
        ProvSource::ConfigElement {
            element,
            line,
            marker_template_path,
            marker_template_line,
        } => (
            format!(
                "config-element {element}:{line} (from {marker_template_path}:{marker_template_line})"
            ),
            "src-element",
            element.clone(),
        ),
        ProvSource::ConfigElementMarker { element, .. } => (
            format!("config-element marker: {element}"),
            "src-element",
            element.clone(),
        ),
        ProvSource::PortInterfaceHeader {
            module_idx,
            port_name,
            derived_interface,
        } => (
            format!("port header [{module_idx}/{port_name}] → {derived_interface}"),
            "src-service-port",
            port_name.clone(),
        ),
        ProvSource::PortPrologue {
            module_idx,
            port_name,
            prologue_line,
        } => (
            format!("port prologue [{module_idx}/{port_name}]:{prologue_line}"),
            "src-prologue",
            port_name.clone(),
        ),
        ProvSource::PortService {
            module_idx,
            port_name,
            service,
            service_line,
        } => (
            format!(
                "service {service} [{module_idx}/{port_name}]:port-config.txt:{service_line}"
            ),
            "src-service-port",
            service.clone(),
        ),
        ProvSource::PortEpilogue {
            module_idx,
            port_name,
            epilogue_line,
        } => (
            format!("port epilogue [{module_idx}/{port_name}]:{epilogue_line}"),
            "src-epilogue",
            port_name.clone(),
        ),
        ProvSource::SviService {
            service,
            service_line,
        } => (
            format!("svi service {service}:svi-config.txt:{service_line}"),
            "src-svi",
            service.clone(),
        ),
        ProvSource::Structural { kind } => (
            format!("structural: {kind}"),
            "src-structural",
            kind.clone(),
        ),
        ProvSource::Unknown { hint } => (format!("unknown: {hint}"), "src-unknown", hint.clone()),
    }
}

fn lookup_logical_device_name(cfggen_base: &std::path::Path, serial: &str) -> String {
    use crate::routes::devices::{load_all_device_configs, serial_to_device_names};
    let configs = load_all_device_configs(cfggen_base);
    let map = serial_to_device_names(&configs);
    map.get(serial).map(|n| n.join(", ")).unwrap_or_else(|| "-".to_string())
}

async fn lookup_hostname(state: &AppState, cfggen_base: &std::path::Path, name: &str) -> String {
    use crate::routes::devices::load_all_device_configs;
    let seen = state.seen_assets.read().await;
    if let Some(d) = seen.get(name) {
        if let Some(h) = d.hostname.clone().filter(|s| !s.is_empty()) {
            return h;
        }
    }
    drop(seen);
    let configs = load_all_device_configs(cfggen_base);
    for cfg in configs.values() {
        let h = cfg
            .get("hostname")
            .and_then(|v| v.as_str())
            .or_else(|| {
                cfg.get("vars")
                    .and_then(|v| v.get("hostname"))
                    .and_then(|v| v.as_str())
            });
        if let Some(h) = h {
            if !h.is_empty() {
                return h.to_string();
            }
        }
    }
    "-".to_string()
}

fn message(
    state: &AppState,
    title: &str,
    body: &str,
    back: Option<(&str, &str)>,
) -> Response {
    let html = state
        .templates
        .render_message(title, Some(body), None, back)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    (StatusCode::OK, Html(html)).into_response()
}

// ── Routes ───────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new().route("/diff/{name}/reconcile", get(reconcile_detail))
}

//! Per-device config reconcile workflow.
//!
//! `GET /diff/{name}/reconcile` runs `aycfggen::compile_device_traced` to
//! get both the compiled target config AND a per-line provenance map. It
//! then computes the same delta as the regular diff page and joins each
//! delta line to its target-side source — which service, which template
//! line, which prologue/epilogue — so the operator can see at a glance
//! which template change would absorb each hunk.
//!
//! Drives the full reconcile cycle:
//!
//! - **Group per port.** Delta lines whose target source is a `PortService`
//!   are grouped under that port. Each group shows the port's current
//!   service plus a dropdown of available services. Picking one re-renders
//!   the page under that hypothetical override (`?try_module=1&try_port=
//!   Port36&try_service=access-vlan2`) so the operator can see the
//!   projected delta before committing.
//!
//! - **Commit a swap.** `POST /diff/{name}/reconcile/swap` writes the new
//!   service to the device JSON on disk, recompiles the target config,
//!   and redirects back to the reconcile page.
//!
//! - **Absorb drift.** Delta lines that came from device drift (`no <cmd>`)
//!   carry an "Add to <port's service>" button. `POST .../absorb` appends
//!   the bare command to the service's `port-config.txt` so future compiles
//!   include it and the drift disappears.
//!
//! - **Drop a target-side line.** Delta lines coming from a port's service
//!   are also affordances to *remove* the line from the service (write
//!   action coming in a follow-up; the read-only side is in place).
//!
//! Delta-line ↔ target-line matching is by trimmed text first, then
//! disambiguated by the active context (current `interface X` block, etc).

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use aycfggen::model::LogicalDeviceConfig;
use aycfggen::provenance::{LineProv, ProvSource};
use aycfggen::sources::LogicalDeviceSource;
use aycicdiff::diff::diff_model::{DiffAction, DiffTree};
use aycicdiff::model::config_tree::ConfigNode;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Events emitted by walking a `DiffTree`. The reconcile renderer consumes
/// them in order, maintaining `iface_ctx` from Enter/Exit so per-port
/// grouping has authoritative context (no parsing of textual delta).
#[derive(Debug)]
enum WalkEvent {
    /// Entered a section (e.g. `interface Gi1/0/12`, `router bgp 65001`).
    /// We push the header onto the context stack until the matching Exit.
    SectionEnter(String),
    SectionExit,
    /// A target-side line that needs to be applied to the device.
    Add(String),
    /// A device-side line that the target wants removed — the actual command
    /// running on the device. NOT the `no X` form aycicdiff would emit.
    Remove(String),
}

fn walk_actions(actions: &[DiffAction], out: &mut Vec<WalkEvent>) {
    for action in actions {
        match action {
            DiffAction::Add(node) => walk_add_node(node, out),
            DiffAction::Remove(node) => walk_remove_node(node, out),
            DiffAction::ModifySection {
                header,
                child_actions,
                ..
            } => {
                out.push(WalkEvent::SectionEnter(header.clone()));
                walk_actions(child_actions, out);
                out.push(WalkEvent::SectionExit);
            }
            DiffAction::ReplaceOrdered {
                header,
                remove_children,
                add_children,
            } => {
                out.push(WalkEvent::SectionEnter(header.clone()));
                for r in remove_children {
                    out.push(WalkEvent::Remove(r.text.clone()));
                }
                for a in add_children {
                    out.push(WalkEvent::Add(a.text.clone()));
                }
                out.push(WalkEvent::SectionExit);
            }
        }
    }
}

fn walk_add_node(node: &ConfigNode, out: &mut Vec<WalkEvent>) {
    match node {
        ConfigNode::Leaf(l) => out.push(WalkEvent::Add(l.text.clone())),
        ConfigNode::Section(s) => {
            // Whole section added: emit the header as an Add too, then
            // every child verbatim.
            out.push(WalkEvent::Add(s.header.clone()));
            out.push(WalkEvent::SectionEnter(s.header.clone()));
            for child in &s.children {
                walk_add_node(child, out);
            }
            out.push(WalkEvent::SectionExit);
        }
    }
}

fn walk_remove_node(node: &ConfigNode, out: &mut Vec<WalkEvent>) {
    match node {
        ConfigNode::Leaf(l) => out.push(WalkEvent::Remove(l.text.clone())),
        ConfigNode::Section(s) => {
            // Whole section is on the device but not in target. The header
            // itself is drift (e.g. `interface Vlan999`), and the children
            // sit underneath that context.
            out.push(WalkEvent::Remove(s.header.clone()));
            out.push(WalkEvent::SectionEnter(s.header.clone()));
            for child in &s.children {
                walk_remove_node(child, out);
            }
            out.push(WalkEvent::SectionExit);
        }
    }
}

use crate::routes::devices::{load_all_device_configs, serial_to_device_names};
use crate::state::AppState;

/// `/diff` is keyed by device serial (matches the `.cfg` filename in
/// `target_configs/`), but `logical-devices/` is keyed by the logical
/// device name. Resolve the URL's `{name}` against both: if it's a real
/// logical-device name use it; otherwise treat it as a serial and look
/// up the logical device name(s) referencing that serial.
///
/// Returns the logical-device name to load configs by, or `None` if the
/// input doesn't resolve.
fn resolve_to_device_name(cfggen_base: &std::path::Path, raw: &str) -> Option<String> {
    let logical_dir = cfggen_base.join("logical-devices");
    if logical_dir.join(format!("{}.json", raw)).exists()
        || logical_dir.join(raw).join("config.json").exists()
    {
        return Some(raw.to_string());
    }
    let configs = load_all_device_configs(cfggen_base);
    let map = serial_to_device_names(&configs);
    map.get(raw).and_then(|names| names.first().cloned())
}

// ── View models ──────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct ReconcileLineCtx {
    text: String,
    indent: usize,
    direction: &'static str,
    source_label: String,
    source_class: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct ServiceOptionCtx {
    name: String,
    selected: bool,
}

#[derive(Serialize)]
struct PortGroupCtx {
    module_idx: usize,
    port_name: String,
    derived_interface: String,
    current_service: String,
    service_options: Vec<ServiceOptionCtx>,
    lines: Vec<ReconcileLineCtx>,
    line_count: usize,
    /// Set when the page was rendered with this port's service overridden
    /// (?try_* params). The template uses it to render a "Commit swap"
    /// form pointing at /swap.
    is_previewing: bool,
    previewing_service: String,
}

#[derive(Serialize)]
struct SviGroupCtx {
    service: String,
    lines: Vec<ReconcileLineCtx>,
    line_count: usize,
}

#[derive(Serialize, Clone)]
struct DriftLineCtx {
    /// Full delta line, with the leading "no " preserved.
    full_text: String,
    /// The bare command after stripping "no " — what would be appended
    /// to a service's port-config.txt to absorb this drift.
    bare_cmd: String,
    /// Interface context the drift sits inside (used to infer the port).
    interface_ctx: String,
    /// The port we inferred for this drift, if any.
    port_name: String,
    module_idx: usize,
    /// Current service for the inferred port, if known. Empty if we
    /// couldn't infer which port the drift line belongs to (e.g. drift
    /// outside any interface block).
    current_service: String,
    has_port: bool,
}

#[derive(Serialize)]
struct ReconcileCtx {
    name: String,
    device_name: String,
    hostname: String,
    has_delta: bool,
    affected_services: usize,
    resolved_count: usize,
    unresolved_count: usize,
    total_count: usize,
    /// Banner shown when ?try_* params were applied.
    is_preview: bool,
    preview_banner: String,
    /// Form values to forward into POST /swap if the operator commits.
    preview_module_idx: usize,
    preview_port_name: String,
    preview_service: String,
    port_groups: Vec<PortGroupCtx>,
    has_port_groups: bool,
    svi_groups: Vec<SviGroupCtx>,
    has_svi_groups: bool,
    drift_lines: Vec<DriftLineCtx>,
    has_drift_lines: bool,
    other_lines: Vec<ReconcileLineCtx>,
    has_other_lines: bool,
}

// ── Query / form ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TryParams {
    pub try_module: Option<usize>,
    pub try_port: Option<String>,
    pub try_service: Option<String>,
}

#[derive(Deserialize)]
pub struct SwapForm {
    pub module_idx: usize,
    pub port_name: String,
    pub service: String,
}

#[derive(Deserialize)]
pub struct AbsorbForm {
    pub module_idx: usize,
    pub port_name: String,
    pub service: String,
    pub command: String,
}

// ── Override LogicalDeviceSource ──────────────────────────────────────────────

/// Wraps another `LogicalDeviceSource`, returning a mutated config for one
/// specific device. Used to preview "what if port X were on service Y?"
/// without touching disk.
struct OverrideLogicalDeviceSource<'a> {
    inner: &'a dyn LogicalDeviceSource,
    device: String,
    override_config: LogicalDeviceConfig,
}

impl LogicalDeviceSource for OverrideLogicalDeviceSource<'_> {
    fn load_device_config(&self, name: &str) -> anyhow::Result<LogicalDeviceConfig> {
        if name == self.device {
            Ok(self.override_config.clone())
        } else {
            self.inner.load_device_config(name)
        }
    }
    fn list_devices(&self) -> anyhow::Result<Vec<String>> {
        self.inner.list_devices()
    }
}

// ── Handler: GET reconcile page ───────────────────────────────────────────────

pub async fn reconcile_detail(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(try_params): Query<TryParams>,
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

    // The URL's {name} can be either a logical-device name (e.g. AD6-X001)
    // or a device serial (e.g. FCW2228L054 — that's what /diff lists). Map
    // it to the logical device name we'll feed the compile pipeline.
    let device_name_for_compile = match resolve_to_device_name(&cfggen_base, &name) {
        Some(n) => n,
        None => {
            let msg = format!(
                "Could not map '{name}' to a logical device — no logical-device JSON \
                 by that name and no device JSON references it as a module serial."
            );
            return message(&state, "Unknown device", &msg, Some(("/diff", "Back")));
        }
    };

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

    // Load the device config so we can inspect current per-port services and
    // (optionally) build the override for preview.
    let real_config = match device_source.load_device_config(&device_name_for_compile) {
        Ok(c) => c,
        Err(err) => {
            warn!(name = %name, error = %err, "load_device_config failed");
            let msg = format!("Could not load device config: {err}");
            return message(&state, "Load failed", &msg, Some(("/diff", "Back")));
        }
    };

    let available_services = load_available_services(&cfggen_base);

    // Build the override config if try_* params are set and validate the swap.
    let (is_preview, preview_module_idx, preview_port_name, preview_service, override_config) =
        match (&try_params.try_module, &try_params.try_port, &try_params.try_service) {
            (Some(m), Some(p), Some(s)) => {
                let mut cfg = real_config.clone();
                let mut applied = false;
                if let Some(Some(module)) = cfg.modules.get_mut(*m) {
                    for port in &mut module.ports {
                        if port.name == *p {
                            port.service = s.clone();
                            applied = true;
                            break;
                        }
                    }
                }
                if applied {
                    (true, *m, p.clone(), s.clone(), Some(cfg))
                } else {
                    warn!(
                        device = %name, module_idx = m, port = %p,
                        "try_* params point at a port that doesn't exist; rendering normally"
                    );
                    (false, 0, String::new(), String::new(), None)
                }
            }
            _ => (false, 0, String::new(), String::new(), None),
        };

    // Compile (with or without override).
    let traced_result = if let Some(override_cfg) = override_config.as_ref() {
        let override_source = OverrideLogicalDeviceSource {
            inner: &device_source,
            device: device_name_for_compile.clone(),
            override_config: override_cfg.clone(),
        };
        compile_device_traced(
            &device_name_for_compile,
            &override_source,
            &hw_source,
            &service_source,
            &template_source,
            &element_source,
            &image_source,
        )
    } else {
        compile_device_traced(
            &device_name_for_compile,
            &device_source,
            &hw_source,
            &service_source,
            &template_source,
            &element_source,
            &image_source,
        )
    };

    let (target_text_raw, target_provs_raw) = match traced_result {
        Ok(pair) => pair,
        Err(err) => {
            warn!(name = %name, error = %err, "compile_device_traced failed");
            let msg = format!("Could not compile target config: {err}");
            return message(&state, "Compile failed", &msg, Some(("/diff", "Back")));
        }
    };

    let (target_text, target_provs) = normalize_with_provs(&target_text_raw, &target_provs_raw);

    let current_file = current_path.join(format!("{}.cfg", name));
    let current_text = match std::fs::read_to_string(&current_file) {
        Ok(c) => aycfgapply::normalize::normalize_config(&c),
        Err(_) => String::new(),
    };

    // Use aycicdiff's structured diff so we get authoritative Add/Remove
    // actions instead of having to parse "no X" prefixes out of a textual
    // delta. Remove(node) carries the literal device-side text — that's
    // what we display in the drift section and absorb into services.
    let rules = aycicdiff::rules::RulesConfig::builtin();
    let current_tree = aycicdiff::parser::parse_config(&current_text, &rules);
    let target_tree = aycicdiff::parser::parse_config(&target_text, &rules);
    let diff_tree: DiffTree = if target_text == current_text {
        DiffTree::new()
    } else {
        aycicdiff::diff::diff_configs(&current_tree, &target_tree, &rules)
    };
    let mut events: Vec<WalkEvent> = Vec::new();
    walk_actions(&diff_tree.actions, &mut events);

    // Index target lines by trimmed text → list of (target_line_idx, &prov).
    let mut by_text: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in target_provs.iter().enumerate() {
        by_text.entry(p.text.trim().to_string()).or_default().push(i);
    }

    // Walk the structured events, building (line + chosen prov + interface ctx).
    let mut walked: Vec<(ReconcileLineCtx, Option<usize>, String)> = Vec::new();
    let mut ctx_stack: Vec<String> = Vec::new();
    let mut resolved = 0usize;
    let mut unresolved = 0usize;
    let mut affected_services_set = std::collections::HashSet::new();

    for ev in &events {
        match ev {
            WalkEvent::SectionEnter(header) => {
                ctx_stack.push(header.trim().to_string());
                continue;
            }
            WalkEvent::SectionExit => {
                ctx_stack.pop();
                continue;
            }
            WalkEvent::Add(text) | WalkEvent::Remove(text) => {
                let is_remove = matches!(ev, WalkEvent::Remove(_));
                let direction = if is_remove { "remove" } else { "add" };
                let iface_ctx = ctx_stack
                    .iter()
                    .rev()
                    .find(|h| h.starts_with("interface "))
                    .cloned()
                    .unwrap_or_default();
                // For the display, indent by ctx depth so the row reads
                // naturally inside a section.
                let indent = ctx_stack.len();
                let display_text = if indent > 0 {
                    format!("{}{}", " ".repeat(indent), text.trim())
                } else {
                    text.clone()
                };

                let mut chosen_idx: Option<usize> = None;
                let (label, class, detail) = if is_remove {
                    (
                        "currently on device, not in target".to_string(),
                        "src-removed-from-current",
                        String::new(),
                    )
                } else {
                    let search_text = text.trim().to_string();
                    match by_text.get(&search_text) {
                        None => ("(unresolved)".to_string(), "src-unknown", String::new()),
                        Some(indices) => {
                            let mut chosen: Option<usize> = None;
                            if !iface_ctx.is_empty() {
                                for &idx in indices {
                                    if line_is_in_interface_block(&target_provs, idx, &iface_ctx) {
                                        chosen = Some(idx);
                                        break;
                                    }
                                }
                            }
                            if chosen.is_none() && indices.len() == 1 {
                                chosen = Some(indices[0]);
                            }
                            chosen_idx = chosen;
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
                                None => (
                                    format!("{} candidates (ambiguous)", indices.len()),
                                    "src-unknown",
                                    String::new(),
                                ),
                            }
                        }
                    }
                };

                if class == "src-unknown" || class == "src-removed-from-current" {
                    unresolved += 1;
                } else {
                    resolved += 1;
                }

                walked.push((
                    ReconcileLineCtx {
                        text: display_text,
                        indent,
                        direction,
                        source_label: label,
                        source_class: class,
                        detail,
                    },
                    chosen_idx,
                    iface_ctx,
                ));
            }
        }
    }

    // Group by attribution.
    let mut port_groups_map: HashMap<(usize, String), PortGroupCtx> = HashMap::new();
    let mut svi_groups_map: HashMap<String, SviGroupCtx> = HashMap::new();
    let mut drift_lines: Vec<DriftLineCtx> = Vec::new();
    let mut other_lines: Vec<ReconcileLineCtx> = Vec::new();

    // For drift inference: map iface text ("interface Gi1/0/12") →
    // (module_idx, port_name, service). Built from target_provs by finding
    // PortInterfaceHeader entries.
    let mut iface_to_port: HashMap<String, (usize, String, String)> = HashMap::new();
    for prov in &target_provs {
        if let ProvSource::PortInterfaceHeader {
            module_idx,
            port_name,
            derived_interface,
        } = &prov.source
        {
            let iface_text = format!("interface {}", derived_interface);
            let svc = real_config
                .modules
                .get(*module_idx)
                .and_then(|m| m.as_ref())
                .and_then(|m| m.ports.iter().find(|p| &p.name == port_name))
                .map(|p| p.service.clone())
                .unwrap_or_default();
            iface_to_port.insert(iface_text, (*module_idx, port_name.clone(), svc));
        }
    }

    for (line_ctx, chosen_idx, iface_ctx) in walked {
        // Drift lines first. line_ctx.text is the literal device-side
        // command (already extracted by walk_remove_node from the
        // ConfigTree's Remove(node) → leaf.text), so no string trickery
        // here — the bare command IS what the device has.
        if line_ctx.direction == "remove" {
            let bare_cmd = line_ctx.text.trim().to_string();
            // Synthesize the form aycicdiff would emit in the textual
            // delta, for the secondary column. Goes through the rules so
            // override-registry negations (e.g. "shutdown" → "no shutdown")
            // are accurate.
            let negation_emitted =
                aycicdiff::serialize::negation::negate_command_with_rules(&bare_cmd, &rules);
            let (module_idx, port_name, current_service) = iface_to_port
                .get(&iface_ctx)
                .cloned()
                .unwrap_or((0, String::new(), String::new()));
            let has_port = !port_name.is_empty();
            drift_lines.push(DriftLineCtx {
                full_text: negation_emitted,
                bare_cmd,
                interface_ctx: iface_ctx.clone(),
                port_name,
                module_idx,
                current_service,
                has_port,
            });
            continue;
        }

        // Now route resolved adds into per-port or per-SVI groups.
        if let Some(idx) = chosen_idx {
            match &target_provs[idx].source {
                ProvSource::PortService {
                    module_idx,
                    port_name,
                    ..
                }
                | ProvSource::PortInterfaceHeader {
                    module_idx,
                    port_name,
                    ..
                }
                | ProvSource::PortPrologue {
                    module_idx,
                    port_name,
                    ..
                }
                | ProvSource::PortEpilogue {
                    module_idx,
                    port_name,
                    ..
                } => {
                    let key = (*module_idx, port_name.clone());
                    let group = port_groups_map.entry(key.clone()).or_insert_with(|| {
                        let current_service = real_config
                            .modules
                            .get(*module_idx)
                            .and_then(|m| m.as_ref())
                            .and_then(|m| m.ports.iter().find(|p| &p.name == port_name))
                            .map(|p| p.service.clone())
                            .unwrap_or_default();
                        let derived_interface = target_provs
                            .iter()
                            .find_map(|prov| match &prov.source {
                                ProvSource::PortInterfaceHeader {
                                    module_idx: m,
                                    port_name: pn,
                                    derived_interface,
                                } if *m == key.0 && pn == &key.1 => Some(derived_interface.clone()),
                                _ => None,
                            })
                            .unwrap_or_else(|| "?".to_string());
                        let is_previewing = is_preview
                            && preview_module_idx == *module_idx
                            && preview_port_name == *port_name;
                        // During preview, the dropdown should reflect the
                        // service the operator just picked — not the saved
                        // value, which still lives in the header
                        // ("current service: <saved>") and in the banner.
                        let selected_service = if is_previewing {
                            preview_service.clone()
                        } else {
                            current_service.clone()
                        };
                        let service_options: Vec<ServiceOptionCtx> = available_services
                            .iter()
                            .map(|svc| ServiceOptionCtx {
                                name: svc.clone(),
                                selected: svc == &selected_service,
                            })
                            .collect();
                        PortGroupCtx {
                            module_idx: *module_idx,
                            port_name: port_name.clone(),
                            derived_interface,
                            current_service,
                            service_options,
                            lines: Vec::new(),
                            line_count: 0,
                            is_previewing,
                            previewing_service: if is_previewing {
                                preview_service.clone()
                            } else {
                                String::new()
                            },
                        }
                    });
                    group.lines.push(line_ctx);
                    continue;
                }
                ProvSource::SviService { service, .. } => {
                    let group = svi_groups_map
                        .entry(service.clone())
                        .or_insert_with(|| SviGroupCtx {
                            service: service.clone(),
                            lines: Vec::new(),
                            line_count: 0,
                        });
                    group.lines.push(line_ctx);
                    continue;
                }
                _ => {
                    other_lines.push(line_ctx);
                    continue;
                }
            }
        }
        other_lines.push(line_ctx);
    }

    // Finalize port groups: compute line_count and sort.
    let mut port_groups: Vec<PortGroupCtx> = port_groups_map
        .into_values()
        .map(|mut g| {
            g.line_count = g.lines.len();
            g
        })
        .collect();
    port_groups.sort_by(|a, b| {
        a.module_idx
            .cmp(&b.module_idx)
            .then_with(|| crate::routes::reconcile::natural_compare_port(&a.port_name, &b.port_name))
    });

    let mut svi_groups: Vec<SviGroupCtx> = svi_groups_map
        .into_values()
        .map(|mut g| {
            g.line_count = g.lines.len();
            g
        })
        .collect();
    svi_groups.sort_by(|a, b| a.service.cmp(&b.service));

    let device_name = lookup_logical_device_name(&cfggen_base, &name);
    let hostname = lookup_hostname(&state, &cfggen_base, &name).await;

    let preview_banner = if is_preview {
        format!(
            "Previewing: port {} on service {} (not yet committed)",
            preview_port_name, preview_service
        )
    } else {
        String::new()
    };

    let ctx = ReconcileCtx {
        name: name.clone(),
        device_name,
        hostname,
        has_delta: !port_groups.is_empty()
            || !svi_groups.is_empty()
            || !drift_lines.is_empty()
            || !other_lines.is_empty(),
        affected_services: affected_services_set.len(),
        resolved_count: resolved,
        unresolved_count: unresolved,
        total_count: resolved + unresolved,
        is_preview,
        preview_banner,
        preview_module_idx,
        preview_port_name,
        preview_service,
        has_port_groups: !port_groups.is_empty(),
        port_groups,
        has_svi_groups: !svi_groups.is_empty(),
        svi_groups,
        has_drift_lines: !drift_lines.is_empty(),
        drift_lines,
        has_other_lines: !other_lines.is_empty(),
        other_lines,
    };

    let title = format!("Reconcile: {name}");
    let html = state
        .templates
        .render_page(&state.templates.reconcile, &title, "", &ctx)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

// ── Handler: POST commit a port service swap ─────────────────────────────────

pub async fn swap_service(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<SwapForm>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };

    let device_name = match resolve_to_device_name(&cfggen_base, &name) {
        Some(n) => n,
        None => {
            return message(
                &state,
                "Unknown device",
                &format!("Could not map '{name}' to a logical device"),
                Some(("/diff", "Back")),
            )
        }
    };

    // Locate the device JSON (flat or directory layout).
    let flat = cfggen_base.join("logical-devices").join(format!("{}.json", device_name));
    let dir = cfggen_base.join("logical-devices").join(&device_name).join("config.json");
    let json_path = if flat.exists() {
        flat
    } else if dir.exists() {
        dir
    } else {
        return message(
            &state,
            "Device not found",
            &format!("No logical-device JSON for '{device_name}'"),
            Some(("/diff", "Back")),
        );
    };

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path = %json_path.display(), error = %e, "Failed to read device JSON");
            return message(&state, "I/O error", &format!("{e}"), Some(("/diff", "Back")));
        }
    };
    let mut raw_json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!(path = %json_path.display(), error = %e, "Invalid JSON");
            return message(&state, "JSON parse error", &format!("{e}"), Some(("/diff", "Back")));
        }
    };

    // Mutate modules[module_idx].ports[?].service.
    let mut applied = false;
    if let Some(modules) = raw_json.get_mut("modules").and_then(|v| v.as_array_mut()) {
        if let Some(module) = modules.get_mut(form.module_idx) {
            if let Some(ports) = module.get_mut("ports").and_then(|p| p.as_array_mut()) {
                for port in ports {
                    if port.get("name").and_then(|v| v.as_str()) == Some(form.port_name.as_str()) {
                        if let Some(obj) = port.as_object_mut() {
                            obj.insert(
                                "service".to_string(),
                                serde_json::Value::String(form.service.clone()),
                            );
                            applied = true;
                            break;
                        }
                    }
                }
            }
        }
    }
    if !applied {
        return message(
            &state,
            "Swap failed",
            &format!(
                "Could not locate module[{}].ports[name={}] in {}",
                form.module_idx,
                form.port_name,
                json_path.display()
            ),
            Some(("/diff", "Back")),
        );
    }

    let new_content = match serde_json::to_string_pretty(&raw_json) {
        Ok(s) => s,
        Err(e) => return message(&state, "Serialize error", &format!("{e}"), None),
    };
    if let Err(e) = std::fs::write(&json_path, new_content) {
        warn!(path = %json_path.display(), error = %e, "Failed to write device JSON");
        return message(&state, "Write failed", &format!("{e}"), None);
    }
    info!(
        device = %device_name, port = %form.port_name, service = %form.service,
        "Swapped port service via reconcile"
    );

    // Recompile target. compile_device_config writes to both preview and
    // target_configs_path so the /diff page picks it up.
    if let Err(e) = crate::routes::devices::compile_device_config(&device_name, &cfggen_base, &state.config) {
        warn!(error = %e, "Recompile after swap failed");
    }

    Redirect::to(&format!("/diff/{}/reconcile", name)).into_response()
}

// ── Handler: POST absorb drift into a service ────────────────────────────────

pub async fn absorb_drift(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<AbsorbForm>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };

    let device_name = match resolve_to_device_name(&cfggen_base, &name) {
        Some(n) => n,
        None => {
            return message(
                &state,
                "Unknown device",
                &format!("Could not map '{name}' to a logical device"),
                Some(("/diff", "Back")),
            )
        }
    };

    let service_dir = cfggen_base.join("services").join(&form.service);
    if !service_dir.exists() {
        return message(
            &state,
            "Unknown service",
            &format!("Service directory not found: {}", service_dir.display()),
            Some(("/diff", "Back")),
        );
    }
    let port_config = service_dir.join("port-config.txt");
    let mut content = std::fs::read_to_string(&port_config).unwrap_or_default();
    if !content.ends_with('\n') {
        content.push('\n');
    }
    // Cisco config sub-mode lines are typically prefixed with a single space
    // — match that convention so the absorbed line slots into the port block
    // alongside the existing entries.
    let cmd = form.command.trim();
    let absorbed = if cmd.starts_with(' ') {
        cmd.to_string()
    } else {
        format!(" {cmd}")
    };
    content.push_str(&absorbed);
    content.push('\n');

    if let Err(e) = std::fs::write(&port_config, content) {
        warn!(path = %port_config.display(), error = %e, "Failed to write port-config");
        return message(&state, "Write failed", &format!("{e}"), None);
    }
    info!(
        device = %device_name, service = %form.service, cmd = %cmd,
        "Absorbed drift command into service"
    );

    if let Err(e) = crate::routes::devices::compile_device_config(&device_name, &cfggen_base, &state.config) {
        warn!(error = %e, "Recompile after absorb failed");
    }

    Redirect::to(&format!("/diff/{}/reconcile", name)).into_response()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn load_available_services(cfggen_base: &std::path::Path) -> Vec<String> {
    let services_dir = cfggen_base.join("services");
    let mut services = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&services_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.path().file_name().and_then(|n| n.to_str()) {
                services.push(name.to_string());
            }
        }
    }
    services.sort();
    services
}

fn normalize_with_provs(text: &str, provs: &[LineProv]) -> (String, Vec<LineProv>) {
    let raw_lines: Vec<&str> = text.lines().collect();
    if raw_lines.len() != provs.len() {
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
    if out_text.ends_with('\n') {
        out_text.pop();
    }
    (out_text, out_provs)
}

fn line_is_in_interface_block(provs: &[LineProv], idx: usize, wanted_iface_line: &str) -> bool {
    for i in (0..=idx).rev() {
        let trimmed = provs[i].text.trim();
        if trimmed.starts_with("interface ") {
            return trimmed == wanted_iface_line;
        }
        if !provs[i].text.starts_with(' ') && !provs[i].text.starts_with('\t') {
            return false;
        }
    }
    false
}

fn label_for(src: &ProvSource) -> (String, &'static str, String) {
    match src {
        ProvSource::Template { path, line } => (
            format!("template {path}:{line}"),
            "src-template",
            path.clone(),
        ),
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

fn message(state: &AppState, title: &str, body: &str, back: Option<(&str, &str)>) -> Response {
    let html = state
        .templates
        .render_message(title, Some(body), None, back)
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    (StatusCode::OK, Html(html)).into_response()
}

/// Compare port names like "Port5" vs "Port12" numerically when possible
/// so the grouping section orders ports naturally.
pub(crate) fn natural_compare_port(a: &str, b: &str) -> std::cmp::Ordering {
    fn split(s: &str) -> (String, u64) {
        let mut prefix = String::new();
        let mut num_str = String::new();
        for c in s.chars() {
            if c.is_ascii_digit() {
                num_str.push(c);
            } else if num_str.is_empty() {
                prefix.push(c);
            } else {
                num_str.push(c);
            }
        }
        let n = num_str.parse::<u64>().unwrap_or(u64::MAX);
        (prefix, n)
    }
    let (ap, an) = split(a);
    let (bp, bn) = split(b);
    ap.cmp(&bp).then_with(|| an.cmp(&bn))
}

// ── Routes ───────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/diff/{name}/reconcile", get(reconcile_detail))
        .route("/diff/{name}/reconcile/swap", post(swap_service))
        .route("/diff/{name}/reconcile/absorb", post(absorb_drift))
}

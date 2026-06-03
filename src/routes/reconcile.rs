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
    /// Best-fit service for this port's *current device-side* body, looked
    /// up via the same matcher that aycfggen's import flow uses
    /// (match_port_body_to_existing_service). When this is non-empty AND
    /// differs from `current_service`, the port is mis-assigned: swapping
    /// to `suggested_service` would zero out the delta for this port
    /// without any disk writes happening during the lookup.
    suggested_service: String,
    has_suggestion: bool,
    /// If this port's JSON has a non-null `prologue`, the text and a flag
    /// surface "Fold prologue into <service>" UI. The fold is a global
    /// operation across every device that uses the same (service,
    /// prologue) pair — confirmation page enumerates the impact.
    has_prologue: bool,
    prologue_text: String,
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

#[derive(Deserialize)]
pub struct FoldQuery {
    pub service: String,
    pub prologue: String,
}

#[derive(Deserialize)]
pub struct FoldForm {
    pub service: String,
    pub prologue: String,
}

// ── Fold-preview view models ─────────────────────────────────────────────────

#[derive(Serialize)]
struct FoldDeviceCtx {
    device_name: String,
    port_count: usize,
    port_names: String,
}

#[derive(Serialize)]
struct FoldPreviewCtx {
    /// The URL's {name} we came from — used for the "Back to reconcile" link.
    name: String,
    /// Full URL the confirmation form should POST to. Device-centric
    /// calls go to /diff/{name}/reconcile/fold-prologue; service-centric
    /// calls go to /reconcile/services/{svc}/fold-prologue.
    form_action: String,
    /// Full URL the Cancel link should go to.
    cancel_url: String,
    service: String,
    prologue: String,
    /// Trimmed display version of the prologue (no leading space).
    prologue_display: String,
    service_path: String,
    devices: Vec<FoldDeviceCtx>,
    total_devices: usize,
    total_ports: usize,
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
                        let port_assign = real_config
                            .modules
                            .get(*module_idx)
                            .and_then(|m| m.as_ref())
                            .and_then(|m| m.ports.iter().find(|p| &p.name == port_name));
                        let current_service = port_assign
                            .map(|p| p.service.clone())
                            .unwrap_or_default();
                        let prologue_text = port_assign
                            .and_then(|p| p.prologue.clone())
                            .filter(|s| !s.is_empty())
                            .unwrap_or_default();
                        let has_prologue = !prologue_text.is_empty();
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
                            // Populated after grouping completes (we need
                            // the parsed current-config bodies for that).
                            suggested_service: String::new(),
                            has_suggestion: false,
                            has_prologue,
                            prologue_text,
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

    // Pre-load every service's port-config.txt once. Used both for the
    // dropdown options and for the import-style match below.
    let services_map = load_service_port_configs(&cfggen_base, &available_services);

    // For each port group, parse its body out of the *current* device
    // config and ask "which existing service does this body match?" — the
    // same matcher decompose_ports uses during import. If the matched
    // service differs from what's currently assigned, that's a strong
    // suggestion for the swap dropdown.
    let current_port_bodies = parse_interface_bodies(&current_tree);

    // Finalize port groups: compute line_count, run the matcher, sort.
    let mut port_groups: Vec<PortGroupCtx> = port_groups_map
        .into_values()
        .map(|mut g| {
            g.line_count = g.lines.len();
            let iface_text = format!("interface {}", g.derived_interface);
            if let Some(body) = current_port_bodies.get(&iface_text) {
                if let Some(matched) =
                    aycfggen::port_decomposition::match_port_body_to_existing_service(
                        body,
                        &services_map,
                    )
                {
                    g.has_suggestion = matched != g.current_service && !matched.is_empty();
                    g.suggested_service = matched;
                }
            }
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

// ── Handler: GET prologue fold confirmation page ─────────────────────────────

pub async fn fold_prologue_preview(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<FoldQuery>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };

    let matches = scan_devices_for_prologue_pair(&cfggen_base, &q.service, &q.prologue);
    let total_devices = matches.len();
    let total_ports: usize = matches.iter().map(|(_, ports)| ports.len()).sum();
    let devices: Vec<FoldDeviceCtx> = matches
        .into_iter()
        .map(|(device_name, ports)| FoldDeviceCtx {
            port_count: ports.len(),
            port_names: ports.join(", "),
            device_name,
        })
        .collect();

    let service_path = cfggen_base
        .join("services")
        .join(&q.service)
        .join("port-config.txt")
        .display()
        .to_string();

    let prologue_display = q.prologue.trim_start().to_string();
    let form_action = format!("/diff/{}/reconcile/fold-prologue", name);
    let cancel_url = format!("/diff/{}/reconcile", name);

    let ctx = FoldPreviewCtx {
        name: name.clone(),
        form_action,
        cancel_url,
        service: q.service,
        prologue: q.prologue,
        prologue_display,
        service_path,
        devices,
        total_devices,
        total_ports,
    };
    let html = state
        .templates
        .render_page(
            &state.templates.reconcile_fold_preview,
            "Fold prologue into service",
            "",
            &ctx,
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

// ── Handler: POST execute prologue fold ──────────────────────────────────────

pub async fn fold_prologue_execute(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<FoldForm>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };

    let port_config_path = cfggen_base
        .join("services")
        .join(&form.service)
        .join("port-config.txt");
    if !port_config_path.exists() {
        return message(
            &state,
            "Unknown service",
            &format!("Service has no port-config.txt: {}", port_config_path.display()),
            Some(("/diff", "Back")),
        );
    }

    // Step 1: prepend the prologue line to the service's port-config.txt.
    // We prepend (not append) because Cisco description conventionally
    // appears at the top of an interface block; the rest of the service
    // body follows.
    let existing = std::fs::read_to_string(&port_config_path).unwrap_or_default();
    let line_to_add = if form.prologue.starts_with(' ') {
        form.prologue.clone()
    } else {
        format!(" {}", form.prologue.trim_start())
    };
    let mut new_content = String::new();
    new_content.push_str(line_to_add.trim_end());
    new_content.push('\n');
    new_content.push_str(&existing);
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    if let Err(e) = std::fs::write(&port_config_path, &new_content) {
        warn!(path = %port_config_path.display(), error = %e, "Failed to write service port-config.txt");
        return message(&state, "Write failed", &format!("{e}"), None);
    }

    // Step 2: walk every device JSON; for each port that uses (service,
    // prologue) match, set prologue=null. Remember which devices we
    // touched so we can recompile them afterwards.
    let matches = scan_devices_for_prologue_pair(&cfggen_base, &form.service, &form.prologue);
    let touched_devices: Vec<String> = matches.iter().map(|(d, _)| d.clone()).collect();

    for (device_name, _) in &matches {
        clear_prologue_for_device(&cfggen_base, device_name, &form.service, &form.prologue);
    }

    info!(
        service = %form.service,
        devices = touched_devices.len(),
        "Folded prologue into service across devices"
    );

    // Step 3: recompile every touched device so target_configs/ is up to
    // date. compile_device_config swallows per-device errors so one bad
    // device doesn't block the rest.
    for device_name in &touched_devices {
        if let Err(e) = crate::routes::devices::compile_device_config(
            device_name,
            &cfggen_base,
            &state.config,
        ) {
            warn!(device = %device_name, error = %e, "Recompile after fold failed");
        }
    }

    Redirect::to(&format!("/diff/{}/reconcile", name)).into_response()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Walk every device JSON under `logical-devices/` and return the list of
/// (device_name, port_names) where any port's (service, prologue) pair
/// matches the given values. Used by the fold-prologue preview/execute
/// handlers to show / apply the cross-device impact.
fn scan_devices_for_prologue_pair(
    cfggen_base: &std::path::Path,
    service: &str,
    prologue: &str,
) -> Vec<(String, Vec<String>)> {
    let dir = cfggen_base.join("logical-devices");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let (device_name, json_path) = if path.is_dir() {
            let n = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            (n, path.join("config.json"))
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let n = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            (n, path)
        } else {
            continue;
        };
        if !json_path.exists() {
            continue;
        }
        let content = match std::fs::read_to_string(&json_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let raw: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut hits: Vec<String> = Vec::new();
        if let Some(modules) = raw.get("modules").and_then(|v| v.as_array()) {
            for module in modules {
                if let Some(ports) = module.get("ports").and_then(|p| p.as_array()) {
                    for port in ports {
                        let svc = port.get("service").and_then(|v| v.as_str());
                        let prol = port.get("prologue").and_then(|v| v.as_str());
                        if svc == Some(service) && prol == Some(prologue) {
                            if let Some(pn) = port.get("name").and_then(|v| v.as_str()) {
                                hits.push(pn.to_string());
                            }
                        }
                    }
                }
            }
        }
        if !hits.is_empty() {
            out.push((device_name, hits));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Build the `service_name → port-config.txt content` map the matcher
/// needs. Reads each service directory once; missing or unreadable
/// `port-config.txt` files are silently skipped (matching the import
/// path's tolerance).
fn load_service_port_configs(
    cfggen_base: &std::path::Path,
    service_names: &[String],
) -> std::collections::HashMap<String, String> {
    let services_dir = cfggen_base.join("services");
    let mut out = std::collections::HashMap::new();
    for name in service_names {
        let path = services_dir.join(name).join("port-config.txt");
        if let Ok(content) = std::fs::read_to_string(&path) {
            out.insert(name.clone(), content);
        }
    }
    out
}

/// Walk a parsed config tree and pull out each `interface X` section's
/// body lines (excluding the header itself). The keys are the section
/// headers (e.g. `interface TwoGigabitEthernet1/0/12`) — matches the
/// `iface_text` we build elsewhere when looking up by port.
fn parse_interface_bodies(
    tree: &aycicdiff::model::config_tree::ConfigTree,
) -> std::collections::HashMap<String, Vec<String>> {
    use aycicdiff::model::config_tree::ConfigNode;
    let mut out = std::collections::HashMap::new();
    for node in &tree.nodes {
        if let ConfigNode::Section(section) = node {
            if section.header.starts_with("interface ") {
                // Render each child as a single body line, preserving the
                // single-space sub-mode indentation that services use.
                let mut lines: Vec<String> = Vec::new();
                for child in &section.children {
                    match child {
                        ConfigNode::Leaf(l) => lines.push(format!(" {}", l.text)),
                        ConfigNode::Section(s) => {
                            // Rare in interface blocks; flatten the header
                            // as a line and ignore further nesting.
                            lines.push(format!(" {}", s.header));
                        }
                    }
                }
                out.insert(section.header.clone(), lines);
            }
        }
    }
    out
}

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

// ── Service-centric reconcile (cross-device) ─────────────────────────────────

#[derive(Serialize)]
struct ServiceIndexRow {
    name: String,
    device_count: usize,
    port_count: usize,
    distinct_prologue_count: usize,
    /// True if this service is referenced by at least one port across the
    /// fleet. Services nobody uses are listed at the bottom for awareness.
    in_use: bool,
    /// True iff `distinct_prologue_count > 0`. Template uses it to tint
    /// the row so cleanup candidates pop out of the list at a glance.
    has_fold_candidates: bool,
}

#[derive(Serialize)]
struct ServicesIndexCtx {
    rows: Vec<ServiceIndexRow>,
    in_use_count: usize,
    unused_count: usize,
    quicksearch_table_id: &'static str,
}

pub async fn services_index(State(state): State<AppState>) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };

    let available = load_available_services(&cfggen_base);
    let usage = collect_service_usage(&cfggen_base);
    let mut by_name: std::collections::HashMap<String, &ServiceUsageRaw> =
        usage.iter().map(|u| (u.name.clone(), u)).collect();

    let mut rows: Vec<ServiceIndexRow> = available
        .iter()
        .map(|svc| {
            let u = by_name.remove(svc);
            let (device_count, port_count, distinct_prologue_count) = u
                .map(|u| (u.devices.len(), u.port_count, u.distinct_prologue_count))
                .unwrap_or((0, 0, 0));
            ServiceIndexRow {
                name: svc.clone(),
                device_count,
                port_count,
                distinct_prologue_count,
                in_use: device_count > 0,
                has_fold_candidates: distinct_prologue_count > 0,
            }
        })
        .collect();
    // In-use first; within in-use, fold candidates first (sorted by
    // candidate count desc, then alphabetically); unused last.
    rows.sort_by(|a, b| {
        b.in_use
            .cmp(&a.in_use)
            .then_with(|| b.has_fold_candidates.cmp(&a.has_fold_candidates))
            .then_with(|| b.distinct_prologue_count.cmp(&a.distinct_prologue_count))
            .then_with(|| a.name.cmp(&b.name))
    });
    let in_use_count = rows.iter().filter(|r| r.in_use).count();
    let unused_count = rows.len() - in_use_count;

    let ctx = ServicesIndexCtx {
        rows,
        in_use_count,
        unused_count,
        quicksearch_table_id: "services-reconcile-table",
    };
    let html = state
        .templates
        .render_page(
            &state.templates.reconcile_services_index,
            "Service prologue reconciliation",
            "",
            &ctx,
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

#[derive(Serialize)]
struct ServicePrologueRow {
    /// The literal prologue text on the device JSON, including leading
    /// space. Used as a hidden form value when folding.
    prologue: String,
    /// Display version (trimmed) — what the operator reads in the table.
    prologue_display: String,
    /// True when the row represents ports whose `prologue` is null (i.e.
    /// already clean). Those rows don't get a Fold button.
    is_null: bool,
    port_count: usize,
    device_count: usize,
    /// "AD6-X001, AD6-X003 (+5 more)" preview.
    devices_preview: String,
    /// Repeated per row so the template's form action can reference it
    /// without needing parent-context lookup (which not all mustache
    /// implementations support inside iteration).
    service: String,
}

#[derive(Serialize)]
struct ServiceDetailCtx {
    service: String,
    service_path: String,
    port_config_preview: String,
    /// Only non-null prologue rows — actual fold candidates.
    rows: Vec<ServicePrologueRow>,
    has_rows: bool,
    /// "X ports across Y devices" summary for the (possibly empty) set
    /// of ports that already have prologue: null — informational only,
    /// never a fold candidate.
    null_port_count: usize,
    null_device_count: usize,
    /// "(N total ports use this service across the fleet, M of them
    /// already have no prologue)" — for the section header.
    total_port_count: usize,
}

pub async fn service_detail(
    State(state): State<AppState>,
    Path(svc): Path<String>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };
    let service_dir = cfggen_base.join("services").join(&svc);
    if !service_dir.exists() {
        return message(
            &state,
            "Unknown service",
            &format!("No service directory at {}", service_dir.display()),
            Some(("/reconcile/services", "Back to services")),
        );
    }

    let port_config_path = service_dir.join("port-config.txt");
    let port_config_preview = std::fs::read_to_string(&port_config_path).unwrap_or_default();

    let prologues = collect_service_prologues(&cfggen_base, &svc);
    let mut null_port_count = 0usize;
    let mut null_device_count = 0usize;
    let mut rows: Vec<ServicePrologueRow> = Vec::new();
    for p in prologues {
        let device_count = p.devices.len();
        let port_count: usize = p.devices.iter().map(|(_, ports)| ports.len()).sum();
        if p.is_null {
            null_device_count = device_count;
            null_port_count = port_count;
            continue;
        }
        let mut names: Vec<&str> = p.devices.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        let preview = if names.len() <= 4 {
            names.join(", ")
        } else {
            format!("{} (+{} more)", names[..4].join(", "), names.len() - 4)
        };
        rows.push(ServicePrologueRow {
            prologue_display: p.prologue.trim_start().to_string(),
            is_null: false,
            prologue: p.prologue,
            port_count,
            device_count,
            devices_preview: preview,
            service: svc.clone(),
        });
    }
    // Largest impact first.
    rows.sort_by(|a, b| b.port_count.cmp(&a.port_count));

    let total_port_count = rows.iter().map(|r| r.port_count).sum::<usize>() + null_port_count;

    let ctx = ServiceDetailCtx {
        service: svc.clone(),
        service_path: port_config_path.display().to_string(),
        port_config_preview,
        has_rows: !rows.is_empty(),
        rows,
        null_port_count,
        null_device_count,
        total_port_count,
    };
    let html = state
        .templates
        .render_page(
            &state.templates.reconcile_service_detail,
            &format!("Service: {svc}"),
            "",
            &ctx,
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

#[derive(Deserialize)]
pub struct ServiceFoldQuery {
    pub prologue: String,
}

#[derive(Deserialize)]
pub struct ServiceFoldForm {
    pub prologue: String,
}

/// Service-centric variant of the fold confirmation page. Reuses the
/// existing `reconcile_fold_preview` template but redirects back to the
/// per-service page after a successful fold (instead of a device's
/// reconcile page).
pub async fn service_fold_preview(
    State(state): State<AppState>,
    Path(svc): Path<String>,
    Query(q): Query<ServiceFoldQuery>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };

    let matches = scan_devices_for_prologue_pair(&cfggen_base, &svc, &q.prologue);
    let total_devices = matches.len();
    let total_ports: usize = matches.iter().map(|(_, ports)| ports.len()).sum();
    let devices: Vec<FoldDeviceCtx> = matches
        .into_iter()
        .map(|(device_name, ports)| FoldDeviceCtx {
            port_count: ports.len(),
            port_names: ports.join(", "),
            device_name,
        })
        .collect();
    let service_path = cfggen_base
        .join("services")
        .join(&svc)
        .join("port-config.txt")
        .display()
        .to_string();
    let prologue_display = q.prologue.trim_start().to_string();

    let form_action = format!("/reconcile/services/{}/fold-prologue", svc);
    let cancel_url = format!("/reconcile/services/{}", svc);
    let ctx = FoldPreviewCtx {
        name: svc.clone(), // unused by the template now, kept for layout
        form_action,
        cancel_url,
        service: svc,
        prologue: q.prologue,
        prologue_display,
        service_path,
        devices,
        total_devices,
        total_ports,
    };
    let html = state
        .templates
        .render_page(
            &state.templates.reconcile_fold_preview,
            "Fold prologue into service",
            "",
            &ctx,
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

pub async fn service_fold_execute(
    State(state): State<AppState>,
    Path(svc): Path<String>,
    Form(form): Form<ServiceFoldForm>,
) -> Response {
    // Delegate to the device-keyed executor — it doesn't actually use
    // the `name` parameter for the write, only for the post-fold redirect.
    // Build a fake-but-honest path so the inner handler's logic is
    // unchanged, then override the redirect ourselves.
    let inner_form = FoldForm {
        service: svc.clone(),
        prologue: form.prologue.clone(),
    };
    // Run the same work in-place — but redirect back to the service page.
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };
    let _ = inner_form; // (kept for parity; the inner call below uses fields directly)

    // ── Mirror fold_prologue_execute, but redirect to /reconcile/services/{svc}
    let port_config_path = cfggen_base
        .join("services")
        .join(&svc)
        .join("port-config.txt");
    if !port_config_path.exists() {
        return message(
            &state,
            "Unknown service",
            &format!("Service has no port-config.txt: {}", port_config_path.display()),
            Some(("/reconcile/services", "Back")),
        );
    }
    let existing = std::fs::read_to_string(&port_config_path).unwrap_or_default();
    let line_to_add = if form.prologue.starts_with(' ') {
        form.prologue.clone()
    } else {
        format!(" {}", form.prologue.trim_start())
    };
    let mut new_content = String::new();
    new_content.push_str(line_to_add.trim_end());
    new_content.push('\n');
    new_content.push_str(&existing);
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    if let Err(e) = std::fs::write(&port_config_path, &new_content) {
        warn!(path = %port_config_path.display(), error = %e, "Failed to write service port-config.txt");
        return message(&state, "Write failed", &format!("{e}"), None);
    }
    let matches = scan_devices_for_prologue_pair(&cfggen_base, &svc, &form.prologue);
    let touched_devices: Vec<String> = matches.iter().map(|(d, _)| d.clone()).collect();
    for (device_name, _) in &matches {
        clear_prologue_for_device(&cfggen_base, device_name, &svc, &form.prologue);
    }
    info!(
        service = %svc,
        devices = touched_devices.len(),
        "Folded prologue into service (service-centric)"
    );
    for device_name in &touched_devices {
        if let Err(e) = crate::routes::devices::compile_device_config(
            device_name,
            &cfggen_base,
            &state.config,
        ) {
            warn!(device = %device_name, error = %e, "Recompile after fold failed");
        }
    }
    Redirect::to(&format!("/reconcile/services/{}", svc)).into_response()
}

// ── Service-centric helpers ──────────────────────────────────────────────────

struct ServiceUsageRaw {
    name: String,
    devices: std::collections::HashSet<String>,
    port_count: usize,
    distinct_prologue_count: usize,
}

fn collect_service_usage(cfggen_base: &std::path::Path) -> Vec<ServiceUsageRaw> {
    let mut usage: std::collections::HashMap<String, ServiceUsageRaw> =
        std::collections::HashMap::new();
    // Only count NON-NULL distinct prologues — the column is the "things
    // worth folding" count, not "values seen including the null/clean ones."
    let mut nonnull_prologues_per_service: std::collections::HashMap<
        String,
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();

    walk_device_jsons(cfggen_base, |device_name, raw| {
        if let Some(modules) = raw.get("modules").and_then(|v| v.as_array()) {
            for module in modules {
                if let Some(ports) = module.get("ports").and_then(|p| p.as_array()) {
                    for port in ports {
                        let svc = port
                            .get("service")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if svc.is_empty() {
                            continue;
                        }
                        let entry = usage.entry(svc.clone()).or_insert_with(|| ServiceUsageRaw {
                            name: svc.clone(),
                            devices: std::collections::HashSet::new(),
                            port_count: 0,
                            distinct_prologue_count: 0,
                        });
                        entry.devices.insert(device_name.to_string());
                        entry.port_count += 1;
                        if let Some(prologue) =
                            port.get("prologue").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                        {
                            nonnull_prologues_per_service
                                .entry(svc)
                                .or_default()
                                .insert(prologue.to_string());
                        }
                    }
                }
            }
        }
    });

    for (svc, set) in nonnull_prologues_per_service {
        if let Some(entry) = usage.get_mut(&svc) {
            entry.distinct_prologue_count = set.len();
        }
    }
    usage.into_values().collect()
}

struct PrologueUsage {
    prologue: String,
    is_null: bool,
    devices: Vec<(String, Vec<String>)>,
}

fn collect_service_prologues(cfggen_base: &std::path::Path, svc: &str) -> Vec<PrologueUsage> {
    // prologue_text → device_name → ports
    let mut buckets: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, Vec<String>>,
    > = std::collections::BTreeMap::new();
    let mut null_buckets: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    walk_device_jsons(cfggen_base, |device_name, raw| {
        if let Some(modules) = raw.get("modules").and_then(|v| v.as_array()) {
            for module in modules {
                if let Some(ports) = module.get("ports").and_then(|p| p.as_array()) {
                    for port in ports {
                        if port.get("service").and_then(|v| v.as_str()) != Some(svc) {
                            continue;
                        }
                        let port_name = port
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string();
                        match port.get("prologue").and_then(|v| v.as_str()) {
                            Some(s) if !s.is_empty() => {
                                buckets
                                    .entry(s.to_string())
                                    .or_default()
                                    .entry(device_name.to_string())
                                    .or_default()
                                    .push(port_name);
                            }
                            _ => {
                                null_buckets
                                    .entry(device_name.to_string())
                                    .or_default()
                                    .push(port_name);
                            }
                        }
                    }
                }
            }
        }
    });

    let mut out: Vec<PrologueUsage> = Vec::new();
    for (prologue, devmap) in buckets {
        let devices: Vec<(String, Vec<String>)> = devmap.into_iter().collect();
        out.push(PrologueUsage {
            prologue,
            is_null: false,
            devices,
        });
    }
    if !null_buckets.is_empty() {
        let devices: Vec<(String, Vec<String>)> = null_buckets.into_iter().collect();
        out.push(PrologueUsage {
            prologue: String::new(),
            is_null: true,
            devices,
        });
    }
    out
}

fn walk_device_jsons(cfggen_base: &std::path::Path, mut cb: impl FnMut(&str, &serde_json::Value)) {
    let dir = cfggen_base.join("logical-devices");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let (device_name, json_path) = if path.is_dir() {
            let n = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            (n, path.join("config.json"))
        } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let n = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            (n, path)
        } else {
            continue;
        };
        if !json_path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&json_path) else { continue };
        let Ok(raw): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
            continue
        };
        cb(&device_name, &raw);
    }
}

/// Set `prologue: null` on every port in `device_name` that uses
/// (service, prologue). Writes the JSON back; logs and continues on
/// individual errors.
fn clear_prologue_for_device(
    cfggen_base: &std::path::Path,
    device_name: &str,
    service: &str,
    prologue: &str,
) {
    let flat = cfggen_base
        .join("logical-devices")
        .join(format!("{}.json", device_name));
    let dir = cfggen_base
        .join("logical-devices")
        .join(device_name)
        .join("config.json");
    let json_path = if flat.exists() {
        flat
    } else if dir.exists() {
        dir
    } else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&json_path) else { return };
    let Ok(mut raw): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
        return
    };
    if let Some(modules) = raw.get_mut("modules").and_then(|v| v.as_array_mut()) {
        for module in modules {
            if let Some(ports) = module.get_mut("ports").and_then(|p| p.as_array_mut()) {
                for port in ports {
                    let svc_matches = port.get("service").and_then(|v| v.as_str()) == Some(service);
                    let prol_matches =
                        port.get("prologue").and_then(|v| v.as_str()) == Some(prologue);
                    if svc_matches && prol_matches {
                        if let Some(obj) = port.as_object_mut() {
                            obj.insert("prologue".to_string(), serde_json::Value::Null);
                        }
                    }
                }
            }
        }
    }
    if let Ok(new_content) = serde_json::to_string_pretty(&raw) {
        let _ = std::fs::write(&json_path, new_content);
    }
}

// ── Service dedup (find services with identical or near-identical bodies) ────

/// Trim each line and drop trailing blank lines. Used as the *strict*
/// match key — two services with the same trimmed/joined content are
/// considered duplicates and one can be merged into the other with no
/// behaviour change.
fn normalize_service_strict(content: &str) -> String {
    let mut out = String::new();
    for line in content.lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Same as `normalize_service_strict` but with all `description ...`
/// lines stripped. Two services with the same loose key are equivalent
/// modulo description text — merging them is still safe semantically
/// but the description text on the device(s) will change. The dedup
/// UI surfaces both cases separately.
fn normalize_service_loose(content: &str) -> String {
    let mut out = String::new();
    for line in content.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("description ") {
            continue;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Both files a service carries. SVI-only services have an empty `port`;
/// access-VLAN services typically have an `svi` too (used when the
/// service is listed in `svi_services` for a device).
#[derive(Clone, Default)]
struct ServiceFiles {
    port: String,
    svi: String,
}

fn load_service_files(
    cfggen_base: &std::path::Path,
    service_names: &[String],
) -> std::collections::HashMap<String, ServiceFiles> {
    let services_dir = cfggen_base.join("services");
    let mut out = std::collections::HashMap::new();
    for name in service_names {
        let dir = services_dir.join(name);
        let port = std::fs::read_to_string(dir.join("port-config.txt")).unwrap_or_default();
        let svi = std::fs::read_to_string(dir.join("svi-config.txt")).unwrap_or_default();
        out.insert(name.clone(), ServiceFiles { port, svi });
    }
    out
}

/// Strict match key spanning both files. The `\u{1}` separator keeps
/// port and svi contents from accidentally aliasing through whitespace
/// (e.g. one ends with `\n`, the other starts with content).
fn normalize_files_strict(files: &ServiceFiles) -> String {
    format!(
        "PORT\u{1}{}\u{1}SVI\u{1}{}",
        normalize_service_strict(&files.port),
        normalize_service_strict(&files.svi)
    )
}

fn normalize_files_loose(files: &ServiceFiles) -> String {
    format!(
        "PORT\u{1}{}\u{1}SVI\u{1}{}",
        normalize_service_loose(&files.port),
        normalize_service_loose(&files.svi)
    )
}

/// True when both files are empty (or whitespace-only) — placeholder
/// service that shouldn't be lumped with others just because their
/// normalization keys collide.
fn service_files_empty(files: &ServiceFiles) -> bool {
    normalize_service_strict(&files.port).trim().is_empty()
        && normalize_service_strict(&files.svi).trim().is_empty()
}

/// Pretty-print both files for the canonical preview block. Empty files
/// get a `(empty)` placeholder so the operator knows the SVI side is
/// also being merged-as-empty.
fn render_files_preview(files: &ServiceFiles) -> String {
    let port = if files.port.trim().is_empty() {
        "(empty)".to_string()
    } else {
        files.port.trim_end().to_string()
    };
    let svi = if files.svi.trim().is_empty() {
        "(empty)".to_string()
    } else {
        files.svi.trim_end().to_string()
    };
    format!("──── port-config.txt ────\n{port}\n\n──── svi-config.txt ────\n{svi}\n")
}

/// Extract just the description lines for diffing.
fn description_lines(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|l| l.trim_start().starts_with("description "))
        .map(|l| l.trim().to_string())
        .collect()
}

fn wrap_lines(lines: Vec<String>) -> Vec<TextLine> {
    lines.into_iter().map(|text| TextLine { text }).collect()
}

#[derive(Serialize, Clone)]
struct TextLine {
    text: String,
}

#[derive(Serialize, Clone)]
struct DedupMember {
    name: String,
    device_count: usize,
    port_count: usize,
    /// `description ...` lines present in this member.
    descriptions: Vec<TextLine>,
    has_descriptions: bool,
    /// True if this is the suggested canonical for its group.
    is_canonical: bool,
}

#[derive(Serialize)]
struct DedupGroup {
    /// "identical" or "description-only" — controls the UI banner.
    kind: &'static str,
    is_identical: bool,
    /// All members of this group, with the suggested canonical first.
    members: Vec<DedupMember>,
    /// CSV of member names to merge INTO the canonical (everything
    /// except the canonical). Embedded in the merge link's query.
    merge_csv: String,
    /// Canonical name (also = members[0].name).
    canonical: String,
    /// Sum of port_counts across all non-canonical members — how many
    /// ports the merge would touch.
    impacted_ports: usize,
    /// Sum of device_counts across all non-canonical members — how many
    /// devices the merge would touch (de-dup'd by name).
    impacted_devices: usize,
    /// Canonical's port-config.txt content for a side preview.
    canonical_preview: String,
}

#[derive(Serialize)]
struct DedupIndexCtx {
    identical_groups: Vec<DedupGroup>,
    description_only_groups: Vec<DedupGroup>,
    has_identical: bool,
    has_description_only: bool,
}

fn find_duplicate_groups(cfggen_base: &std::path::Path) -> (Vec<DedupGroup>, Vec<DedupGroup>) {
    let services = load_available_services(cfggen_base);
    // Both port-config.txt AND svi-config.txt feed the match key — two
    // services with identical port bodies but different SVI bodies (very
    // common for `access-vlanN` services that ride alongside an SVI
    // service per chassis) are NOT duplicates.
    let files_map = load_service_files(cfggen_base, &services);
    let usage = collect_service_usage(cfggen_base);
    let usage_by: std::collections::HashMap<String, &ServiceUsageRaw> =
        usage.iter().map(|u| (u.name.clone(), u)).collect();

    let mut strict_buckets: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut loose_buckets: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for name in &services {
        let files = files_map.get(name).cloned().unwrap_or_default();
        // Skip true placeholders (both files empty / whitespace-only) so
        // we don't surface "merge every stub together" suggestions.
        if service_files_empty(&files) {
            continue;
        }
        let strict_key = normalize_files_strict(&files);
        let loose_key = normalize_files_loose(&files);
        strict_buckets.entry(strict_key).or_default().push(name.clone());
        loose_buckets.entry(loose_key).or_default().push(name.clone());
    }

    let make_group = |members: Vec<String>, kind: &'static str| -> DedupGroup {
        // Canonical = the member with the highest port count, ties broken
        // alphabetically. Stable so a refresh doesn't pick a different one.
        let mut sorted_members = members.clone();
        sorted_members.sort_by(|a, b| {
            let pa = usage_by.get(a).map(|u| u.port_count).unwrap_or(0);
            let pb = usage_by.get(b).map(|u| u.port_count).unwrap_or(0);
            pb.cmp(&pa).then_with(|| a.cmp(b))
        });
        let canonical = sorted_members[0].clone();
        let canonical_files = files_map.get(&canonical).cloned().unwrap_or_default();
        let canonical_preview = render_files_preview(&canonical_files);

        let member_views: Vec<DedupMember> = sorted_members
            .iter()
            .map(|m| {
                let u = usage_by.get(m);
                let device_count = u.map(|u| u.devices.len()).unwrap_or(0);
                let port_count = u.map(|u| u.port_count).unwrap_or(0);
                let f = files_map.get(m).cloned().unwrap_or_default();
                // Pull descriptions from BOTH files — they live in either
                // depending on whether the service describes a port block
                // or an SVI.
                let mut descs = description_lines(&f.port);
                descs.extend(description_lines(&f.svi));
                let has_descriptions = !descs.is_empty();
                DedupMember {
                    name: m.clone(),
                    device_count,
                    port_count,
                    descriptions: wrap_lines(descs),
                    has_descriptions,
                    is_canonical: m == &canonical,
                }
            })
            .collect();

        // Tally impact across non-canonical members; dedupe device set.
        let mut impacted_device_set: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut impacted_ports = 0usize;
        for m in &member_views {
            if m.is_canonical {
                continue;
            }
            impacted_ports += m.port_count;
            if let Some(u) = usage_by.get(&m.name) {
                for d in &u.devices {
                    impacted_device_set.insert(d.clone());
                }
            }
        }
        let merge_csv = member_views
            .iter()
            .filter(|m| !m.is_canonical)
            .map(|m| m.name.clone())
            .collect::<Vec<_>>()
            .join(",");
        DedupGroup {
            kind,
            is_identical: kind == "identical",
            canonical,
            canonical_preview,
            members: member_views,
            merge_csv,
            impacted_ports,
            impacted_devices: impacted_device_set.len(),
        }
    };

    // Strict groups first (these are the slam-dunk merges).
    let mut identical: Vec<DedupGroup> = strict_buckets
        .into_iter()
        .filter(|(_, names)| names.len() >= 2)
        .map(|(_, names)| make_group(names, "identical"))
        .collect();
    identical.sort_by(|a, b| b.impacted_ports.cmp(&a.impacted_ports));

    // Loose groups, EXCLUDING anything already covered by a strict group
    // (so the description-only list only carries near-but-not-identical
    // matches that the operator still has to decide on).
    let strict_member_set: std::collections::HashSet<String> = identical
        .iter()
        .flat_map(|g| g.members.iter().map(|m| m.name.clone()))
        .collect();
    let mut description_only: Vec<DedupGroup> = loose_buckets
        .into_iter()
        .filter(|(_, names)| names.len() >= 2)
        .filter(|(_, names)| !names.iter().all(|n| strict_member_set.contains(n)))
        .map(|(_, names)| make_group(names, "description-only"))
        .collect();
    description_only.sort_by(|a, b| b.impacted_ports.cmp(&a.impacted_ports));

    (identical, description_only)
}

pub async fn dedup_index(State(state): State<AppState>) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };
    let (identical_groups, description_only_groups) = find_duplicate_groups(&cfggen_base);
    let ctx = DedupIndexCtx {
        has_identical: !identical_groups.is_empty(),
        has_description_only: !description_only_groups.is_empty(),
        identical_groups,
        description_only_groups,
    };
    let html = state
        .templates
        .render_page(
            &state.templates.reconcile_dedup_index,
            "Service deduplication",
            "",
            &ctx,
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

// ── Dedup merge preview / execute ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct DedupMergeQuery {
    /// Service name to keep.
    pub canonical: String,
    /// Comma-separated list of services to merge INTO the canonical.
    pub merge: String,
}

#[derive(Deserialize)]
pub struct DedupMergeForm {
    pub canonical: String,
    pub merge: String,
    /// `1` means also delete the source services' directories from disk
    /// after the merge. Default is to leave them in place (operator
    /// can clean up manually).
    #[serde(default)]
    pub delete_after: String,
}

#[derive(Serialize)]
struct DedupAffectedDevice {
    device_name: String,
    /// "Port5, Port6 (was access-vlan6-2); Port12 (was access-vlan6-x)"
    port_breakdown: String,
    port_count: usize,
}

#[derive(Serialize)]
struct DedupMergePreviewCtx {
    canonical: String,
    canonical_preview: String,
    /// CSV passed through to the POST form.
    merge: String,
    /// Member rows including the canonical (so the operator sees a
    /// before-and-after of what they're merging away).
    members: Vec<DedupMember>,
    description_only: bool,
    /// Description lines unique to non-canonical members — surfaces
    /// "you're about to drop these descriptions" warnings.
    extra_descriptions: Vec<TextLine>,
    devices: Vec<DedupAffectedDevice>,
    total_devices: usize,
    total_ports: usize,
    has_extra_descriptions: bool,
}

pub async fn dedup_merge_preview(
    State(state): State<AppState>,
    Query(q): Query<DedupMergeQuery>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };

    let merge_names: Vec<String> = q
        .merge
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if merge_names.is_empty() {
        return message(
            &state,
            "Nothing to merge",
            "merge= query parameter was empty",
            Some(("/reconcile/dedup", "Back")),
        );
    }

    let available = load_available_services(&cfggen_base);
    let files_map = load_service_files(&cfggen_base, &available);
    let canonical_files = files_map.get(&q.canonical).cloned().unwrap_or_default();
    if service_files_empty(&canonical_files) {
        return message(
            &state,
            "Unknown canonical",
            &format!(
                "Service '{}' has no port-config.txt or svi-config.txt content",
                q.canonical
            ),
            Some(("/reconcile/dedup", "Back")),
        );
    }
    let canonical_preview = render_files_preview(&canonical_files);

    let usage = collect_service_usage(&cfggen_base);
    let usage_by: std::collections::HashMap<String, &ServiceUsageRaw> =
        usage.iter().map(|u| (u.name.clone(), u)).collect();

    // Are these strict-identical or just description-only? Both files
    // factor into the comparison.
    let canon_strict_key = normalize_files_strict(&canonical_files);
    let canon_loose_key = normalize_files_loose(&canonical_files);
    let mut description_only = false;
    for n in &merge_names {
        let f = files_map.get(n).cloned().unwrap_or_default();
        if normalize_files_strict(&f) != canon_strict_key
            && normalize_files_loose(&f) == canon_loose_key
        {
            description_only = true;
        }
    }

    let mut members: Vec<DedupMember> = std::iter::once(q.canonical.clone())
        .chain(merge_names.iter().cloned())
        .map(|n| {
            let u = usage_by.get(&n);
            let f = files_map.get(&n).cloned().unwrap_or_default();
            let mut descs = description_lines(&f.port);
            descs.extend(description_lines(&f.svi));
            let has_descriptions = !descs.is_empty();
            DedupMember {
                name: n.clone(),
                device_count: u.map(|u| u.devices.len()).unwrap_or(0),
                port_count: u.map(|u| u.port_count).unwrap_or(0),
                descriptions: wrap_lines(descs),
                has_descriptions,
                is_canonical: n == q.canonical,
            }
        })
        .collect();

    // Description lines on any non-canonical member that aren't on the canonical.
    let canon_descs: std::collections::HashSet<String> = members[0]
        .descriptions
        .iter()
        .map(|t| t.text.clone())
        .collect();
    let mut extra_descriptions: Vec<TextLine> = Vec::new();
    let mut seen_extra: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in members.iter().skip(1) {
        for d in &m.descriptions {
            if !canon_descs.contains(&d.text) && seen_extra.insert(d.text.clone()) {
                extra_descriptions.push(d.clone());
            }
        }
    }

    // Scan every device JSON for ports whose service is one of the mergees.
    let merge_set: std::collections::HashSet<&str> =
        merge_names.iter().map(|s| s.as_str()).collect();
    let mut by_device: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    walk_device_jsons(&cfggen_base, |device_name, raw| {
        if let Some(modules) = raw.get("modules").and_then(|v| v.as_array()) {
            for module in modules {
                if let Some(ports) = module.get("ports").and_then(|p| p.as_array()) {
                    for port in ports {
                        let svc = port.get("service").and_then(|v| v.as_str()).unwrap_or("");
                        if merge_set.contains(svc) {
                            let port_name = port
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string();
                            by_device
                                .entry(device_name.to_string())
                                .or_default()
                                .push((svc.to_string(), port_name));
                        }
                    }
                }
            }
        }
    });

    let mut devices: Vec<DedupAffectedDevice> = Vec::new();
    let mut total_ports = 0usize;
    for (device_name, mut hits) in by_device {
        hits.sort();
        total_ports += hits.len();
        // Group by source service for the human-readable breakdown.
        let mut by_svc: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for (svc, port) in &hits {
            by_svc.entry(svc.clone()).or_default().push(port.clone());
        }
        let breakdown = by_svc
            .into_iter()
            .map(|(svc, ports)| format!("{} (was {})", ports.join(", "), svc))
            .collect::<Vec<_>>()
            .join("; ");
        let port_count = hits.len();
        devices.push(DedupAffectedDevice {
            device_name,
            port_breakdown: breakdown,
            port_count,
        });
    }
    let total_devices = devices.len();

    // Refresh canonical descriptions from disk for display — same source
    // both files used to build the match key.
    let mut canon_descs = description_lines(&canonical_files.port);
    canon_descs.extend(description_lines(&canonical_files.svi));
    members[0].descriptions = wrap_lines(canon_descs);
    members[0].has_descriptions = !members[0].descriptions.is_empty();

    let ctx = DedupMergePreviewCtx {
        canonical: q.canonical,
        canonical_preview,
        merge: q.merge,
        members,
        description_only,
        has_extra_descriptions: !extra_descriptions.is_empty(),
        extra_descriptions,
        devices,
        total_devices,
        total_ports,
    };
    let html = state
        .templates
        .render_page(
            &state.templates.reconcile_dedup_preview,
            "Merge services",
            "",
            &ctx,
        )
        .unwrap_or_else(|e| format!("<h1>Template error</h1><pre>{e}</pre>"));
    Html(html).into_response()
}

pub async fn dedup_merge_execute(
    State(state): State<AppState>,
    Form(form): Form<DedupMergeForm>,
) -> Response {
    let cfggen_base = match &state.config.cfggen_base_dir {
        Some(p) if p.exists() => p.clone(),
        _ => return message(&state, "Not configured", "cfggen_base_dir is unset", None),
    };

    let merge_names: Vec<String> = form
        .merge
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if merge_names.is_empty() {
        return message(
            &state,
            "Nothing to merge",
            "merge field was empty",
            Some(("/reconcile/dedup", "Back")),
        );
    }

    let canonical_dir = cfggen_base.join("services").join(&form.canonical);
    if !canonical_dir.exists() {
        return message(
            &state,
            "Unknown canonical",
            &format!("No service at {}", canonical_dir.display()),
            Some(("/reconcile/dedup", "Back")),
        );
    }

    // Step 1: rewrite every device JSON, swapping merged services →
    // canonical. Remember which devices we touched for the recompile loop.
    let merge_set: std::collections::HashSet<&str> =
        merge_names.iter().map(|s| s.as_str()).collect();
    let mut touched_devices: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    walk_device_jsons(&cfggen_base, |device_name, _| {
        let flat = cfggen_base
            .join("logical-devices")
            .join(format!("{}.json", device_name));
        let dir = cfggen_base
            .join("logical-devices")
            .join(device_name)
            .join("config.json");
        let json_path = if flat.exists() {
            flat
        } else if dir.exists() {
            dir
        } else {
            return;
        };
        let Ok(content) = std::fs::read_to_string(&json_path) else { return };
        let Ok(mut raw): Result<serde_json::Value, _> = serde_json::from_str(&content) else {
            return
        };
        let mut changed = false;
        if let Some(modules) = raw.get_mut("modules").and_then(|v| v.as_array_mut()) {
            for module in modules {
                if let Some(ports) = module.get_mut("ports").and_then(|p| p.as_array_mut()) {
                    for port in ports {
                        let svc_owned: Option<String> = port
                            .get("service")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if let Some(svc) = svc_owned {
                            if merge_set.contains(svc.as_str()) {
                                if let Some(obj) = port.as_object_mut() {
                                    obj.insert(
                                        "service".to_string(),
                                        serde_json::Value::String(form.canonical.clone()),
                                    );
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        if changed {
            touched_devices.insert(device_name.to_string());
            if let Ok(new_content) = serde_json::to_string_pretty(&raw) {
                let _ = std::fs::write(&json_path, new_content);
            }
        }
    });

    info!(
        canonical = %form.canonical,
        merge = ?merge_names,
        devices = touched_devices.len(),
        "Merged services into canonical"
    );

    // Step 2: optionally delete the merged service directories.
    let delete_after = matches!(form.delete_after.as_str(), "1" | "on" | "true");
    if delete_after {
        for n in &merge_names {
            let p = cfggen_base.join("services").join(n);
            if p.exists() {
                if let Err(e) = std::fs::remove_dir_all(&p) {
                    warn!(path = %p.display(), error = %e, "Failed to delete merged service dir");
                }
            }
        }
    }

    // Step 3: recompile every touched device locally.
    for device_name in &touched_devices {
        if let Err(e) = crate::routes::devices::compile_device_config(
            device_name,
            &cfggen_base,
            &state.config,
        ) {
            warn!(device = %device_name, error = %e, "Recompile after merge failed");
        }
    }

    Redirect::to("/reconcile/dedup").into_response()
}

// ── Routes ───────────────────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/diff/{name}/reconcile", get(reconcile_detail))
        .route("/diff/{name}/reconcile/swap", post(swap_service))
        .route("/diff/{name}/reconcile/absorb", post(absorb_drift))
        .route(
            "/diff/{name}/reconcile/fold-prologue",
            get(fold_prologue_preview).post(fold_prologue_execute),
        )
        .route("/reconcile/services", get(services_index))
        .route("/reconcile/services/{svc}", get(service_detail))
        .route(
            "/reconcile/services/{svc}/fold-prologue",
            get(service_fold_preview).post(service_fold_execute),
        )
        .route("/reconcile/dedup", get(dedup_index))
        .route(
            "/reconcile/dedup/merge",
            get(dedup_merge_preview).post(dedup_merge_execute),
        )
}

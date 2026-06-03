//! `aynmsgui cli` — non-server inspection commands.
//!
//! Invoked when argv[1] is literally `cli`. Lets us drive the same data
//! pipelines the web routes use, without spinning up the server — useful
//! for verifying reconcile / dedup output, and for scripting one-off
//! audits.

use std::collections::HashMap;
use std::path::PathBuf;

use aycfggen::compile_traced::compile_device_traced;
use aycfggen::fs_sources::{
    FsConfigElementSource, FsConfigTemplateSource, FsHardwareTemplateSource,
    FsLogicalDeviceSource, FsServiceSource, FsSoftwareImageSource,
};
use aycfggen::provenance::{LineProv, ProvSource};
use aycicdiff::diff::diff_model::DiffAction;
use aycicdiff::model::config_tree::ConfigNode;
use clap::{Parser, Subcommand};

use crate::routes::devices::{load_all_device_configs, serial_to_device_names};

#[derive(Parser)]
#[command(
    name = "aynmsgui cli",
    about = "Inspect aynmsgui state without running the server",
    long_about = "Drive the same data pipelines the web UI uses. Useful for verifying \
                  reconcile output, auditing dedup candidates, and scripting one-off \
                  cleanup operations."
)]
struct CliArgs {
    /// Path to the cfggen base dir.
    #[arg(
        long,
        default_value = "data/cfggen",
        env = "AYNMSGUI_CFGGEN_BASE_DIR"
    )]
    cfggen_base: PathBuf,

    /// Path to the current-configs dir (retrieved device configs).
    #[arg(
        long,
        default_value = "data/current-configs",
        env = "AYNMSGUI_CURRENT_CONFIGS_PATH"
    )]
    current_configs: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Dump per-line delta + provenance for one device's reconcile page.
    /// The argument can be the logical-device name (e.g. AD6-X001) or
    /// the serial (e.g. FCW2228L054) — same resolution the web route does.
    Reconcile {
        device: String,
    },
    /// Print a per-port summary of which group each delta line lands in
    /// (port-service / template / svi / other), so we can verify the
    /// grouping logic that drives the reconcile page.
    ReconcileGroups {
        device: String,
    },
}

pub fn run(argv: Vec<String>) {
    // Initialize tracing in case any inner code logs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();

    let args = match CliArgs::try_parse_from(argv) {
        Ok(a) => a,
        Err(e) => {
            e.print().ok();
            std::process::exit(e.exit_code());
        }
    };
    match &args.cmd {
        Cmd::Reconcile { device } => cmd_reconcile(&args, device),
        Cmd::ReconcileGroups { device } => cmd_reconcile_groups(&args, device),
    }
}

fn resolve_device_name(cfggen_base: &std::path::Path, raw: &str) -> Option<String> {
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

fn compile(base: &std::path::Path, device_name: &str) -> (String, Vec<LineProv>) {
    compile_device_traced(
        device_name,
        &FsLogicalDeviceSource::new(base.join("logical-devices")),
        &FsHardwareTemplateSource::new(base.join("hardware-templates")),
        &FsServiceSource::new(base.join("services")),
        &FsConfigTemplateSource::new(base.join("config-templates")),
        &FsConfigElementSource::new(base.join("config-elements")),
        &FsSoftwareImageSource::new(base.join("software-images")),
    )
    .expect("compile_device_traced")
}

fn current_text(args: &CliArgs, device_name: &str) -> String {
    // Look up the serial via the logical device JSON so we read the right
    // .cfg under current-configs/ (which is serial-keyed).
    let logical = args
        .cfggen_base
        .join("logical-devices")
        .join(device_name)
        .join("config.json");
    let flat = args
        .cfggen_base
        .join("logical-devices")
        .join(format!("{}.json", device_name));
    let json_path = if logical.exists() { logical } else { flat };
    let serial: Option<String> = std::fs::read_to_string(&json_path)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .and_then(|v| {
            v.get("modules")
                .and_then(|m| m.as_array())
                .and_then(|arr| {
                    arr.iter().find_map(|m| {
                        m.get("serial")
                            .and_then(|s| s.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                    })
                })
        });
    let cfg_path = match serial {
        Some(s) => args.current_configs.join(format!("{}.cfg", s)),
        None => args.current_configs.join(format!("{}.cfg", device_name)),
    };
    std::fs::read_to_string(&cfg_path).unwrap_or_default()
}

fn cmd_reconcile(args: &CliArgs, raw: &str) {
    let Some(device_name) = resolve_device_name(&args.cfggen_base, raw) else {
        eprintln!("error: could not resolve '{raw}' to a logical device");
        std::process::exit(2);
    };
    let (target_text, provs) = compile(&args.cfggen_base, &device_name);
    let target_norm = aycfgapply::normalize::normalize_target_config(&target_text);
    let current_norm = aycfgapply::normalize::normalize_config(&current_text(args, &device_name));

    let rules = aycicdiff::rules::RulesConfig::builtin();
    let ct = aycicdiff::parser::parse_config(&current_norm, &rules);
    let tt = aycicdiff::parser::parse_config(&target_norm, &rules);
    let diff = aycicdiff::diff::diff_configs(&ct, &tt, &rules);

    // Build trimmed-text → list of (idx, &prov) the same way reconcile.rs
    // does — keep normalization in lockstep.
    let mut by_text: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in provs.iter().enumerate() {
        by_text.entry(p.text.trim().to_string()).or_default().push(i);
    }

    println!("# Reconcile for {device_name} ({} lines in target)", provs.len());
    println!("# Delta tree walk, sections indent the lines that belong to them.");
    let mut ctx: Vec<String> = Vec::new();
    walk(&diff.actions, &mut ctx, &by_text, &provs);
}

fn cmd_reconcile_groups(args: &CliArgs, raw: &str) {
    let Some(device_name) = resolve_device_name(&args.cfggen_base, raw) else {
        eprintln!("error: could not resolve '{raw}' to a logical device");
        std::process::exit(2);
    };
    let (target_text, provs) = compile(&args.cfggen_base, &device_name);
    let target_norm = aycfgapply::normalize::normalize_target_config(&target_text);
    let current_norm = aycfgapply::normalize::normalize_config(&current_text(args, &device_name));

    let rules = aycicdiff::rules::RulesConfig::builtin();
    let ct = aycicdiff::parser::parse_config(&current_norm, &rules);
    let tt = aycicdiff::parser::parse_config(&target_norm, &rules);
    let diff = aycicdiff::diff::diff_configs(&ct, &tt, &rules);

    let mut by_text: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in provs.iter().enumerate() {
        by_text.entry(p.text.trim().to_string()).or_default().push(i);
    }

    // Tally how many delta lines land in each "group" — port/svi/template/other.
    let mut tally: HashMap<&'static str, usize> = HashMap::new();
    let mut ctx: Vec<String> = Vec::new();
    tally_walk(&diff.actions, &mut ctx, &by_text, &provs, &mut tally);

    println!("# Group tally for {device_name}");
    let mut sorted: Vec<_> = tally.iter().collect();
    sorted.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for (group, n) in sorted {
        println!("  {:20} {n}", group);
    }
}

// ── DiffTree walker ──────────────────────────────────────────────────────────

fn walk(
    actions: &[DiffAction],
    ctx: &mut Vec<String>,
    by_text: &HashMap<String, Vec<usize>>,
    provs: &[LineProv],
) {
    for a in actions {
        match a {
            DiffAction::Add(n) => emit("ADD", n, ctx, by_text, provs),
            DiffAction::Remove(n) => emit("REM", n, ctx, by_text, provs),
            DiffAction::ModifySection {
                header,
                child_actions,
                ..
            } => {
                println!("{}MOD [{}]", indent(ctx.len()), header.trim());
                ctx.push(header.trim().to_string());
                walk(child_actions, ctx, by_text, provs);
                ctx.pop();
            }
            DiffAction::ReplaceOrdered {
                header,
                remove_children,
                add_children,
            } => {
                println!("{}REPLACE [{}]", indent(ctx.len()), header.trim());
                ctx.push(header.trim().to_string());
                for r in remove_children {
                    println!("{}REM  {}  ← (drift)", indent(ctx.len()), r.text);
                }
                for a in add_children {
                    let prov = lookup(&a.text, ctx, by_text, provs);
                    println!("{}ADD  {}  ← {prov}", indent(ctx.len()), a.text);
                }
                ctx.pop();
            }
        }
    }
}

fn emit(
    tag: &str,
    n: &ConfigNode,
    ctx: &mut Vec<String>,
    by_text: &HashMap<String, Vec<usize>>,
    provs: &[LineProv],
) {
    match n {
        ConfigNode::Leaf(l) => {
            let prov = if tag == "ADD" {
                lookup(&l.text, ctx, by_text, provs)
            } else {
                "(drift)".to_string()
            };
            println!("{}{tag}  {}  ← {prov}", indent(ctx.len()), l.text);
        }
        ConfigNode::Section(s) => {
            let prov = if tag == "ADD" {
                lookup(&s.header, ctx, by_text, provs)
            } else {
                "(drift)".to_string()
            };
            println!("{}{tag}  {}  ← {prov}", indent(ctx.len()), s.header);
            ctx.push(s.header.trim().to_string());
            for c in &s.children {
                emit(tag, c, ctx, by_text, provs);
            }
            ctx.pop();
        }
    }
}

fn lookup(
    text: &str,
    ctx: &[String],
    by_text: &HashMap<String, Vec<usize>>,
    provs: &[LineProv],
) -> String {
    let key = text.trim();
    let Some(indices) = by_text.get(key) else {
        return "(unresolved)".to_string();
    };
    let iface_ctx = ctx
        .iter()
        .rev()
        .find(|s| s.starts_with("interface "))
        .cloned();
    for &i in indices {
        if let Some(ref ic) = iface_ctx {
            if line_is_in_interface_block(provs, i, ic) {
                return short_source(&provs[i].source);
            }
        }
    }
    if indices.len() == 1 {
        short_source(&provs[indices[0]].source)
    } else {
        format!("{} candidates", indices.len())
    }
}

fn short_source(src: &ProvSource) -> String {
    match src {
        ProvSource::Template { path, line } => format!("Template {path}:{line}"),
        ProvSource::TemplateVarExpanded { path, line } => format!("Template+vars {path}:{line}"),
        ProvSource::ConfigElement { element, line, .. } => format!("Element {element}:{line}"),
        ProvSource::ConfigElementMarker { element, .. } => format!("ElementMarker {element}"),
        ProvSource::PortInterfaceHeader {
            module_idx, port_name, ..
        } => format!("PortIfaceHeader [{module_idx}/{port_name}]"),
        ProvSource::PortPrologue {
            module_idx,
            port_name,
            prologue_line,
        } => format!("PortPrologue [{module_idx}/{port_name}]:{prologue_line}"),
        ProvSource::PortService {
            module_idx,
            port_name,
            service,
            service_line,
        } => format!("PortService {service} [{module_idx}/{port_name}]:port-config.txt:{service_line}"),
        ProvSource::PortEpilogue {
            module_idx,
            port_name,
            epilogue_line,
        } => format!("PortEpilogue [{module_idx}/{port_name}]:{epilogue_line}"),
        ProvSource::SviService {
            service,
            service_line,
        } => format!("SviService {service}:svi-config.txt:{service_line}"),
        ProvSource::Structural { kind } => format!("Structural {kind}"),
        ProvSource::Unknown { hint } => format!("Unknown ({hint})"),
    }
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

fn line_is_in_interface_block(provs: &[LineProv], idx: usize, wanted: &str) -> bool {
    for i in (0..=idx).rev() {
        let t = provs[i].text.trim();
        if t.starts_with("interface ") {
            return t == wanted;
        }
        if !provs[i].text.starts_with(' ') && !provs[i].text.starts_with('\t') {
            return false;
        }
    }
    false
}

// ── Group tally walker ───────────────────────────────────────────────────────

fn tally_walk(
    actions: &[DiffAction],
    ctx: &mut Vec<String>,
    by_text: &HashMap<String, Vec<usize>>,
    provs: &[LineProv],
    tally: &mut HashMap<&'static str, usize>,
) {
    for a in actions {
        match a {
            DiffAction::Add(n) => tally_emit(n, ctx, by_text, provs, tally),
            DiffAction::Remove(_) => *tally.entry("drift (removes)").or_insert(0) += 1,
            DiffAction::ModifySection {
                header,
                child_actions,
                ..
            } => {
                ctx.push(header.trim().to_string());
                tally_walk(child_actions, ctx, by_text, provs, tally);
                ctx.pop();
            }
            DiffAction::ReplaceOrdered {
                header,
                remove_children,
                add_children,
            } => {
                ctx.push(header.trim().to_string());
                *tally.entry("drift (removes)").or_insert(0) += remove_children.len();
                for a in add_children {
                    tally_one(&a.text, ctx, by_text, provs, tally);
                }
                ctx.pop();
            }
        }
    }
}

fn tally_emit(
    n: &ConfigNode,
    ctx: &mut Vec<String>,
    by_text: &HashMap<String, Vec<usize>>,
    provs: &[LineProv],
    tally: &mut HashMap<&'static str, usize>,
) {
    match n {
        ConfigNode::Leaf(l) => tally_one(&l.text, ctx, by_text, provs, tally),
        ConfigNode::Section(s) => {
            tally_one(&s.header, ctx, by_text, provs, tally);
            ctx.push(s.header.trim().to_string());
            for c in &s.children {
                tally_emit(c, ctx, by_text, provs, tally);
            }
            ctx.pop();
        }
    }
}

fn tally_one(
    text: &str,
    ctx: &[String],
    by_text: &HashMap<String, Vec<usize>>,
    provs: &[LineProv],
    tally: &mut HashMap<&'static str, usize>,
) {
    let key = text.trim();
    let Some(indices) = by_text.get(key) else {
        *tally.entry("unresolved").or_insert(0) += 1;
        return;
    };
    let iface_ctx = ctx
        .iter()
        .rev()
        .find(|s| s.starts_with("interface "))
        .cloned();
    let mut chosen: Option<usize> = None;
    for &i in indices {
        if let Some(ref ic) = iface_ctx {
            if line_is_in_interface_block(provs, i, ic) {
                chosen = Some(i);
                break;
            }
        }
    }
    if chosen.is_none() && indices.len() == 1 {
        chosen = Some(indices[0]);
    }
    let Some(idx) = chosen else {
        *tally.entry("ambiguous").or_insert(0) += 1;
        return;
    };
    let bucket = match &provs[idx].source {
        ProvSource::PortService { .. }
        | ProvSource::PortInterfaceHeader { .. }
        | ProvSource::PortPrologue { .. }
        | ProvSource::PortEpilogue { .. } => "port (service-driven)",
        ProvSource::SviService { .. } => "svi (service-driven)",
        ProvSource::Template { .. } | ProvSource::TemplateVarExpanded { .. } => {
            "template (other)"
        }
        ProvSource::ConfigElement { .. } | ProvSource::ConfigElementMarker { .. } => {
            "config-element (other)"
        }
        ProvSource::Structural { .. } => "structural (other)",
        ProvSource::Unknown { .. } => "unknown",
    };
    *tally.entry(bucket).or_insert(0) += 1;
}

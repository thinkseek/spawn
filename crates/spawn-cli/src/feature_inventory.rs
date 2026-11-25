//! IO adapter, CLI subcommand (`spawn features`), and HTTP payload builders
//! for feature-map file coverage and one-click unassociated-file ("野文件")
//! scanning, and the read-only code graph (`spawn features graph`) that
//! checks the registry's `depends_on` against the code.
//!
//! All pure coverage/consistency logic lives in
//! [`spawn_core::feature_coverage`]. This module performs bounded filesystem
//! walking, git index querying, and atomic registry persistence.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use spawn_core::feature_coverage::{
    AssociationUpdate, BoundaryPolicy, FeatureRegistry, InventoryEntry, SCHEMA_VERSION, ScanReport,
    associate_file, match_directory_exclusion, match_exclusion, scan_coverage,
};
use spawn_core::feature_deps::{
    self, CodeGraph, Diagnostic, DiagnosticCode, FeatureEdge, FeatureIndex, FileEdge, Scope,
    TruthCounts, TruthPolicy, TruthReport,
};

/// Default registry path relative to the project root.
pub const DEFAULT_REGISTRY_REL: &str = "docs/feature-map/registry.json";

/// Resolve the default project root from `SPAWN_FEATURE_ROOT` or current
/// working directory.
pub fn default_root() -> PathBuf {
    if let Some(root) = crate::env_value("SPAWN_FEATURE_ROOT") {
        return PathBuf::from(root);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve the default registry path from `SPAWN_FEATURE_REGISTRY` or
/// `<root>/docs/feature-map/registry.json`.
pub fn default_registry_path(root: &Path) -> PathBuf {
    if let Some(registry) = crate::env_value("SPAWN_FEATURE_REGISTRY") {
        return PathBuf::from(registry);
    }
    root.join(DEFAULT_REGISTRY_REL)
}

/// `path` with its `.` and `..` segments resolved lexically (a `..` drops
/// the segment before it; one past the root stays at the root; a relative
/// path keeps its leading `..`). An empty result is `.`. Only for a path
/// nothing of which exists ([`real_registry_path`]): on disk a `..` after a
/// symlink leaves the link's target, not the link's directory.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normal.components().next_back() {
                Some(Component::Normal(_)) => {
                    normal.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => normal.push(".."),
            },
            other => normal.push(other),
        }
    }
    if normal.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normal
    }
}

/// Where `registry` really is: the filesystem's canonical path when it
/// exists, else its canonical parent joined with its file name, else (when
/// neither exists) its lexical normalization. Only for finding the project
/// root: the registry itself is always opened and saved at the path as
/// configured.
fn real_registry_path(registry: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(registry) {
        return real;
    }
    if let (Some(parent), Some(name)) = (registry.parent(), registry.file_name()) {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(real) = fs::canonicalize(parent) {
            return real.join(name);
        }
    }
    normalize_lexically(registry)
}

/// The project root a mission's `--features` registry file belongs to,
/// from where the file really is ([`real_registry_path`]): the directory
/// holding `docs/feature-map/registry.json` when the file sits at that
/// conventional place, else the nearest ancestor that is a repository root
/// (holds `.git`), else the registry's own directory.
pub fn project_root_for_registry(registry: &Path) -> PathBuf {
    let registry = real_registry_path(registry);
    let registry = registry.as_path();
    let conventional: PathBuf = DEFAULT_REGISTRY_REL.split('/').collect();
    if registry.ends_with(&conventional) {
        let depth = conventional.components().count();
        if let Some(root) = registry.ancestors().nth(depth) {
            return if root.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                root.to_path_buf()
            };
        }
    }
    let parent = registry
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(parent)
        .to_path_buf()
}

/// Current epoch timestamp in milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Load and parse a [`FeatureRegistry`] JSON file.
pub fn load_registry(path: &Path) -> Result<FeatureRegistry, String> {
    let raw = fs::read_to_string(path)
        .map_err(|error| format!("read feature registry {}: {error}", path.display()))?;
    let registry: FeatureRegistry = serde_json::from_str(&raw)
        .map_err(|error| format!("parse feature registry {}: {error}", path.display()))?;
    if registry.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported feature registry schema_version {} in {} (expected {SCHEMA_VERSION})",
            registry.schema_version,
            path.display()
        ));
    }
    Ok(registry)
}

/// Atomically persist a [`FeatureRegistry`] via sibling temp file + `sync_all`
/// + `rename`.
pub fn save_registry_atomic(path: &Path, registry: &FeatureRegistry) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create registry dir {}: {error}", parent.display()))?;
    }
    let mut payload = serde_json::to_string_pretty(registry)
        .map_err(|error| format!("serialize feature registry: {error}"))?;
    payload.push('\n');

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("registry.json");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{nanos}", std::process::id()));

    let write_res = (|| -> Result<(), String> {
        let mut file = File::create(&tmp_path)
            .map_err(|error| format!("create temp registry {}: {error}", tmp_path.display()))?;
        file.write_all(payload.as_bytes())
            .map_err(|error| format!("write temp registry {}: {error}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync temp registry {}: {error}", tmp_path.display()))?;
        drop(file);
        fs::rename(&tmp_path, path).map_err(|error| {
            format!(
                "rename temp registry {} -> {}: {error}",
                tmp_path.display(),
                path.display()
            )
        })
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

/// Whether the feature registry can be saved in place by
/// [`save_registry_atomic`], probed against the real filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryWritability {
    /// `true` when both the registry file (if present) and the directory that
    /// holds it accept writes.
    pub writable: bool,
    /// Human-readable explanation when `writable` is `false`.
    pub read_only_reason: Option<String>,
}

/// Probe whether `registry_path` can be atomically rewritten.
///
/// An atomic save creates a sibling temp file and renames it over the
/// registry, so the parent directory must accept new files; the registry
/// file itself must also be writable (a deployed controller tree is made
/// read-only with `chmod a-w` on every file and directory). The probe opens
/// the registry for writing without truncating it and creates then removes a
/// uniquely named probe file beside it: it never changes the registry.
pub fn probe_registry_writable(registry_path: &Path) -> RegistryWritability {
    let read_only = |reason: String| RegistryWritability {
        writable: false,
        read_only_reason: Some(reason),
    };
    if registry_path.exists()
        && let Err(error) = OpenOptions::new().write(true).open(registry_path)
    {
        return read_only(format!(
            "the feature registry {} is read-only ({}); this looks like a deployed controller \
             tree, so associations cannot be saved here: associate files in a writable \
             checkout and deploy again",
            registry_path.display(),
            error.kind()
        ));
    }
    // The nearest existing directory that will hold the registry (a save
    // creates missing parents first).
    let mut dir = match registry_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    while !dir.is_dir() {
        match dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => dir = parent.to_path_buf(),
            _ => {
                dir = PathBuf::from(".");
                break;
            }
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let probe = dir.join(format!(
        // `.tmp` keeps a concurrent scan from ever listing it as a wild file.
        ".registry-write-probe.{}.{nanos}.tmp",
        std::process::id()
    ));
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            RegistryWritability {
                writable: true,
                read_only_reason: None,
            }
        }
        Err(error) => read_only(format!(
            "the directory {} holding the feature registry is read-only ({}), and an atomic \
             save needs to create a temp file beside the registry; this looks like a deployed \
             controller tree, so associations cannot be saved here: associate files in a \
             writable checkout and deploy again",
            dir.display(),
            error.kind()
        )),
    }
}

/// Query git-tracked files inside `root`, if `root` is inside a git working
/// tree. Non-git fixture directories (and git-archive exports) return an empty
/// tracked set without panicking.
fn git_tracked_files(root: &Path) -> BTreeSet<String> {
    let output = crate::sources::controller_git()
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output();
    let Ok(output) = output else {
        return BTreeSet::new();
    };
    if !output.status.success() {
        return BTreeSet::new();
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|slice| !slice.is_empty())
        .filter_map(|slice| std::str::from_utf8(slice).ok())
        .map(|s| s.replace('\\', "/"))
        .collect()
}

/// Walk ONLY inside `root` without following symlinks, refusing any symlink
/// that escapes `root`, and never reading contents of excluded or secret files.
pub fn collect_inventory(
    root: &Path,
    boundary: &BoundaryPolicy,
) -> Result<Vec<InventoryEntry>, String> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("resolve project root {}: {error}", root.display()))?;
    if !canonical_root.is_dir() {
        return Err(format!(
            "project root {} is not a directory",
            canonical_root.display()
        ));
    }

    let tracked = git_tracked_files(&canonical_root);
    let mut entries = Vec::new();
    walk_dir(
        &canonical_root,
        &canonical_root,
        "",
        boundary,
        &tracked,
        &mut entries,
    )?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn walk_dir(
    canonical_root: &Path,
    dir: &Path,
    rel_prefix: &str,
    boundary: &BoundaryPolicy,
    tracked: &BTreeSet<String>,
    out: &mut Vec<InventoryEntry>,
) -> Result<(), String> {
    let read_dir =
        fs::read_dir(dir).map_err(|error| format!("read directory {}: {error}", dir.display()))?;

    let mut children: Vec<PathBuf> = Vec::new();
    for item in read_dir {
        let item = item.map_err(|error| format!("read entry in {}: {error}", dir.display()))?;
        children.push(item.path());
    }
    children.sort();

    for child_path in children {
        let Some(name_os) = child_path.file_name() else {
            continue;
        };
        let Some(name) = name_os.to_str() else {
            continue;
        };
        let rel = if rel_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{rel_prefix}/{name}")
        };

        let Ok(meta) = fs::symlink_metadata(&child_path) else {
            continue;
        };
        let file_type = meta.file_type();

        if file_type.is_symlink() {
            // Never follow symlinks escaping the project root, and never crawl
            // symlinked directories.
            match fs::canonicalize(&child_path) {
                Ok(resolved) => {
                    if !resolved.starts_with(canonical_root) {
                        continue;
                    }
                    if resolved.is_file() {
                        let is_excluded = match_exclusion(boundary, &rel).is_some();
                        out.push(InventoryEntry {
                            path: rel.clone(),
                            tracked: !is_excluded && tracked.contains(&rel),
                            size_bytes: if is_excluded { 0 } else { meta.len() },
                            sha256: None,
                        });
                    }
                }
                Err(_) => {
                    // Broken symlink: record only if git-tracked (never traversed).
                    if tracked.contains(&rel) {
                        let is_excluded = match_exclusion(boundary, &rel).is_some();
                        out.push(InventoryEntry {
                            path: rel,
                            tracked: !is_excluded,
                            size_bytes: if is_excluded { 0 } else { meta.len() },
                            sha256: None,
                        });
                    }
                }
            }
            continue;
        }

        if file_type.is_dir() {
            if match_directory_exclusion(boundary, &rel).is_some() {
                // Report excluded subtrees (e.g. .git/, target/, node_modules/,
                // secrets/, rsi/.gate-owner/) once without ever calling read_dir inside.
                out.push(InventoryEntry {
                    path: format!("{rel}/"),
                    tracked: false,
                    size_bytes: 0,
                    sha256: None,
                });
                continue;
            }
            walk_dir(canonical_root, &child_path, &rel, boundary, tracked, out)?;
            continue;
        }

        if file_type.is_file() {
            let is_excluded = match_exclusion(boundary, &rel).is_some();
            out.push(InventoryEntry {
                path: rel.clone(),
                tracked: !is_excluded && tracked.contains(&rel),
                size_bytes: if is_excluded { 0 } else { meta.len() },
                sha256: None,
            });
        }
    }

    Ok(())
}

/// Scan `root` against `registry` and return a complete [`ScanReport`].
pub fn scan_project_root(root: &Path, registry: &FeatureRegistry) -> Result<ScanReport, String> {
    let inventory = collect_inventory(root, &registry.boundary)?;
    Ok(scan_coverage(registry, &inventory, now_ms()))
}

/// Associate `path` with `feature_ids`, save the updated registry atomically,
/// and return both the [`AssociationUpdate`] and refreshed [`ScanReport`].
pub fn associate_and_save(
    root: &Path,
    registry_path: &Path,
    path: &str,
    feature_ids: &[String],
) -> Result<(AssociationUpdate, ScanReport), String> {
    let mut registry = load_registry(registry_path)?;
    let inventory = collect_inventory(root, &registry.boundary)?;
    let update = associate_file(&mut registry, path, feature_ids, &inventory)?;
    save_registry_atomic(registry_path, &registry)?;
    let report = scan_coverage(&registry, &inventory, now_ms());
    Ok((update, report))
}

/// Build the JSON payload served by `GET /api/features/coverage` and
/// `POST /api/features/scan`.
pub fn api_coverage_payload(root: &Path, registry_path: &Path) -> Result<Value, String> {
    let registry = load_registry(registry_path)?;
    let report = scan_project_root(root, &registry)?;
    Ok(build_api_payload(&registry, &report, root, registry_path))
}

/// Build the JSON payload served by `POST /api/features/associate`.
pub fn api_associate_payload(
    root: &Path,
    registry_path: &Path,
    path: &str,
    feature_ids: &[String],
) -> Result<Value, String> {
    let (update, report) = associate_and_save(root, registry_path, path, feature_ids)?;
    let registry = load_registry(registry_path)?;
    let mut payload = build_api_payload(&registry, &report, root, registry_path);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("update".to_string(), json!(update));
    }
    Ok(payload)
}

fn build_api_payload(
    registry: &FeatureRegistry,
    report: &ScanReport,
    root: &Path,
    registry_path: &Path,
) -> Value {
    let features: Vec<Value> = registry
        .features
        .iter()
        .map(|node| {
            let active_files = report
                .bidirectional_index
                .feature_to_files
                .get(&node.id)
                .map_or(0, Vec::len);
            json!({
                "id": node.id,
                "title": node.title,
                "kind": node.kind,
                "parent": node.parent,
                "summary": node.summary,
                "status_note": node.status_note,
                "file_count": active_files,
                "relations": node.relations,
                "provenance": node.provenance,
            })
        })
        .collect();
    let writability = probe_registry_writable(registry_path);
    json!({
        "root": root.display().to_string(),
        "registry_path": registry_path.display().to_string(),
        "registry_writable": writability.writable,
        "read_only_reason": writability.read_only_reason,
        "report": report,
        "features": features,
    })
}

/// The usage of `spawn features`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn features scan|check [--root <dir>] [--registry <file>] [--json]\n       \
                                spawn features associate --path <file> --feature <id> [--feature <id>]... \
                                [--root <dir>] [--registry <file>] [--json]\n       \
                                spawn features graph [--root <dir>] [--registry <file>] [--level file|unit|feature] \
                                [--from <id|path>] [--to <id|path>] [--feature <id>] [--json]";

/// Entrypoint for `spawn features <scan|associate|check|graph>`.
pub fn command(args: &[String]) -> Result<i32, String> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(USAGE.to_string());
    };
    match sub.as_str() {
        "scan" => cmd_scan(rest, false),
        "check" => cmd_scan(rest, true),
        "associate" => cmd_associate(rest),
        "graph" => cmd_graph(rest),
        other => Err(format!(
            "unknown features subcommand {other:?}; expected scan, associate, check, or graph"
        )),
    }
}

struct CommonOpts {
    root: PathBuf,
    registry: PathBuf,
    json: bool,
}

fn parse_common_opts(args: &[String]) -> Result<CommonOpts, String> {
    let mut root: Option<PathBuf> = None;
    let mut registry: Option<PathBuf> = None;
    let mut json_out = false;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--root" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "--root requires a directory path".to_string())?;
                root = Some(PathBuf::from(value));
            }
            "--registry" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "--registry requires a file path".to_string())?;
                registry = Some(PathBuf::from(value));
            }
            "--json" => json_out = true,
            other => return Err(format!("unexpected argument {other:?}")),
        }
        idx += 1;
    }
    let root = root.unwrap_or_else(default_root);
    let registry = registry.unwrap_or_else(|| default_registry_path(&root));
    Ok(CommonOpts {
        root,
        registry,
        json: json_out,
    })
}

fn cmd_scan(args: &[String], fail_on_drift: bool) -> Result<i32, String> {
    let opts = parse_common_opts(args)?;
    let registry = load_registry(&opts.registry)?;
    let report = scan_project_root(&opts.root, &registry)?;

    if opts.json {
        let text = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("serialize scan report: {error}"))?;
        println!("{text}");
    } else {
        print_human_report(&report);
    }

    if fail_on_drift && !report.is_clean() {
        Ok(1)
    } else {
        Ok(0)
    }
}

fn cmd_associate(args: &[String]) -> Result<i32, String> {
    let mut root: Option<PathBuf> = None;
    let mut registry: Option<PathBuf> = None;
    let mut path: Option<String> = None;
    let mut features: Vec<String> = Vec::new();
    let mut json_out = false;

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--root" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "--root requires a directory path".to_string())?;
                root = Some(PathBuf::from(value));
            }
            "--registry" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "--registry requires a file path".to_string())?;
                registry = Some(PathBuf::from(value));
            }
            "--path" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "--path requires a repository-relative path".to_string())?;
                path = Some(value.clone());
            }
            "--feature" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "--feature requires a feature id".to_string())?;
                features.push(value.clone());
            }
            "--json" => json_out = true,
            other => return Err(format!("unexpected argument {other:?}")),
        }
        idx += 1;
    }

    let path = path.ok_or_else(|| "spawn features associate requires --path <file>".to_string())?;
    if features.is_empty() {
        return Err("spawn features associate requires at least one --feature <id>".to_string());
    }
    let root = root.unwrap_or_else(default_root);
    let registry_path = registry.unwrap_or_else(|| default_registry_path(&root));
    let (update, report) = associate_and_save(&root, &registry_path, &path, &features)?;

    if json_out {
        let text = serde_json::to_string_pretty(&json!({
            "update": update,
            "counts": report.counts,
        }))
        .map_err(|error| format!("serialize associate output: {error}"))?;
        println!("{text}");
    } else {
        println!(
            "associated {} -> [{}] (added: [{}]; remaining wild files: {})",
            update.path,
            update.all_associated_features.join(", "),
            update.added_to_features.join(", "),
            report.counts.unassociated_wild,
        );
    }
    Ok(0)
}

fn print_human_report(report: &ScanReport) {
    let c = &report.counts;
    println!(
        "feature coverage ({}) — in_scope={} covered={} wild={} stale={} multi_feature={} excluded={} orphans={} invalid_relations={}",
        report.initiative_id,
        c.in_scope,
        c.covered,
        c.unassociated_wild,
        c.stale_references,
        c.multi_feature_files,
        c.explicit_exclusions,
        c.orphan_features,
        c.invalid_relations,
    );
    if !report.unassociated_files.is_empty() {
        println!(
            "unassociated (wild) files ({}):",
            report.unassociated_files.len()
        );
        for finding in &report.unassociated_files {
            println!("  - {} ({})", finding.path, finding.reason);
        }
    }
    if !report.stale_references.is_empty() {
        println!("stale references ({}):", report.stale_references.len());
        for stale in &report.stale_references {
            println!(
                "  - {} referenced by [{}]",
                stale.path,
                stale.referencing_features.join(", ")
            );
        }
    }
    if !report.consistency.is_clean() {
        for err in &report.consistency.duplicate_feature_ids {
            println!("  ! duplicate feature id: {err}");
        }
        for err in &report.consistency.dangling_relations {
            println!("  ! dangling relation: {err}");
        }
        for err in &report.consistency.self_links {
            println!("  ! self link: {err}");
        }
        for err in &report.consistency.cycle_errors {
            println!("  ! cycle: {err}");
        }
        for err in &report.consistency.orphan_features {
            println!("  ! orphan feature: {err}");
        }
        for err in &report.consistency.invalid_paths {
            println!("  ! invalid path: {err}");
        }
    }
}

/// The usage of `spawn features graph`.
const GRAPH_USAGE: &str = "usage: spawn features graph [--root <dir>] [--registry <file>] [--level file|unit|feature] [--from <id|path>] [--to <id|path>] [--feature <id>] [--json]";

/// The value flags `spawn features graph` takes (`--json` is a switch).
const GRAPH_FLAGS: [&str; 6] = ["root", "registry", "level", "from", "to", "feature"];

/// What `spawn features graph` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphLevel {
    /// File edges.
    File,
    /// Edges between package units, against their manifests.
    Unit,
    /// File edges projected onto the registry's features, with the
    /// dependency-truth findings.
    Feature,
}

impl GraphLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Unit => "unit",
            Self::Feature => "feature",
        }
    }
}

#[derive(Debug, Clone)]
struct GraphOpts {
    root: PathBuf,
    registry: PathBuf,
    level: GraphLevel,
    from: Option<String>,
    to: Option<String>,
    feature: Option<String>,
    json: bool,
}

fn parse_graph_opts(args: &[String]) -> Result<GraphOpts, String> {
    let flags = crate::Flags::parse_with_switches(args, &["json"])
        .map_err(|error| format!("{error}\n{GRAPH_USAGE}"))?;
    if let Some(unknown) = flags.names().find(|name| !GRAPH_FLAGS.contains(name)) {
        return Err(format!("unexpected flag --{unknown}\n{GRAPH_USAGE}"));
    }
    let level = match flags.get("level").as_deref() {
        None | Some("feature") => GraphLevel::Feature,
        Some("file") => GraphLevel::File,
        Some("unit") => GraphLevel::Unit,
        Some(other) => {
            return Err(format!(
                "--level {other:?} is not file, unit or feature\n{GRAPH_USAGE}"
            ));
        }
    };
    let root = flags.get("root").map_or_else(default_root, PathBuf::from);
    let registry = flags
        .get("registry")
        .map_or_else(|| default_registry_path(&root), PathBuf::from);
    Ok(GraphOpts {
        root,
        registry,
        level,
        from: flags.get("from"),
        to: flags.get("to"),
        feature: flags.get("feature"),
        json: flags.switch("json"),
    })
}

/// Today's UTC date, `YYYY-MM-DD`: when a waiver stops applying.
fn utc_today() -> String {
    let stamp = crate::backup::Stamp::at(now_ms()).text();
    format!("{}-{}-{}", &stamp[0..4], &stamp[4..6], &stamp[6..8])
}

/// The code graph of `root`'s in-scope files under `boundary`, and how
/// long building it took. Only the in-scope files
/// [`spawn_codegraph::wants`] accepts are read (Rust sources and
/// `Cargo.toml`; TypeScript / JavaScript sources and `package.json`,
/// `tsconfig*.json`, `jsconfig*.json`), each at most
/// [`spawn_codegraph::MAX_SOURCE_BYTES`]; a file not read is listed as
/// `skipped`. Every in-scope path, read or not, is passed as the inventory
/// references may resolve to (assets, `.vue` files listed `unsupported`).
pub fn build_code_graph(
    root: &Path,
    boundary: &BoundaryPolicy,
) -> Result<(CodeGraph, u128), String> {
    let inventory = collect_inventory(root, boundary)?;
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("resolve project root {}: {error}", root.display()))?;
    let mut in_scope = BTreeSet::new();
    let mut sources = BTreeMap::new();
    let mut skipped = Vec::new();
    for entry in &inventory {
        if entry.path.ends_with('/') || match_exclusion(boundary, &entry.path).is_some() {
            continue;
        }
        in_scope.insert(entry.path.clone());
        if !spawn_codegraph::wants(&entry.path) {
            continue;
        }
        match read_source(&canonical_root, &entry.path) {
            Ok(text) => {
                sources.insert(entry.path.clone(), text);
            }
            Err(message) => skipped.push(Diagnostic {
                code: DiagnosticCode::Skipped,
                path: entry.path.clone(),
                line: None,
                message,
            }),
        }
    }
    let started = Instant::now();
    let mut graph = spawn_codegraph::build(&in_scope, &sources);
    let build_ms = started.elapsed().as_millis();
    graph.diagnostics.extend(skipped);
    graph.diagnostics.sort();
    Ok((graph, build_ms))
}

/// The text of the in-scope source `rel`, or why it is not read. Never
/// through a symlink (the link's own path passed the boundary, not its
/// target's): the file itself and every directory on its way must be what
/// the path says, it is opened with `O_NOFOLLOW`, and the size limit is
/// checked on the opened file, then enforced while reading.
fn read_source(canonical_root: &Path, rel: &str) -> Result<String, String> {
    use std::io::Read;
    use std::os::fd::OwnedFd;

    use rustix::fs::{Mode, OFlags};

    let limit = spawn_codegraph::MAX_SOURCE_BYTES;
    let path = canonical_root.join(rel);
    let meta =
        fs::symlink_metadata(&path).map_err(|error| format!("not read ({})", error.kind()))?;
    if meta.file_type().is_symlink() {
        return Err("a symlink: not followed".to_string());
    }
    if fs::canonicalize(&path).ok().as_deref() != Some(path.as_path()) {
        return Err("a directory on its path is a symlink: not followed".to_string());
    }
    let descriptor: OwnedFd = rustix::fs::open(
        &path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| format!("not opened ({error}): not followed"))?;
    let file = File::from(descriptor);
    let opened = file
        .metadata()
        .map_err(|error| format!("not read ({})", error.kind()))?;
    if !opened.is_file() {
        return Err("not a regular file".to_string());
    }
    let over = |bytes: u64| format!("{bytes} bytes, over the {limit} byte limit");
    if opened.len() > limit {
        return Err(over(opened.len()));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("not read ({})", error.kind()))?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if read > limit {
        return Err(over(read));
    }
    String::from_utf8(bytes).map_err(|_| "not UTF-8".to_string())
}

/// The numbers of a code graph. No wall-clock figure: two runs on an
/// unchanged tree print the same bytes.
fn graph_summary(graph: &CodeGraph) -> Value {
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_scope: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_precision: BTreeMap<&str, usize> = BTreeMap::new();
    let mut languages: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for info in graph.files.values() {
        languages.entry(info.language.as_str()).or_default().0 += 1;
    }
    for edge in &graph.edges {
        *by_kind.entry(edge.kind.as_str()).or_default() += 1;
        *by_scope.entry(edge.scope.as_str()).or_default() += 1;
        *by_precision.entry(edge.precision.as_str()).or_default() += 1;
        if let Some(info) = graph.files.get(&edge.from) {
            languages.entry(info.language.as_str()).or_default().1 += 1;
        }
    }
    let mut diagnostic_counts: BTreeMap<String, usize> = BTreeMap::new();
    for diagnostic in &graph.diagnostics {
        let code = serde_json::to_value(diagnostic.code)
            .ok()
            .and_then(|code| code.as_str().map(str::to_string))
            .unwrap_or_default();
        *diagnostic_counts.entry(code).or_default() += 1;
    }
    let structural = graph
        .edges
        .iter()
        .filter(|edge| edge.kind.is_structural())
        .count();
    json!({
        "files_analyzed": graph.files.len(),
        "files_parsed": graph.files.values().filter(|info| info.parsed).count(),
        "edges": graph.edges.len(),
        "structural_edges": structural,
        "dependency_edges": graph.edges.len() - structural,
        "external_imports": graph.external_imports,
        "unresolved": diagnostic_counts.get("unresolved").copied().unwrap_or(0),
        "unresolved_items": diagnostic_counts.get("unresolved_item").copied().unwrap_or(0),
        "reexport_too_deep": diagnostic_counts.get("reexport_too_deep").copied().unwrap_or(0),
        // Edges to a module whose macro call only mentions the name.
        "macro_guesses": by_precision.get("macro").copied().unwrap_or(0),
        "by_kind": by_kind,
        "by_scope": by_scope,
        "by_precision": by_precision,
        "languages": languages
            .iter()
            .map(|(language, (files, edges))| (*language, json!({ "files": files, "edges": edges })))
            .collect::<BTreeMap<_, _>>(),
        "units": graph.units,
        "diagnostic_counts": diagnostic_counts,
        "diagnostics": graph.diagnostics,
        "limits": spawn_codegraph::LIMITS,
    })
}

/// Whether one end of an edge matches a `--from` / `--to` value: a feature
/// id (any of `owners` in its subtree), else a path or a directory prefix.
fn end_matches(
    spec: Option<&str>,
    owners: &BTreeSet<String>,
    path: &str,
    index: Option<&FeatureIndex>,
) -> bool {
    let Some(spec) = spec else {
        return true;
    };
    if let Some(index) = index
        && index.contains(spec)
    {
        let subtree = index.subtree(spec);
        return owners.iter().any(|owner| subtree.contains(owner));
    }
    let spec = spec.trim_end_matches('/');
    path == spec || path.starts_with(&format!("{spec}/"))
}

/// One line for a file edge: `from:line -> to (kind, scope[, via X])`.
fn edge_line(edge: &FileEdge) -> String {
    let via = edge
        .via
        .as_deref()
        .map_or_else(String::new, |via| format!(", via {via}"));
    format!(
        "{}:{} -> {} ({}, {}{via})",
        edge.from,
        edge.line,
        edge.to,
        edge.kind.as_str(),
        edge.scope.as_str()
    )
}

/// The feature level: feature edges and findings, filtered by `--from`,
/// `--to` and `--feature`.
struct FeatureView {
    edges: Vec<FeatureEdge>,
    truth: TruthReport,
}

fn feature_view(opts: &GraphOpts, graph: &CodeGraph, registry: &FeatureRegistry) -> FeatureView {
    let index = FeatureIndex::new(registry);
    let projected = feature_deps::project(graph, registry);
    let mut truth = feature_deps::truth(graph, registry, &TruthPolicy::default(), &utc_today());
    let focus = opts.feature.as_ref().map(|id| index.subtree(id));
    let touches = |ids: &[&Vec<String>]| {
        focus.as_ref().is_none_or(|focus| {
            ids.iter()
                .any(|side| side.iter().any(|id| focus.contains(id)))
        })
    };
    let mut edges = Vec::new();
    for mut edge in projected.edges {
        if !touches(&[&edge.from_any_of, &edge.to_any_of]) {
            continue;
        }
        let from_owners: BTreeSet<String> = edge.from_any_of.iter().cloned().collect();
        let to_owners: BTreeSet<String> = edge.to_any_of.iter().cloned().collect();
        edge.evidence.retain(|file_edge| {
            end_matches(
                opts.from.as_deref(),
                &from_owners,
                &file_edge.from,
                Some(&index),
            ) && end_matches(opts.to.as_deref(), &to_owners, &file_edge.to, Some(&index))
        });
        if !edge.evidence.is_empty() {
            edges.push(edge);
        }
    }
    truth
        .findings
        .retain(|finding| touches(&[&finding.from_any_of, &finding.to_any_of, &finding.features]));
    // The counts describe the findings shown.
    truth.counts = TruthCounts::of(&truth.findings);
    FeatureView { edges, truth }
}

/// Edges between package units, each with whether the source unit's
/// manifest declares the target.
fn unit_edges(graph: &CodeGraph) -> Vec<Value> {
    let declared: BTreeMap<&str, &Vec<String>> = graph
        .units
        .iter()
        .map(|unit| (unit.id.as_str(), &unit.declared_units))
        .collect();
    let unit_of = |path: &str| graph.files.get(path).and_then(|info| info.unit.as_deref());
    let mut counts: BTreeMap<(&str, &str), (usize, usize)> = BTreeMap::new();
    for edge in graph.edges.iter().filter(|edge| !edge.kind.is_structural()) {
        let (Some(from), Some(to)) = (unit_of(&edge.from), unit_of(&edge.to)) else {
            continue;
        };
        if from != to {
            let count = counts.entry((from, to)).or_default();
            count.0 += 1;
            if edge.scope == Scope::Prod {
                count.1 += 1;
            }
        }
    }
    counts
        .into_iter()
        .map(|((from, to), (edges, prod_edges))| {
            json!({
                "from": from,
                "to": to,
                "edges": edges,
                "prod_edges": prod_edges,
                "declared": declared.get(from).is_some_and(|units| units.iter().any(|unit| unit == to)),
            })
        })
        .collect()
}

/// The registry `graph` reads: required at the feature level, optional
/// (for its boundary and owners) at the file and unit levels.
fn graph_registry(opts: &GraphOpts) -> Result<Option<FeatureRegistry>, String> {
    if opts.level == GraphLevel::Feature || opts.registry.exists() {
        load_registry(&opts.registry).map(Some)
    } else {
        Ok(None)
    }
}

/// The boundary when no registry gives one: the generic boundary named
/// after the root directory.
fn generic_boundary(root: &Path) -> BoundaryPolicy {
    let name = fs::canonicalize(root)
        .ok()
        .and_then(|real| {
            real.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "project".to_string());
    BoundaryPolicy::default_generic_boundary(&name)
}

/// The JSON `spawn features graph --json` prints, and how long building
/// the graph took (shown only by the human form).
fn graph_payload(opts: &GraphOpts) -> Result<(Value, u128), String> {
    let registry = graph_registry(opts)?;
    if let Some(feature) = &opts.feature {
        let Some(registry) = &registry else {
            return Err(format!(
                "--feature {feature} needs a registry; {} does not exist",
                opts.registry.display()
            ));
        };
        if !FeatureIndex::new(registry).contains(feature) {
            return Err(format!(
                "--feature {feature:?} names no feature of {}",
                opts.registry.display()
            ));
        }
    }
    let boundary = registry
        .as_ref()
        .map_or_else(|| generic_boundary(&opts.root), |r| r.boundary.clone());
    let (graph, build_ms) = build_code_graph(&opts.root, &boundary)?;
    let mut payload = json!({
        "schema_version": 1,
        "root": opts.root.display().to_string(),
        "registry_path": registry.as_ref().map(|_| opts.registry.display().to_string()),
        "level": opts.level.as_str(),
        "feature": opts.feature,
        "graph": graph_summary(&graph),
    });
    let Some(object) = payload.as_object_mut() else {
        return Ok((payload, build_ms));
    };
    match opts.level {
        GraphLevel::File => {
            let index = registry.as_ref().map(FeatureIndex::new);
            let owners = |path: &str| {
                index
                    .as_ref()
                    .map(|index| index.owners(path))
                    .unwrap_or_default()
            };
            let edges: Vec<&FileEdge> = graph
                .edges
                .iter()
                .filter(|edge| {
                    end_matches(
                        opts.from.as_deref(),
                        &owners(&edge.from),
                        &edge.from,
                        index.as_ref(),
                    ) && end_matches(
                        opts.to.as_deref(),
                        &owners(&edge.to),
                        &edge.to,
                        index.as_ref(),
                    )
                })
                .collect();
            object.insert("edges".to_string(), json!(edges));
        }
        GraphLevel::Unit => {
            object.insert("unit_edges".to_string(), json!(unit_edges(&graph)));
        }
        GraphLevel::Feature => {
            let registry =
                registry.ok_or_else(|| "the feature level needs a registry".to_string())?;
            let view = feature_view(opts, &graph, &registry);
            let projection = feature_deps::project(&graph, &registry).summary;
            object.insert("projection".to_string(), json!(projection));
            object.insert(
                "feature_edges".to_string(),
                json!(
                    view.edges
                        .iter()
                        .map(|edge| json!({
                            "from_any_of": edge.from_any_of,
                            "to_any_of": edge.to_any_of,
                            "status": edge.status,
                            "prod_edges": edge.prod_edges,
                            "other_edges": edge.other_edges,
                            "evidence_total": edge.evidence.len(),
                            "evidence": edge.evidence,
                        }))
                        .collect::<Vec<_>>()
                ),
            );
            object.insert("truth".to_string(), json!(view.truth));
            object.insert("mode".to_string(), json!("report"));
        }
    }
    Ok((payload, build_ms))
}

/// `spawn features graph`: read-only. The code graph of the project's
/// in-scope Rust, TypeScript and JavaScript files, at file, unit or feature level, and why the tool
/// says one feature depends on another, down to `file:line`.
fn cmd_graph(args: &[String]) -> Result<i32, String> {
    let opts = parse_graph_opts(args)?;
    let (payload, build_ms) = graph_payload(&opts)?;
    if opts.json {
        let text = serde_json::to_string_pretty(&payload)
            .map_err(|error| format!("serialize graph: {error}"))?;
        println!("{text}");
    } else {
        print_graph_human(&opts, &payload, build_ms);
    }
    Ok(0)
}

fn count(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

fn print_graph_human(opts: &GraphOpts, payload: &Value, build_ms: u128) {
    let graph = &payload["graph"];
    println!(
        "code graph ({}) — files={} parsed={} edges={} dependency={} structural={} units={} external_imports={} unresolved={} unresolved_items={} macro_guesses={} diagnostics={} build_ms={build_ms}",
        opts.level.as_str(),
        count(graph, "files_analyzed"),
        count(graph, "files_parsed"),
        count(graph, "edges"),
        count(graph, "dependency_edges"),
        count(graph, "structural_edges"),
        graph["units"].as_array().map_or(0, Vec::len),
        count(graph, "external_imports"),
        count(graph, "unresolved"),
        count(graph, "unresolved_items"),
        count(graph, "macro_guesses"),
        graph["diagnostics"].as_array().map_or(0, Vec::len),
    );
    let focused = opts.from.is_some() || opts.to.is_some() || opts.feature.is_some();
    let parse_edge = |value: &Value| serde_json::from_value::<FileEdge>(value.clone()).ok();
    match opts.level {
        GraphLevel::File => {
            for edge in payload["edges"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(parse_edge)
            {
                println!("  {}", edge_line(&edge));
            }
        }
        GraphLevel::Unit => {
            for edge in payload["unit_edges"].as_array().into_iter().flatten() {
                println!(
                    "  {} -> {} edges={} prod={} [{}]",
                    edge["from"].as_str().unwrap_or(""),
                    edge["to"].as_str().unwrap_or(""),
                    count(edge, "edges"),
                    count(edge, "prod_edges"),
                    if edge["declared"].as_bool() == Some(true) {
                        "declared"
                    } else {
                        "undeclared"
                    }
                );
            }
        }
        GraphLevel::Feature => print_feature_level(payload, focused, parse_edge),
    }
}

fn print_feature_level(
    payload: &Value,
    focused: bool,
    parse_edge: impl Fn(&Value) -> Option<FileEdge>,
) {
    let projection = &payload["projection"];
    println!(
        "projection — internal={} hierarchy={} unowned={} declared={} implied={} undeclared={}",
        count(projection, "internal"),
        count(projection, "hierarchy"),
        count(projection, "unowned"),
        count(projection, "declared"),
        count(projection, "implied"),
        count(projection, "undeclared"),
    );
    let edges = payload["feature_edges"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    println!("feature edges ({}):", edges.len());
    let ids = id_list;
    for edge in &edges {
        println!(
            "  {} -> {} [{}] prod={} other={}",
            ids(&edge["from_any_of"]),
            ids(&edge["to_any_of"]),
            edge["status"].as_str().unwrap_or(""),
            count(edge, "prod_edges"),
            count(edge, "other_edges"),
        );
        let evidence: Vec<FileEdge> = edge["evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(&parse_edge)
            .collect();
        let shown = if focused { evidence.len() } else { 3 };
        for file_edge in evidence.iter().take(shown) {
            println!("      {}", edge_line(file_edge));
        }
        if evidence.len() > shown {
            println!(
                "      ... {} more: spawn features graph --from {} --to {}",
                evidence.len() - shown,
                edge["from_any_of"][0].as_str().unwrap_or(""),
                edge["to_any_of"][0].as_str().unwrap_or("")
            );
        }
    }
    let truth = &payload["truth"];
    let by_code = &truth["counts"]["by_code"];
    let roles = count(by_code, "kernel_leak") + count(by_code, "entrypoint_imported");
    println!(
        "dependency truth (report) — under_declared={} implied={} test_only={} ghost={} inverted={} unverifiable={} cycles={} roles={} waived={}",
        count(by_code, "under_declared"),
        count(by_code, "implied_only"),
        count(by_code, "test_only_undeclared"),
        count(by_code, "ghost"),
        count(by_code, "inverted"),
        count(by_code, "unverifiable"),
        count(by_code, "static_cycle"),
        roles,
        count(&truth["counts"], "waived"),
    );
    print_findings(truth, &parse_edge);
}

/// `a|b|c` for a JSON list of ids.
fn id_list(value: &Value) -> String {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("|")
}

/// One line per finding, with its first evidence edge and, on a cycle, how
/// many undeclarable pairs it keeps out of `under_declared`.
fn print_findings(truth: &Value, parse_edge: &impl Fn(&Value) -> Option<FileEdge>) {
    let ids = id_list;
    for finding in truth["findings"].as_array().into_iter().flatten() {
        let names = if finding["features"]
            .as_array()
            .is_some_and(|f| !f.is_empty())
        {
            ids(&finding["features"])
        } else {
            format!(
                "{} -> {}",
                ids(&finding["from_any_of"]),
                ids(&finding["to_any_of"])
            )
        };
        let first = finding["evidence"]
            .as_array()
            .and_then(|evidence| evidence.first())
            .and_then(&parse_edge)
            .map_or_else(String::new, |edge| format!(": {}", edge_line(&edge)));
        let suppressed = &finding["suppressed"];
        let first = if suppressed.is_object() {
            format!(
                "{first} [suppressed {} undeclarable pairs, {} file edges]",
                count(suppressed, "total"),
                count(suppressed, "file_edges")
            )
        } else {
            first
        };
        println!(
            "  {} {} {}{first} (edges={})",
            finding["severity"].as_str().unwrap_or(""),
            finding["code"].as_str().unwrap_or(""),
            names,
            count(finding, "evidence_total"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spawn_core::feature_coverage::{FeatureKind, FeatureNode, FeatureProvenance};
    use spawn_core::feature_deps::Finding;

    fn temp_fixture(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "spawn-feature-inventory-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    fn minimal_registry(files: Vec<&str>) -> FeatureRegistry {
        FeatureRegistry {
            schema_version: SCHEMA_VERSION,
            initiative_id: "t559u".to_string(),
            boundary: BoundaryPolicy::default_spawn_boundary(),
            features: vec![
                FeatureNode {
                    id: "domain-core".to_string(),
                    title: "Core domain".to_string(),
                    kind: FeatureKind::Domain,
                    parent: None,
                    summary: "Core domain files".to_string(),
                    status_note: None,
                    files: files.into_iter().map(str::to_string).collect(),
                    relations: Vec::new(),
                    provenance: FeatureProvenance::default(),
                },
                FeatureNode {
                    id: "cap-api".to_string(),
                    title: "API capability".to_string(),
                    kind: FeatureKind::Capability,
                    parent: Some("domain-core".to_string()),
                    summary: "API layer".to_string(),
                    status_note: None,
                    files: Vec::new(),
                    relations: Vec::new(),
                    provenance: FeatureProvenance::default(),
                },
            ],
        }
    }

    #[test]
    fn scans_wild_excluded_and_stale_files_and_associates_atomically() {
        let root = temp_fixture("scan-and-associate");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::create_dir_all(root.join("target/debug")).expect("target");
        fs::create_dir_all(root.join("secrets")).expect("secrets");
        fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").expect("lib.rs");
        fs::write(root.join("src/wild.rs"), "pub fn wild() {}\n").expect("wild.rs");
        fs::write(root.join("target/debug/bin"), "binary").expect("target bin");
        fs::write(root.join("secrets/api.key"), "secret-value").expect("secret");
        fs::write(root.join(".env"), "TOKEN=nope").expect("env");

        let registry_path = root.join("registry.json");
        let reg = minimal_registry(vec!["src/lib.rs", "src/deleted.rs", "registry.json"]);
        save_registry_atomic(&registry_path, &reg).expect("save");

        let loaded = load_registry(&registry_path).expect("load");
        let report = scan_project_root(&root, &loaded).expect("scan");

        assert_eq!(report.counts.in_scope, 3);
        assert_eq!(report.counts.covered, 2);
        assert_eq!(report.counts.unassociated_wild, 1);
        assert_eq!(report.unassociated_files[0].path, "src/wild.rs");
        assert_eq!(report.counts.stale_references, 1);
        assert_eq!(report.stale_references[0].path, "src/deleted.rs");
        // Excluded entries (target/, .env, secrets/) are never in scope or covered.
        assert_eq!(report.counts.explicit_exclusions, 3);
        assert!(
            report
                .excluded_files
                .iter()
                .any(|entry| entry.path == ".env" && entry.category == "secrets")
        );
        assert!(
            report
                .excluded_files
                .iter()
                .any(|entry| entry.path == "secrets/" && entry.category == "secrets")
        );
        assert!(
            report
                .excluded_files
                .iter()
                .any(|entry| entry.path == "target/" && entry.category == "build-cache")
        );

        // Associate the wild file to two features (many-to-many) and verify persistence.
        let (update, after) = associate_and_save(
            &root,
            &registry_path,
            "src/wild.rs",
            &["domain-core".to_string(), "cap-api".to_string()],
        )
        .expect("associate");
        assert_eq!(
            update.all_associated_features,
            vec!["cap-api".to_string(), "domain-core".to_string()]
        );
        assert_eq!(after.counts.unassociated_wild, 0);
        assert_eq!(after.counts.multi_feature_files, 1);
        assert!(root.join("src/wild.rs").exists(), "never deletes files");

        // Refuses associating an excluded secret file.
        let err = associate_and_save(&root, &registry_path, ".env", &["domain-core".to_string()])
            .expect_err("must refuse excluded file");
        assert!(err.contains("excluded"), "{err}");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn coverage_payload_reports_a_writable_registry() {
        let root = temp_fixture("payload-writable");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").expect("lib.rs");
        let registry_path = root.join("docs/feature-map/registry.json");
        save_registry_atomic(&registry_path, &minimal_registry(vec!["src/lib.rs"])).expect("save");
        let payload = api_coverage_payload(&root, &registry_path).expect("payload");
        let leftovers: Vec<_> = fs::read_dir(root.join("docs/feature-map"))
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(payload["registry_writable"], json!(true));
        assert_eq!(payload["read_only_reason"], Value::Null);
        assert_eq!(
            leftovers,
            vec!["registry.json".to_string()],
            "probe leaves nothing"
        );
    }

    /// A deployed controller tree is `chmod a-w`: the payload says the
    /// registry is read-only (with a reason) instead of failing later.
    #[cfg(unix)]
    #[test]
    fn coverage_payload_reports_a_read_only_registry_and_its_reason() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_fixture("payload-read-only");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").expect("lib.rs");
        let dir = root.join("docs/feature-map");
        let registry_path = dir.join("registry.json");
        save_registry_atomic(&registry_path, &minimal_registry(vec!["src/lib.rs"])).expect("save");

        // Only the directory read-only: the temp-file rename is what fails.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).expect("chmod dir");
        let dir_only = probe_registry_writable(&registry_path);
        // Registry file read-only as well, as deploy/controller.sh leaves it.
        fs::set_permissions(&registry_path, fs::Permissions::from_mode(0o444)).expect("chmod file");
        let payload = api_coverage_payload(&root, &registry_path);
        fs::set_permissions(&registry_path, fs::Permissions::from_mode(0o644)).expect("restore");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("restore dir");
        // Directory writable again but the file not: still read-only.
        fs::set_permissions(&registry_path, fs::Permissions::from_mode(0o444)).expect("chmod file");
        let file_only = probe_registry_writable(&registry_path);
        fs::set_permissions(&registry_path, fs::Permissions::from_mode(0o644)).expect("restore");
        let _ = fs::remove_dir_all(&root);

        if dir_only.writable {
            // Running as root ignores permission bits: nothing to observe.
            eprintln!("skipping: permission bits are not enforced for this user");
            return;
        }
        let dir_reason = dir_only.read_only_reason.expect("dir reason");
        assert!(
            dir_reason.contains("temp file beside the registry"),
            "{dir_reason}"
        );
        assert!(dir_reason.contains("deployed controller"), "{dir_reason}");
        assert!(
            !dir_reason.contains("os error"),
            "no raw OS error: {dir_reason}"
        );

        let payload = payload.expect("a read-only registry still scans");
        assert_eq!(payload["registry_writable"], json!(false));
        let reason = payload["read_only_reason"].as_str().expect("reason string");
        assert!(reason.contains("read-only"), "{reason}");
        assert!(payload["report"]["counts"]["covered"].as_u64() == Some(1));

        assert!(!file_only.writable);
        let file_reason = file_only.read_only_reason.expect("file reason");
        assert!(file_reason.contains("registry.json"), "{file_reason}");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_escaping_project_root_and_surfaces_in_root_file_symlinks() {
        use std::os::unix::fs::symlink;

        let root = temp_fixture("symlink-root");
        let outside = temp_fixture("symlink-outside");
        fs::write(outside.join("outside.txt"), "do-not-crawl").expect("outside file");
        fs::write(root.join("inside.txt"), "ok").expect("inside file");
        fs::create_dir_all(root.join("sub")).expect("sub dir");
        fs::write(root.join("sub/nested.txt"), "nested").expect("nested");
        symlink(&outside, root.join("escape_dir")).expect("symlink dir");
        symlink(outside.join("outside.txt"), root.join("escape_file.txt")).expect("symlink file");
        symlink(root.join("sub"), root.join("in_root_dir_link")).expect("in-root dir link");
        symlink(root.join("inside.txt"), root.join("in_root_file_link.txt"))
            .expect("in-root file link");

        let inv =
            collect_inventory(&root, &BoundaryPolicy::default_spawn_boundary()).expect("inventory");
        let paths: Vec<&str> = inv.iter().map(|e| e.path.as_str()).collect();
        // Escaping symlinks and directory symlinks are never crawled; in-root file symlink is observed.
        assert_eq!(
            paths,
            vec!["in_root_file_link.txt", "inside.txt", "sub/nested.txt"]
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn excluded_secrets_and_runtime_dirs_are_never_crawled() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_fixture("no-crawl-secrets");
        let secrets_dir = root.join("secrets");
        let gate_owner_dir = root.join("rsi/.gate-owner");
        fs::create_dir_all(&secrets_dir).expect("secrets");
        fs::create_dir_all(&gate_owner_dir).expect("gate-owner");
        fs::write(secrets_dir.join("prod.key"), "top-secret").expect("key");
        fs::write(gate_owner_dir.join("lease"), "pid=123").expect("lease");
        fs::write(root.join("lib.rs"), "pub fn ok() {}\n").expect("lib.rs");

        // Remove read/exec permissions from secrets/ so any read_dir attempt would fail with EACCES.
        fs::set_permissions(&secrets_dir, fs::Permissions::from_mode(0o000)).expect("chmod 000");

        let inv_res = collect_inventory(&root, &BoundaryPolicy::default_spawn_boundary());

        // Restore permissions before asserting so cleanup always succeeds.
        let _ = fs::set_permissions(&secrets_dir, fs::Permissions::from_mode(0o700));
        let inv = inv_res.expect("walk_dir must collapse secrets/ without calling read_dir");
        let paths: Vec<&str> = inv.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["lib.rs", "rsi/.gate-owner/", "secrets/"]);

        let reg = minimal_registry(vec!["lib.rs"]);
        let report = scan_coverage(&reg, &inv, 1000);
        assert!(report.is_clean());
        assert_eq!(report.counts.in_scope, 1);
        assert_eq!(report.counts.covered, 1);
        assert_eq!(report.counts.explicit_exclusions, 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_worktree_dot_git_file_and_gitignore_temps_are_excluded_not_wild() {
        let root = temp_fixture("worktree-exclusions");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::write(
            root.join(".git"),
            "gitdir: /tmp/fake-bare/worktrees/audit\n",
        )
        .expect(".git file");
        fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").expect("lib.rs");
        fs::write(root.join("src/lib.rs.bk"), "backup").expect("rs.bk");
        fs::write(root.join("Cargo.lock.bak"), "backup").expect("lock.bak");
        fs::write(root.join("backup.gpg"), "ciphertext").expect("gpg");
        fs::write(root.join("download.part"), "partial").expect("part");
        fs::write(root.join("notes.txt~"), "tilde").expect("tilde");

        let reg = minimal_registry(vec!["src/lib.rs"]);
        let report = scan_project_root(&root, &reg).expect("scan");
        assert!(report.is_clean());
        assert_eq!(report.counts.in_scope, 1);
        assert_eq!(report.counts.covered, 1);
        assert_eq!(report.counts.unassociated_wild, 0);
        assert_eq!(report.counts.explicit_exclusions, 6);
        assert!(
            report
                .excluded_files
                .iter()
                .any(|e| e.path == ".git" && e.rule_id == "vcs-metadata")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cli_check_and_associate_exit_codes_and_non_destructive_guarantee() {
        let root = temp_fixture("cli-check-flow");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::write(root.join("src/lib.rs"), "pub fn lib() {}\n").expect("lib.rs");
        let registry_path = root.join("registry.json");
        save_registry_atomic(
            &registry_path,
            &minimal_registry(vec!["src/lib.rs", "registry.json"]),
        )
        .expect("save");

        let root_s = root.display().to_string();
        let reg_s = registry_path.display().to_string();

        // 1. Clean repo -> check exits 0.
        let code = command(&[
            "check".into(),
            "--root".into(),
            root_s.clone(),
            "--registry".into(),
            reg_s.clone(),
        ])
        .expect("clean check");
        assert_eq!(code, 0);

        // 2. Add unassociated wild file -> check exits 1, file untouched on disk.
        let wild_content = "pub fn newly_added() -> u32 { 42 }\n";
        fs::write(root.join("src/wild.rs"), wild_content).expect("wild.rs");
        let code = command(&[
            "check".into(),
            "--root".into(),
            root_s.clone(),
            "--registry".into(),
            reg_s.clone(),
        ])
        .expect("wild check");
        assert_eq!(code, 1);
        assert_eq!(
            fs::read_to_string(root.join("src/wild.rs")).expect("read wild"),
            wild_content
        );

        // 3. Associate wild file with two features -> returns 0, file intact, check returns 0.
        let code = command(&[
            "associate".into(),
            "--root".into(),
            root_s.clone(),
            "--registry".into(),
            reg_s.clone(),
            "--path".into(),
            "src/wild.rs".into(),
            "--feature".into(),
            "domain-core".into(),
            "--feature".into(),
            "cap-api".into(),
        ])
        .expect("associate");
        assert_eq!(code, 0);
        assert_eq!(
            fs::read_to_string(root.join("src/wild.rs")).expect("read wild after"),
            wild_content
        );
        let code = command(&[
            "check".into(),
            "--root".into(),
            root_s.clone(),
            "--registry".into(),
            reg_s.clone(),
        ])
        .expect("post-associate check");
        assert_eq!(code, 0);

        // 4. Inject an orphan capability -> check exits 1 even though files are 100% covered.
        let mut reg_with_orphan = load_registry(&registry_path).expect("reload");
        reg_with_orphan.features.push(FeatureNode {
            id: "cap-orphan".to_string(),
            title: "Orphan".to_string(),
            kind: FeatureKind::Capability,
            parent: None,
            summary: "Unanchored".to_string(),
            status_note: None,
            files: Vec::new(),
            relations: Vec::new(),
            provenance: FeatureProvenance::default(),
        });
        save_registry_atomic(&registry_path, &reg_with_orphan).expect("save orphan");
        let code = command(&[
            "check".into(),
            "--root".into(),
            root_s.clone(),
            "--registry".into(),
            reg_s.clone(),
        ])
        .expect("orphan check");
        assert_eq!(code, 1);

        let _ = fs::remove_dir_all(&root);
    }

    /// A mission's `--features` registry names its project root: the
    /// conventional `docs/feature-map/registry.json` place, else the
    /// repository holding it, else its own directory.
    #[test]
    fn project_root_for_registry_derives_the_missions_project() {
        assert_eq!(
            project_root_for_registry(Path::new("/srv/acme/docs/feature-map/registry.json")),
            PathBuf::from("/srv/acme")
        );
        assert_eq!(
            project_root_for_registry(Path::new(
                "no-such-project-554/docs/feature-map/registry.json"
            )),
            PathBuf::from("no-such-project-554")
        );

        let repo = temp_fixture("project-root");
        fs::create_dir_all(repo.join(".git")).expect(".git");
        fs::create_dir_all(repo.join("meta/maps")).expect("maps");
        assert_eq!(
            project_root_for_registry(&repo.join("meta/maps/features.json")),
            fs::canonicalize(&repo).expect("repo")
        );
        let loose = temp_fixture("project-root-loose");
        fs::create_dir_all(loose.join("maps")).expect("maps");
        let derived = project_root_for_registry(&loose.join("maps/features.json"));
        let real_loose = fs::canonicalize(&loose).expect("loose");
        if !real_loose.ancestors().any(|dir| dir.join(".git").exists()) {
            assert_eq!(derived, real_loose.join("maps"));
        }

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&loose);
    }

    /// A relative registry reached from another repository's backlog
    /// (`../../../acme/meta/features.json` from Spawn's) resolves its `..`
    /// segments before the `.git` walk: it never lands on the repository
    /// they only pass through.
    #[test]
    fn project_root_for_registry_normalizes_dot_dot_before_the_git_walk() {
        let base = temp_fixture("project-root-dotdot");
        fs::create_dir_all(base.join("spawn/.git")).expect("spawn .git");
        fs::create_dir_all(base.join("spawn/rsi/backlog")).expect("backlog");
        fs::create_dir_all(base.join("acme/.git")).expect("acme .git");
        fs::create_dir_all(base.join("acme/meta")).expect("meta");
        fs::create_dir_all(base.join("acme/docs/feature-map")).expect("feature-map");
        let acme = fs::canonicalize(base.join("acme")).expect("acme");
        let through = base.join("spawn/rsi/backlog/../../../acme/meta/features.json");
        assert_eq!(project_root_for_registry(&through), acme);
        let conventional =
            base.join("spawn/rsi/backlog/../../../acme/docs/feature-map/registry.json");
        assert_eq!(project_root_for_registry(&conventional), acme);

        assert_eq!(
            normalize_lexically(Path::new("/srv/spawn/rsi/backlog/../../../acme/./x.json")),
            PathBuf::from("/srv/acme/x.json")
        );
        assert_eq!(normalize_lexically(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(
            normalize_lexically(Path::new("../a/../../b")),
            PathBuf::from("../../b")
        );
        assert_eq!(normalize_lexically(Path::new("a/..")), PathBuf::from("."));

        let _ = fs::remove_dir_all(&base);
    }

    /// A `..` after a symlink leaves the link's target, as the OS resolves
    /// it (`/var/run/..` is `/`, not `/var`): the root is found from the
    /// file the path really names, and the path itself, which is what the
    /// registry is opened and saved by, names that file.
    #[test]
    fn project_root_for_registry_follows_symlinks_before_dot_dot() {
        let base = temp_fixture("project-root-symlink");
        // The real file: <base>/real/acme/meta/features.json, Acme a repo.
        fs::create_dir_all(base.join("real/acme/.git")).expect("acme .git");
        fs::create_dir_all(base.join("real/acme/meta/run")).expect("run");
        // <base>/spawn is another repo holding `run`, a link into Acme.
        fs::create_dir_all(base.join("spawn/.git")).expect("spawn .git");
        std::os::unix::fs::symlink(base.join("real/acme/meta/run"), base.join("spawn/run"))
            .expect("symlink");
        let registry = base.join("spawn/run/../features.json");
        fs::write(base.join("real/acme/meta/features.json"), "{}").expect("real file");
        // Lexically this would be <base>/spawn/features.json (no such file)
        // under the Spawn repo; on disk it is Acme's file.
        assert_eq!(
            fs::read_to_string(&registry).expect("the path names the real file"),
            "{}"
        );
        assert!(!base.join("spawn/features.json").exists());
        let acme = fs::canonicalize(base.join("real/acme")).expect("acme");
        assert_eq!(project_root_for_registry(&registry), acme);
        // Not created yet: its parent's real place decides the same way.
        let fresh = base.join("spawn/run/../fresh.json");
        assert_eq!(project_root_for_registry(&fresh), acme);

        let _ = fs::remove_dir_all(&base);
    }

    /// A temporary project directory, removed when dropped (passed or
    /// failed), written from a string table of `(path, text)`.
    struct Project(PathBuf);

    impl Project {
        fn new(name: &str, table: &[(&str, &str)]) -> Self {
            let project = Self(temp_fixture(name));
            for (path, text) in table {
                let file = project.0.join(path);
                fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
                fs::write(file, text).expect("write fixture file");
            }
            project
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Two crates: an app that uses a library item by crate name (once in
    /// a macro body), and a test; no `depends_on` declared.
    const GRAPH_PROJECT: &[(&str, &str)] = &[
        ("Cargo.toml", "[workspace]\nmembers = [\"core\", \"app\"]\n"),
        (
            "core/Cargo.toml",
            "[package]\nname = \"fx-core\"\nversion = \"0.1.0\"\n",
        ),
        ("core/src/lib.rs", "pub mod util;\npub use util::clamp;\n"),
        (
            "core/src/util.rs",
            "pub fn clamp(x: i32) -> i32 {\n    x\n}\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname = \"fx-app\"\nversion = \"0.1.0\"\n\n[dependencies]\nfx-core = { path = \"../core\" }\n",
        ),
        (
            "app/src/main.rs",
            "use fx_core::clamp;\n\nfn main() {\n    println!(\"{}\", fx_core::util::clamp(clamp(1)));\n}\n",
        ),
        ("README.md", "# fx\n"),
    ];

    fn graph_registry_fixture() -> FeatureRegistry {
        let node = |id: &str, kind, parent: Option<&str>, files: &[&str]| FeatureNode {
            id: id.to_string(),
            title: id.to_string(),
            kind,
            parent: parent.map(str::to_string),
            summary: String::new(),
            status_note: None,
            files: files.iter().map(|f| (*f).to_string()).collect(),
            relations: Vec::new(),
            provenance: FeatureProvenance::default(),
        };
        FeatureRegistry {
            schema_version: SCHEMA_VERSION,
            initiative_id: "fx".to_string(),
            boundary: BoundaryPolicy::default_generic_boundary("fx"),
            features: vec![
                node("dom-fx", FeatureKind::Domain, None, &[]),
                node(
                    "cap-app",
                    FeatureKind::Capability,
                    Some("dom-fx"),
                    &["app/Cargo.toml", "app/src/main.rs"],
                ),
                node(
                    "cap-core",
                    FeatureKind::Capability,
                    Some("dom-fx"),
                    &["core/Cargo.toml", "core/src/lib.rs", "core/src/util.rs"],
                ),
            ],
        }
    }

    fn graph_args(project: &Project, extra: &[&str]) -> Vec<String> {
        let mut args = vec![
            "--root".to_string(),
            project.0.display().to_string(),
            "--registry".to_string(),
            project.0.join("registry.json").display().to_string(),
        ];
        args.extend(extra.iter().map(|word| (*word).to_string()));
        args
    }

    #[test]
    fn graph_from_to_lists_file_edges_with_lines() {
        let project = Project::new("graph-from-to", GRAPH_PROJECT);
        save_registry_atomic(&project.0.join("registry.json"), &graph_registry_fixture())
            .expect("save registry");

        let opts = parse_graph_opts(&graph_args(
            &project,
            &["--from", "cap-app", "--to", "cap-core"],
        ))
        .expect("options");
        let (payload, _) = graph_payload(&opts).expect("graph");
        assert_eq!(payload["level"], json!("feature"));
        assert_eq!(payload["graph"]["files_analyzed"], json!(3));
        let edges = payload["feature_edges"].as_array().expect("feature edges");
        assert_eq!(edges.len(), 1, "{edges:?}");
        assert_eq!(edges[0]["from_any_of"], json!(["cap-app"]));
        assert_eq!(edges[0]["to_any_of"], json!(["cap-core"]));
        assert_eq!(edges[0]["status"], json!("undeclared"));
        let lines: Vec<(String, u64, String, Option<String>)> = edges[0]["evidence"]
            .as_array()
            .expect("evidence")
            .iter()
            .map(|edge| {
                (
                    edge["from"].as_str().expect("from").to_string(),
                    edge["line"].as_u64().expect("line"),
                    edge["to"].as_str().expect("to").to_string(),
                    edge["via"].as_str().map(str::to_string),
                )
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                (
                    "app/src/main.rs".to_string(),
                    1,
                    "core/src/util.rs".to_string(),
                    Some("core/src/lib.rs".to_string()),
                ),
                (
                    "app/src/main.rs".to_string(),
                    4,
                    "core/src/util.rs".to_string(),
                    None
                ),
            ]
        );
        let findings = payload["truth"]["findings"].as_array().expect("findings");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["code"], json!("under_declared"));
        assert_eq!(findings[0]["evidence_total"], json!(2));

        // The reverse direction has no edge; a path works as an end too.
        let reverse = parse_graph_opts(&graph_args(
            &project,
            &["--from", "cap-core", "--to", "cap-app"],
        ))
        .expect("options");
        let (payload, _) = graph_payload(&reverse).expect("graph");
        assert_eq!(payload["feature_edges"], json!([]));
        let by_path = parse_graph_opts(&graph_args(
            &project,
            &[
                "--level",
                "file",
                "--from",
                "app/src",
                "--to",
                "core/src/util.rs",
            ],
        ))
        .expect("options");
        let (payload, _) = graph_payload(&by_path).expect("graph");
        assert_eq!(payload["edges"].as_array().map(Vec::len), Some(2));

        // The human form runs read-only with rc 0 and leaves the tree as it was.
        let before = fs::read_to_string(project.0.join("registry.json")).expect("registry");
        let code = command(
            &std::iter::once("graph".to_string())
                .chain(graph_args(
                    &project,
                    &["--from", "cap-app", "--to", "cap-core"],
                ))
                .collect::<Vec<_>>(),
        )
        .expect("graph command");
        assert_eq!(code, 0);
        assert_eq!(
            fs::read_to_string(project.0.join("registry.json")).expect("registry"),
            before
        );
    }

    #[test]
    fn the_graph_usage_is_its_line_of_the_features_usage() {
        let graph = GRAPH_USAGE.strip_prefix("usage: ").expect("a usage");
        assert!(
            USAGE.lines().any(|line| line.trim_start() == graph),
            "{GRAPH_USAGE}\nis not a line of\n{USAGE}"
        );
    }

    #[test]
    fn graph_unit_level_needs_no_registry_and_bad_flags_are_refused() {
        let project = Project::new("graph-unit", GRAPH_PROJECT);
        let opts = parse_graph_opts(&graph_args(&project, &["--level", "unit", "--json"]))
            .expect("options");
        assert!(opts.json);
        let (payload, _) = graph_payload(&opts).expect("no registry needed");
        assert_eq!(payload["registry_path"], Value::Null);
        assert_eq!(
            payload["unit_edges"],
            json!([{
                "from": "cargo:fx-app",
                "to": "cargo:fx-core",
                "edges": 2,
                "prod_edges": 2,
                "declared": true,
            }])
        );
        // The feature level needs the registry.
        let feature = parse_graph_opts(&graph_args(&project, &[])).expect("options");
        assert!(graph_payload(&feature).is_err());

        for (bad, needle) in [
            (&["--level", "crate"][..], "--level"),
            (&["--colour", "red"], "unexpected flag --colour"),
            (&["--json", "yes"], "takes no value"),
            (&["--from"], "needs a value"),
        ] {
            let error = parse_graph_opts(&graph_args(&project, bad)).expect_err("refused");
            assert!(error.contains(needle), "{bad:?}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn graph_never_follows_symlinks_or_reads_oversized_files() {
        use std::os::unix::fs::symlink;

        let project = Project::new("graph-symlinks", GRAPH_PROJECT);
        let core = project.0.join("core");
        fs::create_dir_all(core.join("secrets")).expect("secrets");
        // An excluded file whose text must never reach the output.
        fs::write(
            core.join("secrets/k.rs"),
            "use crate::leaked_secret_name::x;\npub fn leaked_secret_fn() {}\n",
        )
        .expect("secret");
        symlink("../secrets/k.rs", core.join("src/link.rs")).expect("link to excluded");
        fs::create_dir_all(core.join("big")).expect("big");
        let limit = usize::try_from(spawn_codegraph::MAX_SOURCE_BYTES).expect("limit");
        fs::write(
            core.join("big/huge.rs"),
            "// pad\n".repeat(limit / 7 + 1024),
        )
        .expect("huge");
        symlink("../big/huge.rs", core.join("src/huge_link.rs")).expect("link to huge");
        fs::write(
            core.join("src/lib.rs"),
            "pub mod util;\npub use util::clamp;\nmod link;\nmod huge_link;\n",
        )
        .expect("lib.rs");

        let opts = parse_graph_opts(&graph_args(&project, &["--level", "file"])).expect("options");
        let (payload, _) = graph_payload(&opts).expect("graph");
        let text = payload.to_string();
        assert!(
            !text.contains("leaked_secret"),
            "the excluded target's text leaked: {text}"
        );
        let skipped: BTreeMap<String, String> = payload["graph"]["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .filter(|d| d["code"] == json!("skipped"))
            .map(|d| {
                (
                    d["path"].as_str().expect("path").to_string(),
                    d["message"].as_str().expect("message").to_string(),
                )
            })
            .collect();
        assert_eq!(
            skipped.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "core/big/huge.rs",
                "core/src/huge_link.rs",
                "core/src/link.rs"
            ],
            "{skipped:?}"
        );
        assert!(
            skipped["core/src/link.rs"].contains("symlink"),
            "{skipped:?}"
        );
        assert!(
            skipped["core/src/huge_link.rs"].contains("symlink"),
            "{skipped:?}"
        );
        assert!(
            skipped["core/big/huge.rs"].contains("over the"),
            "{skipped:?}"
        );
        assert_eq!(payload["graph"]["files_parsed"], json!(3));
    }

    #[test]
    fn graph_json_is_byte_identical_across_runs_and_has_no_clock() {
        let project = Project::new("graph-bytes", GRAPH_PROJECT);
        save_registry_atomic(&project.0.join("registry.json"), &graph_registry_fixture())
            .expect("save registry");
        let opts = parse_graph_opts(&graph_args(&project, &["--json"])).expect("options");
        let first =
            serde_json::to_string_pretty(&graph_payload(&opts).expect("graph").0).expect("json");
        let second =
            serde_json::to_string_pretty(&graph_payload(&opts).expect("graph").0).expect("json");
        assert_eq!(first, second);
        assert!(!first.contains("_ms"), "no wall-clock figure: {first}");
    }

    #[test]
    fn graph_feature_filter_refuses_unknown_ids_and_recounts() {
        let project = Project::new("graph-feature", GRAPH_PROJECT);
        save_registry_atomic(&project.0.join("registry.json"), &graph_registry_fixture())
            .expect("save registry");
        let unknown = parse_graph_opts(&graph_args(&project, &["--feature", "no-such-feature"]))
            .expect("options");
        let error = graph_payload(&unknown).expect_err("unknown feature");
        assert!(
            error.contains("\"no-such-feature\" names no feature"),
            "{error}"
        );
        let code = command(
            &std::iter::once("graph".to_string())
                .chain(graph_args(&project, &["--feature", "no-such-feature"]))
                .collect::<Vec<_>>(),
        );
        assert!(code.is_err(), "the command fails (rc 1 through main)");

        // The domain holds no file of its own: the filter is by subtree.
        let scoped =
            parse_graph_opts(&graph_args(&project, &["--feature", "cap-core"])).expect("options");
        let (payload, _) = graph_payload(&scoped).expect("graph");
        assert_eq!(payload["feature"], json!("cap-core"));
        let findings: Vec<Finding> =
            serde_json::from_value(payload["truth"]["findings"].clone()).expect("findings");
        assert_eq!(findings.len(), 1);
        let counts: TruthCounts =
            serde_json::from_value(payload["truth"]["counts"].clone()).expect("counts");
        assert_eq!(counts, TruthCounts::of(&findings));
        // A feature no finding touches: zero counts, not the whole report's.
        let mut registry = graph_registry_fixture();
        registry.features.push(FeatureNode {
            id: "cap-idle".to_string(),
            title: "idle".to_string(),
            kind: FeatureKind::Capability,
            parent: Some("dom-fx".to_string()),
            summary: String::new(),
            status_note: None,
            files: vec!["README.md".to_string()],
            relations: Vec::new(),
            provenance: FeatureProvenance::default(),
        });
        save_registry_atomic(&project.0.join("registry.json"), &registry).expect("save");
        let idle =
            parse_graph_opts(&graph_args(&project, &["--feature", "cap-idle"])).expect("options");
        let (payload, _) = graph_payload(&idle).expect("graph");
        assert_eq!(payload["truth"]["findings"], json!([]));
        assert_eq!(payload["truth"]["counts"]["error"], json!(0));
        assert_eq!(payload["truth"]["counts"]["by_code"], json!({}));
    }
}

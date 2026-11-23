//! IO adapter, CLI subcommand (`spawn features`), and HTTP payload builders
//! for feature-map file coverage and one-click unassociated-file ("野文件")
//! scanning.
//!
//! All pure coverage/consistency logic lives in
//! [`spawn_core::feature_coverage`]. This module performs bounded filesystem
//! walking, git index querying, and atomic registry persistence.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use spawn_core::feature_coverage::{
    AssociationUpdate, BoundaryPolicy, FeatureRegistry, InventoryEntry, SCHEMA_VERSION, ScanReport,
    associate_file, match_directory_exclusion, match_exclusion, scan_coverage,
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

/// Entrypoint for `spawn features <scan|associate|check>`.
pub fn command(args: &[String]) -> Result<i32, String> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(
            "usage: spawn features <scan|associate|check> [--root <dir>] [--registry <file>] [--json]"
                .to_string(),
        );
    };
    match sub.as_str() {
        "scan" => cmd_scan(rest, false),
        "check" => cmd_scan(rest, true),
        "associate" => cmd_associate(rest),
        other => Err(format!(
            "unknown features subcommand {other:?}; expected scan, associate, or check"
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

#[cfg(test)]
mod tests {
    use super::*;
    use spawn_core::feature_coverage::{FeatureKind, FeatureNode, FeatureProvenance};

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
}

//! Fixture tests covering all 5 `t565u` acceptance criteria plus end-to-end
//! verification of the real `docs/feature-map/registry.json` against this
//! repository's actual working tree.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use spawn_core::feature_coverage::{
    BoundaryPolicy, FeatureKind, FeatureNode, FeatureProvenance, FeatureRegistry, FeatureRelation,
    InventoryEntry, RelationKind, SCHEMA_VERSION, associate_file, scan_coverage, validate_registry,
};

fn fixture_registry() -> FeatureRegistry {
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
                summary: "Core domain logic".to_string(),
                status_note: None,
                files: vec!["crates/spawn-core/src/lib.rs".to_string()],
                relations: vec![FeatureRelation {
                    kind: RelationKind::Contains,
                    target: "cap-coverage".to_string(),
                    note: None,
                }],
                provenance: FeatureProvenance::default(),
            },
            FeatureNode {
                id: "cap-coverage".to_string(),
                title: "Coverage scanner".to_string(),
                kind: FeatureKind::Capability,
                parent: Some("domain-core".to_string()),
                summary: "Wild-file scan and consistency engine".to_string(),
                status_note: None,
                files: vec!["crates/spawn-core/src/feature_coverage.rs".to_string()],
                relations: Vec::new(),
                provenance: FeatureProvenance::default(),
            },
            FeatureNode {
                id: "cap-api".to_string(),
                title: "HTTP API".to_string(),
                kind: FeatureKind::Capability,
                parent: Some("domain-core".to_string()),
                summary: "Serve API endpoints".to_string(),
                status_note: None,
                files: Vec::new(),
                relations: vec![FeatureRelation {
                    kind: RelationKind::DependsOn,
                    target: "cap-coverage".to_string(),
                    note: None,
                }],
                provenance: FeatureProvenance::default(),
            },
        ],
    }
}

fn entry(path: &str, tracked: bool, size_bytes: u64) -> InventoryEntry {
    InventoryEntry {
        path: path.to_string(),
        tracked,
        size_bytes,
        sha256: None,
    }
}

#[test]
fn t565u_1_detects_new_in_scope_unassociated_wild_file() {
    let registry = fixture_registry();
    let inventory = vec![
        entry("crates/spawn-core/src/lib.rs", true, 400),
        entry("crates/spawn-core/src/feature_coverage.rs", true, 8000),
        entry("crates/spawn-core/src/brand_new_wild.rs", false, 128),
    ];

    let report = scan_coverage(&registry, &inventory, 1_795_000_000_000);
    assert!(!report.is_clean());
    assert_eq!(report.counts.in_scope, 3);
    assert_eq!(report.counts.covered, 2);
    assert_eq!(report.counts.unassociated_wild, 1);
    assert_eq!(report.unassociated_files.len(), 1);

    let wild = &report.unassociated_files[0];
    assert_eq!(wild.path, "crates/spawn-core/src/brand_new_wild.rs");
    assert!(!wild.tracked);
    assert!(wild.reason.contains("no feature association"));
    assert!(wild.evidence.iter().any(|e| e == "git_tracked=false"));
    assert!(
        wild.suggested_features
            .contains(&"cap-coverage".to_string())
            || wild.suggested_features.contains(&"domain-core".to_string()),
        "expected directory-prefix feature suggestion, got {:?}",
        wild.suggested_features
    );
}

#[test]
fn t565u_2_and_3_associates_wild_file_many_to_many_and_updates_bidirectional_index() {
    let mut registry = fixture_registry();
    let wild_path = "crates/spawn-core/src/shared_contract.rs";
    let inventory = vec![
        entry("crates/spawn-core/src/lib.rs", true, 400),
        entry("crates/spawn-core/src/feature_coverage.rs", true, 8000),
        entry(wild_path, false, 256),
    ];

    let before = scan_coverage(&registry, &inventory, 100);
    assert_eq!(before.counts.unassociated_wild, 1);

    // Associate the single file with TWO genuine features (many-to-many).
    let update = associate_file(
        &mut registry,
        wild_path,
        &["cap-coverage".to_string(), "cap-api".to_string()],
        &inventory,
    )
    .expect("valid many-to-many association");

    assert_eq!(update.path, wild_path);
    assert_eq!(
        update.added_to_features,
        vec!["cap-api".to_string(), "cap-coverage".to_string()]
    );
    assert_eq!(
        update.all_associated_features,
        vec!["cap-api".to_string(), "cap-coverage".to_string()]
    );

    let after = scan_coverage(&registry, &inventory, 200);
    assert!(after.is_clean());
    assert_eq!(after.counts.unassociated_wild, 0);
    assert_eq!(after.counts.covered, 3);
    assert_eq!(after.counts.multi_feature_files, 1);
    assert_eq!(
        after.bidirectional_index.file_to_features.get(wild_path),
        Some(&vec!["cap-api".to_string(), "cap-coverage".to_string()])
    );
    assert!(
        after
            .bidirectional_index
            .feature_to_files
            .get("cap-coverage")
            .is_some_and(|files| files.contains(&wild_path.to_string()))
    );
    assert!(
        after
            .bidirectional_index
            .feature_to_files
            .get("cap-api")
            .is_some_and(|files| files.contains(&wild_path.to_string()))
    );
}

#[test]
fn t565u_4_explicitly_excluded_files_are_never_counted_as_covered_or_wild() {
    let mut registry = fixture_registry();
    let inventory = vec![
        entry("crates/spawn-core/src/lib.rs", true, 400),
        entry("crates/spawn-core/src/feature_coverage.rs", true, 8000),
        entry("target/debug/spawn", false, 99999),
        entry("web/node_modules/react/index.js", false, 1024),
        entry(".env", false, 64),
        entry("secrets/deploy.key", false, 128),
        entry(".DS_Store", false, 32),
    ];

    let report = scan_coverage(&registry, &inventory, 300);
    assert!(report.is_clean());
    assert_eq!(report.counts.in_scope, 2);
    assert_eq!(report.counts.covered, 2);
    assert_eq!(report.counts.unassociated_wild, 0);
    assert_eq!(report.counts.explicit_exclusions, 5);
    assert_eq!(report.counts.total_seen, 7);

    // Every excluded file carries its rule_id, category, and non-empty reason.
    for excluded in &report.excluded_files {
        assert!(!excluded.rule_id.is_empty());
        assert!(!excluded.category.is_empty());
        assert!(!excluded.reason.is_empty());
        assert!(
            !report
                .bidirectional_index
                .file_to_features
                .contains_key(&excluded.path),
            "excluded path {} must never appear in covered index",
            excluded.path
        );
    }

    // Attempting to associate an excluded or traversal path must be refused.
    assert!(
        associate_file(&mut registry, ".env", &["domain-core".to_string()], &inventory).is_err()
    );
    assert!(
        associate_file(
            &mut registry,
            "../outside.rs",
            &["domain-core".to_string()],
            &inventory
        )
        .is_err()
    );
}

#[test]
fn t565u_5_stale_references_are_flagged_separately_from_wild_files() {
    let mut registry = fixture_registry();
    registry.features[1]
        .files
        .push("crates/spawn-core/src/removed_scanner.rs".to_string());

    let inventory = vec![
        entry("crates/spawn-core/src/lib.rs", true, 400),
        entry("crates/spawn-core/src/feature_coverage.rs", true, 8000),
        entry("crates/spawn-core/src/new_unlinked.rs", false, 200),
    ];

    let report = scan_coverage(&registry, &inventory, 400);
    assert!(!report.is_clean());
    assert_eq!(report.counts.unassociated_wild, 1);
    assert_eq!(report.counts.stale_references, 1);
    assert_eq!(
        report.unassociated_files[0].path,
        "crates/spawn-core/src/new_unlinked.rs"
    );
    assert_eq!(
        report.stale_references[0].path,
        "crates/spawn-core/src/removed_scanner.rs"
    );
    assert_eq!(
        report.stale_references[0].referencing_features,
        vec!["cap-coverage".to_string()]
    );
    assert!(
        report.stale_references[0]
            .rename_candidates
            .contains(&"crates/spawn-core/src/new_unlinked.rs".to_string()),
        "expected same-dir unassociated file as rename candidate: {:?}",
        report.stale_references[0].rename_candidates
    );
}

#[test]
fn validates_cycles_self_links_dangling_relations_and_orphans() {
    let broken = FeatureRegistry {
        schema_version: SCHEMA_VERSION,
        initiative_id: "broken".to_string(),
        boundary: BoundaryPolicy::default_spawn_boundary(),
        features: vec![
            FeatureNode {
                id: "a".to_string(),
                title: "A".to_string(),
                kind: FeatureKind::Capability, // orphan (no parent, no incoming contains)
                parent: None,
                summary: "A".to_string(),
                status_note: None,
                files: vec!["../escape.rs".to_string()],
                relations: vec![
                    FeatureRelation {
                        kind: RelationKind::DependsOn,
                        target: "b".to_string(),
                        note: None,
                    },
                    FeatureRelation {
                        kind: RelationKind::RelatesTo,
                        target: "a".to_string(),
                        note: None,
                    },
                    FeatureRelation {
                        kind: RelationKind::Extends,
                        target: "ghost".to_string(),
                        note: None,
                    },
                ],
                provenance: FeatureProvenance::default(),
            },
            FeatureNode {
                id: "b".to_string(),
                title: "B".to_string(),
                kind: FeatureKind::Domain,
                parent: None,
                summary: "B".to_string(),
                status_note: None,
                files: Vec::new(),
                relations: vec![FeatureRelation {
                    kind: RelationKind::DependsOn,
                    target: "a".to_string(),
                    note: None,
                }],
                provenance: FeatureProvenance::default(),
            },
        ],
    };

    let consistency = validate_registry(&broken);
    assert!(!consistency.is_clean());
    assert_eq!(consistency.orphan_features, vec!["a".to_string()]);
    assert!(!consistency.self_links.is_empty());
    assert!(!consistency.dangling_relations.is_empty());
    assert!(!consistency.cycle_errors.is_empty());
    assert!(!consistency.invalid_paths.is_empty());
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/crates/spawn-core`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn collect_repo_inventory(root: &Path, boundary: &BoundaryPolicy) -> Vec<InventoryEntry> {
    let git_out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .env("LC_ALL", "C")
        .output()
        .expect("git ls-files");
    assert!(git_out.status.success(), "git ls-files failed");
    let tracked: BTreeSet<String> = git_out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        .map(str::to_string)
        .collect();

    let mut entries = Vec::new();
    walk_repo(root, "", boundary, &tracked, &mut entries);
    entries
}

fn walk_repo(
    dir: &Path,
    rel_prefix: &str,
    boundary: &BoundaryPolicy,
    tracked: &BTreeSet<String>,
    out: &mut Vec<InventoryEntry>,
) {
    let mut children: Vec<PathBuf> = fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok().map(|item| item.path()))
        .collect();
    children.sort();
    for path in children {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = if rel_prefix.is_empty() {
            name.to_string()
        } else {
            format!("{rel_prefix}/{name}")
        };
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.file_type().is_dir() {
            if matches!(
                name,
                ".git" | "target" | "node_modules" | "dist" | ".cache" | "__pycache__" | ".spawn" | ".claude"
            ) && spawn_core::feature_coverage::match_exclusion(boundary, &format!("{rel}/"))
                .is_some()
            {
                out.push(InventoryEntry {
                    path: format!("{rel}/"),
                    tracked: false,
                    size_bytes: 0,
                    sha256: None,
                });
                continue;
            }
            walk_repo(&path, &rel, boundary, tracked, out);
        } else if meta.file_type().is_file() {
            out.push(InventoryEntry {
                path: rel.clone(),
                tracked: tracked.contains(&rel),
                size_bytes: meta.len(),
                sha256: None,
            });
        }
    }
}

#[test]
fn real_project_registry_covers_every_in_scope_file_with_zero_drift() {
    let root = repo_root();
    let registry_path = root.join("docs/feature-map/registry.json");
    let raw = fs::read_to_string(&registry_path).expect("read docs/feature-map/registry.json");
    let registry: FeatureRegistry =
        serde_json::from_str(&raw).expect("parse docs/feature-map/registry.json");

    assert_eq!(registry.schema_version, SCHEMA_VERSION);
    assert_eq!(registry.initiative_id, "t559u");

    // Verify required initiatives and slices exist and are cleanly linked.
    let ids: BTreeSet<&str> = registry.features.iter().map(|f| f.id.as_str()).collect();
    for required in [
        "t559u-feature-map-initiative",
        "t559u-slice-a-mindmap",
        "t559u-slice-b-coverage",
        "t559u-slice-c-storage-keys",
        "t565u-wild-file-scan",
        "t567u-multi-machine-distributed",
    ] {
        assert!(ids.contains(required), "missing required feature node {required}");
    }

    let t567u = registry
        .features
        .iter()
        .find(|f| f.id == "t567u-multi-machine-distributed")
        .expect("t567u node");
    assert_eq!(t567u.kind, FeatureKind::ResearchInitiative);
    assert!(
        t567u.parent.is_none(),
        "t567u must be a distinct research initiative, not a child slice of t559u"
    );
    assert!(
        t567u
            .relations
            .iter()
            .any(|r| r.target == "t559u-feature-map-initiative"),
        "t567u must link to t559u-feature-map-initiative"
    );

    let inventory = collect_repo_inventory(&root, &registry.boundary);
    let report = scan_coverage(&registry, &inventory, 1_795_000_000_000);

    assert!(
        report.unassociated_files.is_empty(),
        "expected 0 wild files in real repo, found {}: {:?}",
        report.unassociated_files.len(),
        report
            .unassociated_files
            .iter()
            .take(15)
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        report.stale_references.is_empty(),
        "expected 0 stale references in real repo, found {:?}",
        report.stale_references
    );
    assert!(
        report.consistency.is_clean(),
        "expected clean registry consistency, got {:?}",
        report.consistency
    );
    assert!(
        report.counts.in_scope >= 968,
        "expected at least 968 in-scope files, got {}",
        report.counts.in_scope
    );
    assert_eq!(report.counts.covered, report.counts.in_scope);
    assert!(
        report.counts.multi_feature_files > 10,
        "expected genuine many-to-many shared files (>10), got {}",
        report.counts.multi_feature_files
    );
}

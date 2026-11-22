//! Fixture tests covering all 5 `t565u` acceptance criteria plus end-to-end
//! verification of the real `docs/feature-map/registry.json` against this
//! repository's actual working tree.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use spawn_core::feature_coverage::{
    BoundaryPolicy, FeatureKind, FeatureNode, FeatureProvenance, FeatureRegistry, FeatureRelation,
    InventoryEntry, RelationKind, SCHEMA_VERSION, ScanReport, associate_file,
    match_directory_exclusion, scan_coverage, validate_registry,
};

fn assert_accounting_identities(report: &ScanReport) {
    assert_eq!(
        report.counts.covered + report.counts.unassociated_wild,
        report.counts.in_scope,
        "covered + unassociated_wild must equal in_scope"
    );
    assert_eq!(
        report.counts.in_scope + report.counts.explicit_exclusions,
        report.counts.total_seen,
        "in_scope + explicit_exclusions must equal total_seen"
    );
    assert_eq!(
        report.unassociated_files.len(),
        report.counts.unassociated_wild
    );
    assert_eq!(
        report.stale_references.len(),
        report.counts.stale_references
    );
    assert_eq!(
        report.excluded_files.len(),
        report.counts.explicit_exclusions
    );
    assert_eq!(
        report.bidirectional_index.file_to_features.len(),
        report.counts.covered
    );
    for excluded in &report.excluded_files {
        assert!(
            !report
                .bidirectional_index
                .file_to_features
                .contains_key(&excluded.path),
            "excluded path {} leaked into covered index",
            excluded.path
        );
        assert!(
            !report
                .unassociated_files
                .iter()
                .any(|w| w.path == excluded.path),
            "excluded path {} leaked into unassociated_files",
            excluded.path
        );
    }
}

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
        associate_file(
            &mut registry,
            ".env",
            &["domain-core".to_string()],
            &inventory
        )
        .is_err()
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
                ".git"
                    | "target"
                    | "node_modules"
                    | "dist"
                    | ".cache"
                    | "__pycache__"
                    | ".spawn"
                    | ".claude"
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

fn assert_real_registry_semantic_invariants(registry: &FeatureRegistry, report: &ScanReport) {
    let ids: BTreeSet<&str> = registry.features.iter().map(|f| f.id.as_str()).collect();
    for forbidden_catchall in [
        "domain-workspace-docs",
        "cap-rsi-backlog-specs",
        "cap-rsi-gate-scenarios",
        "cap-web-fleet-home-views",
        "cap-web-missions-rsi-trace-views",
    ] {
        assert!(
            !ids.contains(forbidden_catchall),
            "forbidden catch-all bucket {forbidden_catchall} must not exist"
        );
    }
    for feat in &registry.features {
        if feat.kind == FeatureKind::Domain {
            assert!(
                feat.files.is_empty(),
                "root domain {} must organize child capabilities rather than holding stray files: {:?}",
                feat.id,
                feat.files
            );
        }
    }

    let find_node = |id: &str| {
        registry
            .features
            .iter()
            .find(|f| f.id == id)
            .unwrap_or_else(|| panic!("missing feature {id}"))
    };
    assert!(
        find_node("t559u-slice-a-mindmap").files.is_empty(),
        "unmerged parallel slice t559u-slice-a-mindmap must not claim fabricated placeholder files"
    );
    assert!(
        !find_node("t559u-slice-c-storage-keys")
            .files
            .iter()
            .any(|p| p == "docs/storage.md"),
        "t559u-slice-c-storage-keys must not claim unrelated docs/storage.md"
    );
    assert!(
        !find_node("t567u-multi-machine-distributed")
            .files
            .iter()
            .any(|p| p == "docs/storage.md"),
        "t567u-multi-machine-distributed must not claim unrelated docs/storage.md"
    );

    let f2f = &report.bidirectional_index.file_to_features;
    for (path, expected_feat) in [
        (
            "crates/spawn-cli/tests/data/mission-4-outcomes-2026-11-07T23-05-46.json",
            "cap-mission-dispatch",
        ),
        ("crates/spawn-cli/src/sources.rs", "cap-git-workspaces"),
        ("crates/spawn-cli/src/sources.rs", "cap-mission-dispatch"),
        ("crates/spawn-cli/src/context.rs", "cap-semantic-observer"),
        ("crates/spawn-cli/src/context.rs", "cap-jev-judge-relay"),
        ("crates/spawn-cli/src/memory.rs", "cap-serve-http-api"),
        ("scripts/move-check.py", "cap-rsi-integration-gate"),
        ("rsi/scenarios/check-web.sh", "cap-web-shell-design-system"),
        (
            "deploy/systemd/spawn-backup.service",
            "cap-backup-and-drills",
        ),
        (
            "crates/spawn-postgres/tests/scorecard.rs",
            "cap-scorecard-and-cost",
        ),
        (
            "crates/spawn-postgres/tests/storage.rs",
            "cap-storage-lifecycle",
        ),
        (
            "crates/spawn-postgres/tests/text_masked.rs",
            "cap-secrets-and-masking",
        ),
        (
            "crates/spawn-postgres/tests/deploy.rs",
            "cap-deploy-and-systemd",
        ),
    ] {
        assert!(
            f2f.get(path)
                .is_some_and(|feats| feats.iter().any(|id| id == expected_feat)),
            "expected {path} to be associated with {expected_feat}, got {:?}",
            f2f.get(path)
        );
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
    assert_eq!(
        registry.boundary,
        BoundaryPolicy::default_spawn_boundary(),
        "docs/feature-map/registry.json boundary policy must match BoundaryPolicy::default_spawn_boundary()"
    );

    let ids: BTreeSet<&str> = registry.features.iter().map(|f| f.id.as_str()).collect();
    for required in [
        "t559u-feature-map-initiative",
        "t559u-slice-a-mindmap",
        "t559u-slice-b-coverage",
        "t559u-slice-c-storage-keys",
        "t565u-wild-file-scan",
        "t567u-multi-machine-distributed",
    ] {
        assert!(
            ids.contains(required),
            "missing required feature node {required}"
        );
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
    assert_accounting_identities(&report);
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
        "{:?}",
        report.stale_references
    );
    assert!(report.consistency.is_clean(), "{:?}", report.consistency);
    assert_eq!(report.counts.in_scope, 980);
    assert_eq!(report.counts.covered, report.counts.in_scope);
    assert!(report.counts.multi_feature_files >= 100);

    assert_real_registry_semantic_invariants(&registry, &report);
}

fn simple_node(id: &str, kind: FeatureKind, parent: Option<&str>) -> FeatureNode {
    FeatureNode {
        id: id.to_string(),
        title: id.to_string(),
        kind,
        parent: parent.map(str::to_string),
        summary: id.to_string(),
        status_note: None,
        files: Vec::new(),
        relations: Vec::new(),
        provenance: FeatureProvenance::default(),
    }
}

#[test]
fn orphan_features_detects_direct_transitive_dangling_self_parent_and_isolated_cycles() {
    let mut cyc_a = simple_node("cap-cycle-a", FeatureKind::Capability, None);
    cyc_a.relations.push(FeatureRelation {
        kind: RelationKind::Contains,
        target: "cap-cycle-b".to_string(),
        note: None,
    });
    let mut cyc_b = simple_node("cap-cycle-b", FeatureKind::Capability, None);
    cyc_b.relations.push(FeatureRelation {
        kind: RelationKind::Contains,
        target: "cap-cycle-a".to_string(),
        note: None,
    });

    let reg = FeatureRegistry {
        schema_version: SCHEMA_VERSION,
        initiative_id: "orphan-audit".to_string(),
        boundary: BoundaryPolicy::default_spawn_boundary(),
        features: vec![
            simple_node("domain-root", FeatureKind::Domain, None),
            simple_node("cap-valid", FeatureKind::Capability, Some("domain-root")),
            simple_node("sub-valid", FeatureKind::Subsystem, Some("cap-valid")),
            simple_node("research-root", FeatureKind::ResearchInitiative, None),
            simple_node("cap-direct-orphan", FeatureKind::Capability, None),
            simple_node(
                "sub-transitive-orphan",
                FeatureKind::Subsystem,
                Some("cap-direct-orphan"),
            ),
            simple_node(
                "cap-dangling-parent",
                FeatureKind::Capability,
                Some("ghost-domain"),
            ),
            simple_node(
                "slice-self-parent",
                FeatureKind::Slice,
                Some("slice-self-parent"),
            ),
            cyc_a,
            cyc_b,
        ],
    };

    let consistency = validate_registry(&reg);
    assert!(!consistency.is_clean());
    assert_eq!(
        consistency.orphan_features,
        vec![
            "cap-cycle-a".to_string(),
            "cap-cycle-b".to_string(),
            "cap-dangling-parent".to_string(),
            "cap-direct-orphan".to_string(),
            "slice-self-parent".to_string(),
            "sub-transitive-orphan".to_string(),
        ]
    );
    assert!(!consistency.dangling_relations.is_empty());
    assert!(!consistency.self_links.is_empty());
    assert!(!consistency.cycle_errors.is_empty());
}

#[test]
fn unassociated_files_detects_both_tracked_and_untracked_alongside_stale_and_excluded() {
    let mut registry = fixture_registry();
    // Make lib.rs a multi-feature shared file and add two stale refs.
    registry.features[2]
        .files
        .push("crates/spawn-core/src/lib.rs".to_string());
    registry.features[1]
        .files
        .push("crates/spawn-core/src/old_scanner.rs".to_string());
    registry.features[2]
        .files
        .push("docs/permanently_removed.md".to_string());

    let inventory = vec![
        entry("crates/spawn-core/src/lib.rs", true, 400),
        entry("crates/spawn-core/src/feature_coverage.rs", true, 8000),
        entry("crates/spawn-core/src/old_scanner_v2.rs", true, 512), // git-tracked wild
        entry("docs/untracked_scratch_design.md", false, 200),       // untracked wild
        entry(".git", false, 80),                                    // worktree .git file
        entry("target/debug/spawn", false, 50_000),
        entry("rsi/.gate-owner/lease", false, 24),
        entry("secrets/api.key", false, 64),
    ];

    let report = scan_coverage(&registry, &inventory, 900);
    assert_accounting_identities(&report);
    assert!(!report.is_clean());

    assert_eq!(report.counts.total_seen, 8);
    assert_eq!(report.counts.in_scope, 4);
    assert_eq!(report.counts.covered, 2);
    assert_eq!(report.counts.unassociated_wild, 2);
    assert_eq!(report.counts.stale_references, 2);
    assert_eq!(report.counts.explicit_exclusions, 4);
    assert_eq!(report.counts.multi_feature_files, 1);

    let tracked_wild = report
        .unassociated_files
        .iter()
        .find(|w| w.path == "crates/spawn-core/src/old_scanner_v2.rs")
        .expect("tracked wild");
    assert!(tracked_wild.tracked);
    assert!(tracked_wild.reason.contains("git-tracked"));

    let untracked_wild = report
        .unassociated_files
        .iter()
        .find(|w| w.path == "docs/untracked_scratch_design.md")
        .expect("untracked wild");
    assert!(!untracked_wild.tracked);
    assert!(untracked_wild.reason.contains("untracked"));

    let stale_renamed = report
        .stale_references
        .iter()
        .find(|s| s.path == "crates/spawn-core/src/old_scanner.rs")
        .expect("stale scanner ref");
    assert!(
        stale_renamed
            .rename_candidates
            .contains(&"crates/spawn-core/src/old_scanner_v2.rs".to_string())
    );
}

#[test]
fn excluded_file_in_registry_is_flagged_invalid_and_never_counted_as_covered_or_stale() {
    let mut registry = fixture_registry();
    registry.features[0].files.push(".env".to_string());
    registry.features[1]
        .files
        .push("secrets/absent.key".to_string());

    let inventory = vec![
        entry("crates/spawn-core/src/lib.rs", true, 400),
        entry("crates/spawn-core/src/feature_coverage.rs", true, 8000),
        entry(".env", false, 32),
    ];

    let report = scan_coverage(&registry, &inventory, 1000);
    assert_accounting_identities(&report);
    assert!(!report.is_clean());
    assert_eq!(report.counts.in_scope, 2);
    assert_eq!(report.counts.covered, 2);
    assert_eq!(report.counts.unassociated_wild, 0);
    assert_eq!(report.counts.stale_references, 0);
    assert_eq!(report.counts.explicit_exclusions, 1);
    assert_eq!(report.consistency.invalid_paths.len(), 2);
    assert!(
        report
            .consistency
            .invalid_paths
            .iter()
            .any(|msg| msg.contains(".env") && msg.contains("matches exclusion rule"))
    );
    assert!(
        report
            .consistency
            .invalid_paths
            .iter()
            .any(|msg| msg.contains("secrets/absent.key") && msg.contains("matches exclusion rule"))
    );
}

#[test]
fn duplicate_file_paths_in_feature_and_duplicate_feature_ids_are_rejected() {
    let mut registry = fixture_registry();
    // Same file listed twice (with mixed slash separator).
    registry.features[0]
        .files
        .push("crates\\spawn-core\\src\\lib.rs".to_string());
    // Duplicate feature id.
    let dup = registry.features[1].clone();
    registry.features.push(dup);

    let consistency = validate_registry(&registry);
    assert!(!consistency.is_clean());
    assert_eq!(
        consistency.duplicate_feature_ids,
        vec!["cap-coverage".to_string()]
    );
    assert!(
        consistency
            .invalid_paths
            .iter()
            .any(|m| m.contains("duplicate file path"))
    );

    let inventory = vec![
        entry("crates/spawn-core/src/lib.rs", true, 400),
        entry("crates/spawn-core/src/feature_coverage.rs", true, 8000),
        entry("crates/spawn-core/src/new.rs", false, 64),
    ];
    // Refuses ambiguous duplicate feature id, missing file, unknown feature, empty list.
    assert!(
        associate_file(
            &mut registry,
            "crates/spawn-core/src/new.rs",
            &["cap-coverage".to_string()],
            &inventory,
        )
        .expect_err("duplicate feature id must fail")
        .contains("ambiguous duplicate")
    );
    assert!(
        associate_file(
            &mut registry,
            "crates/spawn-core/src/missing.rs",
            &["domain-core".to_string()],
            &inventory,
        )
        .is_err()
    );
    assert!(
        associate_file(
            &mut registry,
            "crates/spawn-core/src/new.rs",
            &["unknown-feature".to_string()],
            &inventory,
        )
        .is_err()
    );
    assert!(
        associate_file(
            &mut registry,
            "crates/spawn-core/src/new.rs",
            &[],
            &inventory
        )
        .is_err()
    );
}

#[test]
fn associate_file_is_idempotent_and_preserves_existing_links() {
    let mut registry = fixture_registry();
    let inventory = vec![
        entry("crates/spawn-core/src/lib.rs", true, 400),
        entry("crates/spawn-core/src/feature_coverage.rs", true, 8000),
    ];

    let update = associate_file(
        &mut registry,
        "crates/spawn-core/src/lib.rs",
        &["domain-core".to_string(), "cap-api".to_string()],
        &inventory,
    )
    .expect("idempotent associate");
    assert_eq!(
        update.already_associated_features,
        vec!["domain-core".to_string()]
    );
    assert_eq!(update.added_to_features, vec!["cap-api".to_string()]);
    assert_eq!(
        update.all_associated_features,
        vec!["cap-api".to_string(), "domain-core".to_string()]
    );

    let report = scan_coverage(&registry, &inventory, 1100);
    assert!(report.is_clean());
    assert_accounting_identities(&report);
    assert_eq!(report.counts.multi_feature_files, 1);
}

#[test]
fn match_directory_exclusion_collapses_subtree_rules_only() {
    let policy = BoundaryPolicy::default_spawn_boundary();
    for dir in [
        ".git",
        "target",
        "web/node_modules",
        ".pnpm-store",
        "web/dist",
        "secrets",
        "deploy/secrets",
        "rsi/.gate-owner",
        ".spawn",
        ".claude",
        ".cache",
        "scripts/__pycache__",
    ] {
        assert!(
            match_directory_exclusion(&policy, dir).is_some(),
            "expected subtree directory exclusion for {dir}"
        );
    }
    for normal_dir in [
        "crates/spawn-core",
        "docs/feature-map",
        "foo.tmp",
        "web/src",
    ] {
        assert!(
            match_directory_exclusion(&policy, normal_dir).is_none(),
            "must not collapse non-subtree directory {normal_dir}"
        );
    }
}

//! Pure feature-map coverage, bidirectional indexing, wild-file ("野文件")
//! detection, stale-reference reporting, and registry consistency checks.
//!
//! This module performs **no IO**: filesystem walking, git inspection, CLI
//! argument handling, and atomic persistence live in `spawn-cli`
//! (`feature_inventory`). All invariants here are deterministic and directly
//! reusable as a library boundary.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Current schema version for the feature coverage registry and scan reports.
pub const SCHEMA_VERSION: u32 = 1;

/// One explicit exclusion rule disclosing why a matching path is outside
/// project feature coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionRule {
    pub id: String,
    pub pattern: String,
    pub category: String,
    pub reason: String,
}

/// Disclosed project file-universe boundary: what categories of files belong
/// in scope and which paths are explicitly excluded (and therefore never
/// counted as covered or listed as unassociated wild files).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryPolicy {
    pub project_name: String,
    pub description: String,
    pub included_categories: Vec<String>,
    pub exclusions: Vec<ExclusionRule>,
}

impl BoundaryPolicy {
    /// Default disclosed project file-universe boundary for the Spawn
    /// repository.
    pub fn default_spawn_boundary() -> Self {
        Self {
            project_name: "Spawn".to_string(),
            description: "All tracked project source, tests, docs, configuration, scripts, deploy specs, web assets, and RSI backlogs/scenarios inside the repository root, plus any newly created untracked in-scope files. Derived build caches, VCS internals, third-party installed dependencies, ephemeral agent runtime files, local editor swap files, and secrets/credentials are explicitly excluded and never counted as covered.".to_string(),
            included_categories: vec![
                "tracked-source".to_string(),
                "tests".to_string(),
                "docs".to_string(),
                "config".to_string(),
                "assets".to_string(),
                "deploy-and-scripts".to_string(),
                "rsi-scenarios-and-backlogs".to_string(),
                "untracked-in-scope-artifacts".to_string(),
            ],
            exclusions: vec![
                ExclusionRule {
                    id: "vcs-metadata".to_string(),
                    pattern: ".git/**".to_string(),
                    category: "vcs".to_string(),
                    reason: "Git internal object store and repository metadata are never feature artifacts.".to_string(),
                },
                ExclusionRule {
                    id: "rust-build-output".to_string(),
                    pattern: "target/**".to_string(),
                    category: "build-cache".to_string(),
                    reason: "Cargo build outputs are derived and rebuildable from source.".to_string(),
                },
                ExclusionRule {
                    id: "node-dependencies".to_string(),
                    pattern: "**/node_modules/**".to_string(),
                    category: "dependency-cache".to_string(),
                    reason: "Installed third-party frontend packages are external dependencies, not authored project files.".to_string(),
                },
                ExclusionRule {
                    id: "pnpm-store".to_string(),
                    pattern: "**/.pnpm-store/**".to_string(),
                    category: "dependency-cache".to_string(),
                    reason: "pnpm content-addressable package store is a local dependency cache.".to_string(),
                },
                ExclusionRule {
                    id: "web-build-output".to_string(),
                    pattern: "web/dist/**".to_string(),
                    category: "build-cache".to_string(),
                    reason: "Compiled frontend bundle is derived build output.".to_string(),
                },
                ExclusionRule {
                    id: "spawn-runtime-state".to_string(),
                    pattern: ".spawn/**".to_string(),
                    category: "runtime-state".to_string(),
                    reason: "Ephemeral agent execution markers and exit files are runtime state, not project deliverables.".to_string(),
                },
                ExclusionRule {
                    id: "engine-runtime-state".to_string(),
                    pattern: ".claude/**".to_string(),
                    category: "runtime-state".to_string(),
                    reason: "Local coding-engine runtime state and worktrees are excluded from project coverage.".to_string(),
                },
                ExclusionRule {
                    id: "rsi-gate-owner-state".to_string(),
                    pattern: "rsi/.gate-owner/**".to_string(),
                    category: "runtime-state".to_string(),
                    reason: "Ephemeral RSI gate owner lease and runtime state (gitignored) are not authored project artifacts.".to_string(),
                },
                ExclusionRule {
                    id: "deployed-controller-marker".to_string(),
                    pattern: ".controller".to_string(),
                    category: "runtime-state".to_string(),
                    reason: "Root deploy marker (the 41-byte commit id written by deploy/controller.sh into a deployed controller tree) is runtime state, not an authored project artifact.".to_string(),
                },
                ExclusionRule {
                    id: "secrets-env-dotfile".to_string(),
                    pattern: "**/.env".to_string(),
                    category: "secrets".to_string(),
                    reason: "Environment secret files are never indexed or associated in the feature registry.".to_string(),
                },
                ExclusionRule {
                    id: "secrets-env-variants".to_string(),
                    pattern: "**/.env.*".to_string(),
                    category: "secrets".to_string(),
                    reason: "Environment secret variants are never indexed or associated in the feature registry.".to_string(),
                },
                ExclusionRule {
                    id: "secrets-env-suffix".to_string(),
                    pattern: "**/*.env".to_string(),
                    category: "secrets".to_string(),
                    reason: "Host environment files (such as spawn.env) hold secrets and are excluded.".to_string(),
                },
                ExclusionRule {
                    id: "secrets-private-keys".to_string(),
                    pattern: "**/*.key".to_string(),
                    category: "secrets".to_string(),
                    reason: "Private key material is never indexed in feature coverage.".to_string(),
                },
                ExclusionRule {
                    id: "secrets-pem-certs".to_string(),
                    pattern: "**/*.pem".to_string(),
                    category: "secrets".to_string(),
                    reason: "PEM key/certificate files are excluded from feature coverage.".to_string(),
                },
                ExclusionRule {
                    id: "secrets-gpg-encrypted".to_string(),
                    pattern: "**/*.gpg".to_string(),
                    category: "secrets".to_string(),
                    reason: "GPG-encrypted secrets and backup artifacts are excluded from feature coverage.".to_string(),
                },
                ExclusionRule {
                    id: "secrets-directory".to_string(),
                    pattern: "**/secrets/**".to_string(),
                    category: "secrets".to_string(),
                    reason: "Dedicated secrets directories are never crawled or indexed.".to_string(),
                },
                ExclusionRule {
                    id: "os-ds-store".to_string(),
                    pattern: "**/.DS_Store".to_string(),
                    category: "editor-temp".to_string(),
                    reason: "macOS Finder metadata files are transient local artifacts.".to_string(),
                },
                ExclusionRule {
                    id: "editor-swap-files".to_string(),
                    pattern: "**/*.swp".to_string(),
                    category: "editor-temp".to_string(),
                    reason: "Editor swap files are transient local artifacts.".to_string(),
                },
                ExclusionRule {
                    id: "editor-swo-files".to_string(),
                    pattern: "**/*.swo".to_string(),
                    category: "editor-temp".to_string(),
                    reason: "Secondary editor swap files are transient local artifacts.".to_string(),
                },
                ExclusionRule {
                    id: "editor-backup-tilde".to_string(),
                    pattern: "**/*~".to_string(),
                    category: "editor-temp".to_string(),
                    reason: "Editor tilde backup files are transient local artifacts.".to_string(),
                },
                ExclusionRule {
                    id: "rust-backup-files".to_string(),
                    pattern: "**/*.rs.bk".to_string(),
                    category: "editor-temp".to_string(),
                    reason: "Rust formatter backup files (gitignored) are transient local artifacts.".to_string(),
                },
                ExclusionRule {
                    id: "cargo-lock-backup".to_string(),
                    pattern: "Cargo.lock.bak".to_string(),
                    category: "editor-temp".to_string(),
                    reason: "Transient Cargo.lock backup file (gitignored) is outside project coverage.".to_string(),
                },
                ExclusionRule {
                    id: "transient-logs".to_string(),
                    pattern: "**/*.log".to_string(),
                    category: "transient".to_string(),
                    reason: "Runtime log files are transient diagnostic outputs.".to_string(),
                },
                ExclusionRule {
                    id: "transient-tmp".to_string(),
                    pattern: "**/*.tmp".to_string(),
                    category: "transient".to_string(),
                    reason: "Temporary scratch files are outside durable project coverage.".to_string(),
                },
                ExclusionRule {
                    id: "transient-part".to_string(),
                    pattern: "**/*.part".to_string(),
                    category: "transient".to_string(),
                    reason: "Partial transfer/download files are transient artifacts.".to_string(),
                },
                ExclusionRule {
                    id: "tool-caches".to_string(),
                    pattern: "**/.cache/**".to_string(),
                    category: "build-cache".to_string(),
                    reason: "Tool caches are derived and rebuildable.".to_string(),
                },
                ExclusionRule {
                    id: "python-bytecode-cache".to_string(),
                    pattern: "**/__pycache__/**".to_string(),
                    category: "build-cache".to_string(),
                    reason: "Python bytecode caches are derived build artifacts.".to_string(),
                },
            ],
        }
    }
}

/// Kind of feature node in the registry graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureKind {
    Domain,
    Capability,
    Initiative,
    Slice,
    ResearchInitiative,
    Subsystem,
}

impl FeatureKind {
    /// Whether this kind is permitted to sit at the top level without a parent
    /// or incoming `contains` relation.
    pub const fn may_be_root(self) -> bool {
        matches!(
            self,
            Self::Domain | Self::Initiative | Self::ResearchInitiative
        )
    }
}

/// Typed directed relation between two feature nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    DependsOn,
    Blocks,
    Implements,
    Extends,
    RelatesTo,
    Supersedes,
    Verifies,
    DocumentedBy,
    Contains,
}

/// One typed edge from a feature node to another feature node by ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureRelation {
    pub kind: RelationKind,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Provenance evidence attached to a feature node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureProvenance {
    #[serde(default)]
    pub objectives: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<String>,
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub commits: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    #[serde(default)]
    pub deploys: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub discussions: Vec<String>,
}

/// One genuine feature node in the project feature map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureNode {
    pub id: String,
    pub title: String,
    pub kind: FeatureKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_note: Option<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub relations: Vec<FeatureRelation>,
    #[serde(default)]
    pub provenance: FeatureProvenance,
}

/// Versioned, JSON-serializable project feature-map registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureRegistry {
    pub schema_version: u32,
    pub initiative_id: String,
    pub boundary: BoundaryPolicy,
    pub features: Vec<FeatureNode>,
}

/// One file observed by the IO adapter inside the project root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryEntry {
    pub path: String,
    pub tracked: bool,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Strictly separated coverage and consistency metrics from a scan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCounts {
    pub total_seen: usize,
    pub in_scope: usize,
    pub covered: usize,
    pub unassociated_wild: usize,
    pub stale_references: usize,
    pub explicit_exclusions: usize,
    pub multi_feature_files: usize,
    pub orphan_features: usize,
    pub invalid_relations: usize,
}

/// One in-scope existing file with zero feature associations ("野文件").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WildFileFinding {
    pub path: String,
    pub tracked: bool,
    pub size_bytes: u64,
    pub reason: String,
    pub evidence: Vec<String>,
    pub suggested_features: Vec<String>,
}

/// One feature-map file link pointing to a file that does not exist in the
/// current project inventory. Kept strictly separate from existing wild files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleFileReference {
    pub path: String,
    pub referencing_features: Vec<String>,
    pub rename_candidates: Vec<String>,
}

/// One file or directory matched by an explicit exclusion rule. Reported
/// separately and never counted as covered or unassociated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedFileEntry {
    pub path: String,
    pub rule_id: String,
    pub category: String,
    pub reason: String,
}

/// Bidirectional many-to-many index between genuine features and in-scope files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidirectionalIndex {
    pub feature_to_files: BTreeMap<String, Vec<String>>,
    pub file_to_features: BTreeMap<String, Vec<String>>,
}

/// Graph and path consistency findings over a [`FeatureRegistry`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsistencyReport {
    pub orphan_features: Vec<String>,
    pub dangling_relations: Vec<String>,
    pub self_links: Vec<String>,
    pub cycle_errors: Vec<String>,
    pub duplicate_feature_ids: Vec<String>,
    pub invalid_paths: Vec<String>,
}

impl ConsistencyReport {
    /// True when the registry graph has no structural or path errors.
    pub fn is_clean(&self) -> bool {
        self.orphan_features.is_empty()
            && self.dangling_relations.is_empty()
            && self.self_links.is_empty()
            && self.cycle_errors.is_empty()
            && self.duplicate_feature_ids.is_empty()
            && self.invalid_paths.is_empty()
    }
}

/// Complete result of a one-click unassociated-file and consistency scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    pub schema_version: u32,
    pub scanned_at_ms: i64,
    pub initiative_id: String,
    pub boundary: BoundaryPolicy,
    pub counts: ScanCounts,
    pub unassociated_files: Vec<WildFileFinding>,
    pub stale_references: Vec<StaleFileReference>,
    pub excluded_files: Vec<ExcludedFileEntry>,
    pub bidirectional_index: BidirectionalIndex,
    pub consistency: ConsistencyReport,
}

impl ScanReport {
    /// True when every in-scope file is associated, no references are stale,
    /// and the feature graph has zero consistency errors.
    pub fn is_clean(&self) -> bool {
        self.unassociated_files.is_empty()
            && self.stale_references.is_empty()
            && self.consistency.is_clean()
    }
}

/// Summary returned after associating an in-scope file with one or more
/// genuine feature nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssociationUpdate {
    pub path: String,
    pub added_to_features: Vec<String>,
    pub already_associated_features: Vec<String>,
    pub all_associated_features: Vec<String>,
}

/// Normalize and validate a repository-relative file path.
///
/// Rejects empty strings, absolute paths, drive prefixes, control characters,
/// empty segments (`//`), trailing slashes, and `.` / `..` traversal segments.
pub fn normalize_repo_path(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("path must not be empty".to_string());
    }
    if raw.chars().any(char::is_control) {
        return Err(format!("path {raw:?} contains a control character"));
    }
    let replaced = raw.replace('\\', "/");
    if replaced.starts_with('/') {
        return Err(format!(
            "path {raw:?} must be repository-relative, not absolute"
        ));
    }
    if replaced.ends_with('/') {
        return Err(format!(
            "path {raw:?} must name a file, not a trailing slash"
        ));
    }
    if replaced.as_bytes().get(1) == Some(&b':')
        && replaced
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
    {
        return Err(format!("path {raw:?} must not contain a drive prefix"));
    }
    let mut parts = Vec::new();
    for segment in replaced.split('/') {
        if segment.is_empty() {
            return Err(format!("path {raw:?} contains an empty path segment"));
        }
        if segment == "." || segment == ".." {
            return Err(format!(
                "path {raw:?} contains traversal segment {segment:?}"
            ));
        }
        parts.push(segment);
    }
    Ok(parts.join("/"))
}

/// Normalize an inventory path, permitting a trailing `/` for an explicitly
/// collapsed excluded directory sentinel (such as `target/` or `.git/`).
fn normalize_inventory_path(raw: &str) -> Result<(String, bool), String> {
    if let Some(trimmed) = raw.strip_suffix('/') {
        let norm = normalize_repo_path(trimmed)?;
        Ok((format!("{norm}/"), true))
    } else {
        Ok((normalize_repo_path(raw)?, false))
    }
}

/// Return the first explicit [`ExclusionRule`] matching `path`, if any.
pub fn match_exclusion<'a>(policy: &'a BoundaryPolicy, path: &str) -> Option<&'a ExclusionRule> {
    let trimmed = path.trim_end_matches('/');
    policy
        .exclusions
        .iter()
        .find(|rule| glob_matches(&rule.pattern, trimmed) || glob_matches(&rule.pattern, path))
}

/// Return the first explicit whole-subtree directory [`ExclusionRule`]
/// (a rule whose pattern ends with `/**`) matching `dir_rel`, if any.
///
/// Filesystem walkers use this to collapse excluded subtrees (such as `.git/`,
/// `target/`, `node_modules/`, `secrets/`, or `rsi/.gate-owner/`) without ever
/// calling `read_dir` inside them.
pub fn match_directory_exclusion<'a>(
    policy: &'a BoundaryPolicy,
    dir_rel: &str,
) -> Option<&'a ExclusionRule> {
    let trimmed = dir_rel.trim_end_matches('/');
    let probe = format!("{trimmed}/__probe__");
    policy.exclusions.iter().find(|rule| {
        rule.pattern.ends_with("/**")
            && (glob_matches(&rule.pattern, trimmed) || glob_matches(&rule.pattern, &probe))
    })
}

/// Pure glob matcher over `/`-separated relative paths supporting `*`, `?`,
/// and `**` (zero or more segments).
///
/// Patterns ending in `/**` also match the directory itself (e.g., `target/**`
/// matches `target`, `target/`, and `target/debug/app`).
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    if let Some(prefix) = pattern.strip_suffix("/**")
        && glob_matches(prefix, path)
    {
        return true;
    }
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let candidate_parts: Vec<&str> = path.split('/').collect();
    match_segments(&pattern_parts, &candidate_parts)
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match (pat.first(), path.first()) {
        (None, None) => true,
        (Some(&"**"), _) => {
            // `**` matches zero segments or consumes one path segment.
            match_segments(&pat[1..], path) || (!path.is_empty() && match_segments(pat, &path[1..]))
        }
        (None, Some(_)) | (Some(_), None) => false,
        (Some(p), Some(s)) => segment_matches(p, s) && match_segments(&pat[1..], &path[1..]),
    }
}

fn segment_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx: usize = 0;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(star) = star_idx {
            pi = star + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Validate a [`FeatureRegistry`] for structural consistency: duplicate IDs,
/// invalid or excluded file paths, dangling parents/relations, self-links,
/// cycles in `parent` / `depends_on` / `contains`, and orphan features.
pub fn validate_registry(registry: &FeatureRegistry) -> ConsistencyReport {
    let mut report = ConsistencyReport::default();
    let known_ids = collect_feature_ids(registry, &mut report);

    let mut parent_edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut depends_edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut contains_edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut hierarchy_children: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for feature in &registry.features {
        validate_feature_files(feature, &registry.boundary, &mut report);
        validate_feature_links(
            feature,
            &known_ids,
            &mut hierarchy_children,
            &mut parent_edges,
            &mut depends_edges,
            &mut contains_edges,
            &mut report,
        );
    }

    // Root-reachability orphan check: a non-root feature (capability, slice,
    // subsystem) is an orphan unless it is reachable from at least one valid
    // root node (`domain`, `initiative`, `research-initiative`) via valid
    // `parent` or `contains` hierarchy edges. This catches direct orphans,
    // children of orphans, dangling/self-parent orphans, and isolated cycles.
    let mut root_anchored: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = Vec::new();
    for feature in &registry.features {
        if !feature.id.trim().is_empty()
            && feature.kind.may_be_root()
            && root_anchored.insert(feature.id.clone())
        {
            queue.push(feature.id.clone());
        }
    }
    while let Some(curr) = queue.pop() {
        if let Some(children) = hierarchy_children.get(&curr) {
            for child in children {
                if root_anchored.insert(child.clone()) {
                    queue.push(child.clone());
                }
            }
        }
    }
    for feature in &registry.features {
        if !feature.id.trim().is_empty()
            && !feature.kind.may_be_root()
            && !root_anchored.contains(&feature.id)
        {
            report.orphan_features.push(feature.id.clone());
        }
    }

    detect_cycles(
        "parent",
        &known_ids,
        &parent_edges,
        &mut report.cycle_errors,
    );
    detect_cycles(
        "depends_on",
        &known_ids,
        &depends_edges,
        &mut report.cycle_errors,
    );
    detect_cycles(
        "contains",
        &known_ids,
        &contains_edges,
        &mut report.cycle_errors,
    );

    sort_and_dedup(&mut report.orphan_features);
    sort_and_dedup(&mut report.dangling_relations);
    sort_and_dedup(&mut report.self_links);
    sort_and_dedup(&mut report.cycle_errors);
    sort_and_dedup(&mut report.invalid_paths);

    report
}

fn sort_and_dedup(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn collect_feature_ids(
    registry: &FeatureRegistry,
    report: &mut ConsistencyReport,
) -> BTreeSet<String> {
    let mut known_ids: BTreeSet<String> = BTreeSet::new();
    let mut dup_ids: BTreeSet<String> = BTreeSet::new();
    for feature in &registry.features {
        if feature.id.trim().is_empty() {
            report
                .invalid_paths
                .push("feature has an empty id".to_string());
            continue;
        }
        if !known_ids.insert(feature.id.clone()) {
            dup_ids.insert(feature.id.clone());
        }
    }
    report.duplicate_feature_ids = dup_ids.into_iter().collect();
    known_ids
}

/// Returns true when a `provenance.artifacts` or `provenance.tests` entry looks
/// like a repository-relative file path (rather than a URL, free-form command,
/// or non-path identifier).
///
/// `provenance.commits` (git SHAs) and other non-path provenance fields are
/// never passed to this check.
pub fn looks_like_repo_path(raw: &str) -> bool {
    const PATH_EXTS: &[&str] = &[
        ".md", ".rs", ".ts", ".tsx", ".js", ".mjs", ".json", ".toml", ".yaml", ".yml", ".sh",
        ".py", ".sql", ".txt", ".css", ".html", ".env", ".key", ".pem", ".gpg",
    ];
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) || trimmed.contains("://") {
        return false;
    }
    if trimmed.contains('/')
        || trimmed.contains('\\')
        || (trimmed.starts_with('.') && trimmed.len() > 1)
    {
        return true;
    }
    PATH_EXTS.iter().any(|ext| trimmed.ends_with(ext))
}

fn validate_feature_files(
    feature: &FeatureNode,
    boundary: &BoundaryPolicy,
    report: &mut ConsistencyReport,
) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for raw_path in &feature.files {
        match normalize_repo_path(raw_path) {
            Ok(norm) => {
                if !seen.insert(norm.clone()) {
                    report
                        .invalid_paths
                        .push(format!("{}: duplicate file path {norm:?}", feature.id));
                }
                if let Some(rule) = match_exclusion(boundary, &norm) {
                    report.invalid_paths.push(format!(
                        "{}: file {norm:?} matches exclusion rule {:?} ({})",
                        feature.id, rule.id, rule.reason
                    ));
                }
            }
            Err(err) => {
                report.invalid_paths.push(format!(
                    "{}: invalid file path {raw_path:?}: {err}",
                    feature.id
                ));
            }
        }
    }

    for (field, entries) in [
        ("artifacts", &feature.provenance.artifacts),
        ("tests", &feature.provenance.tests),
    ] {
        for raw_path in entries {
            if !looks_like_repo_path(raw_path) {
                continue;
            }
            match normalize_repo_path(raw_path) {
                Ok(norm) => {
                    if let Some(rule) = match_exclusion(boundary, &norm) {
                        report.invalid_paths.push(format!(
                            "{}: provenance.{field} path {norm:?} matches exclusion rule {:?} ({})",
                            feature.id, rule.id, rule.reason
                        ));
                    }
                }
                Err(err) => {
                    report.invalid_paths.push(format!(
                        "{}: invalid provenance.{field} path {raw_path:?}: {err}",
                        feature.id
                    ));
                }
            }
        }
    }
}

fn validate_feature_links(
    feature: &FeatureNode,
    known_ids: &BTreeSet<String>,
    hierarchy_children: &mut BTreeMap<String, Vec<String>>,
    parent_edges: &mut BTreeMap<String, Vec<String>>,
    depends_edges: &mut BTreeMap<String, Vec<String>>,
    contains_edges: &mut BTreeMap<String, Vec<String>>,
    report: &mut ConsistencyReport,
) {
    if let Some(parent) = &feature.parent {
        if parent == &feature.id {
            report
                .self_links
                .push(format!("{}: parent points to itself", feature.id));
        } else if !known_ids.contains(parent) {
            report
                .dangling_relations
                .push(format!("{}: parent {parent:?} does not exist", feature.id));
        } else {
            parent_edges
                .entry(feature.id.clone())
                .or_default()
                .push(parent.clone());
            hierarchy_children
                .entry(parent.clone())
                .or_default()
                .push(feature.id.clone());
        }
    }

    for rel in &feature.relations {
        if rel.target == feature.id {
            report.self_links.push(format!(
                "{}: relation {:?} points to itself",
                feature.id, rel.kind
            ));
            continue;
        }
        if !known_ids.contains(&rel.target) {
            report.dangling_relations.push(format!(
                "{}: relation {:?} target {:?} does not exist",
                feature.id, rel.kind, rel.target
            ));
            continue;
        }
        match rel.kind {
            RelationKind::DependsOn => {
                depends_edges
                    .entry(feature.id.clone())
                    .or_default()
                    .push(rel.target.clone());
            }
            RelationKind::Contains => {
                hierarchy_children
                    .entry(feature.id.clone())
                    .or_default()
                    .push(rel.target.clone());
                contains_edges
                    .entry(feature.id.clone())
                    .or_default()
                    .push(rel.target.clone());
            }
            _ => {}
        }
    }
}

fn detect_cycles(
    label: &str,
    nodes: &BTreeSet<String>,
    edges: &BTreeMap<String, Vec<String>>,
    out: &mut Vec<String>,
) {
    // 0 = unvisited, 1 = visiting (on stack), 2 = done
    let mut color: BTreeMap<&str, u8> = BTreeMap::new();
    let mut stack: Vec<&str> = Vec::new();

    for start in nodes {
        if color.get(start.as_str()).copied().unwrap_or(0) != 0 {
            continue;
        }
        dfs_cycle(label, start.as_str(), edges, &mut color, &mut stack, out);
    }
}

fn dfs_cycle<'a>(
    label: &str,
    node: &'a str,
    edges: &'a BTreeMap<String, Vec<String>>,
    color: &mut BTreeMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
    out: &mut Vec<String>,
) {
    color.insert(node, 1);
    stack.push(node);

    if let Some(neighbors) = edges.get(node) {
        for next in neighbors {
            match color.get(next.as_str()).copied().unwrap_or(0) {
                0 => dfs_cycle(label, next.as_str(), edges, color, stack, out),
                1 => {
                    if let Some(pos) = stack.iter().position(|entry| *entry == next.as_str()) {
                        let mut cycle: Vec<&str> = stack[pos..].to_vec();
                        cycle.push(next.as_str());
                        out.push(format!("{label} cycle: {}", cycle.join(" -> ")));
                    }
                }
                _ => {}
            }
        }
    }

    stack.pop();
    color.insert(node, 2);
}

/// Run a complete, deterministic coverage and consistency scan comparing a
/// [`FeatureRegistry`] against an observed project file inventory.
///
/// Guarantees:
/// - Explicitly excluded files are reported in `excluded_files` and are NEVER
///   counted in `counts.in_scope` or `counts.covered`, and NEVER listed in
///   `unassociated_files`.
/// - Existing in-scope files with zero associations are listed in
///   `unassociated_files` (wild files / "野文件") with reason, evidence, and
///   suggested feature nodes.
/// - Registry references to non-existent files are reported separately in
///   `stale_references` with rename candidates.
/// - `bidirectional_index` exposes both `feature_to_files` and
///   `file_to_features` for true many-to-many associations.
pub fn scan_coverage(
    registry: &FeatureRegistry,
    inventory: &[InventoryEntry],
    scanned_at_ms: i64,
) -> ScanReport {
    let mut consistency = validate_registry(registry);
    let (in_scope_map, excluded_files) =
        partition_inventory(&registry.boundary, inventory, &mut consistency);
    let (feature_to_files, file_to_features, stale_map) =
        index_registry_files(registry, &in_scope_map);

    let unassociated_files =
        collect_wild_files(&in_scope_map, &file_to_features, &feature_to_files);
    let unassociated_paths: BTreeSet<&str> =
        unassociated_files.iter().map(|f| f.path.as_str()).collect();
    let stale_references: Vec<StaleFileReference> = stale_map
        .into_iter()
        .map(|(stale_path, referencing)| StaleFileReference {
            rename_candidates: find_rename_candidates(
                &stale_path,
                &in_scope_map,
                &unassociated_paths,
            ),
            path: stale_path,
            referencing_features: referencing.into_iter().collect(),
        })
        .collect();

    let bidirectional_index = BidirectionalIndex {
        feature_to_files: feature_to_files
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect(),
        file_to_features: file_to_features
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect(),
    };

    sort_and_dedup(&mut consistency.invalid_paths);

    let multi_feature_files = bidirectional_index
        .file_to_features
        .values()
        .filter(|features| features.len() >= 2)
        .count();
    let invalid_relations = consistency.dangling_relations.len()
        + consistency.self_links.len()
        + consistency.cycle_errors.len();

    let counts = ScanCounts {
        total_seen: in_scope_map.len() + excluded_files.len(),
        in_scope: in_scope_map.len(),
        covered: bidirectional_index.file_to_features.len(),
        unassociated_wild: unassociated_files.len(),
        stale_references: stale_references.len(),
        explicit_exclusions: excluded_files.len(),
        multi_feature_files,
        orphan_features: consistency.orphan_features.len(),
        invalid_relations,
    };

    ScanReport {
        schema_version: SCHEMA_VERSION,
        scanned_at_ms,
        initiative_id: registry.initiative_id.clone(),
        boundary: registry.boundary.clone(),
        counts,
        unassociated_files,
        stale_references,
        excluded_files,
        bidirectional_index,
        consistency,
    }
}

fn partition_inventory(
    boundary: &BoundaryPolicy,
    inventory: &[InventoryEntry],
    consistency: &mut ConsistencyReport,
) -> (BTreeMap<String, InventoryEntry>, Vec<ExcludedFileEntry>) {
    let mut in_scope_map: BTreeMap<String, InventoryEntry> = BTreeMap::new();
    let mut excluded_map: BTreeMap<String, ExcludedFileEntry> = BTreeMap::new();

    for entry in inventory {
        let Ok((norm, is_dir_sentinel)) = normalize_inventory_path(&entry.path) else {
            consistency
                .invalid_paths
                .push(format!("inventory path {:?} is invalid", entry.path));
            continue;
        };
        if let Some(rule) = match_exclusion(boundary, &norm) {
            excluded_map
                .entry(norm.clone())
                .or_insert_with(|| ExcludedFileEntry {
                    path: norm,
                    rule_id: rule.id.clone(),
                    category: rule.category.clone(),
                    reason: rule.reason.clone(),
                });
            continue;
        }
        if is_dir_sentinel {
            continue;
        }
        in_scope_map.insert(
            norm.clone(),
            InventoryEntry {
                path: norm,
                tracked: entry.tracked,
                size_bytes: entry.size_bytes,
                sha256: entry.sha256.clone(),
            },
        );
    }

    (in_scope_map, excluded_map.into_values().collect())
}

type IndexedFiles = (
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
);

fn index_registry_files(
    registry: &FeatureRegistry,
    in_scope_map: &BTreeMap<String, InventoryEntry>,
) -> IndexedFiles {
    let mut feature_to_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut file_to_features: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut stale_map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for feature in &registry.features {
        let files_set = feature_to_files.entry(feature.id.clone()).or_default();
        for raw_path in &feature.files {
            let Ok(norm) = normalize_repo_path(raw_path) else {
                continue;
            };
            if match_exclusion(&registry.boundary, &norm).is_some() {
                continue;
            }
            if in_scope_map.contains_key(&norm) {
                files_set.insert(norm.clone());
                file_to_features
                    .entry(norm)
                    .or_default()
                    .insert(feature.id.clone());
            } else {
                stale_map
                    .entry(norm)
                    .or_default()
                    .insert(feature.id.clone());
            }
        }

        // Stale-check repo-relative path entries in `provenance.artifacts` and
        // `provenance.tests` without ever counting provenance links as file
        // coverage in `bidirectional_index` (and never treating `provenance.commits`
        // SHAs as file paths).
        for raw_path in feature
            .provenance
            .artifacts
            .iter()
            .chain(feature.provenance.tests.iter())
        {
            if !looks_like_repo_path(raw_path) {
                continue;
            }
            let Ok(norm) = normalize_repo_path(raw_path) else {
                continue;
            };
            if match_exclusion(&registry.boundary, &norm).is_some() {
                continue;
            }
            if !in_scope_map.contains_key(&norm) {
                stale_map
                    .entry(norm)
                    .or_default()
                    .insert(feature.id.clone());
            }
        }
    }

    (feature_to_files, file_to_features, stale_map)
}

fn collect_wild_files(
    in_scope_map: &BTreeMap<String, InventoryEntry>,
    file_to_features: &BTreeMap<String, BTreeSet<String>>,
    feature_to_files: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<WildFileFinding> {
    let mut unassociated_files = Vec::new();
    for (path, entry) in in_scope_map {
        if file_to_features.contains_key(path) {
            continue;
        }
        let reason = if entry.tracked {
            "In-scope git-tracked file has no feature association in the registry".to_string()
        } else {
            "In-scope untracked file exists on disk with no feature association in the registry"
                .to_string()
        };
        let top_dir = path
            .split('/')
            .next()
            .filter(|_| path.contains('/'))
            .unwrap_or("(repo-root)");
        let evidence = vec![
            format!("git_tracked={}", entry.tracked),
            format!("size_bytes={}", entry.size_bytes),
            format!("top_dir={top_dir}"),
            "matched_exclusion=none".to_string(),
        ];
        unassociated_files.push(WildFileFinding {
            path: path.clone(),
            tracked: entry.tracked,
            size_bytes: entry.size_bytes,
            reason,
            evidence,
            suggested_features: suggest_features_for_path(path, feature_to_files),
        });
    }
    unassociated_files
}

/// Associate an existing in-scope file with one or more genuine feature nodes
/// in `registry` (many-to-many supported).
///
/// Refuses invalid paths, traversal paths, explicitly excluded/secret paths,
/// files absent from `inventory`, empty feature lists, and unknown feature IDs.
/// Never touches or deletes files on disk.
pub fn associate_file(
    registry: &mut FeatureRegistry,
    path: &str,
    feature_ids: &[String],
    inventory: &[InventoryEntry],
) -> Result<AssociationUpdate, String> {
    let norm = normalize_repo_path(path)?;
    if let Some(rule) = match_exclusion(&registry.boundary, &norm) {
        return Err(format!(
            "cannot associate excluded file {norm:?}: matched rule {:?} ({})",
            rule.id, rule.reason
        ));
    }

    let exists_in_inventory = inventory
        .iter()
        .any(|entry| normalize_repo_path(&entry.path).is_ok_and(|candidate| candidate == norm));
    if !exists_in_inventory {
        return Err(format!(
            "file {norm:?} does not exist in the current project inventory"
        ));
    }

    let mut requested: Vec<String> = Vec::new();
    for fid in feature_ids {
        let trimmed = fid.trim();
        if trimmed.is_empty() {
            return Err("feature id must not be empty".to_string());
        }
        if !requested.iter().any(|existing| existing == trimmed) {
            requested.push(trimmed.to_string());
        }
    }
    if requested.is_empty() {
        return Err("at least one genuine feature id is required".to_string());
    }

    let mut id_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for feature in &registry.features {
        *id_counts.entry(feature.id.as_str()).or_insert(0) += 1;
    }
    for fid in &requested {
        match id_counts.get(fid.as_str()).copied().unwrap_or(0) {
            0 => return Err(format!("unknown feature id {fid:?}")),
            1 => {}
            _ => {
                return Err(format!(
                    "ambiguous duplicate feature id {fid:?} in registry"
                ));
            }
        }
    }

    let mut added_to_features = Vec::new();
    let mut already_associated_features = Vec::new();

    for feature in &mut registry.features {
        if !requested.iter().any(|fid| fid == &feature.id) {
            continue;
        }
        let already = feature
            .files
            .iter()
            .any(|existing| normalize_repo_path(existing).is_ok_and(|p| p == norm));
        if already {
            already_associated_features.push(feature.id.clone());
        } else {
            feature.files.push(norm.clone());
            feature.files.sort();
            feature.files.dedup();
            added_to_features.push(feature.id.clone());
        }
    }

    let mut all_associated_features: Vec<String> = registry
        .features
        .iter()
        .filter(|feature| {
            feature
                .files
                .iter()
                .any(|existing| normalize_repo_path(existing).is_ok_and(|p| p == norm))
        })
        .map(|feature| feature.id.clone())
        .collect();
    all_associated_features.sort();
    added_to_features.sort();
    already_associated_features.sort();

    Ok(AssociationUpdate {
        path: norm,
        added_to_features,
        already_associated_features,
        all_associated_features,
    })
}

fn suggest_features_for_path(
    path: &str,
    feature_to_files: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<String> {
    let target_dir_parts: Vec<&str> = path.split('/').collect();
    let target_dir = if target_dir_parts.len() > 1 {
        &target_dir_parts[..target_dir_parts.len() - 1]
    } else {
        &[][..]
    };

    let mut scores: Vec<(usize, usize, &str)> = Vec::new();
    for (feature_id, files) in feature_to_files {
        let mut best_prefix = 0usize;
        let mut dir_matches = 0usize;
        for file in files {
            let parts: Vec<&str> = file.split('/').collect();
            let file_dir = if parts.len() > 1 {
                &parts[..parts.len() - 1]
            } else {
                &[][..]
            };
            let shared = target_dir
                .iter()
                .zip(file_dir.iter())
                .take_while(|(a, b)| a == b)
                .count();
            if shared > best_prefix {
                best_prefix = shared;
            }
            if shared > 0 && shared == target_dir.len() {
                dir_matches += 1;
            }
        }
        if best_prefix > 0 {
            scores.push((best_prefix, dir_matches, feature_id.as_str()));
        }
    }

    scores.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.cmp(b.2))
    });
    scores
        .into_iter()
        .take(5)
        .map(|(_, _, fid)| fid.to_string())
        .collect()
}

fn find_rename_candidates(
    stale_path: &str,
    in_scope_map: &BTreeMap<String, InventoryEntry>,
    unassociated_paths: &BTreeSet<&str>,
) -> Vec<String> {
    let stale_name = stale_path.rsplit('/').next().unwrap_or(stale_path);
    let stale_dir = stale_path.rsplit_once('/').map_or("", |(d, _)| d);
    let stale_stem = stale_name.split('.').next().unwrap_or(stale_name);

    let mut scored: Vec<(u8, &str)> = Vec::new();
    for candidate in in_scope_map.keys() {
        let cand_name = candidate.rsplit('/').next().unwrap_or(candidate.as_str());
        let cand_dir = candidate.rsplit_once('/').map_or("", |(d, _)| d);
        let cand_stem = cand_name.split('.').next().unwrap_or(cand_name);
        let is_unassociated = unassociated_paths.contains(candidate.as_str());

        let score = if cand_name == stale_name {
            if is_unassociated { 4 } else { 3 }
        } else if !stale_dir.is_empty() && cand_dir == stale_dir && cand_stem == stale_stem {
            if is_unassociated { 3 } else { 2 }
        } else {
            u8::from(is_unassociated && !stale_dir.is_empty() && cand_dir == stale_dir)
        };
        if score > 0 {
            scored.push((score, candidate.as_str()));
        }
    }

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored
        .into_iter()
        .take(5)
        .map(|(_, path)| path.to_string())
        .collect()
}

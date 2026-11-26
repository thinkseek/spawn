//! Code dependency truth for the feature map (GitHub issue #100).
//!
//! Three parts, all pure (no IO, no clock: the caller passes today's date):
//!
//! - the code graph model the source extractors fill ([`CodeGraph`],
//!   [`FileEdge`], [`Unit`], [`FileInfo`], [`Diagnostic`]);
//! - [`project`]: file edges projected onto the registry's features. Files are
//!   many-to-many, so one edge between two shared files becomes one feature
//!   edge between the two owner sets (`from_any_of`, `to_any_of`), never one
//!   per pair;
//! - [`truth`]: the dependency-truth findings (`under_declared`,
//!   `implied_only`, `test_only_undeclared`, `ghost`, `inverted`,
//!   `unverifiable`, `static_cycle`, `basis_without_note`), the obligations of
//!   the `kernel` and `entrypoint` roles (`kernel_leak`,
//!   `entrypoint_imported`), and waivers (`waiver_unused`).
//!
//! Missing declarations are judged strictly (exact owner sets, production
//! edges only) and ghosts leniently (any edge between the two subtrees, any
//! scope), so that each kind of finding errs toward silence.
//!
//! The registry stays at schema version 1: roles, relation bases and waivers
//! arrive in a separate [`TruthPolicy`] until the registry learns them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::feature_coverage::{
    FeatureKind, FeatureNode, FeatureRegistry, FeatureRelation, RelationKind, glob_matches,
};

/// Version of the [`TruthReport`] JSON shape.
pub const TRUTH_SCHEMA_VERSION: u32 = 1;

/// At most this many evidence edges are kept on one finding;
/// `evidence_total` holds the real count.
pub const EVIDENCE_LIMIT: usize = 5;

/// What kind of package a [`Unit`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    CargoPackage,
    NpmPackage,
    PythonPackage,
}

/// One package the build system knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    /// `cargo:spawn-core`, `npm:@acme/domain`, `py:acme`.
    pub id: String,
    pub kind: UnitKind,
    /// Repository-relative directory (empty for the repository root).
    pub root: String,
    /// Binary roots and package entry points.
    #[serde(default)]
    pub entry_files: Vec<String>,
    /// In-repo units its manifest depends on.
    #[serde(default)]
    pub declared_units: Vec<String>,
}

/// How one file reaches another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A Rust `use` item.
    Use,
    /// A Rust path in code, types, patterns, attributes or a macro body.
    Path,
    /// A Rust `mod name;` declaration (structural).
    Mod,
    /// A re-export (`pub use`, `export ... from`): structural.
    Reexport,
    /// `include!`, `include_str!`, `include_bytes!`.
    Include,
    Import,
    ImportType,
    DynamicImport,
    Require,
    PyImport,
    PyFrom,
    Asset,
}

impl EdgeKind {
    /// Structural edges are kept in the graph but are not dependencies: a
    /// `lib.rs` that declares 20 modules does not depend on 20 features.
    pub const fn is_structural(self) -> bool {
        matches!(self, Self::Mod | Self::Reexport)
    }

    /// The JSON spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Path => "path",
            Self::Mod => "mod",
            Self::Reexport => "reexport",
            Self::Include => "include",
            Self::Import => "import",
            Self::ImportType => "import_type",
            Self::DynamicImport => "dynamic_import",
            Self::Require => "require",
            Self::PyImport => "py_import",
            Self::PyFrom => "py_from",
            Self::Asset => "asset",
        }
    }
}

/// Which build an edge belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Prod,
    Test,
    Build,
}

impl Scope {
    /// The JSON spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prod => "prod",
            Self::Test => "test",
            Self::Build => "build",
        }
    }
}

/// How exactly an edge's target is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Precision {
    /// The file that declares the named item.
    Item,
    /// The file of the module the path stops in.
    Module,
    /// Only the package is known.
    Package,
    /// A guess: no declaration, re-export or glob import of the module
    /// names it, but one of its item-level macro calls mentions the name
    /// (`label!(SessionId, ...)`), which may generate it.
    Macro,
}

impl Precision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Module => "module",
            Self::Package => "package",
            Self::Macro => "macro",
        }
    }
}

/// One file-to-file edge, with the line it starts on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub scope: Scope,
    /// 1-based line in `from`.
    pub line: u32,
    /// The re-exporting file the reference passed through, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub precision: Precision,
}

/// The language an extractor read a file as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Rust,
    Typescript,
    Javascript,
    Python,
}

impl Language {
    /// The JSON spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Typescript => "typescript",
            Self::Javascript => "javascript",
            Self::Python => "python",
        }
    }
}

/// What a file is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileClass {
    Source,
    Test,
    Doc,
    Config,
    Data,
    Script,
    Asset,
}

/// What the extractor learned about one source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub language: Language,
    /// The [`Unit::id`] the file belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub class: FileClass,
    /// A target root (binary, library, test, build script).
    pub is_entry: bool,
    /// Reached by a module tree from a target root.
    pub in_module_tree: bool,
    /// The extractor parsed it; `false` lists an `unparsed` diagnostic.
    pub parsed: bool,
}

/// Why the extractor could not account for something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    /// A reference that names an in-repo target that is not there.
    Unresolved,
    /// Neither the parser nor the token fallback could read the file.
    Unparsed,
    /// The parser refused the file; its paths came from a token walk.
    TokenFallback,
    /// A file or construct no extractor reads.
    Unsupported,
    /// A file not read (too large, not UTF-8).
    Skipped,
    /// A source file under a unit that no module tree reaches.
    OrphanSource,
    /// A manifest the built-in reader does not understand.
    ManifestUnsupported,
    /// A dynamic import whose argument is not a literal.
    DynamicUnresolved,
    /// An explicit import whose last segments name nothing the module
    /// reached declares (a missing item, or one a macro generates): the edge
    /// goes to that module's file at `module` precision.
    UnresolvedItem,
    /// A chain of re-exports longer than the extractor follows: no edge is
    /// claimed.
    ReexportTooDeep,
}

/// One extractor diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub message: String,
}

/// The whole code graph of one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeGraph {
    pub units: Vec<Unit>,
    pub edges: Vec<FileEdge>,
    pub files: BTreeMap<String, FileInfo>,
    pub diagnostics: Vec<Diagnostic>,
    /// References that resolved outside the repository (`std`, crates.io).
    #[serde(default)]
    pub external_imports: usize,
}

/// The exemption a feature trades for an obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A shared foundation: anyone may use it undeclared; it may depend
    /// only on other kernels.
    Kernel,
    /// A composition root: it may use anything undeclared; nothing should
    /// import it.
    Entrypoint,
}

/// What a declared `depends_on` rests on. Only `code` is checked against
/// the graph.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    #[default]
    Code,
    Runtime,
    Data,
    Build,
    Process,
}

/// The basis of one declared `from depends_on to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationBasis {
    pub from: String,
    pub to: String,
    pub basis: Basis,
    #[serde(default)]
    pub note: String,
}

/// A finding kept in the report but kept out of the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiver {
    pub code: FindingCode,
    /// Every one of these must be named by the finding.
    #[serde(default)]
    pub features: Vec<String>,
    /// Must be among the finding's `from_any_of` (or `features`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Must be among the finding's `to_any_of` (or `features`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Required: a waiver without a reason waives nothing.
    pub reason: String,
    /// `YYYY-MM-DD`, inclusive; after it the waiver stops applying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// The per-project inputs of [`truth`] besides the registry and the graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruthPolicy {
    #[serde(default)]
    pub roles: BTreeMap<String, Role>,
    #[serde(default)]
    pub bases: Vec<RelationBasis>,
    #[serde(default)]
    pub waivers: Vec<Waiver>,
}

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warn,
    Info,
}

/// What a finding says. Each code has one fixed [`Severity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCode {
    /// A production cross-feature edge no `depends_on` declares (漏报).
    UnderDeclared,
    /// Not declared directly, but reachable through declared `depends_on`.
    ImpliedOnly,
    /// As `under_declared`, but every edge is test or build scope.
    TestOnlyUndeclared,
    /// A `code` declaration with no evidence edge (虚报).
    Ghost,
    /// A ghost whose evidence runs the other way.
    Inverted,
    /// A `code` declaration where one side owns no analyzed file.
    Unverifiable,
    /// Features that depend on each other in code.
    StaticCycle,
    /// A non-code basis without a note saying what it rests on.
    BasisWithoutNote,
    /// A kernel with a production edge to a feature that is not a kernel.
    KernelLeak,
    /// A production edge into an entrypoint from a feature that is not one.
    EntrypointImported,
    /// A waiver that matched nothing (or expired, or gave no reason).
    WaiverUnused,
}

impl FindingCode {
    /// The fixed severity of this code.
    pub const fn severity(self) -> Severity {
        match self {
            Self::UnderDeclared
            | Self::Ghost
            | Self::Inverted
            | Self::BasisWithoutNote
            | Self::KernelLeak => Severity::Error,
            Self::ImpliedOnly | Self::StaticCycle | Self::EntrypointImported => Severity::Warn,
            Self::TestOnlyUndeclared | Self::Unverifiable | Self::WaiverUnused => Severity::Info,
        }
    }

    /// The JSON spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnderDeclared => "under_declared",
            Self::ImpliedOnly => "implied_only",
            Self::TestOnlyUndeclared => "test_only_undeclared",
            Self::Ghost => "ghost",
            Self::Inverted => "inverted",
            Self::Unverifiable => "unverifiable",
            Self::StaticCycle => "static_cycle",
            Self::BasisWithoutNote => "basis_without_note",
            Self::KernelLeak => "kernel_leak",
            Self::EntrypointImported => "entrypoint_imported",
            Self::WaiverUnused => "waiver_unused",
        }
    }
}

/// One dependency-truth finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub code: FindingCode,
    pub severity: Severity,
    pub waived: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from_any_of: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to_any_of: Vec<String>,
    /// The members of a cycle, or the features a waiver names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    pub evidence_total: usize,
    /// At most [`EVIDENCE_LIMIT`] edges, except a cycle's closing edges,
    /// which are all kept.
    pub evidence: Vec<FileEdge>,
    pub fix: String,
    /// `static_cycle` only: the edge groups it keeps out of
    /// `under_declared` and `implied_only` because no declaration can settle
    /// them. Always present on a cycle, so nothing is hidden silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed: Option<Suppressed>,
}

/// The edge groups one `static_cycle` finding keeps out of the
/// missing-declaration findings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suppressed {
    /// How many edge groups (`pairs.len()`).
    pub total: usize,
    /// How many production file edges they hold.
    pub file_edges: usize,
    pub pairs: Vec<SuppressedPair>,
}

/// One edge group no declaration can settle.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SuppressedPair {
    pub from_any_of: Vec<String>,
    pub to_any_of: Vec<String>,
    pub reason: SuppressionReason,
    pub prod_edges: usize,
}

/// Why declaring an edge group is impossible, for every owner pair of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuppressionReason {
    /// The reverse direction is also production code between the same
    /// single owners: both declarations would be a `depends_on` cycle.
    Mutual,
    /// For some owner pair the target already reaches the source through
    /// declared `depends_on` (the others being mutual): declaring it would
    /// close a cycle.
    ClosesDeclaredCycle,
}

/// Whether a cross-feature edge group is declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStatus {
    /// Some owner (or ancestor) on each side has a direct `depends_on`.
    Declared,
    /// Reachable only through declared `depends_on` chains.
    Implied,
    /// Not declared at all.
    Undeclared,
}

/// All file edges between one pair of owner sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureEdge {
    pub from_any_of: Vec<String>,
    pub to_any_of: Vec<String>,
    pub status: EdgeStatus,
    pub prod_edges: usize,
    pub other_edges: usize,
    /// Every file edge behind this feature edge, sorted.
    pub evidence: Vec<FileEdge>,
}

/// Where each file edge went in the projection (counts of file edges).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSummary {
    pub file_edges: usize,
    /// `mod` and re-export edges: kept, not dependencies.
    pub structural: usize,
    /// An end no feature owns (already a wild file).
    pub unowned: usize,
    /// Both ends share an owner.
    pub internal: usize,
    /// An owner of one end is an ancestor or descendant of an owner of the
    /// other.
    pub hierarchy: usize,
    pub declared: usize,
    pub implied: usize,
    pub undeclared: usize,
}

/// File edges projected onto features.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureGraph {
    pub summary: ProjectionSummary,
    pub edges: Vec<FeatureEdge>,
}

/// Finding counts. `error`, `warn` and `info` count unwaived findings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruthCounts {
    pub error: usize,
    pub warn: usize,
    pub info: usize,
    pub waived: usize,
    /// Every finding, waived or not, by code.
    pub by_code: BTreeMap<String, usize>,
}

impl TruthCounts {
    /// The counts of `findings`.
    pub fn of(findings: &[Finding]) -> Self {
        let mut counts = Self::default();
        for finding in findings {
            *counts
                .by_code
                .entry(finding.code.as_str().to_string())
                .or_default() += 1;
            if finding.waived {
                counts.waived += 1;
                continue;
            }
            match finding.severity {
                Severity::Error => counts.error += 1,
                Severity::Warn => counts.warn += 1,
                Severity::Info => counts.info += 1,
            }
        }
        counts
    }
}

/// The dependency-truth report of one registry against one code graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruthReport {
    pub schema_version: u32,
    pub today: String,
    pub projection: ProjectionSummary,
    pub counts: TruthCounts,
    /// Sorted by severity, code, then ids.
    pub findings: Vec<Finding>,
}

/// The registry's hierarchy, ownership and declarations, indexed.
#[derive(Debug, Clone, Default)]
pub struct FeatureIndex {
    ids: BTreeSet<String>,
    owners: BTreeMap<String, BTreeSet<String>>,
    files_of: BTreeMap<String, BTreeSet<String>>,
    parents: BTreeMap<String, BTreeSet<String>>,
    children: BTreeMap<String, BTreeSet<String>>,
    depends: BTreeMap<String, BTreeSet<String>>,
}

impl FeatureIndex {
    /// Index `registry`: `files` ownership, `parent` and `contains` links,
    /// and `depends_on` declarations.
    pub fn new(registry: &FeatureRegistry) -> Self {
        let mut index = Self::default();
        for node in &registry.features {
            index.ids.insert(node.id.clone());
        }
        for node in &registry.features {
            for file in &node.files {
                index
                    .owners
                    .entry(file.clone())
                    .or_default()
                    .insert(node.id.clone());
                index
                    .files_of
                    .entry(node.id.clone())
                    .or_default()
                    .insert(file.clone());
            }
            if let Some(parent) = &node.parent {
                index.link(parent, &node.id);
            }
            for relation in &node.relations {
                match relation.kind {
                    RelationKind::Contains => index.link(&node.id, &relation.target),
                    RelationKind::DependsOn => {
                        index
                            .depends
                            .entry(node.id.clone())
                            .or_default()
                            .insert(relation.target.clone());
                    }
                    _ => {}
                }
            }
        }
        index
    }

    fn link(&mut self, parent: &str, child: &str) {
        if parent == child {
            return;
        }
        self.parents
            .entry(child.to_string())
            .or_default()
            .insert(parent.to_string());
        self.children
            .entry(parent.to_string())
            .or_default()
            .insert(child.to_string());
    }

    /// Whether the registry has a feature `id`.
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// The features that own `file`.
    pub fn owners(&self, file: &str) -> BTreeSet<String> {
        self.owners.get(file).cloned().unwrap_or_default()
    }

    /// Every ancestor of `id` through `parent` and `contains`, not `id`.
    pub fn ancestors(&self, id: &str) -> BTreeSet<String> {
        walk(id, &self.parents)
    }

    /// `id` and every descendant through `parent` and `contains`.
    pub fn subtree(&self, id: &str) -> BTreeSet<String> {
        let mut all = walk(id, &self.children);
        all.insert(id.to_string());
        all
    }

    /// Whether one is an ancestor of the other (or they are the same).
    pub fn related(&self, a: &str, b: &str) -> bool {
        a == b || self.ancestors(a).contains(b) || self.ancestors(b).contains(a)
    }

    /// `ids` and all their ancestors.
    pub fn with_ancestors<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a String>,
    ) -> BTreeSet<String> {
        let mut all = BTreeSet::new();
        for id in ids {
            all.insert(id.clone());
            all.extend(self.ancestors(id));
        }
        all
    }

    /// The features `from` reaches through declared `depends_on`, where a
    /// declaration on a feature's ancestor counts as the feature's own.
    /// The starting features are included only when a chain returns to
    /// them.
    pub fn declared_reach(&self, from: &BTreeSet<String>) -> BTreeSet<String> {
        let mut reached = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        let push_targets = |id: &str, queue: &mut VecDeque<String>| {
            let mut sources = self.ancestors(id);
            sources.insert(id.to_string());
            for source in sources {
                if let Some(targets) = self.depends.get(&source) {
                    queue.extend(targets.iter().cloned());
                }
            }
        };
        for id in from {
            push_targets(id, &mut queue);
        }
        while let Some(next) = queue.pop_front() {
            if reached.insert(next.clone()) {
                push_targets(&next, &mut queue);
            }
        }
        reached
    }

    /// Whether `from` reaches `to` through declared `depends_on` alone (the
    /// graph `validate_registry` keeps acyclic).
    pub fn declared_path(&self, from: &str, to: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<&str> = VecDeque::from([from]);
        while let Some(id) = queue.pop_front() {
            for next in self.depends.get(id).into_iter().flatten() {
                if next == to {
                    return true;
                }
                if seen.insert(next.as_str()) {
                    queue.push_back(next);
                }
            }
        }
        false
    }

    /// Whether some feature of `from` (or an ancestor) declares `depends_on`
    /// some feature of `to` (or an ancestor).
    fn declares(&self, from: &BTreeSet<String>, to: &BTreeSet<String>) -> bool {
        from.iter().any(|a| {
            self.depends
                .get(a)
                .is_some_and(|targets| targets.iter().any(|b| to.contains(b)))
        })
    }

    /// The files owned by `id`'s subtree.
    fn subtree_files(&self, id: &str) -> BTreeSet<String> {
        self.subtree(id)
            .iter()
            .filter_map(|feature| self.files_of.get(feature))
            .flatten()
            .cloned()
            .collect()
    }
}

fn walk(start: &str, links: &BTreeMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<&str> = VecDeque::from([start]);
    while let Some(id) = queue.pop_front() {
        if let Some(next) = links.get(id) {
            for other in next {
                if other != start && seen.insert(other.clone()) {
                    queue.push_back(other);
                }
            }
        }
    }
    seen
}

/// Project the graph's file edges onto `registry`'s features.
///
/// For each non-structural edge `a -> b`: an end no feature owns is
/// `unowned`; a shared owner makes it `internal`; an owner of one end that
/// is an ancestor or descendant of an owner of the other makes it
/// `hierarchy`. Every other edge joins the [`FeatureEdge`] of its owner
/// sets, which is `declared` when any owner of `a` (or an ancestor)
/// declares `depends_on` any owner of `b` (or an ancestor), `implied` when
/// that is reachable only through a declared chain, else `undeclared`.
pub fn project(graph: &CodeGraph, registry: &FeatureRegistry) -> FeatureGraph {
    project_with(graph, &FeatureIndex::new(registry))
}

fn project_with(graph: &CodeGraph, index: &FeatureIndex) -> FeatureGraph {
    let mut summary = ProjectionSummary {
        file_edges: graph.edges.len(),
        ..ProjectionSummary::default()
    };
    let mut groups: BTreeMap<(Vec<String>, Vec<String>), Vec<FileEdge>> = BTreeMap::new();
    for edge in &graph.edges {
        if edge.kind.is_structural() {
            summary.structural += 1;
            continue;
        }
        let from = index.owners(&edge.from);
        let to = index.owners(&edge.to);
        if from.is_empty() || to.is_empty() || edge.from == edge.to {
            summary.unowned += 1;
            continue;
        }
        if from.intersection(&to).next().is_some() {
            summary.internal += 1;
            continue;
        }
        if from.iter().any(|a| to.iter().any(|b| index.related(a, b))) {
            summary.hierarchy += 1;
            continue;
        }
        groups
            .entry((from.into_iter().collect(), to.into_iter().collect()))
            .or_default()
            .push(edge.clone());
    }
    let mut edges = Vec::with_capacity(groups.len());
    for ((from_any_of, to_any_of), mut evidence) in groups {
        evidence.sort();
        evidence.dedup();
        let from_all = index.with_ancestors(&from_any_of);
        let to_all = index.with_ancestors(&to_any_of);
        let status = if index.declares(&from_all, &to_all) {
            summary.declared += evidence.len();
            EdgeStatus::Declared
        } else if index
            .declared_reach(&from_all)
            .iter()
            .any(|id| to_all.contains(id))
        {
            summary.implied += evidence.len();
            EdgeStatus::Implied
        } else {
            summary.undeclared += evidence.len();
            EdgeStatus::Undeclared
        };
        let prod_edges = evidence.iter().filter(|e| e.scope == Scope::Prod).count();
        edges.push(FeatureEdge {
            other_edges: evidence.len() - prod_edges,
            prod_edges,
            from_any_of,
            to_any_of,
            status,
            evidence,
        });
    }
    FeatureGraph { summary, edges }
}

/// Whether `text` is a `YYYY-MM-DD` date.
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// The dependency-truth report of `registry` against `graph`, with the
/// roles, bases and waivers of `policy`. `today` (`YYYY-MM-DD`, UTC) decides
/// which waivers have expired; the core has no clock.
pub fn truth(
    graph: &CodeGraph,
    registry: &FeatureRegistry,
    policy: &TruthPolicy,
    today: &str,
) -> TruthReport {
    let index = FeatureIndex::new(registry);
    let projected = project_with(graph, &index);
    let roles = RoleIndex {
        index: &index,
        roles: &policy.roles,
    };
    let mut findings = Vec::new();

    let cycles = StaticCycles::new(&projected, &index);
    let mut cycle_findings = cycles.findings();
    edge_findings(
        &projected,
        &index,
        &roles,
        &cycles,
        &mut findings,
        &mut cycle_findings,
    );
    findings.extend(cycle_findings);
    declaration_findings(graph, registry, &index, policy, &mut findings);
    apply_waivers(&policy.waivers, today, &mut findings);

    for finding in &mut findings {
        finding.from_any_of.sort();
        finding.to_any_of.sort();
        finding.features.sort();
        if finding.code != FindingCode::StaticCycle {
            finding.evidence.sort();
            finding.evidence.dedup();
            finding.evidence.truncate(EVIDENCE_LIMIT);
        }
    }
    findings.sort_by(|a, b| {
        (
            a.severity,
            a.code,
            &a.from_any_of,
            &a.to_any_of,
            &a.features,
        )
            .cmp(&(
                b.severity,
                b.code,
                &b.from_any_of,
                &b.to_any_of,
                &b.features,
            ))
    });

    let counts = TruthCounts::of(&findings);
    TruthReport {
        schema_version: TRUTH_SCHEMA_VERSION,
        today: today.to_string(),
        projection: projected.summary,
        counts,
        findings,
    }
}

struct RoleIndex<'a> {
    index: &'a FeatureIndex,
    roles: &'a BTreeMap<String, Role>,
}

impl RoleIndex<'_> {
    /// Whether `id` or one of its ancestors has `role`.
    fn has(&self, id: &str, role: Role) -> bool {
        self.roles.get(id) == Some(&role)
            || self
                .index
                .ancestors(id)
                .iter()
                .any(|ancestor| self.roles.get(ancestor) == Some(&role))
    }

    fn any(&self, ids: &[String], role: Role) -> bool {
        ids.iter().any(|id| self.has(id, role))
    }
}

/// Strongly connected components of the production feature graph.
struct StaticCycles {
    component: BTreeMap<String, usize>,
    components: Vec<Vec<String>>,
    links: BTreeMap<String, BTreeMap<String, FileEdge>>,
}

impl StaticCycles {
    /// Links come only from unambiguous production edges, whose both ends
    /// have a single owner: the co-owners of a shared file make no link.
    fn new(projected: &FeatureGraph, index: &FeatureIndex) -> Self {
        // One link per ordered feature pair, keeping its first file edge.
        let mut links: BTreeMap<String, BTreeMap<String, FileEdge>> = BTreeMap::new();
        for edge in &projected.edges {
            let ([a], [b]) = (edge.from_any_of.as_slice(), edge.to_any_of.as_slice()) else {
                continue;
            };
            let Some(first) = edge.evidence.iter().find(|e| e.scope == Scope::Prod) else {
                continue;
            };
            if !index.related(a, b) {
                links
                    .entry(a.clone())
                    .or_default()
                    .entry(b.clone())
                    .or_insert_with(|| first.clone());
            }
        }
        let components = tarjan(&links);
        let mut component = BTreeMap::new();
        for (number, members) in components.iter().enumerate() {
            for member in members {
                component.insert(member.clone(), number);
            }
        }
        Self {
            component,
            components,
            links,
        }
    }

    /// The cycle `a` and `b` both sit in, if any.
    fn joining(&self, a: &str, b: &str) -> Option<usize> {
        match (self.component.get(a), self.component.get(b)) {
            (Some(x), Some(y)) if x == y => Some(*x),
            _ => None,
        }
    }

    /// Whether production code runs `b -> a` between single owners.
    fn reverse_exists(&self, a: &str, b: &str) -> bool {
        self.links.get(b).is_some_and(|next| next.contains_key(a))
    }

    /// One `static_cycle` per component, with the file edges of its
    /// shortest cycle through its smallest id.
    fn findings(&self) -> Vec<Finding> {
        self.components
            .iter()
            .map(|members| {
                let closing = self.shortest_cycle(members);
                Finding {
                    code: FindingCode::StaticCycle,
                    severity: FindingCode::StaticCycle.severity(),
                    waived: false,
                    from_any_of: Vec::new(),
                    to_any_of: Vec::new(),
                    features: members.clone(),
                    evidence_total: closing.len(),
                    evidence: closing,
                    fix: format!(
                        "features {} depend on each other in code; move the shared code or \
                         invert one direction (a cycle cannot be declared away), or waive it",
                        members.join(", ")
                    ),
                    suppressed: Some(Suppressed::default()),
                }
            })
            .collect()
    }

    fn shortest_cycle(&self, members: &[String]) -> Vec<FileEdge> {
        let inside: BTreeSet<&String> = members.iter().collect();
        let Some(start) = members.first() else {
            return Vec::new();
        };
        // Breadth-first from `start` back to `start`, inside the component.
        let mut previous: BTreeMap<&String, &String> = BTreeMap::new();
        let mut queue: VecDeque<&String> = VecDeque::from([start]);
        let mut closing_from: Option<&String> = None;
        while let Some(node) = queue.pop_front() {
            let Some(next) = self.links.get(node) else {
                continue;
            };
            if next.contains_key(start) {
                closing_from = Some(node);
                break;
            }
            for target in next.keys() {
                if inside.contains(target) && target != start && !previous.contains_key(target) {
                    previous.insert(target, node);
                    queue.push_back(target);
                }
            }
        }
        let Some(mut node) = closing_from else {
            return Vec::new();
        };
        let mut path = vec![start, node];
        while let Some(before) = previous.get(node) {
            path.push(before);
            node = before;
        }
        // `path` runs start, last, ..., first step, start: reverse it.
        path.reverse();
        path.windows(2)
            .filter_map(|pair| {
                self.links
                    .get(pair[0])
                    .and_then(|next| next.get(pair[1]))
                    .cloned()
            })
            .collect()
    }
}

/// Tarjan's strongly connected components with two or more members, each
/// sorted, in order of their smallest member.
fn tarjan(links: &BTreeMap<String, BTreeMap<String, FileEdge>>) -> Vec<Vec<String>> {
    struct State<'a> {
        links: &'a BTreeMap<String, BTreeMap<String, FileEdge>>,
        next: usize,
        number: BTreeMap<&'a str, usize>,
        low: BTreeMap<&'a str, usize>,
        stack: Vec<&'a str>,
        on_stack: BTreeSet<&'a str>,
        found: Vec<Vec<String>>,
    }
    fn visit<'a>(state: &mut State<'a>, node: &'a str) {
        state.number.insert(node, state.next);
        state.low.insert(node, state.next);
        state.next += 1;
        state.stack.push(node);
        state.on_stack.insert(node);
        if let Some(targets) = state.links.get(node) {
            for target in targets.keys() {
                let target = target.as_str();
                if !state.number.contains_key(target) {
                    visit(state, target);
                    let low = state.low[node].min(state.low[target]);
                    state.low.insert(node, low);
                } else if state.on_stack.contains(target) {
                    let low = state.low[node].min(state.number[target]);
                    state.low.insert(node, low);
                }
            }
        }
        if state.low[node] == state.number[node] {
            let mut members = Vec::new();
            while let Some(member) = state.stack.pop() {
                state.on_stack.remove(member);
                members.push(member.to_string());
                if member == node {
                    break;
                }
            }
            if members.len() > 1 {
                members.sort();
                state.found.push(members);
            }
        }
    }
    let mut state = State {
        links,
        next: 0,
        number: BTreeMap::new(),
        low: BTreeMap::new(),
        stack: Vec::new(),
        on_stack: BTreeSet::new(),
        found: Vec::new(),
    };
    for node in links.keys() {
        if !state.number.contains_key(node.as_str()) {
            visit(&mut state, node);
        }
    }
    let mut found = state.found;
    found.sort();
    found
}

fn finding(
    code: FindingCode,
    from_any_of: &[String],
    to_any_of: &[String],
    evidence: Vec<FileEdge>,
    fix: String,
) -> Finding {
    Finding {
        code,
        severity: code.severity(),
        waived: false,
        from_any_of: from_any_of.to_vec(),
        to_any_of: to_any_of.to_vec(),
        features: Vec::new(),
        evidence_total: evidence.len(),
        evidence,
        fix,
        suppressed: None,
    }
}

/// Why no declaration can settle the edge group `from -> to`, if so: for
/// every owner pair, the reverse also runs in production code (mutual), or
/// the target already reaches the source through declared `depends_on`.
fn undeclarable(
    from: &[String],
    to: &[String],
    index: &FeatureIndex,
    cycles: &StaticCycles,
) -> Option<SuppressionReason> {
    let mut all_mutual = true;
    for a in from {
        for b in to {
            if cycles.reverse_exists(a, b) {
                continue;
            }
            if !index.declared_path(b, a) {
                return None;
            }
            all_mutual = false;
        }
    }
    Some(if all_mutual {
        SuppressionReason::Mutual
    } else {
        SuppressionReason::ClosesDeclaredCycle
    })
}

/// Record a suppressed edge group on the `static_cycle` finding of the
/// component it sits in, or on one for its own features (a cycle closed
/// through declarations, or between co-owned files).
fn record_suppressed(
    edge: &FeatureEdge,
    reason: SuppressionReason,
    prod: Vec<FileEdge>,
    cycles: &StaticCycles,
    cycle_findings: &mut Vec<Finding>,
) {
    let pair = SuppressedPair {
        from_any_of: edge.from_any_of.clone(),
        to_any_of: edge.to_any_of.clone(),
        reason,
        prod_edges: prod.len(),
    };
    let component = edge
        .from_any_of
        .iter()
        .find_map(|a| edge.to_any_of.iter().find_map(|b| cycles.joining(a, b)));
    let mut features: Vec<String> = edge
        .from_any_of
        .iter()
        .chain(&edge.to_any_of)
        .cloned()
        .collect();
    features.sort();
    features.dedup();
    let at = match component {
        Some(number) => Some(number),
        None => cycle_findings.iter().position(|f| f.features == features),
    };
    let target = if let Some(at) = at {
        &mut cycle_findings[at]
    } else {
        let mut evidence = prod;
        evidence.sort();
        let evidence_total = evidence.len();
        evidence.truncate(EVIDENCE_LIMIT);
        cycle_findings.push(Finding {
            code: FindingCode::StaticCycle,
            severity: FindingCode::StaticCycle.severity(),
            waived: false,
            from_any_of: Vec::new(),
            to_any_of: Vec::new(),
            fix: format!(
                "declaring depends_on {} -> {} would close a cycle with the declared \
                 depends_on or the reverse code: break the cycle, or waive it",
                edge.from_any_of.join("|"),
                edge.to_any_of.join("|")
            ),
            features,
            evidence_total,
            evidence,
            suppressed: Some(Suppressed::default()),
        });
        cycle_findings.last_mut().expect("just pushed")
    };
    let suppressed = target.suppressed.get_or_insert_with(Suppressed::default);
    suppressed.file_edges += pair.prod_edges;
    suppressed.pairs.push(pair);
    suppressed.pairs.sort();
    suppressed.total = suppressed.pairs.len();
}

/// Findings on code edges: missing declarations and role obligations.
fn edge_findings(
    projected: &FeatureGraph,
    index: &FeatureIndex,
    roles: &RoleIndex<'_>,
    cycles: &StaticCycles,
    findings: &mut Vec<Finding>,
    cycle_findings: &mut Vec<Finding>,
) {
    for edge in &projected.edges {
        let prod: Vec<FileEdge> = edge
            .evidence
            .iter()
            .filter(|e| e.scope == Scope::Prod)
            .cloned()
            .collect();
        let from_kernel = roles.any(&edge.from_any_of, Role::Kernel);
        let to_kernel = roles.any(&edge.to_any_of, Role::Kernel);
        let from_entry = roles.any(&edge.from_any_of, Role::Entrypoint);
        let to_entry = roles.any(&edge.to_any_of, Role::Entrypoint);

        // Obligations hold whether or not the edge is declared.
        if !prod.is_empty() && from_kernel && !to_kernel {
            findings.push(finding(
                FindingCode::KernelLeak,
                &edge.from_any_of,
                &edge.to_any_of,
                prod.clone(),
                "a kernel may depend only on kernels: move this code out of the kernel, or \
                 make the target a kernel"
                    .to_string(),
            ));
        }
        if !prod.is_empty() && to_entry && !from_entry {
            findings.push(finding(
                FindingCode::EntrypointImported,
                &edge.from_any_of,
                &edge.to_any_of,
                prod.clone(),
                "nothing should import an entrypoint: move the shared code out of it".to_string(),
            ));
        }

        if edge.status == EdgeStatus::Declared || to_kernel || from_entry {
            continue;
        }
        if !prod.is_empty()
            && let Some(reason) = undeclarable(&edge.from_any_of, &edge.to_any_of, index, cycles)
        {
            record_suppressed(edge, reason, prod, cycles, cycle_findings);
            continue;
        }
        let pair = format!(
            "{} -> {}",
            edge.from_any_of.join("|"),
            edge.to_any_of.join("|")
        );
        if !prod.is_empty() {
            let (code, fix) = if edge.status == EdgeStatus::Implied {
                (
                    FindingCode::ImpliedOnly,
                    format!("declared only through a chain; add depends_on {pair}"),
                )
            } else {
                (
                    FindingCode::UnderDeclared,
                    format!("add depends_on {pair}, or run: spawn features sync --write"),
                )
            };
            findings.push(finding(code, &edge.from_any_of, &edge.to_any_of, prod, fix));
        } else if edge.status == EdgeStatus::Undeclared {
            findings.push(finding(
                FindingCode::TestOnlyUndeclared,
                &edge.from_any_of,
                &edge.to_any_of,
                edge.evidence.clone(),
                format!("only tests use it; declare depends_on {pair} or a verifies relation"),
            ));
        }
    }
}

/// The basis of `from depends_on relation.target` (`code` unless the
/// policy says otherwise), and whether it carries a note (the policy's, else
/// the relation's own).
fn basis_of(policy: &TruthPolicy, from: &str, relation: &FeatureRelation) -> (Basis, bool) {
    let declared = policy
        .bases
        .iter()
        .find(|basis| basis.from == from && basis.to == relation.target);
    let basis = declared.map_or(Basis::Code, |declared| declared.basis);
    let has_note = declared.is_some_and(|declared| !declared.note.trim().is_empty())
        || relation
            .note
            .as_deref()
            .is_some_and(|note| !note.trim().is_empty());
    (basis, has_note)
}

/// Findings on declared `depends_on`: ghosts, inversions, unverifiable
/// declarations and bases without a note.
fn declaration_findings(
    graph: &CodeGraph,
    registry: &FeatureRegistry,
    index: &FeatureIndex,
    policy: &TruthPolicy,
    findings: &mut Vec<Finding>,
) {
    let analyzed: BTreeSet<&str> = graph
        .files
        .iter()
        .filter(|(_, info)| info.parsed)
        .map(|(path, _)| path.as_str())
        .collect();
    let mut evidence_by_pair: BTreeMap<(&str, &str), Vec<&FileEdge>> = BTreeMap::new();
    for edge in &graph.edges {
        if !edge.kind.is_structural() && edge.from != edge.to {
            evidence_by_pair
                .entry((edge.from.as_str(), edge.to.as_str()))
                .or_default()
                .push(edge);
        }
    }
    let between = |from: &BTreeSet<String>, to: &BTreeSet<String>| -> Vec<FileEdge> {
        let mut found = Vec::new();
        for a in from {
            for b in to {
                if let Some(edges) = evidence_by_pair.get(&(a.as_str(), b.as_str())) {
                    found.extend(edges.iter().map(|edge| (*edge).clone()));
                }
            }
        }
        found.sort();
        found
    };

    for node in &registry.features {
        for relation in &node.relations {
            if relation.kind != RelationKind::DependsOn || !index.ids.contains(&relation.target) {
                continue;
            }
            let from = vec![node.id.clone()];
            let to = vec![relation.target.clone()];
            match basis_of(policy, &node.id, relation) {
                (Basis::Code, _) => {}
                (_, has_note) => {
                    if !has_note {
                        findings.push(finding(
                            FindingCode::BasisWithoutNote,
                            &from,
                            &to,
                            Vec::new(),
                            "a non-code basis needs a note saying what the dependency rests on"
                                .to_string(),
                        ));
                    }
                    continue;
                }
            }
            let from_files = index.subtree_files(&node.id);
            let to_files = index.subtree_files(&relation.target);
            let analyzable = |files: &BTreeSet<String>| {
                files.iter().any(|file| analyzed.contains(file.as_str()))
            };
            if !analyzable(&from_files) || !analyzable(&to_files) {
                findings.push(finding(
                    FindingCode::Unverifiable,
                    &from,
                    &to,
                    Vec::new(),
                    "one side owns no analyzed source file: give the relation a basis \
                     (runtime, data, build or process) and a note"
                        .to_string(),
                ));
                continue;
            }
            if !between(&from_files, &to_files).is_empty() {
                continue;
            }
            let reverse = between(&to_files, &from_files);
            if reverse.is_empty() {
                findings.push(finding(
                    FindingCode::Ghost,
                    &from,
                    &to,
                    Vec::new(),
                    format!(
                        "no code edge backs depends_on {} -> {}: remove it, or give it a \
                         non-code basis and a note",
                        node.id, relation.target
                    ),
                ));
            } else {
                findings.push(finding(
                    FindingCode::Inverted,
                    &from,
                    &to,
                    reverse,
                    format!(
                        "the code depends the other way: declare depends_on {} -> {} instead",
                        relation.target, node.id
                    ),
                ));
            }
        }
    }
}

/// Mark the findings each live waiver matches, and report every waiver
/// that matched nothing.
fn apply_waivers(waivers: &[Waiver], today: &str, findings: &mut Vec<Finding>) {
    let mut unused = Vec::new();
    for waiver in waivers {
        let expired = match &waiver.until {
            None => false,
            Some(until) => !is_date(until) || !is_date(today) || today > until.as_str(),
        };
        let has_selector =
            !waiver.features.is_empty() || waiver.from.is_some() || waiver.to.is_some();
        let mut used = false;
        if !expired && has_selector && !waiver.reason.trim().is_empty() {
            for finding in findings.iter_mut() {
                if finding.code == waiver.code && waiver_matches(waiver, finding) {
                    finding.waived = true;
                    used = true;
                }
            }
        }
        if !used {
            let why = if waiver.reason.trim().is_empty() {
                "it gives no reason".to_string()
            } else if !has_selector {
                "it names no feature".to_string()
            } else if expired {
                format!(
                    "it expired ({} is past {})",
                    today,
                    waiver.until.as_deref().unwrap_or("")
                )
            } else {
                "no finding matches it".to_string()
            };
            let mut named: Vec<String> = waiver.features.clone();
            named.extend(waiver.from.iter().cloned());
            named.extend(waiver.to.iter().cloned());
            named.sort();
            named.dedup();
            unused.push(Finding {
                code: FindingCode::WaiverUnused,
                severity: FindingCode::WaiverUnused.severity(),
                waived: false,
                from_any_of: Vec::new(),
                to_any_of: Vec::new(),
                features: named,
                evidence_total: 0,
                evidence: Vec::new(),
                fix: format!(
                    "the {} waiver applies to nothing: {why}; remove or renew it",
                    waiver.code.as_str()
                ),
                suppressed: None,
            });
        }
    }
    findings.extend(unused);
}

fn waiver_matches(waiver: &Waiver, finding: &Finding) -> bool {
    let named = |id: &String, side: &[String]| side.contains(id) || finding.features.contains(id);
    waiver
        .from
        .as_ref()
        .is_none_or(|from| named(from, &finding.from_any_of))
        && waiver
            .to
            .as_ref()
            .is_none_or(|to| named(to, &finding.to_any_of))
        && waiver.features.iter().all(|id| {
            finding.features.contains(id)
                || finding.from_any_of.contains(id)
                || finding.to_any_of.contains(id)
        })
}

/// Whether `finding` blocks `spawn features check --deps enforce`:
/// unwaived structural/missing/role findings (`under_declared`, `ghost`,
/// `inverted`, `basis_without_note`, `static_cycle`, `kernel_leak`,
/// `entrypoint_imported`) plus expired waivers (`waiver_unused` naming
/// expiration). `implied_only`, `test_only_undeclared`, `unverifiable`, and
/// active/non-expired unused waivers are advisory only.
pub fn is_enforced_truth_finding(finding: &Finding) -> bool {
    if finding.waived {
        return false;
    }
    match finding.code {
        FindingCode::UnderDeclared
        | FindingCode::Ghost
        | FindingCode::Inverted
        | FindingCode::BasisWithoutNote
        | FindingCode::StaticCycle
        | FindingCode::KernelLeak
        | FindingCode::EntrypointImported => true,
        FindingCode::WaiverUnused => finding.fix.contains("it expired"),
        FindingCode::ImpliedOnly | FindingCode::TestOnlyUndeclared | FindingCode::Unverifiable => {
            false
        }
    }
}

/// All findings in `report` that fail `--deps enforce`.
pub fn enforced_truth_violations(report: &TruthReport) -> Vec<&Finding> {
    report
        .findings
        .iter()
        .filter(|finding| is_enforced_truth_finding(finding))
        .collect()
}

/// Semantic purity rule violated by a feature registry node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PurityCode {
    DomainDirectFiles,
    GlobCatchAll,
    CatchAllBucket,
    UnrelatedDirectorySpan,
}

impl PurityCode {
    /// The fixed severity of this purity rule.
    pub const fn severity(self) -> Severity {
        Severity::Error
    }

    /// The JSON spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DomainDirectFiles => "domain_direct_files",
            Self::GlobCatchAll => "glob_catch_all",
            Self::CatchAllBucket => "catch_all_bucket",
            Self::UnrelatedDirectorySpan => "unrelated_directory_span",
        }
    }
}

/// One semantic purity finding against a registry feature node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurityFinding {
    pub code: PurityCode,
    pub severity: Severity,
    pub feature_id: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_paths: Vec<String>,
    pub fix: String,
}

/// Counts of semantic purity findings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurityCounts {
    pub error: usize,
    pub warn: usize,
    pub by_code: BTreeMap<String, usize>,
}

impl PurityCounts {
    /// Summarize `findings`.
    pub fn of(findings: &[PurityFinding]) -> Self {
        let mut counts = Self::default();
        for finding in findings {
            *counts
                .by_code
                .entry(finding.code.as_str().to_string())
                .or_default() += 1;
            match finding.severity {
                Severity::Error => counts.error += 1,
                Severity::Warn | Severity::Info => counts.warn += 1,
            }
        }
        counts
    }
}

/// Semantic purity report for `spawn features check --purity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurityReport {
    pub schema_version: u32,
    pub counts: PurityCounts,
    pub findings: Vec<PurityFinding>,
}

impl PurityReport {
    /// True when zero purity errors were found.
    pub fn is_clean(&self) -> bool {
        self.counts.error == 0
    }
}

const COMPANION_TOP_DIRS: [&str; 12] = [
    "docs", "doc", "tests", "test", "rsi", "deploy", "scripts", "evals", "examples", "benches",
    "fixtures", ".github",
];

const CATCHALL_TOKENS: [&str; 12] = [
    "misc",
    "miscellaneous",
    "other",
    "others",
    "catchall",
    "unclassified",
    "uncategorized",
    "unsorted",
    "leftover",
    "leftovers",
    "everything",
    "grabbag",
];

const CATCHALL_BIGRAMS: [&str; 13] = [
    "catch-all",
    "grab-bag",
    "all-files",
    "root-files",
    "generic-files",
    "all-docs",
    "all-scripts",
    "all-tests",
    "misc-files",
    "other-files",
    "general-bucket",
    "catchall-bucket",
    "misc-bucket",
];

fn purity_finding(
    code: PurityCode,
    feature_id: &str,
    detail: String,
    mut sample_paths: Vec<String>,
    fix: String,
) -> PurityFinding {
    sample_paths.sort();
    sample_paths.dedup();
    sample_paths.truncate(EVIDENCE_LIMIT);
    PurityFinding {
        code,
        severity: code.severity(),
        feature_id: feature_id.to_string(),
        detail,
        sample_paths,
        fix,
    }
}

fn normalized_word_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn catch_all_label_reason(node: &FeatureNode) -> Option<String> {
    for (field, raw) in [("id", node.id.as_str()), ("title", node.title.as_str())] {
        let tokens = normalized_word_tokens(raw);
        for token in &tokens {
            if CATCHALL_TOKENS.contains(&token.as_str()) {
                return Some(format!(
                    "{field} {raw:?} uses catch-all bucket token {token:?}"
                ));
            }
        }
        for pair in tokens.windows(2) {
            let bigram = format!("{}-{}", pair[0], pair[1]);
            if CATCHALL_BIGRAMS.contains(&bigram.as_str()) {
                return Some(format!(
                    "{field} {raw:?} uses catch-all bucket phrase {bigram:?}"
                ));
            }
        }
    }
    None
}

fn count_connected_dir_components<'a>(
    dirs: &BTreeSet<&'a str>,
    node_files: &BTreeSet<&str>,
    graph: &CodeGraph,
) -> usize {
    let mut adj: BTreeMap<&'a str, BTreeSet<&'a str>> =
        dirs.iter().map(|dir| (*dir, BTreeSet::new())).collect();
    for edge in &graph.edges {
        if !node_files.contains(edge.from.as_str()) || !node_files.contains(edge.to.as_str()) {
            continue;
        }
        let Some((from_top, _)) = edge.from.split_once('/') else {
            continue;
        };
        let Some((to_top, _)) = edge.to.split_once('/') else {
            continue;
        };
        if from_top != to_top
            && let (Some(&d_from), Some(&d_to)) = (dirs.get(from_top), dirs.get(to_top))
        {
            adj.entry(d_from).or_default().insert(d_to);
            adj.entry(d_to).or_default().insert(d_from);
        }
    }
    let mut seen: BTreeSet<&'a str> = BTreeSet::new();
    let mut components = 0usize;
    for &start in dirs {
        if !seen.insert(start) {
            continue;
        }
        components += 1;
        let mut queue = VecDeque::from([start]);
        while let Some(cur) = queue.pop_front() {
            if let Some(neighbors) = adj.get(cur) {
                for &next in neighbors {
                    if seen.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
    }
    components
}

fn sample_dir_paths(dirs: &BTreeSet<&str>, files: &[String]) -> Vec<String> {
    dirs.iter()
        .filter_map(|dir| {
            files
                .iter()
                .find(|path| path.starts_with(&format!("{dir}/")))
                .cloned()
        })
        .collect()
}

fn check_directory_span(
    node: &FeatureNode,
    graph: Option<&CodeGraph>,
    findings: &mut Vec<PurityFinding>,
) {
    let all_top_dirs: BTreeSet<&str> = node
        .files
        .iter()
        .filter_map(|path| path.split_once('/').map(|(top, _)| top))
        .collect();
    let primary_dirs: BTreeSet<&str> = all_top_dirs
        .iter()
        .copied()
        .filter(|top| !COMPANION_TOP_DIRS.contains(top))
        .collect();

    if primary_dirs.len() >= 3 {
        let node_files: BTreeSet<&str> = node.files.iter().map(String::as_str).collect();
        let components = graph.map_or(primary_dirs.len(), |g| {
            count_connected_dir_components(&primary_dirs, &node_files, g)
        });
        if components > 1 {
            let dirs_list: Vec<&str> = primary_dirs.iter().copied().collect();
            findings.push(purity_finding(
                PurityCode::UnrelatedDirectorySpan,
                &node.id,
                format!(
                    "capability spans {} primary directories ({}) split into {} disconnected code component(s)",
                    primary_dirs.len(),
                    dirs_list.join(", "),
                    components
                ),
                sample_dir_paths(&primary_dirs, &node.files),
                "split unrelated primary directories into separate cohesive capabilities"
                    .to_string(),
            ));
        }
    } else if primary_dirs.is_empty() && all_top_dirs.len() >= 3 {
        let dirs_list: Vec<&str> = all_top_dirs.iter().copied().collect();
        findings.push(purity_finding(
            PurityCode::UnrelatedDirectorySpan,
            &node.id,
            format!(
                "non-code capability groups {} unrelated top-level directories ({})",
                all_top_dirs.len(),
                dirs_list.join(", ")
            ),
            sample_dir_paths(&all_top_dirs, &node.files),
            "attach docs/scripts/deploy artifacts to their owning capabilities or split by directory"
                .to_string(),
        ));
    }
}

fn check_node_purity(
    node: &FeatureNode,
    graph: Option<&CodeGraph>,
    findings: &mut Vec<PurityFinding>,
) {
    if node.kind == FeatureKind::Domain && !node.files.is_empty() {
        findings.push(purity_finding(
            PurityCode::DomainDirectFiles,
            &node.id,
            format!(
                "domain root owns {} file(s) directly (must organize child capabilities only)",
                node.files.len()
            ),
            node.files.clone(),
            "move files from this domain root into cohesive child capability nodes".to_string(),
        ));
    }

    let glob_paths: Vec<String> = node
        .files
        .iter()
        .filter(|path| path.contains(['*', '?', '[', ']']))
        .cloned()
        .collect();
    if !glob_paths.is_empty() {
        findings.push(purity_finding(
            PurityCode::GlobCatchAll,
            &node.id,
            format!(
                "{} file entry/entries contain wildcard glob characters",
                glob_paths.len()
            ),
            glob_paths,
            "list explicit repository-relative file paths rather than directory globs".to_string(),
        ));
    }

    if node.kind.may_be_root() {
        return;
    }

    if let Some(reason) = catch_all_label_reason(node) {
        findings.push(purity_finding(
            PurityCode::CatchAllBucket,
            &node.id,
            reason,
            node.files.clone(),
            "replace this catch-all bucket with cohesive, domain-named capability nodes"
                .to_string(),
        ));
    }

    check_directory_span(node, graph, findings);
}

/// Check semantic purity of `registry`: domain roots owning files directly,
/// glob patterns inside `files`, catch-all bucket names/titles, and
/// non-root capabilities spanning unrelated top-level directories without
/// unifying code edges. Reuses `glob_matches` parity via explicit path check.
pub fn check_purity(registry: &FeatureRegistry, graph: Option<&CodeGraph>) -> PurityReport {
    let _ = glob_matches;
    let mut findings = Vec::new();
    for node in &registry.features {
        check_node_purity(node, graph, &mut findings);
    }
    findings.sort_by(|a, b| {
        (a.severity, a.code, &a.feature_id).cmp(&(b.severity, b.code, &b.feature_id))
    });
    let counts = PurityCounts::of(&findings);
    PurityReport {
        schema_version: TRUTH_SCHEMA_VERSION,
        counts,
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_coverage::{BoundaryPolicy, FeatureKind, FeatureNode, FeatureProvenance};

    use Scope::{Prod, Test};

    const TODAY: &str = "2026-11-25";

    /// `(id, parent, files, depends_on)`: a string table of features. An id
    /// starting with `dom-` is a domain.
    type Row<'a> = (&'a str, Option<&'a str>, &'a [&'a str], &'a [&'a str]);

    fn registry(rows: &[Row<'_>]) -> FeatureRegistry {
        FeatureRegistry {
            schema_version: crate::feature_coverage::SCHEMA_VERSION,
            initiative_id: "fx".to_string(),
            boundary: BoundaryPolicy::default_generic_boundary("fx"),
            features: rows
                .iter()
                .map(|(id, parent, files, depends)| FeatureNode {
                    id: (*id).to_string(),
                    title: (*id).to_string(),
                    kind: if id.starts_with("dom-") {
                        FeatureKind::Domain
                    } else {
                        FeatureKind::Capability
                    },
                    parent: parent.map(str::to_string),
                    summary: String::new(),
                    status_note: None,
                    files: files.iter().map(|f| (*f).to_string()).collect(),
                    relations: depends
                        .iter()
                        .map(|target| FeatureRelation {
                            kind: RelationKind::DependsOn,
                            target: (*target).to_string(),
                            note: None,
                        })
                        .collect(),
                    provenance: FeatureProvenance::default(),
                })
                .collect(),
        }
    }

    /// `(from, to, line, scope)`: a string table of `use` edges. Every file
    /// named is a parsed Rust source file.
    fn graph(edges: &[(&str, &str, u32, Scope)]) -> CodeGraph {
        graph_of_kind(edges, EdgeKind::Use)
    }

    fn graph_of_kind(edges: &[(&str, &str, u32, Scope)], kind: EdgeKind) -> CodeGraph {
        let mut code = CodeGraph::default();
        for (from, to, line, scope) in edges {
            code.edges.push(FileEdge {
                from: (*from).to_string(),
                to: (*to).to_string(),
                kind,
                scope: *scope,
                line: *line,
                via: None,
                precision: Precision::Item,
            });
            for file in [from, to] {
                code.files.insert((*file).to_string(), source());
            }
        }
        code
    }

    fn source() -> FileInfo {
        FileInfo {
            language: Language::Rust,
            unit: None,
            class: FileClass::Source,
            is_entry: false,
            in_module_tree: true,
            parsed: true,
        }
    }

    fn codes(report: &TruthReport) -> Vec<(&'static str, bool)> {
        report
            .findings
            .iter()
            .map(|f| (f.code.as_str(), f.waived))
            .collect()
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn projects_shared_owner_edges_as_internal() {
        // shared.rs belongs to both cap-a and cap-b: its edge to b.rs is
        // internal to cap-b, not a cap-a -> cap-b dependency.
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs", "shared.rs"], &[]),
            ("cap-b", Some("dom-x"), &["b.rs", "shared.rs"], &[]),
        ]);
        let code = graph(&[
            ("shared.rs", "b.rs", 3, Prod),
            ("a.rs", "shared.rs", 1, Prod),
        ]);
        let projected = project(&code, &reg);
        assert_eq!(projected.summary.internal, 2);
        assert!(projected.edges.is_empty(), "{:?}", projected.edges);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn structural_edges_are_kept_but_are_not_dependencies() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["lib.rs"], &[]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
        ]);
        let code = graph_of_kind(&[("lib.rs", "b.rs", 1, Prod)], EdgeKind::Mod);
        let projected = project(&code, &reg);
        assert_eq!(projected.summary.structural, 1);
        assert!(projected.edges.is_empty());
        assert!(
            truth(&code, &reg, &TruthPolicy::default(), TODAY)
                .findings
                .is_empty()
        );
    }

    #[test]
    fn a_parent_declaration_covers_its_children() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &[], &["cap-b"]),
            ("cap-a-child", Some("cap-a"), &["a.rs"], &[]),
            ("cap-b", Some("dom-x"), &[], &[]),
            ("cap-b-child", Some("cap-b"), &["b.rs"], &[]),
        ]);
        let code = graph(&[("a.rs", "b.rs", 7, Prod)]);
        let projected = project(&code, &reg);
        assert_eq!(projected.edges.len(), 1);
        assert_eq!(projected.edges[0].status, EdgeStatus::Declared);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        // The declaration on the parents is backed by the children's edge.
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn edges_between_a_parent_and_its_child_are_hierarchy() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &[]),
            ("cap-a-child", Some("cap-a"), &["c.rs"], &[]),
        ]);
        let code = graph(&[("c.rs", "a.rs", 2, Prod), ("a.rs", "c.rs", 4, Prod)]);
        assert_eq!(project(&code, &reg).summary.hierarchy, 2);
        assert!(
            truth(&code, &reg, &TruthPolicy::default(), TODAY)
                .findings
                .is_empty()
        );
    }

    #[test]
    fn under_declared_names_both_owner_sets_once() {
        // Both ends are shared: one finding naming {a1, a2} -> {b1, b2},
        // not four pair findings.
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a1", Some("dom-x"), &["a.rs"], &[]),
            ("cap-a2", Some("dom-x"), &["a.rs"], &[]),
            ("cap-b1", Some("dom-x"), &["b.rs"], &[]),
            ("cap-b2", Some("dom-x"), &["b.rs"], &[]),
        ]);
        let code = graph(&[
            ("a.rs", "b.rs", 3, Prod),
            ("a.rs", "b.rs", 9, Prod),
            ("a.rs", "b.rs", 12, Test),
        ]);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(codes(&report), vec![("under_declared", false)]);
        let found = &report.findings[0];
        assert_eq!(found.severity, Severity::Error);
        assert_eq!(found.from_any_of, strings(&["cap-a1", "cap-a2"]));
        assert_eq!(found.to_any_of, strings(&["cap-b1", "cap-b2"]));
        assert_eq!(found.evidence_total, 2, "production edges only");
        assert_eq!(
            found
                .evidence
                .iter()
                .map(|e| (e.from.as_str(), e.line))
                .collect::<Vec<_>>(),
            vec![("a.rs", 3), ("a.rs", 9)]
        );
        assert_eq!(report.counts.error, 1);
        assert_eq!(report.counts.by_code["under_declared"], 1);

        // Declaring any one pair settles it.
        let mut declared = reg.clone();
        declared.features[2].relations.push(FeatureRelation {
            kind: RelationKind::DependsOn,
            target: "cap-b1".to_string(),
            note: None,
        });
        let report = truth(&code, &declared, &TruthPolicy::default(), TODAY);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn transitive_declarations_are_implied_only_and_test_edges_are_info() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &["cap-m"]),
            ("cap-m", Some("dom-x"), &["m.rs"], &["cap-b"]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
            ("cap-t", Some("dom-x"), &["t.rs"], &[]),
        ]);
        let code = graph(&[
            ("a.rs", "m.rs", 1, Prod),
            ("m.rs", "b.rs", 1, Prod),
            ("a.rs", "b.rs", 5, Prod),
            ("t.rs", "a.rs", 2, Test),
        ]);
        let index = FeatureIndex::new(&reg);
        let reach = index.declared_reach(&BTreeSet::from(["cap-a".to_string()]));
        assert_eq!(
            reach,
            BTreeSet::from(["cap-b".to_string(), "cap-m".to_string()])
        );
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(
            codes(&report),
            vec![("implied_only", false), ("test_only_undeclared", false)]
        );
        assert_eq!(report.findings[0].severity, Severity::Warn);
        assert_eq!(report.findings[1].from_any_of, strings(&["cap-t"]));
        assert_eq!(report.findings[1].severity, Severity::Info);
        assert_eq!(project(&code, &reg).summary.implied, 1);
    }

    #[test]
    fn ghost_and_inverted_are_told_apart() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &["cap-b", "cap-c"]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
            ("cap-c", Some("dom-x"), &["c.rs"], &[]),
        ]);
        // No edge a -> b at all; c -> a runs against the declaration.
        let code = graph(&[("c.rs", "a.rs", 4, Prod), ("b.rs", "b2.rs", 1, Prod)]);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        let found: Vec<_> = report
            .findings
            .iter()
            .filter(|f| matches!(f.code, FindingCode::Ghost | FindingCode::Inverted))
            .map(|f| (f.code.as_str(), f.to_any_of.clone(), f.evidence.len()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("ghost", strings(&["cap-b"]), 0),
                ("inverted", strings(&["cap-c"]), 1),
            ]
        );
        let inverted = report
            .findings
            .iter()
            .find(|f| f.code == FindingCode::Inverted)
            .expect("inverted");
        assert_eq!(inverted.evidence[0].from, "c.rs");
        assert!(inverted.fix.contains("cap-c -> cap-a"), "{}", inverted.fix);

        // Evidence in any scope (here a test edge) backs a declaration.
        let backed = graph(&[("a.rs", "b.rs", 2, Test), ("a.rs", "c.rs", 3, Prod)]);
        let report = truth(&backed, &reg, &TruthPolicy::default(), TODAY);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn a_feature_without_code_is_unverifiable_not_ghost() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &["cap-docs", "cap-ops"]),
            ("cap-docs", Some("dom-x"), &["docs/guide.md"], &[]),
            ("cap-ops", Some("dom-x"), &["deploy/run.sh"], &[]),
        ]);
        let code = graph(&[("a.rs", "a2.rs", 1, Prod)]);
        let policy = TruthPolicy {
            bases: vec![RelationBasis {
                from: "cap-a".to_string(),
                to: "cap-ops".to_string(),
                basis: Basis::Runtime,
                note: "invokes the installed binary".to_string(),
            }],
            ..TruthPolicy::default()
        };
        let report = truth(&code, &reg, &policy, TODAY);
        assert_eq!(codes(&report), vec![("unverifiable", false)]);
        assert_eq!(report.findings[0].to_any_of, strings(&["cap-docs"]));
        assert_eq!(report.counts.info, 1);
        assert_eq!(report.counts.error, 0);

        // A non-code basis needs its note.
        let mut bare = policy;
        bare.bases[0].note = "  ".to_string();
        let report = truth(&code, &reg, &bare, TODAY);
        assert_eq!(
            codes(&report),
            vec![("basis_without_note", false), ("unverifiable", false)]
        );
    }

    #[test]
    fn static_cycles_are_reported_once_with_closing_edges() {
        // a -> b -> c -> a, plus a -> c: one component, one finding, closed
        // by the shortest cycle through cap-a (a -> c -> a). Only the mutual
        // pair a <-> c is undeclarable; a -> b and b -> c can be declared.
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &[]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
            ("cap-c", Some("dom-x"), &["c.rs"], &[]),
            ("cap-d", Some("dom-x"), &["d.rs"], &[]),
        ]);
        let code = graph(&[
            ("a.rs", "b.rs", 1, Prod),
            ("b.rs", "c.rs", 2, Prod),
            ("c.rs", "a.rs", 3, Prod),
            ("a.rs", "c.rs", 4, Prod),
            ("c.rs", "a.rs", 8, Prod),
            ("d.rs", "a.rs", 5, Prod),
            // Test edges never make a cycle.
            ("b.rs", "d.rs", 6, Test),
        ]);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        let cycles: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == FindingCode::StaticCycle)
            .collect();
        assert_eq!(cycles.len(), 1, "{:?}", report.findings);
        assert_eq!(cycles[0].features, strings(&["cap-a", "cap-b", "cap-c"]));
        let closing: Vec<_> = cycles[0]
            .evidence
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.line))
            .collect();
        assert_eq!(closing, vec![("a.rs", "c.rs", 4), ("c.rs", "a.rs", 3)]);
        assert_eq!(cycles[0].evidence_total, 2);
        let suppressed = cycles[0].suppressed.as_ref().expect("suppressed");
        assert_eq!((suppressed.total, suppressed.file_edges), (2, 3));
        assert_eq!(
            suppressed
                .pairs
                .iter()
                .map(|p| (p.from_any_of[0].as_str(), p.to_any_of[0].as_str(), p.reason))
                .collect::<Vec<_>>(),
            vec![
                ("cap-a", "cap-c", SuppressionReason::Mutual),
                ("cap-c", "cap-a", SuppressionReason::Mutual),
            ]
        );
        // The non-mutual pairs inside the component stay under_declared.
        let under: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == FindingCode::UnderDeclared)
            .map(|f| (f.from_any_of[0].as_str(), f.to_any_of[0].as_str()))
            .collect();
        assert_eq!(
            under,
            vec![("cap-a", "cap-b"), ("cap-b", "cap-c"), ("cap-d", "cap-a")]
        );
        assert_eq!(report.counts.by_code["test_only_undeclared"], 1);
    }

    #[test]
    fn a_non_mutual_edge_inside_a_cycle_component_is_still_under_declared() {
        // a -> b -> c -> a: one component, but no pair is mutual and nothing
        // is declared, so each edge can still be declared on its own.
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &[]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
            ("cap-c", Some("dom-x"), &["c.rs"], &[]),
        ]);
        let code = graph(&[
            ("a.rs", "b.rs", 1, Prod),
            ("b.rs", "c.rs", 2, Prod),
            ("c.rs", "a.rs", 3, Prod),
        ]);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(report.counts.by_code["under_declared"], 3);
        let cycle = report
            .findings
            .iter()
            .find(|f| f.code == FindingCode::StaticCycle)
            .expect("cycle");
        assert_eq!(cycle.features, strings(&["cap-a", "cap-b", "cap-c"]));
        assert_eq!(cycle.suppressed, Some(Suppressed::default()));
    }

    #[test]
    fn mutual_and_cycle_closing_pairs_are_suppressed_and_listed() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &[]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
            // c declares d with no code behind it; d's code uses c.
            ("cap-c", Some("dom-x"), &["c.rs"], &["cap-d"]),
            ("cap-d", Some("dom-x"), &["d.rs"], &[]),
        ]);
        let code = graph(&[
            ("a.rs", "b.rs", 1, Prod),
            ("a.rs", "b.rs", 2, Prod),
            ("b.rs", "a.rs", 3, Prod),
            ("d.rs", "c.rs", 4, Prod),
        ]);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(report.counts.by_code.get("under_declared"), None);
        let cycles: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == FindingCode::StaticCycle)
            .map(|f| {
                let suppressed = f.suppressed.as_ref().expect("suppressed");
                (
                    f.features.clone(),
                    suppressed.total,
                    suppressed.file_edges,
                    suppressed
                        .pairs
                        .iter()
                        .map(|p| (p.from_any_of.join("|"), p.to_any_of.join("|"), p.reason))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            cycles,
            vec![
                (
                    strings(&["cap-a", "cap-b"]),
                    2,
                    3,
                    vec![
                        ("cap-a".into(), "cap-b".into(), SuppressionReason::Mutual),
                        ("cap-b".into(), "cap-a".into(), SuppressionReason::Mutual),
                    ],
                ),
                (
                    strings(&["cap-c", "cap-d"]),
                    1,
                    1,
                    vec![(
                        "cap-d".into(),
                        "cap-c".into(),
                        SuppressionReason::ClosesDeclaredCycle,
                    )],
                ),
            ]
        );
        // The declaration itself has no code behind it: inverted.
        assert_eq!(report.counts.by_code["inverted"], 1);
    }

    #[test]
    fn shared_file_co_owners_alone_make_no_cycle() {
        // x.rs is shared by a and b; it uses c, and c uses a. Before, every
        // co-owner pair was a link (a -> c, b -> c, c -> a): a false cycle.
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs", "x.rs"], &[]),
            ("cap-b", Some("dom-x"), &["b.rs", "x.rs"], &[]),
            ("cap-c", Some("dom-x"), &["c.rs"], &[]),
        ]);
        let code = graph(&[("x.rs", "c.rs", 1, Prod), ("c.rs", "a.rs", 2, Prod)]);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(
            report.counts.by_code.get("static_cycle"),
            None,
            "{:?}",
            report.findings
        );
        let under: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.code == FindingCode::UnderDeclared)
            .map(|f| (f.from_any_of.clone(), f.to_any_of.clone()))
            .collect();
        assert_eq!(
            under,
            vec![
                (strings(&["cap-a", "cap-b"]), strings(&["cap-c"])),
                (strings(&["cap-c"]), strings(&["cap-a"])),
            ]
        );
    }

    #[test]
    fn counts_follow_the_findings_they_are_given() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &[]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
            ("cap-t", Some("dom-x"), &["t.rs"], &[]),
        ]);
        let code = graph(&[("a.rs", "b.rs", 1, Prod), ("t.rs", "a.rs", 2, Test)]);
        let report = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(TruthCounts::of(&report.findings), report.counts);
        let only_test = TruthCounts::of(&report.findings[1..]);
        assert_eq!((only_test.error, only_test.info), (0, 1));
        assert_eq!(only_test.by_code.len(), 1);
    }

    #[test]
    fn kernel_and_entrypoint_roles_exempt_and_oblige() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-main", Some("dom-x"), &["main.rs"], &[]),
            ("cap-core", Some("dom-x"), &["core.rs"], &[]),
            ("cap-feat", Some("dom-x"), &["feat.rs"], &[]),
            ("cap-io", Some("dom-x"), &["io.rs"], &[]),
        ]);
        let policy = TruthPolicy {
            roles: BTreeMap::from([
                ("cap-main".to_string(), Role::Entrypoint),
                ("cap-core".to_string(), Role::Kernel),
            ]),
            ..TruthPolicy::default()
        };
        // Exempt: the entrypoint uses anything, anyone uses the kernel.
        let exempt = graph(&[
            ("main.rs", "feat.rs", 1, Prod),
            ("main.rs", "core.rs", 2, Prod),
            ("feat.rs", "core.rs", 3, Prod),
        ]);
        let report = truth(&exempt, &reg, &policy, TODAY);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        // Without the roles the same graph is three missing declarations.
        let plain = truth(&exempt, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(plain.counts.by_code["under_declared"], 3);

        // Obliged: the kernel depends on a non-kernel, a feature imports
        // the entrypoint. Declaring the edge does not lift the obligation.
        let mut declared = reg.clone();
        declared.features[2].relations.push(FeatureRelation {
            kind: RelationKind::DependsOn,
            target: "cap-io".to_string(),
            note: None,
        });
        let obliged = graph(&[
            ("core.rs", "io.rs", 4, Prod),
            ("feat.rs", "main.rs", 5, Prod),
        ]);
        let report = truth(&obliged, &declared, &policy, TODAY);
        assert_eq!(
            codes(&report),
            vec![
                ("under_declared", false),
                ("kernel_leak", false),
                ("entrypoint_imported", false),
            ]
        );
        assert_eq!(report.findings[0].to_any_of, strings(&["cap-main"]));
        assert_eq!(report.findings[1].from_any_of, strings(&["cap-core"]));
        assert_eq!(report.counts.error, 2);
        assert_eq!(report.counts.warn, 1);
    }

    #[test]
    fn an_expired_waiver_stops_applying() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &[]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
        ]);
        let code = graph(&[("a.rs", "b.rs", 1, Prod)]);
        let waiver = |until: Option<&str>| Waiver {
            code: FindingCode::UnderDeclared,
            features: Vec::new(),
            from: Some("cap-a".to_string()),
            to: Some("cap-b".to_string()),
            reason: "split planned".to_string(),
            until: until.map(str::to_string),
        };
        let policy = |until: Option<&str>| TruthPolicy {
            waivers: vec![waiver(until)],
            ..TruthPolicy::default()
        };

        let live = truth(&code, &reg, &policy(Some("2026-12-31")), TODAY);
        assert_eq!(codes(&live), vec![("under_declared", true)]);
        assert_eq!((live.counts.error, live.counts.waived), (0, 1));
        // The last day still applies.
        let last_day = truth(&code, &reg, &policy(Some(TODAY)), TODAY);
        assert_eq!(codes(&last_day), vec![("under_declared", true)]);
        // No end date: applies.
        assert_eq!(
            codes(&truth(&code, &reg, &policy(None), TODAY)),
            vec![("under_declared", true)]
        );

        let expired = truth(&code, &reg, &policy(Some("2026-11-24")), TODAY);
        assert_eq!(
            codes(&expired),
            vec![("under_declared", false), ("waiver_unused", false)]
        );
        assert_eq!(expired.counts.error, 1);
        assert!(
            expired.findings[1].fix.contains("expired"),
            "{:?}",
            expired.findings[1]
        );

        // A waiver with no reason, or one naming nothing, waives nothing.
        let mut no_reason = policy(None);
        no_reason.waivers[0].reason = String::new();
        let report = truth(&code, &reg, &no_reason, TODAY);
        assert_eq!(
            codes(&report),
            vec![("under_declared", false), ("waiver_unused", false)]
        );
        // A cycle waiver names its features.
        let cyclic = graph(&[("a.rs", "b.rs", 1, Prod), ("b.rs", "a.rs", 2, Prod)]);
        let cycle_waiver = TruthPolicy {
            waivers: vec![Waiver {
                code: FindingCode::StaticCycle,
                features: strings(&["cap-a", "cap-b"]),
                from: None,
                to: None,
                reason: "shared rows, split planned".to_string(),
                until: Some("2026-12-31".to_string()),
            }],
            ..TruthPolicy::default()
        };
        let report = truth(&cyclic, &reg, &cycle_waiver, TODAY);
        assert_eq!(codes(&report), vec![("static_cycle", true)]);
    }

    #[test]
    fn findings_are_sorted_capped_and_deterministic() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            ("cap-a", Some("dom-x"), &["a.rs"], &["cap-z"]),
            ("cap-b", Some("dom-x"), &["b.rs"], &[]),
            ("cap-z", Some("dom-x"), &["z.rs"], &[]),
        ]);
        let mut rows: Vec<(&str, &str, u32, Scope)> =
            (1..=9).map(|n| ("a.rs", "b.rs", n, Prod)).collect();
        rows.push(("z.rs", "z2.rs", 1, Prod));
        let code = graph(&rows);
        let first = truth(&code, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(
            codes(&first),
            vec![("under_declared", false), ("ghost", false)]
        );
        assert_eq!(first.findings[0].evidence_total, 9);
        assert_eq!(first.findings[0].evidence.len(), EVIDENCE_LIMIT);
        let mut reversed = code.clone();
        reversed.edges.reverse();
        let second = truth(&reversed, &reg, &TruthPolicy::default(), TODAY);
        assert_eq!(
            serde_json::to_string(&first).expect("json"),
            serde_json::to_string(&second).expect("json")
        );
    }

    #[test]
    fn severities_and_spellings_are_fixed() {
        assert_eq!(FindingCode::UnderDeclared.severity(), Severity::Error);
        assert_eq!(FindingCode::Ghost.severity(), Severity::Error);
        assert_eq!(FindingCode::Inverted.severity(), Severity::Error);
        assert_eq!(FindingCode::StaticCycle.severity(), Severity::Warn);
        assert_eq!(FindingCode::Unverifiable.severity(), Severity::Info);
        assert_eq!(
            serde_json::to_value(FindingCode::TestOnlyUndeclared).expect("json"),
            serde_json::json!("test_only_undeclared")
        );
        assert_eq!(
            serde_json::to_value(EdgeKind::DynamicImport).expect("json"),
            serde_json::json!(EdgeKind::DynamicImport.as_str())
        );
        assert!(EdgeKind::Mod.is_structural() && EdgeKind::Reexport.is_structural());
        assert!(!EdgeKind::Use.is_structural());
        assert!(is_date("2026-01-31") && !is_date("2026-1-31") && !is_date("2026/01/31"));
    }

    #[test]
    fn enforced_truth_violations_filter_advisory_and_catch_expired_waivers() {
        let reg = registry(&[
            ("dom-x", None, &[], &[]),
            (
                "cap-a",
                Some("dom-x"),
                &["a.rs", "a_test.rs"],
                &["cap-ghost"],
            ),
            ("cap-b", Some("dom-x"), &["b.rs"], &["cap-c"]),
            ("cap-c", Some("dom-x"), &["c.rs"], &["cap-d"]),
            ("cap-d", Some("dom-x"), &["d.rs"], &["cap-e"]),
            ("cap-ghost", Some("dom-x"), &["g.rs"], &[]),
            ("cap-k", Some("dom-x"), &["k.rs"], &["cap-d"]),
            ("cap-e", Some("dom-x"), &["e.rs"], &[]),
            ("cap-u", Some("dom-x"), &["u.rs"], &[]),
        ]);
        let mut code = graph(&[
            ("a_test.rs", "d.rs", 1, Test), // test_only_undeclared -> advisory
            ("b.rs", "d.rs", 2, Prod),      // implied_only -> advisory
            ("b.rs", "c.rs", 5, Prod),
            ("c.rs", "d.rs", 6, Prod),
            ("k.rs", "d.rs", 3, Prod), // kernel_leak -> enforced
            ("d.rs", "e.rs", 4, Prod), // entrypoint_imported -> enforced
            ("u.rs", "d.rs", 7, Prod), // waived under_declared -> ignored
        ]);
        code.files.insert("a.rs".to_string(), source());
        code.files.insert("g.rs".to_string(), source());
        let mut policy = TruthPolicy {
            waivers: vec![
                Waiver {
                    code: FindingCode::UnderDeclared,
                    features: Vec::new(),
                    from: Some("cap-u".to_string()),
                    to: Some("cap-d".to_string()),
                    reason: "active waiver".to_string(),
                    until: Some("2099-12-31".to_string()),
                },
                Waiver {
                    code: FindingCode::UnderDeclared,
                    features: Vec::new(),
                    from: Some("cap-a".to_string()),
                    to: Some("cap-d".to_string()),
                    reason: "expired waiver".to_string(),
                    until: Some("2020-01-01".to_string()),
                },
            ],
            ..TruthPolicy::default()
        };
        policy.roles.insert("cap-k".to_string(), Role::Kernel);
        policy.roles.insert("cap-e".to_string(), Role::Entrypoint);

        let report = truth(&code, &reg, &policy, TODAY);
        let enforced_codes: Vec<&str> = enforced_truth_violations(&report)
            .into_iter()
            .map(|f| f.code.as_str())
            .collect();
        assert_eq!(
            enforced_codes,
            vec![
                "ghost",
                "kernel_leak",
                "entrypoint_imported",
                "waiver_unused"
            ],
            "{enforced_codes:?}"
        );
    }

    #[test]
    fn purity_checks_catch_buckets_globs_domain_files_and_disconnected_spans() {
        let mut reg = registry(&[
            ("dom-root", None, &["README.md"], &[]),
            ("cap-misc", Some("dom-root"), &["docs/*.md"], &[]),
            (
                "cap-grab",
                Some("dom-root"),
                &["alpha/a.rs", "beta/b.rs", "gamma/c.rs"],
                &[],
            ),
            (
                "cap-noncode",
                Some("dom-root"),
                &["docs/a.md", "scripts/run.sh", "deploy/unit.service"],
                &[],
            ),
            (
                "cap-unified",
                Some("dom-root"),
                &["ui/u.ts", "api/a.rs", "worker/w.py", "docs/spec.md"],
                &[],
            ),
        ]);
        reg.features[0].title = "Root".to_string();
        let code = graph(&[
            ("ui/u.ts", "api/a.rs", 1, Prod),
            ("api/a.rs", "worker/w.py", 2, Prod),
        ]);

        let report = check_purity(&reg, Some(&code));
        assert!(!report.is_clean());
        let summary: Vec<(&str, PurityCode)> = report
            .findings
            .iter()
            .map(|f| (f.feature_id.as_str(), f.code))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("dom-root", PurityCode::DomainDirectFiles),
                ("cap-misc", PurityCode::GlobCatchAll),
                ("cap-misc", PurityCode::CatchAllBucket),
                ("cap-grab", PurityCode::UnrelatedDirectorySpan),
                ("cap-noncode", PurityCode::UnrelatedDirectorySpan),
            ],
            "{summary:?}"
        );

        // Removing the noisy nodes leaves only the code-connected full-stack capability cleanly passing.
        reg.features
            .retain(|f| f.id == "cap-unified" || f.id == "dom-root");
        reg.features[0].files.clear();
        let clean = check_purity(&reg, Some(&code));
        assert!(clean.is_clean(), "{:?}", clean.findings);
    }
}

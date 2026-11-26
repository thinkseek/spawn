//! D6 Phase 4 Step 4 (`#113` Comment #5 Defense 4 & `#95`):
//! Pure-Rust Layer 0 and workspace directory materializer over [`CasMerkleStore`]
//! with optional incremental hard-linking from a non-CAS nearest layer and
//! `.gitattributes` `export-ignore` / `export-subst` / `eol` pre-cutover auditing.
//!
//! Safety invariants:
//! - Depends strictly on `spawn-host` + `spawn-cas` (zero database dependencies, zero forbidden OS code).
//! - NEVER creates hard links against `0444` objects inside `<cas>/objects`; regular
//!   and executable files are materialized with explicit `0644` / `0755` permissions
//!   and hard-link reuse is permitted ONLY from an explicitly validated non-CAS
//!   `nearest_dir` whose matching inode differs from any CAS object.
//! - Stages into a sibling temporary directory and publishes atomically with `rename`.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use spawn_cas::{Digest, TreeEntry, TreeEntryKind, is_valid_entry_name, sha256_reader};

use crate::cas_merkle::{CasMerkleStore, D6StoreError};
use crate::root::canonical_root;
use crate::tree_path::TreePath;

static LAYER0_STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// How [`materialize_layer0`] handles a pre-existing destination path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExistingDestPolicy {
    /// Fail if `dest_dir` already exists on disk (default fail-closed mode).
    #[default]
    Fail,
    /// Return immediately with `reused_existing_dest = true` if `dest_dir` already exists
    /// as a real directory (idempotent shared read-only Layer 0 baseline).
    ReuseIfPresent,
    /// Materialize in a fresh staging directory and atomically swap out any existing directory.
    Replace,
}

/// Configuration for [`materialize_layer0`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Layer0Options<'a> {
    /// Optional `(nearest_tree, nearest_dir)` pair used to hard-link unchanged `0644`/`0755`
    /// files from a nearby non-CAS baseline directory instead of rewriting them from CAS.
    pub nearest: Option<(&'a Digest, &'a Path)>,
    /// Policy when `dest_dir` already exists.
    pub if_exists: ExistingDestPolicy,
}

/// Summary returned by [`materialize_layer0`] and [`materialize_workspace_from_tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer0MaterializeReport {
    /// Root `spawn-tree-v1` digest that was materialized.
    pub tree: Digest,
    /// Canonical path to the published target directory.
    pub dest_dir: PathBuf,
    /// Files streamed fresh from CAS objects.
    pub files_written: usize,
    /// Unchanged files hard-linked from `nearest_dir` (never from `<cas>/objects`).
    pub hardlinked_from_nearest: usize,
    /// Symbolic links created.
    pub symlinks_written: usize,
    /// Subdirectories created (`040000`).
    pub dirs_created: usize,
    /// Gitlink submodule mount points (`160000`) created as empty `0755` directories.
    pub gitlinks_materialized: usize,
    /// True when an existing immutable destination directory was reused as-is.
    pub reused_existing_dest: bool,
}

impl Layer0MaterializeReport {
    fn empty(tree: Digest, dest_dir: PathBuf, reused_existing_dest: bool) -> Self {
        Self {
            tree,
            dest_dir,
            files_written: 0,
            hardlinked_from_nearest: 0,
            symlinks_written: 0,
            dirs_created: 0,
            gitlinks_materialized: 0,
            reused_existing_dest,
        }
    }
}

/// Effective EOL/text line-ending rule discovered on a tracked path by [`audit_export_attributes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EolPathFinding {
    /// Repository-relative tracked path.
    pub path: TreePath,
    /// Resolved `eol` attribute (`"lf"`, `"crlf"`, `"-eol"`, or `"unspecified"`).
    pub eol: String,
    /// Resolved `text` attribute if explicitly configured (`Some("text")`, `Some("-text")`, `Some("auto")`).
    pub text: Option<String>,
}

/// Source `.gitattributes` rule touching `export-ignore`, `export-subst`, `eol`, `text`, or `crlf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportAttrRuleFinding {
    /// Path to the `.gitattributes` file defining this rule.
    pub source_file: TreePath,
    /// 1-based line number inside `source_file`.
    pub line_number: usize,
    /// Pattern token on that line.
    pub pattern: String,
    /// Relevant attribute assignments parsed from that rule.
    pub attributes: Vec<String>,
}

/// Result of auditing a project tree's `.gitattributes` under D6 Safeguard #4 before cutover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportAttributesAudit {
    /// Tracked `.gitattributes` files inspected inside the tree.
    pub gitattributes_files: Vec<TreePath>,
    /// Tracked paths matching `export-ignore` (excluded by `git archive`, present in D6 trees).
    pub export_ignore_paths: Vec<TreePath>,
    /// Tracked paths matching `export-subst` (`$Format:...$` disabled in pure-Rust CAS materialization).
    pub export_subst_paths: Vec<TreePath>,
    /// Tracked paths carrying explicit `eol`, `text`, or `crlf` normalization attributes.
    pub eol_paths: Vec<EolPathFinding>,
    /// Raw `.gitattributes` rule occurrences contributing to export/EOL findings.
    pub rules: Vec<ExportAttrRuleFinding>,
}

impl ExportAttributesAudit {
    /// Whether any `.gitattributes` rule or matched path carries export or EOL attributes.
    pub fn has_findings(&self) -> bool {
        !self.export_ignore_paths.is_empty()
            || !self.export_subst_paths.is_empty()
            || !self.eol_paths.is_empty()
            || !self.rules.is_empty()
    }

    /// Whether `git archive` / border exports would omit or substitute bytes relative to D6 trees.
    pub fn has_export_divergence_risk(&self) -> bool {
        !self.export_ignore_paths.is_empty() || !self.export_subst_paths.is_empty()
    }

    /// Whether explicit `eol` or `text` line-ending attributes exist in the audited tree.
    pub fn has_eol_rules(&self) -> bool {
        !self.eol_paths.is_empty()
    }
}

struct StageGuard {
    path: Option<PathBuf>,
}

impl StageGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Materialize `tree` cleanly into a brand-new `dest_dir` with no nearest-layer hard-linking.
pub fn materialize_workspace_from_tree(
    store: &CasMerkleStore,
    tree: &Digest,
    dest_dir: &Path,
) -> Result<Layer0MaterializeReport, D6StoreError> {
    materialize_layer0(store, tree, dest_dir, &Layer0Options::default())
}

/// Materialize a canonical `spawn-tree-v1` `tree` into `dest_dir` using atomic staging + `rename`.
///
/// When `options.nearest` supplies `(nearest_tree, nearest_dir)`, unchanged `0644`/`0755` files
/// matching both metadata and SHA-256 content are hard-linked from `nearest_dir` cleanly.
/// Hard-linking against anything inside `store.cas().root()` or any `0444` file is forbidden.
pub fn materialize_layer0(
    store: &CasMerkleStore,
    tree: &Digest,
    dest_dir: &Path,
    options: &Layer0Options<'_>,
) -> Result<Layer0MaterializeReport, D6StoreError> {
    let dest_name = dest_dir
        .file_name()
        .ok_or_else(|| D6StoreError::InvalidInput("dest_dir has no final component".to_string()))?;
    let raw_parent = match dest_dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        Some(_) => Path::new("."),
        None => {
            return Err(D6StoreError::InvalidInput(
                "dest_dir must have a parent directory".to_string(),
            ));
        }
    };

    if let Ok(meta) = fs::symlink_metadata(dest_dir)
        && meta.file_type().is_symlink()
    {
        return Err(D6StoreError::InvalidInput(
            "layer0 destination cannot be a symlink".to_string(),
        ));
    }
    fs::create_dir_all(raw_parent)?;
    let parent_canon = canonical_root(raw_parent);
    let dest_canon = parent_canon.join(dest_name);
    let cas_canon = canonical_root(store.cas().root());
    if dest_canon == cas_canon || dest_canon.starts_with(&cas_canon) {
        return Err(D6StoreError::InvalidInput(
            "layer0 destination directory must not reside inside CAS root".to_string(),
        ));
    }

    if let Some(early) = check_existing_dest(&dest_canon, tree, options.if_exists)? {
        return Ok(early);
    }

    let nearest_state = prepare_nearest(store, options.nearest, &cas_canon)?;

    let seq = LAYER0_STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let stage_dir = parent_canon.join(format!(".d6-layer0-stage-{}-{seq}", std::process::id()));
    fs::create_dir(&stage_dir)?;
    fs::set_permissions(&stage_dir, fs::Permissions::from_mode(0o755))?;
    let mut guard = StageGuard::new(stage_dir.clone());

    let mut report = Layer0MaterializeReport::empty(tree.clone(), dest_canon.clone(), false);

    materialize_subtree(
        store,
        tree,
        &stage_dir,
        b"",
        nearest_state.as_ref().map(|(dir, _)| dir.as_path()),
        nearest_state.as_ref().map(|(_, map)| map),
        &mut report,
    )?;

    publish_staged_dir(
        &stage_dir,
        &dest_canon,
        &parent_canon,
        options.if_exists,
        &mut guard,
        &mut report,
    )?;
    Ok(report)
}

fn check_existing_dest(
    dest_canon: &Path,
    tree: &Digest,
    policy: ExistingDestPolicy,
) -> Result<Option<Layer0MaterializeReport>, D6StoreError> {
    match fs::symlink_metadata(dest_canon) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(D6StoreError::InvalidInput(
                    "layer0 destination cannot be a symlink".to_string(),
                ));
            }
            match policy {
                ExistingDestPolicy::Fail => Err(D6StoreError::InvalidInput(format!(
                    "layer0 destination {} already exists",
                    dest_canon.display()
                ))),
                ExistingDestPolicy::ReuseIfPresent => {
                    if !meta.is_dir() {
                        return Err(D6StoreError::InvalidInput(format!(
                            "layer0 destination {} exists and is not a directory",
                            dest_canon.display()
                        )));
                    }
                    Ok(Some(Layer0MaterializeReport::empty(
                        tree.clone(),
                        dest_canon.to_path_buf(),
                        true,
                    )))
                }
                ExistingDestPolicy::Replace => {
                    if !meta.is_dir() {
                        return Err(D6StoreError::InvalidInput(format!(
                            "layer0 destination {} exists and is not a directory",
                            dest_canon.display()
                        )));
                    }
                    Ok(None)
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

type NearestState = (PathBuf, BTreeMap<Vec<u8>, TreeEntry>);

fn prepare_nearest(
    store: &CasMerkleStore,
    nearest: Option<(&Digest, &Path)>,
    cas_canon: &Path,
) -> Result<Option<NearestState>, D6StoreError> {
    let Some((nearest_tree, nearest_dir)) = nearest else {
        return Ok(None);
    };
    let meta = fs::symlink_metadata(nearest_dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(D6StoreError::InvalidInput(
            "nearest_dir must be a real non-symlink directory".to_string(),
        ));
    }
    let nearest_canon = canonical_root(nearest_dir);
    if nearest_canon == *cas_canon || nearest_canon.starts_with(cas_canon) {
        return Err(D6StoreError::InvalidInput(
            "nearest_dir must not reside inside CAS root".to_string(),
        ));
    }
    let map = store.flatten_tree(nearest_tree)?;
    Ok(Some((nearest_canon, map)))
}

fn publish_staged_dir(
    stage_dir: &Path,
    dest_canon: &Path,
    parent_canon: &Path,
    policy: ExistingDestPolicy,
    guard: &mut StageGuard,
    report: &mut Layer0MaterializeReport,
) -> Result<(), D6StoreError> {
    match policy {
        ExistingDestPolicy::Fail => {
            fs::rename(stage_dir, dest_canon)?;
            guard.disarm();
        }
        ExistingDestPolicy::ReuseIfPresent => match fs::rename(stage_dir, dest_canon) {
            Ok(()) => guard.disarm(),
            Err(err) => {
                if let Ok(meta) = fs::symlink_metadata(dest_canon)
                    && !meta.file_type().is_symlink()
                    && meta.is_dir()
                {
                    *report = Layer0MaterializeReport::empty(
                        report.tree.clone(),
                        dest_canon.to_path_buf(),
                        true,
                    );
                    return Ok(());
                }
                return Err(err.into());
            }
        },
        ExistingDestPolicy::Replace => {
            let seq = LAYER0_STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
            let backup = parent_canon.join(format!(".d6-layer0-old-{}-{seq}", std::process::id()));
            let had_existing = match fs::symlink_metadata(dest_canon) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        return Err(D6StoreError::InvalidInput(
                            "existing layer0 destination must be a non-symlink directory"
                                .to_string(),
                        ));
                    }
                    fs::rename(dest_canon, &backup)?;
                    true
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => false,
                Err(err) => return Err(err.into()),
            };
            if let Err(err) = fs::rename(stage_dir, dest_canon) {
                if had_existing {
                    let _ = fs::rename(&backup, dest_canon);
                }
                return Err(err.into());
            }
            guard.disarm();
            if had_existing {
                let _ = fs::remove_dir_all(&backup);
            }
        }
    }
    sync_dir_nofollow(parent_canon)?;
    Ok(())
}

fn materialize_subtree(
    store: &CasMerkleStore,
    tree_digest: &Digest,
    stage_root: &Path,
    prefix: &[u8],
    nearest_dir: Option<&Path>,
    nearest_map: Option<&BTreeMap<Vec<u8>, TreeEntry>>,
    report: &mut Layer0MaterializeReport,
) -> Result<(), D6StoreError> {
    let tree = store.read_tree_cached(tree_digest)?;
    for entry in tree.entries {
        if !is_valid_entry_name(&entry.name) {
            return Err(D6StoreError::InvalidInput(
                "invalid tree entry name during layer0 materialization".to_string(),
            ));
        }
        let rel_bytes = join_rel_bytes(prefix, &entry.name);
        let rel_path = Path::new(OsStr::from_bytes(&rel_bytes));
        let target_path = stage_root.join(rel_path);

        match entry.kind {
            TreeEntryKind::Dir => {
                fs::create_dir(&target_path)?;
                fs::set_permissions(&target_path, fs::Permissions::from_mode(0o755))?;
                report.dirs_created += 1;
                let sub = entry.digest().ok_or_else(|| {
                    D6StoreError::InvalidInput("dir entry missing digest".to_string())
                })?;
                materialize_subtree(
                    store,
                    &sub,
                    stage_root,
                    &rel_bytes,
                    nearest_dir,
                    nearest_map,
                    report,
                )?;
            }
            TreeEntryKind::File | TreeEntryKind::Executable => {
                let want_mode = if entry.kind == TreeEntryKind::Executable {
                    0o755
                } else {
                    0o644
                };
                let blob_digest = entry.digest().ok_or_else(|| {
                    D6StoreError::InvalidInput("file entry missing digest".to_string())
                })?;
                if try_hardlink_from_nearest(
                    store,
                    nearest_dir,
                    nearest_map,
                    &rel_bytes,
                    rel_path,
                    &target_path,
                    &entry,
                    &blob_digest,
                    want_mode,
                )? {
                    report.hardlinked_from_nearest += 1;
                } else {
                    write_fresh_cas_file(store, &target_path, &blob_digest, entry.size, want_mode)?;
                    report.files_written += 1;
                }
            }
            TreeEntryKind::Symlink => {
                let blob_digest = entry.digest().ok_or_else(|| {
                    D6StoreError::InvalidInput("symlink entry missing digest".to_string())
                })?;
                let target_bytes = store.cas().read_blob(&blob_digest)?;
                if u64::try_from(target_bytes.len()).ok() != Some(entry.size) {
                    return Err(D6StoreError::InvalidInput(
                        "symlink target length mismatches entry size".to_string(),
                    ));
                }
                symlink(OsStr::from_bytes(&target_bytes), &target_path)?;
                report.symlinks_written += 1;
            }
            TreeEntryKind::Gitlink => {
                fs::create_dir(&target_path)?;
                fs::set_permissions(&target_path, fs::Permissions::from_mode(0o755))?;
                report.gitlinks_materialized += 1;
            }
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
fn try_hardlink_from_nearest(
    store: &CasMerkleStore,
    nearest_dir: Option<&Path>,
    nearest_map: Option<&BTreeMap<Vec<u8>, TreeEntry>>,
    rel_bytes: &[u8],
    rel_path: &Path,
    target_path: &Path,
    entry: &TreeEntry,
    blob_digest: &Digest,
    want_mode: u32,
) -> Result<bool, D6StoreError> {
    let (Some(nearest_root), Some(prev_map)) = (nearest_dir, nearest_map) else {
        return Ok(false);
    };
    if prev_map.get(rel_bytes) != Some(entry) {
        return Ok(false);
    }
    let nearest_file = nearest_root.join(rel_path);
    let Some(nearest_parent) = nearest_file.parent() else {
        return Ok(false);
    };
    if !canonical_root(nearest_parent).starts_with(nearest_root) {
        return Ok(false);
    }
    let Ok(meta) = fs::symlink_metadata(&nearest_file) else {
        return Ok(false);
    };
    if !meta.file_type().is_file()
        || meta.len() != entry.size
        || meta.permissions().mode() & 0o777 != want_mode
    {
        return Ok(false);
    }
    if let Ok(cas_meta) = fs::symlink_metadata(store.cas().object_path(blob_digest))
        && cas_meta.dev() == meta.dev()
        && cas_meta.ino() == meta.ino()
    {
        return Ok(false);
    }
    let Ok(mut handle) = open_regular_file_nofollow(&nearest_file) else {
        return Ok(false);
    };
    if sha256_reader(&mut handle)? != *blob_digest {
        return Ok(false);
    }
    drop(handle);

    match fs::hard_link(&nearest_file, target_path) {
        Ok(()) => Ok(true),
        Err(err)
            if matches!(
                err.raw_os_error(),
                Some(code) if code == libc::EXDEV || code == libc::EMLINK
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(err.into()),
    }
}

fn write_fresh_cas_file(
    store: &CasMerkleStore,
    target_path: &Path,
    blob_digest: &Digest,
    expected_size: u64,
    mode: u32,
) -> Result<(), D6StoreError> {
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(target_path)?;
    match store.cas().stream_blob(blob_digest, &mut out) {
        Ok(written) if written == expected_size => {
            out.set_permissions(fs::Permissions::from_mode(mode))?;
            Ok(())
        }
        Ok(written) => {
            drop(out);
            let _ = fs::remove_file(target_path);
            Err(D6StoreError::InvalidInput(format!(
                "blob {blob_digest} wrote {written} bytes, expected {expected_size}"
            )))
        }
        Err(err) => {
            drop(out);
            let _ = fs::remove_file(target_path);
            Err(err.into())
        }
    }
}

fn open_regular_file_nofollow(path: &Path) -> Result<fs::File, D6StoreError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(D6StoreError::InvalidInput(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

fn sync_dir_nofollow(dir: &Path) -> Result<(), D6StoreError> {
    let handle = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    handle.sync_all()?;
    Ok(())
}

fn join_rel_bytes(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        let mut out = Vec::with_capacity(prefix.len() + 1 + name.len());
        out.extend_from_slice(prefix);
        out.push(b'/');
        out.extend_from_slice(name);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RawAttrState {
    Set,
    Unset,
    Unspecified,
    Value(String),
}

#[derive(Debug, Clone)]
struct AttrRuleEntry {
    anchored: bool,
    segments: Vec<Vec<u8>>,
    assignments: Vec<(String, RawAttrState)>,
}

#[derive(Debug, Clone)]
struct AttrLayerEntry {
    base_dir: Vec<u8>,
    rules: Vec<AttrRuleEntry>,
}

/// Audit all tracked `.gitattributes` files inside `tree` for `export-ignore`,
/// `export-subst`, `eol`, `text`, and `crlf` normalization impact (`#113` Defense 4).
pub fn audit_export_attributes(
    store: &CasMerkleStore,
    tree: &Digest,
) -> Result<ExportAttributesAudit, D6StoreError> {
    let flat = store.flatten_tree(tree)?;
    let mut attr_paths: Vec<Vec<u8>> = flat
        .iter()
        .filter_map(|(path, entry)| {
            let is_regular = matches!(entry.kind, TreeEntryKind::File | TreeEntryKind::Executable);
            let file_name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
            (is_regular && file_name == b".gitattributes").then(|| path.clone())
        })
        .collect();
    attr_paths.sort_by(|a, b| {
        let da = a.split(|&c| c == b'/').count();
        let db = b.split(|&c| c == b'/').count();
        da.cmp(&db).then_with(|| a.cmp(b))
    });

    let mut audit = ExportAttributesAudit {
        gitattributes_files: attr_paths
            .iter()
            .map(|p| TreePath::from_bytes(p.clone()))
            .collect(),
        ..ExportAttributesAudit::default()
    };
    if attr_paths.is_empty() {
        return Ok(audit);
    }

    let mut macros: HashMap<String, Vec<(String, RawAttrState)>> = HashMap::new();
    let mut layers = Vec::with_capacity(attr_paths.len());
    for attr_rel in attr_paths {
        let entry = &flat[&attr_rel];
        let digest = entry.digest().ok_or_else(|| {
            D6StoreError::InvalidInput("gitattributes entry missing digest".to_string())
        })?;
        let raw_bytes = store.cas().read_blob(&digest)?;
        let base_dir = match attr_rel.iter().rposition(|&b| b == b'/') {
            Some(pos) => attr_rel[..pos].to_vec(),
            None => Vec::new(),
        };
        let layer = parse_audit_layer(
            &TreePath::from_bytes(attr_rel),
            &base_dir,
            &raw_bytes,
            &mut macros,
            &mut audit.rules,
        )?;
        layers.push(layer);
    }

    for path in flat.keys() {
        let resolved = resolve_audit_path(path, &layers, &macros);
        if matches!(resolved.get("export-ignore"), Some(RawAttrState::Set)) {
            audit
                .export_ignore_paths
                .push(TreePath::from_bytes(path.clone()));
        }
        if matches!(resolved.get("export-subst"), Some(RawAttrState::Set)) {
            audit
                .export_subst_paths
                .push(TreePath::from_bytes(path.clone()));
        }
        if let Some(finding) = extract_eol_finding(path, &resolved) {
            audit.eol_paths.push(finding);
        }
    }

    Ok(audit)
}

fn extract_eol_finding(
    path: &[u8],
    resolved: &HashMap<String, RawAttrState>,
) -> Option<EolPathFinding> {
    let eol_state = resolved.get("eol");
    let text_state = resolved.get("text");
    let crlf_state = resolved.get("crlf");

    let eol_str = match eol_state {
        Some(RawAttrState::Value(v)) => Some(v.clone()),
        Some(RawAttrState::Set) => Some("set".to_string()),
        Some(RawAttrState::Unset) => Some("-eol".to_string()),
        _ => None,
    };
    let crlf_eol = match crlf_state {
        Some(RawAttrState::Value(v)) if v == "input" => Some("input".to_string()),
        Some(RawAttrState::Set) => Some("crlf".to_string()),
        Some(RawAttrState::Unset) => Some("-crlf".to_string()),
        _ => None,
    };
    let text_str = match text_state {
        Some(RawAttrState::Set) => Some("text".to_string()),
        Some(RawAttrState::Unset) => Some("-text".to_string()),
        Some(RawAttrState::Value(v)) => Some(v.clone()),
        _ => None,
    };

    if eol_str.is_none() && text_str.is_none() && crlf_eol.is_none() {
        return None;
    }

    let eol = eol_str
        .or(crlf_eol)
        .unwrap_or_else(|| "unspecified".to_string());
    Some(EolPathFinding {
        path: TreePath::from_bytes(path.to_vec()),
        eol,
        text: text_str,
    })
}

fn parse_audit_layer(
    source_file: &TreePath,
    base_dir: &[u8],
    bytes: &[u8],
    macros: &mut HashMap<String, Vec<(String, RawAttrState)>>,
    rule_findings: &mut Vec<ExportAttrRuleFinding>,
) -> Result<AttrLayerEntry, D6StoreError> {
    let allow_macros = base_dir.is_empty();
    let mut rules = Vec::new();
    for (idx, raw_line) in bytes.split(|&b| b == b'\n').enumerate() {
        let line_number = idx + 1;
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        let trimmed = trim_leading_ws(line);
        if trimmed.is_empty() || trimmed[0] == b'#' {
            continue;
        }
        let Some((raw_pat, rest)) = parse_pattern_token(trimmed)? else {
            continue;
        };
        let tokens_str = std::str::from_utf8(rest)
            .map_err(|_| D6StoreError::InvalidInput("non-utf8 gitattributes tokens".to_string()))?;
        let tokens: Vec<String> = tokens_str
            .split_ascii_whitespace()
            .map(str::to_string)
            .collect();
        if tokens.is_empty() {
            continue;
        }
        let parsed: Vec<(String, RawAttrState)> =
            tokens.iter().map(|t| parse_attr_assignment(t)).collect();

        if allow_macros && let Some(macro_name) = raw_pat.strip_prefix(b"[attr]") {
            let name = String::from_utf8(macro_name.to_vec()).map_err(|_| {
                D6StoreError::InvalidInput("non-utf8 gitattributes macro".to_string())
            })?;
            if !name.is_empty() {
                macros.insert(name, parsed);
            }
            continue;
        }

        if raw_pat.starts_with(b"!") || raw_pat.ends_with(b"/") {
            continue;
        }
        let mut pat = raw_pat;
        if let Some(escaped) = pat.strip_prefix(b"\\!") {
            let mut unescaped = Vec::with_capacity(escaped.len() + 1);
            unescaped.push(b'!');
            unescaped.extend_from_slice(escaped);
            pat = unescaped;
        }
        let anchored = if let Some(stripped) = pat.strip_prefix(b"/") {
            pat = stripped.to_vec();
            true
        } else {
            pat.contains(&b'/')
        };
        if pat.is_empty() {
            continue;
        }

        let relevant: Vec<String> = tokens
            .iter()
            .zip(parsed.iter())
            .filter(|(_, (name, _))| rule_touches_export_or_eol(name, macros, 0))
            .map(|(tok, _)| tok.clone())
            .collect();
        if !relevant.is_empty() {
            rule_findings.push(ExportAttrRuleFinding {
                source_file: source_file.clone(),
                line_number,
                pattern: String::from_utf8_lossy(&pat).into_owned(),
                attributes: relevant,
            });
        }

        let segments = pat.split(|&b| b == b'/').map(<[u8]>::to_vec).collect();
        rules.push(AttrRuleEntry {
            anchored,
            segments,
            assignments: parsed,
        });
    }
    Ok(AttrLayerEntry {
        base_dir: base_dir.to_vec(),
        rules,
    })
}

fn rule_touches_export_or_eol(
    name: &str,
    macros: &HashMap<String, Vec<(String, RawAttrState)>>,
    depth: usize,
) -> bool {
    if depth > 8 {
        return false;
    }
    if matches!(
        name,
        "export-ignore" | "export-subst" | "eol" | "text" | "crlf" | "binary"
    ) {
        return true;
    }
    macros.get(name).is_some_and(|items| {
        items
            .iter()
            .any(|(sub, _)| rule_touches_export_or_eol(sub, macros, depth + 1))
    })
}

fn resolve_audit_path(
    rel_path: &[u8],
    layers: &[AttrLayerEntry],
    macros: &HashMap<String, Vec<(String, RawAttrState)>>,
) -> HashMap<String, RawAttrState> {
    let mut raw_map = HashMap::new();
    for layer in layers {
        let scoped = if layer.base_dir.is_empty() {
            rel_path
        } else if let Some(after) = rel_path.strip_prefix(layer.base_dir.as_slice())
            && let Some(rest) = after.strip_prefix(b"/")
        {
            rest
        } else {
            continue;
        };
        for rule in &layer.rules {
            let matched = if !rule.anchored && rule.segments.len() == 1 {
                let base = scoped.rsplit(|&b| b == b'/').next().unwrap_or(scoped);
                match_attr_glob_seg(&rule.segments[0], base)
            } else {
                let path_segs: Vec<&[u8]> = scoped.split(|&b| b == b'/').collect();
                match_attr_glob_segs(&rule.segments, &path_segs)
            };
            if matched {
                for (name, state) in &rule.assignments {
                    apply_audit_assignment(name, state, macros, &mut raw_map, 0);
                }
            }
        }
    }
    raw_map
}

fn apply_audit_assignment(
    name: &str,
    state: &RawAttrState,
    macros: &HashMap<String, Vec<(String, RawAttrState)>>,
    raw_map: &mut HashMap<String, RawAttrState>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    raw_map.insert(name.to_string(), state.clone());
    match (name, state) {
        ("binary", RawAttrState::Set) => {
            apply_audit_assignment("diff", &RawAttrState::Unset, macros, raw_map, depth + 1);
            apply_audit_assignment("merge", &RawAttrState::Unset, macros, raw_map, depth + 1);
            apply_audit_assignment("text", &RawAttrState::Unset, macros, raw_map, depth + 1);
        }
        ("binary", RawAttrState::Unset) => {
            apply_audit_assignment("diff", &RawAttrState::Set, macros, raw_map, depth + 1);
            apply_audit_assignment("merge", &RawAttrState::Set, macros, raw_map, depth + 1);
            apply_audit_assignment("text", &RawAttrState::Set, macros, raw_map, depth + 1);
        }
        ("binary", RawAttrState::Unspecified) => {
            apply_audit_assignment(
                "diff",
                &RawAttrState::Unspecified,
                macros,
                raw_map,
                depth + 1,
            );
            apply_audit_assignment(
                "merge",
                &RawAttrState::Unspecified,
                macros,
                raw_map,
                depth + 1,
            );
            apply_audit_assignment(
                "text",
                &RawAttrState::Unspecified,
                macros,
                raw_map,
                depth + 1,
            );
        }
        _ => {
            if let Some(expanded) = macros.get(name) {
                for (sub_name, sub_state) in expanded {
                    let effective = match state {
                        RawAttrState::Set | RawAttrState::Value(_) => sub_state.clone(),
                        RawAttrState::Unset => match sub_state {
                            RawAttrState::Set => RawAttrState::Unset,
                            RawAttrState::Unset => RawAttrState::Set,
                            other => other.clone(),
                        },
                        RawAttrState::Unspecified => RawAttrState::Unspecified,
                    };
                    apply_audit_assignment(sub_name, &effective, macros, raw_map, depth + 1);
                }
            }
        }
    }
}

fn trim_leading_ws(mut line: &[u8]) -> &[u8] {
    while let Some((&b, rest)) = line.split_first() {
        if matches!(b, b' ' | b'\t') {
            line = rest;
        } else {
            break;
        }
    }
    line
}

type ParsedPattern<'a> = (Vec<u8>, &'a [u8]);

fn parse_pattern_token(line: &[u8]) -> Result<Option<ParsedPattern<'_>>, D6StoreError> {
    if line.is_empty() {
        return Ok(None);
    }
    if line[0] == b'"' {
        let mut pat = Vec::new();
        let mut i = 1;
        while i < line.len() {
            match line[i] {
                b'"' => return Ok(Some((pat, &line[i + 1..]))),
                b'\\' => {
                    if i + 1 >= line.len() {
                        return Err(D6StoreError::InvalidInput(
                            "dangling escape in .gitattributes pattern".to_string(),
                        ));
                    }
                    let esc = match line[i + 1] {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        other => other,
                    };
                    pat.push(esc);
                    i += 2;
                }
                b => {
                    pat.push(b);
                    i += 1;
                }
            }
        }
        return Err(D6StoreError::InvalidInput(
            "unclosed quote in .gitattributes pattern".to_string(),
        ));
    }
    let mut pat = Vec::new();
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' | b'\t' => return Ok(Some((pat, &line[i..]))),
            b'\\' if i + 1 < line.len() => {
                pat.push(line[i + 1]);
                i += 2;
            }
            b => {
                pat.push(b);
                i += 1;
            }
        }
    }
    Ok(Some((pat, &[])))
}

fn parse_attr_assignment(tok: &str) -> (String, RawAttrState) {
    if let Some(rest) = tok.strip_prefix('-') {
        (rest.to_string(), RawAttrState::Unset)
    } else if let Some(rest) = tok.strip_prefix('!') {
        (rest.to_string(), RawAttrState::Unspecified)
    } else if let Some((name, val)) = tok.split_once('=') {
        (name.to_string(), RawAttrState::Value(val.to_string()))
    } else {
        (tok.to_string(), RawAttrState::Set)
    }
}

fn match_attr_glob_segs(pat: &[Vec<u8>], path: &[&[u8]]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0].as_slice() == b"**" {
        if pat.len() == 1 {
            return !path.is_empty();
        }
        for split in 0..=path.len() {
            if match_attr_glob_segs(&pat[1..], &path[split..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    match_attr_glob_seg(&pat[0], path[0]) && match_attr_glob_segs(&pat[1..], &path[1..])
}

fn match_attr_glob_seg(pat: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut wildcard_pat_idx: Option<usize> = None;
    let mut backtrack_text_idx = 0;
    while ti < text.len() {
        if pi < pat.len() && pat[pi] == b'*' {
            wildcard_pat_idx = Some(pi);
            pi += 1;
            backtrack_text_idx = ti;
        } else if let Some(next_pi) = match_attr_token(pat, pi, text[ti]) {
            pi = next_pi;
            ti += 1;
        } else if let Some(sp) = wildcard_pat_idx {
            pi = sp + 1;
            backtrack_text_idx += 1;
            ti = backtrack_text_idx;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

fn match_attr_token(pat: &[u8], pi: usize, ch: u8) -> Option<usize> {
    if pi >= pat.len() {
        return None;
    }
    match pat[pi] {
        b'?' => Some(pi + 1),
        b'\\' => (pi + 1 < pat.len() && pat[pi + 1] == ch).then_some(pi + 2),
        b'[' => match_attr_char_class(pat, pi, ch),
        other => (other == ch).then_some(pi + 1),
    }
}

fn match_attr_char_class(pat: &[u8], start: usize, ch: u8) -> Option<usize> {
    let mut i = start + 1;
    let mut negated = false;
    if i < pat.len() && matches!(pat[i], b'!' | b'^') {
        negated = true;
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == b']' && !first {
            let ok = if negated { !hit } else { hit };
            return ok.then_some(i + 1);
        }
        first = false;
        let lo = pat[i];
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let hi = pat[i + 2];
            if lo <= ch && ch <= hi {
                hit = true;
            }
            i += 3;
        } else {
            if ch == lo {
                hit = true;
            }
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;
    use spawn_cas::{Tree, empty_tree_digest};

    #[test]
    fn empty_tree_and_invalid_paths_behave_fail_closed() {
        let scratch = Scratch::new("layer0_unit_empty");
        let store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
        let empty = empty_tree_digest();

        let dest = scratch.join("layers/empty");
        let rep = materialize_workspace_from_tree(&store, &empty, &dest).expect("empty layer0");
        assert_eq!(rep.tree, empty);
        assert_eq!(rep.files_written, 0);
        assert_eq!(rep.hardlinked_from_nearest, 0);
        assert!(dest.is_dir());

        let inside_cas = store.cas().root().join("bad_layer");
        let err = materialize_workspace_from_tree(&store, &empty, &inside_cas)
            .expect_err("refuse dest inside CAS root");
        assert!(matches!(err, D6StoreError::InvalidInput(_)));
    }

    #[test]
    fn export_attributes_audit_handles_macros_and_subdirectories() {
        let scratch = Scratch::new("layer0_unit_attrs");
        let store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");

        let root_attrs = b"[attr]ship export-ignore\n*.lock eol=lf\n*.snap export-subst\n";
        let sub_attrs = b"keep.lock -export-ignore text\n";
        let payload = b"hello\n";

        let (ra_id, ra_sz) = store.put_bytes_blob(root_attrs).expect("root attrs");
        let (sa_id, sa_sz) = store.put_bytes_blob(sub_attrs).expect("sub attrs");
        let (pl_id, pl_sz) = store.put_bytes_blob(payload).expect("payload");

        let sub_tree = store
            .put_tree_cached(
                &Tree::from_entries(vec![
                    TreeEntry::file(b".gitattributes".to_vec(), &sa_id, sa_sz),
                    TreeEntry::file(b"keep.lock".to_vec(), &pl_id, pl_sz),
                ])
                .expect("sub tree"),
            )
            .expect("store sub");

        let root_tree = store
            .put_tree_cached(
                &Tree::from_entries(vec![
                    TreeEntry::file(b".gitattributes".to_vec(), &ra_id, ra_sz),
                    TreeEntry::file(b"Cargo.lock".to_vec(), &pl_id, pl_sz),
                    TreeEntry::file(b"data.snap".to_vec(), &pl_id, pl_sz),
                    TreeEntry::dir(b"sub".to_vec(), &sub_tree),
                ])
                .expect("root tree"),
            )
            .expect("store root");

        let audit = audit_export_attributes(&store, &root_tree).expect("audit");
        assert!(audit.has_export_divergence_risk());
        assert!(audit.has_eol_rules());
        assert!(audit.export_ignore_paths.is_empty());
        assert_eq!(audit.export_subst_paths, vec![TreePath::from("data.snap")]);
        assert!(audit.gitattributes_files.len() == 2);

        // Now add a tracked file matched by `ship` (`export-ignore`) macro and `export-subst`.
        let extra_attrs = b"[attr]ship export-ignore\n*.gen ship\n*.subst export-subst\n";
        let (ea_id, ea_sz) = store.put_bytes_blob(extra_attrs).expect("extra attrs");
        let gen_tree = store
            .put_tree_cached(
                &Tree::from_entries(vec![
                    TreeEntry::file(b".gitattributes".to_vec(), &ea_id, ea_sz),
                    TreeEntry::file(b"out.gen".to_vec(), &pl_id, pl_sz),
                    TreeEntry::file(b"ver.subst".to_vec(), &pl_id, pl_sz),
                ])
                .expect("gen tree"),
            )
            .expect("store gen tree");

        let audit2 = audit_export_attributes(&store, &gen_tree).expect("audit2");
        assert!(audit2.has_export_divergence_risk());
        assert_eq!(audit2.export_ignore_paths, vec![TreePath::from("out.gen")]);
        assert_eq!(audit2.export_subst_paths, vec![TreePath::from("ver.subst")]);
    }
}

//! `spawn storage`: operator CLI for D6 persistent CAS cutover, rollback, and recovery (`#113`).
//!
//! Coordinates [`spawn_host::ProjectCutover`]'s lossless 7-step cutover from a project's bare
//! Git repository to D6 (`CasMerkleStore` + `PostgresRefLedger` + read-only Git border),
//! read-only status/parity/GC-fence verification, deterministic rollback back to writable Git,
//! `refs.journal` crash-recovery reconciliation, persistent GC, and offline bundle drills.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::rand::{SecureRandom, SystemRandom};

use serde::{Deserialize, Serialize};
use spawn_cas::{CAS_MARKER, CasError, DEFAULT_PERSISTENT_GC_GRACE, GcFenceBinding};
use spawn_host::d6_cutover::{BundleDrillReport, LocalOnlyLargeFile, ReconcileReport};
use spawn_host::{
    BACKEND_MARKER, CasMerkleStore, CutoverParityReport, CutoverPhase, D6StoreError, GcFenceStatus,
    GitBorderAdapter, ProjectCutover, ProjectStorageMode, WorkspaceBackend,
    assert_zero_pending_merges, bind_store_gc_fence, check_gc_fence, is_readonly_border,
    reconcile_refs_journal, root_backend, run_persistent_gc_for_ledger,
    verify_bundle_snapshot_roundtrip, verify_cutover_parity, verify_zero_pending_merges,
};
use spawn_postgres::PostgresStore;

use crate::config::{D6RuntimeConfig, Flags, env_value, open_store, workspace_root};
use crate::sources::PostgresRefLedger;

/// Usage line for `spawn storage` (`main`'s help reads this).
pub(crate) const USAGE: &str = "\
usage: spawn storage cutover --status|--to d6|--rollback git|--verify [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--reason <text>] [--json]
       spawn storage reconcile [--root <dir>] [--cas-dir <dir>] [--reason <prefix>] [--json]
       spawn storage gc [--root <dir>] [--cas-dir <dir>] [--grace-secs <secs>] [--json]
       spawn storage verify-bundle --bundle <output-path> [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--json]";

const CUTOVER_STATE_FILE: &str = ".cutover-state.json";
const CUTOVER_STATE_SCHEMA_VERSION: u32 = 2;
const LEGACY_CUTOVER_STATE_SCHEMA_VERSION: u32 = 1;
const SWITCHES: &[&str] = &["status", "verify", "json"];
const ALLOWED_FLAGS: &[&str] = &[
    "status", "to", "rollback", "verify", "project", "repo", "root", "cas-dir", "reason", "json",
];
const JSON_ONLY_SWITCHES: &[&str] = &["json"];
const RECONCILE_ALLOWED_FLAGS: &[&str] = &["root", "cas-dir", "reason", "json"];
const GC_ALLOWED_FLAGS: &[&str] = &["root", "cas-dir", "grace-secs", "json"];
const VERIFY_BUNDLE_ALLOWED_FLAGS: &[&str] =
    &["bundle", "project", "repo", "root", "cas-dir", "json"];

static NEXT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
enum StorageCommand {
    Cutover(ParsedArgs),
    Reconcile(ReconcileArgs),
    Gc(GcArgs),
    VerifyBundle(VerifyBundleArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutoverAction {
    Status,
    ToD6,
    RollbackGit,
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedArgs {
    action: CutoverAction,
    project: Option<String>,
    repo: Option<PathBuf>,
    root: PathBuf,
    cas_dir: PathBuf,
    reason: Option<String>,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReconcileArgs {
    cas_dir: PathBuf,
    reason_prefix: String,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GcArgs {
    cas_dir: PathBuf,
    grace: Duration,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BundleRepoArg {
    None,
    FromEnv(PathBuf),
    Explicit(PathBuf),
}

impl BundleRepoArg {
    fn path(&self) -> Option<&Path> {
        match self {
            Self::None => None,
            Self::FromEnv(path) | Self::Explicit(path) => Some(path.as_path()),
        }
    }

    fn explicit_path(&self) -> Option<&Path> {
        match self {
            Self::Explicit(path) => Some(path.as_path()),
            Self::None | Self::FromEnv(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyBundleArgs {
    cas_dir: PathBuf,
    bundle_path: PathBuf,
    project: Option<String>,
    repo: BundleRepoArg,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectCutoverRecord {
    project: String,
    bare_repo: String,
    phase: CutoverPhase,
    reason: Option<String>,
    updated_at_ms: i64,
}

impl ProjectCutoverRecord {
    const fn storage_mode(&self) -> ProjectStorageMode {
        self.phase.storage_mode()
    }

    const fn dispatch_paused(&self) -> bool {
        self.phase.dispatch_paused()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ProjectCutoverRecordView {
    project: String,
    bare_repo: String,
    phase: CutoverPhase,
    storage_mode: ProjectStorageMode,
    dispatch_paused: bool,
    reason: Option<String>,
    updated_at_ms: i64,
}

impl From<&ProjectCutoverRecord> for ProjectCutoverRecordView {
    fn from(rec: &ProjectCutoverRecord) -> Self {
        Self {
            project: rec.project.clone(),
            bare_repo: rec.bare_repo.clone(),
            phase: rec.phase,
            storage_mode: rec.storage_mode(),
            dispatch_paused: rec.dispatch_paused(),
            reason: rec.reason.clone(),
            updated_at_ms: rec.updated_at_ms,
        }
    }
}

fn deserialize_unique_projects<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct UniqueProjectsVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for UniqueProjectsVisitor<T> {
        type Value = BTreeMap<String, T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of unique project cutover records")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut projects = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if projects.contains_key(&key) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate project key `{key}`"
                    )));
                }
                projects.insert(key, value);
            }
            Ok(projects)
        }
    }

    deserializer.deserialize_map(UniqueProjectsVisitor(std::marker::PhantomData))
}

#[derive(Debug, Deserialize)]
struct CutoverStateVersionProbe {
    schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CutoverStateFile {
    schema_version: u32,
    #[serde(deserialize_with = "deserialize_unique_projects")]
    projects: BTreeMap<String, ProjectCutoverRecord>,
}

impl Default for CutoverStateFile {
    fn default() -> Self {
        Self {
            schema_version: CUTOVER_STATE_SCHEMA_VERSION,
            projects: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyProjectCutoverRecord {
    project: String,
    bare_repo: String,
    phase: CutoverPhase,
    storage_mode: ProjectStorageMode,
    dispatch_paused: bool,
    reason: Option<String>,
    updated_at_ms: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCutoverStateFile {
    schema_version: u32,
    #[serde(deserialize_with = "deserialize_unique_projects")]
    projects: BTreeMap<String, LegacyProjectCutoverRecord>,
}

#[derive(Debug, Clone, Serialize)]
struct GcFenceView {
    bound: bool,
    status: String,
    binding: Option<GcFenceBinding>,
    pg_latest_ref_seq: u64,
    journal_latest_seq: Option<u64>,
    detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct GcFenceStatusView {
    binding: GcFenceBinding,
    pg_latest_ref_seq: u64,
    journal_latest_seq: u64,
}

impl From<GcFenceStatus> for GcFenceStatusView {
    fn from(value: GcFenceStatus) -> Self {
        Self {
            binding: value.binding,
            pg_latest_ref_seq: value.pg_latest_ref_seq,
            journal_latest_seq: value.journal_latest_seq,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct PgIdentityView {
    db_system_id: String,
    db_oid: u32,
    schema_name: String,
    latest_ref_seq: u64,
}

#[derive(Debug, Clone, Serialize)]
struct StatusReport {
    schema_version: u32,
    action: &'static str,
    workspace_root: String,
    workspace_backend: Option<String>,
    pending_merges_clean: bool,
    cas_dir: String,
    cas_initialized: bool,
    pg_database: PgIdentityView,
    gc_fence: GcFenceView,
    project: Option<String>,
    bare_repo: Option<String>,
    readonly_border: Option<bool>,
    pg_ref_count: Option<usize>,
    cutover_state: Option<ProjectCutoverRecordView>,
    recorded_projects: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[expect(clippy::struct_excessive_bools)]
struct VerifyChecks {
    pending_merges_clean: bool,
    cas_initialized: bool,
    gc_fence: bool,
    ref_parity: bool,
    readonly_border: bool,
    workspace_backend_marker: bool,
}

#[derive(Debug, Clone, Serialize)]
struct VerifyReport {
    schema_version: u32,
    action: &'static str,
    ok: bool,
    project: String,
    bare_repo: String,
    workspace_root: String,
    workspace_backend: Option<String>,
    cas_dir: String,
    checks: VerifyChecks,
    gc_fence: GcFenceView,
    ref_count: Option<usize>,
    parity_error: Option<String>,
    cutover_state: Option<ProjectCutoverRecordView>,
}

#[derive(Debug, Clone, Serialize)]
struct CutoverResultReport {
    schema_version: u32,
    action: &'static str,
    target: &'static str,
    project: String,
    bare_repo: String,
    workspace_root: String,
    workspace_backend: String,
    cas_dir: String,
    phase: CutoverPhase,
    storage_mode: ProjectStorageMode,
    dispatch_paused: bool,
    readonly_border: bool,
    refs_verified: usize,
    backfill_commits_scanned: usize,
    tail_commits_scanned: usize,
    snapshots_imported: usize,
    gc_fence: GcFenceStatusView,
}

#[derive(Debug, Clone, Serialize)]
struct RollbackResultReport {
    schema_version: u32,
    action: &'static str,
    target: &'static str,
    project: String,
    bare_repo: String,
    workspace_root: String,
    workspace_backend: String,
    cas_dir: String,
    phase: CutoverPhase,
    storage_mode: ProjectStorageMode,
    dispatch_paused: bool,
    readonly_border: bool,
    exported_refs: usize,
    workspace_backend_kept_d6_for_projects: Vec<String>,
}

struct CutoverStepCounts {
    backfill_commits_scanned: usize,
    tail_commits_scanned: usize,
    snapshots_imported: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReconcileCliReport {
    schema_version: u32,
    action: &'static str,
    cas_dir: String,
    reason_prefix: String,
    entries_considered: usize,
    entries_replayed: usize,
    already_applied: usize,
    last_journal_seq: u64,
    pg_latest_ref_seq_before: u64,
    pg_latest_ref_seq_after: u64,
    last_ledger_seq: u64,
    gc_fence_aligned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct GcCliReport {
    schema_version: u32,
    action: &'static str,
    cas_dir: String,
    grace_secs: u64,
    pg_latest_ref_seq: u64,
    roots: usize,
    reachable_objects: usize,
    deleted_objects: Vec<String>,
    deleted_object_count: usize,
    deleted_chunk_indices: Vec<String>,
    deleted_chunk_index_count: usize,
    deferred_objects: usize,
    deferred_chunk_indices: usize,
    freed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BundleRefView {
    snapshot: String,
    git_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LocalOnlyLargeFileView {
    snapshot: String,
    path: String,
    digest: String,
    size: u64,
}

impl From<&LocalOnlyLargeFile> for LocalOnlyLargeFileView {
    fn from(item: &LocalOnlyLargeFile) -> Self {
        Self {
            snapshot: item.snapshot.to_string(),
            path: item.path.shown(),
            digest: item.digest.to_string(),
            size: item.size,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct VerifyBundleCliReport {
    schema_version: u32,
    action: &'static str,
    ok: bool,
    project: String,
    cas_dir: String,
    bundle_path: String,
    bundle_bytes: u64,
    snapshots_verified: usize,
    refs_verified_count: usize,
    refs_verified: BTreeMap<String, BundleRefView>,
    local_only_large_files: Vec<LocalOnlyLargeFileView>,
    missing_local_only_large_files: Vec<LocalOnlyLargeFileView>,
}

/// Entrypoint for `spawn storage ...`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    match parse_storage_command(rest)? {
        StorageCommand::Cutover(parsed) => match parsed.action {
            CutoverAction::Status => run_status(&parsed),
            CutoverAction::ToD6 => run_cutover_to_d6(&parsed),
            CutoverAction::RollbackGit => run_rollback_to_git(&parsed),
            CutoverAction::Verify => run_verify(&parsed),
        },
        StorageCommand::Reconcile(args) => run_reconcile(&args),
        StorageCommand::Gc(args) => run_gc(&args),
        StorageCommand::VerifyBundle(args) => run_verify_bundle(&args),
    }
}

fn parse_storage_command(rest: &[String]) -> Result<StorageCommand, String> {
    let Some((sub, tail)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    match sub.as_str() {
        "cutover" => parse_cutover_args(tail).map(StorageCommand::Cutover),
        "reconcile" => parse_reconcile_args(tail).map(StorageCommand::Reconcile),
        "gc" => parse_gc_args(tail).map(StorageCommand::Gc),
        "verify-bundle" => parse_verify_bundle_args(tail).map(StorageCommand::VerifyBundle),
        _ => Err(format!("unknown storage subcommand {sub}\n{USAGE}")),
    }
}

fn parse_allowed_flags(
    args: &[String],
    switches: &[&str],
    allowed: &[&str],
) -> Result<Flags, String> {
    let flags = Flags::parse_with_switches(args, switches).map_err(|e| format!("{e}\n{USAGE}"))?;
    for name in flags.names() {
        if !allowed.contains(&name) {
            return Err(format!("unknown flag --{name}\n{USAGE}"));
        }
    }
    Ok(flags)
}

fn parse_action(flags: &Flags) -> Result<CutoverAction, String> {
    let want_status = flags.switch("status");
    let want_verify = flags.switch("verify");
    let to_value = flags.get("to");
    let rollback_value = flags.get("rollback");

    let selected = usize::from(want_status)
        + usize::from(want_verify)
        + usize::from(to_value.is_some())
        + usize::from(rollback_value.is_some());
    if selected == 0 {
        return Err(format!(
            "specify one of --status, --to d6, --rollback git, or --verify\n{USAGE}"
        ));
    }
    if selected > 1 {
        return Err(format!(
            "--status, --to, --rollback, and --verify are mutually exclusive\n{USAGE}"
        ));
    }

    if want_status {
        Ok(CutoverAction::Status)
    } else if want_verify {
        Ok(CutoverAction::Verify)
    } else if let Some(target) = to_value {
        if target != "d6" {
            return Err(format!(
                "unsupported --to target {target:?}; only d6 is supported\n{USAGE}"
            ));
        }
        Ok(CutoverAction::ToD6)
    } else if let Some(target) = rollback_value {
        if target != "git" {
            return Err(format!(
                "unsupported --rollback target {target:?}; only git is supported\n{USAGE}"
            ));
        }
        Ok(CutoverAction::RollbackGit)
    } else {
        unreachable!("selected == 1 verified above")
    }
}

fn parse_non_empty_flag(flags: &Flags, name: &str) -> Result<Option<String>, String> {
    match flags.get(name) {
        Some(value) if value.trim().is_empty() => Err(format!("--{name} must not be empty")),
        other => Ok(other),
    }
}

fn parse_u64_flag(flags: &Flags, name: &str) -> Result<Option<u64>, String> {
    let Some(raw) = parse_non_empty_flag(flags, name)? else {
        return Ok(None);
    };
    let value = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("--{name} must be a non-negative integer, got {raw:?}"))?;
    Ok(Some(value))
}

fn normalize_cas_dir(path: &Path) -> Result<PathBuf, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!("cas dir must not be a symlink: {}", path.display()));
            }
            if !meta.is_dir() {
                return Err(format!("cas dir is not a directory: {}", path.display()));
            }
            fs::canonicalize(path).map_err(|e| format!("resolve cas dir {}: {e}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                let cwd =
                    std::env::current_dir().map_err(|e| format!("read current directory: {e}"))?;
                Ok(cwd.join(path))
            }
        }
        Err(err) => Err(format!("stat cas dir {}: {err}", path.display())),
    }
}

fn resolve_cas_dir_from_flags(flags: &Flags) -> Result<PathBuf, String> {
    let validated_root = if let Some(root_raw) = parse_non_empty_flag(flags, "root")? {
        let root_path = PathBuf::from(&root_raw);
        ensure_existing_dir_not_symlink(&root_path, "workspace root")?;
        Some(spawn_host::canonical_root(&root_path))
    } else {
        None
    };

    if let Some(explicit) = parse_non_empty_flag(flags, "cas-dir")? {
        return normalize_cas_dir(Path::new(&explicit));
    }

    let cfg = D6RuntimeConfig::from_env()?;
    if let Some(env_cas) = cfg.cas_dir {
        return normalize_cas_dir(&env_cas);
    }

    let root = if let Some(root) = validated_root {
        root
    } else if let Some(env_root) = env_value("SPAWN_WORKSPACE_ROOT") {
        let root_path = PathBuf::from(&env_root);
        ensure_existing_dir_not_symlink(&root_path, "workspace root")?;
        spawn_host::canonical_root(&root_path)
    } else {
        return Err(
            "missing CAS location: pass --cas-dir <dir>, --root <dir>, or set SPAWN_D6_CAS_DIR / SPAWN_WORKSPACE_ROOT"
                .to_string(),
        );
    };
    Ok(cfg.effective_cas_dir(&root, WorkspaceBackend::D6))
}

fn is_macos_root_system_symlink(cur: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let expected = match cur.to_str() {
            Some("/var") => Some("private/var"),
            Some("/tmp") => Some("private/tmp"),
            Some("/etc") => Some("private/etc"),
            _ => None,
        };
        if let Some(target) = expected {
            return fs::read_link(cur)
                .ok()
                .is_some_and(|link| link == Path::new(target));
        }
    }
    let _ = cur;
    false
}

fn absolutize_and_normalize_existing(path: &Path) -> Result<PathBuf, String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("read current directory: {e}"))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn resolve_path_via_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let normalized = absolutize_and_normalize_existing(path)?;
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = normalized.as_path();
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(_) => {
                let mut resolved = fs::canonicalize(cursor)
                    .map_err(|e| format!("canonicalize {}: {e}", cursor.display()))?;
                for part in suffix.into_iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name() else {
                    return Ok(normalized);
                };
                suffix.push(name.to_os_string());
                let Some(parent) = cursor.parent() else {
                    return Ok(normalized);
                };
                cursor = parent;
            }
            Err(err) => return Err(format!("stat {}: {err}", cursor.display())),
        }
    }
}

fn walk_bundle_components_no_symlink(abs: &Path) -> Result<PathBuf, String> {
    let components: Vec<Component<'_>> = abs.components().collect();
    let mut cur = PathBuf::new();
    let mut missing_seen = false;
    for (idx, comp) in components.iter().enumerate() {
        match comp {
            Component::Prefix(prefix) => cur.push(prefix.as_os_str()),
            Component::RootDir => cur.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if missing_seen {
                    return Err(format!(
                        "bundle output path must not traverse '..' through non-existent directory: {}",
                        abs.display()
                    ));
                }
                if !cur.pop() {
                    return Err(format!(
                        "invalid bundle output parent path: {}",
                        abs.display()
                    ));
                }
            }
            Component::Normal(part) => {
                cur.push(part);
                if missing_seen {
                    continue;
                }
                match fs::symlink_metadata(&cur) {
                    Ok(meta) => {
                        let has_more = idx + 1 < components.len();
                        if meta.file_type().is_symlink() && !is_macos_root_system_symlink(&cur) {
                            return Err(format!(
                                "bundle output path must not traverse or point to a symlink: {}",
                                cur.display()
                            ));
                        }
                        if has_more && !meta.is_dir() {
                            return Err(format!(
                                "bundle parent is not a directory: {}",
                                cur.display()
                            ));
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        missing_seen = true;
                    }
                    Err(err) => return Err(format!("stat bundle path {}: {err}", cur.display())),
                }
            }
        }
    }
    Ok(cur)
}

fn reject_bundle_target_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                Err(format!(
                    "bundle path must not be a symlink: {}",
                    path.display()
                ))
            } else if meta.is_dir() {
                Err(format!("bundle path is a directory: {}", path.display()))
            } else {
                Err(format!(
                    "bundle output path already exists (refusing to overwrite): {}",
                    path.display()
                ))
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("stat bundle path {}: {err}", path.display())),
    }
}

fn validate_bundle_output_path(
    raw: &Path,
    cas_dir: &Path,
    forbidden_repos: &[&Path],
) -> Result<PathBuf, String> {
    let raw_bytes = raw.as_os_str().as_encoded_bytes();
    if raw.as_os_str().is_empty()
        || raw_bytes.contains(&0)
        || raw_bytes.ends_with(b"/")
        || raw_bytes.ends_with(b"\\")
        || !matches!(raw.components().next_back(), Some(Component::Normal(_)))
    {
        return Err(format!(
            "bundle output path must name a file: {}",
            raw.display()
        ));
    }

    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("read current directory: {e}"))?
            .join(raw)
    };
    let walked = walk_bundle_components_no_symlink(&abs)?;
    let resolved = resolve_path_via_existing_ancestor(&walked)?;
    reject_bundle_target_if_exists(&resolved)?;

    let resolved_cas = resolve_path_via_existing_ancestor(cas_dir)?;
    if resolved == resolved_cas || resolved.starts_with(&resolved_cas) {
        return Err(format!(
            "refusing to write bundle inside CAS directory {}: {}",
            resolved_cas.display(),
            resolved.display()
        ));
    }
    for repo in forbidden_repos {
        let resolved_repo = resolve_path_via_existing_ancestor(repo)?;
        if resolved == resolved_repo || resolved.starts_with(&resolved_repo) {
            return Err(format!(
                "refusing to write bundle inside bare repo {}: {}",
                resolved_repo.display(),
                resolved.display()
            ));
        }
    }

    Ok(resolved)
}

fn parse_reconcile_args(args: &[String]) -> Result<ReconcileArgs, String> {
    let flags = parse_allowed_flags(args, JSON_ONLY_SWITCHES, RECONCILE_ALLOWED_FLAGS)?;
    let cas_dir = resolve_cas_dir_from_flags(&flags)?;
    let reason_prefix =
        parse_non_empty_flag(&flags, "reason")?.unwrap_or_else(|| "cli:reconcile".to_string());
    Ok(ReconcileArgs {
        cas_dir,
        reason_prefix,
        json: flags.switch("json"),
    })
}

fn parse_gc_args(args: &[String]) -> Result<GcArgs, String> {
    let flags = parse_allowed_flags(args, JSON_ONLY_SWITCHES, GC_ALLOWED_FLAGS)?;
    let cas_dir = resolve_cas_dir_from_flags(&flags)?;
    let grace = match parse_u64_flag(&flags, "grace-secs")? {
        Some(0) => {
            return Err(
                "--grace-secs must be at least 1 (0 risks deleting objects before ref update)"
                    .to_string(),
            );
        }
        Some(secs) => Duration::from_secs(secs),
        None => DEFAULT_PERSISTENT_GC_GRACE,
    };
    Ok(GcArgs {
        cas_dir,
        grace,
        json: flags.switch("json"),
    })
}

fn parse_verify_bundle_args(args: &[String]) -> Result<VerifyBundleArgs, String> {
    let flags = parse_allowed_flags(args, JSON_ONLY_SWITCHES, VERIFY_BUNDLE_ALLOWED_FLAGS)?;
    let cas_dir = resolve_cas_dir_from_flags(&flags)?;
    let bundle_raw = parse_non_empty_flag(&flags, "bundle")?
        .ok_or_else(|| format!("--bundle <output-path> is required\n{USAGE}"))?;
    let project = parse_non_empty_flag(&flags, "project")?;
    let explicit_repo = parse_non_empty_flag(&flags, "repo")?;
    let repo = match explicit_repo {
        Some(value) => BundleRepoArg::Explicit(normalize_repo_path(Path::new(&value))?),
        None if project.is_none() => match env_value("SPAWN_REPO") {
            Some(value) if value.trim().is_empty() => {
                return Err("--repo must not be empty".to_string());
            }
            Some(value) => BundleRepoArg::FromEnv(normalize_repo_path(Path::new(&value))?),
            None => BundleRepoArg::None,
        },
        None => BundleRepoArg::None,
    };
    let forbidden_repos: Vec<&Path> = repo.path().into_iter().collect();
    let bundle_path =
        validate_bundle_output_path(Path::new(&bundle_raw), &cas_dir, &forbidden_repos)?;

    Ok(VerifyBundleArgs {
        cas_dir,
        bundle_path,
        project,
        repo,
        json: flags.switch("json"),
    })
}

fn parse_cutover_args(args: &[String]) -> Result<ParsedArgs, String> {
    let flags = parse_allowed_flags(args, SWITCHES, ALLOWED_FLAGS)?;

    let action = parse_action(&flags)?;
    let project = parse_non_empty_flag(&flags, "project")?;
    let reason = parse_non_empty_flag(&flags, "reason")?;

    let root_raw = workspace_root(&flags).ok_or_else(|| {
        "missing workspace root: pass --root <dir> or set SPAWN_WORKSPACE_ROOT".to_string()
    })?;
    let root_path = PathBuf::from(&root_raw);
    ensure_existing_dir_not_symlink(&root_path, "workspace root")?;
    let root = spawn_host::canonical_root(&root_path);

    let cas_dir = if let Some(explicit) = parse_non_empty_flag(&flags, "cas-dir")? {
        normalize_cas_dir(Path::new(&explicit))?
    } else {
        D6RuntimeConfig::from_env()?.effective_cas_dir(&root, WorkspaceBackend::D6)
    };

    let repo = match parse_non_empty_flag(&flags, "repo")?.or_else(|| env_value("SPAWN_REPO")) {
        Some(value) if value.trim().is_empty() => {
            return Err("--repo must not be empty".to_string());
        }
        Some(value) => Some(normalize_repo_path(Path::new(&value))?),
        None => None,
    };

    Ok(ParsedArgs {
        action,
        project,
        repo,
        root,
        cas_dir,
        reason,
        json: flags.switch("json"),
    })
}

fn normalize_repo_path(path: &Path) -> Result<PathBuf, String> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "bare repo path must not be a symlink: {}",
                path.display()
            ));
        }
        return fs::canonicalize(path)
            .map_err(|e| format!("resolve bare repo {}: {e}", path.display()));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        let cwd = std::env::current_dir().map_err(|e| format!("read current directory: {e}"))?;
        Ok(cwd.join(path))
    }
}

fn ensure_existing_dir_not_symlink(path: &Path, label: &str) -> Result<(), String> {
    let meta =
        fs::symlink_metadata(path).map_err(|e| format!("{label} {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{label} must not be a symlink: {}", path.display()));
    }
    if !meta.is_dir() {
        return Err(format!("{label} is not a directory: {}", path.display()));
    }
    Ok(())
}

fn read_workspace_backend_safe(root: &Path) -> Result<Option<WorkspaceBackend>, String> {
    let marker_path = root.join(BACKEND_MARKER);
    match fs::symlink_metadata(&marker_path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "workspace backend marker must not be a symlink: {}",
            marker_path.display()
        )),
        Ok(_) | Err(_) => {
            root_backend(root).map_err(|e| format!("read {}: {e}", marker_path.display()))
        }
    }
}

fn write_atomic_no_symlink(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(format!("{label} must not be a symlink: {}", path.display()));
        }
        if meta.is_dir() {
            return Err(format!("{label} is a directory: {}", path.display()));
        }
    }

    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} has no parent directory: {}", path.display()))?;
    ensure_existing_dir_not_symlink(parent, &format!("{label} parent"))?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid file name for {label}: {}", path.display()))?;
    let seq = NEXT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!("{file_name}.tmp.{}.{seq}", std::process::id()));
    if fs::symlink_metadata(&tmp_path).is_ok() {
        return Err(format!(
            "refusing to overwrite existing temp path: {}",
            tmp_path.display()
        ));
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(|e| format!("create temp file {}: {e}", tmp_path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("write temp file {}: {e}", tmp_path.display()))?;
    drop(file);

    fs::rename(&tmp_path, path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp_path.display(), path.display()))?;
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn write_workspace_backend_marker(root: &Path, backend: WorkspaceBackend) -> Result<(), String> {
    let marker_path = root.join(BACKEND_MARKER);
    let content = format!("{}\n", backend.name());
    write_atomic_no_symlink(&marker_path, content.as_bytes(), "workspace backend marker")?;
    let verified = read_workspace_backend_safe(root)?;
    if verified != Some(backend) {
        return Err(format!(
            "workspace backend marker verification failed: expected {}, found {}",
            backend.name(),
            verified.map_or_else(|| "none".to_string(), |b| b.name().to_string())
        ));
    }
    Ok(())
}

fn open_file_no_symlink(path: &Path, label: &str) -> Result<Option<File>, String> {
    let link_meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("stat {label} {}: {err}", path.display())),
    };
    if link_meta.file_type().is_symlink() {
        return Err(format!("{label} must not be a symlink: {}", path.display()));
    }
    if !link_meta.is_file() {
        return Err(format!("{label} is not a regular file: {}", path.display()));
    }
    let file = File::open(path).map_err(|e| format!("open {label} {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        let open_meta = file
            .metadata()
            .map_err(|e| format!("fstat {label} {}: {e}", path.display()))?;
        if (link_meta.dev(), link_meta.ino()) != (open_meta.dev(), open_meta.ino()) {
            return Err(format!("{label} changed while opening: {}", path.display()));
        }
    }
    Ok(Some(file))
}

fn upgrade_legacy_record(
    rec: LegacyProjectCutoverRecord,
    state_path: &Path,
) -> Result<ProjectCutoverRecord, String> {
    if rec.phase == CutoverPhase::DispatchPaused {
        return Err(format!(
            "unsupported legacy v1 cutover phase {:?} for project {:?} in {}: dispatch_paused is schema v2 only",
            rec.phase,
            rec.project,
            state_path.display(),
        ));
    }
    let phase = match (rec.phase, rec.storage_mode, rec.dispatch_paused) {
        (CutoverPhase::BackfillImported, ProjectStorageMode::Git, true) => {
            CutoverPhase::DispatchPaused
        }
        (phase, mode, paused)
            if mode == phase.storage_mode() && paused == phase.dispatch_paused() =>
        {
            phase
        }
        (phase, mode, paused) => {
            return Err(format!(
                "contradictory legacy v1 cutover state for project {:?} in {}: phase {:?} implies storage_mode={:?}, dispatch_paused={}, found storage_mode={:?}, dispatch_paused={}",
                rec.project,
                state_path.display(),
                phase,
                phase.storage_mode(),
                phase.dispatch_paused(),
                mode,
                paused,
            ));
        }
    };
    Ok(ProjectCutoverRecord {
        project: rec.project,
        bare_repo: rec.bare_repo,
        phase,
        reason: rec.reason,
        updated_at_ms: rec.updated_at_ms,
    })
}

fn upgrade_legacy_cutover_state(
    legacy: LegacyCutoverStateFile,
    state_path: &Path,
) -> Result<CutoverStateFile, String> {
    if legacy.schema_version != LEGACY_CUTOVER_STATE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported legacy cutover state schema_version {} in {}",
            legacy.schema_version,
            state_path.display()
        ));
    }
    let mut projects = BTreeMap::new();
    for (key, rec) in legacy.projects {
        projects.insert(key, upgrade_legacy_record(rec, state_path)?);
    }
    Ok(CutoverStateFile {
        schema_version: CUTOVER_STATE_SCHEMA_VERSION,
        projects,
    })
}

fn read_cutover_state(cas_dir: &Path) -> Result<Option<CutoverStateFile>, String> {
    match fs::symlink_metadata(cas_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "cas dir must not be a symlink: {}",
                    cas_dir.display()
                ));
            }
            if !meta.is_dir() {
                return Err(format!("cas dir is not a directory: {}", cas_dir.display()));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("stat cas dir {}: {err}", cas_dir.display())),
    }

    let state_path = cas_dir.join(CUTOVER_STATE_FILE);
    let Some(mut file) = open_file_no_symlink(&state_path, "cutover state")? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", state_path.display()))?;
    let probe: CutoverStateVersionProbe = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse {}: {e}", state_path.display()))?;
    match probe.schema_version {
        CUTOVER_STATE_SCHEMA_VERSION => {
            let parsed: CutoverStateFile = serde_json::from_slice(&bytes)
                .map_err(|e| format!("parse {}: {e}", state_path.display()))?;
            Ok(Some(parsed))
        }
        LEGACY_CUTOVER_STATE_SCHEMA_VERSION => {
            let legacy: LegacyCutoverStateFile = serde_json::from_slice(&bytes)
                .map_err(|e| format!("parse legacy v1 {}: {e}", state_path.display()))?;
            upgrade_legacy_cutover_state(legacy, &state_path).map(Some)
        }
        version => Err(format!(
            "unsupported cutover state schema_version {version} in {}",
            state_path.display()
        )),
    }
}

fn write_cutover_state(cas_dir: &Path, state: &CutoverStateFile) -> Result<(), String> {
    match fs::symlink_metadata(cas_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "cas dir must not be a symlink: {}",
                    cas_dir.display()
                ));
            }
            if !meta.is_dir() {
                return Err(format!("cas dir is not a directory: {}", cas_dir.display()));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(cas_dir)
                .map_err(|e| format!("create cas dir {}: {e}", cas_dir.display()))?;
        }
        Err(err) => return Err(format!("stat cas dir {}: {err}", cas_dir.display())),
    }

    let state_path = cas_dir.join(CUTOVER_STATE_FILE);
    let mut bytes =
        serde_json::to_vec_pretty(state).map_err(|e| format!("serialize cutover state: {e}"))?;
    bytes.push(b'\n');
    write_atomic_no_symlink(&state_path, &bytes, "cutover state")
}

fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn save_cutover_record(
    cas_dir: &Path,
    cutover: &ProjectCutover,
    bare_repo: &Path,
    reason: Option<&str>,
) -> Result<ProjectCutoverRecord, String> {
    let mut state = read_cutover_state(cas_dir)?.unwrap_or_default();
    let record = ProjectCutoverRecord {
        project: cutover.project().to_string(),
        bare_repo: bare_repo.to_string_lossy().into_owned(),
        phase: cutover.phase(),
        reason: reason.map(ToString::to_string),
        updated_at_ms: now_ms(),
    };
    state
        .projects
        .insert(record.project.clone(), record.clone());
    write_cutover_state(cas_dir, &state)?;
    Ok(record)
}

fn cas_is_initialized(cas_dir: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(cas_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "cas dir must not be a symlink: {}",
                    cas_dir.display()
                ));
            }
            if !meta.is_dir() {
                return Err(format!("cas dir is not a directory: {}", cas_dir.display()));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("stat cas dir {}: {err}", cas_dir.display())),
    }
    let marker = cas_dir.join(CAS_MARKER);
    match fs::symlink_metadata(&marker) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "CAS marker must not be a symlink: {}",
                    marker.display()
                ));
            }
            Ok(meta.is_file())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("stat CAS marker {}: {err}", marker.display())),
    }
}

fn live_gc_binding(pg: &PostgresStore) -> Result<(GcFenceBinding, u64), String> {
    let ident = pg
        .database_identity()
        .map_err(|e| format!("query PostgreSQL database identity: {e}"))?;
    let seq = pg
        .latest_ref_log_seq()
        .map_err(|e| format!("query latest spawn_ref_log seq: {e}"))?;
    Ok((
        GcFenceBinding {
            db_system_id: ident.db_system_id,
            db_oid: ident.db_oid,
            schema_name: ident.schema_name,
        },
        seq,
    ))
}

fn inspect_gc_fence_readonly(
    cas_dir: &Path,
    cas_initialized: bool,
    live_binding: &GcFenceBinding,
    pg_latest_ref_seq: u64,
) -> Result<(Option<CasMerkleStore>, GcFenceView), String> {
    if !cas_initialized {
        return Ok((
            None,
            GcFenceView {
                bound: false,
                status: "cas_uninitialized".to_string(),
                binding: None,
                pg_latest_ref_seq,
                journal_latest_seq: None,
                detail: None,
            },
        ));
    }

    let store = CasMerkleStore::open(cas_dir)
        .map_err(|e| format!("open CAS store {}: {e}", cas_dir.display()))?;
    let stored_binding = store
        .cas()
        .read_gc_fence()
        .map_err(|e| format!("read .gc-fence in {}: {e}", cas_dir.display()))?;

    let Some(bound) = stored_binding else {
        return Ok((
            Some(store),
            GcFenceView {
                bound: false,
                status: "unbound".to_string(),
                binding: None,
                pg_latest_ref_seq,
                journal_latest_seq: None,
                detail: Some("persistent CAS root is initialized without .gc-fence".to_string()),
            },
        ));
    };

    match check_gc_fence(&store, live_binding, pg_latest_ref_seq) {
        Ok(ok) => Ok((
            Some(store),
            GcFenceView {
                bound: true,
                status: "ok".to_string(),
                binding: Some(ok.binding),
                pg_latest_ref_seq: ok.pg_latest_ref_seq,
                journal_latest_seq: Some(ok.journal_latest_seq),
                detail: None,
            },
        )),
        Err(D6StoreError::Cas(CasError::GcFenceMismatch { .. })) => Ok((
            Some(store),
            GcFenceView {
                bound: true,
                status: "mismatch".to_string(),
                binding: Some(bound),
                pg_latest_ref_seq,
                journal_latest_seq: None,
                detail: Some(
                    "CAS .gc-fence database identity does not match live PostgreSQL".to_string(),
                ),
            },
        )),
        Err(D6StoreError::Cas(CasError::GcFenceBehindJournal {
            pg_latest_ref_seq: pg_seq,
            journal_latest_seq,
        })) => Ok((
            Some(store),
            GcFenceView {
                bound: true,
                status: "behind_journal".to_string(),
                binding: Some(bound),
                pg_latest_ref_seq: pg_seq,
                journal_latest_seq: Some(journal_latest_seq),
                detail: Some(format!(
                    "PostgreSQL spawn_ref_log seq {pg_seq} lags refs.journal seq {journal_latest_seq}"
                )),
            },
        )),
        Err(err) => Ok((
            Some(store),
            GcFenceView {
                bound: true,
                status: "error".to_string(),
                binding: Some(bound),
                pg_latest_ref_seq,
                journal_latest_seq: None,
                detail: Some(err.to_string()),
            },
        )),
    }
}

fn resolve_target(
    args: &ParsedArgs,
    state: Option<&CutoverStateFile>,
    require_target: bool,
) -> Result<Option<(String, PathBuf, Option<ProjectCutoverRecord>)>, String> {
    if let Some(repo) = &args.repo {
        let project = args
            .project
            .clone()
            .unwrap_or_else(|| repo.to_string_lossy().into_owned());
        let existing = state.and_then(|s| s.projects.get(&project).cloned());
        if let Some(rec) = &existing
            && Path::new(&rec.bare_repo) != repo.as_path()
        {
            return Err(format!(
                "project {project:?} is recorded for bare repo {}, not {}",
                rec.bare_repo,
                repo.display()
            ));
        }
        return Ok(Some((project, repo.clone(), existing)));
    }

    if let Some(project) = &args.project {
        if let Some(rec) = state.and_then(|s| s.projects.get(project)) {
            let repo = normalize_repo_path(Path::new(&rec.bare_repo))?;
            return Ok(Some((project.clone(), repo, Some(rec.clone()))));
        }
        if require_target {
            return Err(format!(
                "missing --repo <bare repo> for project {project:?} (not found in {})",
                args.cas_dir.join(CUTOVER_STATE_FILE).display()
            ));
        }
        return Ok(None);
    }

    if let Some(s) = state
        && s.projects.len() == 1
        && let Some(rec) = s.projects.values().next()
    {
        let repo = normalize_repo_path(Path::new(&rec.bare_repo))?;
        return Ok(Some((rec.project.clone(), repo, Some(rec.clone()))));
    }

    if require_target {
        return Err(
            "missing --repo <bare repo> (and no unique project recorded in cutover state)"
                .to_string(),
        );
    }
    Ok(None)
}

fn audit_pending_merges_clean(root: &Path) -> Result<bool, String> {
    let audit = verify_zero_pending_merges(root)
        .map_err(|e| format!("audit pending merges in {}: {e}", root.display()))?;
    Ok(audit.is_clean())
}

fn run_status(args: &ParsedArgs) -> Result<i32, String> {
    let workspace_backend = read_workspace_backend_safe(&args.root)?;
    let pending_merges_clean = audit_pending_merges_clean(&args.root)?;
    let cas_initialized = cas_is_initialized(&args.cas_dir)?;
    let state = read_cutover_state(&args.cas_dir)?;

    let pg = open_store()?;
    let (live_binding, pg_latest_ref_seq) = live_gc_binding(&pg)?;
    let (_store_opt, gc_fence) = inspect_gc_fence_readonly(
        &args.cas_dir,
        cas_initialized,
        &live_binding,
        pg_latest_ref_seq,
    )?;

    let (project, bare_repo, cutover_state) = match resolve_target(args, state.as_ref(), false)? {
        Some((p, r, rec)) => (Some(p), Some(r), rec),
        None => (args.project.clone(), None, None),
    };
    let readonly_border = match &bare_repo {
        Some(repo) if repo.exists() => Some(
            is_readonly_border(repo)
                .map_err(|e| format!("inspect bare repo {}: {e}", repo.display()))?,
        ),
        _ => None,
    };
    let pg_ref_count = match project.as_deref() {
        Some(proj) => Some(
            pg.list_refs(Some(proj))
                .map_err(|e| format!("list PostgreSQL spawn_refs: {e}"))?
                .len(),
        ),
        None => None,
    };
    let recorded_projects = state
        .as_ref()
        .map(|s| s.projects.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    let report = StatusReport {
        schema_version: 1,
        action: "status",
        workspace_root: args.root.to_string_lossy().into_owned(),
        workspace_backend: workspace_backend.map(|b| b.name().to_string()),
        pending_merges_clean,
        cas_dir: args.cas_dir.to_string_lossy().into_owned(),
        cas_initialized,
        pg_database: PgIdentityView {
            db_system_id: live_binding.db_system_id,
            db_oid: live_binding.db_oid,
            schema_name: live_binding.schema_name,
            latest_ref_seq: pg_latest_ref_seq,
        },
        gc_fence,
        project,
        bare_repo: bare_repo.map(|r| r.to_string_lossy().into_owned()),
        readonly_border,
        pg_ref_count,
        cutover_state: cutover_state.as_ref().map(ProjectCutoverRecordView::from),
        recorded_projects,
    };

    if args.json {
        print_json(&report)?;
    } else {
        print_status_human(&report);
    }
    Ok(0)
}

struct CutoverRunContext<'a> {
    project: &'a str,
    bare_repo: &'a Path,
    args: &'a ParsedArgs,
    store: &'a CasMerkleStore,
    ledger: &'a PostgresRefLedger,
    border: &'a GitBorderAdapter,
    reason: &'a str,
}

fn initialize_or_resume_cutover(
    ctx: &CutoverRunContext<'_>,
    existing_record: Option<ProjectCutoverRecord>,
) -> Result<(ProjectCutover, CutoverStepCounts), String> {
    if let Some(rec) = existing_record
        && rec.storage_mode() == ProjectStorageMode::D6Cas
    {
        return Ok((
            ProjectCutover::from_state(ctx.project, ctx.bare_repo, &ctx.args.root, rec.phase),
            CutoverStepCounts {
                backfill_commits_scanned: 0,
                tail_commits_scanned: 0,
                snapshots_imported: 0,
            },
        ));
    }

    let mut cutover = ProjectCutover::new(ctx.project, ctx.bare_repo, &ctx.args.root);
    let step1 = cutover
        .step1_backfill_history(ctx.store, ctx.ledger)
        .map_err(|e| format!("step1 backfill history failed: {e}"))?;
    let _step2 = cutover
        .step2_pause_dispatch_and_verify_clean(ctx.store)
        .map_err(|e| format!("step2 pause dispatch and clean check failed: {e}"))?;
    let step3 = cutover
        .step3_import_tail_commits(ctx.store, ctx.ledger)
        .map_err(|e| format!("step3 import tail commits failed: {e}"))?;
    let step4 = cutover
        .step4_commit_refs_and_flip_mode(ctx.store, ctx.ledger, ctx.border, ctx.reason)
        .map_err(|e| format!("step4 commit refs and flip mode failed: {e}"))?;
    save_cutover_record(&ctx.args.cas_dir, &cutover, ctx.bare_repo, Some(ctx.reason))?;
    Ok((
        cutover,
        CutoverStepCounts {
            backfill_commits_scanned: step1.commits_scanned,
            tail_commits_scanned: step3.commits_scanned,
            snapshots_imported: step4.snapshots_imported,
        },
    ))
}

fn run_cutover_to_d6(args: &ParsedArgs) -> Result<i32, String> {
    assert_zero_pending_merges(&args.root)
        .map_err(|e| format!("pre-cutover pending merge check failed: {e}"))?;

    let pre_state = read_cutover_state(&args.cas_dir)?;
    let Some((project, bare_repo, existing_record)) =
        resolve_target(args, pre_state.as_ref(), true)?
    else {
        unreachable!("require_target == true always returns Some or Err")
    };
    ensure_existing_dir_not_symlink(&bare_repo, "bare repo")?;

    if let Some(rec) = &existing_record
        && rec.phase == CutoverPhase::Resumed
        && rec.storage_mode() == ProjectStorageMode::D6Cas
    {
        return Err(format!(
            "project {project:?} is already cut over to d6_cas (phase: resumed); run --verify to audit or --rollback git to revert"
        ));
    }

    let pg = Arc::new(open_store()?);
    let ledger = PostgresRefLedger::new(Arc::clone(&pg));
    let (live_binding, _) = live_gc_binding(&pg)?;
    let store = CasMerkleStore::open(&args.cas_dir)
        .map_err(|e| format!("open CAS store {}: {e}", args.cas_dir.display()))?;
    ensure_or_bind_gc_fence(&store, &pg, &live_binding)?;

    let border = GitBorderAdapter::new();
    let reason = args.reason.as_deref().unwrap_or("cli cutover to d6");
    let ctx = CutoverRunContext {
        project: &project,
        bare_repo: &bare_repo,
        args,
        store: &store,
        ledger: &ledger,
        border: &border,
        reason,
    };
    let (mut cutover, counts) = initialize_or_resume_cutover(&ctx, existing_record)?;

    let refs_verified = finish_post_step4_steps(
        &mut cutover,
        &store,
        &ledger,
        &border,
        &args.cas_dir,
        &bare_repo,
        reason,
    )?;

    let post_seq = pg
        .latest_ref_log_seq()
        .map_err(|e| format!("query latest spawn_ref_log seq: {e}"))?;
    let gc_fence = check_gc_fence(&store, &live_binding, post_seq)
        .map_err(|e| format!("post-cutover GC fence verification failed: {e}"))?;

    save_cutover_record(&args.cas_dir, &cutover, &bare_repo, Some(reason))?;
    write_workspace_backend_marker(&args.root, WorkspaceBackend::D6)?;

    let readonly_border = is_readonly_border(&bare_repo)
        .map_err(|e| format!("inspect readonly border {}: {e}", bare_repo.display()))?;
    let report = CutoverResultReport {
        schema_version: 1,
        action: "cutover",
        target: "d6",
        project,
        bare_repo: bare_repo.to_string_lossy().into_owned(),
        workspace_root: args.root.to_string_lossy().into_owned(),
        workspace_backend: WorkspaceBackend::D6.name().to_string(),
        cas_dir: args.cas_dir.to_string_lossy().into_owned(),
        phase: cutover.phase(),
        storage_mode: cutover.storage_mode(),
        dispatch_paused: cutover.dispatch_paused(),
        readonly_border,
        refs_verified,
        backfill_commits_scanned: counts.backfill_commits_scanned,
        tail_commits_scanned: counts.tail_commits_scanned,
        snapshots_imported: counts.snapshots_imported,
        gc_fence: gc_fence.into(),
    };

    if args.json {
        print_json(&report)?;
    } else {
        println!(
            "spawn storage cutover: project={} mode=d6_cas phase={:?} refs_verified={} readonly_border={} workspace_backend=d6",
            report.project, report.phase, report.refs_verified, report.readonly_border
        );
    }
    Ok(0)
}

fn ensure_or_bind_gc_fence(
    store: &CasMerkleStore,
    pg: &PostgresStore,
    live_binding: &GcFenceBinding,
) -> Result<(), String> {
    let existing = store
        .cas()
        .read_gc_fence()
        .map_err(|e| format!("read .gc-fence: {e}"))?;
    if existing.is_none() {
        bind_store_gc_fence(store, live_binding).map_err(|e| format!("bind .gc-fence: {e}"))?;
    }
    let pg_seq = pg
        .latest_ref_log_seq()
        .map_err(|e| format!("query latest spawn_ref_log seq: {e}"))?;
    check_gc_fence(store, live_binding, pg_seq)
        .map_err(|e| format!("GC fence check failed: {e}"))?;
    Ok(())
}

fn finish_post_step4_steps(
    cutover: &mut ProjectCutover,
    store: &CasMerkleStore,
    ledger: &PostgresRefLedger,
    border: &GitBorderAdapter,
    cas_dir: &Path,
    bare_repo: &Path,
    reason: &str,
) -> Result<usize, String> {
    if cutover.phase() == CutoverPhase::RefsCommitted
        && let Err(err) = cutover.step5_verify_parity(store, ledger)
    {
        let _ = save_cutover_record(cas_dir, cutover, bare_repo, Some(reason));
        return Err(format!("step5 verify parity failed: {err}"));
    }
    if cutover.phase() == CutoverPhase::ParityVerified
        && let Err(err) = cutover.step6_configure_readonly_border(border)
    {
        let _ = save_cutover_record(cas_dir, cutover, bare_repo, Some(reason));
        return Err(format!("step6 configure readonly border failed: {err}"));
    }
    if cutover.phase() == CutoverPhase::BorderReadOnly
        && let Err(err) = cutover.step7_resume_dispatch()
    {
        let _ = save_cutover_record(cas_dir, cutover, bare_repo, Some(reason));
        return Err(format!("step7 resume dispatch failed: {err}"));
    }
    let refs_verified = if let Some(parity) = cutover.parity_report() {
        parity.refs.len()
    } else {
        verify_cutover_parity(store, ledger, cutover.project(), bare_repo)
            .map_err(|e| format!("verify parity: {e}"))?
            .refs
            .len()
    };
    Ok(refs_verified)
}

fn run_verify(args: &ParsedArgs) -> Result<i32, String> {
    let pending_merges_clean = audit_pending_merges_clean(&args.root)?;
    let workspace_backend = read_workspace_backend_safe(&args.root)?;
    let workspace_backend_marker = workspace_backend == Some(WorkspaceBackend::D6);
    let cas_initialized = cas_is_initialized(&args.cas_dir)?;
    let state = read_cutover_state(&args.cas_dir)?;

    let Some((project, bare_repo, cutover_state)) = resolve_target(args, state.as_ref(), true)?
    else {
        unreachable!("require_target == true always returns Some or Err")
    };
    ensure_existing_dir_not_symlink(&bare_repo, "bare repo")?;

    let pg = Arc::new(open_store()?);
    let ledger = PostgresRefLedger::new(Arc::clone(&pg));
    let (live_binding, pg_latest_ref_seq) = live_gc_binding(&pg)?;

    let (store_opt, gc_fence_view) = inspect_gc_fence_readonly(
        &args.cas_dir,
        cas_initialized,
        &live_binding,
        pg_latest_ref_seq,
    )?;
    let gc_fence_ok = gc_fence_view.status == "ok";

    let (ref_parity, ref_count, parity_error) = match store_opt {
        Some(ref store) => match verify_cutover_parity(store, &ledger, &project, &bare_repo) {
            Ok(CutoverParityReport { refs, .. }) => (true, Some(refs.len()), None),
            Err(err) => (false, None, Some(err.to_string())),
        },
        None => (
            false,
            None,
            Some("CAS store is not initialized".to_string()),
        ),
    };

    let readonly_border = is_readonly_border(&bare_repo)
        .map_err(|e| format!("inspect readonly border {}: {e}", bare_repo.display()))?;

    let ok = pending_merges_clean
        && cas_initialized
        && gc_fence_ok
        && ref_parity
        && readonly_border
        && workspace_backend_marker;

    let report = VerifyReport {
        schema_version: 1,
        action: "verify",
        ok,
        project,
        bare_repo: bare_repo.to_string_lossy().into_owned(),
        workspace_root: args.root.to_string_lossy().into_owned(),
        workspace_backend: workspace_backend.map(|b| b.name().to_string()),
        cas_dir: args.cas_dir.to_string_lossy().into_owned(),
        checks: VerifyChecks {
            pending_merges_clean,
            cas_initialized,
            gc_fence: gc_fence_ok,
            ref_parity,
            readonly_border,
            workspace_backend_marker,
        },
        gc_fence: gc_fence_view,
        ref_count,
        parity_error,
        cutover_state: cutover_state.as_ref().map(ProjectCutoverRecordView::from),
    };

    if args.json {
        print_json(&report)?;
    } else {
        print_verify_human(&report);
    }
    Ok(i32::from(!ok))
}

fn require_bound_cas_identity(
    cas_dir: &Path,
    pg: &PostgresStore,
    action: &str,
) -> Result<(CasMerkleStore, GcFenceBinding, u64), String> {
    if !cas_is_initialized(cas_dir)? {
        return Err(format!(
            "CAS store is not initialized in {}; refusing {action}",
            cas_dir.display()
        ));
    }
    let (live_binding, pg_latest_ref_seq) = live_gc_binding(pg)?;
    let store = CasMerkleStore::open(cas_dir)
        .map_err(|e| format!("open CAS store {}: {e}", cas_dir.display()))?;
    let Some(bound) = store
        .cas()
        .read_gc_fence()
        .map_err(|e| format!("read .gc-fence: {e}"))?
    else {
        return Err(format!(
            "CAS store {} has no .gc-fence binding; refusing {action}",
            cas_dir.display()
        ));
    };
    if bound != live_binding {
        return Err(format!(
            "GC fence check failed before {action}: CAS .gc-fence ({}:{}:{}) does not match live PostgreSQL ({}:{}:{})",
            bound.db_system_id,
            bound.db_oid,
            bound.schema_name,
            live_binding.db_system_id,
            live_binding.db_oid,
            live_binding.schema_name
        ));
    }
    Ok((store, live_binding, pg_latest_ref_seq))
}

fn require_bound_checked_cas_for(
    cas_dir: &Path,
    pg: &PostgresStore,
    action: &str,
) -> Result<(CasMerkleStore, GcFenceBinding, u64), String> {
    let (store, live_binding, pg_latest_ref_seq) = require_bound_cas_identity(cas_dir, pg, action)?;
    check_gc_fence(&store, &live_binding, pg_latest_ref_seq)
        .map_err(|e| format!("GC fence check failed before {action}: {e}"))?;
    Ok((store, live_binding, pg_latest_ref_seq))
}

fn execute_reconcile(
    args: &ReconcileArgs,
    pg: &Arc<PostgresStore>,
) -> Result<ReconcileCliReport, String> {
    let (store, _, pg_latest_ref_seq_before) =
        require_bound_cas_identity(&args.cas_dir, pg, "reconcile")?;
    let ledger = PostgresRefLedger::new(Arc::clone(pg));
    let report: ReconcileReport = reconcile_refs_journal(
        &store,
        &ledger,
        pg_latest_ref_seq_before,
        &args.reason_prefix,
    )
    .map_err(|e| format!("reconcile refs.journal failed: {e}"))?;
    let (_, pg_latest_ref_seq_after) = live_gc_binding(pg)?;
    Ok(ReconcileCliReport {
        schema_version: 1,
        action: "reconcile",
        cas_dir: args.cas_dir.to_string_lossy().into_owned(),
        reason_prefix: args.reason_prefix.clone(),
        entries_considered: report.entries_considered,
        entries_replayed: report.entries_replayed,
        already_applied: report.already_applied,
        last_journal_seq: report.last_journal_seq,
        pg_latest_ref_seq_before,
        pg_latest_ref_seq_after,
        last_ledger_seq: report.last_ledger_seq,
        gc_fence_aligned: pg_latest_ref_seq_after >= report.last_journal_seq,
    })
}

fn reconcile_exit_code_and_error(report: &ReconcileCliReport) -> (i32, Option<String>) {
    if report.gc_fence_aligned {
        (0, None)
    } else {
        (
            1,
            Some(format!(
                "reconcile incomplete: PostgreSQL ref log seq {} still trails refs.journal seq {} (gc_fence_aligned=false); GC will refuse until PG catches up",
                report.pg_latest_ref_seq_after, report.last_journal_seq
            )),
        )
    }
}

fn run_reconcile(args: &ReconcileArgs) -> Result<i32, String> {
    let pg = Arc::new(open_store()?);
    let report = execute_reconcile(args, &pg)?;
    if args.json {
        print_json(&report)?;
    } else {
        println!(
            "spawn storage reconcile: cas_dir={} replayed={}/{} already_applied={} pg_seq={}->{} journal_seq={} aligned={}",
            report.cas_dir,
            report.entries_replayed,
            report.entries_considered,
            report.already_applied,
            report.pg_latest_ref_seq_before,
            report.pg_latest_ref_seq_after,
            report.last_journal_seq,
            report.gc_fence_aligned
        );
    }
    let (code, err_msg) = reconcile_exit_code_and_error(&report);
    if let Some(msg) = err_msg {
        eprintln!("spawn storage reconcile: {msg}");
    }
    Ok(code)
}

fn execute_gc(args: &GcArgs, pg: &Arc<PostgresStore>) -> Result<GcCliReport, String> {
    if args.grace < Duration::from_secs(1) {
        return Err("--grace-secs must be at least 1".to_string());
    }
    let (store, live_binding, pg_latest_ref_seq) =
        require_bound_checked_cas_for(&args.cas_dir, pg, "gc")?;
    let ledger = PostgresRefLedger::new(Arc::clone(pg));
    let report = run_persistent_gc_for_ledger(
        &store,
        &ledger,
        &live_binding,
        pg_latest_ref_seq,
        args.grace,
    )
    .map_err(|e| format!("persistent GC failed: {e}"))?;
    let deleted_objects: Vec<String> = report
        .deleted_objects
        .iter()
        .map(ToString::to_string)
        .collect();
    let deleted_chunk_indices: Vec<String> = report
        .deleted_chunk_indices
        .iter()
        .map(ToString::to_string)
        .collect();
    Ok(GcCliReport {
        schema_version: 1,
        action: "gc",
        cas_dir: args.cas_dir.to_string_lossy().into_owned(),
        grace_secs: args.grace.as_secs(),
        pg_latest_ref_seq,
        roots: report.roots,
        reachable_objects: report.reachable_objects,
        deleted_object_count: deleted_objects.len(),
        deleted_objects,
        deleted_chunk_index_count: deleted_chunk_indices.len(),
        deleted_chunk_indices,
        deferred_objects: report.deferred_objects,
        deferred_chunk_indices: report.deferred_chunk_indices,
        freed_bytes: report.freed_bytes,
    })
}

fn run_gc(args: &GcArgs) -> Result<i32, String> {
    let pg = Arc::new(open_store()?);
    let report = execute_gc(args, &pg)?;
    if args.json {
        print_json(&report)?;
    } else {
        println!(
            "spawn storage gc: cas_dir={} roots={} reachable={} deleted_objects={} deleted_chunk_indices={} deferred_objects={} deferred_chunk_indices={} freed_bytes={} grace_secs={}",
            report.cas_dir,
            report.roots,
            report.reachable_objects,
            report.deleted_object_count,
            report.deleted_chunk_index_count,
            report.deferred_objects,
            report.deferred_chunk_indices,
            report.freed_bytes,
            report.grace_secs
        );
    }
    Ok(0)
}

struct StagedBundleDirGuard(PathBuf);

impl Drop for StagedBundleDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn create_private_bundle_stage_dir(
    parent: &Path,
) -> Result<(StagedBundleDirGuard, PathBuf), String> {
    let rng = SystemRandom::new();
    let pid = std::process::id();
    for _attempt in 0..16 {
        let mut nonce = [0_u8; 16];
        rng.fill(&mut nonce)
            .map_err(|_| "generate random nonce for verify-bundle staging directory".to_string())?;
        let mut hex = String::with_capacity(32);
        for byte in nonce {
            let _ = std::fmt::Write::write_fmt(&mut hex, format_args!("{byte:02x}"));
        }
        let stage_dir = parent.join(format!(".verify-bundle-{pid}-{hex}.d"));
        let mut builder = DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&stage_dir) {
            Ok(()) => {
                let staged_bundle = stage_dir.join("bundle.tmp");
                return Ok((StagedBundleDirGuard(stage_dir), staged_bundle));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(format!(
                    "create exclusive bundle staging directory {}: {err}",
                    stage_dir.display()
                ));
            }
        }
    }
    Err(format!(
        "exhausted retries creating exclusive bundle staging directory in {}",
        parent.display()
    ))
}

fn resolve_bundle_project_and_repos(
    args: &VerifyBundleArgs,
) -> Result<(String, Vec<PathBuf>), String> {
    let state = read_cutover_state(&args.cas_dir)?;
    let mut forbidden: Vec<PathBuf> = args
        .repo
        .path()
        .into_iter()
        .map(Path::to_path_buf)
        .collect();
    if let Some(s) = &state {
        for rec in s.projects.values() {
            let p = PathBuf::from(&rec.bare_repo);
            if !forbidden.contains(&p) {
                forbidden.push(p);
            }
        }
    }
    if let Some(project) = &args.project {
        if let Some(repo) = args.repo.explicit_path()
            && let Some(rec) = state.as_ref().and_then(|s| s.projects.get(project))
            && Path::new(&rec.bare_repo) != repo
        {
            return Err(format!(
                "project {project:?} is recorded for bare repo {}, not {}",
                rec.bare_repo,
                repo.display()
            ));
        }
        let proj_path = Path::new(project);
        if proj_path.join("HEAD").is_file()
            && proj_path.join("objects").is_dir()
            && !forbidden.iter().any(|p| p == proj_path)
        {
            forbidden.push(proj_path.to_path_buf());
        }
        return Ok((project.clone(), forbidden));
    }
    if let Some(repo) = args.repo.path() {
        return Ok((repo.to_string_lossy().into_owned(), forbidden));
    }
    if let Some(s) = &state
        && s.projects.len() == 1
        && let Some(rec) = s.projects.values().next()
    {
        return Ok((rec.project.clone(), forbidden));
    }
    Err(
        "missing --project <name> (or --repo <bare repo>, and no unique project recorded in cutover state)"
            .to_string(),
    )
}

fn publish_bundle_create_new(staged: &Path, dest: &Path) -> Result<(), String> {
    match fs::hard_link(staged, dest) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(format!(
            "bundle output path already exists (refusing to overwrite): {}",
            dest.display()
        )),
        Err(link_err) => {
            reject_bundle_target_if_exists(dest)?;
            let mut out_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dest)
                .map_err(|open_err| {
                    if open_err.kind() == io::ErrorKind::AlreadyExists {
                        format!(
                            "bundle output path already exists (refusing to overwrite): {}",
                            dest.display()
                        )
                    } else {
                        format!(
                            "publish bundle {} (hard_link failed: {link_err}): {open_err}",
                            dest.display()
                        )
                    }
                })?;
            let copy_res = File::open(staged)
                .and_then(|mut src| io::copy(&mut src, &mut out_file))
                .and_then(|_| out_file.sync_all());
            drop(out_file);
            if let Err(err) = copy_res {
                let _ = fs::remove_file(dest);
                return Err(format!("copy verified bundle to {}: {err}", dest.display()));
            }
            Ok(())
        }
    }
}

fn build_verify_bundle_cli_report(
    cas_dir: &Path,
    report: BundleDrillReport,
) -> VerifyBundleCliReport {
    let refs_verified: BTreeMap<String, BundleRefView> = report
        .refs_verified
        .iter()
        .map(|(git_ref, (snap, oid))| {
            (
                git_ref.clone(),
                BundleRefView {
                    snapshot: snap.to_string(),
                    git_oid: oid.to_hex(),
                },
            )
        })
        .collect();
    VerifyBundleCliReport {
        schema_version: 1,
        action: "verify_bundle",
        ok: true,
        project: report.project,
        cas_dir: cas_dir.to_string_lossy().into_owned(),
        bundle_path: report.bundle_path.to_string_lossy().into_owned(),
        bundle_bytes: report.bundle_bytes,
        snapshots_verified: report.snapshots_verified,
        refs_verified_count: refs_verified.len(),
        refs_verified,
        local_only_large_files: report
            .local_only_large_files
            .iter()
            .map(LocalOnlyLargeFileView::from)
            .collect(),
        missing_local_only_large_files: report
            .missing_local_only_large_files
            .iter()
            .map(LocalOnlyLargeFileView::from)
            .collect(),
    }
}

fn execute_verify_bundle(
    args: &VerifyBundleArgs,
    pg: &Arc<PostgresStore>,
) -> Result<VerifyBundleCliReport, String> {
    let (store, _, _) = require_bound_checked_cas_for(&args.cas_dir, pg, "verify-bundle")?;
    let (project, forbidden_repos) = resolve_bundle_project_and_repos(args)?;
    let forbidden_refs: Vec<&Path> = forbidden_repos.iter().map(PathBuf::as_path).collect();
    let validated_target =
        validate_bundle_output_path(&args.bundle_path, &args.cas_dir, &forbidden_refs)?;
    let parent = validated_target.parent().ok_or_else(|| {
        format!(
            "bundle output path has no parent directory: {}",
            validated_target.display()
        )
    })?;
    let file_name = validated_target.file_name().ok_or_else(|| {
        format!(
            "bundle output path has no file name: {}",
            validated_target.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("create bundle directory {}: {e}", parent.display()))?;
    ensure_existing_dir_not_symlink(parent, "bundle parent")?;
    let canon_parent = fs::canonicalize(parent)
        .map_err(|e| format!("canonicalize bundle parent {}: {e}", parent.display()))?;
    let final_bundle = canon_parent.join(file_name);
    let _ = validate_bundle_output_path(&final_bundle, &args.cas_dir, &forbidden_refs)?;

    let (_staged_guard, staged_bundle) = create_private_bundle_stage_dir(&canon_parent)?;

    let ledger = PostgresRefLedger::new(Arc::clone(pg));
    let adapter = GitBorderAdapter::new();
    let mut report =
        verify_bundle_snapshot_roundtrip(&store, &ledger, &adapter, &project, &staged_bundle)
            .map_err(|e| format!("verify-bundle roundtrip failed: {e}"))?;
    let staged_meta = fs::symlink_metadata(&staged_bundle)
        .map_err(|e| format!("stat staged bundle {}: {e}", staged_bundle.display()))?;
    if !staged_meta.is_file() || staged_meta.file_type().is_symlink() {
        return Err(format!(
            "staged bundle path is not a regular file: {}",
            staged_bundle.display()
        ));
    }
    publish_bundle_create_new(&staged_bundle, &final_bundle)?;
    report.bundle_path = final_bundle;
    Ok(build_verify_bundle_cli_report(&args.cas_dir, report))
}

fn run_verify_bundle(args: &VerifyBundleArgs) -> Result<i32, String> {
    let pg = Arc::new(open_store()?);
    let report = execute_verify_bundle(args, &pg)?;
    if args.json {
        print_json(&report)?;
    } else {
        println!(
            "spawn storage verify-bundle: PASS project={} bundle={} bytes={} snapshots_verified={} refs_verified={} local_only_large_files={} missing_local_only_large_files={}",
            report.project,
            report.bundle_path,
            report.bundle_bytes,
            report.snapshots_verified,
            report.refs_verified_count,
            report.local_only_large_files.len(),
            report.missing_local_only_large_files.len()
        );
    }
    Ok(0)
}

fn update_backend_marker_after_rollback(
    cas_dir: &Path,
    root: &Path,
    rolled_back_project: &str,
) -> Result<(WorkspaceBackend, Vec<String>), String> {
    let updated_state = read_cutover_state(cas_dir)?.unwrap_or_default();
    let other_d6_projects: Vec<String> = updated_state
        .projects
        .values()
        .filter(|rec| {
            rec.project != rolled_back_project && rec.storage_mode() == ProjectStorageMode::D6Cas
        })
        .map(|rec| rec.project.clone())
        .collect();

    let active_backend = if other_d6_projects.is_empty() {
        write_workspace_backend_marker(root, WorkspaceBackend::Git)?;
        WorkspaceBackend::Git
    } else {
        read_workspace_backend_safe(root)?.unwrap_or(WorkspaceBackend::D6)
    };
    Ok((active_backend, other_d6_projects))
}

fn run_rollback_to_git(args: &ParsedArgs) -> Result<i32, String> {
    if !cas_is_initialized(&args.cas_dir)? {
        return Err(format!(
            "CAS store is not initialized in {}; nothing to roll back",
            args.cas_dir.display()
        ));
    }

    let state = read_cutover_state(&args.cas_dir)?;
    let Some((project, bare_repo, record_opt)) = resolve_target(args, state.as_ref(), true)? else {
        unreachable!("require_target == true always returns Some or Err")
    };
    ensure_existing_dir_not_symlink(&bare_repo, "bare repo")?;

    let record = record_opt.ok_or_else(|| {
        format!(
            "no cutover state recorded for project {project:?} in {}",
            args.cas_dir.join(CUTOVER_STATE_FILE).display()
        )
    })?;
    if record.storage_mode() != ProjectStorageMode::D6Cas {
        return Err(format!(
            "project {project:?} is not in d6_cas mode (phase: {:?}); cannot roll back to git",
            record.phase
        ));
    }

    let pg = Arc::new(open_store()?);
    let ledger = PostgresRefLedger::new(Arc::clone(&pg));
    let (store, _, _) = require_bound_checked_cas_for(&args.cas_dir, &pg, "rollback")?;

    let mut cutover = ProjectCutover::from_state(&project, &bare_repo, &args.root, record.phase);
    let border = GitBorderAdapter::new();
    let rb = cutover
        .rollback_to_git(&store, &ledger, &border)
        .map_err(|e| format!("rollback to git failed: {e}"))?;

    let reason = args.reason.as_deref().unwrap_or("cli rollback to git");
    save_cutover_record(&args.cas_dir, &cutover, &bare_repo, Some(reason))?;
    let (active_backend, other_d6_projects) =
        update_backend_marker_after_rollback(&args.cas_dir, &args.root, &project)?;
    let readonly_border = is_readonly_border(&bare_repo)
        .map_err(|e| format!("inspect bare repo {}: {e}", bare_repo.display()))?;

    let report = RollbackResultReport {
        schema_version: 1,
        action: "rollback",
        target: "git",
        project,
        bare_repo: bare_repo.to_string_lossy().into_owned(),
        workspace_root: args.root.to_string_lossy().into_owned(),
        workspace_backend: active_backend.name().to_string(),
        cas_dir: args.cas_dir.to_string_lossy().into_owned(),
        phase: cutover.phase(),
        storage_mode: cutover.storage_mode(),
        dispatch_paused: cutover.dispatch_paused(),
        readonly_border,
        exported_refs: rb.exported_refs,
        workspace_backend_kept_d6_for_projects: other_d6_projects,
    };

    if args.json {
        print_json(&report)?;
    } else {
        println!(
            "spawn storage cutover: rolled back project={} mode=git phase={:?} exported_refs={} readonly_border={} workspace_backend={}",
            report.project,
            report.phase,
            report.exported_refs,
            report.readonly_border,
            report.workspace_backend
        );
    }
    Ok(0)
}

fn print_json<T: Serialize>(value: &T) -> Result<(), String> {
    let encoded =
        serde_json::to_string_pretty(value).map_err(|e| format!("encode JSON report: {e}"))?;
    println!("{encoded}");
    Ok(())
}

fn print_status_human(report: &StatusReport) {
    println!("workspace_root: {}", report.workspace_root);
    println!(
        "workspace_backend: {}",
        report.workspace_backend.as_deref().unwrap_or("unset")
    );
    println!("pending_merges_clean: {}", report.pending_merges_clean);
    println!("cas_dir: {}", report.cas_dir);
    println!("cas_initialized: {}", report.cas_initialized);
    println!(
        "pg_database: system={} oid={} schema={} latest_ref_seq={}",
        report.pg_database.db_system_id,
        report.pg_database.db_oid,
        report.pg_database.schema_name,
        report.pg_database.latest_ref_seq
    );
    println!(
        "gc_fence: bound={} status={}{}",
        report.gc_fence.bound,
        report.gc_fence.status,
        report
            .gc_fence
            .detail
            .as_deref()
            .map_or_else(String::new, |d| format!(" ({d})"))
    );
    if let Some(project) = &report.project {
        println!("project: {project}");
    }
    if let Some(bare_repo) = &report.bare_repo {
        println!("bare_repo: {bare_repo}");
    }
    if let Some(readonly) = report.readonly_border {
        println!("readonly_border: {readonly}");
    }
    if let Some(count) = report.pg_ref_count {
        println!("pg_ref_count: {count}");
    }
    if let Some(rec) = &report.cutover_state {
        println!(
            "cutover_state: phase={:?} storage_mode={:?} dispatch_paused={}",
            rec.phase, rec.storage_mode, rec.dispatch_paused
        );
    }
}

fn print_verify_human(report: &VerifyReport) {
    println!("verify: {}", if report.ok { "PASS" } else { "FAIL" });
    println!("project: {}", report.project);
    println!("bare_repo: {}", report.bare_repo);
    println!("workspace_root: {}", report.workspace_root);
    println!(
        "checks: pending_merges_clean={} cas_initialized={} gc_fence={} ref_parity={} readonly_border={} workspace_backend_marker={}",
        report.checks.pending_merges_clean,
        report.checks.cas_initialized,
        report.checks.gc_fence,
        report.checks.ref_parity,
        report.checks.readonly_border,
        report.checks.workspace_backend_marker
    );
    if let Some(err) = &report.parity_error {
        println!("parity_error: {err}");
    }
    if let Some(detail) = &report.gc_fence.detail {
        println!("gc_fence_detail: {detail}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use spawn_host::RefLedger as _;

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(prefix: &str) -> Self {
            let seq = NEXT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "spawn-cli-{prefix}-{}-{stamp}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("mkdir scratch");
            Self(dir)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn parse_rejects_missing_or_conflicting_modes_and_unknown_values() {
        let scratch = ScratchDir::new("parse-flags");
        let root_str = scratch.0.to_string_lossy().into_owned();

        let missing = parse_cutover_args(&s(&["--root", &root_str])).expect_err("missing mode");
        assert!(missing.contains("specify one of"));

        let conflict = parse_cutover_args(&s(&["--root", &root_str, "--status", "--verify"]))
            .expect_err("conflicting mode");
        assert!(conflict.contains("mutually exclusive"));

        let bad_to = parse_cutover_args(&s(&["--root", &root_str, "--to", "cow"]))
            .expect_err("invalid --to target");
        assert!(bad_to.contains("only d6 is supported"));

        let bad_rb = parse_cutover_args(&s(&["--root", &root_str, "--rollback", "d6"]))
            .expect_err("invalid --rollback target");
        assert!(bad_rb.contains("only git is supported"));

        let unknown_switch = parse_cutover_args(&s(&["--root", &root_str, "--status", "--bogus"]))
            .expect_err("unknown switch");
        assert!(unknown_switch.contains("--bogus"));

        let unknown_val =
            parse_cutover_args(&s(&["--root", &root_str, "--status", "--bogus", "val"]))
                .expect_err("unknown flag with value");
        assert!(unknown_val.contains("unknown flag --bogus"));

        let empty_reason =
            parse_cutover_args(&s(&["--root", &root_str, "--to", "d6", "--reason", "   "]))
                .expect_err("empty reason");
        assert!(empty_reason.contains("--reason"));
    }

    #[test]
    fn parse_accepts_all_modes_and_defaults_cas_dir_and_project() {
        let scratch = ScratchDir::new("parse-ok");
        let root = scratch.0.join("ws");
        let bare = scratch.0.join("origin.git");
        fs::create_dir_all(&root).expect("mkdir root");
        fs::create_dir_all(&bare).expect("mkdir bare");

        let root_str = root.to_string_lossy().into_owned();
        let bare_str = bare.to_string_lossy().into_owned();
        let parsed = parse_cutover_args(&s(&[
            "--to", "d6", "--root", &root_str, "--repo", &bare_str, "--json",
        ]))
        .expect("parse --to d6");

        assert_eq!(parsed.action, CutoverAction::ToD6);
        assert!(parsed.json);
        assert_eq!(parsed.cas_dir, root.join(".d6/cas"));
        assert!(parsed.project.is_none());

        let resolved = resolve_target(&parsed, None, true)
            .expect("resolve")
            .expect("some");
        assert_eq!(
            resolved.0,
            fs::canonicalize(&bare).unwrap().to_string_lossy()
        );
    }

    #[test]
    fn atomic_state_and_marker_roundtrip_refuse_symlinks() {
        let scratch = ScratchDir::new("state-io");
        let root = scratch.0.join("ws");
        let cas = root.join(".d6/cas");
        fs::create_dir_all(&cas).expect("mkdir cas");

        let bare = scratch.0.join("proj.git");
        let cutover = ProjectCutover::from_state("proj", &bare, &root, CutoverPhase::Resumed);
        save_cutover_record(&cas, &cutover, &bare, Some("test cutover")).expect("save state");
        let persisted: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(cas.join(CUTOVER_STATE_FILE)).expect("read state raw"),
        )
        .expect("parse state raw");
        assert_eq!(persisted["schema_version"], CUTOVER_STATE_SCHEMA_VERSION);
        let raw_rec = &persisted["projects"]["proj"];
        assert_eq!(raw_rec["phase"], "resumed");
        assert!(raw_rec.get("storage_mode").is_none());
        assert!(raw_rec.get("dispatch_paused").is_none());

        let loaded = read_cutover_state(&cas)
            .expect("read state")
            .expect("state exists");
        let rec = loaded.projects.get("proj").expect("proj record");
        assert_eq!(rec.phase, CutoverPhase::Resumed);
        assert_eq!(rec.storage_mode(), ProjectStorageMode::D6Cas);
        assert!(!rec.dispatch_paused());
        assert_eq!(rec.reason.as_deref(), Some("test cutover"));

        write_workspace_backend_marker(&root, WorkspaceBackend::D6).expect("write d6 marker");
        assert_eq!(
            read_workspace_backend_safe(&root).expect("read d6 marker"),
            Some(WorkspaceBackend::D6)
        );
        write_workspace_backend_marker(&root, WorkspaceBackend::Git).expect("write git marker");
        assert_eq!(
            read_workspace_backend_safe(&root).expect("read git marker"),
            Some(WorkspaceBackend::Git)
        );

        #[cfg(unix)]
        {
            let decoy = scratch.0.join("decoy");
            fs::write(&decoy, "evil\n").expect("write decoy");
            let state_path = cas.join(CUTOVER_STATE_FILE);
            fs::remove_file(&state_path).expect("rm state");
            symlink(&decoy, &state_path).expect("symlink state");
            let err = read_cutover_state(&cas).expect_err("refuse symlink state");
            assert!(err.contains("symlink"));
        }
    }

    #[test]
    fn legacy_v1_cutover_state_upgrades_and_rejects_contradictions() {
        let scratch = ScratchDir::new("v1-upgrade");
        let cas = scratch.0.join(".d6/cas");
        fs::create_dir_all(&cas).expect("mkdir cas");
        let state_path = cas.join(CUTOVER_STATE_FILE);

        let write_v1 = |phase: &str, mode: &str, paused: bool| {
            let payload = serde_json::json!({
                "schema_version": 1,
                "projects": {
                    "proj": {
                        "project": "proj",
                        "bare_repo": "/tmp/proj.git",
                        "phase": phase,
                        "storage_mode": mode,
                        "dispatch_paused": paused,
                        "reason": "v1",
                        "updated_at_ms": 42
                    }
                }
            });
            fs::write(&state_path, serde_json::to_vec_pretty(&payload).unwrap()).unwrap();
        };

        write_v1("resumed", "d6_cas", false);
        let resumed = read_cutover_state(&cas).expect("read v1 resumed").unwrap();
        assert_eq!(resumed.schema_version, CUTOVER_STATE_SCHEMA_VERSION);
        assert_eq!(resumed.projects["proj"].phase, CutoverPhase::Resumed);

        write_v1("backfill_imported", "git", true);
        let paused = read_cutover_state(&cas).expect("read v1 paused").unwrap();
        assert_eq!(paused.projects["proj"].phase, CutoverPhase::DispatchPaused);
        assert_eq!(
            paused.projects["proj"].storage_mode(),
            ProjectStorageMode::Git
        );
        assert!(paused.projects["proj"].dispatch_paused());

        for (phase, mode, paused) in [
            ("resumed", "git", true),
            ("resumed", "git", false),
            ("resumed", "d6_cas", true),
            ("not_started", "d6_cas", false),
        ] {
            write_v1(phase, mode, paused);
            let err = read_cutover_state(&cas).expect_err("reject contradictory v1 state");
            assert!(
                err.contains("contradictory legacy v1 cutover state"),
                "{err}"
            );
        }
    }

    #[test]
    fn cutover_state_rejects_duplicate_json_keys_and_v2_only_phase_in_v1() {
        let scratch = ScratchDir::new("dup-keys");
        let cas = scratch.0.join(".d6/cas");
        fs::create_dir_all(&cas).expect("mkdir cas");
        let state_path = cas.join(CUTOVER_STATE_FILE);

        let dup_cases = [
            (
                "v2 duplicate phase resumed->rolled_back",
                r#"{"schema_version":2,"projects":{"proj":{"project":"proj","bare_repo":"/tmp/proj.git","phase":"resumed","phase":"rolled_back","reason":null,"updated_at_ms":1}}}"#,
            ),
            (
                "v2 duplicate phase rolled_back->resumed",
                r#"{"schema_version":2,"projects":{"proj":{"project":"proj","bare_repo":"/tmp/proj.git","phase":"rolled_back","phase":"resumed","reason":null,"updated_at_ms":1}}}"#,
            ),
            (
                "v1 duplicate dispatch_paused",
                r#"{"schema_version":1,"projects":{"proj":{"project":"proj","bare_repo":"/tmp/proj.git","phase":"backfill_imported","storage_mode":"git","dispatch_paused":true,"dispatch_paused":false,"reason":null,"updated_at_ms":1}}}"#,
            ),
            (
                "v1 duplicate storage_mode",
                r#"{"schema_version":1,"projects":{"proj":{"project":"proj","bare_repo":"/tmp/proj.git","phase":"resumed","storage_mode":"git","storage_mode":"d6_cas","dispatch_paused":false,"reason":null,"updated_at_ms":1}}}"#,
            ),
            (
                "duplicate schema_version",
                r#"{"schema_version":1,"schema_version":2,"projects":{}}"#,
            ),
            (
                "duplicate project map key",
                r#"{"schema_version":2,"projects":{"proj":{"project":"proj","bare_repo":"/tmp/proj.git","phase":"resumed","reason":null,"updated_at_ms":1},"proj":{"project":"proj","bare_repo":"/tmp/proj.git","phase":"rolled_back","reason":null,"updated_at_ms":2}}}"#,
            ),
        ];

        for (label, raw) in dup_cases {
            fs::write(&state_path, raw.as_bytes()).expect("write duplicate-key fixture");
            let msg = read_cutover_state(&cas).expect_err(label);
            assert!(msg.contains("duplicate"), "{label}: {msg}");
        }

        let v1_dispatch_paused = r#"{"schema_version":1,"projects":{"proj":{"project":"proj","bare_repo":"/tmp/proj.git","phase":"dispatch_paused","storage_mode":"git","dispatch_paused":true,"reason":null,"updated_at_ms":1}}}"#;
        fs::write(&state_path, v1_dispatch_paused.as_bytes()).expect("write v1 dispatch_paused");
        let err = read_cutover_state(&cas).expect_err("reject v2-only dispatch_paused in v1");
        assert!(err.contains("schema v2 only"), "{err}");
    }

    #[test]
    fn pending_merge_audit_helper_detects_dirty_merge_marker_and_clears() {
        let scratch = ScratchDir::new("pending-merge");
        let root = scratch.0.join("ws");
        fs::create_dir_all(root.join(".merge")).expect("mkdir .merge");
        assert!(audit_pending_merges_clean(&root).expect("clean empty .merge"));

        fs::write(root.join(".merge/dirty"), b"1").expect("seed dirty marker");
        assert!(!audit_pending_merges_clean(&root).expect("dirty .merge"));

        fs::remove_dir_all(root.join(".merge")).expect("rm .merge");
        assert!(audit_pending_merges_clean(&root).expect("clean after rm"));
    }

    #[test]
    fn subcommand_dispatch_rejects_missing_or_unknown_subcommand() {
        let empty = command(&[]).expect_err("missing subcommand");
        assert!(empty.contains("spawn storage cutover"));
        assert!(empty.contains("spawn storage reconcile"));
        assert!(empty.contains("spawn storage gc"));
        assert!(empty.contains("spawn storage verify-bundle"));
        let bogus = command(&s(&["compact"])).expect_err("unknown subcommand");
        assert!(bogus.contains("unknown storage subcommand"));
    }

    #[test]
    fn parse_reconcile_accepts_valid_and_rejects_invalid_inputs() {
        let scratch = ScratchDir::new("parse-reconcile");
        let root = scratch.0.join("ws");
        let cas = scratch.0.join("cas");
        fs::create_dir_all(&root).expect("mkdir root");
        fs::create_dir_all(&cas).expect("mkdir cas");
        let root_s = root.to_string_lossy().into_owned();
        let cas_s = cas.to_string_lossy().into_owned();

        let rec = parse_storage_command(&s(&["reconcile", "--cas-dir", &cas_s, "--json"]))
            .expect("parse reconcile");
        assert!(matches!(
            rec,
            StorageCommand::Reconcile(ReconcileArgs {
                reason_prefix,
                json: true,
                ..
            }) if reason_prefix == "cli:reconcile"
        ));

        let rec_custom = parse_storage_command(&s(&[
            "reconcile",
            "--root",
            &root_s,
            "--reason",
            "ops:restore",
        ]))
        .expect("parse reconcile with root & reason");
        assert!(matches!(
            rec_custom,
            StorageCommand::Reconcile(ReconcileArgs {
                reason_prefix,
                cas_dir,
                json: false,
            }) if reason_prefix == "ops:restore"
                && cas_dir == fs::canonicalize(&root).unwrap().join(".d6/cas")
        ));

        for (args, needle) in [
            (
                &["reconcile", "--cas-dir", &cas_s, "--reason", "  "][..],
                "--reason",
            ),
            (
                &["reconcile", "--cas-dir", &cas_s, "stray"],
                "unexpected argument",
            ),
            (
                &["reconcile", "--cas-dir", &cas_s, "--bogus", "1"],
                "unknown flag",
            ),
        ] {
            let err = parse_storage_command(&s(args)).expect_err("reject invalid reconcile args");
            assert!(err.contains(needle), "{args:?}: {err}");
        }

        #[cfg(unix)]
        {
            let link_cas = scratch.0.join("cas.link");
            symlink(&cas, &link_cas).expect("symlink cas");
            let cas_err =
                parse_storage_command(&s(&["reconcile", "--cas-dir", &link_cas.to_string_lossy()]))
                    .expect_err("symlink cas");
            assert!(cas_err.contains("symlink"), "{cas_err}");
        }
    }

    #[test]
    fn parse_gc_accepts_valid_and_rejects_invalid_inputs() {
        let scratch = ScratchDir::new("parse-gc");
        let cas = scratch.0.join("cas");
        fs::create_dir_all(&cas).expect("mkdir cas");
        let cas_s = cas.to_string_lossy().into_owned();

        let gc_default =
            parse_storage_command(&s(&["gc", "--cas-dir", &cas_s])).expect("parse gc default");
        assert!(matches!(
            gc_default,
            StorageCommand::Gc(GcArgs { grace, json: false, .. }) if grace == DEFAULT_PERSISTENT_GC_GRACE
        ));

        let gc_one = parse_storage_command(&s(&[
            "gc",
            "--cas-dir",
            &cas_s,
            "--grace-secs",
            "1",
            "--json",
        ]))
        .expect("parse gc grace 1");
        assert!(matches!(
            gc_one,
            StorageCommand::Gc(GcArgs { grace, json: true, .. }) if grace == Duration::from_secs(1)
        ));

        for bad_grace in ["0", "-1", "abc", "   "] {
            let err =
                parse_storage_command(&s(&["gc", "--cas-dir", &cas_s, "--grace-secs", bad_grace]))
                    .expect_err("bad grace-secs");
            assert!(err.contains("--grace-secs"), "{bad_grace}: {err}");
        }
    }

    #[test]
    fn parse_verify_bundle_accepts_valid_and_rejects_invalid_inputs() {
        let scratch = ScratchDir::new("parse-verify-bundle");
        let cas = scratch.0.join("cas");
        fs::create_dir_all(&cas).expect("mkdir cas");
        let cas_s = cas.to_string_lossy().into_owned();
        let bundle_file = scratch.0.join("out.bundle");
        let bundle_s = bundle_file.to_string_lossy().into_owned();

        let vb = parse_storage_command(&s(&[
            "verify-bundle",
            "--cas-dir",
            &cas_s,
            "--project",
            "demo",
            "--bundle",
            &bundle_s,
            "--json",
        ]))
        .expect("parse verify-bundle");
        assert!(matches!(
            vb,
            StorageCommand::VerifyBundle(VerifyBundleArgs {
                project: Some(p),
                repo: BundleRepoArg::None,
                json: true,
                ..
            }) if p == "demo"
        ));

        assert!(
            parse_storage_command(&s(&["verify-bundle", "--cas-dir", &cas_s]))
                .expect_err("missing --bundle")
                .contains("--bundle")
        );
        assert!(
            parse_storage_command(&s(&[
                "verify-bundle",
                "--cas-dir",
                &cas_s,
                "--bundle",
                &cas_s
            ]))
            .expect_err("directory --bundle")
            .contains("directory")
        );

        let existing = scratch.0.join("existing.bundle");
        fs::write(&existing, b"existing backup bytes\n").expect("write existing");
        let existing_err = parse_storage_command(&s(&[
            "verify-bundle",
            "--cas-dir",
            &cas_s,
            "--bundle",
            &existing.to_string_lossy(),
        ]))
        .expect_err("existing regular file rejected");
        assert!(existing_err.contains("already exists"), "{existing_err}");
        assert_eq!(
            fs::read(&existing).expect("read existing"),
            b"existing backup bytes\n"
        );

        let cas_inner = cas.join("inside-cas.bundle");
        let cas_inner_err = parse_storage_command(&s(&[
            "verify-bundle",
            "--cas-dir",
            &cas_s,
            "--bundle",
            &cas_inner.to_string_lossy(),
        ]))
        .expect_err("inside cas-dir rejected");
        assert!(cas_inner_err.contains("inside CAS"), "{cas_inner_err}");

        let bare_repo = scratch.0.join("repo.git");
        fs::create_dir_all(&bare_repo).expect("mkdir bare");
        let repo_inner = bare_repo.join("inside-repo.bundle");
        let repo_inner_err = parse_storage_command(&s(&[
            "verify-bundle",
            "--cas-dir",
            &cas_s,
            "--repo",
            &bare_repo.to_string_lossy(),
            "--bundle",
            &repo_inner.to_string_lossy(),
        ]))
        .expect_err("inside bare repo rejected");
        assert!(
            repo_inner_err.contains("inside bare repo"),
            "{repo_inner_err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn parse_verify_bundle_rejects_symlink_ancestors_and_traversal() {
        let scratch = ScratchDir::new("parse-verify-bundle-sym");
        let cas = scratch.0.join("cas");
        fs::create_dir_all(&cas).expect("mkdir cas");
        let cas_s = cas.to_string_lossy().into_owned();
        let bundle_file = scratch.0.join("out.bundle");

        let link_bundle = scratch.0.join("bundle.link");
        symlink(&bundle_file, &link_bundle).expect("symlink bundle");
        let err = parse_storage_command(&s(&[
            "verify-bundle",
            "--cas-dir",
            &cas_s,
            "--bundle",
            &link_bundle.to_string_lossy(),
        ]))
        .expect_err("symlink bundle");
        assert!(err.contains("symlink"), "{err}");

        let gp_real = scratch.0.join("real_gp/sub");
        fs::create_dir_all(&gp_real).expect("mkdir real gp");
        let gp_link = scratch.0.join("gp.link");
        symlink(scratch.0.join("real_gp"), &gp_link).expect("symlink grandparent");
        let gp_target = gp_link.join("sub/out.bundle");
        let gp_err = parse_storage_command(&s(&[
            "verify-bundle",
            "--cas-dir",
            &cas_s,
            "--bundle",
            &gp_target.to_string_lossy(),
        ]))
        .expect_err("symlink grandparent");
        assert!(gp_err.contains("symlink"), "{gp_err}");

        let traverse_target = gp_link.join("../escaped.bundle");
        let trav_err = parse_storage_command(&s(&[
            "verify-bundle",
            "--cas-dir",
            &cas_s,
            "--bundle",
            &traverse_target.to_string_lossy(),
        ]))
        .expect_err("link/../x.bundle rejected");
        assert!(trav_err.contains("symlink"), "{trav_err}");
        assert!(!scratch.0.join("real_gp").join("escaped.bundle").exists());
    }

    fn copy_cas_excluding_fence_and_journal(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).expect("mkdir dst cas");
        for entry in fs::read_dir(src).expect("read src cas") {
            let entry = entry.expect("entry");
            let name = entry.file_name();
            if name == ".gc-fence" || name == "refs.journal" || name == ".lock" {
                continue;
            }
            let from = entry.path();
            let to = dst.join(&name);
            let ft = entry.file_type().expect("ft");
            if ft.is_dir() {
                copy_cas_excluding_fence_and_journal(&from, &to);
            } else if ft.is_file() {
                fs::copy(&from, &to).expect("copy cas entry");
            }
        }
    }

    struct SeededMain {
        cas: CasMerkleStore,
        root_digest: spawn_cas::Digest,
        root_snap: spawn_cas::Snapshot,
        root_git_oid: spawn_cas::GitOid,
    }

    fn publish_branch_file(
        cas: &CasMerkleStore,
        ledger: &PostgresRefLedger,
        project: &str,
        base: (&spawn_cas::Digest, &spawn_cas::Digest),
        target: (&str, &[u8], &[u8], i64),
    ) {
        let (base_tree, parent) = base;
        let (ref_name, file_name, content, created_at_ms) = target;
        let (blob_d, blob_sz) = cas.put_bytes_blob(content).expect("put branch blob");
        let tree = cas
            .apply_mutations(
                base_tree,
                &[spawn_cas::TreeMutation::Upsert {
                    path: vec![file_name.to_vec()],
                    kind: spawn_cas::TreeEntryKind::File,
                    id: blob_d.as_bytes(),
                    size: blob_sz,
                }],
            )
            .expect("apply branch mutation");
        let body = spawn_host::format_commit_body(
            "Spawn <spawn@localhost> 1700000200 +0000",
            "Spawn <spawn@localhost> 1700000200 +0000",
            &format!("{ref_name}\n"),
        );
        cas.publish_snapshot(
            ledger,
            &spawn_host::PublishRequest {
                project,
                ref_name,
                kind: spawn_host::D6RefKind::Branch,
                expected_ref: None,
                tree: &tree,
                parents: std::slice::from_ref(parent),
                commit_body: &body,
                created_at_ms,
                reason: "seed branch",
                landing_task_id: None,
            },
        )
        .expect("publish branch");
    }

    fn seed_d6_project_in_schema(
        cas_dir: &Path,
        pg: &Arc<PostgresStore>,
        project: &str,
    ) -> SeededMain {
        let cas = CasMerkleStore::open(cas_dir).expect("open cas");
        let (binding, _) = live_gc_binding(pg).expect("live binding");
        bind_store_gc_fence(&cas, &binding).expect("bind gc fence");
        let ledger = PostgresRefLedger::new(Arc::clone(pg));

        let (readme_d, readme_sz) = cas.put_bytes_blob(b"hello d6\n").expect("readme blob");
        let main_tree = cas
            .put_tree_cached(
                &spawn_cas::Tree::from_entries(vec![spawn_cas::TreeEntry::file(
                    b"README.md".to_vec(),
                    &readme_d,
                    readme_sz,
                )])
                .expect("tree"),
            )
            .expect("put main tree");
        let main_body = spawn_host::format_commit_body(
            "Spawn <spawn@localhost> 1700000100 +0000",
            "Spawn <spawn@localhost> 1700000100 +0000",
            "init main\n",
        );
        let root_snap = spawn_cas::Snapshot {
            tree: main_tree.clone(),
            parents: Vec::new(),
            commit_body: main_body.clone(),
        };
        let pub_main = cas
            .publish_snapshot(
                &ledger,
                &spawn_host::PublishRequest {
                    project,
                    ref_name: "main",
                    kind: spawn_host::D6RefKind::Main,
                    expected_ref: None,
                    tree: &main_tree,
                    parents: &[],
                    commit_body: &main_body,
                    created_at_ms: 1_700_000_100_000,
                    reason: "seed main",
                    landing_task_id: None,
                },
            )
            .expect("publish main");

        publish_branch_file(
            &cas,
            &ledger,
            project,
            (&main_tree, &pub_main.snapshot),
            (
                "feat/alpha",
                b"feat_a.txt",
                b"feature a\n",
                1_700_000_200_000,
            ),
        );
        publish_branch_file(
            &cas,
            &ledger,
            project,
            (&main_tree, &pub_main.snapshot),
            (
                "feat/beta",
                b"feat_b.txt",
                b"feature b\n",
                1_700_000_300_000,
            ),
        );

        SeededMain {
            cas,
            root_digest: pub_main.snapshot,
            root_snap,
            root_git_oid: pub_main.git_oid,
        }
    }

    fn seed_restored_cas_at_main_seq1(
        live_cas_dir: &Path,
        restored_cas_dir: &Path,
        live_cas: &CasMerkleStore,
        seeded: &SeededMain,
        project: &str,
        restored_pg: &Arc<PostgresStore>,
    ) -> GcFenceBinding {
        copy_cas_excluding_fence_and_journal(live_cas_dir, restored_cas_dir);
        let mut journal =
            fs::read(live_cas.cas().refs_journal_path()).expect("read live refs.journal");
        journal.extend_from_slice(b"{\"seq\":999,\"torn\":");
        fs::write(restored_cas_dir.join("refs.journal"), journal).expect("write torn journal");

        let restored_cas = CasMerkleStore::open(restored_cas_dir).expect("open restored cas");
        let (restored_binding, _) = live_gc_binding(restored_pg).expect("restored binding");
        bind_store_gc_fence(&restored_cas, &restored_binding).expect("bind restored fence");

        let restored_ledger = PostgresRefLedger::new(Arc::clone(restored_pg));
        restored_ledger
            .record_snapshot(
                &seeded.root_digest,
                &seeded.root_git_oid,
                &seeded.root_snap.tree,
                &seeded.root_snap.parents,
                project,
                1_700_000_100_000,
            )
            .expect("record backup main");
        let seed_seq = restored_ledger
            .land_main_ref(
                project,
                "main",
                None,
                &seeded.root_digest,
                None,
                "backup init",
            )
            .expect("land backup main");
        assert_eq!(seed_seq, 1);
        restored_binding
    }

    #[test]
    fn pg_storage_reconcile_replays_lagging_pg_and_rejects_fence_mismatch_and_uninitialized() {
        let scratch = ScratchDir::new("cli-reconcile-pg");
        let uninit_cas = scratch.0.join("uninit");
        let live_cas_dir = scratch.0.join("cas-live");
        let live_schema = spawn_postgres::testing::TestSchema::new("cli_reconcile_live");
        let live_pg = Arc::new(live_schema.store());

        let uninit_err = execute_reconcile(
            &ReconcileArgs {
                cas_dir: uninit_cas.clone(),
                reason_prefix: "cli:reconcile".to_string(),
                json: true,
            },
            &live_pg,
        )
        .expect_err("uninitialized CAS rejected");
        assert!(uninit_err.contains("not initialized"), "{uninit_err}");
        assert!(!uninit_cas.exists());

        let unbound_dir = scratch.0.join("cas-unbound");
        let _unbound = CasMerkleStore::open(&unbound_dir).expect("open unbound");
        let unbound_err = execute_reconcile(
            &ReconcileArgs {
                cas_dir: unbound_dir,
                reason_prefix: "cli:reconcile".to_string(),
                json: false,
            },
            &live_pg,
        )
        .expect_err("unbound CAS rejected");
        assert!(unbound_err.contains("no .gc-fence"), "{unbound_err}");

        let seeded = seed_d6_project_in_schema(&live_cas_dir, &live_pg, "proj-reconcile");
        assert_eq!(live_pg.latest_ref_log_seq().expect("live seq"), 3);

        let wrong_schema = spawn_postgres::testing::TestSchema::new("cli_reconcile_wrong");
        let wrong_pg = Arc::new(wrong_schema.store());
        let mismatch_err = execute_reconcile(
            &ReconcileArgs {
                cas_dir: live_cas_dir.clone(),
                reason_prefix: "cli:reconcile".to_string(),
                json: false,
            },
            &wrong_pg,
        )
        .expect_err("wrong PG fails closed");
        assert!(
            mismatch_err.contains("GC fence check failed"),
            "{mismatch_err}"
        );

        let restored_cas_dir = scratch.0.join("cas-restored");
        let restored_schema = spawn_postgres::testing::TestSchema::new("cli_reconcile_restored");
        let restored_pg = Arc::new(restored_schema.store());
        let restored_binding = seed_restored_cas_at_main_seq1(
            &live_cas_dir,
            &restored_cas_dir,
            &seeded.cas,
            &seeded,
            "proj-reconcile",
            &restored_pg,
        );

        let rec_args = ReconcileArgs {
            cas_dir: restored_cas_dir.clone(),
            reason_prefix: "cli:drill".to_string(),
            json: true,
        };
        let report1 = execute_reconcile(&rec_args, &restored_pg).expect("reconcile suffix");
        assert_eq!(report1.action, "reconcile");
        assert_eq!(report1.entries_considered, 2);
        assert_eq!(report1.entries_replayed, 2);
        assert_eq!(report1.pg_latest_ref_seq_before, 1);
        assert_eq!(report1.pg_latest_ref_seq_after, 3);
        assert!(report1.gc_fence_aligned);

        let encoded = serde_json::to_value(&report1).expect("serialize reconcile JSON");
        assert_eq!(encoded["action"], "reconcile");
        assert_eq!(encoded["gc_fence_aligned"], true);

        let restored_cas = CasMerkleStore::open(&restored_cas_dir).expect("reopen restored cas");
        assert_eq!(
            restored_pg
                .list_refs(Some("proj-reconcile"))
                .expect("list restored refs")
                .len(),
            3
        );
        check_gc_fence(&restored_cas, &restored_binding, 3)
            .expect("gc fence aligned after reconcile");

        let report2 = execute_reconcile(&rec_args, &restored_pg).expect("idempotent second run");
        assert_eq!(report2.entries_considered, 0);
        assert_eq!(report2.entries_replayed, 0);
        assert!(report2.gc_fence_aligned);
        assert_eq!(reconcile_exit_code_and_error(&report2), (0, None));
    }

    #[test]
    fn pg_storage_reconcile_fails_closed_when_pg_still_trails_journal() {
        let scratch = ScratchDir::new("cli-reconcile-lag-pg");
        let cas_dir = scratch.0.join("cas");
        let schema = spawn_postgres::testing::TestSchema::new("cli_reconcile_still_lagging");
        let pg = Arc::new(schema.store());
        let seeded = seed_d6_project_in_schema(&cas_dir, &pg, "proj-rec-lag");

        let restored_cas_dir = scratch.0.join("cas-restored");
        copy_cas_excluding_fence_and_journal(&cas_dir, &restored_cas_dir);
        // Append an already-applied main transition at a higher journal seq (= 99) so
        // `reconcile_refs_journal` marks it `already_applied` without advancing PG's seq past 99.
        let orig_journal =
            fs::read_to_string(seeded.cas.cas().refs_journal_path()).expect("read journal");
        let alpha_snap_id: spawn_cas::Digest = pg
            .list_refs(Some("proj-rec-lag"))
            .expect("list refs")
            .into_iter()
            .find(|r| r.name == "feat/alpha")
            .expect("feat/alpha ref")
            .snapshot
            .parse()
            .expect("parse alpha snap");
        let alpha_snap = seeded
            .cas
            .cas()
            .read_merkle_snapshot(&alpha_snap_id)
            .expect("read alpha snap");
        let alpha_oid = seeded
            .cas
            .project_snapshot_oid(&alpha_snap, &[seeded.root_git_oid])
            .expect("alpha oid");
        let trailing_journal = format!(
            "{orig_journal}{{\"seq\":99,\"project\":\"proj-rec-lag\",\"name\":\"feat/alpha\",\"kind\":\"branch\",\"old_snapshot\":\"{}\",\"new_snapshot\":\"{alpha_snap_id}\",\"git_oid\":\"{}\",\"at_ms\":1700000500000}}\n",
            seeded.root_digest,
            alpha_oid.to_hex(),
        );
        fs::write(restored_cas_dir.join("refs.journal"), trailing_journal)
            .expect("write trailing journal");

        let restored_cas = CasMerkleStore::open(&restored_cas_dir).expect("open restored cas");
        let (binding, _) = live_gc_binding(&pg).expect("binding");
        bind_store_gc_fence(&restored_cas, &binding).expect("bind restored fence");

        let report = execute_reconcile(
            &ReconcileArgs {
                cas_dir: restored_cas_dir,
                reason_prefix: "cli:reconcile".to_string(),
                json: true,
            },
            &pg,
        )
        .expect("reconcile executes and reports unaligned state");
        assert!(!report.gc_fence_aligned, "{report:?}");
        assert_eq!(report.already_applied, 1);
        assert_eq!(report.entries_replayed, 0);
        assert!(report.pg_latest_ref_seq_after < report.last_journal_seq);

        let (code, err_msg) = reconcile_exit_code_and_error(&report);
        assert_eq!(code, 1);
        let msg = err_msg.expect("non-zero exit includes clear diagnostic");
        assert!(
            msg.contains("gc_fence_aligned=false") && msg.contains("trails refs.journal"),
            "{msg}"
        );
    }

    #[test]
    fn pg_storage_gc_runs_two_round_sweep_and_refuses_lagging_or_mismatched_fence() {
        let scratch = ScratchDir::new("cli-gc-pg");
        let uninit_dir = scratch.0.join("uninit");
        let cas_dir = scratch.0.join("cas");
        let schema = spawn_postgres::testing::TestSchema::new("cli_gc_live");
        let pg = Arc::new(schema.store());

        let zero_err = execute_gc(
            &GcArgs {
                cas_dir: cas_dir.clone(),
                grace: Duration::ZERO,
                json: false,
            },
            &pg,
        )
        .expect_err("grace=0 refused");
        assert!(zero_err.contains("--grace-secs"), "{zero_err}");

        let uninit_err = execute_gc(
            &GcArgs {
                cas_dir: uninit_dir,
                grace: Duration::from_secs(1),
                json: true,
            },
            &pg,
        )
        .expect_err("uninitialized rejected");
        assert!(uninit_err.contains("not initialized"), "{uninit_err}");

        let seeded = seed_d6_project_in_schema(&cas_dir, &pg, "proj-gc");
        let (orphan_digest, _) = seeded
            .cas
            .put_bytes_blob(b"unreferenced persistent orphan blob\n")
            .expect("put orphan blob");
        assert!(seeded.cas.cas().has(&orphan_digest));

        // Wrong PG schema must fail closed.
        let wrong_schema = spawn_postgres::testing::TestSchema::new("cli_gc_wrong");
        let wrong_pg = Arc::new(wrong_schema.store());
        let wrong_err = execute_gc(
            &GcArgs {
                cas_dir: cas_dir.clone(),
                grace: Duration::from_secs(1),
                json: false,
            },
            &wrong_pg,
        )
        .expect_err("wrong PG rejected");
        assert!(wrong_err.contains("GC fence check failed"), "{wrong_err}");

        // Append journal entry beyond PG seq so PG lags journal -> must fail closed.
        let cas_lag_dir = scratch.0.join("cas-lag");
        copy_cas_excluding_fence_and_journal(&cas_dir, &cas_lag_dir);
        let lag_cas = CasMerkleStore::open(&cas_lag_dir).expect("open lag cas");
        let (binding, live_seq) = live_gc_binding(&pg).expect("binding");
        bind_store_gc_fence(&lag_cas, &binding).expect("bind lag fence");
        let lag_journal = format!(
            "{}{{\"seq\":{},\"project\":\"proj-gc\",\"name\":\"extra\",\"kind\":\"branch\",\"old_snapshot\":null,\"new_snapshot\":\"{}\",\"git_oid\":\"{}\",\"at_ms\":1700000400000}}\n",
            fs::read_to_string(seeded.cas.cas().refs_journal_path()).expect("read journal"),
            live_seq + 1,
            seeded.root_digest,
            spawn_cas::GitOid::from_bytes([0x11; 20]).to_hex()
        );
        fs::write(cas_lag_dir.join("refs.journal"), lag_journal).expect("write lag journal");
        let lag_err = execute_gc(
            &GcArgs {
                cas_dir: cas_lag_dir,
                grace: Duration::from_secs(1),
                json: false,
            },
            &pg,
        )
        .expect_err("lagging PG rejected");
        assert!(lag_err.contains("GC fence check failed"), "{lag_err}");

        // Round 1 with minimum grace=1s defers orphan (2-round reachability invariant).
        let gc_args = GcArgs {
            cas_dir: cas_dir.clone(),
            grace: Duration::from_secs(1),
            json: true,
        };
        let sweep1 = execute_gc(&gc_args, &pg).expect("gc sweep 1");
        assert_eq!(sweep1.roots, 3);
        assert_eq!(sweep1.deleted_object_count, 0);
        assert!(sweep1.deferred_objects >= 1, "{sweep1:?}");
        assert!(seeded.cas.cas().has(&orphan_digest));

        std::thread::sleep(Duration::from_millis(1150));

        // Round 2 after grace=1s deletes orphan while keeping all reachable objects.
        let sweep2 = execute_gc(&gc_args, &pg).expect("gc sweep 2");
        assert_eq!(sweep2.deleted_object_count, 1, "{sweep2:?}");
        assert!(sweep2.deleted_objects.contains(&orphan_digest.to_string()));
        assert!(sweep2.freed_bytes > 0);
        assert!(!seeded.cas.cas().has(&orphan_digest));
        assert!(seeded.cas.cas().has(&seeded.root_digest));

        let encoded = serde_json::to_value(&sweep2).expect("serialize gc JSON");
        assert_eq!(encoded["action"], "gc");
        assert_eq!(encoded["deleted_object_count"], 1);
    }

    #[test]
    fn pg_storage_verify_bundle_roundtrips_project_and_fails_closed_on_invalid_inputs() {
        let scratch = ScratchDir::new("cli-verify-bundle-pg");
        let cas_dir = scratch.0.join("cas");
        let schema = spawn_postgres::testing::TestSchema::new("cli_verify_bundle_live");
        let pg = Arc::new(schema.store());
        let _seeded = seed_d6_project_in_schema(&cas_dir, &pg, "proj-bundle");

        let bundle_path = scratch.0.join("out/proj-bundle.bundle");
        let report = execute_verify_bundle(
            &VerifyBundleArgs {
                cas_dir: cas_dir.clone(),
                bundle_path: bundle_path.clone(),
                project: Some("proj-bundle".to_string()),
                repo: BundleRepoArg::None,
                json: true,
            },
            &pg,
        )
        .expect("verify-bundle succeeds");
        assert!(report.ok);
        assert_eq!(report.snapshots_verified, 3);
        assert_eq!(report.refs_verified_count, 3);
        assert!(report.refs_verified.contains_key("refs/heads/main"));
        assert!(report.refs_verified.contains_key("refs/heads/feat/alpha"));
        assert!(report.refs_verified.contains_key("refs/heads/feat/beta"));
        assert!(bundle_path.is_file());

        let encoded = serde_json::to_value(&report).expect("serialize verify-bundle JSON");
        assert_eq!(encoded["action"], "verify_bundle");
        assert_eq!(encoded["ok"], true);
        assert_eq!(encoded["refs_verified_count"], 3);

        let missing_proj_err = execute_verify_bundle(
            &VerifyBundleArgs {
                cas_dir: cas_dir.clone(),
                bundle_path: scratch.0.join("missing-proj.bundle"),
                project: None,
                repo: BundleRepoArg::None,
                json: false,
            },
            &pg,
        )
        .expect_err("missing project rejected");
        assert!(
            missing_proj_err.contains("missing --project"),
            "{missing_proj_err}"
        );

        let fake_bare = scratch.0.join("proj.git");
        let cutover = ProjectCutover::from_state(
            "proj-bundle",
            &fake_bare,
            &scratch.0,
            CutoverPhase::Resumed,
        );
        save_cutover_record(&cas_dir, &cutover, &fake_bare, Some("state")).expect("save state");
        let implicit = execute_verify_bundle(
            &VerifyBundleArgs {
                cas_dir: cas_dir.clone(),
                bundle_path: scratch.0.join("out/implicit.bundle"),
                project: None,
                repo: BundleRepoArg::None,
                json: false,
            },
            &pg,
        )
        .expect("implicit project resolved via cutover state");
        assert_eq!(implicit.project, "proj-bundle");

        let empty_proj_err = execute_verify_bundle(
            &VerifyBundleArgs {
                cas_dir: cas_dir.clone(),
                bundle_path: scratch.0.join("empty.bundle"),
                project: Some("no-such-project".to_string()),
                repo: BundleRepoArg::None,
                json: false,
            },
            &pg,
        )
        .expect_err("unknown project fails closed");
        assert!(
            empty_proj_err.contains("no non-WIP refs"),
            "{empty_proj_err}"
        );

        let wrong_schema = spawn_postgres::testing::TestSchema::new("cli_verify_bundle_wrong");
        let wrong_pg = Arc::new(wrong_schema.store());
        let fence_err = execute_verify_bundle(
            &VerifyBundleArgs {
                cas_dir,
                bundle_path: scratch.0.join("wrong-pg.bundle"),
                project: Some("proj-bundle".to_string()),
                repo: BundleRepoArg::None,
                json: false,
            },
            &wrong_pg,
        )
        .expect_err("wrong PG fails closed");
        assert!(fence_err.contains("GC fence check failed"), "{fence_err}");
    }

    fn assert_bundle_refused_and_bytes_intact(
        cas_dir: &Path,
        pg: &Arc<PostgresStore>,
        project: &str,
        repo: BundleRepoArg,
        target: &Path,
        needles: &[&str],
    ) {
        let before = target
            .is_file()
            .then(|| fs::read(target).expect("read before"));
        let err = execute_verify_bundle(
            &VerifyBundleArgs {
                cas_dir: cas_dir.to_path_buf(),
                bundle_path: target.to_path_buf(),
                project: Some(project.to_string()),
                repo,
                json: false,
            },
            pg,
        )
        .expect_err(&format!("must refuse {}", target.display()));
        assert!(
            needles.iter().any(|needle| err.contains(needle)),
            "{}: {err}",
            target.display()
        );
        match before {
            Some(bytes) => assert_eq!(fs::read(target).expect("read after"), bytes),
            None => assert!(!target.exists(), "must not create {}", target.display()),
        }
    }

    #[test]
    fn pg_storage_verify_bundle_refuses_overwrite_and_internal_dirs() {
        let scratch = ScratchDir::new("cli-verify-bundle-r1-pg");
        let cas_dir = scratch.0.join("cas");
        let schema = spawn_postgres::testing::TestSchema::new("cli_verify_bundle_r1");
        let pg = Arc::new(schema.store());
        let _seeded = seed_d6_project_in_schema(&cas_dir, &pg, "proj-r1");

        let bare_repo = scratch.0.join("proj-r1.git");
        fs::create_dir_all(bare_repo.join("objects")).expect("mkdir bare objects");
        fs::write(bare_repo.join("HEAD"), b"ref: refs/heads/main\n").expect("write bare HEAD");
        let cutover =
            ProjectCutover::from_state("proj-r1", &bare_repo, &scratch.0, CutoverPhase::Resumed);
        save_cutover_record(&cas_dir, &cutover, &bare_repo, Some("r1")).expect("save cutover");

        let existing_file = scratch.0.join("keep.bundle");
        fs::write(&existing_file, b"CRITICAL_EXISTING_BACKUP_CONTENT\n").expect("write sentinel");

        for (target, repo, needles) in [
            (existing_file, BundleRepoArg::None, &["already exists"][..]),
            (
                cas_dir.join("refs.journal"),
                BundleRepoArg::None,
                &["already exists", "inside CAS"][..],
            ),
            (
                cas_dir.join(".gc-fence"),
                BundleRepoArg::None,
                &["already exists", "inside CAS"][..],
            ),
            (
                cas_dir.join("new.bundle"),
                BundleRepoArg::None,
                &["inside CAS"][..],
            ),
            (
                bare_repo.join("HEAD"),
                BundleRepoArg::Explicit(bare_repo.clone()),
                &["already exists", "inside bare repo"][..],
            ),
            (
                bare_repo.join("new.bundle"),
                BundleRepoArg::Explicit(bare_repo.clone()),
                &["inside bare repo"][..],
            ),
        ] {
            assert_bundle_refused_and_bytes_intact(
                &cas_dir, &pg, "proj-r1", repo, &target, needles,
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn pg_storage_verify_bundle_refuses_symlink_ancestors_and_traversal() {
        let scratch = ScratchDir::new("cli-verify-bundle-r1-sym-pg");
        let cas_dir = scratch.0.join("cas");
        let schema = spawn_postgres::testing::TestSchema::new("cli_verify_bundle_r1_sym");
        let pg = Arc::new(schema.store());
        let _seeded = seed_d6_project_in_schema(&cas_dir, &pg, "proj-r1-sym");

        let real_base = scratch.0.join("real_base");
        fs::create_dir_all(real_base.join("sub")).expect("mkdir real_base/sub");
        let gp_link = scratch.0.join("sym_gp");
        symlink(&real_base, &gp_link).expect("symlink gp");

        assert_bundle_refused_and_bytes_intact(
            &cas_dir,
            &pg,
            "proj-r1-sym",
            BundleRepoArg::None,
            &gp_link.join("sub/grandparent.bundle"),
            &["symlink"],
        );
        assert!(!real_base.join("sub/grandparent.bundle").exists());

        assert_bundle_refused_and_bytes_intact(
            &cas_dir,
            &pg,
            "proj-r1-sym",
            BundleRepoArg::None,
            &gp_link.join("../x.bundle"),
            &["symlink"],
        );
        assert!(!scratch.0.join("x.bundle").exists());
        assert!(!real_base.join("x.bundle").exists());
    }

    #[cfg(unix)]
    #[test]
    fn pg_storage_verify_bundle_ignores_preplanted_staging_symlink() {
        let scratch = ScratchDir::new("cli-verify-bundle-r3-stage-sym-pg");
        let cas_dir = scratch.0.join("cas");
        let schema = spawn_postgres::testing::TestSchema::new("cli_verify_bundle_r3_stage_sym");
        let pg = Arc::new(schema.store());
        let _seeded = seed_d6_project_in_schema(&cas_dir, &pg, "proj-r3-stage-sym");

        let out_dir = scratch.0.join("out");
        fs::create_dir_all(&out_dir).expect("mkdir out");
        let sentinel = scratch.0.join("outside-sentinel.txt");
        let expected_bytes = b"UNTOUCHED_OUTSIDE_SENTINEL\n";
        fs::write(&sentinel, expected_bytes).expect("write sentinel");

        let pid = std::process::id();
        let next_seq = NEXT_TMP_SEQ.load(Ordering::Relaxed);
        for link_path in [
            out_dir.join(format!(".verify-bundle-{pid}-{next_seq}.tmp")),
            out_dir.join(format!(".verify-bundle-{pid}-{}.tmp", next_seq + 1)),
            out_dir.join("bundle.tmp"),
        ] {
            symlink(&sentinel, &link_path).expect("preplant staging symlink");
        }

        let final_bundle = out_dir.join("verified.bundle");
        let report = execute_verify_bundle(
            &VerifyBundleArgs {
                cas_dir,
                bundle_path: final_bundle.clone(),
                project: Some("proj-r3-stage-sym".to_string()),
                repo: BundleRepoArg::None,
                json: true,
            },
            &pg,
        )
        .expect("verify-bundle ignores preplanted symlinks in out_dir");
        assert!(report.ok);
        let meta = fs::symlink_metadata(&final_bundle).expect("stat final bundle");
        assert!(meta.is_file() && !meta.file_type().is_symlink());
        assert_eq!(fs::read(&sentinel).expect("read sentinel"), expected_bytes);
        for entry in fs::read_dir(&out_dir).expect("read out_dir") {
            let name = entry.expect("dir entry").file_name();
            let name = name.to_string_lossy();
            assert!(
                !(name.starts_with(".verify-bundle-") && name.ends_with(".d")),
                "staging directory leaked: {name}"
            );
        }
    }
}

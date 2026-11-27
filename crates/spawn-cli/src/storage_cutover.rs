//! `spawn storage cutover`: operator CLI for D6 persistent CAS cutover and rollback (`#113`).
//!
//! Coordinates [`spawn_host::ProjectCutover`]'s lossless 7-step cutover from a project's bare
//! Git repository to D6 (`CasMerkleStore` + `PostgresRefLedger` + read-only Git border),
//! read-only status/parity/GC-fence verification, and deterministic rollback back to writable Git.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use spawn_cas::{CAS_MARKER, CasError, GcFenceBinding};
use spawn_host::{
    BACKEND_MARKER, CasMerkleStore, CutoverParityReport, CutoverPhase, D6StoreError, GcFenceStatus,
    GitBorderAdapter, ProjectCutover, ProjectStorageMode, WorkspaceBackend,
    assert_zero_pending_merges, bind_store_gc_fence, check_gc_fence, is_readonly_border,
    root_backend, verify_cutover_parity, verify_zero_pending_merges,
};
use spawn_postgres::PostgresStore;

use crate::config::{D6RuntimeConfig, Flags, env_value, open_store, workspace_root};
use crate::sources::PostgresRefLedger;

/// Usage line for `spawn storage` (`main`'s help reads this).
pub(crate) const USAGE: &str = "usage: spawn storage cutover --status|--to d6|--rollback git|--verify [--project <name>] [--repo <bare repo>] [--root <dir>] [--cas-dir <dir>] [--reason <text>] [--json]";

const CUTOVER_STATE_FILE: &str = ".cutover-state.json";
const CUTOVER_STATE_SCHEMA_VERSION: u32 = 2;
const LEGACY_CUTOVER_STATE_SCHEMA_VERSION: u32 = 1;
const SWITCHES: &[&str] = &["status", "verify", "json"];
const ALLOWED_FLAGS: &[&str] = &[
    "status", "to", "rollback", "verify", "project", "repo", "root", "cas-dir", "reason", "json",
];

static NEXT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutoverAction {
    Status,
    ToD6,
    RollbackGit,
    Verify,
}

#[derive(Debug, Clone)]
struct ParsedArgs {
    action: CutoverAction,
    project: Option<String>,
    repo: Option<PathBuf>,
    root: PathBuf,
    cas_dir: PathBuf,
    reason: Option<String>,
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

/// Entrypoint for `spawn storage ...`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let Some((sub, tail)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    if sub != "cutover" {
        return Err(format!("unknown storage subcommand {sub}\n{USAGE}"));
    }
    let parsed = parse_cutover_args(tail)?;
    match parsed.action {
        CutoverAction::Status => run_status(&parsed),
        CutoverAction::ToD6 => run_cutover_to_d6(&parsed),
        CutoverAction::RollbackGit => run_rollback_to_git(&parsed),
        CutoverAction::Verify => run_verify(&parsed),
    }
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

fn parse_cutover_args(args: &[String]) -> Result<ParsedArgs, String> {
    let flags = Flags::parse_with_switches(args, SWITCHES).map_err(|e| format!("{e}\n{USAGE}"))?;
    for name in flags.names() {
        if !ALLOWED_FLAGS.contains(&name) {
            return Err(format!("unknown flag --{name}\n{USAGE}"));
        }
    }

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
        PathBuf::from(explicit)
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

fn require_bound_checked_cas(cas_dir: &Path, pg: &PostgresStore) -> Result<CasMerkleStore, String> {
    let (live_binding, pg_latest_ref_seq) = live_gc_binding(pg)?;
    let store = CasMerkleStore::open(cas_dir)
        .map_err(|e| format!("open CAS store {}: {e}", cas_dir.display()))?;
    if store
        .cas()
        .read_gc_fence()
        .map_err(|e| format!("read .gc-fence: {e}"))?
        .is_none()
    {
        return Err(format!(
            "CAS store {} has no .gc-fence binding; refusing rollback",
            cas_dir.display()
        ));
    }
    check_gc_fence(&store, &live_binding, pg_latest_ref_seq)
        .map_err(|e| format!("GC fence check failed before rollback: {e}"))?;
    Ok(store)
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
    let store = require_bound_checked_cas(&args.cas_dir, &pg)?;

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

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(prefix: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("spawn-cli-{prefix}-{}-{stamp}", std::process::id()));
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
        let bogus = command(&s(&["compact"])).expect_err("unknown subcommand");
        assert!(bogus.contains("unknown storage subcommand"));
    }
}

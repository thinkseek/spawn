//! Configuration read from the environment and flags, and the store it
//! points at.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its hook and runner need.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use spawn_postgres::{ConnectOptions, LibpqDefaults, PostgresStore};

pub(crate) use crate::config_file::SecretString;
pub(crate) use crate::workspace_backend::WorkspaceBackend;

/// Process exit status (`EX_CONFIG` in `<sysexits.h>`) when a Keep-delivered
/// `$CREDENTIALS_DIRECTORY/<id>` credential file is unreadable, empty, malformed,
/// or a symlink so systemd `RestartPreventExitStatus=78` halts fail-closed.
pub(crate) const CREDENTIAL_EXIT_CODE: i32 = 78;

/// Validate a credential file inside `cred_dir` without reading or echoing its bytes.
///
/// Returns `Ok(None)` when `cred_dir` is `None` or `<cred_dir>/<id>` does not exist,
/// falling back cleanly to legacy configuration. Once `<cred_dir>/<id>` exists,
/// symlinks, non-regular files, empty files, and group/world permissions fail closed.
pub(crate) fn load_credential_file(
    cred_dir: Option<&Path>,
    id: &str,
) -> Result<Option<PathBuf>, String> {
    let Some(dir) = cred_dir else {
        return Ok(None);
    };
    let path = dir.join(id);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("credential {id}: {error}")),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!("credential {id} must be a regular file"));
    }
    if meta.len() == 0 {
        return Err(format!("credential {id} is empty"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "credential {id} has mode {mode:o}: must be 0400 or 0600"
            ));
        }
    }
    Ok(Some(path))
}

/// Read a single-value UTF-8 credential from `cred_dir/<id>` into a [`SecretString`],
/// stripping at most one trailing newline while rejecting empty values, embedded
/// newlines/NULs, or symlinks.
pub(crate) fn load_credential_secret(
    cred_dir: Option<&Path>,
    id: &str,
) -> Result<Option<SecretString>, String> {
    let Some(path) = load_credential_file(cred_dir, id)? else {
        return Ok(None);
    };
    let bytes = fs::read(&path).map_err(|error| format!("credential {id}: {error}"))?;
    let raw =
        String::from_utf8(bytes).map_err(|_| format!("credential {id} is not valid UTF-8"))?;
    let trimmed = if let Some(rest) = raw.strip_suffix('\n') {
        rest.strip_suffix('\r').unwrap_or(rest)
    } else {
        &raw
    };
    if trimmed.is_empty() {
        return Err(format!("credential {id} is empty"));
    }
    if trimmed.contains(['\n', '\r', '\0']) {
        return Err(format!(
            "credential {id} contains embedded newline or NUL byte"
        ));
    }
    Ok(Some(SecretString::new(trimmed)))
}

/// Resolve `$CREDENTIALS_DIRECTORY/<id>` as a validated [`SecretString`].
/// Process environment boundary primitive: reads one UTF-8 env var at the CLI edge.
#[allow(clippy::disallowed_methods)]
pub(crate) fn process_var(name: &str) -> Result<String, std::env::VarError> {
    std::env::var(name)
}

/// Process environment boundary primitive: reads one raw `OsString` env var at the CLI edge.
#[allow(clippy::disallowed_methods)]
pub(crate) fn process_var_os(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

/// Process environment boundary primitive: collects all process env var keys at the CLI edge.
#[allow(clippy::disallowed_methods)]
pub(crate) fn process_var_keys() -> Vec<std::ffi::OsString> {
    std::env::vars_os().map(|(key, _)| key).collect()
}

/// Lookup adapter passed to `config_file` without introducing a crate dependency edge.
pub(crate) fn process_env_for_config(name: &str) -> Option<String> {
    process_var(name).ok()
}

/// Returns whether a process environment variable is present (including empty values).
pub(crate) fn has_env_var(name: &str) -> bool {
    process_var_os(name).is_some()
}

pub(crate) fn credential_secret(id: &str) -> Result<Option<SecretString>, String> {
    load_credential_secret(
        env_value("CREDENTIALS_DIRECTORY").as_deref().map(Path::new),
        id,
    )
}

pub(crate) fn env_value(name: &str) -> Option<String> {
    process_var(name).ok().filter(|value| !value.is_empty())
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    process_var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn workspace_root_dir() -> Option<PathBuf> {
    process_var_os("SPAWN_WORKSPACE_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn data_root_dir() -> Option<PathBuf> {
    process_var_os("SPAWN_DATA_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub(crate) fn workspace_max_file_bytes() -> u64 {
    spawn_host::workspace_rules::max_file_bytes(process_var("SPAWN_MAX_FILE_MB").ok().as_deref())
}

pub(crate) fn collector_mask_config() -> spawn_host::CollectorMaskConfig {
    spawn_host::CollectorMaskConfig {
        tool: process_var_os("SPAWN_ARCHIVE_LOCATIONS_TOOL")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        sandbox: process_var("SPAWN_GATE_SANDBOX").ok(),
    }
}

pub(crate) fn herdr_discovery_env() -> spawn_host::HerdrDiscoveryEnv {
    spawn_host::HerdrDiscoveryEnv {
        hc_socket: env_value("HC_SOCKET")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from),
        xdg_runtime_dir: process_var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    }
}

pub(crate) fn terminal_host_env() -> spawn_host::TerminalHostEnv {
    spawn_host::TerminalHostEnv {
        home: home_dir(),
        xdg_state_home: env_value("XDG_STATE_HOME").map(PathBuf::from),
        xdg_config_home: env_value("XDG_CONFIG_HOME").map(PathBuf::from),
        xdg_runtime_dir: env_value("XDG_RUNTIME_DIR").map(PathBuf::from),
        launch_clear_limit: Some(spawn_host::herdr::launch_clear_limit_from(
            env_value("SPAWN_HERDR_LAUNCH_CLEAR_MS").as_deref(),
        )),
        scrubbed_env_keys: spawn_host::TerminalHostEnv::collect_scrubbed_keys(process_var_keys()),
    }
}

pub(crate) fn serve_local_open() -> bool {
    process_var("SPAWN_SERVE_LOCAL_OPEN").is_ok_and(|value| value == "1")
}

/// Closed sum type selecting how `spawn serve` obtains feature-coverage data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FeatureCoverageSource {
    /// Walk the local source tree against its registry file.
    LiveRepo { root: PathBuf, registry: PathBuf },
    /// Serve an immutable precomputed snapshot JSON without walking any source tree.
    PrecomputedSnapshot { snapshot_path: PathBuf },
}

impl FeatureCoverageSource {
    /// Resolve the active coverage source from an environment lookup and working directory.
    pub(crate) fn resolve_with<F>(env: F, cwd: &Path) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let non_empty = |name: &str| env(name).filter(|value| !value.trim().is_empty());
        if let Some(snapshot_path) = non_empty("SPAWN_FEATURE_COVERAGE_SNAPSHOT") {
            return Self::PrecomputedSnapshot {
                snapshot_path: PathBuf::from(snapshot_path),
            };
        }
        let root = non_empty("SPAWN_FEATURE_ROOT").map_or_else(|| cwd.to_path_buf(), PathBuf::from);
        let registry = non_empty("SPAWN_FEATURE_REGISTRY").map_or_else(
            || root.join(crate::feature_inventory::DEFAULT_REGISTRY_REL),
            PathBuf::from,
        );
        Self::LiveRepo { root, registry }
    }
}

/// Resolve `SPAWN_FEATURE_ROOT` override from the process environment boundary.
pub(crate) fn feature_root_override() -> Option<PathBuf> {
    env_value("SPAWN_FEATURE_ROOT")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

/// Resolve `SPAWN_FEATURE_REGISTRY` override from the process environment boundary.
pub(crate) fn feature_registry_override() -> Option<PathBuf> {
    env_value("SPAWN_FEATURE_REGISTRY")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

/// Resolve [`FeatureCoverageSource`] from the process environment boundary.
pub(crate) fn feature_coverage_source() -> FeatureCoverageSource {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    FeatureCoverageSource::resolve_with(env_value, &cwd)
}

/// Resolve the canonical local bare Spawn mission repository path
/// (`<live>/missions/spawn.git`) from the process environment boundary.
pub(crate) fn canonical_spawn_mission_repo() -> Option<PathBuf> {
    canonical_spawn_mission_repo_with(env_value)
}

fn canonical_spawn_mission_repo_with<F>(env: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let non_empty_abs = |name: &str| {
        env(name).and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty() && Path::new(trimmed).is_absolute())
                .then(|| PathBuf::from(trimmed))
        })
    };
    if let Some(repo) = non_empty_abs("SPAWN_MISSION_REPO") {
        return Some(repo);
    }
    if let Some(live) = non_empty_abs("SPAWN_LIVE") {
        return Some(live.join("missions").join("spawn.git"));
    }
    if let Some(rsi_dir) = non_empty_abs("SPAWN_RSI_DIR") {
        return Some(rsi_dir.join("missions").join("spawn.git"));
    }
    if let Some(feature_root) = non_empty_abs("SPAWN_FEATURE_ROOT")
        && feature_root.file_name().and_then(|s| s.to_str()) == Some("spawn")
        && let Some(controllers_dir) = feature_root.parent()
        && controllers_dir.file_name().and_then(|s| s.to_str()) == Some("controllers")
        && let Some(live_base) = controllers_dir.parent()
    {
        return Some(live_base.join("missions").join("spawn.git"));
    }
    non_empty_abs("HOME").map(|home| home.join(".local/share/spawn-rsi/missions/spawn.git"))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ServePoolEnv {
    pub(crate) reads: Option<String>,
    pub(crate) writes: Option<String>,
    pub(crate) node: Option<String>,
    pub(crate) events: Option<String>,
}

impl ServePoolEnv {
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        match name {
            "SPAWN_SERVE_READS_POOL" => self.reads.as_deref(),
            "SPAWN_SERVE_WRITES_POOL" => self.writes.as_deref(),
            "SPAWN_SERVE_NODE_POOL" => self.node.as_deref(),
            "SPAWN_SERVE_EVENTS_POOL" => self.events.as_deref(),
            _ => None,
        }
    }
}

pub(crate) fn serve_pool_env() -> ServePoolEnv {
    ServePoolEnv {
        reads: process_var("SPAWN_SERVE_READS_POOL").ok(),
        writes: process_var("SPAWN_SERVE_WRITES_POOL").ok(),
        node: process_var("SPAWN_SERVE_NODE_POOL").ok(),
        events: process_var("SPAWN_SERVE_EVENTS_POOL").ok(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SshForwardedEnv {
    pub(crate) entries: [(&'static str, Option<std::ffi::OsString>); 5],
}

pub(crate) fn ssh_forwarded_env() -> SshForwardedEnv {
    SshForwardedEnv {
        entries: [
            ("PATH", process_var_os("PATH")),
            ("HOME", process_var_os("HOME")),
            ("USER", process_var_os("USER")),
            ("LOGNAME", process_var_os("LOGNAME")),
            ("SSH_AUTH_SOCK", process_var_os("SSH_AUTH_SOCK")),
        ],
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GateLoadEnv {
    pub(crate) max_load: Option<String>,
    pub(crate) max_load_per_core: Option<String>,
    pub(crate) load_backoff_ms: Option<String>,
    pub(crate) load_backoff_attempts: Option<String>,
    pub(crate) load_retries: Option<String>,
}

impl GateLoadEnv {
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        match name {
            "SPAWN_GATE_MAX_LOAD" => self.max_load.as_deref(),
            "SPAWN_GATE_MAX_LOAD_PER_CORE" => self.max_load_per_core.as_deref(),
            "SPAWN_GATE_LOAD_BACKOFF_MS" => self.load_backoff_ms.as_deref(),
            "SPAWN_GATE_LOAD_BACKOFF_ATTEMPTS" => self.load_backoff_attempts.as_deref(),
            "SPAWN_GATE_LOAD_RETRIES" => self.load_retries.as_deref(),
            _ => None,
        }
    }
}

pub(crate) fn gate_load_env() -> GateLoadEnv {
    GateLoadEnv {
        max_load: process_var("SPAWN_GATE_MAX_LOAD").ok(),
        max_load_per_core: process_var("SPAWN_GATE_MAX_LOAD_PER_CORE").ok(),
        load_backoff_ms: process_var("SPAWN_GATE_LOAD_BACKOFF_MS").ok(),
        load_backoff_attempts: process_var("SPAWN_GATE_LOAD_BACKOFF_ATTEMPTS").ok(),
        load_retries: process_var("SPAWN_GATE_LOAD_RETRIES").ok(),
    }
}

/// How long a message may go untaken before it is said not taken
/// (objective 045): `SPAWN_MESSAGE_TAKEN_SECS`, whole seconds, default
/// [`MESSAGE_TAKEN_SECS`].
pub(crate) const MESSAGE_TAKEN_SECS: u64 = 120;

/// The bound `text` (`SPAWN_MESSAGE_TAKEN_SECS`) sets, in milliseconds, and
/// the warning a value that is no whole number of seconds gets (the default
/// is used).
pub(crate) fn message_taken_bound(text: Option<&str>) -> (i64, Option<String>) {
    let default = MESSAGE_TAKEN_SECS.saturating_mul(1000).cast_signed();
    let Some(text) = text else {
        return (default, None);
    };
    match text.trim().parse::<u64>() {
        Ok(secs) => (
            i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX),
            None,
        ),
        Err(_) => (
            default,
            Some(format!(
                "SPAWN_MESSAGE_TAKEN_SECS: {text:?} is not a whole number of seconds; using \
                 {MESSAGE_TAKEN_SECS}"
            )),
        ),
    }
}

/// The bound of a message not taken, from the environment, read once per
/// process: a bad value is warned of once.
pub(crate) fn message_taken_ms() -> i64 {
    static BOUND: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *BOUND.get_or_init(|| {
        let (bound, warning) =
            message_taken_bound(env_value("SPAWN_MESSAGE_TAKEN_SECS").as_deref());
        if let Some(warning) = warning {
            eprintln!("spawn warning: {warning}");
        }
        bound
    })
}

pub(crate) fn env_or(name: &str, fallback: &str) -> String {
    env_value(name).unwrap_or_else(|| fallback.to_string())
}

/// Where `SPAWN_DATABASE_URL` comes from, as the usage and the error of a
/// command run without it say (GitHub #63, part 3): spawn reads it from its
/// environment only, and a fleet host keeps it in the loop's `spawn.env`.
pub(crate) const DATABASE_URL_SOURCE: &str = "spawn reads SPAWN_DATABASE_URL from its \
     environment only: on a fleet host it is in the loop's spawn.env (source that file \
     first); elsewhere export SPAWN_DATABASE_URL as a postgres:// connection string";

pub(crate) fn required_env(name: &str) -> Result<String, String> {
    env_value(name).ok_or_else(|| not_set(name))
}

/// The error of a required variable that is not set; the database's says
/// where it comes from ([`DATABASE_URL_SOURCE`]).
fn not_set(name: &str) -> String {
    if name == "SPAWN_DATABASE_URL" {
        format!("{name} is not set; {DATABASE_URL_SOURCE}")
    } else {
        format!("{name} is not set")
    }
}

/// Resolve the controller's PostgreSQL URL as a [`SecretString`], preferring Keep's
/// `$CREDENTIALS_DIRECTORY/spawn.database-url` when present and falling back to
/// `SPAWN_DATABASE_URL`.
pub(crate) fn database_url_secret() -> Result<SecretString, String> {
    if let Some(url) = credential_secret("spawn.database-url")? {
        return Ok(url);
    }
    required_env("SPAWN_DATABASE_URL").map(SecretString::new)
}

/// Resolve libpq passfile location at the CLI boundary (`PGPASSFILE` else `$HOME/.pgpass`).
pub(crate) fn pg_passfile() -> Option<PathBuf> {
    spawn_postgres::resolve_passfile_path(
        process_var_os("PGPASSFILE").as_deref(),
        process_var_os("HOME").as_deref(),
    )
}

/// Explicit libpq connect options injected from the CLI environment boundary.
pub(crate) fn pg_connect_options(statement_timeout: Option<std::time::Duration>) -> ConnectOptions {
    ConnectOptions {
        passfile: pg_passfile(),
        statement_timeout,
    }
}

/// Explicit libpq default identity fields read at the CLI environment boundary.
pub(crate) fn libpq_defaults() -> LibpqDefaults {
    LibpqDefaults {
        port: env_value("PGPORT").and_then(|port| port.trim().parse().ok()),
        user: env_value("PGUSER").or_else(|| env_value("USER")),
        database: env_value("PGDATABASE"),
    }
}

pub(crate) fn open_store() -> Result<PostgresStore, String> {
    let url = database_url_secret()?;
    PostgresStore::connect_with_options(url.expose_secret(), &pg_connect_options(None))
        .map_err(|error| error.to_string())
}

/// Whether this host runs the supervisor (`SPAWN_SUPERVISOR` is set).
pub(crate) fn supervised() -> bool {
    env_value("SPAWN_SUPERVISOR").is_some()
}

/// The `spawn` program every caller names: in the terminals the Bridge
/// starts, in the commands the dispatcher submits, and in the supervisor's
/// charter. `SPAWN_PROGRAM`, else the current executable, else `spawn`.
pub(crate) fn spawn_program() -> String {
    resolve_spawn_program(env_value("SPAWN_PROGRAM"), std::env::current_exe().ok())
}

fn resolve_spawn_program(configured: Option<String>, current: Option<PathBuf>) -> String {
    match configured {
        Some(program) => program,
        None => current.map_or_else(|| "spawn".to_string(), |path| path.display().to_string()),
    }
}

/// `--root`, else `SPAWN_WORKSPACE_ROOT`, resolved through its symlinks,
/// once, here (`spawn_host::canonical_root`). An engine's process sees its
/// workspace under the resolved path, and files its transcript under it:
/// the mounts, the panes, the workspace ids and the transcript paths all
/// derive from this one root, so no two of them can name a workspace two
/// ways.
pub(crate) fn workspace_root(flags: &Flags) -> Option<String> {
    resolve_workspace_root(flags.get("root"), env_value("SPAWN_WORKSPACE_ROOT"))
        .map(|root| canonical_workspace_root(&root))
}

/// `root` resolved through its symlinks ([`workspace_root`]).
fn canonical_workspace_root(root: &str) -> String {
    spawn_host::canonical_root(std::path::Path::new(root))
        .display()
        .to_string()
}

fn resolve_workspace_root(flag: Option<String>, environment: Option<String>) -> Option<String> {
    flag.or(environment)
}

/// The oldest git Spawn runs on. Merges of main into a branch, the Bridge's
/// and the integrator's, take main's merge rules from its `.gitattributes`
/// with the global option `git --attr-source=<main>` (a bare repository
/// reads no in-tree attributes otherwise). Git 2.41 introduced that option,
/// and `git merge-tree` crashed when given it until Git 2.43 fixed that, so
/// 2.43 is the first git whose merges work.
pub(crate) const MIN_GIT: (u32, u32) = (2, 43);

/// Major and minor version of `git --version`'s answer (`git version
/// 2.56.0`, `git version 2.39.5 (Apple Git-154)`, `git version
/// 2.45.1.windows.1`).
fn git_version(answer: &str) -> Option<(u32, u32)> {
    let version = answer.trim().strip_prefix("git version ")?;
    let mut parts = version.split(['.', ' ']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// `Ok` when `answer` (what `git --version` printed) names [`MIN_GIT`] or
/// newer; otherwise why not, in words that say what to install.
fn check_git_version(answer: &str) -> Result<(), String> {
    let (major, minor) = MIN_GIT;
    match git_version(answer) {
        Some(found) if found >= MIN_GIT => Ok(()),
        Some((found_major, found_minor)) => Err(format!(
            "git {found_major}.{found_minor} is too old: Spawn needs git {major}.{minor} or later \
             (merges take main's .gitattributes through `git --attr-source`, which \
             `git merge-tree` runs without crashing since {major}.{minor})"
        )),
        None => Err(format!(
            "cannot read the git version from {:?}; Spawn needs git {major}.{minor} or later",
            answer.trim()
        )),
    }
}

/// Check once, at start, that the `git` on `PATH` is [`MIN_GIT`] or newer,
/// so an old git fails with a clear error instead of an unknown option in
/// the middle of a merge.
pub(crate) fn require_git() -> Result<(), String> {
    let output = crate::sources::controller_git()
        .arg("--version")
        .output()
        .map_err(|error| format!("run git --version: {error}"))?;
    check_git_version(&String::from_utf8_lossy(&output.stdout))
}

/// Pause between loop passes: `SPAWN_LOOP_INTERVAL_MS`, 500 by default.
/// Each pass reads every live engine's transcript and screen, so a short
/// pause is how quickly the fleet notices anything; the cost is a few reads.
pub(crate) fn loop_interval() -> std::time::Duration {
    std::time::Duration::from_millis(
        env_value("SPAWN_LOOP_INTERVAL_MS")
            .and_then(|text| text.parse().ok())
            .unwrap_or(500),
    )
}

/// The backend `value` (`SPAWN_WORKSPACE_BACKEND`) names: git when it is
/// unset or empty; anything else unrecognized (including retired `cow`,
/// `overlay`, `d6-shadow`) is refused, never read as git.
pub(crate) fn workspace_backend(value: Option<&str>) -> Result<WorkspaceBackend, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(WorkspaceBackend::Named(spawn_host::WorkspaceBackend::Git)),
        Some(retired @ ("cow" | "overlay" | "d6-shadow")) => Err(format!(
            "SPAWN_WORKSPACE_BACKEND={retired:?} names retired workspace backend \
             {retired} (removed in S3a; see docs/upgrading.md): use git (the default), d6 or auto"
        )),
        Some(name) => WorkspaceBackend::parse(name).ok_or_else(|| {
            format!(
                "SPAWN_WORKSPACE_BACKEND={name:?} names no workspace backend: use git (the \
                 default), d6 or auto"
            )
        }),
    }
}

/// Default bounded L1 `tmpfs` budget in MiB when `SPAWN_D6_TMPFS_ROOT` is configured
/// without `SPAWN_D6_TMPFS_BUDGET_MB`.
pub(crate) const DEFAULT_D6_TMPFS_BUDGET_MB: u64 = 2048;

/// Optional runtime configuration for the opt-in `d6` workspace backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct D6RuntimeConfig {
    /// `SPAWN_D6_CAS_DIR`: override for the CAS Merkle + `refs.journal` directory.
    pub(crate) cas_dir: Option<PathBuf>,
    /// `SPAWN_D6_TMPFS_ROOT`: opt-in bounded L1 `tmpfs` upperdir directory.
    pub(crate) tmpfs_root: Option<PathBuf>,
    /// `SPAWN_D6_TMPFS_BUDGET_MB`: hard L1 memory budget in MiB (`2048` default when `tmpfs_root` set).
    pub(crate) tmpfs_budget_mb: Option<u64>,
}

fn parse_abs_dir(var_name: &str, raw: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let path = std::path::Path::new(text);
    if !path.is_absolute() {
        return Err(format!(
            "{var_name}={text:?} must be an absolute directory path"
        ));
    }
    Ok(Some(spawn_host::canonical_root(path)))
}

fn parse_positive_u64(var_name: &str, raw: Option<&str>) -> Result<Option<u64>, String> {
    let Some(text) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    match text.parse::<u64>() {
        Ok(value) if value > 0 => Ok(Some(value)),
        _ => Err(format!(
            "{var_name}={text:?} must be a positive whole number of MiB"
        )),
    }
}

impl D6RuntimeConfig {
    /// Parse `SPAWN_D6_*` configuration values explicitly (unit-testable without mutating env).
    pub(crate) fn parse(
        cas_dir: Option<&str>,
        tmpfs_root: Option<&str>,
        tmpfs_budget_mb: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Self {
            cas_dir: parse_abs_dir("SPAWN_D6_CAS_DIR", cas_dir)?,
            tmpfs_root: parse_abs_dir("SPAWN_D6_TMPFS_ROOT", tmpfs_root)?,
            tmpfs_budget_mb: parse_positive_u64("SPAWN_D6_TMPFS_BUDGET_MB", tmpfs_budget_mb)?,
        })
    }

    /// Read and validate `SPAWN_D6_*` settings from the process environment.
    pub(crate) fn from_env() -> Result<Self, String> {
        Self::parse(
            env_value("SPAWN_D6_CAS_DIR").as_deref(),
            env_value("SPAWN_D6_TMPFS_ROOT").as_deref(),
            env_value("SPAWN_D6_TMPFS_BUDGET_MB").as_deref(),
        )
    }

    /// Resolved CAS root under workspace `root`.
    pub(crate) fn effective_cas_dir(&self, root: &std::path::Path) -> PathBuf {
        if let Some(dir) = &self.cas_dir {
            return dir.clone();
        }
        root.join(".d6").join("cas")
    }

    /// Bounded L1 `tmpfs` policy when `SPAWN_D6_TMPFS_ROOT` is configured.
    pub(crate) fn l1_policy(&self) -> Option<spawn_host::d6_workspace::L1TmpfsPolicy> {
        self.tmpfs_root.as_ref().map(|tmpfs_root| {
            spawn_host::d6_workspace::L1TmpfsPolicy::new(
                tmpfs_root,
                self.tmpfs_budget_mb.unwrap_or(DEFAULT_D6_TMPFS_BUDGET_MB),
            )
        })
    }
}

/// How existing-root markers are inspected (in production via [`spawn_host::root_backend`],
/// or stubbed in unit tests).
pub(crate) struct Probes<'a> {
    /// The backend the root already is ([`spawn_host::root_backend`]: its
    /// marker, else what it holds), read and never written. Asked only for
    /// `auto`, which follows it.
    pub(crate) marked:
        &'a dyn Fn(&std::path::Path) -> Result<Option<spawn_host::WorkspaceBackend>, String>,
}

impl Probes<'static> {
    /// The host's own reader for an existing root marker ([`spawn_host::root_backend`]).
    pub(crate) fn host() -> Self {
        fn marked(root: &std::path::Path) -> Result<Option<spawn_host::WorkspaceBackend>, String> {
            spawn_host::root_backend(root).map_err(|error| error.to_string())
        }
        Self { marked: &marked }
    }
}

/// The backend the workspaces under one root run on, chosen once: every
/// store of the process (the default repository's, every mission's, the
/// one-shot stores of a merge or a set-aside) is that backend, and the root
/// is claimed for it ([`spawn_host::claim_root`]), so no two backends ever
/// share a root, whatever process asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceSettings {
    /// `SPAWN_WORKSPACE_BACKEND` as given (`git` when unset).
    pub(crate) configured: WorkspaceBackend,
    /// The active concrete host workspace backend (`Git` or `D6`).
    pub(crate) active: spawn_host::WorkspaceBackend,
    /// D6 runtime configuration (`SPAWN_D6_*`).
    pub(crate) d6: D6RuntimeConfig,
    /// File size cap (`SPAWN_MAX_FILE_MB`) applied to checkpoints in the opened store.
    pub(crate) max_file_bytes: u64,
}

impl WorkspaceSettings {
    /// Resolve `configured` under `root`. `auto` follows the backend the
    /// root already holds ([`spawn_host::root_backend`]: its marker, else an
    /// unclaimed root with workspaces stays `git`); on a clean root with no
    /// backend marker yet, [`spawn_host::WorkspaceBackend::auto`] selects `Git`.
    pub(crate) fn resolve(
        configured: WorkspaceBackend,
        root: &std::path::Path,
        probes: &Probes<'_>,
        max_file_bytes: u64,
    ) -> Result<Self, String> {
        let active = match configured {
            WorkspaceBackend::Named(host) => host,
            WorkspaceBackend::Auto => match (probes.marked)(root)? {
                Some(there) => there,
                None => spawn_host::WorkspaceBackend::auto(root),
            },
        };
        Ok(Self {
            configured,
            active,
            d6: D6RuntimeConfig::default(),
            max_file_bytes,
        })
    }

    /// Attach explicit [`D6RuntimeConfig`] settings (builder for tests and callers).
    #[must_use]
    pub(crate) fn with_d6(mut self, d6: D6RuntimeConfig) -> Self {
        self.d6 = d6;
        self
    }

    /// The settings the environment gives for `root`
    /// (`SPAWN_WORKSPACE_BACKEND`, `SPAWN_D6_*`), resolved on the host and
    /// the root claimed for the backend ([`spawn_host::claim_root`]: a root
    /// another incompatible backend runs on is refused), once per root and
    /// process. For a process that runs workspaces only: it writes under the root.
    pub(crate) fn from_env(root: &std::path::Path) -> Result<Self, String> {
        static RESOLVED: std::sync::OnceLock<
            std::sync::Mutex<HashMap<PathBuf, Result<WorkspaceSettings, String>>>,
        > = std::sync::OnceLock::new();
        let configured = workspace_backend(env_value("SPAWN_WORKSPACE_BACKEND").as_deref())?;
        let d6 = D6RuntimeConfig::from_env()?;
        let max_file_bytes = workspace_max_file_bytes();
        let mut resolved = RESOLVED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        resolved
            .entry(root.to_path_buf())
            .or_insert_with(|| {
                let settings =
                    Self::resolve(configured, root, &Probes::host(), max_file_bytes)?.with_d6(d6);
                if let Some(existing) = spawn_host::root_backend(root).map_err(|error| {
                    format!("read workspace backend under {}: {error}", root.display())
                })? && !spawn_host::workspace::backends_compatible(existing, settings.active)
                {
                    return Err(format!(
                        "workspace root {} is claimed by {} backend, refusing {}",
                        root.display(),
                        existing.name(),
                        settings.active.name()
                    ));
                }
                if settings.active == spawn_host::WorkspaceBackend::D6 {
                    let cas_dir = settings.d6.effective_cas_dir(root);
                    spawn_host::CasMerkleStore::open(cas_dir.clone()).map_err(|error| {
                        format!("open D6 CAS store at {}: {error}", cas_dir.display())
                    })?;
                }
                settings.claim(root)?;
                Ok(settings)
            })
            .clone()
    }

    /// Claim `root` for this backend ([`spawn_host::claim_root`]).
    pub(crate) fn claim(&self, root: &std::path::Path) -> Result<(), String> {
        spawn_host::claim_root(root, self.active)
    }

    /// What `spawn mission show` says of the backend of `root`, read and
    /// never written: no root is made. The backend is the one the root's
    /// marker names ([`spawn_host::root_backend`]) or the one asked for by
    /// name; `auto` on a root no loop claimed yet is undecided (`active: null`).
    pub(crate) fn peek(root: &std::path::Path) -> serde_json::Value {
        if let Err(error) = D6RuntimeConfig::from_env() {
            return serde_json::json!({ "error": error });
        }
        Self::peek_with(
            workspace_backend(env_value("SPAWN_WORKSPACE_BACKEND").as_deref()),
            spawn_host::root_backend(root).map_err(|error| error.to_string()),
        )
    }

    /// [`Self::peek`] of the environment's answers and the root's marker.
    pub(crate) fn peek_with(
        configured: Result<WorkspaceBackend, String>,
        marked: Result<Option<spawn_host::WorkspaceBackend>, String>,
    ) -> serde_json::Value {
        let (configured, marked) = match (configured, marked) {
            (Ok(configured), Ok(marked)) => (configured, marked),
            (Err(error), _) | (_, Err(error)) => {
                return serde_json::json!({ "error": error });
            }
        };
        let named = configured.to_host();
        if let (Some(named), Some(marked)) = (named, marked)
            && !spawn_host::workspace::backends_compatible(marked, named)
        {
            return serde_json::json!({
                "error": format!(
                    "SPAWN_WORKSPACE_BACKEND={} but the root is the {} backend's: spawn loop \
                     refuses to start",
                    named.name(),
                    marked.name()
                ),
                "root": marked.name(),
            });
        }
        let active = named.or(marked);
        serde_json::json!({
            "configured": configured.name(),
            "active": active.map(spawn_host::WorkspaceBackend::name),
            "root": marked.map(spawn_host::WorkspaceBackend::name),
        })
    }

    fn build_d6_store(
        &self,
        repo: &std::path::Path,
        root: &std::path::Path,
        ledger: spawn_host::workspace::DynRefLedger,
    ) -> Result<spawn_host::LocalWorkspaceStore, String> {
        let cas_dir = self.d6.effective_cas_dir(root);
        let cas = std::sync::Arc::new(
            spawn_host::CasMerkleStore::open(&cas_dir)
                .map_err(|error| format!("open D6 CAS store at {}: {error}", cas_dir.display()))?,
        );
        let mut opts = spawn_host::workspace::D6StoreOptions::new(cas, ledger, self.max_file_bytes);
        if let Some(policy) = self.d6.l1_policy() {
            opts = opts.with_l1_policy(policy);
        }
        Ok(spawn_host::LocalWorkspaceStore::d6(
            repo.to_string_lossy().into_owned(),
            root,
            opts,
        ))
    }

    /// Construct a store bound directly to `pg` for the D6 backend.
    pub(crate) fn store_with_pg(
        &self,
        repo: &std::path::Path,
        root: &std::path::Path,
        pg: std::sync::Arc<PostgresStore>,
    ) -> Result<spawn_host::LocalWorkspaceStore, String> {
        match self.active {
            spawn_host::WorkspaceBackend::D6 => {
                let ledger: spawn_host::workspace::DynRefLedger =
                    std::sync::Arc::new(crate::sources::PostgresRefLedger::new(pg));
                self.build_d6_store(repo, root, ledger)
            }
            spawn_host::WorkspaceBackend::Git => Ok(spawn_host::LocalWorkspaceStore::git(
                repo,
                root,
                self.max_file_bytes,
            )),
        }
    }

    /// The store of this backend over the bare repository `repo`,
    /// workspaces under `root`.
    pub(crate) fn store(
        &self,
        repo: &std::path::Path,
        root: &std::path::Path,
    ) -> spawn_host::LocalWorkspaceStore {
        match self.active {
            spawn_host::WorkspaceBackend::D6 => {
                let ledger: spawn_host::workspace::DynRefLedger =
                    std::sync::Arc::new(crate::sources::LazyPgRefLedger::new());
                self.build_d6_store(repo, root, ledger)
                    .unwrap_or_else(|error| panic!("{error}"))
            }
            spawn_host::WorkspaceBackend::Git => {
                spawn_host::LocalWorkspaceStore::git(repo, root, self.max_file_bytes)
            }
        }
    }

    /// Builds this backend's store for any repository, workspaces under
    /// `root` ([`crate::sources::Factory`]).
    pub(crate) fn factory(&self, root: &std::path::Path) -> crate::sources::Factory {
        let settings = self.clone();
        let root = root.to_path_buf();
        std::sync::Arc::new(move |repo: &std::path::Path| settings.store(repo, &root))
    }

    /// The mission store over `repo` and `root` with this backend, for a
    /// one-shot change (a merge of main, a set-aside:
    /// [`crate::sources::MissionWorkspaces::one_shot`]).
    pub(crate) fn one_shot(
        &self,
        repo: &std::path::Path,
        root: &std::path::Path,
        store: std::sync::Arc<PostgresStore>,
    ) -> crate::sources::MissionWorkspaces {
        let default = match self.active {
            spawn_host::WorkspaceBackend::D6 => self
                .store_with_pg(repo, root, std::sync::Arc::clone(&store))
                .unwrap_or_else(|error| panic!("{error}")),
            spawn_host::WorkspaceBackend::Git => self.store(repo, root),
        };
        crate::sources::MissionWorkspaces::one_shot(default, root, store, self.factory(root))
    }

    /// The line `spawn loop` says at start.
    pub(crate) fn said(&self, root: &std::path::Path) -> String {
        format!(
            "workspace backend {} under {} (SPAWN_WORKSPACE_BACKEND={})",
            self.active.name(),
            root.display(),
            self.configured.name(),
        )
    }

    /// Whether this workspace backend is pure-Rust D6 (no bare Git mirror or Git push).
    pub(crate) fn is_d6(&self) -> bool {
        self.active == spawn_host::WorkspaceBackend::D6
    }
}

/// Environment variable overriding the TOML file searched for `[auth.profiles.<name>]`.
pub(crate) const AUTH_CONFIG_ENV: &str = "SPAWN_AUTH_CONFIG";

/// Environment variable overriding the root directory for isolated `native_login` profile dirs.
pub(crate) const AUTH_PROFILES_DIR_ENV: &str = "SPAWN_AUTH_PROFILES_DIR";

fn non_empty_env(env_lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env_lookup(name).filter(|value| !value.trim().is_empty())
}

fn default_auth_config_path_with(env_lookup: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(xdg) = non_empty_env(env_lookup, "XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("spawn").join("auth.toml");
    }
    if let Some(home) = non_empty_env(env_lookup, "HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("spawn")
            .join("auth.toml");
    }
    PathBuf::from(".spawn").join("auth.toml")
}

fn default_spawn_toml_path_with(env_lookup: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(xdg) = non_empty_env(env_lookup, "XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("spawn").join("spawn.toml"));
    }
    non_empty_env(env_lookup, "HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("spawn")
            .join("spawn.toml")
    })
}

/// Resolve the TOML file path used when writing `[auth.profiles.<name>]`.
///
/// Precedence: explicit `--config <path>` -> `SPAWN_AUTH_CONFIG` ->
/// `$XDG_CONFIG_HOME/spawn/auth.toml` -> `$HOME/.config/spawn/auth.toml` ->
/// `.spawn/auth.toml`.
pub(crate) fn auth_config_write_path_with(
    explicit: Option<&Path>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = non_empty_env(env_lookup, AUTH_CONFIG_ENV) {
        return PathBuf::from(path);
    }
    default_auth_config_path_with(env_lookup)
}

/// Resolve the TOML file path used when reading `[auth.profiles.<name>]`.
///
/// Precedence: explicit `--config <path>` -> `SPAWN_AUTH_CONFIG` -> existing
/// default `auth.toml` -> existing `SPAWN_CONFIG` -> existing default `spawn.toml` ->
/// default `auth.toml`.
pub(crate) fn auth_config_read_path_with(
    explicit: Option<&Path>,
    env_lookup: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = non_empty_env(env_lookup, AUTH_CONFIG_ENV) {
        return PathBuf::from(path);
    }
    let default_auth = default_auth_config_path_with(env_lookup);
    if exists(&default_auth) {
        return default_auth;
    }
    if let Some(spawn_cfg) = non_empty_env(env_lookup, "SPAWN_CONFIG").map(PathBuf::from)
        && exists(&spawn_cfg)
    {
        return spawn_cfg;
    }
    if let Some(spawn_toml) = default_spawn_toml_path_with(env_lookup)
        && exists(&spawn_toml)
    {
        return spawn_toml;
    }
    default_auth
}

/// Resolve the isolated root directory for `native_login` auth profile directories.
///
/// Precedence: `SPAWN_AUTH_PROFILES_DIR` -> `$XDG_STATE_HOME/spawn/auth-profiles` ->
/// `$XDG_DATA_HOME/spawn/auth-profiles` -> `$HOME/.local/state/spawn/auth-profiles`.
/// Refuses to fall back to shared `/tmp` directories when none of these env vars exist.
pub(crate) fn auth_profiles_root_dir_with(
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    if let Some(dir) = non_empty_env(env_lookup, AUTH_PROFILES_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(xdg) = non_empty_env(env_lookup, "XDG_STATE_HOME") {
        return Ok(PathBuf::from(xdg).join("spawn").join("auth-profiles"));
    }
    if let Some(xdg) = non_empty_env(env_lookup, "XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join("spawn").join("auth-profiles"));
    }
    if let Some(home) = non_empty_env(env_lookup, "HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("spawn")
            .join("auth-profiles"));
    }
    Err(
        "cannot resolve isolated auth profiles directory: set SPAWN_AUTH_PROFILES_DIR, \
         XDG_STATE_HOME, XDG_DATA_HOME, or HOME (shared temp fallback is refused)"
            .to_string(),
    )
}

/// Resolve [`auth_config_write_path_with`] against the process environment.
pub(crate) fn auth_config_write_path(explicit: Option<&Path>) -> PathBuf {
    auth_config_write_path_with(explicit, &|name| env_value(name))
}

/// Resolve [`auth_config_read_path_with`] against the process environment and filesystem.
pub(crate) fn auth_config_read_path(explicit: Option<&Path>) -> PathBuf {
    auth_config_read_path_with(explicit, &|name| env_value(name), &Path::exists)
}

/// Resolve [`auth_profiles_root_dir_with`] against the process environment.
pub(crate) fn auth_profiles_root_dir() -> Result<PathBuf, String> {
    auth_profiles_root_dir_with(&|name| env_value(name))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_message_bound_is_whole_seconds_and_a_bad_value_is_the_default_said() {
        use super::message_taken_bound;
        assert_eq!(message_taken_bound(None), (120_000, None));
        assert_eq!(message_taken_bound(Some("6")), (6_000, None));
        assert_eq!(message_taken_bound(Some(" 0 ")), (0, None));
        for bad in ["1.5", "-3", "two", "6s"] {
            let (bound, warning) = message_taken_bound(Some(bad));
            assert_eq!(bound, 120_000, "{bad}");
            let warning = warning.expect("a bad value is said");
            assert!(warning.contains("SPAWN_MESSAGE_TAKEN_SECS"), "{warning}");
        }
    }

    use super::*;

    #[test]
    fn spawn_program_prefers_the_environment() {
        assert_eq!(
            resolve_spawn_program(Some("/opt/spawn".into()), Some(PathBuf::from("/bin/x"))),
            "/opt/spawn"
        );
    }

    #[test]
    fn spawn_program_falls_back_to_the_current_executable() {
        assert_eq!(
            resolve_spawn_program(None, Some(PathBuf::from("/usr/local/bin/spawn"))),
            "/usr/local/bin/spawn"
        );
    }

    #[test]
    fn spawn_program_falls_back_to_the_plain_name() {
        assert_eq!(resolve_spawn_program(None, None), "spawn");
    }

    #[test]
    fn workspace_root_prefers_the_flag() {
        assert_eq!(
            resolve_workspace_root(Some("/flag".into()), Some("/env".into())).as_deref(),
            Some("/flag")
        );
        assert_eq!(
            resolve_workspace_root(None, Some("/env".into())).as_deref(),
            Some("/env")
        );
        assert_eq!(resolve_workspace_root(None, None), None);
    }

    #[test]
    fn git_versions_read_from_every_known_shape() {
        assert_eq!(git_version("git version 2.56.0\n"), Some((2, 56)));
        assert_eq!(
            git_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39))
        );
        assert_eq!(git_version("git version 2.45.1.windows.1"), Some((2, 45)));
        assert_eq!(git_version("git version 3.0"), Some((3, 0)));
        assert_eq!(git_version("hub version 2.14.2"), None);
        assert_eq!(git_version(""), None);
    }

    #[test]
    fn git_older_than_the_attr_source_option_is_refused_clearly() {
        assert!(check_git_version("git version 2.43.0").is_ok());
        assert!(check_git_version("git version 2.43.7").is_ok());
        assert!(check_git_version("git version 2.56.0").is_ok());
        assert!(check_git_version("git version 3.1.0").is_ok());
        let old = check_git_version("git version 2.42.0").expect_err("too old");
        assert!(old.contains("git 2.42 is too old"), "{old}");
        assert!(old.contains("2.43 or later"), "{old}");
        assert!(check_git_version("git version 2.41.0").is_err());
        assert!(check_git_version("git version 2.40.1").is_err());
        assert!(check_git_version("git version 2.39.5").is_err());
        assert!(old.contains("--attr-source"), "{old}");
        assert!(check_git_version("git version 2.30.2").is_err());
        assert!(check_git_version("not git").is_err());
    }

    #[test]
    fn the_git_on_this_host_is_new_enough() {
        require_git().expect("the build host runs a current git");
    }

    #[test]
    fn the_workspace_root_is_resolved_through_its_symlinks() {
        let base = std::env::temp_dir().join(format!("spawn-root-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/ws")).expect("scratch");
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).expect("link");
        let real = std::fs::canonicalize(base.join("real/ws")).expect("resolve");
        let configured = base.join("link/ws").display().to_string();
        assert_eq!(
            super::canonical_workspace_root(&configured),
            real.display().to_string()
        );
        let flags = Flags::parse(&["--root".to_string(), configured.clone()]).expect("flags");
        assert_eq!(
            workspace_root(&flags).as_deref(),
            Some(real.display().to_string().as_str())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_missing_database_url_says_where_it_comes_from() {
        let error = not_set("SPAWN_DATABASE_URL");
        assert!(
            error.starts_with("SPAWN_DATABASE_URL is not set; "),
            "{error}"
        );
        assert!(error.contains("spawn.env"), "{error}");
        assert!(error.contains("environment"), "{error}");
        assert_eq!(not_set("SPAWN_OTHER"), "SPAWN_OTHER is not set");
    }

    #[test]
    fn the_terminal_backend_is_herdr_only_and_refuses_tmux() {
        assert_eq!(check_terminal_backend(None), Ok(()));
        assert_eq!(check_terminal_backend(Some("")), Ok(()));
        assert_eq!(check_terminal_backend(Some("herdr")), Ok(()));
        assert_eq!(check_terminal_backend(Some(" herdr ")), Ok(()));
        for removed in ["tmux", " tmux "] {
            let error = check_terminal_backend(Some(removed)).expect_err(removed);
            assert!(error.contains("SPAWN_TERMINAL_BACKEND"), "{error}");
            assert!(error.contains("tmux"), "{error}");
            assert!(error.contains("removed"), "{error}");
        }
        for unknown in ["screen", "Herdr", "TMUX", "zellij"] {
            let error = check_terminal_backend(Some(unknown)).expect_err(unknown);
            assert!(error.contains("SPAWN_TERMINAL_BACKEND"), "{error}");
            assert!(error.contains(unknown), "{error}");
            assert!(error.contains("herdr (the default)"), "{error}");
        }
    }

    #[test]
    fn terminal_host_selects_core_or_legacy_herdr_modes() {
        use super::{HerdrMode, herdr_mode};
        assert_eq!(herdr_mode(None), Ok(HerdrMode::Core));
        assert_eq!(herdr_mode(Some("")), Ok(HerdrMode::Core));
        assert_eq!(herdr_mode(Some(" core ")), Ok(HerdrMode::Core));
        assert_eq!(herdr_mode(Some("legacy")), Ok(HerdrMode::Legacy));
        assert!(herdr_mode(Some("tmux")).is_err());
    }

    #[test]
    fn workspace_root_reads_the_root_flag() {
        let flags = Flags::parse(&["--root".to_string(), "/w".to_string()]).unwrap();
        assert_eq!(workspace_root(&flags).as_deref(), Some("/w"));
    }
}

/// `--name value` arguments, as every subcommand takes them, and the
/// value-less switches a subcommand declares (`--no-gate`).
#[derive(Debug, Default)]
pub(crate) struct Flags {
    values: HashMap<String, String>,
    switches: HashSet<String>,
}

impl Flags {
    pub(crate) fn parse(arguments: &[String]) -> Result<Self, String> {
        Self::parse_with_switches(arguments, &[])
    }

    /// As [`Flags::parse`], except that the names in `switches` take no
    /// value: a switch followed by a word that is not a flag is refused
    /// (`--no-gate x`), and every other flag still needs its value.
    pub(crate) fn parse_with_switches(
        arguments: &[String],
        switches: &[&str],
    ) -> Result<Self, String> {
        let mut flags = Self::default();
        let mut rest = arguments.iter().peekable();
        while let Some(word) = rest.next() {
            let Some(name) = word.strip_prefix("--") else {
                return Err(format!("unexpected argument {word}"));
            };
            if switches.contains(&name) {
                if let Some(next) = rest.peek()
                    && !next.starts_with("--")
                {
                    return Err(format!("--{name} takes no value, found {next}"));
                }
                flags.switches.insert(name.to_string());
                continue;
            }
            let Some(next) = rest.next() else {
                return Err(format!("--{name} needs a value"));
            };
            if next.starts_with("--") {
                return Err(format!("--{name} needs a value, found {next}"));
            }
            flags.values.insert(name.to_string(), next.clone());
        }
        Ok(flags)
    }

    /// Whether the switch `name` was given.
    pub(crate) fn switch(&self, name: &str) -> bool {
        self.switches.contains(name)
    }

    pub(crate) fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    /// The names of the flags given (values only, not switches), in no
    /// order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    pub(crate) fn required(&self, name: &str) -> Result<String, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }
}

/// Rejects removed (`tmux`) or unknown `SPAWN_TERMINAL_BACKEND` values; only
/// unset, empty, or `"herdr"` is accepted.
fn check_terminal_backend(value: Option<&str>) -> Result<(), String> {
    match value.map(str::trim) {
        None | Some("" | "herdr") => Ok(()),
        Some("tmux") => Err(
            "SPAWN_TERMINAL_BACKEND=\"tmux\" is no longer supported: tmux has been removed; \
             use herdr (the default)"
                .to_string(),
        ),
        Some(other) => Err(format!(
            "SPAWN_TERMINAL_BACKEND={other:?} names no terminal backend: only herdr (the default) \
             is supported"
        )),
    }
}

/// How many Herdr server shards `SPAWN_HERDR_SHARDS` configures (`1` when unset
/// or empty; `1..=spawn_host::herdr::MAX_SHARDS`).
pub(crate) fn herdr_shards(value: Option<&str>) -> Result<usize, String> {
    let Some(text) = value.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(1);
    };
    match text.parse::<usize>() {
        Ok(shards) if (1..=spawn_host::herdr::MAX_SHARDS).contains(&shards) => Ok(shards),
        _ => Err(format!(
            "SPAWN_HERDR_SHARDS={text:?} must be a whole number from 1 to {}",
            spawn_host::herdr::MAX_SHARDS
        )),
    }
}

/// Which Herdr implementation backs `SPAWN_TERMINAL_BACKEND=herdr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HerdrMode {
    /// Embedded `herdr-core` daemon via `spawn-hc` (the default).
    Core,
    /// Explicit fallback to external legacy `herdr 0.9.5`.
    Legacy,
}

/// Selects [`HerdrMode`] from `SPAWN_HERDR_MODE` (`core` by default).
pub(crate) fn herdr_mode(value: Option<&str>) -> Result<HerdrMode, String> {
    match value.map(str::trim) {
        None | Some("" | "core") => Ok(HerdrMode::Core),
        Some("legacy") => Ok(HerdrMode::Legacy),
        Some(other) => Err(format!(
            "SPAWN_HERDR_MODE={other:?} names no herdr engine: use core (the default) or legacy"
        )),
    }
}

fn sibling_spawn_hc() -> String {
    if let Some(program) = env_value("SPAWN_HERDR_CORE_PROGRAM") {
        return program;
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in [exe.parent(), exe.parent().and_then(std::path::Path::parent)]
            .into_iter()
            .flatten()
        {
            let candidate = dir.join("spawn-hc");
            if candidate.is_file() {
                return candidate.display().to_string();
            }
        }
    }
    "spawn-hc".to_string()
}

/// The terminal host the environment describes: embedded `herdr-core` via
/// `spawn-hc` by default (or external legacy `herdr 0.9.5` with
/// `SPAWN_HERDR_MODE=legacy`) on the session `SPAWN_HERDR_SESSION` (`spawn` by
/// default), sharded across `SPAWN_HERDR_SHARDS` independent servers when
/// greater than 1.
pub(crate) fn terminal_host() -> Result<spawn_host::LocalTerminalHost, String> {
    check_terminal_backend(env_value("SPAWN_TERMINAL_BACKEND").as_deref())?;
    let mode = herdr_mode(env_value("SPAWN_HERDR_MODE").as_deref())?;
    let session = env_or("SPAWN_HERDR_SESSION", spawn_host::herdr::DEFAULT_SESSION);
    let shards = herdr_shards(env_value("SPAWN_HERDR_SHARDS").as_deref())?;
    let host_env = terminal_host_env();
    Ok(match (mode, shards) {
        (HerdrMode::Core, 1) => spawn_host::LocalTerminalHost::HerdrCore(
            spawn_host::HerdrCoreHost::on_session(&sibling_spawn_hc(), &session, host_env)
                .map_err(|error| format!("SPAWN_HERDR_SESSION: {}", error.0))?,
        ),
        (HerdrMode::Core, count) => spawn_host::LocalTerminalHost::HerdrPool(
            spawn_host::HerdrPool::on_core_session(&sibling_spawn_hc(), &session, count, &host_env)
                .map_err(|error| format!("SPAWN_HERDR_SESSION: {}", error.0))?,
        ),
        (HerdrMode::Legacy, 1) => spawn_host::LocalTerminalHost::Herdr(
            spawn_host::HerdrHost::on_session(
                &env_or("SPAWN_HERDR_PROGRAM", "herdr"),
                &session,
                host_env,
            )
            .map_err(|error| format!("SPAWN_HERDR_SESSION: {}", error.0))?,
        ),
        (HerdrMode::Legacy, count) => spawn_host::LocalTerminalHost::HerdrPool(
            spawn_host::HerdrPool::on_session(
                &env_or("SPAWN_HERDR_PROGRAM", "herdr"),
                &session,
                count,
                &host_env,
            )
            .map_err(|error| format!("SPAWN_HERDR_SESSION: {}", error.0))?,
        ),
    })
}

#[cfg(test)]
mod workspace_tests {
    use super::{Probes, WorkspaceBackend, WorkspaceSettings, workspace_backend};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    #[allow(clippy::unnecessary_wraps)] // the probe's signature
    fn unmarked(_: &Path) -> Result<Option<spawn_host::WorkspaceBackend>, String> {
        Ok(None)
    }

    fn probes<'a>() -> Probes<'a> {
        Probes { marked: &unmarked }
    }

    #[test]
    fn the_workspace_backend_defaults_to_git() {
        use spawn_host::WorkspaceBackend as HostBackend;

        assert_eq!(
            workspace_backend(None),
            Ok(WorkspaceBackend::Named(HostBackend::Git))
        );
        assert_eq!(
            workspace_backend(Some("")),
            Ok(WorkspaceBackend::Named(HostBackend::Git))
        );
        assert_eq!(
            workspace_backend(Some("git")),
            Ok(WorkspaceBackend::Named(HostBackend::Git))
        );
        assert_eq!(
            workspace_backend(Some(" d6 ")),
            Ok(WorkspaceBackend::Named(HostBackend::D6))
        );
        assert_eq!(workspace_backend(Some("auto")), Ok(WorkspaceBackend::Auto));
        for retired in ["cow", "overlay", "d6-shadow"] {
            let error = workspace_backend(Some(retired)).expect_err(retired);
            assert!(error.contains("SPAWN_WORKSPACE_BACKEND"), "{error}");
            assert!(error.contains("retired"), "{error}");
            assert!(error.contains("git (the default), d6 or auto"), "{error}");
        }
        for unknown in ["Git", "COW", "btrfs", "zfs", "overlayfs", "d6_shadow", "1"] {
            let error = workspace_backend(Some(unknown)).expect_err(unknown);
            assert!(error.contains("SPAWN_WORKSPACE_BACKEND"), "{error}");
            assert!(error.contains("git (the default), d6 or auto"), "{error}");
        }
    }

    /// `auto` is spawn-host's rule: Git on clean roots, or existing root marker when present.
    #[test]
    fn auto_selects_git_on_clean_roots() {
        let root = Path::new("/srv/work");
        let cap = spawn_host::DEFAULT_MAX_FILE_BYTES;
        let auto =
            WorkspaceSettings::resolve(WorkspaceBackend::Auto, root, &probes(), cap).unwrap();
        assert_eq!(auto.active, spawn_host::WorkspaceBackend::Git);
        assert_eq!(auto.active, spawn_host::WorkspaceBackend::auto(root));

        for (configured, backend) in [
            (
                WorkspaceBackend::Named(spawn_host::WorkspaceBackend::Git),
                spawn_host::WorkspaceBackend::Git,
            ),
            (
                WorkspaceBackend::Named(spawn_host::WorkspaceBackend::D6),
                spawn_host::WorkspaceBackend::D6,
            ),
        ] {
            let settings = WorkspaceSettings::resolve(configured, root, &probes(), cap).unwrap();
            assert_eq!(settings.active, backend);
        }
    }

    #[test]
    fn the_settings_are_said_and_build_matching_stores() {
        let root = Path::new("/srv/work");
        let cap = spawn_host::DEFAULT_MAX_FILE_BYTES;
        let auto =
            WorkspaceSettings::resolve(WorkspaceBackend::Auto, root, &probes(), cap).unwrap();
        assert_eq!(
            auto.said(root),
            "workspace backend git under /srv/work (SPAWN_WORKSPACE_BACKEND=auto)"
        );
        let git = WorkspaceSettings::resolve(
            WorkspaceBackend::Named(spawn_host::WorkspaceBackend::Git),
            root,
            &probes(),
            cap,
        )
        .unwrap();
        let d6 = WorkspaceSettings::resolve(
            WorkspaceBackend::Named(spawn_host::WorkspaceBackend::D6),
            root,
            &probes(),
            cap,
        )
        .unwrap();
        assert_eq!(
            d6.said(root),
            "workspace backend d6 under /srv/work (SPAWN_WORKSPACE_BACKEND=d6)"
        );
        assert_eq!(git.store(Path::new("/r.git"), root).backend_name(), "git");
        assert_eq!(auto.store(Path::new("/r.git"), root).backend_name(), "git");
        let store_root = std::env::temp_dir().join(format!(
            "spawn-settings-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&store_root);
        std::fs::create_dir_all(&store_root).expect("create temp store root");
        assert_eq!(
            d6.store(Path::new("/r.git"), &store_root).backend_name(),
            "d6"
        );
        let _ = std::fs::remove_dir_all(&store_root);
    }

    /// A read of the settings: from the environment and the root's marker
    /// alone, a conflict said as the loop would refuse it.
    #[test]
    fn a_peek_reads_the_marker_and_names_a_conflict() {
        use spawn_host::WorkspaceBackend as HostBackend;

        let peek = WorkspaceSettings::peek_with;
        assert_eq!(
            peek(Ok(WorkspaceBackend::Named(HostBackend::Git)), Ok(None)),
            json!({"configured": "git", "active": "git", "root": null})
        );
        assert_eq!(
            peek(Ok(WorkspaceBackend::Auto), Ok(None)),
            json!({"configured": "auto", "active": null, "root": null})
        );
        assert_eq!(
            peek(Ok(WorkspaceBackend::Auto), Ok(Some(HostBackend::D6))),
            json!({"configured": "auto", "active": "d6", "root": "d6"})
        );
        let conflict = peek(
            Ok(WorkspaceBackend::Named(HostBackend::D6)),
            Ok(Some(HostBackend::Git)),
        );
        assert!(
            conflict["error"]
                .as_str()
                .unwrap()
                .contains("git backend's"),
            "{conflict}"
        );
        let bad = peek(
            Err("SPAWN_WORKSPACE_BACKEND=\"x\" ...".to_string()),
            Ok(None),
        );
        assert_eq!(bad.as_object().map(serde_json::Map::len), Some(1));
    }

    /// `auto` on a root that already is a backend's follows it: a Git-shaped
    /// root with no marker (its workspaces, `.transcripts/`) stays Git's, as
    /// `spawn mission show` says.
    #[test]
    fn auto_follows_an_existing_roots_engine() {
        let base = std::env::temp_dir().join(format!(
            "spawn-auto-existing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("owner-gates")).expect("a workspace");
        std::fs::create_dir_all(base.join(".transcripts")).expect("transcripts");
        let host = Probes::host();
        let settings = WorkspaceSettings::resolve(
            WorkspaceBackend::Auto,
            &base,
            &host,
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        )
        .unwrap();
        assert_eq!(settings.active, spawn_host::WorkspaceBackend::Git);
        settings.claim(&base).expect("Git's root claimed for Git");
        let peek = WorkspaceSettings::peek_with(
            Ok(WorkspaceBackend::Auto),
            spawn_host::root_backend(&base).map_err(|error| error.to_string()),
        );
        assert_eq!(peek["active"], "git", "{peek}");

        // A claimed D6 root: auto follows the marker.
        let d6_root = base.join("d6-root");
        spawn_host::claim_root(&d6_root, spawn_host::WorkspaceBackend::D6).unwrap();
        let settings = WorkspaceSettings::resolve(
            WorkspaceBackend::Auto,
            &d6_root,
            &host,
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        )
        .unwrap();
        assert_eq!(settings.active, spawn_host::WorkspaceBackend::D6);

        // An empty clean root: auto selects Git until explicit D6 cutover/opt-in.
        let clean_root = base.join("clean-root");
        std::fs::create_dir_all(&clean_root).expect("empty clean root");
        let clean_settings = WorkspaceSettings::resolve(
            WorkspaceBackend::Auto,
            &clean_root,
            &host,
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        )
        .unwrap();
        assert_eq!(clean_settings.active, spawn_host::WorkspaceBackend::Git);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn herdr_shards_parses_bounds_and_defaults() {
        use super::herdr_shards;
        assert_eq!(herdr_shards(None), Ok(1));
        assert_eq!(herdr_shards(Some("")), Ok(1));
        assert_eq!(herdr_shards(Some(" 16 ")), Ok(16));
        assert!(herdr_shards(Some("0")).is_err());
        assert!(herdr_shards(Some("65")).is_err());
        assert!(herdr_shards(Some("abc")).is_err());
    }

    #[test]
    fn herdr_mode_defaults_to_core_and_allows_legacy() {
        use super::{HerdrMode, herdr_mode};
        assert_eq!(herdr_mode(None), Ok(HerdrMode::Core));
        assert_eq!(herdr_mode(Some("")), Ok(HerdrMode::Core));
        assert_eq!(herdr_mode(Some(" core ")), Ok(HerdrMode::Core));
        assert_eq!(herdr_mode(Some("legacy")), Ok(HerdrMode::Legacy));
        assert!(herdr_mode(Some("other")).is_err());
    }

    #[test]
    fn d6_runtime_config_parses_and_validates_all_fields() {
        use super::{D6RuntimeConfig, DEFAULT_D6_TMPFS_BUDGET_MB};

        let def = D6RuntimeConfig::parse(None, None, None).expect("default");
        assert_eq!(def, D6RuntimeConfig::default());
        assert_eq!(
            def.effective_cas_dir(Path::new("/srv/work")),
            PathBuf::from("/srv/work/.d6/cas")
        );
        assert!(def.l1_policy().is_none());

        let configured =
            D6RuntimeConfig::parse(Some(" /var/lib/d6-cas "), Some("/dev/shm/d6-l1"), None)
                .expect("configured");
        assert_eq!(configured.cas_dir, Some(PathBuf::from("/var/lib/d6-cas")));
        assert_eq!(configured.tmpfs_root, Some(PathBuf::from("/dev/shm/d6-l1")));
        let l1 = configured.l1_policy().expect("l1 policy");
        assert_eq!(l1.tmpfs_budget_mb, DEFAULT_D6_TMPFS_BUDGET_MB);

        let custom =
            D6RuntimeConfig::parse(None, Some("/dev/shm/d6-l1"), Some("4096")).expect("custom");
        assert_eq!(custom.l1_policy().expect("l1").tmpfs_budget_mb, 4096);

        assert!(D6RuntimeConfig::parse(Some("relative/cas"), None, None).is_err());
        assert!(D6RuntimeConfig::parse(None, Some("relative/tmpfs"), None).is_err());
        assert!(D6RuntimeConfig::parse(None, None, Some("0")).is_err());
        assert!(D6RuntimeConfig::parse(None, None, Some("abc")).is_err());
    }

    #[test]
    fn config_origin_and_secret_string_behavior() {
        use super::{SecretString, resolve_spawn_program, resolve_workspace_root};
        use crate::config_file::{ConfigOrigin, SpawnConfig};

        assert_eq!(
            resolve_spawn_program(Some("/opt/spawn".into()), None),
            "/opt/spawn"
        );
        assert_eq!(resolve_spawn_program(None, None), "spawn");
        assert_eq!(
            resolve_workspace_root(Some("/flag".into()), Some("/env".into())).as_deref(),
            Some("/flag")
        );
        assert_eq!(
            resolve_workspace_root(None, Some("/env".into())).as_deref(),
            Some("/env")
        );
        assert_eq!(ConfigOrigin::File.as_str(), "file");
        assert_eq!(ConfigOrigin::Credential.as_str(), "credential");

        let cfg = SpawnConfig::from_toml_str_with(
            "schema_version = 1\n[instance]\ndata_dir = \"/srv/spawn\"\n",
            &|_| None,
        )
        .expect("parse");
        let origins = cfg.field_origins();
        assert_eq!(origins.get("instance"), Some(&ConfigOrigin::File));
        assert_eq!(origins.get("database"), Some(&ConfigOrigin::Default));

        let secret = SecretString::new("TopSecretValue_001");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
        assert_eq!(secret.expose_secret(), "TopSecretValue_001");
        assert_eq!(secret.into_inner(), "TopSecretValue_001");
    }

    #[test]
    fn d6_compatibility_in_resolve_and_peek() {
        use spawn_host::WorkspaceBackend as HostBackend;

        let peek = WorkspaceSettings::peek_with;
        let refused_d6 = peek(
            Ok(WorkspaceBackend::Named(HostBackend::D6)),
            Ok(Some(HostBackend::Git)),
        );
        assert!(
            refused_d6["error"]
                .as_str()
                .unwrap()
                .contains("git backend's"),
            "{refused_d6}"
        );
        // pure d6 root refuses git
        assert!(
            peek(
                Ok(WorkspaceBackend::Named(HostBackend::Git)),
                Ok(Some(HostBackend::D6))
            )["error"]
                .is_string()
        );
        // resolve d6 / git succeed
        for (cfg, eng) in [
            (WorkspaceBackend::Named(HostBackend::D6), HostBackend::D6),
            (WorkspaceBackend::Named(HostBackend::Git), HostBackend::Git),
        ] {
            let s = WorkspaceSettings::resolve(
                cfg,
                Path::new("/srv/w"),
                &probes(),
                spawn_host::DEFAULT_MAX_FILE_BYTES,
            )
            .expect("resolve");
            assert_eq!(s.active, eng);
        }
    }

    #[test]
    fn load_credential_supports_valid_files_absent_fallback_and_fail_closed_checks() {
        use super::{SecretString, load_credential_file, load_credential_secret};
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::storage::tests::scratch("config-load-credential");
        assert_eq!(load_credential_file(None, "spawn.database-url"), Ok(None));
        assert!(
            load_credential_secret(Some(&dir), "spawn.database-url")
                .unwrap()
                .is_none()
        );

        let canary = "canary-cred-val-8391";
        let valid_0400 = dir.join("spawn.database-url");
        std::fs::write(&valid_0400, format!("{canary}\n")).expect("write 0400");
        std::fs::set_permissions(&valid_0400, std::fs::Permissions::from_mode(0o400))
            .expect("chmod 400");
        assert_eq!(
            load_credential_file(Some(&dir), "spawn.database-url").unwrap(),
            Some(valid_0400.clone())
        );
        assert_eq!(
            load_credential_secret(Some(&dir), "spawn.database-url")
                .unwrap()
                .map(SecretString::into_inner),
            Some(canary.to_string())
        );

        let valid_0600 = dir.join("backup.key.k1");
        std::fs::write(&valid_0600, format!("{canary}\r\n")).expect("write 0600");
        std::fs::set_permissions(&valid_0600, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
        assert_eq!(
            load_credential_secret(Some(&dir), "backup.key.k1")
                .unwrap()
                .map(SecretString::into_inner),
            Some(canary.to_string())
        );

        let mut errs = Vec::new();
        for (name, bytes, mode) in [
            ("empty.cred", b"".as_slice(), 0o600),
            ("nl.cred", b"\n", 0o600),
            ("multi.cred", b"canary-one\ncanary-two\n", 0o600),
            ("badutf8.cred", &[0xff, 0xfe], 0o600),
            ("leaky.cred", format!("{canary}\n").as_bytes(), 0o640),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, bytes).expect("write bad");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
            errs.push(load_credential_secret(Some(&dir), name).unwrap_err());
        }

        let target = dir.join("real.target");
        std::fs::write(&target, canary).expect("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let link = dir.join("sym.cred");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        errs.push(load_credential_file(Some(&dir), "sym.cred").unwrap_err());
        errs.push(load_credential_secret(Some(&dir), "sym.cred").unwrap_err());

        for err in &errs {
            assert!(err.starts_with("credential "), "{err}");
            assert!(!err.contains(canary), "secret leaked in {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_config_and_profile_paths_follow_precedence() {
        use super::{
            AUTH_CONFIG_ENV, AUTH_PROFILES_DIR_ENV, auth_config_read_path_with,
            auth_config_write_path_with, auth_profiles_root_dir_with,
        };
        use std::collections::HashMap;

        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            move |name: &str| map.get(name).cloned()
        };

        let explicit = Path::new("/etc/spawn/custom-auth.toml");
        let full_env = env(&[
            (AUTH_CONFIG_ENV, "/srv/auth.toml"),
            ("XDG_CONFIG_HOME", "/home/u/.xdg-cfg"),
            ("HOME", "/home/u"),
            ("SPAWN_CONFIG", "/etc/spawn/spawn.toml"),
            (AUTH_PROFILES_DIR_ENV, "/var/lib/spawn/auth-profiles"),
            ("XDG_DATA_HOME", "/home/u/.xdg-data"),
        ]);

        assert_eq!(
            auth_config_write_path_with(Some(explicit), &full_env),
            explicit
        );
        assert_eq!(
            auth_config_write_path_with(None, &full_env),
            PathBuf::from("/srv/auth.toml")
        );

        let xdg_only = env(&[("XDG_CONFIG_HOME", "/home/u/.xdg-cfg"), ("HOME", "/home/u")]);
        assert_eq!(
            auth_config_write_path_with(None, &xdg_only),
            PathBuf::from("/home/u/.xdg-cfg/spawn/auth.toml")
        );

        let home_only = env(&[
            ("HOME", "/home/u"),
            ("SPAWN_CONFIG", "/etc/spawn/spawn.toml"),
        ]);
        assert_eq!(
            auth_config_write_path_with(None, &home_only),
            PathBuf::from("/home/u/.config/spawn/auth.toml")
        );

        // Read falls back to existing SPAWN_CONFIG if default auth.toml does not exist.
        let spawn_cfg_exists = |p: &Path| p == Path::new("/etc/spawn/spawn.toml");
        assert_eq!(
            auth_config_read_path_with(None, &home_only, &spawn_cfg_exists),
            PathBuf::from("/etc/spawn/spawn.toml")
        );
        // Read prefers existing default auth.toml over SPAWN_CONFIG.
        let both_exist = |p: &Path| {
            p == Path::new("/home/u/.config/spawn/auth.toml")
                || p == Path::new("/etc/spawn/spawn.toml")
        };
        assert_eq!(
            auth_config_read_path_with(None, &home_only, &both_exist),
            PathBuf::from("/home/u/.config/spawn/auth.toml")
        );

        assert_eq!(
            auth_profiles_root_dir_with(&full_env).unwrap(),
            PathBuf::from("/var/lib/spawn/auth-profiles")
        );
        let xdg_state = env(&[
            ("XDG_STATE_HOME", "/home/u/.xdg-state"),
            ("XDG_DATA_HOME", "/home/u/.xdg-data"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            auth_profiles_root_dir_with(&xdg_state).unwrap(),
            PathBuf::from("/home/u/.xdg-state/spawn/auth-profiles")
        );
        let xdg_data = env(&[("XDG_DATA_HOME", "/home/u/.xdg-data"), ("HOME", "/home/u")]);
        assert_eq!(
            auth_profiles_root_dir_with(&xdg_data).unwrap(),
            PathBuf::from("/home/u/.xdg-data/spawn/auth-profiles")
        );
        let home_data = env(&[("HOME", "/home/u")]);
        assert_eq!(
            auth_profiles_root_dir_with(&home_data).unwrap(),
            PathBuf::from("/home/u/.local/state/spawn/auth-profiles")
        );
        let empty_env = env(&[]);
        assert!(
            auth_profiles_root_dir_with(&empty_env)
                .unwrap_err()
                .contains("refused")
        );
    }

    #[test]
    fn feature_coverage_source_resolves_snapshot_before_live_repo() {
        use super::FeatureCoverageSource;
        use std::collections::HashMap;

        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            move |name: &str| map.get(name).cloned()
        };
        let cwd = Path::new("/srv/cwd");

        assert_eq!(
            FeatureCoverageSource::resolve_with(
                env(&[
                    ("SPAWN_FEATURE_COVERAGE_SNAPSHOT", "/var/lib/spawn/cov.json"),
                    ("SPAWN_FEATURE_ROOT", "/srv/spawn"),
                    ("SPAWN_FEATURE_REGISTRY", "/srv/spawn/custom.json"),
                ]),
                cwd,
            ),
            FeatureCoverageSource::PrecomputedSnapshot {
                snapshot_path: PathBuf::from("/var/lib/spawn/cov.json"),
            }
        );

        assert_eq!(
            FeatureCoverageSource::resolve_with(
                env(&[
                    ("SPAWN_FEATURE_COVERAGE_SNAPSHOT", "   "),
                    ("SPAWN_FEATURE_ROOT", "/srv/spawn"),
                    ("SPAWN_FEATURE_REGISTRY", "/srv/spawn/custom.json"),
                ]),
                cwd,
            ),
            FeatureCoverageSource::LiveRepo {
                root: PathBuf::from("/srv/spawn"),
                registry: PathBuf::from("/srv/spawn/custom.json"),
            }
        );

        assert_eq!(
            FeatureCoverageSource::resolve_with(env(&[("SPAWN_FEATURE_ROOT", "/srv/repo")]), cwd),
            FeatureCoverageSource::LiveRepo {
                root: PathBuf::from("/srv/repo"),
                registry: PathBuf::from("/srv/repo/docs/feature-map/registry.json"),
            }
        );

        assert_eq!(
            FeatureCoverageSource::resolve_with(env(&[]), cwd),
            FeatureCoverageSource::LiveRepo {
                root: PathBuf::from("/srv/cwd"),
                registry: PathBuf::from("/srv/cwd/docs/feature-map/registry.json"),
            }
        );
    }

    #[test]
    fn canonical_spawn_mission_repo_follows_precedence_and_rejects_relative() {
        use super::canonical_spawn_mission_repo_with;
        use std::collections::HashMap;

        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            move |name: &str| map.get(name).cloned()
        };

        assert_eq!(
            canonical_spawn_mission_repo_with(env(&[
                ("SPAWN_MISSION_REPO", "  /srv/custom/spawn.git  "),
                ("SPAWN_LIVE", "/srv/live"),
                ("HOME", "/home/u"),
            ])),
            Some(PathBuf::from("/srv/custom/spawn.git"))
        );
        assert_eq!(
            canonical_spawn_mission_repo_with(env(&[
                ("SPAWN_LIVE", "/srv/live"),
                ("SPAWN_RSI_DIR", "/srv/rsi"),
                ("HOME", "/home/u"),
            ])),
            Some(PathBuf::from("/srv/live/missions/spawn.git"))
        );
        assert_eq!(
            canonical_spawn_mission_repo_with(env(&[
                ("SPAWN_RSI_DIR", "/srv/rsi"),
                ("SPAWN_FEATURE_ROOT", "/srv/ctl/controllers/spawn"),
            ])),
            Some(PathBuf::from("/srv/rsi/missions/spawn.git"))
        );
        assert_eq!(
            canonical_spawn_mission_repo_with(env(&[(
                "SPAWN_FEATURE_ROOT",
                "/var/lib/spawn-rsi/controllers/spawn/",
            )])),
            Some(PathBuf::from("/var/lib/spawn-rsi/missions/spawn.git"))
        );
        // Non-controllers/spawn feature root falls back to HOME.
        assert_eq!(
            canonical_spawn_mission_repo_with(env(&[
                ("SPAWN_FEATURE_ROOT", "/home/u/src/spawn"),
                ("HOME", "/home/u"),
            ])),
            Some(PathBuf::from(
                "/home/u/.local/share/spawn-rsi/missions/spawn.git"
            ))
        );
        // Relative paths are ignored.
        assert_eq!(
            canonical_spawn_mission_repo_with(env(&[
                ("SPAWN_MISSION_REPO", "relative/spawn.git"),
                ("SPAWN_LIVE", "relative/live"),
                ("HOME", "/home/u"),
            ])),
            Some(PathBuf::from(
                "/home/u/.local/share/spawn-rsi/missions/spawn.git"
            ))
        );
        assert_eq!(canonical_spawn_mission_repo_with(env(&[])), None);
    }
}

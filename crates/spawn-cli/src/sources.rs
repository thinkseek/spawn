//! Workspaces for missions, not repositories.
//!
//! The Bridge asks for a workspace by branch, which is the agent id. This
//! store looks up the agent's mission and materializes the workspace from
//! that mission's source: a git repository (path or URL), mirrored once
//! into `<root>/.sources/`, or an empty scratch repository when the mission
//! names none. Agents no mission owns (the supervisor, the RSI agent, ad hoc
//! workers) use the default repository.
//!
//! A mirror follows its source: before a new agent's branch is created,
//! `main` is fetched again, so a worker starts from the source as it is now
//! (a merge that landed since is in), and before a merge of `main` at most
//! every [`REFRESH_EVERY`] (docs/storage.md, "Main at a start or resume" and
//! "Main into a workspace").
//!
//! Every checkpoint's branch is published, in the background
//! ([`crate::push`]), to the source's `spawn/<agent>` when the source is a
//! directory on this host ([`receives_branches`]); the integrator lands what
//! was published there, never a mounted workspace. Every store of a process
//! pushes through the process's one pusher per source
//! ([`crate::push::shared`]). Its queue is in memory: a process that ends
//! cleanly pushes what waits ([`Drop`] of the store the Bridge runs on), a
//! new one queues again the branch of every agent with an open task
//! ([`MissionWorkspaces::republish`]; a source that is away then is queued
//! too and retried until it is back), and the integrator pushes again a
//! branch whose recorded checkpoint the source still lacks
//! ([`push_recorded`]), so a push lost with a killed process is made after
//! the restart. A one-shot store (a merge of main, a set-aside;
//! [`MissionWorkspaces::one_shot`]) publishes what it changed itself
//! ([`publish`]) and leaves the rest of the queue alone. A branch lives on
//! this host: nothing is taken from the source but `main`.
//!
//! A source is git run as the owner, so it is checked before git sees it
//! ([`MissionSource`]: an absolute path, `https://` or `ssh://`), and every
//! git the controller runs allows only those transports
//! ([`controller_git`]), whatever the host's git or its configuration
//! would allow.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spawn_cas::{Digest, GitOid};
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, Hold, SetAside, WorkspaceError, WorkspaceStore};
use spawn_host::d6_workspace::D6Workspace;
use spawn_host::workspace::DynRefLedger;
use spawn_host::{
    CasMerkleStore, D6RefKind, D6StoreError, LedgerRef, LedgerSnapshot, LocalWorkspaceStore,
    RefLedger, WorkspaceBackend as HostWorkspaceBackend,
};
use spawn_postgres::{PostgresStore, RefKind};

use crate::push::{Pushed, Pusher, pushed_ref};

/// Least time between two refreshes of one mirror's `main` at turn ends.
pub(crate) const REFRESH_EVERY: Duration = Duration::from_secs(30);

/// Builds a store for one bare repository: every store of a process is
/// the one backend `SPAWN_WORKSPACE_BACKEND` chose for the root
/// ([`crate::config::WorkspaceSettings`]).
pub(crate) type Factory = Arc<dyn Fn(&Path) -> LocalWorkspaceStore + Send + Sync>;

/// Workspace store that routes every agent to its mission's source.
pub(crate) struct MissionWorkspaces {
    default: Arc<LocalWorkspaceStore>,
    root: PathBuf,
    store: Arc<PostgresStore>,
    factory: Factory,
    mirrors: Mutex<HashMap<String, Routed>>,
    /// When each mirror's `main` was last fetched.
    refreshed: Mutex<HashMap<PathBuf, Instant>>,
    /// A store for one change ([`Self::one_shot`]): it queues no push and
    /// flushes nothing when dropped; its caller publishes what it changed.
    one_shot: bool,
}

/// One agent's store and, for a mission with a source, where its mirror
/// lives and what it mirrors.
#[derive(Clone)]
struct Routed {
    store: Arc<LocalWorkspaceStore>,
    mirror: Option<(PathBuf, String)>,
}

fn unavailable(text: String) -> WorkspaceError {
    WorkspaceError::Unavailable(text)
}

/// Transports the controller's git may use: local paths, https and ssh.
/// `ext::` (a command), `fd::` and anything a later git adds are refused by
/// git itself, before any helper runs.
pub(crate) const ALLOWED_PROTOCOLS: &str = "file:https:ssh";

/// `git`, as every controller-side call runs it: in the C locale
/// ([`spawn_host::git_command`]), so what it answers reads the same on a
/// host whose locale is translated; only [`ALLOWED_PROTOCOLS`], whatever
/// the installed git defaults to or the host's configuration allows
/// (`GIT_ALLOW_PROTOCOL` overrides `protocol.*.allow`); and no standard
/// input.
pub(crate) fn controller_git() -> Command {
    let mut command = spawn_host::git_command("git");
    command
        .env("GIT_ALLOW_PROTOCOL", ALLOWED_PROTOCOLS)
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .stdin(Stdio::null());
    command
}

/// A mission's source as git may be given it: an absolute path, an
/// `https://` or `ssh://` URL, or ssh's `user@host:path`. Nothing that names
/// another transport (`ext::`, `fd::`, `file://`), nothing relative, nothing
/// that begins like an option; no whitespace or control character in a URL.
/// `spawn_source_allowed` in the schema is the same rule of shape, where a
/// mission is added.
///
/// Objective 171 adds two rules of Spawn's own, before the shape: an
/// `https://` or `ssh://` URL whose port is not digits from 1 to 65535 is
/// refused ([`port_is_digits`]; no git can fetch it, and a password written
/// where the port goes would otherwise be stored), and so is one carrying
/// credentials (any userinfo in an `https://` URL, a password in an
/// `ssh://` one: [`carries_credentials`]). No refusal repeats a URL-shaped
/// text: each says a fixed explanation and what the text is
/// ([`described`]); only an absolute path with no `:` and no `@` is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissionSource(String);

impl MissionSource {
    /// Longest source accepted.
    const MAX_LEN: usize = 4096;

    /// `text` as a source, or why it is not one.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let refused = || {
            format!(
                "a source is an absolute path or an https:// or ssh:// URL, not {}",
                described(text)
            )
        };
        if text.len() > Self::MAX_LEN {
            return Err(format!(
                "a source is at most {} bytes, not {}",
                Self::MAX_LEN,
                text.len()
            ));
        }
        if let Some(authority) = authority(text) {
            if !port_is_digits(authority) {
                return Err(format!(
                    "a source URL's port is digits from 1 to 65535, after the host: not {}",
                    described(text)
                ));
            }
            if carries_credentials(text, authority) {
                return Err(format!(
                    "a source URL carries credentials (its userinfo, before the host's @): give \
                     git its credentials through a credential helper or an ssh key, not in the \
                     URL ({})",
                    described(text)
                ));
            }
        }
        let plain = |rest: &str| {
            !rest.is_empty() && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
        };
        let lead =
            |rest: &str, banned: &[char]| rest.chars().next().is_some_and(|c| !banned.contains(&c));
        if text.is_empty() {
            return Err(refused());
        }
        let allowed = if text.starts_with('/') {
            !text.chars().any(char::is_control)
        } else if let Some(rest) = text
            .strip_prefix("https://")
            .or_else(|| text.strip_prefix("ssh://"))
        {
            plain(rest) && lead(rest, &['-', '/'])
        } else if let Some((user, rest)) = text.split_once('@')
            && let Some((host, path)) = rest.split_once(':')
        {
            let name = |part: &str, extra: &[char]| {
                part.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
                    && part.chars().all(|c| {
                        c.is_ascii_alphanumeric() || c == '.' || c == '-' || extra.contains(&c)
                    })
            };
            name(user, &['_']) && name(host, &[]) && plain(path) && lead(path, &['-', ':'])
        } else {
            false
        };
        if allowed {
            Ok(Self(text.to_string()))
        } else {
            Err(refused())
        }
    }

    /// The source as git is given it.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The authority of an `https://` or `ssh://` URL (exactly those letters):
/// what follows the scheme up to the first `/`, or the end. `None` for any
/// other text.
fn authority(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("ssh://"))?;
    Some(
        rest.split_once('/')
            .map_or(rest, |(authority, _)| authority),
    )
}

/// Whether an authority's port, if it names one, is digits from 1 to 65535.
/// The host follows the userinfo (up to the last `@`); a bracketed host
/// (`[2001:db8::1]`) ends at its `]`, any other at its first `:`, and the
/// port is everything after that colon: `user:<secret>:443` names the port
/// "<secret>:443", refused, and so does `user:<secret>` (a password where
/// the port goes, the host forgotten). An empty port is refused too.
pub(crate) fn port_is_digits(authority: &str) -> bool {
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let port = if let Some(bracketed) = host.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((_, "")) => return true,
            Some((_, after)) => match after.strip_prefix(':') {
                Some(port) => port,
                None => return false,
            },
            None => return false,
        }
    } else {
        match host.split_once(':') {
            Some((_, port)) => port,
            None => return true,
        }
    };
    !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && port
            .parse::<u32>()
            .is_ok_and(|port| (1..=65535).contains(&port))
}

/// Whether an `https://` or `ssh://` URL carries credentials in its
/// `authority`: any userinfo in an https URL (`user:password@`, or a token
/// as the user), a password in an ssh one (`user:password@`). An ssh user
/// alone (`ssh://git@host/…`) is no credential.
fn carries_credentials(text: &str, authority: &str) -> bool {
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return false;
    };
    text.starts_with("https://") || userinfo.contains(':')
}

/// The schemes a refusal names; any other reads as "another scheme".
const NAMED_SCHEMES: &[&str] = &["https", "ssh", "http", "git", "ftp", "file"];

/// What a refused source is, never what it says (cc-astra A83): an
/// absolute path with no `:` and no `@` is shown (escaped); any other text
/// is described by its scheme from [`NAMED_SCHEMES`] (in lower case),
/// "another scheme" or "no scheme", and its length, so no shape of
/// credential (a token as the user, a password where the port goes, under
/// any scheme) is ever repeated.
pub(crate) fn described(text: &str) -> String {
    if text.starts_with('/') && !text.contains(':') && !text.contains('@') {
        return format!("{text:?}");
    }
    let scheme = text.split_once("://").and_then(|(scheme, _)| {
        let mut chars = scheme.chars();
        (chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .then(|| scheme.to_ascii_lowercase())
    });
    let what = match scheme {
        Some(scheme) if NAMED_SCHEMES.contains(&scheme.as_str()) => {
            format!("a URL of scheme {scheme}://")
        }
        Some(_) => "a URL of another scheme".to_string(),
        None => "a text with no scheme".to_string(),
    };
    format!("{what} of {} bytes (not shown)", text.len())
}

/// Run git and return its trimmed stdout, or the failure as an error.
pub(crate) fn git(arguments: &[&str]) -> Result<String, WorkspaceError> {
    let output = controller_git()
        .args(arguments)
        .output()
        .map_err(|error| unavailable(format!("run git: {error}")))?;
    if !output.status.success() {
        return Err(unavailable(format!(
            "git {}: {}",
            arguments.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// [`git`] as a test runs its own (a seed's `init`, `add` and `commit`, a
/// look at a ref): [`spawn_host::test_git_command`], which no setting of
/// the host's git configuration reaches, so a host that signs every commit
/// cannot hang a test. What Spawn itself runs stays on [`git`].
#[cfg(test)]
pub(crate) fn test_git(arguments: &[&str]) -> Result<String, WorkspaceError> {
    let output = spawn_host::test_git_command()
        .args(arguments)
        .output()
        .map_err(|error| unavailable(format!("run git: {error}")))?;
    if !output.status.success() {
        return Err(unavailable(format!(
            "git {}: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// FNV-1a: a stable short hash for names and keys.
pub(crate) fn fnv(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Directory name of a source's mirror: readable, and unique by a hash.
fn mirror_name(source: &str) -> String {
    let hash = fnv(source);
    let tail: String = source
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("source")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    format!(
        "{}-{hash:016x}.git",
        if tail.is_empty() { "source" } else { &tail }
    )
}

/// The path [`ensure_mirror`] uses, without touching the filesystem. The
/// validated source's stored spelling, not a canonicalized path, keys the
/// mirror name so a read-only operator view resolves exactly the same mirror.
pub(crate) fn mirror_path(root: &Path, source: Option<&str>) -> Result<PathBuf, WorkspaceError> {
    let directory = root.join(".sources");
    match source {
        None => Ok(directory.join("scratch.git")),
        Some(source) => {
            let source = MissionSource::parse(source).map_err(unavailable)?;
            Ok(directory.join(mirror_name(source.as_str())))
        }
    }
}

/// Adapter implementing [`spawn_host::RefLedger`] directly over a shared [`PostgresStore`].
pub(crate) struct PostgresRefLedger {
    store: Arc<PostgresStore>,
}

impl PostgresRefLedger {
    pub(crate) const fn new(store: Arc<PostgresStore>) -> Self {
        Self { store }
    }
}

/// Borrowed [`spawn_host::RefLedger`] adapter over `&PostgresStore` for short-lived
/// read/pin helpers that receive `&PostgresStore` directly.
pub(crate) struct BorrowedPgRefLedger<'a>(&'a PostgresStore);

impl<'a> BorrowedPgRefLedger<'a> {
    pub(crate) const fn new(store: &'a PostgresStore) -> Self {
        Self(store)
    }
}

impl RefLedger for BorrowedPgRefLedger<'_> {
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError> {
        pg_record_snapshot(self.0, id, git_oid, tree, parents, project, created_at_ms)
    }

    fn get_snapshot(&self, id: &Digest) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        self.0
            .get_snapshot(&id.to_string())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?
            .map(snapshot_row_to_ledger)
            .transpose()
    }

    fn get_snapshot_by_oid(
        &self,
        git_oid: &GitOid,
    ) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        self.0
            .get_snapshot_by_oid(&git_oid.to_hex())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?
            .map(snapshot_row_to_ledger)
            .transpose()
    }

    fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: D6RefKind,
        expected: Option<&Digest>,
        new_snapshot: &Digest,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        pg_cas_ref(self.0, project, name, kind, expected, new_snapshot, reason)
    }

    fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        pg_land_main_ref(
            self.0,
            project,
            name,
            expected_base,
            new_snapshot,
            task_id,
            reason,
        )
    }

    fn list_refs(&self, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError> {
        pg_list_refs(self.0, project)
    }

    fn retire_project_refs(&self, project: &str, reason: &str) -> Result<usize, D6StoreError> {
        pg_retire_project_refs(self.0, project, reason)
    }
}

/// Closed project storage backend classification resolved from host boundary
/// markers, root marker, D6 ledger state, and optional environment configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectStorage {
    Git,
    D6,
}

impl ProjectStorage {
    pub(crate) const fn is_d6(self) -> bool {
        matches!(self, Self::D6)
    }
}

/// Signals evaluated by [`project_storage`] / [`project_storage_with_env`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProjectStorageQuery<'a> {
    pub(crate) root: Option<&'a Path>,
    pub(crate) git_dir: Option<&'a Path>,
    pub(crate) ledger_only_d6: bool,
}

/// Single source of truth determining whether a project or workspace root uses
/// [`ProjectStorage::D6`] or [`ProjectStorage::Git`].
pub(crate) fn project_storage_with_env(
    query: ProjectStorageQuery<'_>,
    env_backend: Option<&str>,
) -> ProjectStorage {
    let border_ro = query
        .git_dir
        .is_some_and(|dir| spawn_host::is_readonly_border(dir).unwrap_or(false));
    let root_d6 = query.root.is_some_and(|root| {
        matches!(
            spawn_host::root_backend(root),
            Ok(Some(HostWorkspaceBackend::D6))
        )
    });
    let env_d6 = env_backend.is_some_and(|value| value.trim().eq_ignore_ascii_case("d6"));
    if border_ro || root_d6 || (query.git_dir.is_none() && query.ledger_only_d6) || env_d6 {
        ProjectStorage::D6
    } else {
        ProjectStorage::Git
    }
}

/// Convenience wrapper for [`project_storage_with_env`] reading `SPAWN_WORKSPACE_BACKEND`.
pub(crate) fn project_storage(query: ProjectStorageQuery<'_>) -> ProjectStorage {
    let env_backend = crate::config::env_value("SPAWN_WORKSPACE_BACKEND");
    project_storage_with_env(query, env_backend.as_deref())
}

/// Resolve workspace backend settings for `root` without claiming the root on disk
/// or caching into [`WorkspaceSettings::from_env`]'s process-global `OnceLock`.
/// When `SPAWN_WORKSPACE_BACKEND` is unset, resolves `Auto` (following `root`'s marker
/// or selecting `D6` on clean roots).
pub(crate) fn workspace_settings_readonly(
    root: &Path,
) -> Result<crate::config::WorkspaceSettings, String> {
    fn marked_probe(r: &Path) -> Result<Option<HostWorkspaceBackend>, String> {
        spawn_host::root_backend(r).map_err(|error| error.to_string())
    }

    let raw_backend = crate::config::env_value("SPAWN_WORKSPACE_BACKEND");
    let existing = spawn_host::root_backend(root)
        .map_err(|error| format!("read workspace backend under {}: {error}", root.display()))?;
    let configured = crate::config::workspace_backend(raw_backend.as_deref())?;
    let d6 = crate::config::D6RuntimeConfig::from_env()?;
    let max_file_bytes = crate::config::workspace_max_file_bytes();

    let probes = crate::config::Probes {
        marked: &marked_probe,
    };
    let settings =
        crate::config::WorkspaceSettings::resolve(configured, root, &probes, max_file_bytes)?
            .with_d6(d6);
    if let Some(there) = existing
        && !spawn_host::workspace::backends_compatible(there, settings.active)
    {
        return Err(format!(
            "workspace root {} is claimed by {} backend, refusing {}",
            root.display(),
            there.name(),
            settings.active.name()
        ));
    }
    Ok(settings)
}

/// Construct a [`LocalWorkspaceStore`] for the legacy `git` workspace backend.
pub(crate) fn open_non_d6_store(
    settings: &crate::config::WorkspaceSettings,
    repo: &Path,
    root: &Path,
) -> Result<LocalWorkspaceStore, String> {
    match settings.active {
        HostWorkspaceBackend::Git => Ok(LocalWorkspaceStore::git(
            repo,
            root,
            settings.max_file_bytes,
        )),
        HostWorkspaceBackend::D6 => {
            Err("workspace backend d6 requires CAS and RefLedger configuration".to_string())
        }
    }
}

/// Open a backend-aware [`LocalWorkspaceStore`] bound to `pg` using explicit `settings`.
pub(crate) fn open_store_readonly_with_settings(
    settings: &crate::config::WorkspaceSettings,
    repo: &Path,
    root: &Path,
    pg: &Arc<PostgresStore>,
) -> Result<LocalWorkspaceStore, String> {
    match settings.active {
        HostWorkspaceBackend::D6 => settings.store_with_pg(repo, root, Arc::clone(pg)),
        HostWorkspaceBackend::Git => open_non_d6_store(settings, repo, root),
    }
}

/// Open a read-only backend-aware [`LocalWorkspaceStore`] resolved from environment + root marker.
pub(crate) fn open_workspace_store_readonly(
    repo: &Path,
    root: &Path,
    pg: &Arc<PostgresStore>,
) -> Result<LocalWorkspaceStore, String> {
    let settings = workspace_settings_readonly(root)?;
    open_store_readonly_with_settings(&settings, repo, root, pg)
}

/// Construct a short-lived [`D6Workspace`] bound to a borrowed `&PostgresStore` ledger.
pub(crate) fn open_d6_workspace_borrowed<'a>(
    repo: &Path,
    root: &Path,
    d6_cfg: &crate::config::D6RuntimeConfig,
    max_file_bytes: u64,
    pg: &'a PostgresStore,
) -> Result<D6Workspace<BorrowedPgRefLedger<'a>>, String> {
    let cas_dir = d6_cfg.effective_cas_dir(root);
    let cas = Arc::new(
        CasMerkleStore::open(&cas_dir)
            .map_err(|error| format!("open D6 CAS store at {}: {error}", cas_dir.display()))?,
    );
    let ledger = Arc::new(BorrowedPgRefLedger::new(pg));
    let mut store = D6Workspace::new(
        repo.to_string_lossy().into_owned(),
        root,
        cas,
        ledger,
        max_file_bytes,
    );
    if let Some(policy) = d6_cfg.l1_policy() {
        store = store.with_l1_policy(policy);
    }
    Ok(store)
}

/// Read non-marker conflict `<root>/.main-copies/<agent>` directly under `root` in tests,
/// supporting both Git hex commits and D6 `sha256:...` digests when `root` is a D6 workspace root.
#[cfg(test)]
pub(crate) fn read_main_copies_at(root: &Path, agent: &str) -> Option<(String, Vec<String>)> {
    use spawn_host::tree_path::TreePath;
    let canonical = spawn_host::canonical_root(root);
    let bytes = std::fs::read(canonical.join(".main-copies").join(agent)).ok()?;
    let mut fields = bytes.split(|&b| b == b'\0');
    if fields.next()? != b"main-copies" {
        return None;
    }
    let main_bytes = fields.next()?;
    let main = std::str::from_utf8(main_bytes).ok()?.to_string();
    let valid_main = match project_storage_with_env(
        ProjectStorageQuery {
            root: Some(&canonical),
            ..Default::default()
        },
        None,
    ) {
        ProjectStorage::D6 => Digest::parse(&main).is_ok(),
        ProjectStorage::Git => !main.is_empty() && main.bytes().all(|b| b.is_ascii_hexdigit()),
    };
    if !valid_main {
        return None;
    }
    // Trailing NUL yields one empty final slice in `.split(...)`.
    let files = fields
        .filter(|entry| !entry.is_empty())
        .map(|entry| TreePath::from_bytes(entry).shown())
        .collect();
    Some((main, files))
}

/// Lazy [`RefLedger`] wrapper used by standalone `WorkspaceSettings::store` calls before a shared
/// [`PostgresStore`] is injected (resolves via `SPAWN_DATABASE_URL` on first ledger operation).
pub(crate) struct LazyPgRefLedger {
    store: Mutex<Option<Arc<PostgresStore>>>,
}

impl LazyPgRefLedger {
    pub(crate) const fn new() -> Self {
        Self {
            store: Mutex::new(None),
        }
    }

    fn with_store<T>(
        &self,
        op: impl FnOnce(&PostgresStore) -> Result<T, D6StoreError>,
    ) -> Result<T, D6StoreError> {
        let store = {
            let mut guard = self
                .store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = &*guard {
                Arc::clone(existing)
            } else {
                let opened = Arc::new(crate::config::open_store().map_err(D6StoreError::Ledger)?);
                *guard = Some(Arc::clone(&opened));
                opened
            }
        };
        op(&store)
    }
}

fn to_pg_kind(kind: D6RefKind) -> RefKind {
    match kind {
        D6RefKind::Main => RefKind::Main,
        D6RefKind::Branch => RefKind::Branch,
        D6RefKind::Wip => RefKind::Wip,
        D6RefKind::Tag => RefKind::Tag,
    }
}

fn from_pg_kind(kind: RefKind) -> D6RefKind {
    match kind {
        RefKind::Main => D6RefKind::Main,
        RefKind::Branch => D6RefKind::Branch,
        RefKind::Wip => D6RefKind::Wip,
        RefKind::Tag => D6RefKind::Tag,
    }
}

fn snapshot_row_to_ledger(
    row: spawn_postgres::SnapshotRow,
) -> Result<LedgerSnapshot, D6StoreError> {
    let parents = row
        .parents
        .iter()
        .map(|p| Digest::parse(p))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LedgerSnapshot {
        id: Digest::parse(&row.id)?,
        git_oid: GitOid::parse_hex(&row.git_oid)?,
        tree: Digest::parse(&row.tree)?,
        parents,
        project: row.project,
        created_at_ms: row.created_at_ms,
    })
}

fn pg_record_snapshot(
    pg: &PostgresStore,
    id: &Digest,
    git_oid: &GitOid,
    tree: &Digest,
    parents: &[Digest],
    project: &str,
    created_at_ms: i64,
) -> Result<Digest, D6StoreError> {
    let parent_strs: Vec<String> = parents.iter().map(Digest::to_string).collect();
    let out = pg
        .record_snapshot(
            &id.to_string(),
            &git_oid.to_hex(),
            &tree.to_string(),
            &parent_strs,
            project,
            created_at_ms,
        )
        .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
    Ok(Digest::parse(&out)?)
}

fn pg_cas_ref(
    pg: &PostgresStore,
    project: &str,
    name: &str,
    kind: D6RefKind,
    expected: Option<&Digest>,
    new_snapshot: &Digest,
    reason: &str,
) -> Result<u64, D6StoreError> {
    let exp_str = expected.map(Digest::to_string);
    let new_str = new_snapshot.to_string();
    let seq = pg
        .cas_ref(
            project,
            name,
            to_pg_kind(kind),
            exp_str.as_deref(),
            &new_str,
            reason,
        )
        .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
    u64::try_from(seq).map_err(|_| D6StoreError::Ledger(format!("negative ref seq: {seq}")))
}

fn pg_land_main_ref(
    pg: &PostgresStore,
    project: &str,
    name: &str,
    expected_base: Option<&Digest>,
    new_snapshot: &Digest,
    task_id: Option<i64>,
    reason: &str,
) -> Result<u64, D6StoreError> {
    let exp_str = expected_base.map(Digest::to_string);
    let new_str = new_snapshot.to_string();
    let seq = pg
        .land_main_ref(project, name, exp_str.as_deref(), &new_str, task_id, reason)
        .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
    u64::try_from(seq).map_err(|_| D6StoreError::Ledger(format!("negative ref seq: {seq}")))
}

fn pg_list_refs(pg: &PostgresStore, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError> {
    let rows = pg
        .list_refs(project)
        .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
    rows.into_iter()
        .map(|row| {
            Ok(LedgerRef {
                project: row.project,
                name: row.name,
                kind: from_pg_kind(row.kind),
                snapshot: Digest::parse(&row.snapshot)?,
                updated_at_ms: row.updated_at_ms,
            })
        })
        .collect()
}

fn pg_retire_project_refs(
    pg: &PostgresStore,
    project: &str,
    reason: &str,
) -> Result<usize, D6StoreError> {
    let retired = pg
        .retire_project_refs(project, reason)
        .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
    usize::try_from(retired)
        .map_err(|_| D6StoreError::Ledger(format!("negative retired ref count: {retired}")))
}

impl RefLedger for PostgresRefLedger {
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError> {
        pg_record_snapshot(
            &self.store,
            id,
            git_oid,
            tree,
            parents,
            project,
            created_at_ms,
        )
    }

    fn get_snapshot(&self, id: &Digest) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        self.store
            .get_snapshot(&id.to_string())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?
            .map(snapshot_row_to_ledger)
            .transpose()
    }

    fn get_snapshot_by_oid(
        &self,
        git_oid: &GitOid,
    ) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        self.store
            .get_snapshot_by_oid(&git_oid.to_hex())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?
            .map(snapshot_row_to_ledger)
            .transpose()
    }

    fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: D6RefKind,
        expected: Option<&Digest>,
        new_snapshot: &Digest,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        pg_cas_ref(
            &self.store,
            project,
            name,
            kind,
            expected,
            new_snapshot,
            reason,
        )
    }

    fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        pg_land_main_ref(
            &self.store,
            project,
            name,
            expected_base,
            new_snapshot,
            task_id,
            reason,
        )
    }

    fn list_refs(&self, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError> {
        pg_list_refs(&self.store, project)
    }

    fn retire_project_refs(&self, project: &str, reason: &str) -> Result<usize, D6StoreError> {
        pg_retire_project_refs(&self.store, project, reason)
    }
}

impl RefLedger for LazyPgRefLedger {
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError> {
        self.with_store(|pg| {
            pg_record_snapshot(pg, id, git_oid, tree, parents, project, created_at_ms)
        })
    }

    fn get_snapshot(&self, id: &Digest) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        self.with_store(|pg| {
            pg.get_snapshot(&id.to_string())
                .map_err(|e| D6StoreError::Ledger(e.to_string()))?
                .map(snapshot_row_to_ledger)
                .transpose()
        })
    }

    fn get_snapshot_by_oid(
        &self,
        git_oid: &GitOid,
    ) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        self.with_store(|pg| {
            pg.get_snapshot_by_oid(&git_oid.to_hex())
                .map_err(|e| D6StoreError::Ledger(e.to_string()))?
                .map(snapshot_row_to_ledger)
                .transpose()
        })
    }

    fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: D6RefKind,
        expected: Option<&Digest>,
        new_snapshot: &Digest,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        self.with_store(|pg| pg_cas_ref(pg, project, name, kind, expected, new_snapshot, reason))
    }

    fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        self.with_store(|pg| {
            pg_land_main_ref(
                pg,
                project,
                name,
                expected_base,
                new_snapshot,
                task_id,
                reason,
            )
        })
    }

    fn list_refs(&self, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError> {
        self.with_store(|pg| pg_list_refs(pg, project))
    }

    fn retire_project_refs(&self, project: &str, reason: &str) -> Result<usize, D6StoreError> {
        self.with_store(|pg| pg_retire_project_refs(pg, project, reason))
    }
}

fn rebuild_d6_workspace_with_ledger(
    existing: &D6Workspace<DynRefLedger>,
    ledger: DynRefLedger,
) -> D6Workspace<DynRefLedger> {
    let mut rebuilt = D6Workspace::new(
        existing.project_name().to_string(),
        existing.root(),
        existing.store().clone(),
        Arc::new(ledger),
        existing.max_file_bytes(),
    )
    .with_extra_exclude_files(existing.extra_exclude_files().to_vec());
    if let Some(policy) = existing.l1_policy() {
        rebuilt = rebuilt.with_l1_policy(policy.clone());
    }
    rebuilt
}

fn rebind_d6_store(local: LocalWorkspaceStore, pg: &Arc<PostgresStore>) -> LocalWorkspaceStore {
    let ledger: DynRefLedger = Arc::new(PostgresRefLedger::new(Arc::clone(pg)));
    match local {
        LocalWorkspaceStore::D6(store) => {
            LocalWorkspaceStore::D6(rebuild_d6_workspace_with_ledger(&store, ledger))
        }
        other @ LocalWorkspaceStore::Git(_) => other,
    }
}

impl MissionWorkspaces {
    pub(crate) fn new(
        default: LocalWorkspaceStore,
        root: &Path,
        store: Arc<PostgresStore>,
        factory: Factory,
    ) -> Self {
        let is_d6_family = default.backend() == HostWorkspaceBackend::D6;
        let default = rebind_d6_store(default, &store);
        let factory: Factory = if is_d6_family {
            let pg = Arc::clone(&store);
            Arc::new(move |repo: &Path| rebind_d6_store(factory(repo), &pg))
        } else {
            factory
        };
        Self {
            default: Arc::new(default),
            root: root.to_path_buf(),
            store,
            factory,
            mirrors: Mutex::new(HashMap::new()),
            refreshed: Mutex::new(HashMap::new()),
            one_shot: false,
        }
    }

    fn routed_store(&self, agent: &str) -> Result<Arc<LocalWorkspaceStore>, WorkspaceError> {
        self.route(agent).map(|routed| routed.store)
    }

    pub(crate) fn main_copies(&self, agent: &str) -> Option<(String, Vec<String>)> {
        let routed = self.route(agent).ok()?;
        routed.store.main_copies(agent)
    }

    /// Inspect `agent`'s head snapshot as a clean merge of `main` when backed by D6.
    pub(crate) fn d6_head_clean_merge(&self, agent: &str) -> Option<(String, Vec<String>)> {
        self.routed_store(agent)
            .ok()?
            .d6_workspace()?
            .head_clean_merge(agent)
    }

    /// Whether `prior_commit` introduces any tree changes relative to its merge base with `main`
    /// in the D6 backend routed for `agent`.
    pub(crate) fn d6_prior_has_work(
        &self,
        agent: &str,
        prior_commit: &str,
    ) -> Result<bool, String> {
        let routed = self.routed_store(agent).map_err(|e| e.to_string())?;
        let Some(d6) = routed.d6_workspace() else {
            return Err("expected D6 workspace backend".to_string());
        };
        d6.prior_has_work(prior_commit).map_err(|e| e.to_string())
    }

    /// A store for one change inside a process that goes on (a merge of
    /// main, a set-aside): it queues no push of its own, so nothing is
    /// published before its caller recorded the change; the caller then
    /// publishes it ([`publish`], through the process's pusher for the
    /// source), and dropping the store pushes nothing.
    pub(crate) fn one_shot(
        default: LocalWorkspaceStore,
        root: &Path,
        store: Arc<PostgresStore>,
        factory: Factory,
    ) -> Self {
        let mut one_shot = Self::new(default, root, store, factory);
        one_shot.one_shot = true;
        one_shot
    }

    /// [`WorkspaceStore::set_aside`] through the routed store, only while
    /// `ready` says the agent is idle, asked under the branch's lock
    /// ([`LocalWorkspaceStore::set_aside_when`], the same rule in every
    /// backend).
    pub(crate) fn set_aside_when(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        let routed = self.route(branch.as_str())?;
        let set_aside = routed.store.set_aside_when(branch, start, message, ready)?;
        if let (SetAside::Created { .. }, Some((repo, source))) = (&set_aside, &routed.mirror) {
            self.mark(repo, source, branch.as_str());
        }
        Ok(set_aside)
    }

    /// Queue a push of an agent's branch, to a source that receives them
    /// (a one-shot store's caller publishes instead).
    fn mark(&self, mirror: &Path, source: &str, agent: &str) {
        if !self.one_shot && receives_branches(source) {
            crate::push::shared(source).mark(mirror, source, agent);
        }
    }

    /// Publish again every branch of an agent with an open task: the
    /// pusher's queue lives in this process's memory, so a push that was
    /// queued or backing off when the last process died is otherwise never
    /// made, and the integrator judges the task `stale` on every pass. Run
    /// once, when the Bridge is built; returns how many were queued.
    pub(crate) fn republish(&self) -> Result<usize, String> {
        let agents = self
            .store
            .publishing_agents()
            .map_err(|error| format!("agents to publish: {error}"))?;
        Ok(republish(&self.root, &agents, crate::push::shared))
    }

    /// Fetch a mirror's `main` from its source, unless that happened less
    /// than [`REFRESH_EVERY`] ago and `force` is false.
    fn refresh_main(&self, mirror: &Path, source: &str, force: bool) {
        {
            let mut refreshed = self
                .refreshed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = Instant::now();
            if !force
                && refreshed
                    .get(mirror)
                    .is_some_and(|at| now.duration_since(*at) < REFRESH_EVERY)
            {
                return;
            }
            refreshed.insert(mirror.to_path_buf(), now);
        }
        // A workspace still starts from the last copy of main.
        if let Err(error) = refresh(mirror, source) {
            eprintln!("sources: {error}");
        }
    }

    /// The store for one agent's workspace.
    fn route(&self, agent: &str) -> Result<Routed, WorkspaceError> {
        let source = self
            .store
            .agent_source(agent)
            .map_err(|error| unavailable(format!("mission source of {agent}: {error}")))?;
        let Some(source) = source else {
            return Ok(Routed {
                store: self.default.clone(),
                mirror: None,
            });
        };
        let key = source.clone().unwrap_or_default();
        let mut mirrors = self
            .mirrors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(routed) = mirrors.get(&key) {
            return Ok(routed.clone());
        }
        let (repo, mirror) = if self.default.backend() == HostWorkspaceBackend::D6 {
            (mirror_path(&self.root, source.as_deref())?, None)
        } else {
            let repo = ensure_mirror(&self.root, source.as_deref())?;
            (repo.clone(), source.map(|source| (repo, source)))
        };
        let routed = Routed {
            store: Arc::new((self.factory)(&repo)),
            mirror,
        };
        mirrors.insert(key, routed.clone());
        Ok(routed)
    }
}

/// The mirror of a mission's source under `<root>/.sources/`, cloned on
/// first use; an empty scratch repository when the mission has none.
pub(crate) fn ensure_mirror(root: &Path, source: Option<&str>) -> Result<PathBuf, WorkspaceError> {
    let mirror = mirror_path(root, source)?;
    let directory = root.join(".sources");
    std::fs::create_dir_all(&directory)
        .map_err(|error| unavailable(format!("create {}: {error}", directory.display())))?;
    let Some(source) = source else {
        if !mirror.exists() {
            git(&["init", "--quiet", "--bare", &mirror.to_string_lossy()])?;
        }
        return Ok(mirror);
    };
    // A source is handed to git only once it is one ([`MissionSource`]):
    // a source stored before the check is refused here, not run.
    let source = MissionSource::parse(source).map_err(unavailable)?;
    let source = source.as_str();
    if !mirror.exists() {
        git(&[
            "clone",
            "--quiet",
            "--bare",
            "--",
            source,
            &mirror.to_string_lossy(),
        ])?;
        let path = mirror.to_string_lossy().to_string();
        // Workspaces branch from `main`; a repository whose default
        // branch is called otherwise gets `main` at its HEAD.
        if git(&[
            "--git-dir",
            &path,
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/main",
        ])
        .is_err()
        {
            let head = git(&["--git-dir", &path, "rev-parse", "HEAD"])?;
            git(&["--git-dir", &path, "update-ref", "refs/heads/main", &head])?;
        }
    }
    Ok(mirror)
}

/// Queue a push of each `(agent, source)` branch that this host's mirror
/// of a source receiving branches holds, on `pusher_of(source)`. A branch
/// the source already has is pushed as a no-op; one it lacks, or has
/// behind, is published. A source that is not there now ([`host_directory`])
/// is queued too: this runs once, so a source left out here would stay
/// without its branches until the next restart, and the pusher retries a
/// failed push, at most every five minutes, until the source is back.
/// Returns how many were queued.
fn republish(
    root: &Path,
    agents: &[(String, Option<String>)],
    pusher_of: impl Fn(&str) -> Arc<Pusher>,
) -> usize {
    let mut queued = 0;
    for (agent, source) in agents {
        let Some(source) = source.as_deref().filter(|source| host_directory(source)) else {
            continue;
        };
        let mirror = root.join(".sources").join(mirror_name(source));
        if mirror.is_dir() && has_branch(&mirror, agent) {
            pusher_of(source).mark(&mirror, source, agent);
            queued += 1;
        }
    }
    queued
}

/// Push `agent`'s branch from `mirror` to `source` now, through the
/// process's pusher for `source` ([`Pusher::push_now`]): what a one-shot
/// store changed (a merge of main) is published before its caller goes on.
/// A push that failed is not dropped: the branch stays queued on that
/// pusher, which retries it. `None`: `source` receives no branches.
pub(crate) fn publish(mirror: &Path, source: &str, agent: &str) -> Option<Pushed> {
    publish_with(&crate::push::shared(source), mirror, source, agent)
}

/// [`publish`] on `pusher`.
pub(crate) fn publish_with(
    pusher: &Pusher,
    mirror: &Path,
    source: &str,
    agent: &str,
) -> Option<Pushed> {
    receives_branches(source).then(|| pusher.push_now(mirror, source, agent))
}

/// Push `agent`'s branch from this host's mirror again when its head is
/// `recorded`, the agent's latest recorded checkpoint, which the source
/// lacks: a push queued in a process that died before it was made (a merge
/// of main's, whose record is in the log) would otherwise leave the
/// integrator judging the task `stale` on every pass, forever (issue #8). A
/// head past the record is not pushed: its own record is due, and its push
/// with it. `None`: nothing was pushed (another head, no branch, a source
/// that receives none).
pub(crate) fn push_recorded(
    mirror: &Path,
    source: &str,
    agent: &str,
    recorded: &str,
) -> Option<Pushed> {
    let head = ref_commit(mirror, &format!("refs/heads/{agent}"))?;
    if head != recorded {
        return None;
    }
    publish(mirror, source, agent)
}

/// Bring a mirror's `main` up to its source's default branch. A source that
/// cannot be reached leaves the mirror as it was, and says so: the mirror's
/// `main` is then only the last copy, not the source's main. A workspace
/// may start from it; the integrator may not judge or halt by it.
pub(crate) fn refresh(mirror: &Path, source: &str) -> Result<(), String> {
    let path = mirror.to_string_lossy();
    MissionSource::parse(source).map_err(|error| format!("not refreshing {path}: {error}"))?;
    git(&[
        "--git-dir",
        &path,
        "fetch",
        "--quiet",
        "--",
        source,
        "+HEAD:refs/heads/main",
    ])
    .map(|_| ())
    .map_err(|error| format!("could not refresh {path} from {source}: {error}"))
}

/// The commit a ref of a bare repository points at, if it exists.
pub(crate) fn ref_commit(repo: &Path, reference: &str) -> Option<String> {
    git(&[
        "--git-dir",
        &repo.to_string_lossy(),
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{reference}^{{commit}}"),
    ])
    .ok()
}

/// Whether a bare repository already has a branch.
fn has_branch(repo: &Path, branch: &str) -> bool {
    ref_commit(repo, &format!("refs/heads/{branch}")).is_some()
}

/// Whether agents' branches are published to `source` (`spawn/<agent>`,
/// for the integrator to land): only a directory on this host. A remote may
/// be public, and every agent commit would leave the host.
pub(crate) fn receives_branches(source: &str) -> bool {
    let path = Path::new(source);
    !source.contains("://") && path.is_absolute() && path.is_dir()
}

/// Whether `source` is the kind that receives branches, there now or not:
/// a path on this host that is a directory or, for now, nothing (a volume
/// not mounted, a directory being moved). Its branches are still owed: a
/// push queued for it fails and is retried with the pusher's growing pause
/// until the source is back ([`crate::push`]).
fn host_directory(source: &str) -> bool {
    let path = Path::new(source);
    !source.contains("://") && path.is_absolute() && (path.is_dir() || !path.exists())
}

/// How long a take-back that failed to read its source marks the source
/// unreadable for `source_away` ([`source_readable`]): a failing take-back
/// is tried every pass, so the mark stays while it fails.
const TAKE_BACK_MARK: Duration = Duration::from_mins(2);

/// When a take-back from what was published last failed, by source
/// (objective 205 R5): what `source_away` reads beside `ls-remote`, which
/// lists a ref the source cannot send. Cleared by a take-back that gets
/// through or reads nothing published.
static TAKE_BACK_FAILED: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

fn take_back_failed(source: &str, failed: bool) {
    let mut marks = TAKE_BACK_FAILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let marks = marks.get_or_insert_with(HashMap::new);
    if failed {
        marks.insert(source.to_string(), Instant::now());
    } else {
        marks.remove(source);
    }
}

/// Whether a take-back from `source` failed in the last [`TAKE_BACK_MARK`].
fn take_back_failing(source: &str) -> bool {
    TAKE_BACK_FAILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|marks| marks.get(source))
        .is_some_and(|at| at.elapsed() < TAKE_BACK_MARK)
}

/// Whether a mission source on this host is there for its agents' branches
/// (`source_away`, objective 205 R5): a directory, one git can read (`git
/// ls-remote --heads`), and no take-back from it failed in the last
/// [`TAKE_BACK_MARK`]. An empty directory, a plain one, or a source whose
/// published ref cannot be fetched is away: a branch gone from its mirror
/// cannot be taken back from it, and its checkpoint waits.
pub(crate) fn source_readable(source: &str) -> bool {
    Path::new(source).is_dir()
        && git(&["ls-remote", "--heads", "--", source]).is_ok()
        && !take_back_failing(source)
}

/// Whether `source` is a path on this host, there now or not: the kind
/// whose mirror's branches are published to it.
fn on_this_host(source: &str) -> bool {
    !source.contains("://") && Path::new(source).is_absolute()
}

/// The text of a workspace error, without its kind.
fn error_text(error: &WorkspaceError) -> String {
    match error {
        WorkspaceError::Unavailable(text) => text.clone(),
        WorkspaceError::Refused { code, message } => format!("{code}: {message}"),
    }
}

/// Take `agent`'s branch, gone from `mirror`, back from what this host
/// published to `source` (objective 205): the source's `spawn/<agent>`,
/// listed with `ls-remote`, fetched into a private ref
/// (`refs/spawn-restore/<agent>`, removed after) and made the branch only
/// while the branch is still absent (`update-ref` with the empty old value),
/// so an existing branch is never moved. `Some`: the head taken back.
/// `None`: the source was read and holds nothing published, or the branch
/// is there now (made meanwhile, or a ref that names no commit: the store's
/// own rule refuses it). A source that cannot be read (`ls-remote` fails,
/// the fetch fails, the ref is missing after it) is an error: publishing a
/// branch that does not descend from what was pushed is worse than a
/// delayed checkpoint (R5).
pub(crate) fn restore_published(
    mirror: &Path,
    source: &str,
    agent: &str,
) -> Result<Option<String>, WorkspaceError> {
    let source = MissionSource::parse(source).map_err(unavailable)?;
    let source = source.as_str();
    let remote = pushed_ref(agent);
    let path = mirror.to_string_lossy().to_string();
    // A fixed first line for each step, git's own lines after it: git's
    // processes write their errors in no set order, and a failing commit is
    // logged by its first line (`spawn_core::bridge::failure_key`).
    let step = |what: String| {
        move |error: WorkspaceError| unavailable(format!("{what} failed:\n{}", error_text(&error)))
    };
    let listed = git(&[
        "--git-dir",
        &path,
        "ls-remote",
        "--refs",
        "--",
        source,
        &remote,
    ])
    .map_err(step(format!("listing {remote} in {source}")))?;
    if !listed
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(remote.as_str()))
    {
        return Ok(None);
    }
    let private = format!("refs/spawn-restore/{agent}");
    let branch = format!("refs/heads/{agent}");
    let restored = git(&[
        "--git-dir",
        &path,
        "fetch",
        "--quiet",
        "--no-tags",
        "--",
        source,
        &format!("{remote}:{private}"),
    ])
    .map_err(step(format!("fetching {remote} from {source}")))
    .and_then(|_| {
        ref_commit(mirror, &private).ok_or_else(|| {
            unavailable(format!(
                "{source}'s {remote} is not in {path} after the fetch"
            ))
        })
    })
    .and_then(
        |head| match git(&["--git-dir", &path, "update-ref", &branch, &head, ""]) {
            Ok(_) => Ok(Some(head)),
            Err(error) => {
                let there = git(&[
                    "--git-dir",
                    &path,
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &branch,
                ]);
                if there.is_ok() { Ok(None) } else { Err(error) }
            }
        },
    );
    if !is_protected_task_ref(&private) {
        let _ = git(&["--git-dir", &path, "update-ref", "-d", &private]);
    }
    restored
}

/// What `routed`'s store does for [`MissionWorkspaces::commit`], with no
/// database: the commit, asked under the branch's lock to take a gone
/// branch back from what this host published (R6: whether the branch is
/// there is read under the lock, never before it). For a mission's mirror
/// whose source is a path on this host, the take-back first checks the
/// source is a directory, then [`restore_published`]; an error of either is
/// the commit's ("take <agent> back from what was published: ..."), the
/// branch is not made, the checkpoint stays due, and the source reads as
/// away ([`source_readable`]). The store's result is then read as a
/// take-back's ([`taken_back`]).
fn commit_routed(
    routed: &Routed,
    workspace: &WorkspaceId,
    message: &str,
) -> Result<Commit, WorkspaceError> {
    let agent = workspace.sandbox();
    let taken: Mutex<Option<String>> = Mutex::new(None);
    let take_back = || -> Result<(), WorkspaceError> {
        let Some((repo, source)) = &routed.mirror else {
            return Ok(());
        };
        if !on_this_host(source) {
            return Ok(());
        }
        let restored = if Path::new(source).is_dir() {
            restore_published(repo, source, agent)
        } else {
            Err(unavailable("the source is not a directory now".to_string()))
        };
        take_back_failed(source, restored.is_err());
        let head = restored.map_err(|error| {
            unavailable(format!(
                "take {agent} back from what was published: {}",
                error_text(&error)
            ))
        })?;
        *taken
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = head;
        Ok(())
    };
    #[cfg(test)]
    tests::before_store_commit(agent);
    let commit = routed
        .store
        .commit_or_take_back(workspace, message, &take_back)?;
    let taken = taken
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    taken_back(commit, taken)
}

/// The store's commit after a take-back that made the branch at `taken`
/// (R3): a new commit is [`Commit::Recreated`] on the published head;
/// nothing new is `Recreated` at the published head itself (commit and
/// parent the same). No take-back: the store's result as it is.
fn taken_back(commit: Commit, taken: Option<String>) -> Result<Commit, WorkspaceError> {
    let Some(head) = taken else {
        return Ok(commit);
    };
    let head = CommitId::new(head).map_err(|error| unavailable(error.to_string()))?;
    Ok(match commit {
        Commit::Created(commit) => Commit::Recreated {
            commit,
            parent: Some(head),
        },
        Commit::Unchanged => Commit::Recreated {
            commit: head.clone(),
            parent: Some(head),
        },
        recreated @ Commit::Recreated { .. } => recreated,
    })
}

/// The source's `spawn/<agent>` (what this host's pusher published), fetched
/// into the mirror as `tracking` for the integrator, or `None` when the
/// source has none (a `tracking` left from an earlier fetch is then
/// removed). An unreachable source is an error.
pub(crate) fn fetch_pushed_into(
    mirror: &Path,
    source: &str,
    agent: &str,
    tracking: &str,
) -> Result<Option<String>, WorkspaceError> {
    let source = MissionSource::parse(source).map_err(unavailable)?;
    let source = source.as_str();
    let remote = pushed_ref(agent);
    let path = mirror.to_string_lossy().to_string();
    let listed = git(&[
        "--git-dir",
        &path,
        "ls-remote",
        "--refs",
        "--",
        source,
        &remote,
    ])?;
    if !listed
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(remote.as_str()))
    {
        if !is_protected_task_ref(tracking)
            && !tracking.starts_with("refs/spawn/workers/")
            && ref_commit(mirror, tracking).is_some()
        {
            let _ = git(&["--git-dir", &path, "update-ref", "-d", tracking]);
        }
        return Ok(None);
    }
    git(&[
        "--git-dir",
        &path,
        "fetch",
        "--quiet",
        "--",
        source,
        &format!("+{remote}:{tracking}"),
    ])?;
    Ok(ref_commit(mirror, tracking))
}

/// Whether `ref_name` is a protected per-task durable ref (`refs/spawn/tasks/<id>/*`).
pub(crate) fn is_protected_task_ref(ref_name: &str) -> bool {
    ref_name.starts_with("refs/spawn/tasks/")
}

impl WorkspaceStore for MissionWorkspaces {
    fn project(&self) -> &'static str {
        "missions"
    }

    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError> {
        let routed = self.route(branch.as_str())?;
        let Some((repo, source)) = routed.mirror.clone() else {
            return routed.store.ensure_branch(branch, from);
        };
        if !has_branch(&repo, branch.as_str()) {
            self.refresh_main(&repo, &source, true);
        }
        let made = routed.store.ensure_branch(branch, from)?;
        // A new branch publishes its fork point; an existing one is pushed
        // again (a lost push is also re-queued at a restart: `republish`).
        self.mark(&repo, &source, branch.as_str());
        Ok(made)
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.mount_when(branch, &|| Ok(()))
    }

    /// The routed store's mount.
    fn mount_when(
        &self,
        branch: &BranchName,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let routed = self.route(branch.as_str())?;
        let workspace = routed.store.mount_when(branch, ready)?;
        Ok(workspace)
    }

    /// Every mission's store keeps its workspaces under the one root, so
    /// the default store names any of them; nothing is routed.
    fn canonical(&self, workspace: &WorkspaceId) -> WorkspaceId {
        self.default.canonical(workspace)
    }

    /// The routed store's commit ([`commit_routed`]: a branch gone from a
    /// mission's mirror is taken back from what this host published first).
    /// A new commit is published, and so is a branch made again (objective
    /// 205): on main or with no parent, a source that still holds an older
    /// `spawn/<agent>` refuses its push as a rewrite, which the pusher
    /// reports and never forces.
    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        let routed = self.route(workspace.sandbox())?;
        let commit = commit_routed(&routed, workspace, message)?;
        if let (Commit::Created(_) | Commit::Recreated { .. }, Some((repo, source))) =
            (&commit, &routed.mirror)
        {
            self.mark(repo, source, workspace.sandbox());
        }
        Ok(commit)
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.unmount_when(workspace, &|| Ok(()))
    }

    /// The routed store's unmount.
    fn unmount_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<(), WorkspaceError> {
        let routed = self.route(workspace.sandbox())?;
        routed.store.unmount_when(workspace, ready)?;
        if let Some((repo, source)) = &routed.mirror
            && receives_branches(source)
        {
            let worker_ref = spawn_host::workspace_rules::worker_head_ref(workspace.sandbox());
            if ref_commit(repo, &worker_ref).is_some() {
                match crate::push::push_ref(repo, source, &worker_ref, &worker_ref) {
                    Pushed::Done => {}
                    Pushed::Rejected(why) | Pushed::Failed(why) => {
                        eprintln!("sources: push {worker_ref}: {why}");
                    }
                }
            }
            if has_branch(repo, workspace.sandbox()) {
                self.mark(repo, source, workspace.sandbox());
            }
        }
        Ok(())
    }

    fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
        self.route(workspace.sandbox()).map_or_else(
            |_| self.default.is_mounted(workspace),
            |routed| routed.store.is_mounted(workspace),
        )
    }

    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        self.merge_base_when(workspace, &|| Ok(()))
    }

    /// Main is fetched first, then the routed store asks `ready` under the
    /// branch's lock: the slow fetch is never between the question and the
    /// merge.
    fn merge_base_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<BaseMerge, WorkspaceError> {
        let routed = self.route(workspace.sandbox())?;
        if let Some((repo, source)) = &routed.mirror {
            self.refresh_main(repo, source, false);
        }
        let merged = routed.store.merge_base_when(workspace, ready)?;
        if let (BaseMerge::Merged { .. }, Some((repo, source))) = (&merged, &routed.mirror) {
            self.mark(repo, source, workspace.sandbox());
        }
        Ok(merged)
    }

    fn hold(&self, workspace: &WorkspaceId) -> Option<Hold> {
        self.route(workspace.sandbox()).ok()?.store.hold(workspace)
    }

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        self.route(branch.as_str())?.store.branch_head(branch)
    }

    fn set_aside(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_when(branch, start, message, &|| Ok(()))
    }

    fn notices(&self, workspace: &WorkspaceId) -> Vec<String> {
        self.route(workspace.sandbox())
            .map(|routed| routed.store.notices(workspace))
            .unwrap_or_default()
    }

    fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]) {
        if let Ok(routed) = self.route(workspace.sandbox()) {
            routed.store.notices_told(workspace, notices);
        }
    }

    fn merge_main_clean(&self, branch: &BranchName) -> Result<BaseMerge, WorkspaceError> {
        let routed = self.route(branch.as_str())?;
        if let Some((repo, source)) = &routed.mirror {
            self.refresh_main(repo, source, false);
        }
        let merged = routed.store.merge_main_clean(branch)?;
        if let (BaseMerge::Merged { .. }, Some((repo, source))) = (&merged, &routed.mirror) {
            self.mark(repo, source, branch.as_str());
        }
        Ok(merged)
    }
}

/// Resolves the repository key (and optional mission source) that backs `agent_id`,
/// skipping bare Git mirror initialization when `backend` is [`HostWorkspaceBackend::D6`].
fn resolve_task_repo_for_backend(
    store: &PostgresStore,
    default_repo: Option<&Path>,
    root: &Path,
    agent_id: &str,
    backend: HostWorkspaceBackend,
) -> Result<Option<(PathBuf, Option<String>)>, String> {
    match store
        .agent_source(agent_id)
        .map_err(|error| format!("mission source of {agent_id}: {error}"))?
    {
        Some(source) => {
            let repo = if backend == HostWorkspaceBackend::D6 {
                mirror_path(root, source.as_deref()).map_err(|error| error.to_string())?
            } else {
                ensure_mirror(root, source.as_deref()).map_err(|error| error.to_string())?
            };
            Ok(Some((repo, source)))
        }
        None => Ok(default_repo
            .filter(|repo| backend == HostWorkspaceBackend::D6 || repo.exists())
            .map(|repo| (repo.to_path_buf(), None))),
    }
}

fn d6_pin_worker_head<L: RefLedger + Send + Sync>(
    d6: &D6Workspace<L>,
    agent_id: &str,
    target_refs: &[&str],
) -> Result<Option<String>, String> {
    d6.pin_worker_head(agent_id, None, target_refs)
        .map(|commit| commit.map(|id| id.as_str().to_string()))
        .map_err(|error| error.to_string())
}

fn persist_worker_commits_with_store(
    local: &LocalWorkspaceStore,
    repo: &Path,
    source: Option<&str>,
    root: &Path,
    agent_id: &str,
    task_id: i64,
    stage: &str,
) -> Result<Option<String>, String> {
    let worker_ref = spawn_host::workspace_rules::worker_head_ref(agent_id);
    let task_ref = spawn_host::workspace_rules::task_head_ref(task_id, stage);
    if let Some(d6) = local.d6_workspace() {
        return d6_pin_worker_head(d6, agent_id, &[&task_ref])
            .map_err(|error| format!("pin {task_ref}: {error}"));
    }
    let workspace_dir = root.join(agent_id);
    let dir_opt = workspace_dir.is_dir().then_some(workspace_dir.as_path());
    let pinned = local
        .pin_worker_head(agent_id, dir_opt, &[&task_ref])
        .map_err(|error| format!("pin {task_ref}: {error}"))?;
    let Some(commit) = pinned else {
        return Ok(None);
    };
    if let Some(source) = source
        && receives_branches(source)
    {
        for reference in [&worker_ref, &task_ref] {
            match crate::push::push_ref(repo, source, reference, reference) {
                Pushed::Done => {}
                Pushed::Rejected(why) | Pushed::Failed(why) => {
                    eprintln!("sources: push {reference}: {why}");
                }
            }
        }
    }
    Ok(Some(commit.as_str().to_string()))
}

/// Pins worker commits for `(agent_id, task_id, stage)` through the configured workspace
/// backend (`d6` natively in CAS/`RefLedger`; `git` via [`LocalWorkspaceStore::pin_worker_head`]).
#[cfg(test)]
pub(crate) fn persist_worker_commits_to_repo(
    repo: &Path,
    source: Option<&str>,
    root: &Path,
    agent_id: &str,
    task_id: i64,
    stage: &str,
) -> Result<Option<String>, String> {
    let settings = workspace_settings_readonly(root)?;
    let local = match settings.active {
        HostWorkspaceBackend::D6 => settings.store(repo, root),
        HostWorkspaceBackend::Git => open_non_d6_store(&settings, repo, root)?,
    };
    persist_worker_commits_with_store(&local, repo, source, root, agent_id, task_id, stage)
}

pub(crate) fn task_worker_head_pinned_with_settings(
    store: &PostgresStore,
    settings: &crate::config::WorkspaceSettings,
    default_repo: Option<&Path>,
    root: &Path,
    agent_id: &str,
    task_id: i64,
    stage: &str,
) -> bool {
    let Ok(Some((repo, _))) =
        resolve_task_repo_for_backend(store, default_repo, root, agent_id, settings.active)
    else {
        return false;
    };
    let task_ref = spawn_host::workspace_rules::task_head_ref(task_id, stage);
    if settings.is_d6() {
        let project = repo.to_string_lossy();
        return store
            .list_refs(Some(project.as_ref()))
            .is_ok_and(|refs| refs.into_iter().any(|r| r.name == task_ref));
    }
    ref_commit(&repo, &task_ref).is_some()
}

/// Returns whether `refs/spawn/tasks/<task_id>/<stage>` is already pinned in the
/// store backing `agent_id`.
pub(crate) fn task_worker_head_pinned(
    store: &PostgresStore,
    default_repo: Option<&Path>,
    root: &Path,
    agent_id: &str,
    task_id: i64,
    stage: &str,
) -> bool {
    let Ok(settings) = workspace_settings_readonly(root) else {
        return false;
    };
    task_worker_head_pinned_with_settings(
        store,
        &settings,
        default_repo,
        root,
        agent_id,
        task_id,
        stage,
    )
}

pub(crate) fn persist_task_worker_commits_with_settings(
    store: &PostgresStore,
    settings: &crate::config::WorkspaceSettings,
    default_repo: Option<&Path>,
    root: &Path,
    agent_id: &str,
    task_id: i64,
    stage: &str,
) -> Result<Option<String>, String> {
    let Some((repo, source)) =
        resolve_task_repo_for_backend(store, default_repo, root, agent_id, settings.active)?
    else {
        return Ok(None);
    };
    if settings.is_d6() {
        let task_ref = spawn_host::workspace_rules::task_head_ref(task_id, stage);
        let d6 =
            open_d6_workspace_borrowed(&repo, root, &settings.d6, settings.max_file_bytes, store)?;
        return d6_pin_worker_head(&d6, agent_id, &[&task_ref])
            .map_err(|error| format!("pin {task_ref}: {error}"));
    }
    let local = open_non_d6_store(settings, &repo, root)?;
    persist_worker_commits_with_store(
        &local,
        &repo,
        source.as_deref(),
        root,
        agent_id,
        task_id,
        stage,
    )
}

/// Current head commit of `agent`'s branch across all configured workspace backends.
pub(crate) fn agent_branch_head_with_settings(
    store: &PostgresStore,
    settings: &crate::config::WorkspaceSettings,
    root: &Path,
    agent: &str,
) -> Option<String> {
    let Ok(Some(Some(source))) = store.agent_source(agent) else {
        return None;
    };
    let branch = BranchName::new(agent).ok()?;
    if settings.is_d6() {
        let cas_dir = settings.d6.effective_cas_dir(root);
        if !cas_dir.exists() {
            return None;
        }
        let mirror = mirror_path(root, Some(&source)).ok()?;
        let d6 =
            open_d6_workspace_borrowed(&mirror, root, &settings.d6, settings.max_file_bytes, store)
                .ok()?;
        return d6
            .branch_head(&branch)
            .ok()
            .flatten()
            .map(|commit| commit.as_str().to_string());
    }
    let mirror = ensure_mirror(root, Some(&source)).ok()?;
    let local = open_non_d6_store(settings, &mirror, root).ok()?;
    local
        .branch_head(&branch)
        .ok()
        .flatten()
        .map(|commit| commit.as_str().to_string())
}

/// Persists worker-authored commits for `task_id` (`stage`: `"head"` or
/// `"accepted"`) into the agent's workspace backend (and mission source if configured).
pub(crate) fn persist_task_worker_commits(
    store: &PostgresStore,
    default_repo: Option<&Path>,
    root: &Path,
    agent_id: &str,
    task_id: i64,
    stage: &str,
) -> Result<Option<String>, String> {
    let settings = workspace_settings_readonly(root)?;
    persist_task_worker_commits_with_settings(
        store,
        &settings,
        default_repo,
        root,
        agent_id,
        task_id,
        stage,
    )
}

impl Drop for MissionWorkspaces {
    /// A process that ends (`spawn tick`, `spawn loop` after SIGTERM) pushes
    /// what is still waiting, its one-shot stores' failed pushes included.
    fn drop(&mut self) {
        if !self.one_shot {
            crate::push::flush_shared();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::{
        MissionSource, Routed, commit_routed, ensure_mirror, fetch_pushed_into, mirror_name,
        mirror_path, port_is_digits, receives_branches, restore_published, source_readable,
        test_git as git,
    };
    use crate::push::{Pushed, Pusher, push_branch};
    use spawn_core::id::{BranchName, CommitId, WorkspaceId};
    use spawn_core::port::{Commit, WorkspaceError, WorkspaceStore};
    use spawn_host::GitWorkspace;
    use std::ffi::OsStr;
    use std::path::Path;

    /// A test's step between the routed commit's start and the store's
    /// lock, given the agent.
    type Step = Box<dyn FnOnce(&str)>;

    thread_local! {
        /// What [`before_store_commit`] runs on this thread, once.
        static BEFORE_STORE: std::cell::RefCell<Option<Step>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The pause [`super::commit_routed`] makes right before the store
    /// (R6): a test deletes the branch there, as another writer could
    /// between a pass's first look and the store's lock.
    pub(super) fn before_store_commit(agent: &str) {
        if let Some(step) = BEFORE_STORE.with(|step| step.borrow_mut().take()) {
            step(agent);
        }
    }

    /// One host's checkpoint of `agent` in its mirror: a new file.
    fn checkpoint(store: &GitWorkspace, agent: &str, file: &str) -> String {
        let branch = BranchName::new(agent).expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace: WorkspaceId = store.mount(&branch).expect("mount");
        std::fs::write(Path::new(workspace.path()).join(file), file).expect("write");
        let Commit::Created(commit) = store.commit(&workspace, "work").expect("commit") else {
            panic!("a change");
        };
        store.unmount(&workspace).expect("unmount");
        commit.as_str().to_string()
    }

    #[test]
    fn what_this_host_publishes_is_what_the_integrator_fetches() {
        let base = std::env::temp_dir().join(format!("spawn-sources-push-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        git(&["-C", &seed_path, "add", "-A"]).expect("add");
        git(&[
            "-C",
            &seed_path,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "base",
        ])
        .expect("commit");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");

        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let host = GitWorkspace::new(&mirror, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
        let landing = "refs/spawn-integrate/agent_x";

        // Nothing published yet: the integrator finds nothing.
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("asked"),
            None
        );

        // Each checkpoint, once pushed, is the candidate.
        let first = checkpoint(&host, "agent_x", "one.txt");
        assert_eq!(push_branch(&mirror, &source, "agent_x"), Pushed::Done);
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("fetched"),
            Some(first)
        );
        let second = checkpoint(&host, "agent_x", "two.txt");
        assert_eq!(push_branch(&mirror, &source, "agent_x"), Pushed::Done);
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("fetched"),
            Some(second.clone())
        );
        // The branch the Bridge owns is never moved by the fetch.
        assert_eq!(
            git(&[
                "--git-dir",
                &mirror.to_string_lossy(),
                "rev-parse",
                "refs/heads/agent_x"
            ])
            .expect("branch"),
            second
        );

        // A source that is gone: the push failed (retried), the fetch an
        // error, not "nothing published".
        let gone = base.join("gone.git").to_string_lossy().to_string();
        assert!(matches!(
            push_branch(&mirror, &gone, "agent_x"),
            Pushed::Failed(_)
        ));
        assert!(fetch_pushed_into(&mirror, &gone, "agent_x", landing).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A checkpoint whose push was queued in a process that was killed
    /// (a deploy's SIGTERM, before the pusher ran) is published by the next
    /// process: the integrator would otherwise judge it `stale` forever
    /// (causality audit #4).
    #[test]
    fn a_push_lost_with_a_killed_process_is_made_after_the_restart() {
        let base =
            std::env::temp_dir().join(format!("spawn-sources-republish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        git(&["-C", &seed_path, "add", "-A"]).expect("add");
        git(&[
            "-C",
            &seed_path,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "base",
        ])
        .expect("commit");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let host = GitWorkspace::new(&mirror, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
        let landing = "refs/spawn-integrate/agent_x";

        // The killed process committed and queued a push it never made.
        let head = checkpoint(&host, "agent_x", "one.txt");
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("asked"),
            None
        );

        // The next process: its pusher starts empty.
        let pusher = std::sync::Arc::new(Pusher::new());
        let agents = [
            ("agent_x".to_string(), Some(source.clone())),
            ("agent_none".to_string(), Some(source.clone())),
            ("agent_scratch".to_string(), None),
        ];
        assert_eq!(
            super::republish(&root, &agents, |_| std::sync::Arc::clone(&pusher)),
            1
        );
        assert_eq!(pusher.flush(), 0);
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("fetched"),
            Some(head)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// GitHub #16 item 6: a source that is away when the process starts (an
    /// unmounted volume) is still owed its branches. They are queued, the
    /// pusher's failed pushes wait their pause, and the push is made once
    /// the source is back, with no restart.
    #[test]
    fn a_source_away_at_the_restart_is_published_when_it_is_back() {
        let base = std::env::temp_dir().join(format!("spawn-sources-away-{}", std::process::id()));
        let (source, mirror, host) = published_source(&base);
        let root = base.join("host");
        let landing = "refs/spawn-integrate/agent_x";
        // A checkpoint the killed process never pushed, and the source gone.
        let head = checkpoint(&host, "agent_x", "two.txt");
        let away = format!("{source}.away");
        std::fs::rename(&source, &away).expect("the source goes away");
        assert!(!receives_branches(&source), "not there now");

        // The next process: every push it makes is told to the test.
        let (told, results) = std::sync::mpsc::channel();
        let told = std::sync::Mutex::new(told);
        let pusher = std::sync::Arc::new(Pusher::with(
            Box::new(move |mirror, source, branch| {
                let pushed = crate::push::push_branch(mirror, source, branch);
                let _ = told
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .send(pushed.clone());
                pushed
            }),
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(20),
        ));
        let agents = [("agent_x".to_string(), Some(source.clone()))];
        assert_eq!(
            super::republish(&root, &agents, |_| std::sync::Arc::clone(&pusher)),
            1,
            "queued though the source is away"
        );
        let limit = std::time::Duration::from_mins(1);
        assert!(
            matches!(results.recv_timeout(limit), Ok(Pushed::Failed(_))),
            "the push fails while the source is away"
        );
        assert!(pusher.waiting(&mirror, "agent_x"), "and waits its retry");

        std::fs::rename(&away, &source).expect("the source is back");
        assert!(
            pusher.settled(&mirror, "agent_x", limit),
            "retried until done"
        );
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("fetched"),
            Some(head)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What is queued at a restart: a path on this host, there or not, and
    /// nothing else.
    #[test]
    fn a_restart_queues_only_a_host_path_that_is_a_directory_or_missing() {
        let base = std::env::temp_dir().join(format!("spawn-sources-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("directory");
        let file = base.join("source.bundle");
        std::fs::write(&file, "not a directory").expect("write");
        assert!(super::host_directory(&base.to_string_lossy()));
        assert!(super::host_directory(
            &base.join("unmounted.git").to_string_lossy()
        ));
        assert!(
            !super::host_directory(&file.to_string_lossy()),
            "a file never receives branches"
        );
        assert!(!super::host_directory("https://github.com/acme/widget.git"));
        assert!(!super::host_directory("git@github.com:acme/widget.git"));
        assert!(!super::host_directory(
            "ssh://git@github.com/acme/widget.git"
        ));
        assert!(!super::host_directory("file:///srv/widget.git"));
        assert!(!super::host_directory("relative/dir"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A source with one commit on `main` under a new `base`, and this
    /// host's mirror of it (under `<base>/host`), with `agent_x` checkpointed
    /// once and published: `(source, mirror, host)`.
    fn published_source(base: &Path) -> (String, std::path::PathBuf, GitWorkspace) {
        let _ = std::fs::remove_dir_all(base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        git(&["-C", &seed_path, "add", "-A"]).expect("add");
        git(&[
            "-C",
            &seed_path,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "base",
        ])
        .expect("commit");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let host = GitWorkspace::new(&mirror, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
        checkpoint(&host, "agent_x", "one.txt");
        assert_eq!(push_branch(&mirror, &source, "agent_x"), Pushed::Done);
        (source, mirror, host)
    }

    /// `agent_x` of `mirror` moved back to `main`, under the head the
    /// source holds, and checkpointed again: a rewritten branch.
    fn rewrite(mirror: &Path, host: &GitWorkspace) {
        let path = mirror.to_string_lossy().to_string();
        let main = git(&["--git-dir", &path, "rev-parse", "refs/heads/main"]).expect("main");
        git(&[
            "--git-dir",
            &path,
            "update-ref",
            "refs/heads/agent_x",
            &main,
        ])
        .expect("rewrite");
        checkpoint(host, "agent_x", "three.txt");
    }

    /// Issue #8: a push that loses the source's ref lock to another push
    /// (git says `[remote rejected]`) is a failure, retried until it lands;
    /// only git's own refusal of a rewritten branch (`[rejected]`) is not
    /// retried. Such a push used to count as refused, and was never made.
    ///
    /// The retry is waited for until it is settled, not until it leaves the
    /// queue: a retry the background thread is making has left the queue
    /// and not landed yet, and the fetch that followed it used to find the
    /// old head (about one run in five, in any locale).
    #[test]
    fn a_push_that_loses_the_ref_lock_is_retried_and_a_rewritten_branch_is_refused() {
        let base = std::env::temp_dir().join(format!("spawn-sources-lock-{}", std::process::id()));
        let (source, mirror, host) = published_source(&base);
        let landing = "refs/spawn-integrate/agent_x";
        let head = checkpoint(&host, "agent_x", "two.txt");

        // Another push holds the ref's lock.
        let lock = Path::new(&source).join("refs/heads/spawn/agent_x.lock");
        std::fs::write(&lock, "").expect("lock");
        let pusher = Pusher::with(
            Box::new(push_branch),
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(50),
        );
        assert!(
            matches!(
                pusher.push_now(&mirror, &source, "agent_x"),
                Pushed::Failed(_)
            ),
            "a lost lock is no refusal"
        );
        // While the lock is held no retry can land: the branch is owed its
        // push, queued or being retried, however slow this host is.
        assert!(pusher.waiting(&mirror, "agent_x"), "it waits for its retry");
        std::fs::remove_file(&lock).expect("the other push ends");
        assert!(
            pusher.settled(&mirror, "agent_x", std::time::Duration::from_mins(2)),
            "the retry ends"
        );
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("fetched"),
            Some(head),
            "the retry landed it"
        );

        // A branch rewritten under its published head is refused, not
        // retried and not overwritten.
        rewrite(&mirror, &host);
        assert!(matches!(
            push_branch(&mirror, &source, "agent_x"),
            Pushed::Rejected(why) if why.contains("[rejected]")
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_directory_on_this_host_receives_branches() {
        let here = std::env::temp_dir();
        assert!(
            receives_branches(&here.to_string_lossy()),
            "a directory on this host"
        );
        assert!(!receives_branches("https://github.com/acme/widget.git"));
        assert!(!receives_branches("git@github.com:acme/widget.git"));
        assert!(!receives_branches("ssh://git@github.com/acme/widget.git"));
        assert!(!receives_branches("/no/such/directory.git"));
        assert!(!receives_branches("file:///srv/widget.git"));
        assert!(!receives_branches("relative/dir"));
    }

    #[test]
    fn a_source_that_names_a_transport_command_is_refused_by_spawn() {
        let base = std::env::temp_dir().join(format!("spawn-h5-repro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");
        let marker = base.join("ran");
        let source = format!("ext::sh -c touch% {}", marker.display());
        let refused = ensure_mirror(&base, Some(&source)).expect_err("not a source");
        let text = refused.to_string();
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&base);
        // `git clone: …` is git's refusal (its text is localized); Spawn's
        // own refusal names the source before any git runs.
        assert!(
            !text.contains("git clone:"),
            "only git's own default stood between the source and a command: {text}"
        );
    }

    #[test]
    fn a_source_is_an_absolute_path_or_an_https_or_ssh_url() {
        for good in [
            "/srv/widget.git",
            "/home/me/my repo",
            "https://github.com/acme/widget.git",
            "ssh://git@github.com/acme/widget.git",
            "git@github.com:acme/widget.git",
            "deploy_bot@git.example.org:team/x.git",
        ] {
            assert_eq!(
                MissionSource::parse(good).map(|source| source.as_str().to_string()),
                Ok(good.to_string()),
                "{good}"
            );
        }
        for bad in [
            "",
            "ext::sh -c touch% /tmp/x",
            "fd::3",
            "file:///srv/widget.git",
            "http://github.com/acme/widget.git",
            "git://github.com/acme/widget.git",
            "widget",
            "../widget",
            "-uhelp",
            "--upload-pack=touch /tmp/x",
            "https://-oProxyCommand=x/repo",
            "https:///etc/passwd",
            "https://host/a b",
            "https://host/a\nb",
            "ssh://host/\u{0}",
            "-u@host:path",
            "git@-host:path",
            "git@host:-path",
            "git@host::path",
            "ext@host:path with space",
            "/srv/a\u{7}b",
        ] {
            assert!(MissionSource::parse(bad).is_err(), "{bad:?}");
        }
        assert!(MissionSource::parse(&format!("/{}", "a".repeat(5000))).is_err());
    }

    /// Objective 171: a URL carrying credentials is refused by name,
    /// without repeating them; an ssh user alone, an @ in a path and an
    /// scp-style source are no credential.
    #[test]
    fn a_source_url_carrying_credentials_is_refused_by_name() {
        let token = "ghp_171madeup0123456789abcdefghijklmnopq";
        for (bad, secret) in [
            (
                format!("https://user:{token}@git.example.org/acme/r.git"),
                token,
            ),
            (format!("https://{token}@git.example.org/acme/r.git"), token),
            (
                "https://deploy:hunter171@github.com:443/acme/r.git".to_string(),
                "hunter171",
            ),
            (
                "ssh://git:hunter171@git.example.org/acme/r.git".to_string(),
                "hunter171",
            ),
        ] {
            let error = MissionSource::parse(&bad).expect_err(&bad);
            assert!(error.contains("carries credentials"), "{error}");
            assert!(error.contains("credential helper"), "{error}");
            assert!(!error.contains(secret), "{error}");
        }
        for good in [
            "ssh://git@github.com/acme/widget.git",
            "ssh://git@github.com:2222/acme/widget.git",
            "https://git.example.org/acme/r@2.git",
            "https://git.example.org:8443/acme/r.git",
            "https://[2001:db8::1]:443/acme/r.git",
            "https://[2001:db8::1]/acme/r.git",
            "git@github.com:acme/widget.git",
        ] {
            assert_eq!(
                MissionSource::parse(good).map(|source| source.as_str().to_string()),
                Ok(good.to_string()),
                "{good}"
            );
        }
    }

    /// cc-astra A76, A83 and rev-g's F1 to F3: no refusal repeats a
    /// credential, whatever its form and whichever rule refuses it; a port
    /// that is not digits from 1 to 65535 is refused; only a plain absolute
    /// path is shown.
    #[test]
    fn no_refusal_repeats_a_credential_in_any_form() {
        let opaque = "Zq81mPW04xLrT7vKjHc2NdYb";
        let digits = "8675309123456";
        let long = "a".repeat(4200);
        let cases = [
            format!("https://{opaque}@git.example.org/acme/r.git"),
            format!("https://deploy:{digits}@git.example.org/acme/r.git"),
            format!("https://u:{opaque}@git.example.org/{long}"),
            format!("ftp://u:{opaque}@git.example.org/acme/r.git"),
            format!("u:{digits}@git.example.org"),
            format!("u:{opaque}@git.example.org/x://y"),
            format!("https://u:abc/{opaque}@git.example.org/r b.git"),
            format!("ftp://u:abc/{opaque}@git.example.org/r.git"),
            format!("https://u:abc/{opaque}@git.example.org/r.git"),
            format!("https://user:{opaque}/r.git"),
            format!("ssh://git@git.example.org:{opaque}/r"),
            format!("https://user:{digits}/r.git"),
            format!("oauth2:{opaque}"),
            opaque.to_string(),
            format!("http://oauth2:{opaque}/r.git"),
            format!("HTTPS://user:{opaque}/r.git"),
            format!("SSH://git@git.example.org:{opaque}/r"),
            format!("git://git.example.org:{opaque}/r"),
            format!("https://u/{opaque}@git.example.org/r b.git"),
            format!("https://user:{opaque}:443/r b.git"),
            format!("https://user:{opaque}:443/r.git"),
            format!("ssh://git@git.example.org:{digits}:22/r"),
            format!("/srv/u:{opaque}@git.example.org\u{1}"),
            format!("/srv/{opaque}@git.example.org\u{1}"),
            format!("ssh://deploy:{digits}@git.example.org/acme/r.git"),
            format!("https://[2001:db8::1]:{opaque}/acme/r.git"),
        ];
        for bad in &cases {
            let error = MissionSource::parse(bad).expect_err(bad);
            assert!(
                !error.contains(opaque) && !error.contains(digits),
                "{bad:?}: {error}"
            );
        }
        let overlength = MissionSource::parse(&cases[2]).expect_err("overlength");
        assert_eq!(
            overlength,
            format!("a source is at most 4096 bytes, not {}", cases[2].len())
        );
        let port = MissionSource::parse(&cases[9]).expect_err("port");
        assert!(port.contains("port"), "{port}");
        assert!(port.contains("scheme https://"), "{port}");
        assert!(
            MissionSource::parse(&cases[13])
                .expect_err("pasted")
                .contains("no scheme")
        );
        // A plain absolute path is shown; ports 1 to 65535 in digits are
        // taken, any other refused.
        let plain = MissionSource::parse("/srv/a\u{7}b").expect_err("control");
        assert!(plain.contains(r#""/srv/a\u{7}b""#), "{plain}");
        for port in ["1", "22", "65535"] {
            let good = format!("https://git.example.org:{port}/r.git");
            assert!(MissionSource::parse(&good).is_ok(), "{good}");
        }
        for port in ["0", "65536", "abc", "", "00000022", "+22"] {
            let bad = format!("https://git.example.org:{port}/r.git");
            assert!(MissionSource::parse(&bad).is_err(), "{bad}");
            assert!(
                !port_is_digits(&format!("git.example.org:{port}")),
                "{port}"
            );
        }
        assert!(!port_is_digits("[2001:db8::1]:x"));
        assert!(!port_is_digits("[2001:db8::1"));
        assert!(port_is_digits("git@[2001:db8::1]:22"));
    }

    /// The controller's git runs only file, https and ssh, even on a host
    /// whose git configuration allows `ext::`: GIT_ALLOW_PROTOCOL wins.
    #[test]
    fn the_controllers_git_runs_no_transport_command_whatever_the_host_allows() {
        let base = std::env::temp_dir().join(format!("spawn-git-protocols-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");
        let marker = base.join("ran");
        let allowing = |mut command: std::process::Command| {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "protocol.ext.allow")
                .env("GIT_CONFIG_VALUE_0", "always")
                .args([
                    "ls-remote",
                    &format!("ext::sh -c touch% {}", marker.display()),
                ])
                .output()
                .expect("git runs")
        };
        // A git without the controller's rule: the configuration given on
        // the command (`GIT_CONFIG_*`, as a host's would be) lets it run.
        let _ = allowing(spawn_host::test_git_command());
        assert!(marker.exists(), "the host's configuration would run it");
        std::fs::remove_file(&marker).expect("reset");
        let refused = allowing(super::controller_git());
        assert!(!refused.status.success());
        assert!(
            !marker.exists(),
            "the controller's git ran a transport command"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every git the controller runs is built by `controller_git`, on
    /// `spawn_host::git_command`, so it runs in the C locale; every git a
    /// test runs for itself by `test_git` (`spawn_host::test_git_command`),
    /// which no setting of the host's git configuration reaches. No file of
    /// the CLI, its tests included, starts git itself, and no test runs its
    /// own git through the controller's `git`.
    #[test]
    fn every_controller_git_is_built_in_one_place() {
        fn sources(directory: &Path, found: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(directory).expect("a source directory") {
                let path = entry.expect("an entry").path();
                if path.is_dir() {
                    sources(&path, found);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    found.push(path);
                }
            }
        }
        /// Whether `code` names the controller's `git`: by a path (`::`
        /// then `git`, the needle is spelled apart so this file does not
        /// name it), or alone in a `use` list (`test_git as git` is the
        /// test's).
        fn names_git(code: &str) -> bool {
            let needle = concat!(":", ":git");
            let path = code.match_indices(needle).any(|(at, _)| {
                let rest = &code[at + needle.len()..];
                !rest.starts_with(|next: char| next.is_alphanumeric() || next == '_')
            });
            let listed = code.match_indices("use ").any(|(at, _)| {
                let line = code[..at].rfind('\n').map_or(0, |newline| newline + 1);
                code[line..at].trim().is_empty()
                    && code[at..]
                        .split(';')
                        .next()
                        .and_then(|statement| statement.split_once('{'))
                        .is_some_and(|(_, items)| {
                            items
                                .split([',', '{', '}'])
                                .any(|item| item.trim() == "git")
                        })
            });
            path || listed
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        sources(&root.join("src"), &mut files);
        sources(&root.join("tests"), &mut files);
        assert!(
            files.iter().any(|file| file.ends_with("src/sources.rs")),
            "{files:?}"
        );
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read a source");
            assert!(
                !text.contains("Command::new(\"git\")"),
                "{} starts git without controller_git or test_git",
                file.display()
            );
            let tests = if file.starts_with(root.join("tests")) || file.ends_with("tests.rs") {
                text.as_str()
            } else {
                text.split_once("#[cfg(test)]\nmod ")
                    .map_or("", |(_, tests)| tests)
            };
            assert!(
                !names_git(tests),
                "{} runs a test's own git through the controller's git, not test_git",
                file.display()
            );
        }
        assert!(names_git(concat!("let git = crate::sources:", ":git;")));
        assert!(names_git("    use crate::sources::{ensure_mirror, git};"));
        assert!(!names_git(
            "    use crate::sources::{ensure_mirror, test_git as git};"
        ));
        assert!(!names_git("let git = crate::sources::test_git;"));
        let own = include_str!("sources.rs");
        let code = own.split("#[cfg(test)]\nmod ").next().unwrap_or(own);
        assert_eq!(code.matches("spawn_host::git_command(\"git\")").count(), 1);
        let command = super::controller_git();
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("LC_ALL"), Some(OsStr::new("C")))));
        assert!(envs.contains(&(OsStr::new("LANGUAGE"), None)));
    }

    /// Marks a line of [`decisions_in_this_locale`]'s output.
    const DECISION: &str = "spawn-decision: ";

    /// Marks the line of [`decisions_in_this_locale`]'s output that shows
    /// git's own answer, outside Spawn.
    const PLAIN: &str = "plain-git: ";

    /// What Spawn makes of git in this process's locale, one line each, the
    /// run's scratch directory written `<base>`: a push that lost the ref
    /// lock, a push of a rewritten branch, a fetch from a source that is
    /// gone. Then, outside Spawn, what a plain git says about a missing
    /// repository. Run by
    /// [`a_translated_locale_changes_nothing_spawn_reads_from_git`] in one
    /// process per locale; alone it only prints.
    #[test]
    #[ignore = "run by a_translated_locale_changes_nothing_spawn_reads_from_git"]
    fn decisions_in_this_locale() {
        let base =
            std::env::temp_dir().join(format!("spawn-sources-locale-{}", std::process::id()));
        let shown = base.to_string_lossy().to_string();
        let say = |mark: &str, decision: String| {
            println!("{mark}{}", decision.replace(&shown, "<base>"));
        };
        let (source, mirror, host) = published_source(&base);
        checkpoint(&host, "agent_x", "two.txt");

        // Another push holds the ref's lock: a failure, retried.
        let lock = Path::new(&source).join("refs/heads/spawn/agent_x.lock");
        std::fs::write(&lock, "").expect("lock");
        let pushed = push_branch(&mirror, &source, "agent_x");
        assert!(
            matches!(&pushed, Pushed::Failed(why) if why.contains("[remote rejected]")),
            "{pushed:?}"
        );
        say(DECISION, format!("{pushed:?}"));
        std::fs::remove_file(&lock).expect("the other push ends");

        // A rewritten branch: refused.
        rewrite(&mirror, &host);
        let pushed = push_branch(&mirror, &source, "agent_x");
        assert!(
            matches!(&pushed, Pushed::Rejected(why) if why.contains("[rejected]")),
            "{pushed:?}"
        );
        say(DECISION, format!("{pushed:?}"));

        // A source that is gone: an error, not "nothing published".
        let gone = base.join("gone.git").to_string_lossy().to_string();
        let fetched = fetch_pushed_into(&mirror, &gone, "agent_x", "refs/spawn-integrate/x");
        assert!(fetched.is_err(), "{fetched:?}");
        say(DECISION, format!("{fetched:?}"));

        // git's own answer in this process's locale: a test's git, given
        // back the locale `test_git_command` takes away.
        let mut own = spawn_host::test_git_command();
        for name in ["LC_ALL", "LANGUAGE"] {
            match std::env::var_os(name) {
                Some(value) => own.env(name, value),
                None => own.env_remove(name),
            };
        }
        let plain = own
            .args(["--git-dir", &gone, "rev-parse", "HEAD"])
            .output()
            .expect("git runs");
        say(
            PLAIN,
            String::from_utf8_lossy(&plain.stderr)
                .trim()
                .replace('\n', " "),
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every git the controller runs is in the C locale
    /// ([`super::controller_git`]), so a host whose locale is translated
    /// changes nothing Spawn reads from git: the same pushes and fetch, made
    /// in a process whose whole environment is Chinese (`LC_ALL`, `LANG`,
    /// `LC_MESSAGES`, `LANGUAGE`), come to the same decisions, the texts
    /// they quote included, as in C. Before, the texts were git's Chinese.
    /// On a host without zh_CN (`locale -a`) or git's Chinese messages,
    /// nothing is translated and the comparison holds anyway.
    #[test]
    fn a_translated_locale_changes_nothing_spawn_reads_from_git() {
        let program = std::env::current_exe().expect("the test binary");
        let run = |locale: &str, language: Option<&str>| {
            let mut command = std::process::Command::new(&program);
            command
                .args([
                    "--exact",
                    "sources::tests::decisions_in_this_locale",
                    "--ignored",
                    "--nocapture",
                ])
                .env("LC_ALL", locale)
                .env("LANG", locale)
                .env("LC_MESSAGES", locale);
            match language {
                Some(language) => command.env("LANGUAGE", language),
                None => command.env_remove("LANGUAGE"),
            };
            let output = command.output().expect("run the test binary");
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{locale}: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let marked = |mark: &str| -> Vec<String> {
                stdout
                    .lines()
                    .filter_map(|line| line.strip_prefix(mark))
                    .map(str::to_string)
                    .collect()
            };
            (marked(DECISION), marked(PLAIN))
        };
        let (english, plain_english) = run("C", None);
        assert_eq!(english.len(), 3, "{english:?}");
        let (translated, plain_translated) = run("zh_CN.UTF-8", Some("zh_CN:zh"));
        if plain_translated == plain_english {
            eprintln!("this host does not translate git ({plain_translated:?}): nothing to prove");
        }
        assert_eq!(
            translated, english,
            "git's answer here: {plain_translated:?}"
        );
    }

    #[test]
    fn mirrors_are_named_by_source_and_unique() {
        let a = mirror_name("https://github.com/acme/widget.git");
        let b = mirror_name("/home/me/widget");
        assert!(a.starts_with("widget-") && a.to_lowercase().ends_with(".git"));
        assert!(b.starts_with("widget-"));
        assert_ne!(a, b);
        assert!(mirror_name("git@host:team/x y.git").starts_with("xy-"));
    }

    #[test]
    fn mirror_path_is_pure_and_is_exactly_what_ensure_mirror_uses() {
        let base = std::env::temp_dir().join(format!("spawn-mirror-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed repo");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        git(&["-C", &seed_path, "add", "-A"]).expect("add");
        git(&[
            "-C",
            &seed_path,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "base",
        ])
        .expect("commit");
        let root = base.join("root with ' quote");
        let remote = "https://git.example.org/acme/widget.git";
        let local_path = mirror_path(&root, Some(&seed_path)).expect("local path");
        let remote_path = mirror_path(&root, Some(remote)).expect("remote path");
        let scratch_path = mirror_path(&root, None).expect("scratch path");
        assert!(!root.exists(), "the pure lookup created nothing");
        assert_eq!(
            local_path,
            root.join(".sources").join(mirror_name(&seed_path))
        );
        assert_eq!(remote_path, root.join(".sources").join(mirror_name(remote)));
        assert_eq!(scratch_path, root.join(".sources/scratch.git"));

        // Existing mirrors make ensure_mirror a pure path lookup too: no
        // fetch or clone of the remote URL is needed for this equality test.
        for (source, expected) in [(seed_path.as_str(), local_path), (remote, remote_path)] {
            std::fs::create_dir_all(expected.parent().expect("mirror parent")).expect("parent");
            git(&[
                "clone",
                "-q",
                "--bare",
                &seed_path,
                &expected.to_string_lossy(),
            ])
            .expect("preexisting mirror");
            assert_eq!(
                ensure_mirror(&root, Some(source)).expect("ensure"),
                expected
            );
        }
        assert_eq!(ensure_mirror(&root, None).expect("scratch"), scratch_path);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `agent_x`'s store on `mirror`, routed as a mission's agent on
    /// `source` is.
    fn routed_x(mirror: &Path, root: &Path, source: &str) -> Routed {
        Routed {
            store: std::sync::Arc::new(
                GitWorkspace::new(mirror, root, spawn_host::DEFAULT_MAX_FILE_BYTES).into(),
            ),
            mirror: Some((mirror.to_path_buf(), source.to_string())),
        }
    }

    fn rev(mirror: &Path, reference: &str) -> Option<String> {
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            "--verify",
            "--quiet",
            reference,
        ])
        .ok()
    }

    fn delete_ref(repo: &Path, reference: &str) {
        git(&[
            "--git-dir",
            &repo.to_string_lossy(),
            "update-ref",
            "-d",
            reference,
        ])
        .expect("delete the ref");
    }

    /// Objective 205: a branch gone from a mission's mirror is taken back
    /// from what this host published. Nothing published: `None`. Published:
    /// the pushed head, made the branch, and no private ref left. A branch
    /// that is there is never moved. A source that cannot be read (R5: gone)
    /// is an error, and leaves no branch.
    #[test]
    fn a_branch_gone_from_the_mirror_is_taken_back_from_what_was_published() {
        let base =
            std::env::temp_dir().join(format!("spawn-sources-take-back-{}", std::process::id()));
        let (source, mirror, _host) = published_source(&base);
        let pushed = rev(&mirror, "refs/heads/agent_x").expect("the published head");
        let private = "refs/spawn-restore/agent_x";

        assert_eq!(
            restore_published(&mirror, &source, "agent_y").expect("read"),
            None,
            "nothing published"
        );
        delete_ref(&mirror, "refs/heads/agent_x");
        assert_eq!(
            restore_published(&mirror, &source, "agent_x").expect("taken back"),
            Some(pushed.clone())
        );
        assert_eq!(rev(&mirror, "refs/heads/agent_x"), Some(pushed.clone()));
        assert_eq!(rev(&mirror, private), None, "no private ref left");

        // There: never moved.
        let main = rev(&mirror, "refs/heads/main").expect("main");
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "update-ref",
            "refs/heads/agent_x",
            &main,
        ])
        .expect("move");
        assert_eq!(
            restore_published(&mirror, &source, "agent_x").expect("read"),
            None
        );
        assert_eq!(rev(&mirror, "refs/heads/agent_x"), Some(main));
        assert_eq!(rev(&mirror, private), None);

        // The source gone: an error, and no branch.
        delete_ref(&mirror, "refs/heads/agent_x");
        let away = format!("{source}.away");
        std::fs::rename(&source, &away).expect("move the source away");
        assert!(restore_published(&mirror, &source, "agent_x").is_err());
        assert_eq!(rev(&mirror, "refs/heads/agent_x"), None);
        std::fs::rename(&away, &source).expect("move it back");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Objective 205 R5: a published ref the source lists but cannot send
    /// (an id it has no object for) fails the commit, which says it could
    /// not take the branch back, makes no branch, and reads the source as
    /// away until a take-back gets through, on what was published.
    #[test]
    fn a_published_ref_the_source_cannot_send_is_an_error_and_reads_as_away() {
        let base =
            std::env::temp_dir().join(format!("spawn-sources-unsent-{}", std::process::id()));
        let (source, mirror, host) = published_source(&base);
        let pushed = rev(&mirror, "refs/heads/agent_x").expect("the published head");
        let routed = routed_x(&mirror, &base.join("host"), &source);
        let branch = BranchName::new("agent_x").expect("valid branch");
        let workspace = host.mount(&branch).expect("mount");
        std::fs::write(Path::new(workspace.path()).join("two.txt"), "two").expect("write");
        assert!(source_readable(&source));

        let published = Path::new(&source).join("refs/heads/spawn/agent_x");
        delete_ref(Path::new(&source), "refs/heads/spawn/agent_x");
        std::fs::create_dir_all(published.parent().expect("a parent")).expect("refs");
        std::fs::write(&published, format!("{}\n", "1".repeat(40))).expect("a broken ref");
        delete_ref(&mirror, "refs/heads/agent_x");
        let failed = commit_routed(&routed, &workspace, "two");
        assert!(
            matches!(&failed, Err(WorkspaceError::Unavailable(text))
                     if text.starts_with("take agent_x back from what was published: ")),
            "{failed:?}"
        );
        assert_eq!(rev(&mirror, "refs/heads/agent_x"), None, "no branch made");
        assert!(!source_readable(&source), "away while the take-back fails");

        std::fs::write(&published, format!("{pushed}\n")).expect("mend the ref");
        let made = commit_routed(&routed, &workspace, "two").expect("taken back");
        let Commit::Recreated { commit, parent } = made else {
            panic!("taken back: {made:?}");
        };
        assert_eq!(parent.as_ref().map(CommitId::as_str), Some(pushed.as_str()));
        assert_eq!(rev(&mirror, &format!("{commit}^")), Some(pushed));
        assert!(source_readable(&source), "there again");
        host.unmount(&workspace).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Objective 205 R5: a source is there for `source_away` only when git
    /// can read it: a bare repository is, a plain directory and a missing
    /// path are away.
    #[test]
    fn a_source_is_there_only_when_git_can_read_it() {
        let base =
            std::env::temp_dir().join(format!("spawn-sources-readable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repository = base.join("bare.git");
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).expect("plain");
        git(&["init", "-q", "--bare", &repository.to_string_lossy()]).expect("bare");
        assert!(source_readable(&repository.to_string_lossy()));
        assert!(!source_readable(&plain.to_string_lossy()));
        assert!(!source_readable(&base.join("missing").to_string_lossy()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Objective 205 R6: a branch deleted between a pass's start and the
    /// store's lock is still taken back from what was published (the store
    /// reads whether it is there under its lock), never made again on main
    /// beside it.
    #[test]
    fn a_branch_deleted_on_the_way_to_the_store_is_still_taken_back() {
        let base =
            std::env::temp_dir().join(format!("spawn-sources-on-the-way-{}", std::process::id()));
        let (source, mirror, host) = published_source(&base);
        let pushed = rev(&mirror, "refs/heads/agent_x").expect("the published head");
        let routed = routed_x(&mirror, &base.join("host"), &source);
        let branch = BranchName::new("agent_x").expect("valid branch");
        let workspace = host.mount(&branch).expect("mount");
        std::fs::write(Path::new(workspace.path()).join("two.txt"), "two").expect("write");
        let deleted = mirror.clone();
        BEFORE_STORE.with(|step| {
            *step.borrow_mut() = Some(Box::new(move |agent: &str| {
                delete_ref(&deleted, &format!("refs/heads/{agent}"));
            }));
        });
        let made = commit_routed(&routed, &workspace, "two").expect("taken back");
        let Commit::Recreated { commit, parent } = made else {
            panic!("taken back: {made:?}");
        };
        assert_eq!(parent.as_ref().map(CommitId::as_str), Some(pushed.as_str()));
        assert_eq!(rev(&mirror, &format!("{commit}^")), Some(pushed));
        assert_eq!(
            rev(&mirror, "refs/heads/agent_x"),
            Some(commit.as_str().to_string())
        );
        host.unmount(&workspace).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A bare repository whose `main` holds a README and a source file.
    fn backend_repo(base: &Path) -> std::path::PathBuf {
        let seed = base.join("seed");
        std::fs::create_dir_all(seed.join("src")).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        std::fs::write(seed.join("src/lib.rs"), "fn lib() {}\n").expect("write");
        git(&["-C", &seed_path, "add", "-A"]).expect("add");
        git(&["-C", &seed_path, "commit", "-qm", "base"]).expect("commit");
        let repo = base.join("default.git");
        git(&["clone", "-q", "--bare", &seed_path, &repo.to_string_lossy()]).expect("bare");
        repo
    }

    /// The Git backend through [`MissionWorkspaces`], for an agent no mission
    /// owns (routed to the default store): mount, cancelled mount, hold,
    /// checkpoint, set-aside (waiting, then made, then the same answer again)
    /// and unmount. (Native D6 lifecycle is covered by
    /// `mission_workspaces_d6_native_lifecycle_with_l1_tmpfs_and_zero_git`.)
    #[test]
    #[allow(clippy::too_many_lines)] // one backend's whole run, step by step
    fn every_engine_runs_alike_through_the_mission_store() {
        use crate::config::{Probes, WorkspaceBackend, WorkspaceSettings};
        use spawn_core::port::{MOUNT_CANCELLED, SetAside, UNMOUNT_IN_USE};
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("backends");
        let store = std::sync::Arc::new(
            spawn_postgres::PostgresStore::connect_in_schema_with_options(
                &url,
                &schema,
                &spawn_postgres::testing::test_connect_options(),
            )
            .expect("connect and migrate"),
        );
        let base =
            std::env::temp_dir().join(format!("spawn-sources-backends-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = backend_repo(&base);
        let main = git(&["--git-dir", &repo.to_string_lossy(), "rev-parse", "main"]).expect("main");
        let configured = WorkspaceBackend::Named(spawn_host::WorkspaceBackend::Git);
        let root = base.join(format!("{configured:?}").to_lowercase());
        let settings = WorkspaceSettings::resolve(
            configured,
            &root,
            &Probes::host(),
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        )
        .expect("resolve git");
        let name = settings.active.name();
        let workspaces = settings.one_shot(&repo, &root, store.clone());
        let agent = format!("agent_backend_{name}");
        let branch = BranchName::new(agent.as_str()).expect("branch");
        workspaces.ensure_branch(&branch, None).expect("branch");
        let cancelled = workspaces.mount_when(&branch, &|| Err("not now".to_string()));
        assert!(
            matches!(&cancelled, Err(WorkspaceError::Refused { code, .. }) if code == MOUNT_CANCELLED),
            "{name}: {cancelled:?}"
        );
        let workspace = workspaces.mount_when(&branch, &|| Ok(())).expect("mount");
        assert!(workspaces.is_mounted(&workspace), "{name}");
        assert!(workspaces.hold(&workspace).is_some(), "{name}");
        let routed = workspaces.route(&agent).expect("routed");
        assert_eq!(routed.store.backend_name(), name);
        assert!(routed.mirror.is_none(), "no mission: the default store");
        let view = std::path::PathBuf::from(workspace.path());
        std::fs::write(view.join("README.md"), "changed\n").expect("write");
        std::fs::create_dir_all(view.join("target/.transcripts")).expect("target");
        std::fs::write(view.join("target/.transcripts/t.jsonl"), "{}\n").expect("write");
        assert!(matches!(
            workspaces.commit(&workspace, "work").expect("commit"),
            Commit::Created(_)
        ));
        std::fs::write(view.join("loose.txt"), "loose\n").expect("write");

        let waits = workspaces.set_aside_when(&branch, None, "spawn: set aside #1", &|| {
            Err("a turn runs".to_string())
        });
        assert!(
            matches!(&waits, Err(WorkspaceError::Refused { code, .. }) if code == spawn_host::SET_ASIDE_WAITS),
            "{name}: {waits:?}"
        );
        assert!(view.join("loose.txt").is_file(), "{name}: nothing changed");
        let made = workspaces
            .set_aside_when(&branch, None, "spawn: set aside #1", &|| Ok(()))
            .expect("set aside");
        let SetAside::Created { commit, files, .. } = &made else {
            panic!("{name}: {made:?}");
        };
        assert_eq!(files, &["README.md", "loose.txt"], "{name}");
        let tree = |commit: &str| {
            git(&[
                "--git-dir",
                &repo.to_string_lossy(),
                "rev-parse",
                &format!("{commit}^{{tree}}"),
            ])
            .expect("tree")
        };
        assert_eq!(tree(commit.as_str()), tree(&main), "{name}");
        assert_eq!(
            std::fs::read_to_string(view.join("README.md")).expect("read"),
            "base\n",
            "{name}"
        );
        assert!(!view.join("loose.txt").exists(), "{name}");
        // The transcript under the build output is never touched.
        assert!(view.join("target/.transcripts/t.jsonl").is_file(), "{name}");
        assert_eq!(
            workspaces
                .set_aside_when(&branch, None, "spawn: set aside #1", &|| Ok(()))
                .expect("again"),
            made,
            "{name}"
        );
        assert_eq!(
            workspaces.commit(&workspace, "idle").expect("commit"),
            Commit::Unchanged,
            "{name}"
        );
        let busy = workspaces.unmount_when(&workspace, &|| {
            Err(WorkspaceError::Refused {
                code: UNMOUNT_IN_USE.to_string(),
                message: "in use".to_string(),
            })
        });
        assert!(busy.is_err() && workspaces.is_mounted(&workspace), "{name}");
        std::fs::remove_dir_all(view.join("target")).expect("clean");
        workspaces
            .unmount_when(&workspace, &|| Ok(()))
            .expect("unmount");
        assert!(!workspaces.is_mounted(&workspace), "{name}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The commit of a mission's agent runs through the routed store's
    /// commit-or-take-back: a branch gone under the mounted workspace is
    /// taken back (or made again) under its lock.
    #[test]
    fn a_gone_branch_is_made_again_by_every_engine_through_the_routed_commit() {
        use crate::config::{Probes, WorkspaceBackend, WorkspaceSettings};
        let base = std::env::temp_dir().join(format!("spawn-sources-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = backend_repo(&base);
        let configured = WorkspaceBackend::Named(spawn_host::WorkspaceBackend::Git);
        let root = base.join(format!("{configured:?}").to_lowercase());
        let settings = WorkspaceSettings::resolve(
            configured,
            &root,
            &Probes::host(),
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        )
        .expect("resolve git");
        let routed = Routed {
            store: std::sync::Arc::new(settings.store(&repo, &root)),
            // A source elsewhere: nothing to take back from this host.
            mirror: Some((repo.clone(), "https://example.invalid/r.git".to_string())),
        };
        let branch = BranchName::new("agent_gone").expect("branch");
        routed.store.ensure_branch(&branch, None).expect("branch");
        let workspace = routed.store.mount(&branch).expect("mount");
        let view = std::path::PathBuf::from(workspace.path());
        std::fs::write(view.join("new.txt"), "new\n").expect("write");
        git(&[
            "--git-dir",
            &repo.to_string_lossy(),
            "update-ref",
            "-d",
            "refs/heads/agent_gone",
        ])
        .expect("delete the branch");
        let commit = commit_routed(&routed, &workspace, "work").expect("commit");
        assert!(
            matches!(commit, Commit::Recreated { .. }),
            "{}: {commit:?}",
            settings.active.name()
        );
        routed.store.unmount(&workspace).expect("unmount");
        git(&[
            "--git-dir",
            &repo.to_string_lossy(),
            "update-ref",
            "-d",
            "refs/heads/agent_gone",
        ])
        .expect("clean up");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worker_commits_persist_through_mirror_to_source_across_unmount_and_gc() {
        use super::{persist_worker_commits_to_repo, ref_commit};
        use std::path::PathBuf;

        let base = std::env::temp_dir().join(format!(
            "spawn-sources-worker-commits-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let source = base.join("source.git");
        let work = base.join("work");
        let seed = base.join("seed");
        std::fs::create_dir_all(&work).expect("work");
        std::fs::create_dir_all(&seed).expect("seed");
        git(&["init", "--bare", "-q", &source.to_string_lossy()]).expect("init bare source");
        git(&[
            "-C",
            &seed.to_string_lossy(),
            "init",
            "-q",
            "--initial-branch=main",
        ])
        .expect("init seed");
        std::fs::write(seed.join("README.md"), "root\n").expect("write readme");
        git(&["-C", &seed.to_string_lossy(), "add", "-A"]).expect("add seed");
        git(&["-C", &seed.to_string_lossy(), "commit", "-qm", "root"]).expect("commit seed");
        git(&[
            "-C",
            &seed.to_string_lossy(),
            "push",
            "-q",
            &source.to_string_lossy(),
            "HEAD:refs/heads/main",
        ])
        .expect("push main");

        let source_str = source.to_string_lossy().to_string();
        let mirror = ensure_mirror(&work, Some(&source_str)).expect("ensure mirror");
        let store =
            spawn_host::GitWorkspace::new(&mirror, &work, spawn_host::DEFAULT_MAX_FILE_BYTES);
        let branch = BranchName::new("agent_worker_m").expect("branch");
        store.ensure_branch(&branch, None).expect("ensure branch");
        let ws = store.mount(&branch).expect("mount");
        let ws_dir = PathBuf::from(ws.path());

        let ws_str = ws_dir.to_string_lossy().to_string();
        git(&["-C", &ws_str, "init", "-q", "--initial-branch=main"]).expect("worker init");
        std::fs::write(ws_dir.join("work.txt"), "one\n").expect("write work");
        git(&["-C", &ws_str, "add", "-A"]).expect("worker add");
        git(&["-C", &ws_str, "commit", "-qm", "authored work"]).expect("worker commit");
        let sha = git(&["-C", &ws_str, "rev-parse", "HEAD"]).expect("worker head");

        let pinned = persist_worker_commits_to_repo(
            &mirror,
            Some(&source_str),
            &work,
            branch.as_str(),
            77,
            "head",
        )
        .expect("persist head")
        .expect("sha returned");
        assert_eq!(pinned, sha);

        store.unmount(&ws).expect("unmount");
        assert!(!ws_dir.exists());

        let accepted = persist_worker_commits_to_repo(
            &mirror,
            Some(&source_str),
            &work,
            branch.as_str(),
            77,
            "accepted",
        )
        .expect("persist accepted")
        .expect("sha returned post unmount");
        assert_eq!(accepted, sha);

        git(&[
            "--git-dir",
            &source_str,
            "gc",
            "--prune=now",
            "--aggressive",
            "-q",
        ])
        .expect("source gc");
        for r in [
            spawn_host::workspace_rules::worker_head_ref(branch.as_str()),
            spawn_host::workspace_rules::task_head_ref(77, "head"),
            spawn_host::workspace_rules::task_head_ref(77, "accepted"),
        ] {
            assert_eq!(
                ref_commit(&source, &r).as_deref(),
                Some(sha.as_str()),
                "{r}"
            );
        }
        assert_eq!(
            git(&["--git-dir", &source_str, "cat-file", "-t", &sha]).expect("cat-file"),
            "commit"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mission_workspaces_d6_native_lifecycle_with_l1_tmpfs_and_zero_git() {
        use crate::config::{D6RuntimeConfig, Probes, WorkspaceBackend, WorkspaceSettings};

        let schema = spawn_postgres::testing::TestSchema::new("mw_d6_native");
        let store = std::sync::Arc::new(schema.store());
        let base = std::env::temp_dir().join(format!(
            "spawn-sources-d6-native-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let tmpfs_dir = std::path::PathBuf::from(format!(
            "/dev/shm/spawn-d6-step7-mw-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&tmpfs_dir);
        std::fs::create_dir_all(&base).expect("base");

        let root = base.join("d6-root");
        let settings = WorkspaceSettings::resolve(
            WorkspaceBackend::Named(spawn_host::WorkspaceBackend::D6),
            &root,
            &Probes::host(),
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        )
        .expect("resolve")
        .with_d6(D6RuntimeConfig {
            cas_dir: None,
            tmpfs_root: Some(tmpfs_dir.clone()),
            tmpfs_budget_mb: Some(128),
        });
        settings.claim(&root).expect("claim d6");
        assert!(
            spawn_host::claim_root(&root, spawn_host::WorkspaceBackend::Git).is_err(),
            "claimed d6 root must refuse git"
        );

        let mission = store
            .mission_add(
                "d6 native mission",
                &serde_json::json!(["https://example.invalid/d6/proj.git"]),
                "human",
            )
            .expect("mission_add");
        let _ = store
            .task_assign(mission, "ephemeral", "agent_mw_d6", "d6 work", "default")
            .expect("task_assign");

        let default_proj = base.join("nonexistent-default.git");
        let factory = settings.factory(&root);
        let workspaces =
            super::MissionWorkspaces::new(factory(&default_proj), &root, store.clone(), factory);
        let branch = BranchName::new("agent_mw_d6").expect("branch");
        workspaces
            .ensure_branch(&branch, None)
            .expect("ensure_branch");
        let ws = workspaces.mount_when(&branch, &|| Ok(())).expect("mount");
        assert!(workspaces.is_mounted(&ws));
        assert!(
            Path::new(ws.path()).starts_with(spawn_host::canonical_root(&tmpfs_dir)),
            "expected L1 tmpfs mount under {}, got {}",
            tmpfs_dir.display(),
            ws.path()
        );
        assert!(workspaces.hold(&ws).is_some());

        std::fs::write(
            Path::new(ws.path()).join("lib.rs"),
            "pub fn ok() -> bool { true }\n",
        )
        .expect("write");
        assert!(matches!(
            workspaces.commit(&ws, "d6 native commit").expect("commit"),
            Commit::Created(_)
        ));
        assert_eq!(
            workspaces.commit(&ws, "noop").expect("noop"),
            Commit::Unchanged
        );
        let routed = workspaces.route(branch.as_str()).expect("routed");
        assert_eq!(routed.store.backend_name(), "d6");
        assert!(
            routed
                .store
                .d6_workspace()
                .expect("d6")
                .l1_policy()
                .is_some()
        );
        assert!(routed.store.d6_l1_reserved_bytes().is_some_and(|b| b > 0));
        assert!(
            !root.join(".sources").exists(),
            "native D6 route must never clone git mirrors under .sources"
        );
        workspaces.unmount_when(&ws, &|| Ok(())).expect("unmount");
        assert!(!workspaces.is_mounted(&ws));
        assert!(!store.list_refs(None).expect("list_refs").is_empty());

        let _ = std::fs::remove_dir_all(&tmpfs_dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn protected_task_ref_matches_task_namespace() {
        assert!(super::is_protected_task_ref(
            &spawn_host::workspace_rules::task_head_ref(42, "head")
        ));
        assert!(!super::is_protected_task_ref(
            &spawn_host::workspace_rules::worker_head_ref("w-active")
        ));
        assert!(!super::is_protected_task_ref("refs/heads/spawn/w-active"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn d6_persist_and_check_task_worker_commits_without_git_mirror() {
        use crate::config::{D6RuntimeConfig, Probes, WorkspaceBackend, WorkspaceSettings};

        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("d6_pin_worker");
        let store = std::sync::Arc::new(
            spawn_postgres::PostgresStore::connect_in_schema_with_options(
                &url,
                &schema,
                &spawn_postgres::testing::test_connect_options(),
            )
            .expect("connect"),
        );
        let base = std::env::temp_dir().join(format!(
            "spawn-sources-d6-pin-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");

        let root = base.join("d6-root");
        let settings = WorkspaceSettings::resolve(
            WorkspaceBackend::Named(spawn_host::WorkspaceBackend::D6),
            &root,
            &Probes::host(),
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        )
        .expect("resolve")
        .with_d6(D6RuntimeConfig::default());
        settings.claim(&root).expect("claim d6");

        let source = "https://example.invalid/d6/pin.git";
        let mission = store
            .mission_add("d6 pin mission", &serde_json::json!([source]), "human")
            .expect("mission_add");
        let task_id = store
            .task_assign(mission, "ephemeral", "agent_d6_pin", "pin work", "default")
            .expect("task_assign");
        let mut pg_client = postgres::Client::connect(&url, postgres::NoTls).expect("pg");
        pg_client
            .batch_execute(&format!("set search_path to {schema}"))
            .expect("schema");
        let agent_id: String = pg_client
            .query_one(
                "select agent_id from mission_tasks where id = $1",
                &[&task_id],
            )
            .expect("task row")
            .get(0);

        let default_proj = base.join("nonexistent-default.git");
        let factory = settings.factory(&root);
        let workspaces =
            super::MissionWorkspaces::new(factory(&default_proj), &root, store.clone(), factory);
        let branch = BranchName::new(&agent_id).expect("branch");
        workspaces
            .ensure_branch(&branch, None)
            .expect("ensure_branch");
        let ws = workspaces.mount(&branch).expect("mount");
        std::fs::write(Path::new(ws.path()).join("work.txt"), "worker work\n").expect("write");
        let Commit::Created(head_commit) = workspaces.commit(&ws, "worker commit").expect("commit")
        else {
            panic!("expected Commit::Created");
        };
        workspaces.unmount(&ws).expect("unmount");
        let head_hex = head_commit.as_str().to_string();

        assert!(
            !super::task_worker_head_pinned_with_settings(
                &store,
                &settings,
                Some(&default_proj),
                &root,
                &agent_id,
                task_id,
                "head",
            ),
            "must not be pinned before persist"
        );
        assert!(
            !root.join(".sources").exists(),
            "d6 task_worker_head_pinned must not clone bare git mirrors"
        );

        let pinned_head = super::persist_task_worker_commits_with_settings(
            &store,
            &settings,
            Some(&default_proj),
            &root,
            &agent_id,
            task_id,
            "head",
        )
        .expect("persist head")
        .expect("pinned head");
        assert_eq!(pinned_head, head_hex);
        assert!(super::task_worker_head_pinned_with_settings(
            &store,
            &settings,
            Some(&default_proj),
            &root,
            &agent_id,
            task_id,
            "head",
        ));

        let pinned_accepted = super::persist_task_worker_commits_with_settings(
            &store,
            &settings,
            Some(&default_proj),
            &root,
            &agent_id,
            task_id,
            "accepted",
        )
        .expect("persist accepted")
        .expect("pinned accepted");
        assert_eq!(pinned_accepted, head_hex);
        assert!(super::task_worker_head_pinned_with_settings(
            &store,
            &settings,
            Some(&default_proj),
            &root,
            &agent_id,
            task_id,
            "accepted",
        ));

        let repo_key = super::mirror_path(&root, Some(source))
            .expect("mirror_path")
            .to_string_lossy()
            .into_owned();
        let refs = store.list_refs(Some(&repo_key)).expect("list_refs");
        for expected_ref in [
            spawn_host::workspace_rules::worker_head_ref(&agent_id),
            spawn_host::workspace_rules::task_head_ref(task_id, "head"),
            spawn_host::workspace_rules::task_head_ref(task_id, "accepted"),
        ] {
            assert!(
                refs.iter().any(|r| r.name == expected_ref),
                "missing D6 ledger ref {expected_ref} in {refs:?}"
            );
        }
        assert!(
            !root.join(".sources").exists(),
            "native D6 pin must never create bare git mirrors under .sources"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    fn publish_test_ref(
        cas: &spawn_host::CasMerkleStore,
        ledger: &dyn spawn_host::RefLedger,
        project: &str,
        ref_name: &str,
        kind: spawn_host::D6RefKind,
        entry: (&[u8], &[u8]),
        at_ms: i64,
    ) -> spawn_cas::Digest {
        let (file_name, content) = entry;
        let (blob, size) = cas.put_bytes_blob(content).expect("blob");
        let tree = cas
            .put_tree_cached(
                &spawn_cas::Tree::from_entries(vec![spawn_cas::TreeEntry::file(
                    file_name.to_vec(),
                    &blob,
                    size,
                )])
                .expect("tree"),
            )
            .expect("put tree");
        let body = spawn_host::format_commit_body(
            "t <t@t> 1000 +0000",
            "t <t@t> 1000 +0000",
            &format!("{project}:{ref_name}\n"),
        );
        cas.publish_snapshot(
            ledger,
            &spawn_host::PublishRequest {
                project,
                ref_name,
                kind,
                expected_ref: None,
                tree: &tree,
                parents: &[],
                commit_body: &body,
                created_at_ms: at_ms,
                reason: "test:publish",
                landing_task_id: None,
            },
        )
        .expect("publish")
        .snapshot
    }

    fn run_cutover_steps(
        cutover: &mut spawn_host::ProjectCutover,
        cas: &spawn_host::CasMerkleStore,
        ledger: &super::PostgresRefLedger,
        border: &spawn_host::GitBorderAdapter,
        reason: &str,
    ) {
        cutover
            .step1_backfill_history(cas, ledger)
            .expect("step1_backfill_history");
        cutover
            .step2_pause_dispatch_and_verify_clean(cas)
            .expect("step2_pause_dispatch_and_verify_clean");
        cutover
            .step3_import_tail_commits(cas, ledger)
            .expect("step3_import_tail_commits");
        cutover
            .step4_commit_refs_and_flip_mode(cas, ledger, border, reason)
            .expect("step4_commit_refs_and_flip_mode");
        cutover
            .step5_verify_parity(cas, ledger)
            .expect("step5_verify_parity");
        cutover
            .step6_configure_readonly_border(border)
            .expect("step6_configure_readonly_border");
        cutover
            .step7_resume_dispatch()
            .expect("step7_resume_dispatch");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn pg_ref_ledger_adapters_retire_project_refs_audit_and_preserve_snapshots_and_other_projects()
    {
        use spawn_host::{CasMerkleStore, D6RefKind, RefLedger};
        use spawn_postgres::SnapshotState;

        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("d6_retire_adapters");
        let store = std::sync::Arc::new(
            spawn_postgres::PostgresStore::connect_in_schema_with_options(
                &url,
                &schema,
                &spawn_postgres::testing::test_connect_options(),
            )
            .expect("connect"),
        );
        let base = std::env::temp_dir().join(format!(
            "spawn-sources-retire-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let cas = CasMerkleStore::open(base.join("cas")).expect("cas");

        let pg_ledger = super::PostgresRefLedger::new(std::sync::Arc::clone(&store));
        let lazy_ledger = super::LazyPgRefLedger {
            store: std::sync::Mutex::new(Some(std::sync::Arc::clone(&store))),
        };
        let borrowed_ledger = super::BorrowedPgRefLedger::new(&store);

        let mut retired_snaps = Vec::new();
        for (idx, (project, ledger_ref)) in [
            ("proj-pg", &pg_ledger as &dyn RefLedger),
            ("proj-lazy", &lazy_ledger as &dyn RefLedger),
            ("proj-borrowed", &borrowed_ledger as &dyn RefLedger),
        ]
        .into_iter()
        .enumerate()
        {
            let offset = i64::try_from(idx).expect("idx") * 10_000;
            let s_main = publish_test_ref(
                &cas,
                ledger_ref,
                project,
                "main",
                D6RefKind::Main,
                (b"README.md", format!("main:{project}\n").as_bytes()),
                1_000_000 + offset,
            );
            let s_branch = publish_test_ref(
                &cas,
                ledger_ref,
                project,
                "feat/work",
                D6RefKind::Branch,
                (b"WORK.md", format!("branch:{project}\n").as_bytes()),
                1_001_000 + offset,
            );
            retired_snaps.push(s_main);
            retired_snaps.push(s_branch);
        }

        let other_main = publish_test_ref(
            &cas,
            &pg_ledger,
            "proj-other",
            "main",
            D6RefKind::Main,
            (b"OTHER.md", b"keep-main\n"),
            2_000_000,
        );
        let other_branch = publish_test_ref(
            &cas,
            &pg_ledger,
            "proj-other",
            "topic/keep",
            D6RefKind::Branch,
            (b"TOPIC.md", b"keep-branch\n"),
            2_001_000,
        );

        assert!(pg_ledger.retire_project_refs("   ", "blank").is_err());

        let seq_before = store.latest_ref_log_seq().expect("seq before");
        assert_eq!(
            (&pg_ledger as &dyn RefLedger)
                .retire_project_refs("proj-pg", "test:pg-retire")
                .expect("retire pg"),
            2
        );
        assert_eq!(
            (&lazy_ledger as &dyn RefLedger)
                .retire_project_refs("proj-lazy", "test:lazy-retire")
                .expect("retire lazy"),
            2
        );
        assert_eq!(
            (&borrowed_ledger as &dyn RefLedger)
                .retire_project_refs("proj-borrowed", "test:borrowed-retire")
                .expect("retire borrowed"),
            2
        );

        for project in ["proj-pg", "proj-lazy", "proj-borrowed"] {
            assert!(
                store.list_refs(Some(project)).expect("list").is_empty(),
                "{project} must have zero active spawn_refs after retire"
            );
            assert_eq!(
                pg_ledger
                    .retire_project_refs(project, "repeat")
                    .expect("repeat retire"),
                0,
                "repeating retire for {project} must be a 0-row no-op"
            );
        }

        let kept_refs = store.list_refs(Some("proj-other")).expect("other refs");
        assert_eq!(kept_refs.len(), 2);
        assert!(
            kept_refs
                .iter()
                .any(|r| r.name == "main" && r.snapshot == other_main.to_string())
        );
        assert!(
            kept_refs
                .iter()
                .any(|r| r.name == "topic/keep" && r.snapshot == other_branch.to_string())
        );

        for snap in &retired_snaps {
            let row = store
                .get_snapshot(&snap.to_string())
                .expect("get_snapshot")
                .expect("retired snapshot row must stay intact");
            assert_eq!(row.state, SnapshotState::Live);
        }

        let log_rows = store
            .list_ref_log_after(i64::try_from(seq_before).expect("seq fits i64"))
            .expect("list_ref_log_after");
        assert_eq!(log_rows.len(), 6);
        for (project, reason) in [
            ("proj-pg", "test:pg-retire"),
            ("proj-lazy", "test:lazy-retire"),
            ("proj-borrowed", "test:borrowed-retire"),
        ] {
            let matches: Vec<_> = log_rows
                .iter()
                .filter(|row| row.project == project && row.reason == reason)
                .collect();
            assert_eq!(matches.len(), 2, "missing audit rows for {project}");
            for row in matches {
                assert_eq!(row.old_snapshot.as_deref(), Some(row.new_snapshot.as_str()));
            }
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn pg_ref_ledger_rollback_to_git_advance_main_and_recutover_to_d6() {
        use spawn_host::{
            CasMerkleStore, D6RefKind, GitBorderAdapter, ProjectCutover, RefLedger,
            is_readonly_border, verify_cutover_parity,
        };
        use spawn_postgres::SnapshotState;

        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("d6_recutover");
        let store = std::sync::Arc::new(
            spawn_postgres::PostgresStore::connect_in_schema_with_options(
                &url,
                &schema,
                &spawn_postgres::testing::test_connect_options(),
            )
            .expect("connect"),
        );
        let ledger = super::PostgresRefLedger::new(std::sync::Arc::clone(&store));

        let scratch = std::env::temp_dir().join(format!(
            "spawn-sources-recutover-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        let bare = scratch.join("bare.git");
        let work = scratch.join("work");
        let ws_root = scratch.join("ws-root");
        std::fs::create_dir_all(&ws_root).expect("ws_root");
        let bare_s = bare.to_string_lossy().into_owned();
        let work_s = work.to_string_lossy().into_owned();

        git(&["init", "--quiet", "--bare", &bare_s]).expect("init bare");
        git(&[
            "--git-dir",
            &bare_s,
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ])
        .expect("symref");
        std::fs::create_dir_all(&work).expect("work");
        git(&["-C", &work_s, "init", "--quiet"]).expect("init work");
        git(&["-C", &work_s, "checkout", "-B", "main"]).expect("checkout main");
        git(&["-C", &work_s, "config", "user.name", "Test"]).expect("cfg name");
        git(&["-C", &work_s, "config", "user.email", "t@t"]).expect("cfg email");
        std::fs::write(work.join("README.md"), "v1\n").expect("write v1");
        git(&["-C", &work_s, "add", "README.md"]).expect("add v1");
        git(&["-C", &work_s, "commit", "-q", "-m", "m1"]).expect("commit m1");
        let m1_oid = git(&["-C", &work_s, "rev-parse", "HEAD"]).expect("m1 oid");
        git(&["-C", &work_s, "remote", "add", "origin", &bare_s]).expect("remote");
        git(&["-C", &work_s, "push", "-q", "-u", "origin", "main"]).expect("push m1");
        git(&["-C", &work_s, "checkout", "-q", "-b", "feat"]).expect("checkout feat");
        std::fs::write(work.join("FEAT.md"), "feat\n").expect("write feat");
        git(&["-C", &work_s, "add", "FEAT.md"]).expect("add feat");
        git(&["-C", &work_s, "commit", "-q", "-m", "feat"]).expect("commit feat");
        git(&["-C", &work_s, "push", "-q", "origin", "feat"]).expect("push feat");
        git(&["-C", &work_s, "checkout", "-q", "main"]).expect("back to main");

        let cas = CasMerkleStore::open(ws_root.join(".d6/cas")).expect("cas");
        let border = GitBorderAdapter::new();
        let kept_other_snap = publish_test_ref(
            &cas,
            &ledger,
            "proj-keep",
            "main",
            D6RefKind::Main,
            (b"KEEP.md", b"keep\n"),
            500_000,
        );

        let project = bare_s.clone();
        let mut cutover1 = ProjectCutover::new(&project, &bare, &ws_root);
        run_cutover_steps(&mut cutover1, &cas, &ledger, &border, "cutover-1");
        assert!(is_readonly_border(&bare).expect("ro after cutover 1"));

        let refs_before_rb = ledger.list_refs(Some(&project)).expect("refs before rb");
        let main_before_rb = refs_before_rb
            .iter()
            .find(|r| r.name == "main")
            .expect("main ref before rollback");
        let main_snap_m1 = ledger
            .get_snapshot(&main_before_rb.snapshot)
            .expect("get m1 snap")
            .expect("m1 snap exists");
        assert_eq!(main_snap_m1.git_oid.to_hex(), m1_oid);

        let seq_before_rb = store.latest_ref_log_seq().expect("seq before rb");
        let rb = cutover1
            .rollback_to_git(&cas, &ledger, &border)
            .expect("rollback_to_git");
        assert_eq!(rb.exported_refs, refs_before_rb.len());
        assert!(!is_readonly_border(&bare).expect("writable after rollback"));
        assert!(
            ledger
                .list_refs(Some(&project))
                .expect("refs after rb")
                .is_empty(),
            "rollback must retire all active spawn_refs for project"
        );
        assert_eq!(
            store
                .get_snapshot(&main_before_rb.snapshot.to_string())
                .expect("snap after rb")
                .expect("snap still present")
                .state,
            SnapshotState::Live
        );
        assert_eq!(
            ledger
                .list_refs(Some("proj-keep"))
                .expect("kept project")
                .first()
                .map(|r| r.snapshot.clone()),
            Some(kept_other_snap)
        );
        let rb_logs = store
            .list_ref_log_after(i64::try_from(seq_before_rb).expect("seq"))
            .expect("rb log");
        assert_eq!(rb_logs.len(), rb.exported_refs);
        assert!(
            rb_logs
                .iter()
                .all(|row| row.project == project && row.reason == "rollback to git")
        );

        // Advance `main` directly in Git while rolled back, then re-cut over to D6.
        std::fs::write(work.join("README.md"), "v2 from git\n").expect("write v2");
        git(&["-C", &work_s, "add", "README.md"]).expect("add v2");
        git(&["-C", &work_s, "commit", "-q", "-m", "m2"]).expect("commit m2");
        let m2_oid = git(&["-C", &work_s, "rev-parse", "HEAD"]).expect("m2 oid");
        assert_ne!(m2_oid, m1_oid);
        git(&["-C", &work_s, "push", "-q", "origin", "main"]).expect("push m2");

        let mut cutover2 = ProjectCutover::new(&project, &bare, &ws_root);
        run_cutover_steps(&mut cutover2, &cas, &ledger, &border, "cutover-2");
        assert!(is_readonly_border(&bare).expect("ro after cutover 2"));
        verify_cutover_parity(&cas, &ledger, &project, &bare).expect("parity after recutover");
        let refs_after = ledger
            .list_refs(Some(&project))
            .expect("refs after recutover");
        let main_after = refs_after
            .iter()
            .find(|r| r.name == "main")
            .expect("main ref after recutover");
        let main_snap_m2 = ledger
            .get_snapshot(&main_after.snapshot)
            .expect("get m2 snap")
            .expect("m2 snap exists");
        assert_eq!(main_snap_m2.git_oid.to_hex(), m2_oid);
        assert_eq!(
            ledger
                .list_refs(Some("proj-keep"))
                .expect("kept still")
                .len(),
            1
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    fn init_bare_repo_with_main(scratch: &std::path::Path) -> std::path::PathBuf {
        let bare = scratch.join("bare.git");
        let work = scratch.join("work");
        std::fs::create_dir_all(&bare).expect("mkdir bare");
        std::fs::create_dir_all(&work).expect("mkdir work");
        let bare_s = bare.to_string_lossy().into_owned();
        let work_s = work.to_string_lossy().into_owned();
        git(&["init", "--bare", "-q", &bare_s]).expect("init bare");
        git(&["init", "-q", &work_s]).expect("init work");
        git(&["-C", &work_s, "config", "user.name", "T"]).expect("u");
        git(&["-C", &work_s, "config", "user.email", "t@t"]).expect("e");
        std::fs::write(work.join("README.md"), "v1\n").expect("readme");
        git(&["-C", &work_s, "add", "README.md"]).expect("add");
        git(&["-C", &work_s, "commit", "-q", "-m", "m1"]).expect("commit");
        git(&["-C", &work_s, "branch", "-M", "main"]).expect("branch");
        git(&["-C", &work_s, "remote", "add", "origin", &bare_s]).expect("remote");
        git(&["-C", &work_s, "push", "-q", "origin", "main"]).expect("push");
        bare
    }

    #[test]
    fn project_storage_unifies_root_ledger_and_env_signals() {
        use super::{
            ProjectStorage, ProjectStorageQuery, project_storage_with_env,
            workspace_settings_readonly,
        };

        let scratch = std::env::temp_dir().join(format!(
            "spawn-project-storage-signals-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        let git_root = scratch.join("git-root");
        let d6_root = scratch.join("d6-root");
        for dir in [&git_root, &d6_root] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        spawn_host::claim_root(&git_root, spawn_host::WorkspaceBackend::Git).expect("claim git");
        spawn_host::claim_root(&d6_root, spawn_host::WorkspaceBackend::D6).expect("claim d6");
        let bare = init_bare_repo_with_main(&scratch);

        // Plain git roots without read-only border or ledger-only resolve to Git.
        let git_q = ProjectStorageQuery {
            root: Some(&git_root),
            git_dir: Some(&bare),
            ledger_only_d6: false,
        };
        assert_eq!(project_storage_with_env(git_q, None), ProjectStorage::Git);
        // Plain `d6` env var triggers D6 storage even before root marker inspection.
        assert_eq!(
            project_storage_with_env(ProjectStorageQuery::default(), Some(" D6 ")),
            ProjectStorage::D6
        );
        // `ledger_only_d6` applies only when no local Git repository exists.
        assert_eq!(
            project_storage_with_env(
                ProjectStorageQuery {
                    ledger_only_d6: true,
                    ..git_q
                },
                None
            ),
            ProjectStorage::Git
        );
        assert_eq!(
            project_storage_with_env(
                ProjectStorageQuery {
                    root: Some(&git_root),
                    git_dir: None,
                    ledger_only_d6: true
                },
                None,
            ),
            ProjectStorage::D6
        );
        // `d6` root marker wins even if env_backend names `git`, matching `workspace_settings_readonly`.
        assert_eq!(
            project_storage_with_env(
                ProjectStorageQuery {
                    root: Some(&d6_root),
                    git_dir: None,
                    ledger_only_d6: false
                },
                Some("git"),
            ),
            ProjectStorage::D6
        );
        if let Ok(settings) = workspace_settings_readonly(&d6_root) {
            assert_eq!(settings.active, spawn_host::WorkspaceBackend::D6);
        }

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn project_storage_respects_readonly_git_border() {
        use super::{ProjectStorage, ProjectStorageQuery, project_storage_with_env};

        let scratch = std::env::temp_dir().join(format!(
            "spawn-project-storage-border-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        let git_root = scratch.join("git-root");
        let d6_root = scratch.join("d6-root");
        std::fs::create_dir_all(&git_root).expect("mkdir git");
        std::fs::create_dir_all(&d6_root).expect("mkdir d6");
        spawn_host::claim_root(&git_root, spawn_host::WorkspaceBackend::Git).expect("claim git");
        spawn_host::claim_root(&d6_root, spawn_host::WorkspaceBackend::D6).expect("claim d6");
        let bare = init_bare_repo_with_main(&scratch);

        let border = spawn_host::GitBorderAdapter::new();
        let cas = spawn_host::CasMerkleStore::open(d6_root.join(".cas")).expect("open cas");
        let schema = spawn_postgres::testing::TestSchema::new("project_storage_ro_border");
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let pg = spawn_postgres::PostgresStore::connect_in_schema_with_options(
            &url,
            &schema,
            &spawn_postgres::testing::test_connect_options(),
        )
        .expect("test pg");
        let ledger = super::PostgresRefLedger::new(std::sync::Arc::new(pg));
        let mut cutover = spawn_host::ProjectCutover::new("proj-storage-border", &bare, &d6_root);
        run_cutover_steps(&mut cutover, &cas, &ledger, &border, "storage test");

        assert_eq!(
            project_storage_with_env(
                ProjectStorageQuery {
                    root: Some(&git_root),
                    git_dir: Some(&bare),
                    ledger_only_d6: false,
                },
                None,
            ),
            ProjectStorage::D6
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    struct TempDirGuard(std::path::PathBuf);

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn copy_cas_dir_with_journal(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).expect("mkdir dst");
        for entry in std::fs::read_dir(src).expect("read src") {
            let entry = entry.expect("dir entry");
            let name = entry.file_name();
            if name == ".gc-fence" || name == ".lock" || name == ".refs-journal.lock" {
                continue;
            }
            let from = entry.path();
            let to = dst.join(&name);
            let ft = entry.file_type().expect("file type");
            if ft.is_dir() {
                copy_cas_dir_with_journal(&from, &to);
            } else if ft.is_file() {
                std::fs::copy(&from, &to).expect("copy cas entry");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = std::fs::metadata(&from).expect("meta").permissions().mode() & 0o777;
                    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))
                        .expect("chmod copy");
                }
            }
        }
    }

    fn open_bound_cas_and_pg(
        schema_name: &str,
        cas_dir: &Path,
    ) -> (
        spawn_postgres::testing::TestSchema,
        std::sync::Arc<spawn_postgres::PostgresStore>,
        std::sync::Arc<spawn_host::CasMerkleStore>,
        spawn_cas::GcFenceBinding,
    ) {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new(schema_name);
        let pg = std::sync::Arc::new(
            spawn_postgres::PostgresStore::connect_in_schema_with_options(
                &url,
                &schema,
                &spawn_postgres::testing::test_connect_options(),
            )
            .expect("connect"),
        );
        let cas = std::sync::Arc::new(spawn_host::CasMerkleStore::open(cas_dir).expect("open cas"));
        let ident = pg.database_identity().expect("db ident");
        let binding = spawn_cas::GcFenceBinding {
            db_system_id: ident.db_system_id,
            db_oid: ident.db_oid,
            schema_name: ident.schema_name,
        };
        spawn_host::bind_store_gc_fence(&cas, &binding).expect("bind gc fence");
        (schema, pg, cas, binding)
    }

    fn replay_cli_journal_into_fresh_pg(
        cas_dir: &Path,
        restored_dir: &Path,
    ) -> Vec<spawn_host::LedgerRef> {
        copy_cas_dir_with_journal(cas_dir, restored_dir);
        let (_schema, pg, restored_cas, binding) =
            open_bound_cas_and_pg("d6_cli_pin_journal_restore", restored_dir);
        let ledger = super::PostgresRefLedger::new(std::sync::Arc::clone(&pg));
        let report =
            spawn_host::reconcile_refs_journal(&restored_cas, &ledger, 0, "test_cli_pin_reconcile")
                .expect("reconcile_refs_journal recovers pinned refs");
        assert!(report.entries_replayed >= 4);
        let restored_seq = pg.latest_ref_log_seq().expect("restored seq");
        let fence = spawn_host::check_gc_fence(&restored_cas, &binding, restored_seq)
            .expect("restored gc fence aligned");
        assert_eq!(fence.pg_latest_ref_seq, fence.journal_latest_seq);
        spawn_host::RefLedger::list_refs(&ledger, None).expect("list restored refs")
    }

    #[test]
    fn d6_pin_worker_head_journals_worker_and_target_refs_and_survives_replay() {
        use spawn_core::port::WorkspaceStore as _;

        let base = TempDirGuard(std::env::temp_dir().join(format!(
            "spawn-sources-d6-pin-journal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        std::fs::create_dir_all(&base.0).expect("mkdir base");
        let cas_dir = base.0.join("cas");
        let (_schema, store, cas, binding) = open_bound_cas_and_pg("d6_cli_pin_journal", &cas_dir);
        let ledger =
            std::sync::Arc::new(super::PostgresRefLedger::new(std::sync::Arc::clone(&store)));
        let project = "proj-cli-pin-journal";
        let _main0 = publish_test_ref(
            &cas,
            ledger.as_ref(),
            project,
            "main",
            spawn_host::D6RefKind::Main,
            (b"README.md", b"base\n"),
            1_700_000_000_000,
        );
        let d6 = spawn_host::D6Workspace::new(
            project,
            base.0.join("work"),
            std::sync::Arc::clone(&cas),
            ledger,
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        );
        let branch = BranchName::new("agent_pin").expect("branch");
        d6.ensure_branch(&branch, None).expect("ensure_branch");
        let ws = d6.mount(&branch).expect("mount");
        std::fs::write(
            Path::new(ws.path()).join("feature.rs"),
            "pub fn v() -> i32 { 7 }\n",
        )
        .expect("write");
        let Commit::Created(head_commit) = d6.checkpoint(&ws, "worker commit").expect("ckpt")
        else {
            panic!("expected Commit::Created");
        };
        d6.unmount(&ws).expect("unmount");
        let head_snap = d6
            .branch_snapshot(branch.as_str())
            .expect("branch_snapshot")
            .expect("present");

        for bad_agent in ["", "-bad", "a..b", "a@{b"] {
            let err = super::d6_pin_worker_head(&d6, bad_agent, &[]).unwrap_err();
            assert!(err.contains("invalid branch name"), "{bad_agent}: {err}");
        }
        let bad_target =
            super::d6_pin_worker_head(&d6, branch.as_str(), &["refs/heads/main"]).unwrap_err();
        assert!(bad_target.contains("refs/spawn/"), "{bad_target}");

        let head_ref = spawn_host::workspace_rules::task_head_ref(88, "head");
        let extra_ref = spawn_host::workspace_rules::task_head_ref(88, "extra");
        let accepted_ref = spawn_host::workspace_rules::task_head_ref(88, "accepted");
        let worker_ref = spawn_host::workspace_rules::worker_head_ref(branch.as_str());
        assert_eq!(
            super::d6_pin_worker_head(&d6, branch.as_str(), &[&head_ref, &extra_ref])
                .expect("pin head+extra")
                .as_deref(),
            Some(head_commit.as_str())
        );
        assert_eq!(
            super::d6_pin_worker_head(&d6, branch.as_str(), &[&accepted_ref])
                .expect("pin accepted")
                .as_deref(),
            Some(head_commit.as_str())
        );

        let entries = cas.cas().read_ref_journal().expect("read refs.journal");
        assert!(entries.windows(2).all(|w| w[1].seq > w[0].seq));
        let suffix = cas
            .cas()
            .reconcile_entries_after(1)
            .expect("reconcile_entries_after(1) valid");
        assert!(suffix.len() >= 5);
        let pg_seq = store.latest_ref_log_seq().expect("pg seq");
        let fence = spawn_host::check_gc_fence(&cas, &binding, pg_seq).expect("gc fence aligned");
        assert_eq!(fence.pg_latest_ref_seq, fence.journal_latest_seq);

        let restored = replay_cli_journal_into_fresh_pg(&cas_dir, &base.0.join("cas-restored"));
        for expected in [&worker_ref, &head_ref, &extra_ref, &accepted_ref] {
            assert!(
                entries.iter().any(|e| &e.name == expected),
                "missing journal entry for {expected}"
            );
            assert!(
                restored.iter().any(|r| {
                    r.project == project && &r.name == expected && r.snapshot == head_snap.id
                }),
                "missing restored ref {expected} in {restored:?}"
            );
        }
    }

    fn verify_max_file_cap_on_branch(
        workspaces: &super::MissionWorkspaces,
        backend: &str,
        repo: &Path,
        branch_name: &str,
        small_file: &str,
        big_file: &str,
    ) {
        let branch = BranchName::new(branch_name).expect("branch");
        workspaces
            .ensure_branch(&branch, None)
            .expect("ensure_branch");
        let routed = workspaces.route(branch.as_str()).expect("routed");
        assert_eq!(routed.store.max_file_bytes(), 1 << 20, "{backend}");

        let ws = workspaces.mount(&branch).expect("mount");
        let ws_dir = std::path::PathBuf::from(ws.path());
        std::fs::write(ws_dir.join(small_file), "ok\n").expect("write small");
        std::fs::write(ws_dir.join(big_file), vec![b'B'; 2 << 20]).expect("write big");
        let Commit::Created(commit) = workspaces.commit(&ws, "cap check").expect("commit") else {
            panic!("{backend}: expected Commit::Created");
        };

        let notices = workspaces.notices(&ws);
        assert_eq!(notices.len(), 1, "{backend}: {notices:?}");
        assert!(
            notices[0].starts_with(&format!("{big_file} (2 MiB) is over the 1 MiB cap")),
            "{backend}: {notices:?}"
        );

        let primary_records =
            spawn_host::workspace_rules::oversized_record(routed.store.root(), branch.as_str());
        assert_eq!(primary_records.len(), 1, "{backend}: {primary_records:?}");
        assert_eq!(primary_records[0].path.as_str(), Some(big_file));
        assert_eq!(primary_records[0].size, 2 << 20);

        if let Some(d6) = routed.store.d6_workspace() {
            let snap = d6
                .branch_snapshot(branch.as_str())
                .expect("branch_snapshot")
                .expect("snapshot present");
            let flat = d6.store().flatten_tree(&snap.tree).expect("flatten_tree");
            assert!(
                flat.contains_key(small_file.as_bytes()),
                "{backend}: missing {small_file}"
            );
            assert!(
                !flat.contains_key(big_file.as_bytes()),
                "{backend}: committed oversized {big_file}"
            );
        } else {
            let repo_str = repo.to_string_lossy().to_string();
            let listed = git(&[
                "--git-dir",
                &repo_str,
                "ls-tree",
                "-r",
                "--name-only",
                commit.as_str(),
            ])
            .expect("ls-tree");
            let names: Vec<&str> = listed.lines().collect();
            assert!(
                names.contains(&small_file),
                "{backend}: missing {small_file} in {names:?}"
            );
            assert!(
                !names.contains(&big_file),
                "{backend}: committed oversized {big_file} in {names:?}"
            );
        }

        workspaces.unmount(&ws).expect("unmount");
    }

    #[test]
    fn workspace_max_file_mb_enforced_through_from_env_and_mission_workspaces_across_backends() {
        const CHILD_ENV: &str = "SPAWN_TEST_MAX_FILE_CAP_BACKEND";
        const CAP_TEST: &str = "sources::tests::workspace_max_file_mb_enforced_through_from_env_and_mission_workspaces_across_backends";

        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );

        let Ok(backend) = std::env::var(CHILD_ENV) else {
            let exe = std::env::current_exe().expect("current_exe");
            for backend in ["git", "d6"] {
                let mut cmd = std::process::Command::new(&exe);
                cmd.args(["--exact", CAP_TEST, "--nocapture"])
                    .env(CHILD_ENV, backend)
                    .env("SPAWN_WORKSPACE_BACKEND", backend)
                    .env("SPAWN_MAX_FILE_MB", "1")
                    .env_remove("SPAWN_D6_CAS_DIR")
                    .env_remove("SPAWN_D6_TMPFS_ROOT")
                    .env_remove("SPAWN_D6_TMPFS_BUDGET_MB");
                let out = cmd.output().expect("spawn child cap check");
                assert!(
                    out.status.success(),
                    "backend={backend} failed:\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            return;
        };

        let schema = spawn_postgres::testing::TestSchema::new("mw_max_file_cap");
        let store = std::sync::Arc::new(
            spawn_postgres::PostgresStore::connect_in_schema_with_options(
                &url,
                &schema,
                &spawn_postgres::testing::test_connect_options(),
            )
            .expect("connect"),
        );
        let base = TempDirGuard(std::env::temp_dir().join(format!(
            "spawn-sources-cap-{backend}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        std::fs::create_dir_all(&base.0).expect("mkdir base");
        let repo = backend_repo(&base.0);
        let root = base.0.join("ws-root");

        let settings = crate::config::WorkspaceSettings::from_env(&root).expect("from_env");
        assert_eq!(settings.max_file_bytes, 1 << 20, "{backend}");
        assert_eq!(settings.active.name(), backend);

        let factory = settings.factory(&root);
        let workspaces =
            super::MissionWorkspaces::new(factory(&repo), &root, store.clone(), factory);
        verify_max_file_cap_on_branch(
            &workspaces,
            &backend,
            &repo,
            "agent_cap_check",
            "small.txt",
            "big.bin",
        );

        if backend == "d6" {
            let one_shot = settings.one_shot(&repo, &root, store);
            verify_max_file_cap_on_branch(
                &one_shot,
                "d6-one-shot",
                &repo,
                "agent_cap_oneshot",
                "small2.txt",
                "big2.bin",
            );
        }
    }

    #[test]
    fn rebind_d6_store_preserves_single_shared_max_file_bytes_on_d6() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("rebind_d6_cap");
        let pg = std::sync::Arc::new(
            spawn_postgres::PostgresStore::connect_in_schema_with_options(
                &url,
                &schema,
                &spawn_postgres::testing::test_connect_options(),
            )
            .expect("connect"),
        );
        let base = TempDirGuard(std::env::temp_dir().join(format!(
            "spawn-rebind-d6-cap-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        std::fs::create_dir_all(&base.0).expect("mkdir");
        let repo = backend_repo(&base.0);
        let root = base.0.join("ws-root");
        let cas = std::sync::Arc::new(
            spawn_host::CasMerkleStore::open(root.join(".d6/cas")).expect("cas"),
        );
        let initial_ledger: super::DynRefLedger =
            std::sync::Arc::new(spawn_host::InMemoryRefLedger::default());
        let cap: u64 = 3 << 20;
        let tmpfs_dir = base.0.join("l1-tmpfs");
        let opts = spawn_host::workspace::D6StoreOptions::new(cas, initial_ledger, cap)
            .with_l1_policy(spawn_host::d6_workspace::L1TmpfsPolicy::new(&tmpfs_dir, 64));
        let local =
            super::LocalWorkspaceStore::d6(repo.to_string_lossy().into_owned(), &root, opts);
        assert_eq!(local.max_file_bytes(), cap);
        let spawn_host::LocalWorkspaceStore::D6(ref before) = local else {
            panic!("expected D6");
        };
        assert_eq!(before.max_file_bytes(), cap);
        assert_eq!(before.l1_policy().map(|p| p.tmpfs_budget_mb), Some(64));

        let rebound = super::rebind_d6_store(local, &pg);
        assert_eq!(rebound.max_file_bytes(), cap);
        let spawn_host::LocalWorkspaceStore::D6(ref after) = rebound else {
            panic!("expected D6");
        };
        assert_eq!(after.max_file_bytes(), cap);
        let policy = after.l1_policy().expect("l1_policy preserved");
        assert_eq!(policy.tmpfs_budget_mb, 64);
    }
}

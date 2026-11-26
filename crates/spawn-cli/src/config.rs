//! Configuration read from the environment and flags, and the store it
//! points at.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its hook and runner need.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use spawn_postgres::PostgresStore;

pub(crate) fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
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

pub(crate) fn open_store() -> Result<PostgresStore, String> {
    let url = required_env("SPAWN_DATABASE_URL")?;
    PostgresStore::connect(&url).map_err(|error| error.to_string())
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
    configured
        .or_else(|| current.map(|path| path.display().to_string()))
        .unwrap_or_else(|| "spawn".to_string())
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

/// What `SPAWN_WORKSPACE_BACKEND` asks for (docs/storage.md, "Workspace
/// backends"): an engine by name, or `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceBackend {
    /// Git working copies: the default.
    Git,
    /// Copy-on-write clones of a per-commit baseline.
    Cow,
    /// An unprivileged overlay per workspace (Linux): by name only.
    Overlay,
    /// The cheapest engine the root supports that the loop serves
    /// ([`spawn_host::WorkspaceBackend::auto_with`]), chosen once at start.
    Auto,
}

impl WorkspaceBackend {
    fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Cow => "cow",
            Self::Overlay => "overlay",
            Self::Auto => "auto",
        }
    }
}

/// The backend `value` (`SPAWN_WORKSPACE_BACKEND`) names: git when it is
/// unset or empty; anything it does not name is refused, never read as git.
pub(crate) fn workspace_backend(value: Option<&str>) -> Result<WorkspaceBackend, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(WorkspaceBackend::Git),
        Some("auto") => Ok(WorkspaceBackend::Auto),
        Some(name) => match spawn_host::WorkspaceBackend::parse(name) {
            Some(spawn_host::WorkspaceBackend::Git) => Ok(WorkspaceBackend::Git),
            Some(spawn_host::WorkspaceBackend::Cow) => Ok(WorkspaceBackend::Cow),
            Some(spawn_host::WorkspaceBackend::Overlay) => Ok(WorkspaceBackend::Overlay),
            None => Err(format!(
                "SPAWN_WORKSPACE_BACKEND={name:?} names no workspace backend: use git (the \
                 default), cow, overlay or auto"
            )),
        },
    }
}

/// Whether `value` (`SPAWN_CAS_DEDUP`) turns the deduplication of build
/// artifacts on: `1` does, unset, empty or `0` does not; anything else is
/// refused.
pub(crate) fn cas_dedup(value: Option<&str>) -> Result<bool, String> {
    match value.map(str::trim) {
        None | Some("" | "0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(format!(
            "SPAWN_CAS_DEDUP={other:?} is neither 1 (deduplicate build artifacts) nor 0"
        )),
    }
}

/// How the engines' support of a root is asked (the kernel and the file
/// system in production, fixed answers in a test).
pub(crate) struct Probes<'a> {
    /// Whether an unprivileged overlay mounts under the root
    /// ([`spawn_host::overlay_supported_in`]); `Err` with the reason. Asked
    /// only when the overlay is asked for by name.
    pub(crate) overlay: &'a dyn Fn(&std::path::Path) -> Result<(), String>,
    /// Whether the root's file system clones copy-on-write
    /// ([`spawn_host::cow_clone_supported`]). Asked only for `auto` on a
    /// root no engine has yet.
    pub(crate) cow: &'a dyn Fn(&std::path::Path) -> bool,
    /// The engine the root already is ([`spawn_host::root_backend`]: its
    /// marker, else what it holds), read and never written. Asked only for
    /// `auto`, which follows it.
    pub(crate) marked:
        &'a dyn Fn(&std::path::Path) -> Result<Option<spawn_host::WorkspaceBackend>, String>,
}

impl Probes<'static> {
    /// The host's own answers. Both write under the root (a probe file, a
    /// probe mount): only a process that runs workspaces asks them, never
    /// a read like `spawn mission show` ([`WorkspaceSettings::peek`]).
    pub(crate) fn host() -> Self {
        #[cfg(target_os = "linux")]
        fn overlay(root: &std::path::Path) -> Result<(), String> {
            let _ = std::fs::create_dir_all(root);
            spawn_host::overlay_supported_in(root)
        }
        #[cfg(not(target_os = "linux"))]
        fn overlay(_root: &std::path::Path) -> Result<(), String> {
            Err("the overlay workspace store needs Linux".to_string())
        }
        fn cow(root: &std::path::Path) -> bool {
            spawn_host::WorkspaceBackend::auto(root) == spawn_host::WorkspaceBackend::Cow
        }
        fn marked(root: &std::path::Path) -> Result<Option<spawn_host::WorkspaceBackend>, String> {
            spawn_host::root_backend(root).map_err(|error| error.to_string())
        }
        Self {
            overlay: &overlay,
            cow: &cow,
            marked: &marked,
        }
    }
}

/// The engine the workspaces under one root run on, chosen once: every
/// store of the process (the default repository's, every mission's, the
/// one-shot stores of a merge or a set-aside) is that engine, and the root
/// is claimed for it ([`spawn_host::claim_root`]), so no two engines ever
/// share a root, whatever process asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceSettings {
    /// `SPAWN_WORKSPACE_BACKEND` as given (`git` when unset).
    pub(crate) configured: WorkspaceBackend,
    /// The engine.
    pub(crate) active: spawn_host::WorkspaceBackend,
    /// `SPAWN_CAS_DEDUP=1`: build artifacts deduplicated (Cow only).
    pub(crate) cas_dedup: bool,
}

impl WorkspaceSettings {
    /// The engine `configured` is under `root`. `overlay` asked for where
    /// the kernel mounts none (not Linux, no user namespaces, a file system
    /// it refuses) is refused with the kernel's reason, never run as
    /// another engine. `auto` follows the engine the root already is (its
    /// marker, else what it holds: a root with workspaces and no marker is
    /// Git's), so it starts where `spawn mission show` says it would; on a
    /// root no engine has, it is spawn-host's own rule
    /// ([`spawn_host::WorkspaceBackend::auto_with`]): Cow where the file
    /// system clones, else Git; never the overlay.
    pub(crate) fn resolve(
        configured: WorkspaceBackend,
        cas_dedup: bool,
        root: &std::path::Path,
        probes: &Probes<'_>,
    ) -> Result<Self, String> {
        let active = match configured {
            WorkspaceBackend::Git => spawn_host::WorkspaceBackend::Git,
            WorkspaceBackend::Cow => spawn_host::WorkspaceBackend::Cow,
            WorkspaceBackend::Overlay => {
                (probes.overlay)(root).map_err(|why| {
                    format!(
                        "SPAWN_WORKSPACE_BACKEND=overlay: no unprivileged overlay under {}: \
                         {why}; use git, cow or auto",
                        root.display()
                    )
                })?;
                spawn_host::WorkspaceBackend::Overlay
            }
            WorkspaceBackend::Auto => match (probes.marked)(root)? {
                Some(there) => there,
                None => spawn_host::WorkspaceBackend::auto_with((probes.cow)(root)),
            },
        };
        Ok(Self {
            configured,
            active,
            cas_dedup,
        })
    }

    /// The settings the environment gives for `root`
    /// (`SPAWN_WORKSPACE_BACKEND`, `SPAWN_CAS_DEDUP`), resolved on the host
    /// and the root claimed for the engine ([`spawn_host::claim_root`]: a
    /// root another engine runs on is refused), once per root and process.
    /// For a process that runs workspaces only: it writes under the root.
    pub(crate) fn from_env(root: &std::path::Path) -> Result<Self, String> {
        static RESOLVED: std::sync::OnceLock<
            std::sync::Mutex<HashMap<PathBuf, Result<WorkspaceSettings, String>>>,
        > = std::sync::OnceLock::new();
        let configured = workspace_backend(env_value("SPAWN_WORKSPACE_BACKEND").as_deref())?;
        let cas = cas_dedup(env_value("SPAWN_CAS_DEDUP").as_deref())?;
        let mut resolved = RESOLVED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        resolved
            .entry(root.to_path_buf())
            .or_insert_with(|| {
                let settings = Self::resolve(configured, cas, root, &Probes::host())?;
                settings.claim(root)?;
                Ok(settings)
            })
            .clone()
    }

    /// Claim `root` for this engine ([`spawn_host::claim_root`]), only once
    /// the loop can serve it ([`Self::servable`]): a start refused for the
    /// overlay writes no marker, so the root stays free for another engine.
    pub(crate) fn claim(&self, root: &std::path::Path) -> Result<(), String> {
        self.servable()?;
        spawn_host::claim_root(root, self.active)
    }

    /// What `spawn mission show` says of the engine of `root`, read and
    /// never written: no root is made, no probe file or mount tried. The
    /// engine is the one the root's marker names
    /// ([`spawn_host::root_backend`]) or the one asked for by name; `auto`
    /// on a root no loop claimed yet is undecided (`active: null`).
    pub(crate) fn peek(root: &std::path::Path) -> serde_json::Value {
        Self::peek_with(
            workspace_backend(env_value("SPAWN_WORKSPACE_BACKEND").as_deref()),
            cas_dedup(env_value("SPAWN_CAS_DEDUP").as_deref()),
            spawn_host::root_backend(root).map_err(|error| error.to_string()),
        )
    }

    /// [`Self::peek`] of the environment's answers and the root's marker.
    pub(crate) fn peek_with(
        configured: Result<WorkspaceBackend, String>,
        cas: Result<bool, String>,
        marked: Result<Option<spawn_host::WorkspaceBackend>, String>,
    ) -> serde_json::Value {
        let (configured, cas, marked) = match (configured, cas, marked) {
            (Ok(configured), Ok(cas), Ok(marked)) => (configured, cas, marked),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                return serde_json::json!({ "error": error });
            }
        };
        let named = match configured {
            WorkspaceBackend::Git => Some(spawn_host::WorkspaceBackend::Git),
            WorkspaceBackend::Cow => Some(spawn_host::WorkspaceBackend::Cow),
            WorkspaceBackend::Overlay => Some(spawn_host::WorkspaceBackend::Overlay),
            WorkspaceBackend::Auto => None,
        };
        if let (Some(named), Some(marked)) = (named, marked)
            && named != marked
        {
            return serde_json::json!({
                "error": format!(
                    "SPAWN_WORKSPACE_BACKEND={} but the root is the {} engine's: spawn loop \
                     refuses to start",
                    named.name(),
                    marked.name()
                ),
                "root": marked.name(),
            });
        }
        let active = marked.or(named);
        let settings = active.map(|active| Self {
            configured,
            active,
            cas_dedup: cas,
        });
        serde_json::json!({
            "configured": configured.name(),
            "active": active.map(spawn_host::WorkspaceBackend::name),
            "root": marked.map(spawn_host::WorkspaceBackend::name),
            "cas_dedup": settings.as_ref().map_or(if cas { "on" } else { "off" }, Self::cas_state),
            "servable": settings.as_ref().is_none_or(|settings| settings.servable().is_ok()),
        })
    }

    /// The store of this engine over the bare repository `repo`,
    /// workspaces under `root`.
    pub(crate) fn store(
        &self,
        repo: &std::path::Path,
        root: &std::path::Path,
    ) -> spawn_host::LocalWorkspaceStore {
        match self.active {
            spawn_host::WorkspaceBackend::Cow => spawn_host::LocalWorkspaceStore::cow(repo, root),
            #[cfg(target_os = "linux")]
            // Supported: [`Self::resolve`] asked the kernel.
            spawn_host::WorkspaceBackend::Overlay => spawn_host::LocalWorkspaceStore::Overlay(
                spawn_host::OverlayWorkspace::new(repo, root),
            ),
            _ => spawn_host::LocalWorkspaceStore::git(repo, root),
        }
    }

    /// Builds this engine's store for any repository, workspaces under
    /// `root` ([`crate::sources::Factory`]).
    pub(crate) fn factory(&self, root: &std::path::Path) -> crate::sources::Factory {
        let settings = self.clone();
        let root = root.to_path_buf();
        std::sync::Arc::new(move |repo: &std::path::Path| settings.store(repo, &root))
    }

    /// The mission store over `repo` and `root` with this engine, for a
    /// one-shot change (a merge of main, a set-aside:
    /// [`crate::sources::MissionWorkspaces::one_shot`]).
    pub(crate) fn one_shot(
        &self,
        repo: &std::path::Path,
        root: &std::path::Path,
        store: std::sync::Arc<PostgresStore>,
    ) -> crate::sources::MissionWorkspaces {
        crate::sources::MissionWorkspaces::one_shot(
            self.store(repo, root),
            root,
            store,
            self.factory(root),
        )
        .with_cas_dedup(self.cas_dedup)
    }

    /// What the cas deduplication does with this engine: `"on"`, `"off"`,
    /// or `"inactive"` (asked for, but only Cow workspaces deduplicate).
    pub(crate) fn cas_state(&self) -> &'static str {
        match (self.cas_dedup, self.active) {
            (false, _) => "off",
            (true, spawn_host::WorkspaceBackend::Cow) => "on",
            (true, _) => "inactive",
        }
    }

    /// Refused where engines run (`spawn loop`, `spawn tick`) for an engine
    /// they cannot serve yet: an overlay workspace is seen only inside its
    /// holder's mount namespace, while the Bridge starts an engine in the
    /// workspace's host path and the loop reads its reports, acks and exit
    /// files there. Never run half-served. (`auto` never chooses it.)
    pub(crate) fn servable(&self) -> Result<(), String> {
        if self.active == spawn_host::WorkspaceBackend::Overlay {
            return Err(
                "SPAWN_WORKSPACE_BACKEND=overlay: spawn loop does not serve overlay workspaces \
                 to engines yet (an engine would start in the empty host mount point, and its \
                 report, acks and exit file would not be read); use SPAWN_WORKSPACE_BACKEND=cow, \
                 git or auto"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// The line `spawn loop` says at start.
    pub(crate) fn said(&self, root: &std::path::Path) -> String {
        format!(
            "workspace backend {} under {} (SPAWN_WORKSPACE_BACKEND={}); cas dedup {}",
            self.active.name(),
            root.display(),
            self.configured.name(),
            self.cas_state()
        )
    }
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
    fn the_terminal_backend_is_tmux_unless_herdr_is_asked_for() {
        assert_eq!(terminal_backend(None), Ok(TerminalBackend::Tmux));
        assert_eq!(terminal_backend(Some("")), Ok(TerminalBackend::Tmux));
        assert_eq!(terminal_backend(Some("tmux")), Ok(TerminalBackend::Tmux));
        assert_eq!(terminal_backend(Some("herdr")), Ok(TerminalBackend::Herdr));
        assert_eq!(
            terminal_backend(Some(" herdr ")),
            Ok(TerminalBackend::Herdr)
        );
        for unknown in ["screen", "Herdr", "TMUX", "zellij"] {
            let error = terminal_backend(Some(unknown)).expect_err(unknown);
            assert!(error.contains("SPAWN_TERMINAL_BACKEND"), "{error}");
            assert!(error.contains(unknown), "{error}");
        }
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

/// The terminal backends `SPAWN_TERMINAL_BACKEND` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalBackend {
    Tmux,
    Herdr,
}

/// The backend `value` (`SPAWN_TERMINAL_BACKEND`) names: tmux when it is
/// unset or empty, Herdr when it says so; anything else is refused, never
/// read as tmux.
pub(crate) fn terminal_backend(value: Option<&str>) -> Result<TerminalBackend, String> {
    match value.map(str::trim) {
        None | Some("" | "tmux") => Ok(TerminalBackend::Tmux),
        Some("herdr") => Ok(TerminalBackend::Herdr),
        Some(other) => Err(format!(
            "SPAWN_TERMINAL_BACKEND={other:?} names no terminal backend: use tmux (the default) \
             or herdr"
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

/// The terminal host the environment describes: tmux on this machine, on
/// the server of `SPAWN_TMUX_SOCKET` when it is set; or, with
/// `SPAWN_TERMINAL_BACKEND=herdr`, Herdr (`SPAWN_HERDR_PROGRAM`, `herdr` by
/// default) on the session `SPAWN_HERDR_SESSION` (`spawn` by default),
/// sharded across `SPAWN_HERDR_SHARDS` independent servers when greater than 1.
pub(crate) fn terminal_host() -> Result<spawn_host::LocalTerminalHost, String> {
    Ok(
        match terminal_backend(env_value("SPAWN_TERMINAL_BACKEND").as_deref())? {
            TerminalBackend::Tmux => {
                spawn_host::LocalTerminalHost::Tmux(spawn_host::TmuxHost::on_socket(
                    &env_or("SPAWN_TMUX_PROGRAM", "tmux"),
                    env_value("SPAWN_TMUX_SOCKET"),
                ))
            }
            TerminalBackend::Herdr => {
                let program = env_or("SPAWN_HERDR_PROGRAM", "herdr");
                let session = env_or("SPAWN_HERDR_SESSION", spawn_host::herdr::DEFAULT_SESSION);
                let shards = herdr_shards(env_value("SPAWN_HERDR_SHARDS").as_deref())?;
                if shards <= 1 {
                    spawn_host::LocalTerminalHost::Herdr(
                        spawn_host::HerdrHost::on_session(&program, &session)
                            .map_err(|error| format!("SPAWN_HERDR_SESSION: {}", error.0))?,
                    )
                } else {
                    spawn_host::LocalTerminalHost::HerdrPool(
                        spawn_host::HerdrPool::on_session(&program, &session, shards)
                            .map_err(|error| format!("SPAWN_HERDR_SESSION: {}", error.0))?,
                    )
                }
            }
        },
    )
}

#[cfg(test)]
mod workspace_tests {
    use super::{Probes, WorkspaceBackend, WorkspaceSettings, cas_dedup, workspace_backend};
    use serde_json::json;
    use spawn_host::WorkspaceBackend as Engine;
    use std::path::Path;

    #[allow(clippy::unnecessary_wraps)] // the probe's signature
    fn unmarked(_: &Path) -> Result<Option<Engine>, String> {
        Ok(None)
    }

    fn probes<'a>(
        overlay: &'a dyn Fn(&Path) -> Result<(), String>,
        cow: &'a dyn Fn(&Path) -> bool,
    ) -> Probes<'a> {
        Probes {
            overlay,
            cow,
            marked: &unmarked,
        }
    }

    #[test]
    fn the_workspace_backend_is_git_unless_another_is_named() {
        assert_eq!(workspace_backend(None), Ok(WorkspaceBackend::Git));
        assert_eq!(workspace_backend(Some("")), Ok(WorkspaceBackend::Git));
        assert_eq!(workspace_backend(Some("git")), Ok(WorkspaceBackend::Git));
        assert_eq!(workspace_backend(Some(" cow ")), Ok(WorkspaceBackend::Cow));
        assert_eq!(
            workspace_backend(Some("overlay")),
            Ok(WorkspaceBackend::Overlay)
        );
        assert_eq!(workspace_backend(Some("auto")), Ok(WorkspaceBackend::Auto));
        for unknown in ["Git", "COW", "btrfs", "zfs", "overlayfs", "1"] {
            let error = workspace_backend(Some(unknown)).expect_err(unknown);
            assert!(error.contains("SPAWN_WORKSPACE_BACKEND"), "{error}");
            assert!(
                error.contains("git (the default), cow, overlay or auto"),
                "{error}"
            );
        }
    }

    #[test]
    fn cas_dedup_is_off_unless_one_and_refuses_anything_else() {
        assert_eq!(cas_dedup(None), Ok(false));
        assert_eq!(cas_dedup(Some("")), Ok(false));
        assert_eq!(cas_dedup(Some("0")), Ok(false));
        assert_eq!(cas_dedup(Some("1")), Ok(true));
        for unknown in ["yes", "true", "2", "on"] {
            let error = cas_dedup(Some(unknown)).expect_err(unknown);
            assert!(error.contains("SPAWN_CAS_DEDUP"), "{error}");
        }
    }

    #[test]
    fn overlay_asked_for_where_none_mounts_is_refused_never_run_as_another() {
        let root = Path::new("/srv/work");
        let none = |_: &Path| Err("user namespaces are off".to_string());
        let clones = |_: &Path| true;
        let error = WorkspaceSettings::resolve(
            WorkspaceBackend::Overlay,
            false,
            root,
            &probes(&none, &clones),
        )
        .expect_err("refused");
        assert!(error.contains("SPAWN_WORKSPACE_BACKEND=overlay"), "{error}");
        assert!(error.contains("user namespaces are off"), "{error}");
        assert!(error.contains("/srv/work"), "{error}");
        #[cfg(not(target_os = "linux"))]
        assert!(
            WorkspaceSettings::resolve(WorkspaceBackend::Overlay, false, root, &Probes::host())
                .is_err()
        );
    }

    /// `auto` is spawn-host's rule: Cow where clones work, else Git, and
    /// never the overlay, which the loop would refuse.
    #[test]
    fn auto_takes_cow_then_git_and_never_the_overlay() {
        let root = Path::new("/srv/work");
        let mounts = |_: &Path| Ok(());
        let never = |_: &Path| -> Result<(), String> { panic!("auto asks no overlay probe") };
        let clones = |_: &Path| true;
        let copies = |_: &Path| false;
        let active = |overlay: &dyn Fn(&Path) -> Result<(), String>,
                      cow: &dyn Fn(&Path) -> bool| {
            WorkspaceSettings::resolve(WorkspaceBackend::Auto, false, root, &probes(overlay, cow))
                .unwrap()
        };
        assert_eq!(active(&never, &clones).active, Engine::Cow);
        assert_eq!(active(&never, &copies).active, Engine::Git);
        for cow in [true, false] {
            let probe = move |_: &Path| cow;
            let settings = active(&mounts, &probe);
            assert_eq!(settings.active, Engine::auto_with(cow), "one rule");
            assert!(settings.servable().is_ok(), "auto always starts the loop");
        }
        // Asked for by name, git and cow need no probe.
        let never_cow = |_: &Path| -> bool { panic!("no probe for a named engine") };
        for (configured, engine) in [
            (WorkspaceBackend::Git, Engine::Git),
            (WorkspaceBackend::Cow, Engine::Cow),
        ] {
            let settings =
                WorkspaceSettings::resolve(configured, true, root, &probes(&never, &never_cow))
                    .unwrap();
            assert_eq!(settings.active, engine);
        }
    }

    #[test]
    fn the_settings_are_said_with_the_cas_state_and_overlay_is_not_served() {
        let root = Path::new("/srv/work");
        let mounts = |_: &Path| Ok(());
        let clones = |_: &Path| true;
        let auto = WorkspaceSettings::resolve(
            WorkspaceBackend::Auto,
            true,
            root,
            &probes(&mounts, &clones),
        )
        .unwrap();
        assert_eq!(
            auto.said(root),
            "workspace backend cow under /srv/work (SPAWN_WORKSPACE_BACKEND=auto); cas dedup on"
        );
        let git = WorkspaceSettings::resolve(
            WorkspaceBackend::Git,
            true,
            root,
            &probes(&mounts, &clones),
        )
        .unwrap();
        assert_eq!(git.cas_state(), "inactive");
        assert!(git.servable().is_ok() && auto.servable().is_ok());
        assert_eq!(git.store(Path::new("/r.git"), root).backend_name(), "git");
        assert_eq!(auto.store(Path::new("/r.git"), root).backend_name(), "cow");
        let overlay = WorkspaceSettings::resolve(
            WorkspaceBackend::Overlay,
            false,
            root,
            &probes(&mounts, &clones),
        )
        .unwrap();
        assert_eq!(overlay.active, Engine::Overlay);
        assert_eq!(overlay.cas_state(), "off");
        let refused = overlay.servable().expect_err("not served");
        assert!(
            refused.contains("SPAWN_WORKSPACE_BACKEND=overlay"),
            "{refused}"
        );
        assert!(refused.contains("cow, git or auto"), "{refused}");
    }

    /// A read of the settings: from the environment and the root's marker
    /// alone, a conflict said as the loop would refuse it.
    #[test]
    fn a_peek_reads_the_marker_and_names_a_conflict() {
        let peek = WorkspaceSettings::peek_with;
        assert_eq!(
            peek(Ok(WorkspaceBackend::Git), Ok(false), Ok(None)),
            json!({"configured": "git", "active": "git", "root": null, "cas_dedup": "off",
                   "servable": true})
        );
        assert_eq!(
            peek(Ok(WorkspaceBackend::Auto), Ok(true), Ok(None)),
            json!({"configured": "auto", "active": null, "root": null, "cas_dedup": "on",
                   "servable": true})
        );
        assert_eq!(
            peek(Ok(WorkspaceBackend::Auto), Ok(true), Ok(Some(Engine::Cow))),
            json!({"configured": "auto", "active": "cow", "root": "cow", "cas_dedup": "on",
                   "servable": true})
        );
        assert_eq!(
            peek(Ok(WorkspaceBackend::Overlay), Ok(false), Ok(None)),
            json!({"configured": "overlay", "active": "overlay", "root": null,
                   "cas_dedup": "off", "servable": false})
        );
        let conflict = peek(Ok(WorkspaceBackend::Cow), Ok(false), Ok(Some(Engine::Git)));
        assert!(
            conflict["error"].as_str().unwrap().contains("git engine's"),
            "{conflict}"
        );
        let bad = peek(
            Err("SPAWN_WORKSPACE_BACKEND=\"x\" ...".to_string()),
            Ok(false),
            Ok(None),
        );
        assert_eq!(bad.as_object().map(serde_json::Map::len), Some(1));
    }

    /// `auto` on a root that already is an engine's follows it, even where
    /// the file system clones: a Git-shaped root with no marker (its
    /// workspaces, `.transcripts/`) stays Git's, as `spawn mission show`
    /// says, instead of being refused as Cow at the claim.
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
        let clones = |_: &Path| true;
        let never = |_: &Path| -> Result<(), String> { panic!("no overlay probe for auto") };
        let host = Probes::host();
        let probes = Probes {
            overlay: &never,
            cow: &clones,
            marked: host.marked,
        };
        let settings =
            WorkspaceSettings::resolve(WorkspaceBackend::Auto, false, &base, &probes).unwrap();
        assert_eq!(settings.active, Engine::Git);
        settings.claim(&base).expect("Git's root claimed for Git");
        let peek = WorkspaceSettings::peek_with(
            Ok(WorkspaceBackend::Auto),
            Ok(false),
            spawn_host::root_backend(&base).map_err(|error| error.to_string()),
        );
        assert_eq!(peek["active"], "git", "{peek}");
        assert_eq!(peek["servable"], true, "{peek}");

        // A claimed Cow root: auto follows the marker.
        let cow = base.join("cow-root");
        spawn_host::claim_root(&cow, Engine::Cow).unwrap();
        let copies = |_: &Path| false;
        let probes = Probes {
            overlay: &never,
            cow: &copies,
            marked: host.marked,
        };
        let settings =
            WorkspaceSettings::resolve(WorkspaceBackend::Auto, false, &cow, &probes).unwrap();
        assert_eq!(settings.active, Engine::Cow);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A start refused for the overlay claims nothing: the fresh root stays
    /// free, and a later start as Cow, Git or auto claims it.
    #[test]
    fn a_refused_overlay_start_leaves_a_fresh_root_unclaimed() {
        let root = std::env::temp_dir().join(format!(
            "spawn-overlay-refused-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mounts = |_: &Path| Ok(());
        let clones = |_: &Path| true;
        let overlay = WorkspaceSettings::resolve(
            WorkspaceBackend::Overlay,
            false,
            &root,
            &probes(&mounts, &clones),
        )
        .unwrap();
        let refused = overlay.claim(&root).expect_err("not served");
        assert!(
            refused.contains("SPAWN_WORKSPACE_BACKEND=overlay"),
            "{refused}"
        );
        assert!(
            !root.join(spawn_host::BACKEND_MARKER).exists(),
            "a refused start claimed the root"
        );
        assert_eq!(spawn_host::root_backend(&root).unwrap(), None);
        let cow = WorkspaceSettings::resolve(
            WorkspaceBackend::Cow,
            false,
            &root,
            &probes(&mounts, &clones),
        )
        .unwrap();
        cow.claim(&root).expect("the root is free");
        assert_eq!(spawn_host::root_backend(&root).unwrap(), Some(Engine::Cow));
        let _ = std::fs::remove_dir_all(&root);
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
}

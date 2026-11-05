//! Configuration read from the environment and flags, and the store it
//! points at.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its hook and runner need.

use std::collections::HashMap;
use std::path::PathBuf;

use spawn_postgres::PostgresStore;

pub(crate) fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub(crate) fn env_or(name: &str, fallback: &str) -> String {
    env_value(name).unwrap_or_else(|| fallback.to_string())
}

pub(crate) fn required_env(name: &str) -> Result<String, String> {
    env_value(name).ok_or_else(|| format!("{name} is not set"))
}

pub(crate) fn open_store() -> Result<PostgresStore, String> {
    let url = required_env("SPAWN_DATABASE_URL")?;
    PostgresStore::connect(&url).map_err(|error| error.to_string())
}

/// The chunk store that keeps workspaces' large files (docs/storage.md):
/// the directory `SPAWN_CHUNK_STORE` names, else `<root>/.chunks`, which
/// mount and unmount never clear.
pub(crate) fn chunk_store(root: &str) -> std::sync::Arc<dyn spawn_core::port::ChunkStore> {
    std::sync::Arc::new(spawn_host::LocalChunkStore::new(resolve_chunk_store(
        env_value("SPAWN_CHUNK_STORE"),
        root,
    )))
}

fn resolve_chunk_store(configured: Option<String>, root: &str) -> PathBuf {
    configured.map_or_else(
        || PathBuf::from(root).join(spawn_host::DEFAULT_CHUNK_DIRECTORY),
        PathBuf::from,
    )
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

/// `--root`, else `SPAWN_WORKSPACE_ROOT`.
pub(crate) fn workspace_root(flags: &Flags) -> Option<String> {
    resolve_workspace_root(flags.get("root"), env_value("SPAWN_WORKSPACE_ROOT"))
}

fn resolve_workspace_root(flag: Option<String>, environment: Option<String>) -> Option<String> {
    flag.or(environment)
}

/// The oldest git Spawn runs on. Merges of main into a branch, the Bridge's
/// and the integrator's, take main's merge rules from its `.gitattributes`
/// with the global option `git --attr-source=<main>` (a bare repository
/// reads no in-tree attributes otherwise), which Git 2.40 introduced.
pub(crate) const MIN_GIT: (u32, u32) = (2, 40);

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
             (merges take main's .gitattributes through `git --attr-source`, added in \
             {major}.{minor})"
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
    let output = std::process::Command::new("git")
        .arg("--version")
        .stdin(std::process::Stdio::null())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chunk_store_is_configurable_and_defaults_under_the_root() {
        assert_eq!(
            resolve_chunk_store(Some("/srv/chunks".into()), "/ws"),
            PathBuf::from("/srv/chunks")
        );
        assert_eq!(
            resolve_chunk_store(None, "/ws"),
            PathBuf::from("/ws/.chunks")
        );
    }

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
        assert!(check_git_version("git version 2.40.0").is_ok());
        assert!(check_git_version("git version 2.56.0").is_ok());
        assert!(check_git_version("git version 3.1.0").is_ok());
        let old = check_git_version("git version 2.39.5").expect_err("too old");
        assert!(old.contains("git 2.39 is too old"), "{old}");
        assert!(old.contains("2.40 or later"), "{old}");
        assert!(old.contains("--attr-source"), "{old}");
        assert!(check_git_version("git version 2.30.2").is_err());
        assert!(check_git_version("not git").is_err());
    }

    #[test]
    fn the_git_on_this_host_is_new_enough() {
        require_git().expect("the build host runs a current git");
    }

    #[test]
    fn workspace_root_reads_the_root_flag() {
        let flags = Flags::parse(&["--root".to_string(), "/w".to_string()]).unwrap();
        assert_eq!(workspace_root(&flags).as_deref(), Some("/w"));
    }
}

/// `--name value` arguments, as every subcommand takes them.
#[derive(Debug, Default)]
pub(crate) struct Flags {
    values: HashMap<String, String>,
}

impl Flags {
    pub(crate) fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut flags = Self::default();
        let mut rest = arguments.iter();
        while let Some(word) = rest.next() {
            let Some(name) = word.strip_prefix("--") else {
                return Err(format!("unexpected argument {word}"));
            };
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

    pub(crate) fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    pub(crate) fn required(&self, name: &str) -> Result<String, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }
}

/// The terminal host the environment describes: tmux on this machine, or
/// inside each workspace's container when `SPAWN_EXEC_IMAGE` is set.
pub(crate) fn terminal_host() -> spawn_host::TmuxHost {
    use spawn_core::id::WorkspaceId;
    let program = env_or("SPAWN_TMUX_PROGRAM", "tmux");
    let socket = env_value("SPAWN_TMUX_SOCKET");
    let prefix: std::sync::Arc<spawn_host::PrefixBuilder> = match env_value("SPAWN_EXEC_IMAGE") {
        Some(_) => {
            let docker = env_or("SPAWN_DOCKER_PROGRAM", "docker");
            std::sync::Arc::new(move |workspace: &WorkspaceId| {
                spawn_host::docker_prefix(&docker, workspace.sandbox())
            })
        }
        None => std::sync::Arc::new(|_: &WorkspaceId| Vec::new()),
    };
    spawn_host::TmuxHost::in_workspaces(&program, prefix, socket)
}

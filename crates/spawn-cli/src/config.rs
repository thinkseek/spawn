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

#[cfg(test)]
mod tests {
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

    /// The names of the flags given, in no order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    pub(crate) fn required(&self, name: &str) -> Result<String, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }
}

/// The terminal host the environment describes: tmux on this machine, on
/// the server of `SPAWN_TMUX_SOCKET` when it is set.
pub(crate) fn terminal_host() -> spawn_host::TmuxHost {
    spawn_host::TmuxHost::on_socket(
        &env_or("SPAWN_TMUX_PROGRAM", "tmux"),
        env_value("SPAWN_TMUX_SOCKET"),
    )
}

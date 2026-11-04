//! Workspaces for missions, not repositories.
//!
//! The Bridge asks for a workspace by branch, which is the agent id. This
//! store looks up the agent's mission and materializes the workspace from
//! that mission's source: any git repository (path or URL), mirrored once
//! into `<root>/.sources/`, or an empty scratch repository when the mission
//! names none. Agents no mission owns (the supervisor, the RSI agent, ad hoc
//! workers) use the default repository.
//!
//! A mirror follows its source: before a new agent's branch is created,
//! `main` is fetched again, so a worker starts from the source as it is now
//! (a merge that landed since is in). An agent's existing branch keeps the
//! base it started from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use spawn_core::id::{BranchName, WorkspaceId};
use spawn_core::port::{Commit, WorkspaceError, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

/// Builds a store for one bare repository (local, or in containers).
pub(crate) type Factory = Arc<dyn Fn(&Path) -> GitWorkspace + Send + Sync>;

/// Workspace store that routes every agent to its mission's source.
pub(crate) struct MissionWorkspaces {
    default: Arc<GitWorkspace>,
    root: PathBuf,
    store: Arc<PostgresStore>,
    factory: Factory,
    mirrors: Mutex<HashMap<String, Routed>>,
}

/// One agent's store and, for a mission with a source, where its mirror
/// lives and what it mirrors.
#[derive(Clone)]
struct Routed {
    store: Arc<GitWorkspace>,
    mirror: Option<(PathBuf, String)>,
}

fn unavailable(text: String) -> WorkspaceError {
    WorkspaceError::Unavailable(text)
}

fn git(arguments: &[&str]) -> Result<String, WorkspaceError> {
    let output = Command::new("git")
        .args(arguments)
        .stdin(Stdio::null())
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

/// Directory name of a source's mirror: readable, and unique by a hash.
fn mirror_name(source: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in source.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
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

impl MissionWorkspaces {
    pub(crate) fn new(
        default: GitWorkspace,
        root: &Path,
        store: Arc<PostgresStore>,
        factory: Factory,
    ) -> Self {
        Self {
            default: Arc::new(default),
            root: root.to_path_buf(),
            store,
            factory,
            mirrors: Mutex::new(HashMap::new()),
        }
    }

    /// Bare repository for a source, cloned on first use.
    fn mirror(&self, source: Option<&str>) -> Result<PathBuf, WorkspaceError> {
        let directory = self.root.join(".sources");
        std::fs::create_dir_all(&directory)
            .map_err(|error| unavailable(format!("create {}: {error}", directory.display())))?;
        let Some(source) = source else {
            let scratch = directory.join("scratch.git");
            if !scratch.exists() {
                git(&["init", "--quiet", "--bare", &scratch.to_string_lossy()])?;
            }
            return Ok(scratch);
        };
        // A source is handed to git; one that looks like an option is not.
        if source.starts_with('-') {
            return Err(unavailable(format!("source {source} starts with '-'")));
        }
        let mirror = directory.join(mirror_name(source));
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
        let repo = self.mirror(source.as_deref())?;
        let routed = Routed {
            store: Arc::new((self.factory)(&repo)),
            mirror: source.map(|source| (repo, source)),
        };
        mirrors.insert(key, routed.clone());
        Ok(routed)
    }
}

/// Bring a mirror's `main` up to its source's default branch. A source that
/// cannot be reached leaves the mirror as it was: the worker still starts,
/// from the last copy, and the reason is logged.
fn refresh(mirror: &Path, source: &str) {
    let path = mirror.to_string_lossy();
    if let Err(error) = git(&[
        "--git-dir",
        &path,
        "fetch",
        "--quiet",
        "--",
        source,
        "+HEAD:refs/heads/main",
    ]) {
        eprintln!("sources: could not refresh {path} from {source}: {error}");
    }
}

/// Whether a bare repository already has a branch.
fn has_branch(repo: &Path, branch: &BranchName) -> bool {
    git(&[
        "--git-dir",
        &repo.to_string_lossy(),
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{}", branch.as_str()),
    ])
    .is_ok()
}

impl WorkspaceStore for MissionWorkspaces {
    fn project(&self) -> &'static str {
        "missions"
    }

    fn ensure_branch(&self, branch: &BranchName) -> Result<(), WorkspaceError> {
        let routed = self.route(branch.as_str())?;
        if let Some((repo, source)) = &routed.mirror
            && !has_branch(repo, branch)
        {
            refresh(repo, source);
        }
        routed.store.ensure_branch(branch)
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.route(branch.as_str())?.store.mount(branch)
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        self.route(workspace.sandbox())?
            .store
            .commit(workspace, message)
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.route(workspace.sandbox())?.store.unmount(workspace)
    }
}

#[cfg(test)]
mod tests {
    use super::mirror_name;

    #[test]
    fn mirrors_are_named_by_source_and_unique() {
        let a = mirror_name("https://github.com/acme/widget.git");
        let b = mirror_name("/home/me/widget");
        assert!(a.starts_with("widget-") && a.to_lowercase().ends_with(".git"));
        assert!(b.starts_with("widget-"));
        assert_ne!(a, b);
        assert!(mirror_name("git@host:team/x y.git").starts_with("xy-"));
    }
}

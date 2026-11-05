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
//! was published there, never a mounted workspace. The pusher's queue is in
//! memory: a process that ends cleanly pushes what waits ([`Drop`]), and a
//! new one queues again the branch of every agent with an open task
//! ([`MissionWorkspaces::republish`]), so a push lost with a killed process
//! is made after the restart. A branch lives on this host: nothing is taken
//! from the source but `main`.
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

use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, WorkspaceError, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::push::{Pusher, pushed_ref};

/// Least time between two refreshes of one mirror's `main` at turn ends.
pub(crate) const REFRESH_EVERY: Duration = Duration::from_secs(30);

/// Builds a store for one bare repository.
pub(crate) type Factory = Arc<dyn Fn(&Path) -> GitWorkspace + Send + Sync>;

/// Workspace store that routes every agent to its mission's source.
pub(crate) struct MissionWorkspaces {
    default: Arc<GitWorkspace>,
    root: PathBuf,
    store: Arc<PostgresStore>,
    factory: Factory,
    mirrors: Mutex<HashMap<String, Routed>>,
    /// When each mirror's `main` was last fetched.
    refreshed: Mutex<HashMap<PathBuf, Instant>>,
    pusher: Pusher,
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

/// Transports the controller's git may use: local paths, https and ssh.
/// `ext::` (a command), `fd::` and anything a later git adds are refused by
/// git itself, before any helper runs.
pub(crate) const ALLOWED_PROTOCOLS: &str = "file:https:ssh";

/// `git`, as every controller-side call runs it: only
/// [`ALLOWED_PROTOCOLS`], whatever the installed git defaults to or the
/// host's configuration allows (`GIT_ALLOW_PROTOCOL` overrides
/// `protocol.*.allow`), and no standard input.
pub(crate) fn controller_git() -> Command {
    let mut command = Command::new("git");
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
/// `spawn_source_allowed` in the schema is the same rule, where a mission is
/// added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MissionSource(String);

impl MissionSource {
    /// Longest source accepted.
    const MAX_LEN: usize = 4096;

    /// `text` as a source, or why it is not one.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let refused =
            || format!("a source is an absolute path or an https:// or ssh:// URL, not {text:?}");
        let plain = |rest: &str| {
            !rest.is_empty() && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
        };
        let lead =
            |rest: &str, banned: &[char]| rest.chars().next().is_some_and(|c| !banned.contains(&c));
        if text.is_empty() || text.len() > Self::MAX_LEN {
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
            refreshed: Mutex::new(HashMap::new()),
            pusher: Pusher::new(),
        }
    }

    /// Queue a push of an agent's branch, to a source that receives them.
    fn mark(&self, mirror: &Path, source: &str, agent: &str) {
        if receives_branches(source) {
            self.pusher.mark(mirror, source, agent);
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
        Ok(republish(&self.pusher, &self.root, &agents))
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
        let repo = ensure_mirror(&self.root, source.as_deref())?;
        let routed = Routed {
            store: Arc::new((self.factory)(&repo)),
            mirror: source.map(|source| (repo, source)),
        };
        mirrors.insert(key, routed.clone());
        Ok(routed)
    }
}

/// The mirror of a mission's source under `<root>/.sources/`, cloned on
/// first use; an empty scratch repository when the mission has none.
pub(crate) fn ensure_mirror(root: &Path, source: Option<&str>) -> Result<PathBuf, WorkspaceError> {
    let directory = root.join(".sources");
    std::fs::create_dir_all(&directory)
        .map_err(|error| unavailable(format!("create {}: {error}", directory.display())))?;
    let Some(source) = source else {
        let scratch = directory.join("scratch.git");
        if !scratch.exists() {
            git(&["init", "--quiet", "--bare", &scratch.to_string_lossy()])?;
        }
        return Ok(scratch);
    };
    // A source is handed to git only once it is one ([`MissionSource`]):
    // a source stored before the check is refused here, not run.
    let source = MissionSource::parse(source).map_err(unavailable)?;
    let source = source.as_str();
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

/// Queue a push of each `(agent, source)` branch that this host's mirror
/// of a source receiving branches holds. A branch the source already has
/// is pushed as a no-op; one it lacks, or has behind, is published. Returns
/// how many were queued.
fn republish(pusher: &Pusher, root: &Path, agents: &[(String, Option<String>)]) -> usize {
    let mut queued = 0;
    for (agent, source) in agents {
        let Some(source) = source.as_deref().filter(|source| receives_branches(source)) else {
            continue;
        };
        let mirror = root.join(".sources").join(mirror_name(source));
        if mirror.is_dir() && has_branch(&mirror, agent) {
            pusher.mark(&mirror, source, agent);
            queued += 1;
        }
    }
    queued
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
fn ref_commit(repo: &Path, reference: &str) -> Option<String> {
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
        if ref_commit(mirror, tracking).is_some() {
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
        self.route(branch.as_str())?.store.mount(branch)
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        let routed = self.route(workspace.sandbox())?;
        let commit = routed.store.commit(workspace, message)?;
        if let (Commit::Created(_), Some((repo, source))) = (&commit, &routed.mirror) {
            self.mark(repo, source, workspace.sandbox());
        }
        Ok(commit)
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.route(workspace.sandbox())?.store.unmount(workspace)
    }

    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        let routed = self.route(workspace.sandbox())?;
        if let Some((repo, source)) = &routed.mirror {
            self.refresh_main(repo, source, false);
        }
        let merged = routed.store.merge_base(workspace)?;
        if let (BaseMerge::Merged { .. }, Some((repo, source))) = (&merged, &routed.mirror) {
            self.mark(repo, source, workspace.sandbox());
        }
        Ok(merged)
    }

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        self.route(branch.as_str())?.store.branch_head(branch)
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

impl Drop for MissionWorkspaces {
    /// A process that ends (`spawn tick`) pushes what is still waiting.
    fn drop(&mut self) {
        self.pusher.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MissionSource, ensure_mirror, fetch_pushed_into, git, mirror_name, receives_branches,
    };
    use crate::push::{Pushed, Pusher, push_branch};
    use spawn_core::id::{BranchName, WorkspaceId};
    use spawn_core::port::{Commit, WorkspaceStore};
    use spawn_host::GitWorkspace;
    use std::path::Path;

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
        let host = GitWorkspace::new(&mirror, &root);
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
        let host = GitWorkspace::new(&mirror, &root);
        let landing = "refs/spawn-integrate/agent_x";

        // The killed process committed and queued a push it never made.
        let head = checkpoint(&host, "agent_x", "one.txt");
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("asked"),
            None
        );

        // The next process: its pusher starts empty.
        let pusher = Pusher::new();
        let agents = [
            ("agent_x".to_string(), Some(source.clone())),
            ("agent_none".to_string(), Some(source.clone())),
            ("agent_scratch".to_string(), None),
        ];
        assert_eq!(super::republish(&pusher, &root, &agents), 1);
        pusher.flush();
        assert_eq!(
            fetch_pushed_into(&mirror, &source, "agent_x", landing).expect("fetched"),
            Some(head)
        );
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
        let mut plain = std::process::Command::new("git");
        plain.stdin(std::process::Stdio::null());
        let _ = allowing(plain);
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

    /// Every git the controller runs is built by `controller_git`: no other
    /// module of the CLI starts git itself (tests aside).
    #[test]
    fn every_controller_git_is_built_in_one_place() {
        for (name, text) in [
            ("push.rs", include_str!("push.rs")),
            ("integrate.rs", include_str!("integrate.rs")),
            ("missions.rs", include_str!("missions.rs")),
            ("config.rs", include_str!("config.rs")),
            ("deliver.rs", include_str!("deliver.rs")),
            ("supervise.rs", include_str!("supervise.rs")),
            ("runtime.rs", include_str!("runtime.rs")),
        ] {
            let code = text.split("#[cfg(test)]").next().unwrap_or(text);
            assert!(
                !code.contains("Command::new(\"git\")"),
                "{name} starts git without controller_git"
            );
        }
        let own = include_str!("sources.rs");
        let code = own.split("#[cfg(test)]").next().unwrap_or(own);
        assert_eq!(code.matches("Command::new(\"git\")").count(), 1);
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
}

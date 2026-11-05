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
//! (a merge that landed since is in), and at a turn end at most every
//! [`REFRESH_EVERY`], so `main` can be merged into the idle branch
//! (docs/storage.md, "Main into branches automatically").
//!
//! Branches live on the source too: every checkpoint's branch is pushed, in
//! the background ([`crate::push`]), to the source's `spawn/<agent>`. A host
//! that starts an agent whose branch it lacks takes that branch from the
//! source when it is there (resume on another host), and fast-forwards a
//! local branch another host moved on since. While the source cannot be
//! asked, such a start fails (the failure escalates, and a later start
//! tries again): it never makes a fresh branch from `main` beside the
//! pushed one ([`adopt_for_branch`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spawn_core::event::Fork;
use spawn_core::id::{AgentId, BranchName, CommitId, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, WorkspaceError, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::push::{Pusher, pushed_ref};

/// Least time between two refreshes of one mirror's `main` at turn ends.
pub(crate) const REFRESH_EVERY: Duration = Duration::from_secs(30);

/// Builds a store for one bare repository (local, or in containers).
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
    /// Remote sources that may receive and give agents' branches
    /// (`SPAWN_PUSH_SOURCES`); see [`shares_branches`].
    push_sources: Vec<String>,
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

/// Run git and return its trimmed stdout, or the failure as an error.
pub(crate) fn git(arguments: &[&str]) -> Result<String, WorkspaceError> {
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
            push_sources: push_sources(),
        }
    }

    /// Queue a push of an agent's branch, to a source that shares branches.
    fn mark(&self, mirror: &Path, source: &str, agent: &str) {
        if shares_branches(source, &self.push_sources) {
            self.pusher.mark(mirror, source, agent);
        }
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
        refresh(mirror, source);
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

/// Bring a mirror's `main` up to its source's default branch. A source that
/// cannot be reached leaves the mirror as it was: the worker still starts,
/// from the last copy, and the reason is logged.
pub(crate) fn refresh(mirror: &Path, source: &str) {
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

/// The remote sources listed in `SPAWN_PUSH_SOURCES` (comma-separated).
pub(crate) fn push_sources() -> Vec<String> {
    std::env::var("SPAWN_PUSH_SOURCES")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether agents' branches are pushed to `source` and adopted from it: a
/// directory on this host (the central store) by default; a remote URL only
/// when `allowed` (`SPAWN_PUSH_SOURCES`, comma-separated) names it exactly. A
/// remote may be public, and every agent commit would leave the host; and
/// whoever can push `spawn/*` there would choose an agent's starting tree.
pub(crate) fn shares_branches(source: &str, allowed: &[String]) -> bool {
    if allowed.iter().any(|listed| listed == source) {
        return true;
    }
    let path = Path::new(source);
    !source.contains("://") && path.is_absolute() && path.is_dir()
}

/// What [`adopt_pushed`] did with the branch the source holds for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Adopted {
    /// The source has no `spawn/<agent>`.
    Absent,
    /// The mirror lacked the branch; it was created at the source's head.
    Created(String),
    /// Another host moved the branch on; the mirror's was fast-forwarded.
    Advanced(String),
    /// The mirror's branch is the source's, or ahead of it.
    Kept,
    /// Both moved: two writers. The mirror's branch stays.
    Diverged {
        /// The mirror's head.
        local: String,
        /// The source's head.
        pushed: String,
    },
}

/// Bring the agent's branch the source holds (`spawn/<agent>`, pushed by
/// whichever host ran it last) into the mirror: create the local branch
/// from it, or fast-forward the local one. A local branch is never moved
/// back or sideways. An unreachable source is an error.
pub(crate) fn adopt_pushed(
    mirror: &Path,
    source: &str,
    agent: &str,
) -> Result<Adopted, WorkspaceError> {
    let Some(pushed) = fetch_pushed(mirror, source, agent)? else {
        return Ok(Adopted::Absent);
    };
    let path = mirror.to_string_lossy().to_string();
    let reference = format!("refs/heads/{agent}");
    let Some(local) = ref_commit(mirror, &reference) else {
        // An empty old value: only if no branch appeared meanwhile.
        git(&["--git-dir", &path, "update-ref", &reference, &pushed, ""])?;
        return Ok(Adopted::Created(pushed));
    };
    let ancestor =
        |a: &str, b: &str| git(&["--git-dir", &path, "merge-base", "--is-ancestor", a, b]).is_ok();
    if local == pushed || ancestor(&pushed, &local) {
        return Ok(Adopted::Kept);
    }
    if ancestor(&local, &pushed) {
        git(&[
            "--git-dir",
            &path,
            "update-ref",
            &reference,
            &pushed,
            &local,
        ])?;
        return Ok(Adopted::Advanced(pushed));
    }
    Ok(Adopted::Diverged { local, pushed })
}

/// What an agent's start takes from the source before its branch is made
/// here: the branch another host pushed (`Some`, the mirror's branch now
/// created at it), or nothing (`None`: the mirror's own branch stands, or
/// the source has none and a new branch is made from `main`).
///
/// Absent is not unknown. When the source could not be asked (unreachable,
/// unreadable, a local source directory gone) and the mirror has no branch
/// of the agent, the answer is an error: the start fails, and a later one
/// tries again. A fresh branch from `main` would leave the pushed one
/// behind as a second, divergent history. With a local branch, this host
/// keeps working on it.
pub(crate) fn adopt_for_branch(
    mirror: &Path,
    source: &str,
    agent: &str,
    local: bool,
    push_sources: &[String],
) -> Result<Option<Fork>, WorkspaceError> {
    let could_not_ask = |why: String| {
        unavailable(format!(
            "could not ask {source} for {}: {why}; {agent}'s branch is not made from main \
             while the source may hold it, start again once it answers",
            pushed_ref(agent)
        ))
    };
    if !shares_branches(source, push_sources) {
        // A local source shares branches whenever it is a directory: one
        // that is not (any more) cannot say whether it holds the branch.
        let looks_local = !source.contains("://") && Path::new(source).is_absolute();
        if looks_local && !local {
            return Err(could_not_ask("not a directory".to_string()));
        }
        return Ok(None);
    }
    match adopt_pushed(mirror, source, agent) {
        Ok(Adopted::Created(commit)) => Ok(Some(Fork {
            from: format!("spawn/{agent}"),
            commit: commit_id(commit)?,
        })),
        Ok(Adopted::Diverged { local, pushed }) => {
            eprintln!(
                "sources: {agent} is at {local} here but at {pushed} on {source}: \
                 two writers; this host keeps its own"
            );
            Ok(None)
        }
        Ok(Adopted::Absent | Adopted::Advanced(_) | Adopted::Kept) => Ok(None),
        Err(error) if local => {
            eprintln!(
                "sources: could not read {} from {source}: {error}",
                pushed_ref(agent)
            );
            Ok(None)
        }
        Err(error) => Err(could_not_ask(error.to_string())),
    }
}

/// The source's `spawn/<agent>`, fetched into the mirror under
/// `refs/remotes/source/`, or `None` when the source has none.
fn fetch_pushed(
    mirror: &Path,
    source: &str,
    agent: &str,
) -> Result<Option<String>, WorkspaceError> {
    fetch_pushed_into(
        mirror,
        source,
        agent,
        &format!("refs/remotes/source/spawn/{agent}"),
    )
}

/// The source's `spawn/<agent>`, fetched into the mirror as `tracking`, or
/// `None` when the source has none (a `tracking` left from an earlier fetch
/// is then removed). An unreachable source is an error.
pub(crate) fn fetch_pushed_into(
    mirror: &Path,
    source: &str,
    agent: &str,
    tracking: &str,
) -> Result<Option<String>, WorkspaceError> {
    if source.starts_with('-') {
        return Err(unavailable(format!("source {source} starts with '-'")));
    }
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

fn commit_id(commit: String) -> Result<CommitId, WorkspaceError> {
    CommitId::new(commit).map_err(|error| unavailable(error.to_string()))
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
        let agent = branch.as_str();
        let local = has_branch(&repo, agent);
        if !local {
            self.refresh_main(&repo, &source, true);
        }
        let shared = shares_branches(&source, &self.push_sources);
        // Another host may have run this agent: its branch is on the source.
        if let Some(fork) = adopt_for_branch(&repo, &source, agent, local, &self.push_sources)? {
            return Ok(Branch::Created(fork));
        }
        // A fork from an agent this mirror has no branch of: its head as the
        // source holds it.
        let mut resolved = from.map(str::to_string);
        if let Some(from) = from
            && shared
            && !local
            && AgentId::new(from).is_ok()
            && !has_branch(&repo, from)
        {
            // Could not ask is not "has none": the start fails for now, not
            // refused as a fork from an agent without a branch.
            if let Some(commit) = fetch_pushed(&repo, &source, from).map_err(|error| {
                unavailable(format!(
                    "could not read {} from {source}: {error}",
                    pushed_ref(from)
                ))
            })? {
                resolved = Some(commit);
            }
        }
        let made = match routed.store.ensure_branch(branch, resolved.as_deref())? {
            Branch::Created(fork) => Branch::Created(Fork {
                from: from.map_or(fork.from, str::to_string),
                commit: fork.commit,
            }),
            Branch::Existing => Branch::Existing,
        };
        // A new branch shares its fork point; an existing one is pushed
        // again in case its last push was lost with a process.
        self.mark(&repo, &source, agent);
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
        Adopted, adopt_for_branch, adopt_pushed, ensure_mirror, git, mirror_name, shares_branches,
    };
    use crate::push::{Pushed, push_branch};
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
    fn a_branch_pushed_by_one_host_is_resumed_and_advanced_on_another() {
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

        let (root_a, root_b) = (base.join("a"), base.join("b"));
        let mirror_a = ensure_mirror(&root_a, Some(&source)).expect("mirror a");
        let mirror_b = ensure_mirror(&root_b, Some(&source)).expect("mirror b");
        let host_a = GitWorkspace::new(&mirror_a, &root_a);
        let host_b = GitWorkspace::new(&mirror_b, &root_b);

        // Host A works and pushes; host B, which never ran the agent, takes
        // the branch from the source.
        let first = checkpoint(&host_a, "agent_x", "one.txt");
        assert_eq!(push_branch(&mirror_a, &source, "agent_x"), Pushed::Done);
        assert_eq!(
            adopt_pushed(&mirror_b, &source, "agent_x").expect("adopt"),
            Adopted::Created(first.clone())
        );
        assert_eq!(
            adopt_pushed(&mirror_b, &source, "agent_none").expect("absent"),
            Adopted::Absent
        );

        // A moves on; B's stale branch is fast-forwarded.
        let second = checkpoint(&host_a, "agent_x", "two.txt");
        assert_eq!(push_branch(&mirror_a, &source, "agent_x"), Pushed::Done);
        assert_eq!(
            adopt_pushed(&mirror_b, &source, "agent_x").expect("adopt"),
            Adopted::Advanced(second.clone())
        );
        assert_eq!(
            adopt_pushed(&mirror_a, &source, "agent_x").expect("same"),
            Adopted::Kept
        );

        // Both write: the second push is refused, not forced, and the
        // divergence is seen.
        checkpoint(&host_b, "agent_x", "three.txt");
        assert_eq!(push_branch(&mirror_b, &source, "agent_x"), Pushed::Done);
        checkpoint(&host_a, "agent_x", "four.txt");
        assert!(matches!(
            push_branch(&mirror_a, &source, "agent_x"),
            Pushed::Rejected(_)
        ));
        assert!(matches!(
            adopt_pushed(&mirror_a, &source, "agent_x").expect("diverged"),
            Adopted::Diverged { .. }
        ));

        // An unreachable source is an error, not "absent".
        let gone = base.join("gone.git").to_string_lossy().to_string();
        assert!(adopt_pushed(&mirror_a, &gone, "agent_x").is_err());
        assert!(matches!(
            push_branch(&mirror_a, &gone, "agent_x"),
            Pushed::Failed(_)
        ));
    }

    #[test]
    fn a_source_that_cannot_be_asked_never_gets_a_branch_made_from_main() {
        let base = std::env::temp_dir().join(format!("spawn-sources-ask-{}", std::process::id()));
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
        let source_dir = base.join("source.git");
        let source = source_dir.to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");

        // Readable and without the branch: absent, a branch from main may
        // be made.
        assert_eq!(
            adopt_for_branch(&mirror, &source, "agent_x", false, &[]).expect("asked"),
            None
        );

        // Unreadable: an error, and no branch appears in the mirror.
        let mode = |bits: u32| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&source_dir, std::fs::Permissions::from_mode(bits))
                .expect("chmod the source");
        };
        mode(0o000);
        let unreadable = std::fs::read_dir(&source_dir).is_err();
        let asked = adopt_for_branch(&mirror, &source, "agent_x", false, &[]);
        mode(0o755);
        if unreadable {
            assert!(asked.is_err(), "{asked:?}");
        }
        assert!(!super::has_branch(&mirror, "agent_x"));

        // Gone: a local source that is no directory cannot be asked either.
        std::fs::rename(&source_dir, base.join("moved.git")).expect("move the source away");
        let asked = adopt_for_branch(&mirror, &source, "agent_x", false, &[]);
        assert!(
            asked
                .as_ref()
                .is_err_and(|error| error.to_string().contains("not made from main")),
            "{asked:?}"
        );
        // A branch this host has keeps working without the source.
        assert_eq!(
            adopt_for_branch(&mirror, &source, "agent_x", true, &[]).expect("local"),
            None
        );
        // A listed remote that cannot be reached is the same error.
        let remote = "file:///nonexistent/spawn-source.git".to_string();
        assert!(
            adopt_for_branch(
                &mirror,
                &remote,
                "agent_x",
                false,
                std::slice::from_ref(&remote)
            )
            .is_err()
        );
        assert!(!super::has_branch(&mirror, "agent_x"));
    }

    #[test]
    fn only_local_or_listed_sources_share_branches() {
        let here = std::env::temp_dir();
        let here = here.to_string_lossy();
        assert!(shares_branches(&here, &[]), "a directory on this host");
        let remote = "https://github.com/acme/widget.git";
        assert!(!shares_branches(remote, &[]));
        assert!(!shares_branches("git@github.com:acme/widget.git", &[]));
        assert!(!shares_branches("/no/such/directory.git", &[]));
        assert!(!shares_branches("file:///srv/widget.git", &[]));
        assert!(shares_branches(remote, &[remote.to_string()]), "listed");
        assert!(
            !shares_branches(remote, &[format!("{remote}/")]),
            "an exact match only"
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
}

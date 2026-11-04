//! git-backed workspace store.
//!
//! One bare repository holds every base commit. A mount exports a branch with
//! `git archive` into a plain directory, so the engine never sees `.git` and
//! cannot read the history, another branch, or the controller's refs.
//!
//! A commit is written with plumbing against a temporary index, so it never
//! touches a checked-out tree and needs no branch to be current.
//!
//! Identifiers travel as `<branch>:<absolute directory>`. The branch is the
//! part before the colon, and the container of an execution is named from it,
//! so every call rebuilds the mapping from the identifier alone.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use spawn_core::event::Fork;
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, ForkFrom, WorkspaceError, WorkspaceStore};

/// Ref a new branch is forked from.
const BASE: &str = "main";

/// Paths that never enter a commit.
const BUILD_OUTPUT: [&str; 2] = ["target", ".spawn"];

/// Files larger than this stay out of git (docs/storage.md): git stores
/// every version of a file whole, so one rewritten 200 MiB binary costs
/// seconds per checkpoint on the Bridge's loop and its full size again in
/// the repository each time.
pub const LARGE_FILE_BYTES: u64 = 8 << 20;

/// Docker settings for a workspace that hosts the engine.
#[derive(Debug, Clone)]
pub struct Container {
    /// Program that talks to the daemon.
    pub docker: String,
    /// Image the engine runs in.
    pub image: String,
}

/// Name of the container that hosts one branch's workspace.
pub fn container_name(branch: &str) -> String {
    format!("spawn-exec-{branch}")
}

/// argv prefix that runs one shell command inside a workspace container.
///
/// The container mounts the workspace at its host path, so no path
/// translation is needed anywhere.
pub fn docker_prefix(docker: &str, branch: &str) -> Vec<String> {
    vec![
        docker.to_string(),
        "exec".to_string(),
        "-i".to_string(),
        "-t".to_string(),
        container_name(branch),
        "sh".to_string(),
        "-c".to_string(),
    ]
}

/// Trimmed stdout of a git command, or the failure as an error.
fn report(output: std::io::Result<Output>, word: &str) -> Result<String, WorkspaceError> {
    let output =
        output.map_err(|error| WorkspaceError::Unavailable(format!("run {word}: {error}")))?;
    if !output.status.success() {
        return Err(WorkspaceError::Unavailable(format!(
            "git {word}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Workspace store backed by a bare git repository.
#[derive(Debug, Clone)]
pub struct GitWorkspace {
    repo: PathBuf,
    root: PathBuf,
    git: String,
    container: Option<Container>,
    /// Large files already reported, by branch, path and size: one warning
    /// per version, not one per checkpoint.
    reported: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl GitWorkspace {
    /// Store that keeps working trees on this machine.
    pub fn new(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Self {
        Self {
            repo: repo.as_ref().to_path_buf(),
            root: root.as_ref().to_path_buf(),
            git: "git".to_string(),
            container: None,
            reported: std::sync::Arc::default(),
        }
    }

    /// Store whose working trees are mounted into one disposable container.
    pub fn with_container(
        repo: impl AsRef<Path>,
        root: impl AsRef<Path>,
        docker: &str,
        image: &str,
    ) -> Self {
        Self {
            container: Some(Container {
                docker: docker.to_string(),
                image: image.to_string(),
            }),
            ..Self::new(repo, root)
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.git);
        command.arg(format!("--git-dir={}", self.repo.display()));
        // commit-tree needs a committer identity. Supplying it here keeps the
        // store independent of the host's git configuration.
        command
            .env("GIT_AUTHOR_NAME", "spawn")
            .env("GIT_AUTHOR_EMAIL", "spawn@localhost")
            .env("GIT_COMMITTER_NAME", "spawn")
            .env("GIT_COMMITTER_EMAIL", "spawn@localhost");
        command
    }

    /// Workspace directory, refused when it lies outside the store root.
    fn checked_path(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let directory = PathBuf::from(workspace.path());
        if directory.starts_with(&self.root) {
            return Ok(directory);
        }
        Err(WorkspaceError::Unavailable(format!(
            "workspace {} is outside {}",
            directory.display(),
            self.root.display()
        )))
    }

    fn command_in(&self, directory: &Path, index: &Path) -> Command {
        let mut command = self.command();
        command
            .arg(format!("--work-tree={}", directory.display()))
            .env("GIT_INDEX_FILE", index);
        command
    }

    fn git_output(&self, arguments: &[&str]) -> Result<String, WorkspaceError> {
        let output = self.command().args(arguments).stdin(Stdio::null()).output();
        report(output, &arguments.join(" "))
    }

    fn output_in(
        &self,
        directory: &Path,
        index: &Path,
        arguments: &[&str],
    ) -> Result<String, WorkspaceError> {
        let output = self
            .command_in(directory, index)
            .args(arguments)
            .stdin(Stdio::null())
            .output();
        report(output, &arguments.join(" "))
    }

    fn has_base(&self) -> bool {
        self.git_output(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{BASE}"),
        ])
        .is_ok()
    }

    /// Root commit of an empty repository.
    fn seed_base(&self) -> Result<(), WorkspaceError> {
        let tree = self.git_output(&["mktree"])?;
        let commit = self.git_output(&["commit-tree", &tree, "-m", "spawn: base"])?;
        self.git_output(&["update-ref", &format!("refs/heads/{BASE}"), &commit])?;
        Ok(())
    }

    fn start_container(&self, branch: &str, directory: &Path) -> Result<(), WorkspaceError> {
        let Some(container) = self.container.as_ref() else {
            return Ok(());
        };
        let name = container_name(branch);
        let _ = Command::new(&container.docker)
            .args(["rm", "-f", &name])
            .stdin(Stdio::null())
            .output();
        let mount = format!("{}:{}", directory.display(), directory.display());
        let output = Command::new(&container.docker)
            .args([
                "run",
                "-d",
                "--name",
                &name,
                "-v",
                &mount,
                "-w",
                &directory.display().to_string(),
                &container.image,
                "sleep",
                "infinity",
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| {
                WorkspaceError::Unavailable(format!("run {}: {error}", container.docker))
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(WorkspaceError::Unavailable(format!(
            "container {name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    /// Exclude build output and the controller's marker from every commit.
    ///
    /// `info/exclude` lives in the bare repository, so the exported tree stays
    /// untouched and the engine cannot see or edit the rule.
    fn exclude_build_output(&self) -> Result<(), WorkspaceError> {
        let info = self.repo.join("info");
        let path = info.join("exclude");
        std::fs::create_dir_all(&info).map_err(|error| {
            WorkspaceError::Unavailable(format!("create {}: {error}", info.display()))
        })?;
        std::fs::write(&path, "target/\n.spawn/\n").map_err(|error| {
            WorkspaceError::Unavailable(format!("write {}: {error}", path.display()))
        })
    }

    /// Commit a reference points at now.
    pub fn commit_id_of(&self, reference: &str) -> Result<String, WorkspaceError> {
        self.git_output(&["rev-parse", "--verify", &format!("{reference}^{{commit}}")])
    }

    /// Patch that turns `from` into `to`.
    pub fn patch_between(&self, from: &str, to: &str) -> Result<String, WorkspaceError> {
        self.git_output(&["diff", from, to])
    }

    /// Move `refs/heads/main` to `candidate` only if it still points at `base`.
    ///
    /// False means another writer advanced the baseline first.
    pub fn advance_base(&self, base: &str, candidate: &str) -> Result<bool, WorkspaceError> {
        let reference = format!("refs/heads/{BASE}");
        let output = self
            .command()
            .args(["update-ref", &reference, candidate, base])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| WorkspaceError::Unavailable(format!("run {}: {error}", self.git)))?;
        if output.status.success() {
            return Ok(true);
        }
        // A lost race, a repeat of a move that already happened, and a real
        // failure all exit non-zero. Reading the ref back separates them.
        let current = self.git_output(&["rev-parse", &reference])?;
        if current == candidate {
            // Already moved, by an earlier call that stopped before its
            // caller recorded it.
            return Ok(true);
        }
        if current != base {
            return Ok(false);
        }
        report(Ok(output), &format!("update-ref {reference}"))?;
        Ok(false)
    }

    /// The index of one branch, outside every work tree, kept between
    /// checkpoints: its stat cache makes `add -A` hash only the files that
    /// changed, not the whole tree on every tool call. A commit holds
    /// `<index>.spawn-lock` for its whole sequence, since the Bridge and the
    /// integrator both commit (git itself owns `<index>.lock`). An index
    /// that fails is removed, and the next checkpoint rebuilds it.
    fn index(&self, branch: &str) -> PathBuf {
        self.root.join(".index").join(branch)
    }

    /// Tree of the work tree in `directory`, built against `index`.
    ///
    /// Build output and files over [`LARGE_FILE_BYTES`] leave the index after
    /// `add -A`, so a `.gitignore` inside the tree cannot re-include them; a
    /// large file is excluded before it is hashed.
    fn tree_of(
        &self,
        branch: &str,
        directory: &Path,
        index: &Path,
    ) -> Result<String, WorkspaceError> {
        let large = self.large_files(directory, index)?;
        let mut add = vec!["add", "-A", "--", "."];
        let excluded: Vec<String> = large
            .iter()
            .map(|(path, _)| format!(":(exclude,literal){path}"))
            .collect();
        add.extend(excluded.iter().map(String::as_str));
        self.output_in(directory, index, &add)?;
        self.unstage(directory, index, &large)?;
        self.report_large(branch, &large);
        let tree = self.output_in(directory, index, &["write-tree"])?;
        // A file that grew past the limit between the size check and `add`
        // was hashed anyway; keep it off the branch all the same.
        let grown = self.large_blobs(&tree)?;
        if grown.is_empty() {
            return Ok(tree);
        }
        self.unstage(directory, index, &grown)?;
        self.report_large(branch, &grown);
        self.output_in(directory, index, &["write-tree"])
    }

    /// Take build output and `paths` out of the index. Paths go to git as
    /// literals: a file named `*` or `:/` must not match others.
    fn unstage(
        &self,
        directory: &Path,
        index: &Path,
        paths: &[(String, u64)],
    ) -> Result<(), WorkspaceError> {
        let literal: Vec<String> = paths
            .iter()
            .map(|(path, _)| format!(":(literal){path}"))
            .collect();
        let mut remove = vec!["rm", "-r", "--cached", "--ignore-unmatch", "--"];
        remove.extend(BUILD_OUTPUT);
        remove.extend(literal.iter().map(String::as_str));
        self.output_in(directory, index, &remove).map(|_| ())
    }

    /// Blobs of `tree` larger than [`LARGE_FILE_BYTES`], with their sizes.
    fn large_blobs(&self, tree: &str) -> Result<Vec<(String, u64)>, WorkspaceError> {
        let listed = self.git_output(&["ls-tree", "-r", "-l", "-z", tree])?;
        Ok(listed
            .split('\0')
            .filter_map(|entry| {
                // `<mode> blob <id> <size>\t<path>`
                let (meta, path) = entry.split_once('\t')?;
                let mut fields = meta.split_whitespace();
                let kind = fields.nth(1)?;
                let size: u64 = fields.nth(1)?.parse().ok()?;
                (kind == "blob" && size > LARGE_FILE_BYTES).then(|| (path.to_string(), size))
            })
            .collect())
    }

    /// Files git would commit that are larger than [`LARGE_FILE_BYTES`], with
    /// their sizes: tracked or untracked, ignore rules applied.
    fn large_files(
        &self,
        directory: &Path,
        index: &Path,
    ) -> Result<Vec<(String, u64)>, WorkspaceError> {
        let listed = self.output_in(
            directory,
            index,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        let mut large: Vec<(String, u64)> = listed
            .split('\0')
            .filter(|path| !path.is_empty())
            .filter_map(|path| {
                let size = std::fs::symlink_metadata(directory.join(path))
                    .ok()
                    .filter(std::fs::Metadata::is_file)?
                    .len();
                (size > LARGE_FILE_BYTES).then(|| (path.to_string(), size))
            })
            .collect();
        large.sort();
        large.dedup();
        Ok(large)
    }

    /// Say once per version that a large file stays out of the checkpoint:
    /// it lives in the workspace until unmount and is not on the branch.
    fn report_large(&self, branch: &str, large: &[(String, u64)]) {
        let mut reported = self
            .reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (path, size) in large {
            if reported.insert(format!("{branch}\0{path}\0{size}")) {
                eprintln!(
                    "git: {branch}: {path} ({size} bytes) is over {LARGE_FILE_BYTES} \
                     bytes and stays out of the checkpoint (docs/storage.md)"
                );
            }
        }
    }
}

impl WorkspaceStore for GitWorkspace {
    fn project(&self) -> &str {
        self.repo.to_str().unwrap_or("")
    }

    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError> {
        // A name that starts with '-' would reach git as an option.
        if branch.as_str().starts_with('-') {
            return Err(WorkspaceError::Unavailable(format!(
                "branch {branch} starts with '-'"
            )));
        }
        if !self.has_base() {
            self.seed_base()?;
        }
        let reference = format!("refs/heads/{branch}");
        if self
            .git_output(&["rev-parse", "--verify", "--quiet", &reference])
            .is_ok()
        {
            return Ok(Branch::Existing);
        }
        let (label, commit) = match from {
            None => (
                BASE.to_string(),
                self.commit_id_of(&format!("refs/heads/{BASE}"))?,
            ),
            Some(from) => (from.to_string(), self.resolve_from(from)?),
        };
        // An empty old value: create only, never move a branch that
        // appeared meanwhile.
        self.git_output(&["update-ref", &reference, &commit, ""])?;
        Ok(Branch::Created(Fork {
            from: label,
            commit: CommitId::new(commit)
                .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?,
        }))
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.exclude_build_output()?;
        // Verify first: a failed mount must not destroy the previous tree.
        self.git_output(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .map_err(|_| WorkspaceError::Unavailable(format!("branch {branch} has no commit")))?;

        let directory = self.root.join(branch.as_str());
        if directory.exists() {
            std::fs::remove_dir_all(&directory).map_err(|error| {
                WorkspaceError::Unavailable(format!("clear {}: {error}", directory.display()))
            })?;
        }
        std::fs::create_dir_all(&directory).map_err(|error| {
            WorkspaceError::Unavailable(format!("create {}: {error}", directory.display()))
        })?;

        let mut archive = self
            .command()
            .args(["archive", &format!("refs/heads/{branch}")])
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| WorkspaceError::Unavailable(format!("run {}: {error}", self.git)))?;
        let stdout = archive
            .stdout
            .take()
            .ok_or_else(|| WorkspaceError::Unavailable("archive produced no output".to_string()))?;
        let extracted = Command::new("tar")
            .arg("-x")
            .arg("-C")
            .arg(&directory)
            .stdin(Stdio::from(stdout))
            .status();
        let archived = archive.wait();
        if !archived.is_ok_and(|status| status.success()) {
            return Err(WorkspaceError::Unavailable(format!(
                "archive {branch} failed"
            )));
        }
        let extracted =
            extracted.map_err(|error| WorkspaceError::Unavailable(format!("extract: {error}")))?;
        if !extracted.success() {
            return Err(WorkspaceError::Unavailable(format!(
                "extract {branch}: tar exited {extracted}"
            )));
        }

        self.start_container(branch.as_str(), &directory)?;

        WorkspaceId::new(format!("{branch}:{}", directory.display()))
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        let branch = workspace.sandbox().to_string();
        let directory = self.checked_path(workspace)?;
        let index = self.index(&branch);
        if let Some(parent) = index.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                WorkspaceError::Unavailable(format!("create {}: {error}", parent.display()))
            })?;
        }
        // The Bridge and the integrator both commit workspaces under one
        // root: the branch's index is theirs one at a time.
        let lock_path = index.with_extension("spawn-lock");
        let lock = std::fs::File::create(&lock_path).map_err(|error| {
            WorkspaceError::Unavailable(format!("create {}: {error}", lock_path.display()))
        })?;
        lock.lock().map_err(|error| {
            WorkspaceError::Unavailable(format!("lock {}: {error}", lock_path.display()))
        })?;
        let tree = self.tree_of(&branch, &directory, &index);
        if tree.is_err() {
            let _ = std::fs::remove_file(&index);
        }
        let tree = tree?;

        let reference = format!("refs/heads/{branch}");
        let parent = self.git_output(&["rev-parse", &reference])?;
        let parent_tree = self.git_output(&["rev-parse", &format!("{parent}^{{tree}}")])?;
        if tree == parent_tree {
            return Ok(Commit::Unchanged);
        }
        let commit = self.git_output(&["commit-tree", &tree, "-p", &parent, "-m", message])?;
        // Expected old value: a branch that moved is not overwritten.
        self.git_output(&["update-ref", &reference, &commit, &parent])
            .map_err(|error| {
                WorkspaceError::Unavailable(format!("{error} (branch moved under the commit)"))
            })?;
        CommitId::new(commit)
            .map(Commit::Created)
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        let directory = self.checked_path(workspace)?;
        if let Some(container) = self.container.as_ref() {
            let _ = Command::new(&container.docker)
                .args(["rm", "-f", &container_name(workspace.sandbox())])
                .stdin(Stdio::null())
                .output();
        }
        match std::fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(WorkspaceError::Unavailable(format!(
                "remove {}: {error}",
                directory.display()
            ))),
        }
    }

    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        self.merge_main(workspace)
    }
}

/// Branching and merging (docs/storage.md, "Branching and merging").
impl GitWorkspace {
    /// The commit a start's `from` names: another agent's branch head or a
    /// full commit id present in the repository. Anything else is refused.
    pub fn resolve_from(&self, from: &str) -> Result<String, WorkspaceError> {
        let refused = |message: String| WorkspaceError::Refused {
            code: "NoSuchFrom".to_string(),
            message,
        };
        match ForkFrom::parse(from)? {
            ForkFrom::Agent(agent) => self
                .commit_id_of(&format!("refs/heads/{agent}"))
                .map_err(|_| refused(format!("{agent} has no branch in {}", self.project()))),
            ForkFrom::Commit(commit) => {
                let found = self
                    .git_output(&[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("{commit}^{{commit}}"),
                    ])
                    .map_err(|_| refused(format!("no commit {commit} in {}", self.project())))?;
                if found != commit.as_str() {
                    return Err(refused(format!("{commit} resolves to {found}")));
                }
                Ok(found)
            }
        }
    }

    /// Whether `ancestor` is reachable from `commit`.
    fn is_ancestor(&self, ancestor: &str, commit: &str) -> Result<bool, WorkspaceError> {
        let status = self
            .command()
            .args(["merge-base", "--is-ancestor", ancestor, commit])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| WorkspaceError::Unavailable(format!("run {}: {error}", self.git)))?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(WorkspaceError::Unavailable(format!(
                "git merge-base --is-ancestor {ancestor} {commit}: {status}"
            ))),
        }
    }

    /// [`WorkspaceStore::merge_base`]: merge `main` into the workspace's
    /// branch when it moved.
    ///
    /// The merge rules come from `main`'s `.gitattributes`
    /// (`--attr-source`; a bare repository ignores in-tree attributes
    /// otherwise), which only the gate moves. The directory moves first, by a
    /// two-tree `read-tree -m -u` against a temporary index that refuses when
    /// a file the merge changes differs from the branch; only then does the
    /// branch move, by compare-and-swap. The other order would let the next
    /// checkpoint commit the old files over the merge and revert `main`.
    fn merge_main(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        let branch = workspace.sandbox().to_string();
        let directory = self.checked_path(workspace)?;
        let reference = format!("refs/heads/{branch}");
        let head = self.commit_id_of(&reference)?;
        let main = self.commit_id_of(&format!("refs/heads/{BASE}"))?;
        if self.is_ancestor(&main, &head)? {
            return Ok(BaseMerge::Current);
        }
        // No shared history (a default repository reseeded under an older
        // branch): there is nothing to merge, and git would refuse anyway.
        if self.git_output(&["merge-base", &main, &head]).is_err() {
            return Ok(BaseMerge::Current);
        }
        // `main` first, as the integrator lands it: a union merge orders the
        // lines the same way the landing would.
        let output = self
            .command()
            .arg(format!("--attr-source={main}"))
            .args([
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                &main,
                &head,
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| WorkspaceError::Unavailable(format!("run {}: {error}", self.git)))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut fields = stdout.split('\0');
        let merged = fields.next().unwrap_or_default().trim().to_string();
        match output.status.code() {
            Some(0) => {}
            Some(1) => {
                let mut files: Vec<String> = fields
                    .take_while(|field| !field.is_empty())
                    .map(str::to_string)
                    .collect();
                files.dedup();
                return Ok(BaseMerge::Conflicted { files });
            }
            _ => {
                return Err(WorkspaceError::Unavailable(format!(
                    "git merge-tree {main} {head}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        let head_tree = self.git_output(&["rev-parse", &format!("{head}^{{tree}}")])?;
        self.update_directory(&branch, &directory, &head_tree, &merged)?;
        let changed =
            self.git_output(&["diff-tree", "-r", "--name-only", "-z", &head_tree, &merged])?;
        let files = changed
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect();
        // The branch keeps its own large files: their manifest trailer, if
        // the head names one, goes on the merge commit.
        let trailers =
            self.git_output(&["log", "-1", "--format=%(trailers:key=Spawn-Large)", &head])?;
        let mut message = format!("spawn: merge main {main} into {branch}");
        if !trailers.is_empty() {
            message = format!("{message}\n\n{trailers}");
        }
        let commit = self.git_output(&[
            "commit-tree",
            &merged,
            "-p",
            &head,
            "-p",
            &main,
            "-m",
            &message,
        ])?;
        self.git_output(&["update-ref", &reference, &commit, &head])
            .map_err(|error| {
                WorkspaceError::Unavailable(format!("{error} (branch moved under the merge)"))
            })?;
        Ok(BaseMerge::Merged {
            commit: CommitId::new(commit)
                .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?,
            files,
        })
    }

    /// Move the files of `directory` from tree `from` to tree `to`, or
    /// refuse, changing nothing, when a file that differs between them also
    /// differs in the directory from `from` (or is in the way untracked).
    fn update_directory(
        &self,
        branch: &str,
        directory: &Path,
        from: &str,
        to: &str,
    ) -> Result<(), WorkspaceError> {
        let index = self.root.join(".index").join(format!("{branch}.merge"));
        if let Some(parent) = index.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                WorkspaceError::Unavailable(format!("create {}: {error}", parent.display()))
            })?;
        }
        let _ = std::fs::remove_file(&index);
        let result = self
            .output_in(directory, &index, &["read-tree", from])
            .and_then(|_| {
                // Stat information for the fresh index, so unchanged files
                // count as up to date; what differs is caught below.
                let _ = self.output_in(directory, &index, &["update-index", "-q", "--refresh"]);
                self.output_in(directory, &index, &["read-tree", "-m", "-u", from, to])
                    .map_err(|error| WorkspaceError::Refused {
                        code: "WorkspaceDiffers".to_string(),
                        message: error.to_string(),
                    })
            });
        let _ = std::fs::remove_file(&index);
        result.map(|_| ())
    }
}

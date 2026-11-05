//! git-backed workspace store.
//!
//! One bare repository holds every base commit. A mount exports a branch with
//! `git archive` into a plain directory, so the engine never sees `.git` and
//! cannot read the history, another branch, or the controller's refs.
//!
//! A commit is written with plumbing against a temporary index, so it never
//! touches a checked-out tree and needs no branch to be current.
//!
//! Files over [`LARGE_FILE_BYTES`] never enter git: they are cut into
//! chunks in the chunk store, the checkpoint commit names their manifest in a
//! `Spawn-Large: <hash>` trailer, and a mount materializes them again from
//! the branch head (docs/storage.md). The branch head alone is the
//! whole workspace. The one exception: a large file git already holds as a
//! blob in the branch head or in `main` (it came from the source) stays a
//! blob, so a checkpoint never drops it from the tree.
//!
//! Identifiers travel as `<branch>:<absolute directory>`. The branch is the
//! part before the colon, and the container of an execution is named from it,
//! so every call rebuilds the mapping from the identifier alone.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use serde_json::{Value, json};

use spawn_core::event::Fork;
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{
    BaseMerge, Branch, ChunkStore, Commit, ForkFrom, WorkspaceError, WorkspaceStore,
};

use crate::chunks::{LargeFile, LocalChunkStore, Manifest, materialize, mode_of, store_file};
use crate::tree_path::{TreePath, nul_fields};

/// Distinguishes the staged mounts of one process ([`GitWorkspace::staging_path`]).
static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Ref a new branch is forked from.
const BASE: &str = "main";

/// Paths that never enter a commit.
const BUILD_OUTPUT: [&str; 2] = ["target", ".spawn"];

/// Files larger than this stay out of git and go to the chunk store
/// (docs/storage.md): git stores every version of a file whole, so one
/// rewritten 200 MiB binary costs seconds per checkpoint on the Bridge's loop
/// and its full size again in the repository each time.
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

/// Stdout of a git command exactly as written, bytes and all, or the
/// failure as an error. A `-z` listing is read this way: a path may begin
/// or end with whitespace, which trimming would cut off the first or the
/// last path, and its bytes need not be UTF-8 ([`TreePath`]).
fn report_listing(output: std::io::Result<Output>, word: &str) -> Result<Vec<u8>, WorkspaceError> {
    let output =
        output.map_err(|error| WorkspaceError::Unavailable(format!("run {word}: {error}")))?;
    if !output.status.success() {
        return Err(WorkspaceError::Unavailable(format!(
            "git {word}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Trimmed stdout of a git command that prints one value (a commit, a
/// tree), or the failure as an error. Never for a listing of paths.
fn report(output: std::io::Result<Output>, word: &str) -> Result<String, WorkspaceError> {
    report_listing(output, word).map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
}

/// git arguments joined for an error message.
fn shown_arguments<S: AsRef<OsStr>>(arguments: &[S]) -> String {
    arguments
        .iter()
        .map(|argument| argument.as_ref().to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Bytes of pathspecs one `git ls-tree` call carries on its command line:
/// far under Linux's 2 MiB for arguments and environment together, and
/// over any single path, so every batch holds at least one.
const PATHSPEC_BATCH_BYTES: usize = 128 << 10;

/// `specs` in order, cut into runs of at most [`PATHSPEC_BATCH_BYTES`]
/// (a longer spec gets a run of its own).
fn pathspec_batches(specs: Vec<OsString>) -> Vec<Vec<OsString>> {
    let mut batches: Vec<Vec<OsString>> = Vec::new();
    let mut bytes = 0;
    for spec in specs {
        let length = spec.len() + 1;
        match batches.last_mut() {
            Some(batch) if bytes + length <= PATHSPEC_BATCH_BYTES => {
                bytes += length;
                batch.push(spec);
            }
            _ => {
                bytes = length;
                batches.push(vec![spec]);
            }
        }
    }
    batches
}

/// `specs` NUL-terminated, for `--pathspec-from-file=- --pathspec-file-nul`.
fn nul_terminated(specs: &[OsString]) -> Vec<u8> {
    let mut input = Vec::new();
    for spec in specs {
        input.extend_from_slice(spec.as_bytes());
        input.push(0);
    }
    input
}

/// The paths of a NUL-separated listing of paths.
fn listed_paths(listing: &[u8]) -> Vec<TreePath> {
    nul_fields(listing).map(TreePath::from_bytes).collect()
}

/// `paths` as they are shown (in an event, a note): bytes that are not
/// UTF-8 replaced.
fn shown_paths(paths: &[TreePath]) -> Vec<String> {
    paths.iter().map(TreePath::shown).collect()
}

/// First field of a pending merge's file.
const PENDING_TAG: &str = "pending-merge";

/// A merge of `main` whose conflict markers are not resolved yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    /// `main`'s commit: the second parent of the checkpoint that resolves
    /// the markers.
    main: String,
    /// The branch head the merge was written over; `None` in a file written
    /// before the head was recorded.
    written_over: Option<String>,
    /// The conflicted paths.
    files: Vec<TreePath>,
}

/// Workspace store backed by a bare git repository.
#[derive(Debug, Clone)]
pub struct GitWorkspace {
    repo: PathBuf,
    root: PathBuf,
    git: String,
    container: Option<Container>,
    /// Where large files are kept, as content-defined chunks.
    chunks: Arc<dyn ChunkStore>,
}

/// Directory of the default chunk store under a workspace root: mount and
/// unmount never clear it, and it is never inside a workspace.
pub const DEFAULT_CHUNK_DIRECTORY: &str = ".chunks";

/// What a checkpoint knew of one large file: its stat, when that stat was
/// taken, and its chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Known {
    stat: FileStat,
    /// Wall-clock time (ns since the epoch) just before the stat was taken.
    cached_at_ns: i128,
    chunks: Vec<String>,
}

/// How close a file's modification or change time may come to the time its
/// stat was cached before the entry is "racily clean" (git's term): a file
/// rewritten in the same timestamp tick as the stat, after it, keeps the
/// same stat, so such an entry is hashed again instead of trusted. Wide
/// enough for a coarse file system clock (2 s on FAT, 1 s on some network
/// file systems) and a little skew.
const RACY_WINDOW_NS: i128 = 3_000_000_000;

impl Known {
    /// Whether the stat alone cannot vouch for the content ([`RACY_WINDOW_NS`]).
    fn is_racy(&self) -> bool {
        self.stat.mtime_ns.max(self.stat.ctime_ns) + RACY_WINDOW_NS >= self.cached_at_ns
    }
}

/// What the stat cache compares: a rewrite changes the change time and
/// usually the modification time; a restored modification time (`touch -d`)
/// still moves the change time; a file replaced by another has a new inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStat {
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
    inode: u64,
    mode: u32,
}

/// The stat of a file, `None` when it is not a regular file.
fn stat_of(path: &Path) -> Option<FileStat> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)?;
    let nanoseconds =
        |seconds: i64, nanos: i64| i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    Some(FileStat {
        size: metadata.len(),
        mtime_ns: nanoseconds(metadata.mtime(), metadata.mtime_nsec()),
        ctime_ns: nanoseconds(metadata.ctime(), metadata.ctime_nsec()),
        inode: metadata.ino(),
        mode: mode_of(&metadata),
    })
}

/// Wall-clock time now, in nanoseconds since the epoch.
fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i128::try_from(since.as_nanos()).unwrap_or(0))
}

fn chunk_failure(error: impl std::fmt::Display) -> WorkspaceError {
    WorkspaceError::Unavailable(format!("chunk store: {error}"))
}

impl GitWorkspace {
    /// Store that keeps working trees on this machine.
    pub fn new(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Self {
        Self {
            repo: repo.as_ref().to_path_buf(),
            root: root.as_ref().to_path_buf(),
            git: "git".to_string(),
            container: None,
            chunks: Arc::new(LocalChunkStore::new(
                root.as_ref().join(DEFAULT_CHUNK_DIRECTORY),
            )),
        }
    }

    /// The same store keeping large files in `chunks` instead of the default
    /// directory under the root.
    #[must_use]
    pub fn with_chunks(mut self, chunks: Arc<dyn ChunkStore>) -> Self {
        self.chunks = chunks;
        self
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

    /// Stdout of a `-z` listing of paths, untrimmed bytes
    /// ([`report_listing`]).
    fn git_listing<S: AsRef<OsStr>>(&self, arguments: &[S]) -> Result<Vec<u8>, WorkspaceError> {
        let output = self.command().args(arguments).stdin(Stdio::null()).output();
        report_listing(output, &shown_arguments(arguments))
    }

    fn output_in<S: AsRef<OsStr>>(
        &self,
        directory: &Path,
        index: &Path,
        arguments: &[S],
    ) -> Result<String, WorkspaceError> {
        self.listing_in(directory, index, arguments)
            .map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
    }

    /// Stdout of a `-z` listing of paths in a work tree, untrimmed bytes.
    fn listing_in<S: AsRef<OsStr>>(
        &self,
        directory: &Path,
        index: &Path,
        arguments: &[S],
    ) -> Result<Vec<u8>, WorkspaceError> {
        let output = self
            .command_in(directory, index)
            .args(arguments)
            .stdin(Stdio::null())
            .output();
        report_listing(output, &shown_arguments(arguments))
    }

    /// [`Self::listing_in`] for a command that reads its pathspecs from
    /// standard input (`--pathspec-from-file=- --pathspec-file-nul`): any
    /// number of paths, none of them on the command line.
    fn listing_in_with_pathspecs<S: AsRef<OsStr>>(
        &self,
        directory: &Path,
        index: &Path,
        arguments: &[S],
        specs: &[OsString],
    ) -> Result<Vec<u8>, WorkspaceError> {
        let word = shown_arguments(arguments);
        let mut child = self
            .command_in(directory, index)
            .args(arguments)
            .args(["--pathspec-from-file=-", "--pathspec-file-nul"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| WorkspaceError::Unavailable(format!("run {word}: {error}")))?;
        let stdin = child.stdin.take();
        let input = nul_terminated(specs);
        // Written beside the read: a long answer must not block the input.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                use std::io::Write;
                stdin.map(|mut stdin| stdin.write_all(&input))
            });
            report_listing(child.wait_with_output(), &word)
        })
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

    /// Tree of the work tree in `directory`, built against `index`, and the
    /// large files left out of it.
    ///
    /// Build output and files over [`LARGE_FILE_BYTES`] leave the index after
    /// `add -A`, so a `.gitignore` inside the tree cannot re-include them; a
    /// large file is excluded before it is hashed.
    ///
    /// A large file that git already holds as a blob in one of `kept_in`
    /// (the branch head, `main`, a pending merge's `main`) stays a blob: it
    /// came from the source (main may get one from outside Spawn), and
    /// leaving it out of the tree would delete it from the branch, and from
    /// `main` when the branch lands.
    fn tree_of(
        &self,
        directory: &Path,
        index: &Path,
        kept_in: &[String],
    ) -> Result<(String, Vec<(TreePath, u64)>), WorkspaceError> {
        let mut large = self.large_files(directory, index)?;
        let blobs = self.blobs_in(kept_in, &large)?;
        large.retain(|(path, _)| !blobs.contains(path));
        self.stage_all_but(directory, index, &large)?;
        self.unstage(directory, index, &large)?;
        let tree = self.output_in(directory, index, &["write-tree"])?;
        // A file that grew past the limit between the size check and `add`
        // was hashed anyway; keep it off the branch all the same, and in the
        // chunk store with the other large files.
        let mut grown = self.large_blobs(&tree)?;
        let blobs = self.blobs_in(kept_in, &grown)?;
        grown.retain(|(path, _)| !blobs.contains(path));
        if grown.is_empty() {
            return Ok((tree, large));
        }
        self.unstage(directory, index, &grown)?;
        large.extend(grown);
        large.sort();
        large.dedup();
        let tree = self.output_in(directory, index, &["write-tree"])?;
        Ok((tree, large))
    }

    /// `git add -A` of the work tree in `directory` except `large`, which is
    /// never hashed. The paths go to git on standard input, so any number
    /// of large files fits.
    fn stage_all_but(
        &self,
        directory: &Path,
        index: &Path,
        large: &[(TreePath, u64)],
    ) -> Result<(), WorkspaceError> {
        let mut add = vec![OsString::from(".")];
        add.extend(
            large
                .iter()
                .map(|(path, _)| path.pathspec("exclude,literal")),
        );
        self.listing_in_with_pathspecs(directory, index, &["add", "-A"], &add)
            .map(|_| ())
    }

    /// Take build output and `paths` out of the index. Paths go to git as
    /// literals: a file named `*` or `:/` must not match others.
    fn unstage(
        &self,
        directory: &Path,
        index: &Path,
        paths: &[(TreePath, u64)],
    ) -> Result<(), WorkspaceError> {
        let mut remove: Vec<OsString> = BUILD_OUTPUT.into_iter().map(OsString::from).collect();
        remove.extend(paths.iter().map(|(path, _)| path.pathspec("literal")));
        self.listing_in_with_pathspecs(
            directory,
            index,
            &["rm", "-r", "--cached", "--ignore-unmatch", "--quiet"],
            &remove,
        )
        .map(|_| ())
    }

    /// Which of `paths` are a blob in at least one of `commits`.
    fn blobs_in(
        &self,
        commits: &[String],
        paths: &[(TreePath, u64)],
    ) -> Result<HashSet<TreePath>, WorkspaceError> {
        let mut found = HashSet::new();
        if paths.is_empty() {
            return Ok(found);
        }
        let asked: HashSet<&TreePath> = paths.iter().map(|(path, _)| path).collect();
        let batches =
            pathspec_batches(paths.iter().map(|(path, _)| path.pathspec("literal")).collect());
        for commit in commits {
            for batch in &batches {
                let mut arguments: Vec<OsString> =
                    ["ls-tree", "-r", "-z", "--full-tree", commit.as_str(), "--"]
                        .map(OsString::from)
                        .into();
                arguments.extend(batch.iter().cloned());
                let listed = self.git_listing(&arguments)?;
                for entry in nul_fields(&listed) {
                    // `<mode> blob <id>\t<path>`
                    if let Some((meta, path)) = tree_entry(entry)
                        && meta.split_whitespace().nth(1) == Some("blob")
                        && asked.contains(&path)
                    {
                        found.insert(path);
                    }
                }
            }
        }
        Ok(found)
    }

    /// The commits whose blobs a checkpoint of `branch` over `parent` keeps
    /// as blobs ([`Self::tree_of`]): the head, `main`, and the `main` of a
    /// pending merge.
    fn source_commits(&self, branch: &str, parent: &str) -> Vec<String> {
        let mut commits = vec![parent.to_string()];
        if let Ok(main) = self.commit_id_of(&format!("refs/heads/{BASE}")) {
            commits.push(main);
        }
        if let Some(pending) = self.pending_merge(branch) {
            commits.push(pending.main);
        }
        commits.sort();
        commits.dedup();
        commits
    }

    /// Blobs of `tree` larger than [`LARGE_FILE_BYTES`], with their sizes.
    fn large_blobs(&self, tree: &str) -> Result<Vec<(TreePath, u64)>, WorkspaceError> {
        let listed = self.git_listing(&["ls-tree", "-r", "-l", "-z", tree])?;
        Ok(nul_fields(&listed)
            .filter_map(|entry| {
                // `<mode> blob <id> <size>\t<path>`
                let (meta, path) = tree_entry(entry)?;
                let mut fields = meta.split_whitespace();
                let kind = fields.nth(1)?;
                let size: u64 = fields.nth(1)?.parse().ok()?;
                (kind == "blob" && size > LARGE_FILE_BYTES).then_some((path, size))
            })
            .collect())
    }

    /// Files git would commit that are larger than [`LARGE_FILE_BYTES`], with
    /// their sizes: tracked or untracked, ignore rules applied.
    fn large_files(
        &self,
        directory: &Path,
        index: &Path,
    ) -> Result<Vec<(TreePath, u64)>, WorkspaceError> {
        let listed = self.listing_in(
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
        let mut large: Vec<(TreePath, u64)> = listed_paths(&listed)
            .into_iter()
            .filter_map(|path| {
                let size = std::fs::symlink_metadata(directory.join(path.as_path()))
                    .ok()
                    .filter(std::fs::Metadata::is_file)?
                    .len();
                (size > LARGE_FILE_BYTES).then_some((path, size))
            })
            .collect();
        large.sort();
        large.dedup();
        Ok(large)
    }

    /// The stat cache of one branch's large files, beside its kept index:
    /// a file whose stat ([`FileStat`]) is unchanged and was not racily
    /// clean ([`Known::is_racy`]) is not chunked again.
    fn large_cache(&self, branch: &str) -> PathBuf {
        self.root.join(".large").join(branch)
    }

    /// The stat cache: a JSON array of entries, each path in the form of a
    /// manifest's ([`crate::chunks::put_path`]). An entry without every
    /// field (one of an earlier build) is dropped, and the file chunked
    /// again; a cache in another form counts as empty.
    fn read_cache(&self, branch: &str) -> BTreeMap<TreePath, Known> {
        let Ok(bytes) = std::fs::read(self.large_cache(branch)) else {
            return BTreeMap::new();
        };
        let Ok(Value::Array(files)) = serde_json::from_slice::<Value>(&bytes) else {
            return BTreeMap::new();
        };
        let number =
            |entry: &Value, key: &str| -> Option<i128> { entry.get(key)?.as_str()?.parse().ok() };
        files
            .iter()
            .filter_map(|entry| {
                let known = Known {
                    stat: FileStat {
                        size: entry.get("size")?.as_u64()?,
                        mtime_ns: number(entry, "mtime_ns")?,
                        ctime_ns: number(entry, "ctime_ns")?,
                        inode: entry.get("inode")?.as_u64()?,
                        mode: u32::try_from(entry.get("mode")?.as_u64()?).ok()?,
                    },
                    cached_at_ns: number(entry, "cached_at_ns")?,
                    chunks: entry
                        .get("chunks")?
                        .as_array()?
                        .iter()
                        .map(|chunk| chunk.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()?,
                };
                Some((crate::chunks::path_of(entry)?, known))
            })
            .collect()
    }

    /// Best effort: a lost cache only costs one re-chunk.
    fn write_cache(&self, branch: &str, known: &BTreeMap<TreePath, Known>) {
        let path = self.large_cache(branch);
        if known.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let files: Vec<Value> = known
            .iter()
            .map(|(file, known)| {
                let FileStat {
                    size,
                    mtime_ns,
                    ctime_ns,
                    inode,
                    mode,
                } = known.stat;
                let mut entry = json!({
                    "size": size, "mtime_ns": mtime_ns.to_string(),
                    "ctime_ns": ctime_ns.to_string(), "inode": inode, "mode": mode,
                    "cached_at_ns": known.cached_at_ns.to_string(), "chunks": known.chunks,
                });
                crate::chunks::put_path(&mut entry, file);
                entry
            })
            .collect();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temporary = path.with_extension("tmp");
        if std::fs::write(&temporary, Value::Array(files).to_string()).is_ok() {
            let _ = std::fs::rename(&temporary, &path);
        }
    }

    /// Whether the store still holds every chunk a stat-cache entry names,
    /// each made young again (a collection must not take it before this
    /// checkpoint's commit names it). False: chunk the file again.
    fn still_held(&self, chunks: &[String]) -> Result<bool, WorkspaceError> {
        for chunk in chunks {
            if !self.chunks.touch(chunk).map_err(chunk_failure)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The chunks of the stat-cache entry `cached` when they are still the
    /// file's at `full` with stat `stat`: the stat is the entry's, the store
    /// holds the chunks, and a racily clean entry hashes to the same chunks
    /// again. `None`: chunk the file again.
    fn cached_chunks(
        &self,
        cached: Option<&Known>,
        full: &Path,
        stat: FileStat,
    ) -> Result<Option<Vec<String>>, WorkspaceError> {
        let Some(entry) = cached.filter(|entry| entry.stat == stat) else {
            return Ok(None);
        };
        if !self.still_held(&entry.chunks)? {
            return Ok(None);
        }
        if !entry.is_racy() {
            return Ok(Some(entry.chunks.clone()));
        }
        let (chunks, read) = crate::chunks::chunk_names_of(full).map_err(chunk_failure)?;
        let same = chunks == entry.chunks && read == stat.size && stat_of(full) == Some(stat);
        Ok(same.then_some(chunks))
    }

    /// Chunk the large files of a checkpoint into the store and store their
    /// manifest; its name, or `None` when there are no large files.
    fn store_large(
        &self,
        branch: &str,
        directory: &Path,
        large: &[(TreePath, u64)],
    ) -> Result<Option<String>, WorkspaceError> {
        let cached = self.read_cache(branch);
        let mut known = BTreeMap::new();
        let mut files = Vec::with_capacity(large.len());
        for (path, _) in large {
            let full = directory.join(path.as_path());
            // Before the stat: a rewrite after it falls in the racy window.
            let cached_at_ns = now_ns();
            let Some(stat) = stat_of(&full) else {
                continue;
            };
            let (chunks, size, stable) =
                if let Some(chunks) = self.cached_chunks(cached.get(path), &full, stat)? {
                    (chunks, stat.size, true)
                } else {
                    let (chunks, read) =
                        store_file(self.chunks.as_ref(), &full).map_err(chunk_failure)?;
                    // A file that changed while it was read is recorded as
                    // read and chunked again at the next checkpoint.
                    let stable = stat_of(&full) == Some(stat) && read == stat.size;
                    (chunks, read, stable)
                };
            files.push(LargeFile {
                path: path.clone(),
                mode: stat.mode,
                size,
                chunks: chunks.clone(),
            });
            if stable {
                known.insert(
                    path.clone(),
                    Known {
                        stat,
                        cached_at_ns,
                        chunks,
                    },
                );
            }
        }
        self.write_cache(branch, &known);
        if files.is_empty() {
            return Ok(None);
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let manifest = Manifest { files }.to_bytes();
        self.chunks.put(&manifest).map(Some).map_err(chunk_failure)
    }
    /// The large-file manifest a commit names in its `Spawn-Large` trailer.
    fn manifest_of(&self, commit: &str) -> Result<Option<String>, WorkspaceError> {
        let message = self.git_output(&["show", "-s", "--format=%B", commit, "--"])?;
        Ok(crate::chunks::trailer_of(&message))
    }

    /// The manifest object `name`, read and checked.
    fn manifest(&self, name: &str) -> Result<Manifest, WorkspaceError> {
        let bytes = self.chunks.get(name).map_err(chunk_failure)?;
        Manifest::from_bytes(&bytes).map_err(chunk_failure)
    }

    /// The large files of `manifest` that `tree` has no room for: the tree
    /// holds an entry at the file's own path, or something other than a
    /// directory (a symlink, a file) at one of its parent directories. Such a
    /// file cannot be materialized without replacing a tracked file or
    /// writing through a symlink, so a merge that makes one is refused and a
    /// mount of one fails.
    ///
    /// Every path and each of its leading directories is asked of `git
    /// ls-tree -t -z` as a literal pathspec, so a name of any bytes (a
    /// newline too) is asked as it is.
    fn large_clashes(
        &self,
        manifest: &Manifest,
        tree: &str,
    ) -> Result<Vec<TreePath>, WorkspaceError> {
        let asked: std::collections::BTreeSet<TreePath> = manifest
            .files
            .iter()
            .flat_map(|file| file.path.prefixes())
            .collect();
        if asked.is_empty() {
            return Ok(Vec::new());
        }
        // The type of each entry asked for; a directory's other children
        // the listing shows on the way are not asked for.
        let mut kinds: BTreeMap<TreePath, String> = BTreeMap::new();
        for batch in pathspec_batches(asked.iter().map(|path| path.pathspec("literal")).collect()) {
            let mut arguments: Vec<OsString> = ["ls-tree", "-t", "-z", "--full-tree", tree, "--"]
                .map(OsString::from)
                .into();
            arguments.extend(batch);
            let listed = self.git_listing(&arguments)?;
            kinds.extend(nul_fields(&listed).filter_map(|entry| {
                // `<mode> <type> <id>\t<path>`
                let (meta, path) = tree_entry(entry)?;
                let kind = meta.split_whitespace().nth(1)?.to_string();
                asked.contains(&path).then_some((path, kind))
            }));
        }
        Ok(manifest
            .files
            .iter()
            .filter(|file| {
                file.path.prefixes().any(|prefix| {
                    let kind = kinds.get(&prefix);
                    if prefix == file.path {
                        kind.is_some()
                    } else {
                        kind.is_some_and(|kind| kind != "tree")
                    }
                })
            })
            .map(|file| file.path.clone())
            .collect())
    }

    /// The manifest `head` names, which a merge commit over `tree` keeps,
    /// and the large files of it `tree` has no room for. The branch keeps its
    /// own large files, so a main that put a file or a symlink where a large
    /// file or its directory is would make the next mount write through it:
    /// the merge is refused, like a conflict markers cannot show.
    fn manifest_over(
        &self,
        head: &str,
        tree: &str,
    ) -> Result<(Option<String>, Vec<TreePath>), WorkspaceError> {
        let Some(name) = self.manifest_of(head)? else {
            return Ok((None, Vec::new()));
        };
        let clashes = self.large_clashes(&self.manifest(&name)?, tree)?;
        Ok((Some(name), clashes))
    }

    /// Write the large files the branch head names into `directory` (a
    /// mount being staged), and their stat-cache entries, so the next
    /// checkpoint does not chunk them again. Every chunk is fetched and
    /// verified; a file is written whole or not at all, and only inside the
    /// directory ([`materialize`]). A manifest the head's tree has no room
    /// for ([`Self::large_clashes`]) fails.
    fn materialize_large(
        &self,
        branch: &str,
        directory: &Path,
    ) -> Result<BTreeMap<TreePath, Known>, WorkspaceError> {
        let head = format!("refs/heads/{branch}");
        let mut known = BTreeMap::new();
        let Some(name) = self.manifest_of(&head)? else {
            return Ok(known);
        };
        let manifest = self.manifest(&name)?;
        let clashes = self.large_clashes(&manifest, &format!("{head}^{{tree}}"))?;
        if !clashes.is_empty() {
            return Err(WorkspaceError::Unavailable(format!(
                "large files {} clash with the tree of {branch}: a tracked entry is at the \
                 path, or a symlink or a file at a parent directory",
                shown_paths(&clashes).join(", ")
            )));
        }
        for file in &manifest.files {
            materialize(self.chunks.as_ref(), directory, file).map_err(|error| {
                WorkspaceError::Unavailable(format!("large file {}: {error}", file.path))
            })?;
            // Just written, so racily clean: the first checkpoint hashes it
            // once more, then trusts its stat.
            let cached_at_ns = now_ns();
            if let Some(stat) = stat_of(&directory.join(file.path.as_path())) {
                known.insert(
                    file.path.clone(),
                    Known {
                        stat,
                        cached_at_ns,
                        chunks: file.chunks.clone(),
                    },
                );
            }
        }
        Ok(known)
    }

    /// Where a mount is staged before it replaces a workspace, and where the
    /// replaced one goes until it is removed: `<root>/.mount/`, on the
    /// workspace's file system, so each step is one rename. A branch name
    /// holds no `.`, so `<branch>.` starts this branch's entries only.
    fn staging_directory(&self) -> PathBuf {
        self.root.join(".mount")
    }

    /// A fresh path under [`Self::staging_directory`] for `branch`, and the
    /// leftovers of an earlier mount of it (one that died midway) removed.
    fn staging_path(&self, branch: &str, what: &str) -> Result<PathBuf, WorkspaceError> {
        let staging = self.staging_directory();
        std::fs::create_dir_all(&staging).map_err(|error| {
            WorkspaceError::Unavailable(format!("create {}: {error}", staging.display()))
        })?;
        let prefix = format!("{branch}.");
        let ours = format!("{branch}.{}.", std::process::id());
        if let Ok(entries) = std::fs::read_dir(&staging) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) && !name.starts_with(&ours) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        Ok(staging.join(format!(
            "{ours}{what}.{}",
            STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )))
    }

    /// Export the branch head into a new directory beside the workspaces,
    /// large files and all: everything a mount needs, fetched and verified
    /// before the workspace is touched. Its path and the large files'
    /// stat-cache entries; on any error the directory is removed.
    fn stage_mount(
        &self,
        branch: &str,
    ) -> Result<(PathBuf, BTreeMap<TreePath, Known>), WorkspaceError> {
        let staged = self.staging_path(branch, "staged")?;
        let built = std::fs::create_dir(&staged)
            .map_err(|error| {
                WorkspaceError::Unavailable(format!("create {}: {error}", staged.display()))
            })
            .and_then(|()| self.export(branch, &staged))
            .and_then(|()| self.materialize_large(branch, &staged));
        match built {
            Ok(known) => Ok((staged, known)),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staged);
                Err(error)
            }
        }
    }

    /// Write the tree of the branch head into `directory` (`git archive`).
    fn export(&self, branch: &str, directory: &Path) -> Result<(), WorkspaceError> {
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
            .arg(directory)
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
        Ok(())
    }

    /// Put the staged mount `staged` where the workspace `directory` is: the
    /// old workspace is renamed aside, the staged one renamed into place,
    /// then the old one removed. A failed second rename puts the old one
    /// back.
    fn replace_workspace(
        &self,
        branch: &str,
        staged: &Path,
        directory: &Path,
    ) -> Result<(), WorkspaceError> {
        let replaced = match std::fs::symlink_metadata(directory) {
            Ok(_) => {
                let aside = self.staging_path(branch, "replaced")?;
                std::fs::rename(directory, &aside).map_err(|error| {
                    WorkspaceError::Unavailable(format!(
                        "move {} aside: {error}",
                        directory.display()
                    ))
                })?;
                Some(aside)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(WorkspaceError::Unavailable(format!(
                    "stat {}: {error}",
                    directory.display()
                )));
            }
        };
        if let Err(error) = std::fs::rename(staged, directory) {
            if let Some(aside) = &replaced {
                let _ = std::fs::rename(aside, directory);
            }
            return Err(WorkspaceError::Unavailable(format!(
                "move the new tree to {}: {error}",
                directory.display()
            )));
        }
        if let Some(aside) = replaced {
            let _ = std::fs::remove_dir_all(aside);
        }
        Ok(())
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

    /// Export the branch head as the workspace. The whole new tree, its
    /// large files fetched and verified chunk by chunk, is built beside the
    /// workspace first ([`GitWorkspace::stage_mount`]): a mount that fails
    /// (no commit, a chunk missing or corrupt, a clash) leaves the existing
    /// workspace, uncommitted files and all, as it was. Only then is the
    /// workspace replaced, by renames.
    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.exclude_build_output()?;
        self.git_output(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .map_err(|_| WorkspaceError::Unavailable(format!("branch {branch} has no commit")))?;
        let (staged, known) = self.stage_mount(branch.as_str())?;

        // A pending merge stays only when the branch carries its markers.
        self.settle_pending(branch.as_str());
        let directory = self.root.join(branch.as_str());
        if let Err(error) = self.replace_workspace(branch.as_str(), &staged, &directory) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error);
        }
        // A rename keeps each file's inode and times: the entries hold.
        self.write_cache(branch.as_str(), &known);
        // A fresh export is the branch head: nothing stale is left.
        self.clear_stale(branch.as_str());

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
        self.refuse_stale(&branch)?;
        let reference = format!("refs/heads/{branch}");
        let parent = self.git_output(&["rev-parse", &reference])?;
        let tree = self.tree_of(&directory, &index, &self.source_commits(&branch, &parent));
        if tree.is_err() {
            let _ = std::fs::remove_file(&index);
        }
        let (tree, large) = tree?;
        let manifest = self.store_large(&branch, &directory, &large)?;

        let parent_tree = self.git_output(&["rev-parse", &format!("{parent}^{{tree}}")])?;
        // A merge of main whose conflicts are now resolved: main is the
        // second parent, even when the tree did not change.
        let merged = self.resolved_merge(&branch, &directory);
        if tree == parent_tree && merged.is_none() && manifest == self.manifest_of(&parent)? {
            return Ok(Commit::Unchanged);
        }
        let message = crate::chunks::with_trailer(message, manifest.as_deref());
        let mut arguments = vec!["commit-tree", tree.as_str(), "-p", parent.as_str()];
        if let Some(main) = merged.as_deref() {
            arguments.extend(["-p", main]);
        }
        arguments.extend(["-m", message.as_str()]);
        let commit = self.git_output(&arguments)?;
        // Expected old value: a branch that moved is not overwritten.
        self.git_output(&["update-ref", &reference, &commit, &parent])
            .map_err(|error| {
                WorkspaceError::Unavailable(format!("{error} (branch moved under the commit)"))
            })?;
        if merged.is_some() {
            self.clear_pending(&branch);
        }
        CommitId::new(commit)
            .map(Commit::Created)
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        let directory = self.checked_path(workspace)?;
        // A pending merge stays only when the branch carries its markers.
        self.settle_pending(workspace.sandbox());
        if let Some(container) = self.container.as_ref() {
            let _ = Command::new(&container.docker)
                .args(["rm", "-f", &container_name(workspace.sandbox())])
                .stdin(Stdio::null())
                .output();
        }
        match std::fs::remove_dir_all(&directory) {
            Ok(()) => {
                self.clear_stale(workspace.sandbox());
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.clear_stale(workspace.sandbox());
                Ok(())
            }
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
    /// checkpoint commit the old files over the merge and revert `main`. A
    /// compare-and-swap that loses to another writer takes the directory to
    /// the branch's new head ([`Self::follow_moved_branch`]).
    ///
    /// A content conflict is written into the directory the same safe way,
    /// conflict markers and all, and `main` waits under `.merge/<branch>` as
    /// the second parent of the checkpoint that has them resolved (the way
    /// `git merge` leaves `MERGE_HEAD`). Until then no newer `main` is merged:
    /// markers never stack. A conflict markers cannot show (modify/delete,
    /// renames, binary files) changes nothing.
    fn merge_main(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        let branch = workspace.sandbox().to_string();
        let directory = self.checked_path(workspace)?;
        // The integrator commits this workspace too: no checkpoint reads the
        // directory halfway through a merge.
        let _lock = self.lock_branch(&branch)?;
        self.refuse_stale(&branch)?;
        if let Some(Pending { main, files, .. }) = self.pending_merge(&branch) {
            if unresolved(&directory, &self.shortest_markers(&[&main], &files)?) {
                return Ok(BaseMerge::Conflicted {
                    files: shown_paths(&files),
                    marked: true,
                });
            }
            // Resolved: the next checkpoint commits the merge.
            return Ok(BaseMerge::Current);
        }
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
        let (merged, files, kinds) = merge_tree_output(&output.stdout);
        let head_tree = self.git_output(&["rev-parse", &format!("{head}^{{tree}}")])?;
        match output.status.code() {
            Some(0) => {}
            Some(1) => {
                // Only a conflict in a file's lines is shown by markers; a
                // listing without any conflict kind is not trusted.
                let mut conflicts = kinds.iter().filter(|kind| kind.starts_with("CONFLICT"));
                let marked = conflicts.clone().next().is_some()
                    && conflicts.all(|kind| kind == "CONFLICT (contents)");
                if marked {
                    self.update_directory(&branch, &directory, &head_tree, &merged)?;
                    if let Err(error) = self.write_pending(&branch, &main, &head, &files) {
                        // Without its pending parent the merge must not stay.
                        let _ = self.update_directory(&branch, &directory, &merged, &head_tree);
                        return Err(error);
                    }
                }
                return Ok(BaseMerge::Conflicted {
                    files: shown_paths(&files),
                    marked,
                });
            }
            _ => {
                return Err(WorkspaceError::Unavailable(format!(
                    "git merge-tree {main} {head}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        let (manifest, clashes) = self.manifest_over(&head, &merged)?;
        if !clashes.is_empty() {
            return Ok(BaseMerge::Conflicted {
                files: shown_paths(&clashes),
                marked: false,
            });
        }
        self.update_directory(&branch, &directory, &head_tree, &merged)?;
        let changed =
            self.git_listing(&["diff-tree", "-r", "--name-only", "-z", &head_tree, &merged])?;
        let files = shown_paths(&listed_paths(&changed));
        let message = crate::chunks::with_trailer(
            &format!("spawn: merge main {main} into {branch}"),
            manifest.as_deref(),
        );
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
        if let Err(error) = self.git_output(&["update-ref", &reference, &commit, &head]) {
            return Err(self.follow_moved_branch(&branch, &directory, &merged, &error));
        }
        Ok(BaseMerge::Merged {
            commit: CommitId::new(commit)
                .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?,
            files,
        })
    }

    /// After a merge wrote tree `merged` into `directory` and its
    /// compare-and-swap lost (another writer moved the branch), bring the
    /// directory to the branch's head as it is now, the same two-tree way:
    /// otherwise the next checkpoint would commit the merge over the other
    /// writer's commit as plain edits. When that is refused (the directory
    /// changed meanwhile in a file the two trees differ in), the directory
    /// is marked stale: checkpoints are refused until it is mounted again.
    /// The answer is the error the merge returns.
    fn follow_moved_branch(
        &self,
        branch: &str,
        directory: &Path,
        merged: &str,
        lost: &WorkspaceError,
    ) -> WorkspaceError {
        let followed = self
            .commit_id_of(&format!("refs/heads/{branch}"))
            .and_then(|now| {
                let tree = self.git_output(&["rev-parse", &format!("{now}^{{tree}}")])?;
                self.update_directory(branch, directory, merged, &tree)?;
                Ok(now)
            });
        match followed {
            Ok(now) => WorkspaceError::Unavailable(format!(
                "{lost} (branch moved under the merge; the workspace follows its head {now} \
                 again)"
            )),
            Err(error) => {
                let marked = self.mark_stale(branch);
                WorkspaceError::Unavailable(format!(
                    "{lost} (branch moved under the merge, and the workspace cannot follow \
                     its head: {error}; checkpoints are refused until it is mounted again{})",
                    marked
                        .err()
                        .map_or(String::new(), |error| format!(", {error}"))
                ))
            }
        }
    }

    /// Where a branch's workspace is marked stale ([`Self::follow_moved_branch`]).
    fn stale_path(&self, branch: &str) -> PathBuf {
        self.root.join(".stale").join(branch)
    }

    fn mark_stale(&self, branch: &str) -> Result<(), WorkspaceError> {
        let path = self.stale_path(branch);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                WorkspaceError::Unavailable(format!("create {}: {error}", parent.display()))
            })?;
        }
        std::fs::write(&path, b"").map_err(|error| {
            WorkspaceError::Unavailable(format!("write {}: {error}", path.display()))
        })
    }

    fn clear_stale(&self, branch: &str) {
        let _ = std::fs::remove_file(self.stale_path(branch));
    }

    /// Refuse to commit or merge a workspace marked stale.
    fn refuse_stale(&self, branch: &str) -> Result<(), WorkspaceError> {
        if !self.stale_path(branch).exists() {
            return Ok(());
        }
        Err(WorkspaceError::Refused {
            code: "WorkspaceStale".to_string(),
            message: format!(
                "the workspace of {branch} holds a merge of main its branch never took (another \
                 writer moved the branch) and changes of its own; mount it again"
            ),
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

    /// Where a merge of `main` whose conflicts are not resolved yet waits
    /// ([`Pending`]), NUL-separated: [`PENDING_TAG`], `main`'s commit, the
    /// branch head the merge was written over, then the conflicted paths.
    fn pending_path(&self, branch: &str) -> PathBuf {
        self.root.join(".merge").join(branch)
    }

    /// The pending merge of a branch. An unreadable or malformed file counts
    /// as none; so does one without the head it was written over (the form
    /// before the head was recorded is `main`, then the paths).
    fn pending_merge(&self, branch: &str) -> Option<Pending> {
        let bytes = std::fs::read(self.pending_path(branch)).ok()?;
        let mut fields = nul_fields(&bytes);
        let commit = |field: &[u8]| {
            (field.iter().all(u8::is_ascii_hexdigit))
                .then(|| String::from_utf8_lossy(field).into_owned())
        };
        let first = fields.next()?;
        let (main, written_over) = if first == PENDING_TAG.as_bytes() {
            let main = commit(fields.next()?)?;
            let head = commit(fields.next()?)?;
            (main, Some(head))
        } else {
            (commit(first)?, None)
        };
        Some(Pending {
            main,
            written_over,
            files: fields.map(TreePath::from_bytes).collect(),
        })
    }

    /// Record a pending merge of `main` written over the branch head `head`,
    /// atomically: a write to a temporary file, then a rename.
    fn write_pending(
        &self,
        branch: &str,
        main: &str,
        head: &str,
        files: &[TreePath],
    ) -> Result<(), WorkspaceError> {
        let path = self.pending_path(branch);
        let unavailable = |what: &str, error: std::io::Error| {
            WorkspaceError::Unavailable(format!("{what} {}: {error}", path.display()))
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| unavailable("create", error))?;
        }
        let mut text = format!("{PENDING_TAG}\0{main}\0{head}\0").into_bytes();
        for file in files {
            text.extend_from_slice(file.as_bytes());
            text.push(0);
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, text).map_err(|error| unavailable("write", error))?;
        std::fs::rename(&temporary, &path).map_err(|error| unavailable("rename", error))
    }

    /// The second parent of the next checkpoint of `branch`: the pending
    /// `main`, once no conflicted file in `directory` holds a marker. A
    /// pending `main` the branch already contains is cleared. Markers whose
    /// length cannot be read count as still there: this checkpoint is no
    /// resolution, a later one may be.
    fn resolved_merge(&self, branch: &str, directory: &Path) -> Option<String> {
        let Pending { main, files, .. } = self.pending_merge(branch)?;
        let contained = self
            .commit_id_of(&format!("refs/heads/{branch}"))
            .and_then(|head| self.is_ancestor(&main, &head));
        if matches!(contained, Ok(true)) {
            self.clear_pending(branch);
            return None;
        }
        let sized = self.shortest_markers(&[&main], &files).ok()?;
        (!unresolved(directory, &sized)).then_some(main)
    }

    /// Each of `paths` with the shortest conflict markers a merge could have
    /// written in it: git's default ([`CONFLICT_MARKER_SIZE`]), or less when
    /// the `.gitattributes` of one of `sources` (the commits whose merge
    /// rules apply, read with `--attr-source` as the merge reads them) sets
    /// `conflict-marker-size` lower for the path. Longer markers are always
    /// found ([`has_conflict_block_sized`]).
    fn shortest_markers(
        &self,
        sources: &[&str],
        paths: &[TreePath],
    ) -> Result<Vec<(TreePath, usize)>, WorkspaceError> {
        let mut sized: Vec<(TreePath, usize)> = paths
            .iter()
            .map(|path| (path.clone(), CONFLICT_MARKER_SIZE))
            .collect();
        if paths.is_empty() {
            return Ok(sized);
        }
        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_bytes());
            input.push(0);
        }
        for source in sources {
            let mut child = self
                .command()
                .arg(format!("--attr-source={source}"))
                .args(["check-attr", "-z", "--stdin", "conflict-marker-size"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| {
                    WorkspaceError::Unavailable(format!("run {}: {error}", self.git))
                })?;
            let stdin = child.stdin.take();
            // Written beside the read: a long answer must not block the
            // question.
            let listed = std::thread::scope(|scope| {
                scope.spawn(|| {
                    use std::io::Write;
                    stdin.map(|mut stdin| stdin.write_all(&input))
                });
                report_listing(child.wait_with_output(), "check-attr conflict-marker-size")
            })?;
            // `<path> NUL conflict-marker-size NUL <value> NUL` per path.
            let fields: Vec<&[u8]> = listed.split(|byte| *byte == 0).collect();
            for answer in fields.chunks_exact(3) {
                let size = marker_size_of(answer[2]);
                for (path, shortest) in &mut sized {
                    if path.as_bytes() == answer[0] {
                        *shortest = (*shortest).min(size);
                    }
                }
            }
        }
        Ok(sized)
    }

    fn clear_pending(&self, branch: &str) {
        let _ = std::fs::remove_file(self.pending_path(branch));
    }

    /// When a workspace's directory goes (unmount) or is exported afresh
    /// (mount): keep the pending merge only when the branch carries it. That
    /// is when a checkpoint was taken since the merge was written (the
    /// branch head moved on from the head the merge was written over), so
    /// the committed tree holds the merge's markers and main's other changes
    /// and the next export shows them; and when main is not yet an ancestor
    /// of the branch. The checkpoint that resolves the markers then takes
    /// main as its second parent, as it would have in the same directory.
    ///
    /// Otherwise it goes: the head is still the pre-merge one, so a fresh
    /// export holds none of main's changes and main must never become a
    /// parent of it (the next idle merge writes the merge again); or main is
    /// already contained; or the file is unreadable or has no head.
    fn settle_pending(&self, branch: &str) {
        let carried = self.pending_merge(branch).is_some_and(|pending| {
            let Some(written_over) = pending.written_over else {
                return false;
            };
            let Ok(head) = self.commit_id_of(&format!("refs/heads/{branch}")) else {
                return false;
            };
            head != written_over
                && matches!(self.is_ancestor(&written_over, &head), Ok(true))
                && matches!(self.is_ancestor(&pending.main, &head), Ok(false))
        });
        if !carried {
            self.clear_pending(branch);
        }
    }

    /// Hold the branch's `<index>.spawn-lock`, as a checkpoint does, until
    /// the returned file is dropped.
    fn lock_branch(&self, branch: &str) -> Result<std::fs::File, WorkspaceError> {
        let lock_path = self.index(branch).with_extension("spawn-lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                WorkspaceError::Unavailable(format!("create {}: {error}", parent.display()))
            })?;
        }
        let lock = std::fs::File::create(&lock_path).map_err(|error| {
            WorkspaceError::Unavailable(format!("create {}: {error}", lock_path.display()))
        })?;
        lock.lock().map_err(|error| {
            WorkspaceError::Unavailable(format!("lock {}: {error}", lock_path.display()))
        })?;
        Ok(lock)
    }

    /// Whether a merge of `main` into `branch` waits for its conflict
    /// markers to be resolved.
    pub fn has_pending_merge(&self, branch: &str) -> bool {
        self.pending_merge(branch).is_some()
    }

    /// The files `commit` changes against `base` that hold a whole conflict
    /// marker block ([`has_conflict_block`]): work that must not be judged,
    /// let alone land. The paths are shown lossily; each file is read by its
    /// own bytes.
    pub fn conflict_markers(
        &self,
        base: &str,
        commit: &str,
    ) -> Result<Vec<String>, WorkspaceError> {
        let changed = self.git_listing(&[
            "diff-tree",
            "-r",
            "--raw",
            "--diff-filter=AM",
            "-z",
            base,
            commit,
        ])?;
        // `:<old mode> <new mode> <old id> <new id> <status>`, then the path.
        let mut records = nul_fields(&changed);
        let mut files = Vec::new();
        while let (Some(meta), Some(path)) = (records.next(), records.next()) {
            let mode = String::from_utf8_lossy(meta)
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            // A submodule's commit is not in this repository.
            if mode != "160000" {
                files.push(TreePath::from_bytes(path));
            }
        }
        let mut marked = Vec::new();
        for (path, shortest) in self.shortest_markers(&[base, commit], &files)? {
            let output = self
                .command()
                .arg("cat-file")
                .arg("blob")
                .arg(path.in_tree(commit))
                .stdin(Stdio::null())
                .output()
                .map_err(|error| {
                    WorkspaceError::Unavailable(format!("run {}: {error}", self.git))
                })?;
            if !output.status.success() {
                // Listed as added or modified, so it is there: a blob that
                // cannot be read is not one without markers.
                return Err(WorkspaceError::Unavailable(format!(
                    "git cat-file blob {commit}:{path}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            if has_conflict_block_sized(&output.stdout, shortest) {
                marked.push(path.shown());
            }
        }
        Ok(marked)
    }
}

/// `<meta>\t<path>`, an entry of a `-z` tree listing: the meta fields
/// (ASCII) and the path as bytes.
fn tree_entry(entry: &[u8]) -> Option<(&str, TreePath)> {
    let tab = entry.iter().position(|byte| *byte == b'\t')?;
    let meta = std::str::from_utf8(&entry[..tab]).ok()?;
    Some((meta, TreePath::from_bytes(&entry[tab + 1..])))
}

/// The tree, the conflicted paths and the message kinds (`Auto-merging`,
/// `CONFLICT (contents)`, `CONFLICT (modify/delete)`, ...: stable, never
/// translated) of `git merge-tree --write-tree --name-only -z`.
fn merge_tree_output(stdout: &[u8]) -> (String, Vec<TreePath>, Vec<String>) {
    let mut fields = stdout.split(|byte| *byte == 0);
    let tree = String::from_utf8_lossy(fields.next().unwrap_or_default())
        .trim()
        .to_string();
    let mut files: Vec<TreePath> = fields
        .by_ref()
        .take_while(|field| !field.is_empty())
        .map(TreePath::from_bytes)
        .collect();
    files.dedup();
    // Each message: a path count, the paths, its kind, its text.
    let mut kinds = Vec::new();
    while let Some(count) = fields
        .next()
        .and_then(|field| std::str::from_utf8(field).ok()?.parse::<usize>().ok())
    {
        if fields.by_ref().take(count).count() < count {
            break;
        }
        let Some(kind) = fields.next() else { break };
        kinds.push(String::from_utf8_lossy(kind).into_owned());
        fields.next();
    }
    (tree, files, kinds)
}

/// Whether any of `files` (the ones the merge listed) in `directory` still
/// holds a whole conflict marker block whose markers are at least as long
/// as the path's shortest ([`GitWorkspace::shortest_markers`]). A deleted
/// file holds none.
fn unresolved(directory: &Path, files: &[(TreePath, usize)]) -> bool {
    files.iter().any(|(file, shortest)| {
        std::fs::read(directory.join(file.as_path()))
            .is_ok_and(|bytes| has_conflict_block_sized(&bytes, *shortest))
    })
}

/// The length of the conflict markers git writes by default.
pub const CONFLICT_MARKER_SIZE: usize = 7;

/// Whether `bytes` hold a whole conflict marker block of git's default
/// marker size or longer ([`has_conflict_block_sized`]).
pub fn has_conflict_block(bytes: &[u8]) -> bool {
    has_conflict_block_sized(bytes, CONFLICT_MARKER_SIZE)
}

/// Whether `bytes` hold a whole conflict marker block whose markers are
/// `shortest` characters long or longer: a line of `n` `<` (alone or before
/// a space and a label), later a line that is exactly `n` `=`, later a line
/// of `n` `>` (alone or before a space), one `n` for the whole block. git
/// writes markers of `conflict-marker-size` (7 unless `.gitattributes` sets
/// it), and longer ones (by two per level) for a conflict inside a merge
/// base, which a diff3 base section shows; a block inside another counts on
/// its own. A lone `=======` (a Markdown heading's underline) is not one,
/// nor is a block whose three markers differ in length.
pub fn has_conflict_block_sized(bytes: &[u8], shortest: usize) -> bool {
    let shortest = shortest.max(1);
    // The stage each marker length has reached: 1 after its `<` line, 2
    // after its `=` line.
    let mut stages: BTreeMap<usize, u8> = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(&first) = line.first() else {
            continue;
        };
        let run = line.iter().take_while(|byte| **byte == first).count();
        if run < shortest {
            continue;
        }
        let labelled = matches!(line.get(run), None | Some(b' '));
        let stage = stages.entry(run).or_insert(0);
        match (first, *stage) {
            (b'<', 0) if labelled => *stage = 1,
            (b'=', 1) if run == line.len() => *stage = 2,
            (b'>', 2) if labelled => return true,
            _ => {}
        }
    }
    false
}

/// The marker size a `conflict-marker-size` attribute value sets: a
/// positive number; anything else (`unspecified`, `set`, ...) leaves git's
/// default.
fn marker_size_of(value: &[u8]) -> usize {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(CONFLICT_MARKER_SIZE)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use spawn_core::id::{BranchName, WorkspaceId};
    use spawn_core::port::{BaseMerge, Commit, WorkspaceError, WorkspaceStore};

    use super::GitWorkspace;

    fn git(directory: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args([
                "-c",
                "user.name=spawn-test",
                "-c",
                "user.email=spawn@localhost",
            ])
            .args(arguments)
            .output()
            .expect("git is installed");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Far more large paths than one command line holds (Linux: 2 MiB of
    /// arguments) still go through every git call that names them: the
    /// checkpoint's `add`, `rm --cached`, the blob lookup and the clash check.
    #[test]
    fn many_large_paths_never_overflow_a_command_line() {
        use super::{LargeFile, Manifest, TreePath};
        let base =
            std::env::temp_dir().join(format!("spawn-git-many-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let checkout = base.join("repo");
        std::fs::create_dir_all(checkout.join("big")).expect("create the repository");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        let long = "x".repeat(200);
        let names: Vec<String> = (0..20_000).map(|n| format!("big/{long}-{n}")).collect();
        // Two of them are blobs on main, and one more file is not asked for.
        for name in &names[..2] {
            std::fs::write(checkout.join(name), "held\n").expect("write");
        }
        std::fs::write(checkout.join("other.txt"), "other\n").expect("write");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);
        let store = GitWorkspace::new(checkout.join(".git"), base.join("work"));
        let paths: Vec<(TreePath, u64)> = names
            .iter()
            .map(|name| (TreePath::from(name.as_str()), 1))
            .collect();
        let bytes: usize = names.iter().map(|name| name.len() + 12).sum();
        assert!(bytes > 2 << 20, "the paths alone exceed ARG_MAX: {bytes}");

        let held = store.blobs_in(&["main".to_string()], &paths).expect("blob lookup");
        let mut held: Vec<String> = held.iter().map(ToString::to_string).collect();
        held.sort();
        let mut expected = names[..2].to_vec();
        expected.sort();
        assert_eq!(held, expected);

        let manifest = Manifest {
            files: names
                .iter()
                .map(|name| LargeFile {
                    path: TreePath::from(name.as_str()),
                    mode: 0o100_644,
                    size: 1,
                    chunks: Vec::new(),
                })
                .collect(),
        };
        let tree = git(&checkout, &["rev-parse", "main^{tree}"]);
        let mut clashes: Vec<String> = store
            .large_clashes(&manifest, &tree)
            .expect("clash check")
            .iter()
            .map(ToString::to_string)
            .collect();
        clashes.sort();
        assert_eq!(clashes, expected, "a blob where a large file goes clashes");

        let staged = |index: &Path| {
            let listed = Command::new("git")
                .arg("-C")
                .arg(&checkout)
                .arg("ls-files")
                .env("GIT_INDEX_FILE", index)
                .output()
                .expect("ls-files");
            String::from_utf8_lossy(&listed.stdout).to_string()
        };
        std::fs::write(checkout.join("added.txt"), "added\n").expect("write");
        let index = base.join("index");
        store
            .stage_all_but(&checkout, &index, &paths)
            .expect("add with every large path excluded");
        assert_eq!(staged(&index), "added.txt\nother.txt\n", "no large path is staged");
        // An index that holds two of them loses both.
        let full = base.join("full-index");
        let added = Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["add", "-A"])
            .env("GIT_INDEX_FILE", &full)
            .status()
            .expect("add");
        assert!(added.success());
        assert_eq!(staged(&full).lines().count(), 4);
        store
            .unstage(&checkout, &full, &paths)
            .expect("rm --cached of every large path");
        assert_eq!(staged(&full), "added.txt\nother.txt\n");
        std::fs::remove_dir_all(&base).expect("clean up");
    }

    /// A repository with `main` checked out, an agent branch with one
    /// checkpoint mounted, and `main` moved on by a new file: a clean merge
    /// is due. Returns the checkout, the store, the workspace and the base
    /// directory.
    fn due_merge(tag: &str) -> (PathBuf, GitWorkspace, WorkspaceId, PathBuf) {
        let base = std::env::temp_dir().join(format!("spawn-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let checkout = base.join("repo");
        std::fs::create_dir_all(&checkout).expect("create the repository");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(checkout.join("README.md"), "base\n").expect("write");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);
        let store = GitWorkspace::new(checkout.join(".git"), base.join("work"));
        let branch = BranchName::new("agent_a").expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        std::fs::write(Path::new(workspace.path()).join("work.txt"), "mine\n").expect("write");
        assert!(matches!(
            store.commit(&workspace, "work").expect("commit"),
            Commit::Created(_)
        ));
        std::fs::write(checkout.join("new.txt"), "theirs\n").expect("write on main");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "landed"]);
        (checkout, store, workspace, base)
    }

    /// Another writer's commit on `agent_a`: its head plus `other.txt`.
    fn other_writers_commit(checkout: &Path) -> String {
        let index = checkout.join(".git").join("other-writer-index");
        let with_index = |arguments: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(checkout)
                .args(arguments)
                .env("GIT_INDEX_FILE", &index)
                .output()
                .expect("git is installed");
            assert!(output.status.success(), "git {arguments:?}: {output:?}");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        with_index(&["read-tree", "refs/heads/agent_a"]);
        std::fs::write(checkout.join("other.txt"), "the other writer\n").expect("write");
        let blob = git(checkout, &["hash-object", "-w", "other.txt"]);
        std::fs::remove_file(checkout.join("other.txt")).expect("remove");
        with_index(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},other.txt"),
        ]);
        let tree = with_index(&["write-tree"]);
        git(
            checkout,
            &[
                "commit-tree",
                &tree,
                "-p",
                "refs/heads/agent_a",
                "-m",
                "the other writer",
            ],
        )
    }

    /// A git that, just before the merge commit is written (after the
    /// directory took the merge, before the compare-and-swap), moves
    /// `agent_a` to `other` and runs `meanwhile`.
    fn racing_git(base: &Path, checkout: &Path, other: &str, meanwhile: &str) -> String {
        let script = base.join("racing-git");
        let git_dir = checkout.join(".git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"spawn: merge main\"*)\n    git --git-dir='{}' \
                 update-ref refs/heads/agent_a {other}\n    {meanwhile}\n    ;;\nesac\nexec git \
                 \"$@\"\n",
                git_dir.display()
            ),
        )
        .expect("write the racing git");
        let status = Command::new("chmod")
            .arg("+x")
            .arg(&script)
            .status()
            .expect("chmod");
        assert!(status.success());
        script.to_string_lossy().to_string()
    }

    #[test]
    fn a_merge_that_loses_the_branch_race_leaves_the_workspace_at_the_branch_head() {
        let (checkout, store, workspace, base) = due_merge("race");
        let directory = PathBuf::from(workspace.path());
        let other = other_writers_commit(&checkout);
        let racing = GitWorkspace {
            git: racing_git(&base, &checkout, &other, ":"),
            ..store.clone()
        };
        let lost = racing.merge_base(&workspace);
        assert!(
            matches!(&lost, Err(WorkspaceError::Unavailable(text)) if text.contains("follows its head")),
            "{lost:?}"
        );
        assert_eq!(git(&checkout, &["rev-parse", "refs/heads/agent_a"]), other);
        // The directory is the other writer's head, not the lost merge.
        assert!(!directory.join("new.txt").exists(), "the merge went back");
        assert_eq!(
            std::fs::read_to_string(directory.join("other.txt")).expect("theirs"),
            "the other writer\n"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("work.txt")).expect("mine"),
            "mine\n"
        );
        // So the next checkpoint has nothing to commit over it, and the
        // next merge merges into the head that won.
        assert_eq!(
            store.commit(&workspace, "after").expect("commit"),
            Commit::Unchanged
        );
        let BaseMerge::Merged { commit, .. } = store.merge_base(&workspace).expect("merge") else {
            panic!("main is still due");
        };
        let parents = git(
            &checkout,
            &["rev-list", "--parents", "-n1", commit.as_str()],
        );
        assert!(parents.contains(&other), "{parents}");
        assert!(directory.join("new.txt").exists());
    }

    #[test]
    fn a_lost_race_that_cannot_be_undone_refuses_checkpoints_until_a_mount() {
        let (checkout, store, workspace, base) = due_merge("race-stale");
        let directory = PathBuf::from(workspace.path());
        let other = other_writers_commit(&checkout);
        // A change the merge did not make, in a file the merge wrote.
        let meanwhile = format!(
            "printf 'edited\\n' > '{}'",
            directory.join("new.txt").display()
        );
        let racing = GitWorkspace {
            git: racing_git(&base, &checkout, &other, &meanwhile),
            ..store.clone()
        };
        let lost = racing.merge_base(&workspace);
        assert!(
            matches!(&lost, Err(WorkspaceError::Unavailable(text)) if text.contains("cannot follow")),
            "{lost:?}"
        );
        let refused = |result: Result<_, WorkspaceError>| matches!(result, Err(WorkspaceError::Refused { code, .. }) if code == "WorkspaceStale");
        assert!(refused(store.commit(&workspace, "over").map(|_| ())));
        assert!(refused(store.merge_base(&workspace).map(|_| ())));
        assert_eq!(git(&checkout, &["rev-parse", "refs/heads/agent_a"]), other);

        let branch = BranchName::new("agent_a").expect("valid branch");
        let workspace = store.mount(&branch).expect("mount again");
        assert_eq!(
            store.commit(&workspace, "fresh").expect("commit"),
            Commit::Unchanged
        );
    }

    /// Deterministic incompressible bytes (xorshift), just over the large
    /// file limit.
    fn large_noise(seed: u64) -> Vec<u8> {
        let length = usize::try_from(super::LARGE_FILE_BYTES).expect("fits") + 4096;
        let mut state = seed | 1;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    /// A mounted workspace of `agent_a` with a checkpoint of a large
    /// `model.bin` holding `large_noise(1)`.
    fn large_checkpoint(tag: &str) -> (GitWorkspace, WorkspaceId, PathBuf) {
        let base = std::env::temp_dir().join(format!("spawn-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let checkout = base.join("repo");
        std::fs::create_dir_all(&checkout).expect("create the repository");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(checkout.join("README.md"), "base\n").expect("write");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);
        let store = GitWorkspace::new(checkout.join(".git"), base.join("work"));
        let branch = BranchName::new("agent_a").expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        let model = PathBuf::from(workspace.path()).join("model.bin");
        std::fs::write(&model, large_noise(1)).expect("write the large file");
        assert!(matches!(
            store.commit(&workspace, "large").expect("commit"),
            Commit::Created(_)
        ));
        (store, workspace, model)
    }

    /// The chunks the branch head's manifest lists for `model.bin`.
    fn committed_chunks(store: &GitWorkspace) -> Vec<String> {
        let name = store
            .manifest_of("refs/heads/agent_a")
            .expect("read the head")
            .expect("a manifest");
        let manifest = store.manifest(&name).expect("the manifest");
        manifest.files[0].chunks.clone()
    }

    /// Make every stat-cache entry of `agent_a` old: cached well after its
    /// file last changed, so its stat is trusted without a hash.
    fn age_cache(store: &GitWorkspace) {
        let mut cache = store.read_cache("agent_a");
        assert!(!cache.is_empty(), "a cache to age");
        for known in cache.values_mut() {
            known.cached_at_ns =
                known.stat.mtime_ns.max(known.stat.ctime_ns) + 2 * super::RACY_WINDOW_NS;
        }
        store.write_cache("agent_a", &cache);
    }

    #[test]
    fn a_same_size_rewrite_with_its_mtime_restored_is_committed() {
        let (store, workspace, model) = large_checkpoint("cache-mtime");
        age_cache(&store);
        let modified = std::fs::metadata(&model)
            .and_then(|metadata| metadata.modified())
            .expect("mtime");
        // Same size, same mode, the old modification time put back.
        std::fs::write(&model, large_noise(2)).expect("rewrite");
        std::fs::File::options()
            .write(true)
            .open(&model)
            .and_then(|file| file.set_modified(modified))
            .expect("restore the mtime");
        assert!(matches!(
            store.commit(&workspace, "rewrite").expect("commit"),
            Commit::Created(_)
        ));
        let (expected, _) = crate::chunks::chunk_names_of(&model).expect("hash");
        assert_eq!(committed_chunks(&store), expected, "the new content");
    }

    #[test]
    fn a_racily_clean_cache_entry_is_hashed_again() {
        let (store, workspace, model) = large_checkpoint("cache-racy");
        let old = committed_chunks(&store);
        std::fs::write(&model, large_noise(3)).expect("rewrite");
        // As if the last stat had been taken just after this rewrite, in
        // the same timestamp tick: the stat matches, the chunks are old.
        let forge = |cached_at_ns: Option<i128>| {
            let mut cache = store.read_cache("agent_a");
            let known = cache.values_mut().next().expect("an entry");
            known.stat = super::stat_of(&model).expect("stat");
            known.chunks.clone_from(&old);
            known.cached_at_ns = cached_at_ns.unwrap_or_else(super::now_ns);
            store.write_cache("agent_a", &cache);
        };
        forge(None);
        assert!(matches!(
            store.commit(&workspace, "rewrite").expect("commit"),
            Commit::Created(_)
        ));
        let (expected, _) = crate::chunks::chunk_names_of(&model).expect("hash");
        assert_ne!(expected, old);
        assert_eq!(committed_chunks(&store), expected, "hashed again");

        // The same entry cached long after the file changed is trusted: the
        // window is what found the rewrite.
        let stat = super::stat_of(&model).expect("stat");
        forge(Some(
            stat.mtime_ns.max(stat.ctime_ns) + 2 * super::RACY_WINDOW_NS,
        ));
        store.commit(&workspace, "trusted").expect("commit");
        assert_eq!(committed_chunks(&store), old);
    }
}

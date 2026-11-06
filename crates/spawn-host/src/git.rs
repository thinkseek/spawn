//! git-backed workspace store.
//!
//! One bare repository holds every base commit. A mount exports a branch with
//! `git archive` into a plain directory, so the engine never sees `.git` and
//! cannot read the history, another branch, or the controller's refs.
//!
//! A commit is written with plumbing against a temporary index, so it never
//! touches a checked-out tree and needs no branch to be current.
//!
//! Every file of the workspace is a blob of the checkpoint, up to a size cap
//! (`SPAWN_MAX_FILE_MB`, [`DEFAULT_MAX_FILE_MB`]). A file over it is left out
//! of the checkpoint, never failing it: the checkpoint goes on without it,
//! and the agent is told once, by the prompt that carries it
//! ([`WorkspaceStore::notices`], [`WorkspaceStore::notices_told`]). The one
//! exception: a file git already holds as a blob in the branch head or in
//! `main` (it came from the source) stays a blob, so a checkpoint never drops
//! it from the tree. The branch head alone is the whole workspace
//! (docs/storage.md).
//!
//! The files the engine writes for itself in its working directory
//! ([`ENGINE_RUNTIME_FILES`]: Claude Code's scheduler lock, worktrees and
//! the rest of its own list) never enter a checkpoint, whatever the
//! project's `.gitignore` says: they are excluded by pathspec at every
//! staging, so a workspace mounted before this rule is covered at its next
//! checkpoint. The rest of `.claude/` is the project's and is committed. A
//! runtime file the project tracks (both the branch and `main` hold it) is
//! committed like any tracked file; Spawn never removes it from `main`.
//!
//! Identifiers travel as `<branch>:<absolute directory>`. The branch is the
//! part before the colon, so every call rebuilds the mapping from the
//! identifier alone.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use spawn_core::event::Fork;
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{
    BaseMerge, Branch, Commit, ForkFrom, Hold, MERGE_WAITS, SetAside, WORKSPACE_STALE,
    WorkspaceError, WorkspaceStore,
};

use crate::tree_path::{TreePath, nul_fields};

/// Distinguishes the staged mounts of one process ([`GitWorkspace::staging_path`]).
static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Ref a new branch is forked from.
const BASE: &str = "main";

/// Paths that never enter a commit.
const BUILD_OUTPUT: [&str; 2] = ["target", ".spawn"];

/// Files the engine writes for itself in its working directory, at any
/// depth: its runtime state (a session's scheduler lock, its worktrees, its
/// checkpoints, its mailbox), never the project's. No checkpoint adds one,
/// whatever the project's `.gitignore` says; the rest of `.claude/` (the
/// settings, agents and commands a project commits) is committed as usual
/// ([`GitWorkspace::track_runtime_files`], docs/storage.md).
///
/// An entry that ends in `/` is a directory; any other is a file, or a
/// directory, and everything under it. Claude Code is the only engine
/// (`spawn_core::event::ENGINE`), and the list is its own: the paths it
/// writes to a checkout's `.git/info/exclude` under `# claude-code-runtime`
/// (Claude Code 2.1.318), plus `.claude/settings.local.json`, the local
/// permissions it writes when a person allows a tool, which a project never
/// commits. An exported workspace has no `.git`, so Claude Code writes none
/// there.
pub const ENGINE_RUNTIME_FILES: [&str; 11] = [
    ".claude/scheduled_tasks.lock",
    ".claude/settings.local.json",
    ".claude/scheduled_tasks.json",
    ".claude/routines/.state/",
    ".claude/worktrees/",
    ".claude/checkpoints/",
    ".claude/mailbox/",
    ".claude/agent-registry.json",
    ".claude/agent-memory-local",
    ".claude/first-run",
    ".claude/assistant-daemon-state.json",
];

/// Pathspecs that match the engine's runtime files
/// ([`ENGINE_RUNTIME_FILES`]) at any depth, each with `glob` magic and
/// `magic` before it (`exclude`, or nothing). A glob pathspec never matches
/// what is under a directory it names, so a directory gets `<path>/**`.
fn runtime_pathspecs(magic: &str) -> Vec<OsString> {
    let magic = if magic.is_empty() {
        "glob".to_string()
    } else {
        format!("{magic},glob")
    };
    ENGINE_RUNTIME_FILES
        .iter()
        .flat_map(|entry| {
            let (path, directory) = match entry.strip_suffix('/') {
                Some(path) => (path, true),
                None => (*entry, false),
            };
            let under = format!(":({magic})**/{path}/**");
            if directory {
                vec![under]
            } else {
                vec![format!(":({magic})**/{path}"), under]
            }
        })
        .map(OsString::from)
        .collect()
}

/// Where main's version of each file that conflicts with the branch in a
/// way markers cannot show is copied, relative to the workspace (under
/// `.spawn/`, never committed), for a worker that cannot run git. Every
/// mount puts the copies back while the conflict is open
/// ([`GitWorkspace::restore_main_copies`]).
pub const MAIN_COPIES: &str = ".spawn/main";

/// First field of the record of main's copies ([`GitWorkspace::main_copies`]).
const COPIES_TAG: &str = "main-copies";

/// The size cap of one file in a checkpoint, in MiB, when
/// `SPAWN_MAX_FILE_MB` does not set one: git stores every version of a file
/// whole, so a file over it would cost its full size in the repository at
/// every checkpoint that changed it (docs/storage.md).
pub const DEFAULT_MAX_FILE_MB: u64 = 100;

/// The size cap of one file in a checkpoint, in bytes: `SPAWN_MAX_FILE_MB`
/// (`value`) when it is a positive number of MiB, else
/// [`DEFAULT_MAX_FILE_MB`].
pub fn max_file_bytes(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|megabytes| *megabytes > 0)
        .unwrap_or(DEFAULT_MAX_FILE_MB)
        .saturating_mul(1 << 20)
}

/// The trailer by which a checkpoint of an earlier version named a manifest
/// of large files it kept outside git, in a chunk store this version does
/// not have.
pub const LARGE_FILE_TRAILER: &str = "Spawn-Large";

/// Whether a commit message names large files kept outside git: a
/// `Spawn-Large: <hash>` line in its last paragraph ([`LARGE_FILE_TRAILER`]).
/// Such a commit's tree is not the whole workspace, so a mount of it is
/// refused and a landing of it drops the trailer.
pub fn names_files_outside_git(message: &str) -> bool {
    let Some(paragraph) = message.trim_end().rsplit("\n\n").next() else {
        return false;
    };
    paragraph.lines().any(|line| {
        line.strip_prefix(LARGE_FILE_TRAILER)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(str::trim)
            .is_some_and(|name| {
                name.len() == 64
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    })
}

/// The locale every git Spawn runs gets (`LC_ALL`): git's own, untranslated
/// messages.
pub const GIT_LOCALE: &str = "C";

/// A command that runs `program` (the git to run) the way Spawn runs every
/// git: in the C locale, [`GIT_LOCALE`] as `LC_ALL` and no `LANGUAGE`.
/// Every git of [`GitWorkspace`] and of the controller (`spawn-cli`'s
/// `controller_git`) is built here, so none can be left to the host's
/// locale.
///
/// Spawn reads what git answers: a push's summary, a merge's conflict
/// kinds, a listing, an exit code, and the error it quotes in a record or a
/// note. Under a translated locale (`LANG=zh_CN.UTF-8`) git, and the
/// `git-receive-pack` or hook it starts, writes its messages in that
/// language; nothing Spawn decides may change with the host's language.
/// `LC_ALL` wins over `LANG` and every `LC_*`; `LANGUAGE`, which gettext
/// would otherwise read first, is removed. Paths, listings and commit
/// messages are bytes to git in any locale.
pub fn git_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("LC_ALL", GIT_LOCALE).env_remove("LANGUAGE");
    command
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
    /// The size cap of one file in a checkpoint ([`max_file_bytes`]).
    max_file_bytes: u64,
}

impl GitWorkspace {
    /// Store that keeps working trees on this machine, under `root`
    /// resolved through its symlinks once, here ([`crate::root`]): every
    /// workspace id it mints names the directory as the engine's process
    /// sees it.
    pub fn new(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Self {
        Self {
            repo: repo.as_ref().to_path_buf(),
            root: crate::root::canonical_root(root.as_ref()),
            git: "git".to_string(),
            max_file_bytes: max_file_bytes(std::env::var("SPAWN_MAX_FILE_MB").ok().as_deref()),
        }
    }

    /// The same store with a size cap of `bytes` for one file in a
    /// checkpoint instead of the one `SPAWN_MAX_FILE_MB` sets.
    #[must_use]
    pub fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// Every git this store runs: in the C locale ([`git_command`]), on its
    /// repository.
    fn command(&self) -> Command {
        let mut command = git_command(&self.git);
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
    /// An id recorded under another spelling of the root (before the root
    /// was resolved through its symlinks) names the same directory under the
    /// resolved root ([`crate::root::legacy_workspace`]).
    fn checked_path(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let directory = PathBuf::from(workspace.path());
        if directory.starts_with(&self.root) {
            return Ok(directory);
        }
        if let Some(directory) =
            crate::root::legacy_workspace(&self.root, workspace.sandbox(), &directory)
        {
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
        let mut with_input: Vec<&OsStr> = arguments.iter().map(AsRef::as_ref).collect();
        with_input.extend(["--pathspec-from-file=-", "--pathspec-file-nul"].map(OsStr::new));
        self.listing_in_with_input(directory, index, &with_input, &nul_terminated(specs))
    }

    /// [`Self::listing_in`] for a command that reads `input` on standard
    /// input.
    fn listing_in_with_input<S: AsRef<OsStr>>(
        &self,
        directory: &Path,
        index: &Path,
        arguments: &[S],
        input: &[u8],
    ) -> Result<Vec<u8>, WorkspaceError> {
        let word = shown_arguments(arguments);
        let mut child = self
            .command_in(directory, index)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| WorkspaceError::Unavailable(format!("run {word}: {error}")))?;
        let stdin = child.stdin.take();
        // Written beside the read: a long answer must not block the input.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                use std::io::Write;
                stdin.map(|mut stdin| stdin.write_all(input))
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

    /// Tree of the work tree in `directory` for a checkpoint of `branch`
    /// whose parent is `head`, built against `index`, and the files over
    /// the size cap ([`max_file_bytes`]) left out of it, with their sizes.
    ///
    /// Build output and files over the cap leave the index after `add -A`,
    /// so a `.gitignore` inside the tree cannot re-include them; a file over
    /// the cap is excluded before it is hashed. The engine's runtime files
    /// are never added, and the ones the project tracks are taken as any
    /// tracked file is ([`Self::track_runtime_files`]).
    ///
    /// A file over the cap that git already holds as a blob in `main` or a
    /// pending merge's `main` ([`Self::source_commits`]) stays a blob: it
    /// came from the source (main may get one from outside Spawn), and
    /// leaving it out of the tree would delete it from `main` when the
    /// branch lands.
    fn tree_of(
        &self,
        directory: &Path,
        index: &Path,
        branch: &str,
        head: &str,
    ) -> Result<(String, Vec<(TreePath, u64)>), WorkspaceError> {
        clear_dead_lock(index);
        let pending_main = self.pending_merge(branch).map(|pending| pending.main);
        let kept_in = self.source_commits(pending_main.as_deref());
        let (mut oversized, special) = self.left_out_files(directory, index)?;
        let blobs = self.blobs_in(&kept_in, &oversized)?;
        oversized.retain(|(path, _)| !blobs.contains(path));
        // A special file (a FIFO, a socket, a device) is never committed:
        // `git add` refuses one at a tracked path, which would fail every
        // checkpoint, so it is left out too, silently: it holds no content.
        let mut left_out = oversized.clone();
        left_out.extend(special);
        self.stage_all_but(directory, index, &left_out)?;
        self.track_runtime_files(directory, index, head, pending_main.as_deref())?;
        self.unstage(directory, index, &left_out)?;
        let tree = self.output_in(directory, index, &["write-tree"])?;
        // A file that grew past the cap between the size check and `add`
        // was hashed anyway; it is left out all the same.
        let mut grown = self.oversized_blobs(&tree)?;
        let blobs = self.blobs_in(&kept_in, &grown)?;
        grown.retain(|(path, _)| !blobs.contains(path));
        if grown.is_empty() {
            return Ok((tree, oversized));
        }
        self.unstage(directory, index, &grown)?;
        oversized.extend(grown);
        oversized.sort();
        oversized.dedup_by(|later, earlier| later.0 == earlier.0);
        let tree = self.output_in(directory, index, &["write-tree"])?;
        Ok((tree, oversized))
    }

    /// `git add -A` of the work tree in `directory` except `left_out` and
    /// the engine's runtime files ([`ENGINE_RUNTIME_FILES`]), which are
    /// never hashed: an exclude pathspec, so no ignore rule of the project
    /// (a `.gitignore` that re-includes one) brings one in. The paths go to
    /// git on standard input, so any number of them fits.
    fn stage_all_but(
        &self,
        directory: &Path,
        index: &Path,
        left_out: &[(TreePath, u64)],
    ) -> Result<(), WorkspaceError> {
        let mut add = vec![OsString::from(".")];
        add.extend(
            left_out
                .iter()
                .map(|(path, _)| path.pathspec("exclude,literal")),
        );
        add.extend(runtime_pathspecs("exclude"));
        self.listing_in_with_pathspecs(directory, index, &["add", "-A"], &add)
            .map(|_| ())
    }

    /// Make the index's entries for the engine's runtime files
    /// ([`ENGINE_RUNTIME_FILES`]) exactly the ones the project tracks, each
    /// as the work tree in `directory` holds it now.
    ///
    /// A runtime file is tracked when the branch holds it and so does
    /// `main`: the project's own, committed on purpose or by mistake. The
    /// branch is `head` (the checkpoint's parent), and, while a merge of
    /// `main` waits on its markers (`pending_main`), `head` with what that
    /// `main` changed since the merge base: the merge wrote those changes
    /// into the directory. A tracked one is committed like any tracked
    /// file, git's rule for a tracked file an ignore rule names: the
    /// engine's edits to it, and its deletion. Spawn never takes one out
    /// of `main` (the owner removes it). One only the branch holds (a
    /// checkpoint from before this rule committed it), or one `main` no
    /// longer holds (the owner removed it), leaves the branch: either way
    /// the landing changes nothing in `main` for it, and does not conflict
    /// on it. One the branch does not hold never enters the index
    /// ([`Self::stage_all_but`]).
    ///
    /// The kept index is only a stat cache (a fresh one after a failure,
    /// one that saw no merge of main, one written before this rule), so
    /// its runtime entries are first reset from `head`, never trusted.
    /// When neither `head` nor the index holds a runtime file and no merge
    /// waits (nearly every checkpoint), one listing says so and nothing
    /// else runs.
    fn track_runtime_files(
        &self,
        directory: &Path,
        index: &Path,
        head: &str,
        pending_main: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        let specs = runtime_pathspecs("");
        if pending_main.is_none() {
            // `--with-tree`: the index's entries and `head`'s together.
            let mut probe: Vec<OsString> =
                ["ls-files", "-z", "--cached"].map(OsString::from).into();
            probe.push(OsString::from(format!("--with-tree={head}")));
            probe.push(OsString::from("--"));
            probe.extend(specs.iter().cloned());
            if self.listing_in(directory, index, &probe)?.is_empty() {
                return Ok(());
            }
        }
        let mut reset: Vec<OsString> = ["reset", "-q", "--no-refresh", head, "--"]
            .map(OsString::from)
            .into();
        reset.extend(specs.iter().cloned());
        self.listing_in(directory, index, &reset)?;
        if let Some(main) = pending_main
            && let Ok(base) = self.git_output(&["merge-base", head, main])
        {
            let mut changed: Vec<OsString> = [
                "diff-tree",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                base.as_str(),
                main,
                "--",
            ]
            .map(OsString::from)
            .into();
            changed.extend(specs.iter().cloned());
            let changed = listed_paths(&self.git_listing(&changed)?);
            if !changed.is_empty() {
                let literal: Vec<OsString> = changed
                    .iter()
                    .map(|path| path.pathspec("literal"))
                    .collect();
                self.listing_in_with_pathspecs(
                    directory,
                    index,
                    &["reset", "-q", "--no-refresh", main],
                    &literal,
                )?;
            }
        }
        let mut listing: Vec<OsString> = ["ls-files", "-z", "--cached", "--"]
            .map(OsString::from)
            .into();
        listing.extend(specs);
        let entries: Vec<(TreePath, u64)> =
            listed_paths(&self.listing_in(directory, index, &listing)?)
                .into_iter()
                .map(|path| (path, 0))
                .collect();
        if entries.is_empty() {
            return Ok(());
        }
        let in_main = match self.commit_id_of(&format!("refs/heads/{BASE}")) {
            Ok(main) => self.blobs_in(&[main], &entries)?,
            Err(_) => entries.iter().map(|(path, _)| path.clone()).collect(),
        };
        // git adds nothing beyond a symbolic link and refuses a directory or
        // a special file: what is not a file there now is gone from the
        // index, as `add -A` would have it.
        let (present, gone): (Vec<TreePath>, Vec<TreePath>) = entries
            .into_iter()
            .map(|(path, _)| path)
            .partition(|path| in_main.contains(path) && is_file_in(directory, path));
        for (flag, paths) in [("--add", present), ("--force-remove", gone)] {
            if paths.is_empty() {
                continue;
            }
            let names: Vec<OsString> = paths
                .iter()
                .map(|path| path.as_path().as_os_str().to_os_string())
                .collect();
            self.listing_in_with_input(
                directory,
                index,
                &["update-index", flag, "-z", "--stdin"],
                &nul_terminated(&names),
            )?;
        }
        Ok(())
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
        let batches = pathspec_batches(
            paths
                .iter()
                .map(|(path, _)| path.pathspec("literal"))
                .collect(),
        );
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

    /// The commits whose blobs over the cap a checkpoint keeps as blobs
    /// ([`Self::tree_of`]): `main`, and `pending_main`, the `main` of the
    /// branch's pending merge. Not the branch head: a file the agent
    /// committed under the cap and grew past it is left out like any other.
    fn source_commits(&self, pending_main: Option<&str>) -> Vec<String> {
        let mut commits = Vec::new();
        if let Ok(main) = self.commit_id_of(&format!("refs/heads/{BASE}")) {
            commits.push(main);
        }
        if let Some(pending_main) = pending_main {
            commits.push(pending_main.to_string());
        }
        commits.sort();
        commits.dedup();
        commits
    }

    /// Blobs of `tree` over the size cap, with their sizes.
    fn oversized_blobs(&self, tree: &str) -> Result<Vec<(TreePath, u64)>, WorkspaceError> {
        let listed = self.git_listing(&["ls-tree", "-r", "-l", "-z", tree])?;
        Ok(nul_fields(&listed)
            .filter_map(|entry| {
                // `<mode> blob <id> <size>\t<path>`
                let (meta, path) = tree_entry(entry)?;
                let mut fields = meta.split_whitespace();
                let kind = fields.nth(1)?;
                let size: u64 = fields.nth(1)?.parse().ok()?;
                (kind == "blob" && size > self.max_file_bytes).then_some((path, size))
            })
            .collect())
    }

    /// Paths git would commit (tracked or untracked, ignore rules applied,
    /// the engine's runtime files aside) that a checkpoint leaves out: the
    /// files over the size cap, with their sizes; then the paths that hold
    /// something other than a regular file, a symlink or a directory (a
    /// FIFO, a socket, a device), which git cannot add.
    fn left_out_files(&self, directory: &Path, index: &Path) -> Result<LeftOut, WorkspaceError> {
        let mut arguments: Vec<OsString> = [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ".",
        ]
        .map(OsString::from)
        .into();
        arguments.extend(runtime_pathspecs("exclude"));
        let listed = self.listing_in(directory, index, &arguments)?;
        let mut oversized = Vec::new();
        let mut special = Vec::new();
        for path in listed_paths(&listed) {
            let Ok(metadata) = std::fs::symlink_metadata(directory.join(path.as_path())) else {
                continue;
            };
            let kind = metadata.file_type();
            if kind.is_file() {
                if metadata.len() > self.max_file_bytes {
                    oversized.push((path, metadata.len()));
                }
            } else if !kind.is_symlink() && !kind.is_dir() {
                special.push((path, 0));
            }
        }
        oversized.sort();
        oversized.dedup();
        special.sort();
        special.dedup();
        Ok((oversized, special))
    }

    /// Where the files over the cap that checkpoints of a branch left out are
    /// recorded, NUL-separated, three fields each: `told` or `untold`, the
    /// size in bytes, the path.
    fn oversized_path(&self, branch: &str) -> PathBuf {
        self.root.join(".oversized").join(branch)
    }

    /// The record at [`Self::oversized_path`]: by path, the size and whether
    /// the agent was told. An unreadable or malformed record is empty.
    fn read_oversized(&self, branch: &str) -> BTreeMap<TreePath, (u64, bool)> {
        let Ok(bytes) = std::fs::read(self.oversized_path(branch)) else {
            return BTreeMap::new();
        };
        let fields: Vec<&[u8]> = nul_fields(&bytes).collect();
        fields
            .chunks_exact(3)
            .filter_map(|entry| {
                let told = match entry[0] {
                    b"told" => true,
                    b"untold" => false,
                    _ => return None,
                };
                let size = std::str::from_utf8(entry[1]).ok()?.parse().ok()?;
                Some((TreePath::from_bytes(entry[2]), (size, told)))
            })
            .collect()
    }

    /// Best effort: a lost record tells the agent again at worst.
    fn write_oversized(&self, branch: &str, record: &BTreeMap<TreePath, (u64, bool)>) {
        let path = self.oversized_path(branch);
        if record.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let mut bytes = Vec::new();
        for (file, (size, told)) in record {
            bytes.extend_from_slice(if *told { b"told" } else { b"untold" });
            bytes.push(0);
            bytes.extend_from_slice(size.to_string().as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(file.as_bytes());
            bytes.push(0);
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temporary = path.with_extension("tmp");
        if std::fs::write(&temporary, bytes).is_ok() {
            let _ = std::fs::rename(&temporary, &path);
        }
    }

    /// Record the files over the cap a checkpoint of `branch` just left out:
    /// one already told of stays told; one no longer left out is forgotten,
    /// so it is told of again if it grows past the cap again. The caller
    /// holds the branch's lock.
    fn record_oversized(&self, branch: &str, oversized: &[(TreePath, u64)]) {
        let before = self.read_oversized(branch);
        let record: BTreeMap<TreePath, (u64, bool)> = oversized
            .iter()
            .map(|(path, size)| {
                let told = before.get(path).is_some_and(|(_, told)| *told);
                (path.clone(), (*size, told))
            })
            .collect();
        if record != before {
            self.write_oversized(branch, &record);
        }
    }

    /// The notice of one file over the cap.
    fn oversized_notice(&self, path: &TreePath, size: u64) -> String {
        format!(
            "{path} ({} MiB) is over the {} MiB cap for one file in a checkpoint \
             (SPAWN_MAX_FILE_MB), so checkpoints leave it out: it is not on your branch, \
             and it is lost when this workspace is unmounted. Make it smaller, split it, \
             or make sure it can be generated again from what is committed.",
            size.div_ceil(1 << 20),
            self.max_file_bytes >> 20
        )
    }

    /// One notice for each file over the cap that a checkpoint of `branch`
    /// left out and the agent was not told of yet. Nothing is marked: the
    /// prompt that carries them does ([`Self::oversized_told`]).
    fn untold_oversized(&self, branch: &str) -> Vec<String> {
        self.read_oversized(branch)
            .iter()
            .filter(|(_, (_, told))| !*told)
            .map(|(path, (size, _))| self.oversized_notice(path, *size))
            .collect()
    }

    /// Mark told each file over the cap whose notice is in `notices`: a
    /// prompt carried it. A file whose size changed since has another
    /// notice, and stays untold.
    fn oversized_told(&self, branch: &str, notices: &[String]) {
        let Ok(_lock) = self.lock_branch(branch) else {
            return;
        };
        let mut record = self.read_oversized(branch);
        let mut changed = false;
        for (path, (size, told)) in &mut record {
            if !*told && notices.contains(&self.oversized_notice(path, *size)) {
                *told = true;
                changed = true;
            }
        }
        if changed {
            self.write_oversized(branch, &record);
        }
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
    /// before the workspace is touched; its path. On any error the directory
    /// is removed.
    fn stage_mount(&self, branch: &str) -> Result<PathBuf, WorkspaceError> {
        let staged = self.staging_path(branch, "staged")?;
        let built = std::fs::create_dir(&staged)
            .map_err(|error| {
                WorkspaceError::Unavailable(format!("create {}: {error}", staged.display()))
            })
            .and_then(|()| self.export(branch, &staged));
        match built {
            Ok(()) => Ok(staged),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staged);
                Err(error)
            }
        }
    }

    /// Refuse a branch whose head names large files kept outside git by an
    /// earlier version ([`names_files_outside_git`]): its tree is not the
    /// whole workspace, and this version cannot bring the rest back.
    fn refuse_files_outside_git(&self, branch: &str) -> Result<(), WorkspaceError> {
        let message = self.git_output(&[
            "show",
            "-s",
            "--format=%B",
            &format!("refs/heads/{branch}"),
            "--",
        ])?;
        if !names_files_outside_git(&message) {
            return Ok(());
        }
        Err(WorkspaceError::Refused {
            code: "FilesOutsideGit".to_string(),
            message: format!(
                "the head of {branch} names large files an earlier version kept outside git \
                 (a {LARGE_FILE_TRAILER} trailer, in the chunk store this version no longer \
                 has), so its tree is not the whole workspace; start from another commit, or \
                 move the branch to a commit without the trailer"
            ),
        })
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

    /// An id under the resolved root is this store's own; one recorded
    /// under another spelling of it names the same directory there.
    fn canonical(&self, workspace: &WorkspaceId) -> WorkspaceId {
        let directory = Path::new(workspace.path());
        if directory.starts_with(&self.root) {
            return workspace.clone();
        }
        crate::root::legacy_workspace(&self.root, workspace.sandbox(), directory)
            .and_then(|directory| {
                WorkspaceId::new(format!("{}:{}", workspace.sandbox(), directory.display())).ok()
            })
            .unwrap_or_else(|| workspace.clone())
    }

    /// Export the branch head as the workspace. The whole new tree is built
    /// beside the workspace first ([`GitWorkspace::stage_mount`]): a mount
    /// that fails (no commit, a head that names files outside git) leaves the
    /// existing workspace, uncommitted files and all, as it was. Only then is
    /// the workspace replaced, by renames.
    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.exclude_build_output()?;
        self.git_output(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .map_err(|_| WorkspaceError::Unavailable(format!("branch {branch} has no commit")))?;
        self.refuse_files_outside_git(branch.as_str())?;
        let staged = self.stage_mount(branch.as_str())?;

        let directory = self.root.join(branch.as_str());
        // A stale directory holds work its branch does not: kept before it
        // is replaced, or the mount fails and the directory stays.
        if let Err(error) = self.keep_stale_directory(branch.as_str(), &directory) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error);
        }
        // A pending merge stays only when the branch carries its markers.
        self.settle_pending(branch.as_str());
        if let Err(error) = self.replace_workspace(branch.as_str(), &staged, &directory) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error);
        }
        // A fresh export is the branch head: nothing stale is left.
        self.clear_stale(branch.as_str());
        // `.spawn/` is never committed: main's copies for an open conflict
        // markers cannot show come back from their record.
        self.restore_main_copies(branch.as_str(), &directory);

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
        if self.is_stale(&branch) {
            let kept = self.keep_stale(&branch, &directory)?;
            return Err(stale_refusal(&branch, Some(&kept)));
        }
        let reference = format!("refs/heads/{branch}");
        let parent = self.git_output(&["rev-parse", &reference])?;
        let tree = self.tree_of(&directory, &index, &branch, &parent);
        if tree.is_err() {
            let _ = std::fs::remove_file(&index);
        }
        let (tree, oversized) = tree?;
        self.record_oversized(&branch, &oversized);

        let parent_tree = self.git_output(&["rev-parse", &format!("{parent}^{{tree}}")])?;
        // A merge of main whose conflicts are now resolved: main is the
        // second parent, even when the tree did not change.
        let merged = self.resolved_merge(&branch, &directory);
        if tree == parent_tree && merged.is_none() {
            return Ok(Commit::Unchanged);
        }
        let mut arguments = vec!["commit-tree", tree.as_str(), "-p", parent.as_str()];
        if let Some(main) = merged.as_deref() {
            arguments.extend(["-p", main]);
        }
        arguments.extend(["-m", message]);
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
        // Not even a discard removes a stale directory before it is kept.
        self.keep_stale_directory(workspace.sandbox(), &directory)?;
        // A pending merge stays only when the branch carries its markers.
        self.settle_pending(workspace.sandbox());
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
        self.merge_main(workspace, &|| Ok(()))
    }

    fn merge_base_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<BaseMerge, WorkspaceError> {
        self.merge_main(workspace, ready)
    }

    /// The branch's lock, which every merge into the workspace takes.
    fn hold(&self, workspace: &WorkspaceId) -> Option<Hold> {
        self.lock_branch(workspace.sandbox())
            .ok()
            .map(|lock| Box::new(lock) as Hold)
    }

    fn merge_main_clean(&self, branch: &BranchName) -> Result<BaseMerge, WorkspaceError> {
        self.merge_main_into_branch(branch.as_str())
    }

    fn set_aside(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_since(branch.as_str(), start, message)
    }

    /// What checkpoints of the workspace left out and the agent was not
    /// told of yet ([`GitWorkspace::record_oversized`]).
    fn notices(&self, workspace: &WorkspaceId) -> Vec<String> {
        self.untold_oversized(workspace.sandbox())
    }

    fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]) {
        self.oversized_told(workspace.sandbox(), notices);
    }

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        let head = self.git_output(&[
            "for-each-ref",
            "--format=%(objectname)",
            &format!("refs/heads/{branch}"),
        ])?;
        if head.is_empty() {
            return Ok(None);
        }
        CommitId::new(head)
            .map(Some)
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))
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
    /// renames, binary files) changes nothing but main's version of each
    /// such file, copied to [`MAIN_COPIES`] and recorded beside the
    /// workspace so every mount puts the copies back
    /// ([`Self::restore_main_copies`]).
    ///
    /// Nothing is written unless `ready` agrees, asked under the lock right
    /// before the first write ([`WorkspaceStore::merge_base_when`]); a
    /// refusal is [`MERGE_WAITS`] with its reason, and changes nothing.
    fn merge_main(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<BaseMerge, WorkspaceError> {
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
            self.clear_main_copies(&branch);
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
        // The last moment before anything is written: an agent that is not
        // idle keeps its workspace as it is, and the merge waits.
        if matches!(output.status.code(), Some(0 | 1)) {
            ready().map_err(|why| WorkspaceError::Refused {
                code: MERGE_WAITS.to_string(),
                message: why,
            })?;
        }
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
                    self.clear_main_copies(&branch);
                } else {
                    self.copy_mains_versions(&branch, &directory, &main, &files)?;
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
        self.update_directory(&branch, &directory, &head_tree, &merged)?;
        let changed =
            self.git_listing(&["diff-tree", "-r", "--name-only", "-z", &head_tree, &merged])?;
        let files = shown_paths(&listed_paths(&changed));
        let message = format!("spawn: merge main {main} into {branch}");
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
        self.clear_main_copies(&branch);
        Ok(BaseMerge::Merged {
            commit: CommitId::new(commit)
                .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?,
            files,
        })
    }

    /// [`WorkspaceStore::set_aside`]: a new commit on top of `branch` whose
    /// tree is `start`'s, so the work done since `start` leaves the branch's
    /// tree but stays in its history. Under the branch's lock, like a
    /// checkpoint. A mounted directory that holds what no checkpoint took is
    /// committed first (its parent is the head), so nothing it holds is
    /// lost; then the directory moves to `start`'s tree, the same two-tree
    /// way a merge of main does; only then does the branch move, by
    /// compare-and-swap. `start` must be an ancestor of the head: no foreign
    /// tree is ever written. `None` is where the branch forked from `main`.
    fn set_aside_since(
        &self,
        branch: &str,
        start: Option<&str>,
        message: &str,
    ) -> Result<SetAside, WorkspaceError> {
        let _lock = self.lock_branch(branch)?;
        self.refuse_stale(branch)?;
        let reference = format!("refs/heads/{branch}");
        let mut head = self.commit_id_of(&reference)?;
        let start = if let Some(start) = start {
            self.commit_id_of(&format!("{start}^{{commit}}"))?
        } else {
            let main = self.commit_id_of(&format!("refs/heads/{BASE}"))?;
            self.git_output(&["merge-base", &main, &head])?
        };
        if !self.is_ancestor(&start, &head)? {
            return Err(WorkspaceError::Refused {
                code: "NotAncestor".to_string(),
                message: format!("{start} is not a commit of branch {branch} (head {head})"),
            });
        }
        let directory = self.root.join(branch);
        let mounted = directory.is_dir();
        if mounted {
            // What the workspace holds that no checkpoint took is kept first.
            let index = self.root.join(".index").join(format!("{branch}.set-aside"));
            if let Some(parent) = index.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    WorkspaceError::Unavailable(format!("create {}: {error}", parent.display()))
                })?;
            }
            let _ = std::fs::remove_file(&index);
            let tree = self.tree_of(&directory, &index, branch, &head);
            let _ = std::fs::remove_file(&index);
            let (tree, _) = tree?;
            if tree != self.git_output(&["rev-parse", &format!("{head}^{{tree}}")])? {
                let kept_message = format!(
                    "spawn: the workspace of {branch} as it was, kept before its work is set aside"
                );
                let kept =
                    self.git_output(&["commit-tree", &tree, "-p", &head, "-m", &kept_message])?;
                self.git_output(&["update-ref", &reference, &kept, &head])
                    .map_err(|error| {
                        WorkspaceError::Unavailable(format!(
                            "{error} (branch moved under the set-aside)"
                        ))
                    })?;
                head = kept;
            }
        }
        let head_tree = self.git_output(&["rev-parse", &format!("{head}^{{tree}}")])?;
        let start_tree = self.git_output(&["rev-parse", &format!("{start}^{{tree}}")])?;
        if head_tree == start_tree {
            return Ok(SetAside::Unchanged);
        }
        let changed = self.git_listing(&[
            "diff-tree",
            "-r",
            "--name-only",
            "-z",
            &head_tree,
            &start_tree,
        ])?;
        let files = shown_paths(&listed_paths(&changed));
        if mounted {
            self.update_directory(branch, &directory, &head_tree, &start_tree)?;
        }
        let commit = self.git_output(&["commit-tree", &start_tree, "-p", &head, "-m", message])?;
        if let Err(error) = self.git_output(&["update-ref", &reference, &commit, &head]) {
            if mounted {
                return Err(self.follow_moved_branch(branch, &directory, &start_tree, &error));
            }
            return Err(WorkspaceError::Unavailable(format!(
                "{error} (branch moved under the set-aside)"
            )));
        }
        // A merge whose markers were in the work is set aside with it.
        self.clear_pending(branch);
        Ok(SetAside::Created {
            commit: CommitId::new(commit)
                .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?,
            kept: CommitId::new(head)
                .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?,
            files,
        })
    }

    /// Merge `main` into `branch`, which is not mounted, only when the merge
    /// is clean ([`WorkspaceStore::merge_main_clean`]): no directory is
    /// touched; the branch moves by compare-and-swap. Any conflict leaves
    /// the branch as it is.
    fn merge_main_into_branch(&self, branch: &str) -> Result<BaseMerge, WorkspaceError> {
        let _lock = self.lock_branch(branch)?;
        // A merge on top would hide what the mount refuses.
        self.refuse_files_outside_git(branch)?;
        let reference = format!("refs/heads/{branch}");
        let head = self.commit_id_of(&reference)?;
        let main = self.commit_id_of(&format!("refs/heads/{BASE}"))?;
        if self.is_ancestor(&main, &head)?
            || self.git_output(&["merge-base", &main, &head]).is_err()
        {
            return Ok(BaseMerge::Current);
        }
        // `main` first, as the integrator lands it.
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
        let (merged, files, _) = merge_tree_output(&output.stdout);
        match output.status.code() {
            Some(0) => {}
            Some(1) => {
                return Ok(BaseMerge::Conflicted {
                    files: shown_paths(&files),
                    marked: false,
                });
            }
            _ => {
                return Err(WorkspaceError::Unavailable(format!(
                    "git merge-tree {main} {head}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        let head_tree = self.git_output(&["rev-parse", &format!("{head}^{{tree}}")])?;
        let changed =
            self.git_listing(&["diff-tree", "-r", "--name-only", "-z", &head_tree, &merged])?;
        let message = format!("spawn: merge main {main} into {branch}");
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
            files: shown_paths(&listed_paths(&changed)),
        })
    }

    /// After a merge wrote tree `merged` into `directory` and its
    /// compare-and-swap lost (another writer moved the branch), bring the
    /// directory to the branch's head as it is now, the same two-tree way.
    /// Otherwise the next checkpoint would commit the merge over the other
    /// writer's commit as plain edits. When that is refused (the directory
    /// changed meanwhile in a file the two sides differ in), the directory
    /// is marked stale: its checkpoints go to `refs/spawn-stale/<branch>`,
    /// not the branch ([`Self::keep_stale`]). The answer is the error the
    /// merge returns.
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
                     its head: {error}; its checkpoints go to {} until it is mounted \
                     again{})",
                    stale_ref(branch),
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

    fn is_stale(&self, branch: &str) -> bool {
        self.stale_path(branch).exists()
    }

    /// Refuse to merge into a workspace marked stale.
    fn refuse_stale(&self, branch: &str) -> Result<(), WorkspaceError> {
        if !self.is_stale(branch) {
            return Ok(());
        }
        Err(stale_refusal(branch, None))
    }

    /// Keep the state of a stale workspace off its branch: commit the
    /// directory's tree (like a checkpoint, files over the cap left out) to
    /// [`stale_ref`], with the branch head as first parent and the snapshot
    /// before it, if any, as the second. The branch is never moved. Returns
    /// the snapshot's commit; one equal to the last snapshot is not written
    /// again. The caller holds the branch's lock.
    fn keep_stale(&self, branch: &str, directory: &Path) -> Result<String, WorkspaceError> {
        let index = self.root.join(".index").join(format!("{branch}.stale"));
        if let Some(parent) = index.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                WorkspaceError::Unavailable(format!("create {}: {error}", parent.display()))
            })?;
        }
        let head = self.commit_id_of(&format!("refs/heads/{branch}"))?;
        let reference = stale_ref(branch);
        let previous = self.commit_id_of(&reference).ok();
        let tree = self.tree_of(directory, &index, branch, &head);
        if tree.is_err() {
            let _ = std::fs::remove_file(&index);
        }
        let (tree, oversized) = tree?;
        self.record_oversized(branch, &oversized);
        if let Some(previous) = &previous
            && self.git_output(&["rev-parse", &format!("{previous}^{{tree}}")])? == tree
        {
            return Ok(previous.clone());
        }
        let message = format!(
            "spawn: stale workspace of {branch}, kept off the branch (another writer moved the \
             branch under a merge and the workspace could not follow)"
        );
        let mut arguments = vec!["commit-tree", tree.as_str(), "-p", head.as_str()];
        if let Some(previous) = previous.as_deref().filter(|previous| *previous != head) {
            arguments.extend(["-p", previous]);
        }
        arguments.extend(["-m", message.as_str()]);
        let commit = self.git_output(&arguments)?;
        let old = previous.unwrap_or_default();
        self.git_output(&["update-ref", &reference, &commit, &old])?;
        Ok(commit)
    }

    /// When `branch` is marked stale and its directory exists, keep the
    /// directory's state ([`Self::keep_stale`]) under the branch's lock.
    fn keep_stale_directory(&self, branch: &str, directory: &Path) -> Result<(), WorkspaceError> {
        if !self.is_stale(branch) || std::fs::symlink_metadata(directory).is_err() {
            return Ok(());
        }
        let _lock = self.lock_branch(branch)?;
        self.keep_stale(branch, directory).map(|_| ())
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
        clear_dead_lock(&index);
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

    /// Where the record of main's copies for `branch` lives, beside the
    /// workspaces and outside every one, so no remount wipes it:
    /// NUL-separated, [`COPIES_TAG`], `main`'s commit, then the files
    /// copied.
    fn copies_path(&self, branch: &str) -> PathBuf {
        self.root.join(".main-copies").join(branch)
    }

    /// The open conflict markers cannot show of `branch`, as recorded when
    /// main's versions were copied ([`MAIN_COPIES`]): main's commit and the
    /// files copied (those main has), shown. `None`: none is recorded.
    pub fn main_copies(&self, branch: &str) -> Option<(String, Vec<String>)> {
        self.read_main_copies(branch)
            .map(|(main, files)| (main, shown_paths(&files)))
    }

    fn read_main_copies(&self, branch: &str) -> Option<(String, Vec<TreePath>)> {
        let bytes = std::fs::read(self.copies_path(branch)).ok()?;
        let mut fields = nul_fields(&bytes);
        if fields.next()? != COPIES_TAG.as_bytes() {
            return None;
        }
        let main = fields.next()?;
        if main.is_empty() || !main.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        Some((
            String::from_utf8_lossy(main).into_owned(),
            fields.map(TreePath::from_bytes).collect(),
        ))
    }

    fn clear_main_copies(&self, branch: &str) {
        let _ = std::fs::remove_file(self.copies_path(branch));
    }

    /// Main's version of `path` at `main`, if main has it as a file.
    fn mains_version(&self, main: &str, path: &TreePath) -> Option<Vec<u8>> {
        let mut spec = OsString::from(format!("{main}:"));
        spec.push(OsStr::from_bytes(path.as_bytes()));
        let output = self
            .command()
            .arg("cat-file")
            .arg("blob")
            .arg(spec)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    }

    /// Write main's version of each of `files` that main has into
    /// [`MAIN_COPIES`] in `directory`, then record what was copied, atomically,
    /// so a mount can put the copies back ([`Self::restore_main_copies`]).
    /// A record that cannot be written fails the merge: the note would
    /// promise copies a remount loses.
    fn copy_mains_versions(
        &self,
        branch: &str,
        directory: &Path,
        main: &str,
        files: &[TreePath],
    ) -> Result<(), WorkspaceError> {
        let copied: Vec<&TreePath> = files
            .iter()
            .filter(|file| self.copy_mains_version(directory, main, file))
            .collect();
        let path = self.copies_path(branch);
        let unavailable = |what: &str, error: std::io::Error| {
            WorkspaceError::Unavailable(format!("{what} {}: {error}", path.display()))
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| unavailable("create", error))?;
        }
        let mut text = format!("{COPIES_TAG}\0{main}\0").into_bytes();
        for file in copied {
            text.extend_from_slice(file.as_bytes());
            text.push(0);
        }
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, text).map_err(|error| unavailable("write", error))?;
        std::fs::rename(&temporary, &path).map_err(|error| unavailable("rename", error))
    }

    /// Copy main's version of `file` at `main` into [`MAIN_COPIES`] in
    /// `directory` (never through a symlink the agent put there); whether it
    /// was copied.
    fn copy_mains_version(&self, directory: &Path, main: &str, file: &TreePath) -> bool {
        let Some(bytes) = self.mains_version(main, file) else {
            return false;
        };
        let mut copy = format!("{MAIN_COPIES}/").into_bytes();
        copy.extend_from_slice(file.as_bytes());
        crate::agent_files::replace_with(directory, &TreePath::from_bytes(copy), &bytes).is_ok()
    }

    /// At a mount: put main's copies of the branch's open conflict markers
    /// cannot show back into the fresh `directory`, from their record, so
    /// the note that names them still holds after the workspace was
    /// exported again (an engine lost, a host restart, a resume). A branch
    /// that contains the recorded main has no such conflict any more: the
    /// record goes.
    fn restore_main_copies(&self, branch: &str, directory: &Path) {
        let Some((main, files)) = self.read_main_copies(branch) else {
            return;
        };
        let contained = self
            .commit_id_of(&format!("refs/heads/{branch}"))
            .and_then(|head| self.is_ancestor(&main, &head));
        if matches!(contained, Ok(true)) {
            self.clear_main_copies(branch);
            return;
        }
        for file in &files {
            self.copy_mains_version(directory, &main, file);
        }
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
    /// parent of it (the next merge of main writes the merge again); or main is
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
/// Where the state of a stale workspace of `branch` is kept
/// ([`GitWorkspace::keep_stale`]): never pushed and never landed.
pub fn stale_ref(branch: &str) -> String {
    format!("refs/spawn-stale/{branch}")
}

/// The refusal of a stale workspace's checkpoint or merge, naming the
/// snapshot that keeps its state when there is one.
fn stale_refusal(branch: &str, kept: Option<&str>) -> WorkspaceError {
    let kept = kept.map_or_else(String::new, |commit| {
        format!(
            " Its state is kept as {commit} on {}, not on the branch.",
            stale_ref(branch)
        )
    });
    WorkspaceError::Refused {
        code: WORKSPACE_STALE.to_string(),
        message: format!(
            "the workspace of {branch} could not follow its branch (another writer moved it \
             under a merge of main).{kept} End the execution; the next mount starts from the \
             branch head."
        ),
    }
}

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
/// as the path's shortest ([`GitWorkspace::shortest_markers`]). Each is
/// opened the safe way ([`crate::agent_files::read_regular`]): a deleted
/// file holds none, and neither does something that is not a regular file
/// (a symlink is not followed, a FIFO is not waited on: the merge holds the
/// branch's lock). A file with more than [`CONFLICT_READ_LIMIT`] bytes, or
/// one that cannot be read, counts as unresolved.
fn unresolved(directory: &Path, files: &[(TreePath, usize)]) -> bool {
    use crate::agent_files::{AgentFileError, read_regular};
    files.iter().any(
        |(file, shortest)| match read_regular(directory, file, CONFLICT_READ_LIMIT) {
            Ok(Some((bytes, _, more))) => more || has_conflict_block_sized(&bytes, *shortest),
            Ok(None) | Err(AgentFileError::NotRegular(_) | AgentFileError::Unsafe(_)) => false,
            Err(AgentFileError::Failed(_)) => true,
        },
    )
}

/// Files over the cap with their sizes, then special files
/// ([`GitWorkspace::left_out_files`]).
type LeftOut = (Vec<(TreePath, u64)>, Vec<(TreePath, u64)>);

/// Whether `path` is a regular file or a symlink in `directory`, reached
/// through directories only: git adds nothing beyond a symbolic link.
fn is_file_in(directory: &Path, path: &TreePath) -> bool {
    for prefix in path.prefixes() {
        let Ok(metadata) = std::fs::symlink_metadata(directory.join(prefix.as_path())) else {
            return false;
        };
        let kind = metadata.file_type();
        if prefix == *path {
            return kind.is_file() || kind.is_symlink();
        }
        if !kind.is_dir() {
            return false;
        }
    }
    false
}

/// Remove git's own lock beside `index` (`<index>.lock`), left by a git
/// that died mid-command (killed, out of memory, a SIGBUS on a file the
/// agent truncated while git mapped it): while it is there every git on
/// the index fails, so every checkpoint and merge of the branch would, and
/// only a discard got out. Called only under the branch's lock, where no
/// git of ours runs on that index.
fn clear_dead_lock(index: &Path) {
    let mut lock = index.as_os_str().to_owned();
    lock.push(".lock");
    let _ = std::fs::remove_file(PathBuf::from(lock));
}

/// The most of one conflicted file [`unresolved`] reads.
const CONFLICT_READ_LIMIT: u64 = 64 << 20;

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

    /// Far more left-out paths than one command line holds (Linux: 2 MiB
    /// of arguments) still go through every git call that names them: the
    /// checkpoint's `add`, `rm --cached` and the blob lookup.
    #[test]
    fn many_left_out_paths_never_overflow_a_command_line() {
        use super::TreePath;
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

        let held = store
            .blobs_in(&["main".to_string()], &paths)
            .expect("blob lookup");
        let mut held: Vec<String> = held.iter().map(ToString::to_string).collect();
        held.sort();
        let mut expected = names[..2].to_vec();
        expected.sort();
        assert_eq!(held, expected);

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
            .expect("add with every left-out path excluded");
        assert_eq!(
            staged(&index),
            "added.txt\nother.txt\n",
            "no left-out path is staged"
        );
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
            .expect("rm --cached of every left-out path");
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
        let show = |path: &str| {
            git(
                &checkout,
                &["show", &format!("refs/spawn-stale/agent_a:{path}")],
            )
        };
        assert_eq!(
            show("new.txt"),
            "edited",
            "the checkpoint was kept off the branch"
        );

        // Work after the last checkpoint: a mount keeps it before it
        // replaces the directory.
        std::fs::write(directory.join("work.txt"), "later\n").expect("write");
        let branch = BranchName::new("agent_a").expect("valid branch");
        let workspace = store.mount(&branch).expect("mount again");
        assert_eq!(show("work.txt"), "later");
        assert_eq!(
            git(
                &checkout,
                &["rev-list", "--count", "refs/spawn-stale/agent_a"]
            ),
            git(&checkout, &["rev-list", "--count", "refs/heads/agent_a"])
                .parse::<u32>()
                .map(|count| (count + 2).to_string())
                .expect("a count"),
            "two snapshots, chained on the head"
        );
        assert_eq!(
            store.commit(&workspace, "fresh").expect("commit"),
            Commit::Unchanged
        );
    }

    /// A stale workspace whose state cannot be kept (the snapshot's commit
    /// fails) is neither replaced by a mount nor removed by an unmount.
    #[test]
    fn a_stale_workspace_that_cannot_be_kept_is_not_wiped() {
        let (_checkout, store, workspace, base) = due_merge("stale-unkept");
        let directory = PathBuf::from(workspace.path());
        store.mark_stale("agent_a").expect("mark");
        std::fs::write(directory.join("work.txt"), "unkept\n").expect("write");
        let script = base.join("failing-git");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$*\" in\n  *\"stale workspace\"*) exit 1 ;;\nesac\nexec git \"$@\"\n",
        )
        .expect("write the failing git");
        assert!(
            Command::new("chmod")
                .arg("+x")
                .arg(&script)
                .status()
                .expect("chmod")
                .success()
        );
        let failing = GitWorkspace {
            git: script.to_string_lossy().to_string(),
            ..store.clone()
        };
        let branch = BranchName::new("agent_a").expect("valid branch");
        assert!(failing.mount(&branch).is_err(), "the mount fails");
        assert!(failing.unmount(&workspace).is_err(), "the unmount fails");
        assert_eq!(
            std::fs::read_to_string(directory.join("work.txt")).expect("still there"),
            "unkept\n"
        );
        store.unmount(&workspace).expect("kept, then removed");
        assert!(!directory.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A repository with `main`, and `agent_a` mounted under `<base>/work`
    /// by a store whose cap for one file is 1 MiB.
    fn capped(tag: &str) -> (PathBuf, GitWorkspace, WorkspaceId, PathBuf) {
        let base = std::env::temp_dir().join(format!("spawn-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let checkout = base.join("repo");
        std::fs::create_dir_all(&checkout).expect("create the repository");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(checkout.join("README.md"), "base\n").expect("write");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);
        let store = GitWorkspace::new(checkout.join(".git"), base.join("work"))
            .with_max_file_bytes(1 << 20);
        let branch = BranchName::new("agent_a").expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        (checkout, store, workspace, base)
    }

    /// A file at the cap is an ordinary blob of the checkpoint; one over it
    /// is left out without failing the checkpoint, the rest is committed,
    /// and the agent is told once. A file that shrinks under the cap is
    /// committed, and told of again if it grows past it again.
    #[test]
    fn a_file_over_the_cap_is_left_out_and_told_once() {
        let (checkout, store, workspace, base) = capped("cap");
        let directory = PathBuf::from(workspace.path());
        std::fs::write(directory.join("at-cap.bin"), vec![1u8; 1 << 20]).expect("write");
        std::fs::write(directory.join("over.bin"), vec![2u8; (1 << 20) + 1]).expect("write");
        std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
        assert!(matches!(
            store
                .commit(&workspace, "one")
                .expect("the checkpoint goes on"),
            Commit::Created(_)
        ));
        let files = git(&checkout, &["ls-tree", "--name-only", "agent_a"]);
        assert_eq!(
            files, "README.md\nat-cap.bin\nwork.txt",
            "over.bin is left out"
        );
        let notices = store.notices(&workspace);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with("over.bin (2 MiB) is over the 1 MiB cap"),
            "{notices:?}"
        );
        assert_eq!(
            store.notices(&workspace),
            notices,
            "untold until a prompt carries it"
        );
        store.notices_told(&workspace, &notices);
        assert!(store.notices(&workspace).is_empty(), "told once");
        std::fs::write(directory.join("work.txt"), "more\n").expect("write");
        store.commit(&workspace, "two").expect("commit");
        assert!(store.notices(&workspace).is_empty(), "still told once");

        std::fs::write(directory.join("over.bin"), "small now\n").expect("write");
        store.commit(&workspace, "three").expect("commit");
        assert_eq!(
            git(&checkout, &["show", "agent_a:over.bin"]),
            "small now",
            "under the cap it is a blob"
        );
        std::fs::write(directory.join("over.bin"), vec![3u8; 3 << 20]).expect("write");
        store.commit(&workspace, "four").expect("commit");
        assert_eq!(store.notices(&workspace).len(), 1, "told again");
        assert!(
            !git(&checkout, &["ls-tree", "--name-only", "agent_a"]).contains("over.bin"),
            "left out again"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A file over the cap that `main` holds as a blob (it came from the
    /// source) stays a blob: leaving it out would delete it from the branch,
    /// and from `main` when the branch lands.
    #[test]
    fn a_file_over_the_cap_that_main_holds_stays_a_blob() {
        let base =
            std::env::temp_dir().join(format!("spawn-git-cap-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let checkout = base.join("repo");
        std::fs::create_dir_all(&checkout).expect("create the repository");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(checkout.join("model.bin"), vec![4u8; 2 << 20]).expect("write");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);
        let store = GitWorkspace::new(checkout.join(".git"), base.join("work"))
            .with_max_file_bytes(1 << 20);
        let branch = BranchName::new("agent_a").expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        let directory = PathBuf::from(workspace.path());
        assert_eq!(
            std::fs::metadata(directory.join("model.bin"))
                .expect("mounted")
                .len(),
            2 << 20
        );
        std::fs::write(directory.join("model.bin"), vec![5u8; 2 << 20]).expect("write");
        assert!(matches!(
            store.commit(&workspace, "changed").expect("commit"),
            Commit::Created(_)
        ));
        assert_eq!(
            git(&checkout, &["cat-file", "-s", "agent_a:model.bin"]),
            (2u64 << 20).to_string()
        );
        assert!(store.notices(&workspace).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A branch whose head names large files an earlier version kept
    /// outside git is refused at mount, with a message that says so; the
    /// workspace already there stays.
    #[test]
    fn a_head_that_names_files_outside_git_is_refused_at_mount() {
        let (checkout, store, workspace, base) = capped("outside-git");
        let directory = PathBuf::from(workspace.path());
        std::fs::write(directory.join("work.txt"), "unsaved\n").expect("write");
        let tree = git(&checkout, &["rev-parse", "agent_a^{tree}"]);
        let message = format!("spawn checkpoint\n\nSpawn-Large: {}", "a".repeat(64));
        let commit = git(
            &checkout,
            &["commit-tree", &tree, "-p", "agent_a", "-m", &message],
        );
        git(&checkout, &["update-ref", "refs/heads/agent_a", &commit]);
        let branch = BranchName::new("agent_a").expect("valid branch");
        let refused = store.mount(&branch);
        assert!(
            matches!(&refused, Err(WorkspaceError::Refused { code, message }) if code == "FilesOutsideGit" && message.contains("Spawn-Large")),
            "{refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("work.txt")).expect("still there"),
            "unsaved\n"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_hash_in_the_last_paragraph_names_files_outside_git() {
        use super::names_files_outside_git;
        let hash = "0123456789abcdef".repeat(4);
        assert!(names_files_outside_git(&format!(
            "work\n\nSpawn-Large: {hash}\n"
        )));
        assert!(!names_files_outside_git("work"));
        assert!(!names_files_outside_git(&format!(
            "Spawn-Large: {hash}\n\nwork"
        )));
        assert!(!names_files_outside_git("work\n\nSpawn-Large: not-a-hash"));
    }

    #[test]
    fn the_cap_is_a_positive_number_of_mib_or_the_default() {
        use super::{DEFAULT_MAX_FILE_MB, max_file_bytes};
        assert_eq!(max_file_bytes(Some("20")), 20 << 20);
        for unset in [None, Some(""), Some("0"), Some("-5"), Some("lots")] {
            assert_eq!(
                max_file_bytes(unset),
                DEFAULT_MAX_FILE_MB << 20,
                "{unset:?}"
            );
        }
    }

    /// A store with branch `agent_a` checkpointed once (`start`, holding
    /// `kept.txt`) and once more with a cancelled task's work (`work`,
    /// adding `one.txt` and changing `kept.txt`).
    fn worked(tag: &str) -> (GitWorkspace, WorkspaceId, PathBuf, String, String) {
        let (_, store, workspace, base) = capped(tag);
        let directory = PathBuf::from(workspace.path());
        std::fs::write(directory.join("kept.txt"), "before\n").expect("write");
        let Commit::Created(start) = store.commit(&workspace, "start").expect("commit") else {
            panic!("a change");
        };
        std::fs::write(directory.join("kept.txt"), "during\n").expect("write");
        std::fs::write(directory.join("one.txt"), "one\n").expect("write");
        let Commit::Created(work) = store.commit(&workspace, "work").expect("commit") else {
            panic!("a change");
        };
        (
            store,
            workspace,
            base,
            start.as_str().to_string(),
            work.as_str().to_string(),
        )
    }

    /// The set-aside is a new commit on top of the head: its tree is the
    /// start's, it descends from the old head, and the old head (the work)
    /// stays reachable from the branch. The mounted workspace follows it.
    #[test]
    fn a_set_aside_restores_the_start_tree_on_top_of_the_work() {
        use spawn_core::port::SetAside;
        let (store, workspace, base, start, work) = worked("set-aside");
        let branch = BranchName::new("agent_a").expect("valid branch");
        let SetAside::Created {
            commit,
            kept,
            files,
        } = store
            .set_aside(&branch, Some(&start), "spawn: set aside task #7")
            .expect("set aside")
        else {
            panic!("the work differs from the start");
        };
        assert_eq!(kept.as_str(), work, "the old head holds the work");
        assert_eq!(files, ["kept.txt", "one.txt"]);
        let repo = base.join("repo");
        let tree = |commit: &str| git(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
        assert_eq!(tree(commit.as_str()), tree(&start), "the start's tree");
        assert_eq!(git(&repo, &["rev-parse", "agent_a"]), commit.as_str());
        assert_eq!(
            git(&repo, &["rev-parse", &format!("{}^", commit.as_str())]),
            work,
            "it descends from the old head"
        );
        assert_eq!(git(&repo, &["show", "agent_a~1:one.txt"]), "one");
        let directory = PathBuf::from(workspace.path());
        assert!(!directory.join("one.txt").exists());
        assert_eq!(
            std::fs::read_to_string(directory.join("kept.txt")).expect("read"),
            "before\n"
        );
        assert_eq!(
            store.commit(&workspace, "next").expect("commit"),
            Commit::Unchanged,
            "the workspace is the branch head"
        );
        assert_eq!(
            store
                .set_aside(&branch, Some(&start), "again")
                .expect("again"),
            SetAside::Unchanged,
            "a second set-aside finds nothing to set aside"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What the workspace holds and no checkpoint took is committed before
    /// the set-aside, so it is kept too; a branch not mounted moves alone;
    /// a start that is no commit of the branch is refused.
    #[test]
    fn a_set_aside_keeps_what_no_checkpoint_took_and_refuses_a_foreign_start() {
        use spawn_core::port::SetAside;
        let (store, workspace, base, start, work) = worked("set-aside-unsaved");
        let branch = BranchName::new("agent_a").expect("valid branch");
        let directory = PathBuf::from(workspace.path());
        std::fs::write(directory.join("late.txt"), "late\n").expect("write");
        let repo = base.join("repo");
        let foreign = git(
            &repo,
            &["commit-tree", "-m", "foreign", &format!("{start}^{{tree}}")],
        );
        assert!(matches!(
            store.set_aside(&branch, Some(&foreign), "no"),
            Err(WorkspaceError::Refused { .. })
        ));
        let SetAside::Created { kept, .. } = store
            .set_aside(&branch, Some(&start), "spawn: set aside")
            .expect("set aside")
        else {
            panic!("the work differs from the start");
        };
        assert_ne!(
            kept.as_str(),
            work,
            "the unsaved state got a commit of its own"
        );
        assert_eq!(
            git(&repo, &["rev-parse", &format!("{}^", kept.as_str())]),
            work
        );
        assert_eq!(
            git(&repo, &["show", &format!("{}:late.txt", kept.as_str())]),
            "late"
        );
        assert!(!directory.join("late.txt").exists());

        // Not mounted: the branch moves, no directory is made.
        std::fs::write(directory.join("two.txt"), "two\n").expect("write");
        store.commit(&workspace, "more").expect("commit");
        store.unmount(&workspace).expect("unmount");
        assert!(matches!(
            store.set_aside(&branch, Some(&start), "spawn: set aside"),
            Ok(SetAside::Created { .. })
        ));
        assert!(!directory.exists());
        let tree = |commit: &str| git(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
        assert_eq!(tree("agent_a"), tree(&start));
        let _ = std::fs::remove_dir_all(&base);
    }
}

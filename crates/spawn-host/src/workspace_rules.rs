//! Shared workspace policy constants, locking, oversized-file tracking,
//! conflict-marker detection, and trailer rules shared across workspace stores
//! (`D6Workspace`, `GitWorkspace`).
//!
//! Keeping these pure-Rust / descriptor-safe rules outside `crate::git` lets
//! D6 CAS workspaces enforce identical boundaries without pulling any Git
//! subprocess adapter dependency into `d6_workspace`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use spawn_core::port::WorkspaceError;

use crate::agent_files::{open_regular, read_regular};
use crate::tree_path::TreePath;

/// Files the engine writes for itself in its working directory, at any
/// depth: its runtime state (a session's scheduler lock, its worktrees, its
/// checkpoints, its mailbox), never the project's. No checkpoint adds one,
/// whatever the project's `.gitignore` says; the rest of `.claude/` (the
/// settings, agents and commands a project commits) is committed as usual
/// (`GitWorkspace::track_runtime_files`, docs/storage.md).
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

/// Where main's version of each file that conflicts with the branch in a
/// way markers cannot show is copied, relative to the workspace (under
/// `.spawn/`, never committed), for a worker that cannot run git. Every
/// mount puts the copies back while the conflict is open
/// (`GitWorkspace::restore_main_copies`).
pub const MAIN_COPIES: &str = ".spawn/main";

/// Where a set-aside moves a file over the cap that was in the way of a
/// file of the tree it returns the workspace to, under the commit that
/// keeps the rest of the work: `.spawn/set-aside/<commit>/<path>`. Like
/// [`MAIN_COPIES`], under `.spawn/`, which no checkpoint takes.
pub const SET_ASIDE_FILES: &str = ".spawn/set-aside";

/// The refusal code of a set-aside whose `ready` said the agent is not idle
/// (`GitWorkspace::set_aside_when`): nothing was written, and the caller
/// asks again later.
pub const SET_ASIDE_WAITS: &str = "SetAsideWaits";

/// The size cap of one file in a checkpoint, in MiB, when
/// `SPAWN_MAX_FILE_MB` does not set one: git stores every version of a file
/// whole, so a file over it would cost its full size in the repository at
/// every checkpoint that changed it (docs/storage.md).
pub const DEFAULT_MAX_FILE_MB: u64 = 100;

/// The default size cap of one file in a checkpoint, in bytes
/// ([`DEFAULT_MAX_FILE_MB`] MiB).
pub const DEFAULT_MAX_FILE_BYTES: u64 = DEFAULT_MAX_FILE_MB << 20;

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

/// The most bytes [`oversized_record`] reads of a record: one larger is
/// no record of this store's and reads as none.
pub(crate) const OVERSIZED_RECORD_LIMIT: u64 = 64 << 20;

/// One entry of a branch's record of files over the cap
/// ([`oversized_record`]): its size, whether the agent was told, and its
/// `sha256:` digest (`None` in a record written before digests were, or
/// for a file that could not be read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Oversized {
    pub(crate) size: u64,
    pub(crate) told: bool,
    pub(crate) digest: Option<String>,
}

/// One file over the cap that checkpoints of a branch left out, as
/// `<root>/.oversized/<branch>` records it ([`oversized_record`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OversizedFile {
    /// The workspace path.
    pub path: TreePath,
    /// Its size in bytes when the checkpoint left it out.
    pub size: u64,
    /// `sha256:<64 hex>` of its bytes then; `None` in a record written
    /// before digests were recorded, or for a file that could not be read.
    pub digest: Option<String>,
    /// Whether the agent was told.
    pub told: bool,
}

/// Where a branch's record of files over the cap is under the workspace
/// root `root`.
pub(crate) fn oversized_file(root: &Path, branch: &str) -> PathBuf {
    root.join(".oversized").join(branch)
}

/// Whether `name` is exactly one normal path component: not empty, no
/// `/` or `\`, not `.` or `..`, no root or prefix.
pub(crate) fn is_plain_name(name: &str) -> bool {
    if name.is_empty() || name.contains(['/', '\\', '\0']) {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(part)), None) if part == OsStr::new(name)
    )
}

/// Whether `text` is `sha256:` and 64 lowercase hex digits.
fn is_sha256_digest(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// A record of files over the cap, by path ([`oversized_record`] for the
/// format). An entry that does not parse is left out, an empty path
/// included: fields are split on every NUL (an empty field keeps its
/// place), so one bad entry never shifts the ones after it.
pub(crate) fn parse_oversized(bytes: &[u8]) -> BTreeMap<TreePath, Oversized> {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    if bytes.is_empty() {
        return BTreeMap::new();
    }
    let fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    fields
        .chunks_exact(3)
        .filter_map(|entry| {
            let told = match entry[0] {
                b"told" => true,
                b"untold" => false,
                _ => return None,
            };
            let sized = std::str::from_utf8(entry[1]).ok()?;
            let (size, digest) = match sized.split_once(' ') {
                Some((size, digest)) => (size, is_sha256_digest(digest).then_some(digest)),
                None => (sized, None),
            };
            let size = size.parse().ok()?;
            if entry[2].is_empty() {
                return None;
            }
            Some((
                TreePath::from_bytes(entry[2]),
                Oversized {
                    size,
                    told,
                    digest: digest.map(str::to_string),
                },
            ))
        })
        .collect()
}

/// Serialize a branch's oversized-file table into the NUL-delimited `.oversized/<branch>` format.
pub(crate) fn encode_oversized(record: &BTreeMap<TreePath, Oversized>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (file, entry) in record {
        bytes.extend_from_slice(if entry.told { b"told" } else { b"untold" });
        bytes.push(0);
        bytes.extend_from_slice(entry.size.to_string().as_bytes());
        if let Some(digest) = &entry.digest {
            bytes.push(b' ');
            bytes.extend_from_slice(digest.as_bytes());
        }
        bytes.push(0);
        bytes.extend_from_slice(file.as_bytes());
        bytes.push(0);
    }
    bytes
}

/// Format the agent-facing notice for one file excluded due to exceeding `max_file_bytes`.
pub(crate) fn oversized_notice_message(path: &TreePath, size: u64, max_file_bytes: u64) -> String {
    format!(
        "{path} ({} MiB) is over the {} MiB cap for one file in a checkpoint \
         (SPAWN_MAX_FILE_MB), so checkpoints leave it out: it is not on your branch, \
         and it is lost when this workspace is unmounted. Make it smaller, split it, \
         or make sure it can be generated again from what is committed.",
        size.div_ceil(1 << 20),
        max_file_bytes >> 20
    )
}

/// The files over the cap ([`max_file_bytes`]) that checkpoints of
/// `branch` left out, as the record under the workspace root `root` holds
/// them, by path; empty when there is none, it cannot be read, or `branch`
/// is not one plain file name (one normal path component: no `/`, `\`,
/// `.`, `..`, root or prefix), so the record read is always a file directly
/// under `<root>/.oversized/`. The record is opened without following a
/// symlink on the way (`.oversized` included) and must be a regular file.
///
/// The record, `<root>/.oversized/<branch>`, is NUL-separated, three fields
/// each: `told` or `untold`, the size in bytes, the path. Since PV4
/// (decision D6) the size field is followed by a space and the file's
/// `sha256:<64 hex>` digest: the file is in no commit and its bytes are
/// kept nowhere, but what it was is known. A record written before has no
/// digest and still reads (`digest: None`).
pub fn oversized_record(root: &Path, branch: &str) -> Vec<OversizedFile> {
    if !is_plain_name(branch) {
        return Vec::new();
    }
    let mut relative = b".oversized/".to_vec();
    relative.extend_from_slice(branch.as_bytes());
    let Ok(Some((bytes, _, false))) = read_regular(
        root,
        &TreePath::from_bytes(relative),
        OVERSIZED_RECORD_LIMIT,
    ) else {
        return Vec::new();
    };
    parse_oversized(&bytes)
        .into_iter()
        .map(|(path, entry)| OversizedFile {
            path,
            size: entry.size,
            digest: entry.digest,
            told: entry.told,
        })
        .collect()
}

/// The `sha256:` digest of the file over the cap at `path` under
/// `directory`, `size` bytes when the checkpoint measured it. The digest
/// `earlier` recorded is kept when the file still has that size and was
/// last modified before the record was `written`; otherwise the file is
/// read, without following a symlink anywhere on its path. `None` when it
/// cannot be read or no longer has `size` bytes (a file that changed under
/// the checkpoint: the next one records it).
pub(crate) fn oversized_digest(
    directory: &Path,
    path: &TreePath,
    size: u64,
    earlier: Option<&Oversized>,
    written: Option<SystemTime>,
) -> Option<String> {
    let opened = open_regular(directory, path).ok()??;
    if opened.metadata.len() != size {
        return None;
    }
    if let (Some(earlier), Some(written), Ok(modified)) =
        (earlier, written, opened.metadata.modified())
        && earlier.size == size
        && earlier.digest.is_some()
        && modified < written
    {
        return earlier.digest.clone();
    }
    let mut file = opened.file;
    let digest = spawn_cas::sha256_reader(&mut file).ok()?;
    Some(format!("sha256:{}", digest.hex()))
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

/// The commit message of the checkpoint a set-aside takes first of a
/// mounted workspace's work no checkpoint took, in every store.
pub(crate) fn set_aside_kept_message(branch: &str) -> String {
    format!("spawn: the workspace of {branch} as it was, kept before its work is set aside")
}

/// Hold the exclusive `flock` of `lock_path` (made, with its directory,
/// when missing) until the returned file is dropped: the lock every
/// checkpoint, mount and unmount of one branch takes across workspace stores.
pub(crate) fn lock_file(lock_path: &Path) -> Result<std::fs::File, WorkspaceError> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            WorkspaceError::Unavailable(format!("create {}: {error}", parent.display()))
        })?;
    }
    let lock = std::fs::File::create(lock_path).map_err(|error| {
        WorkspaceError::Unavailable(format!("create {}: {error}", lock_path.display()))
    })?;
    lock.lock().map_err(|error| {
        WorkspaceError::Unavailable(format!("lock {}: {error}", lock_path.display()))
    })?;
    Ok(lock)
}

/// The length of the conflict markers git writes by default.
pub(crate) const CONFLICT_MARKER_SIZE: usize = 7;

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

/// Ref prefix where Spawn preserves the latest worker-authored `HEAD` for a branch.
pub const WORKER_HEAD_REF_PREFIX: &str = "refs/spawn/workers/";

/// Ref prefix where Spawn pins task-scoped worker `HEAD` commits by stage.
pub const TASK_HEAD_REF_PREFIX: &str = "refs/spawn/tasks/";

/// Ref prefix for worker-scoped branch refs under `refs/heads/`.
pub const SPAWN_BRANCH_REF_PREFIX: &str = "refs/heads/spawn/";

/// Ref prefix where stale workspace checkpoints are preserved.
pub const STALE_REF_PREFIX: &str = "refs/spawn-stale/";

/// Bare-repo or D6 reference pinning the latest worker-authored `HEAD` commit
/// graph preserved from `branch`'s mounted workspace (`#103`, `#110`).
#[must_use]
pub fn worker_head_ref(branch: &str) -> String {
    format!("{WORKER_HEAD_REF_PREFIX}{branch}/head")
}

/// Bare-repo or D6 reference pinning a task's worker-authored `HEAD` commit
/// graph at `stage` (`"head"` at report, `"accepted"` at task acceptance).
#[must_use]
pub fn task_head_ref(task_id: i64, stage: &str) -> String {
    format!("{TASK_HEAD_REF_PREFIX}{task_id}/{stage}")
}

/// Worker branch reference under `refs/heads/spawn/`.
#[must_use]
pub fn spawn_branch_ref(branch: &str) -> String {
    format!("{SPAWN_BRANCH_REF_PREFIX}{branch}")
}

/// Reference where a stale workspace's diverged work is kept (`never pushed`,
/// `never landed`).
#[must_use]
pub fn stale_ref(branch: &str) -> String {
    format!("{STALE_REF_PREFIX}{branch}")
}

/// Candidate full ref names checked in priority order when resolving a task's
/// preserved commits for review, diff, or landing.
#[must_use]
pub fn candidate_task_refs(task_id: i64, agent_id: &str) -> Vec<String> {
    let mut refs = vec![task_head_ref(task_id, "head")];
    if !agent_id.is_empty() {
        refs.push(worker_head_ref(agent_id));
        refs.push(spawn_branch_ref(agent_id));
    }
    refs
}

/// Whether `ref_name` is a protected per-task durable ref (`refs/spawn/tasks/<id>/*`).
#[must_use]
pub fn is_protected_task_ref(ref_name: &str) -> bool {
    ref_name.starts_with(TASK_HEAD_REF_PREFIX)
}

/// Whether `ref_name` is an internal Spawn coordination, preservation, or
/// staging reference outside user-facing branches and tags.
#[must_use]
pub fn is_spawn_internal_ref(ref_name: &str) -> bool {
    ref_name.starts_with(TASK_HEAD_REF_PREFIX)
        || ref_name.starts_with(WORKER_HEAD_REF_PREFIX)
        || ref_name.starts_with(STALE_REF_PREFIX)
        || ref_name.starts_with("refs/spawn-integrate/")
        || ref_name.starts_with("refs/spawn-restore/")
        || ref_name.starts_with("refs/spawn-owner/")
        || ref_name.starts_with("refs/d6-stage/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;

    #[test]
    fn d6_workspace_has_zero_crate_git_references() {
        let d6_src = include_str!("d6_workspace.rs");
        assert!(
            !d6_src.contains("crate::git"),
            "d6_workspace.rs must remain strictly decoupled from crate::git"
        );
    }

    #[test]
    fn shared_workspace_rule_helpers_preserve_contract() {
        assert_eq!(max_file_bytes(None), DEFAULT_MAX_FILE_MB << 20);
        assert_eq!(max_file_bytes(Some("0")), DEFAULT_MAX_FILE_MB << 20);
        assert_eq!(max_file_bytes(Some("12")), 12 << 20);

        let trailer_msg = format!("subject\n\n{LARGE_FILE_TRAILER}: {}\n", "a".repeat(64));
        assert!(names_files_outside_git(&trailer_msg));
        assert!(!names_files_outside_git("normal commit\n"));

        assert!(has_conflict_block(
            b"<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n"
        ));
        assert!(!has_conflict_block(b"Title\n=======\n"));
        assert!(set_aside_kept_message("feat/x").contains("feat/x"));
    }

    #[test]
    fn oversized_record_reads_valid_entries_and_rejects_traversal() {
        let scratch = Scratch::new("ws_rules_oversized");
        let root = scratch.join("root");
        std::fs::create_dir_all(root.join(".oversized")).expect("mkdir .oversized");
        let digest = format!("sha256:{}", "b".repeat(64));
        let payload = format!("untold\x001048577 {digest}\0big.bin\0");
        std::fs::write(root.join(".oversized/agent_a"), &payload).expect("write record");

        let rows = oversized_record(&root, "agent_a");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path.as_bytes(), b"big.bin");
        assert_eq!(rows[0].size, 1_048_577);
        assert_eq!(rows[0].digest.as_deref(), Some(digest.as_str()));
        assert!(!rows[0].told);

        for bad in ["", ".", "..", "../secret", "a/b", "agent_a/"] {
            assert!(oversized_record(&root, bad).is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn pure_ref_naming_helpers_preserve_contract() {
        assert_eq!(worker_head_ref("w-1"), "refs/spawn/workers/w-1/head");
        assert_eq!(task_head_ref(42, "head"), "refs/spawn/tasks/42/head");
        assert_eq!(
            task_head_ref(42, "accepted"),
            "refs/spawn/tasks/42/accepted"
        );
        assert_eq!(spawn_branch_ref("w-1"), "refs/heads/spawn/w-1");
        assert_eq!(stale_ref("w-1"), "refs/spawn-stale/w-1");

        assert_eq!(
            candidate_task_refs(42, "w-1"),
            vec![
                "refs/spawn/tasks/42/head",
                "refs/spawn/workers/w-1/head",
                "refs/heads/spawn/w-1",
            ]
        );
        assert_eq!(
            candidate_task_refs(42, ""),
            vec!["refs/spawn/tasks/42/head"]
        );

        assert!(is_protected_task_ref("refs/spawn/tasks/42/head"));
        assert!(!is_protected_task_ref("refs/spawn/workers/w-1/head"));

        for internal in [
            "refs/spawn/tasks/42/head",
            "refs/spawn/workers/w-1/head",
            "refs/spawn-stale/w-1",
            "refs/spawn-integrate/w-1",
            "refs/spawn-restore/w-1",
            "refs/spawn-owner/push-1",
            "refs/d6-stage/run-1",
        ] {
            assert!(is_spawn_internal_ref(internal), "{internal}");
        }
        assert!(!is_spawn_internal_ref("refs/heads/main"));
        assert!(!is_spawn_internal_ref("refs/heads/spawn/w-1"));
        assert!(!is_spawn_internal_ref("refs/tags/v1.0"));
    }
}

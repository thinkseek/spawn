//! Copy-on-write workspaces: every worker gets a normal POSIX directory
//! cloned, extent by extent, from one read-only baseline per commit.
//!
//! No VM, no FUSE, no overlay mount and no wrapper around the agent, its
//! compiler or its IDE: the workspace is a plain directory on the host's own
//! file system. What makes it cheap is the file system's clone primitive:
//!
//! - Linux (btrfs, XFS with `reflink=1`, bcachefs): `ioctl(FICLONE)`, the
//!   whole-file form of `FICLONERANGE`, through `rustix` (the workspace
//!   forbids `unsafe`, and `rustix` wraps the ioctl safely), between two
//!   descriptors.
//! - macOS (APFS): `fclonefileat(2)` through `rustix`, from the open source
//!   into a directory descriptor; it never overwrites (`EEXIST`).
//! - Anywhere else, or on a file system without clones (ext4, tmpfs): a
//!   copy into an exclusively made target ([`clone_or_copy`]), so the store
//!   still works, only slower; it is counted as a copy, never a clone.
//!
//! Layout under the store's root:
//!
//! ```text
//! <root>/<branch>/                      the worker's workspace (plain dir)
//! <root>/.cow/baselines/<commit>/tree/  Layer 0: read-only files of <commit>
//! <root>/.cow/baselines/<commit>/entries  `git ls-tree -r -z` of <commit>
//! <root>/.cow/state/<branch>            head, mounted generation, manifest
//! <root>/.cow/locks/<branch>.lock       the branch's flock
//! <root>/.cow/locks/<branch>.admission  the workspace's admission lock
//! <root>/.cow/kept/                     what an unmount refused to delete
//! ```
//!
//! A tree is checked before a byte of it is written ([`UNSAFE_TREE`]): no
//! empty, `.` or `..` component, no `.git` in any spelling git itself
//! refuses (case, trailing dots and spaces, `git~1`, HFS ignorables), no
//! duplicate path and no path below another entry. Every file is then made
//! relative to a directory descriptor, one component at a time, with
//! `O_NOFOLLOW`: a tree can never write outside its own directory.
//!
//! Which paths a checkpoint takes is the Git store's rule, shared with it
//! (`GitWorkspace::checkpoint_paths`): git's ignore rules, never an engine
//! runtime file, no file over the size cap.
//!
//! The manifest records, for every committed path, its git mode and blob id
//! plus the `lstat` stamp (size, inode, mtime, ctime) the workspace file had
//! when that blob id was last known to be right. A checkpoint
//! ([`WorkspaceStore::commit`]) re-hashes only the files whose stamp moved,
//! so an unchanged tree of any size costs one `lstat` per file and no read.
//! A stamp is trusted only when it is older than the manifest's fence: a
//! timestamp read off the file system's own clock (a probe file touched in
//! `.cow/tmp`) before the `lstat` walk. A file changed after the walk gets a
//! time at or past the fence, so its stamp moves even when the clock is
//! coarse; one changed in the fence's own tick is re-hashed (git's "racy
//! clean" rule, without a guessed margin). Submodule gitlinks (mode
//! `160000`) are mounted as empty directories and carried over unchanged
//! while their directory is there.
//!
//! The branch moves by compare-and-swap (`git update-ref <ref> <new> <old>`);
//! a branch someone else moved meanwhile is refused as
//! [`WORKSPACE_STALE`], and the workspace's state is kept first on
//! `refs/spawn-cow/stale/<branch>/<commit>` (one ref per kept state, never
//! overwritten), so nothing is lost.
//!
//! Every mount, commit and unmount of a branch holds the branch's `flock`,
//! so two stores (or two processes) on one root never interleave on it. Only
//! the directory a mount installed (its `(device, inode)`, recorded in the
//! state) is ever checked, replaced or deleted ([`WORKSPACE_NOT_OWNED`]).
//! An unmount, and a mount that would replace a workspace, hold the
//! workspace's admission lock exclusively throughout ([`WorkspaceLease`]), and
//! are refused while a worker holds its lease or a process is seen with
//! its working directory, an open file or a mapping inside it
//! ([`directory_occupancy`], [`WORKSPACE_BUSY`]).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "linux"))]
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{
    Branch, Commit, Hold, MOUNT_CANCELLED, SetAside, WORKSPACE_STALE, WorkspaceError,
    WorkspaceStore,
};

use crate::git::{GitWorkspace, report};

/// The store's own directory under its root.
const COW_DIRECTORY: &str = ".cow";

/// Directory names under which a checkpoint adds no untracked file, at any
/// depth, on top of git's own ignore rules: build output, the controller's
/// marker, transcripts and nested repositories. A path the branch tracks
/// there is taken as any tracked path is.
const SKIPPED_DIRECTORIES: [&str; 4] = ["target", ".spawn", ".transcripts", ".git"];

/// The git modes a tree entry may have here: file, executable, symlink,
/// gitlink.
const MODES: [u32; 4] = [0o100_644, 0o100_755, 0o120_000, 0o160_000];

/// The [`WorkspaceError::Refused`] code of a mount over a directory with
/// work no checkpoint took.
pub const WORKSPACE_DIRTY: &str = "WorkspaceDirty";

/// The [`WorkspaceError::Refused`] code of an unmount that would delete a
/// `*.jsonl` file (a transcript): nothing was deleted.
pub const WORKSPACE_HOLDS_TRANSCRIPTS: &str = "WorkspaceHoldsTranscripts";

/// The [`WorkspaceError::Refused`] code of an unmount that could not read
/// the whole workspace (so could not prove it holds no transcript): nothing
/// was deleted.
pub const WORKSPACE_UNREADABLE: &str = "WorkspaceUnreadable";

/// The [`WorkspaceError::Refused`] code of an unmount (or a replacing
/// mount) of a workspace a process still works in: nothing changed.
pub const WORKSPACE_BUSY: &str = "WorkspaceBusy";

/// The [`WorkspaceError::Refused`] code of an operation on a directory
/// that is not the generation this store mounted there (no state, or a
/// state of another directory): nothing changed.
pub const WORKSPACE_NOT_OWNED: &str = "WorkspaceNotOwned";

/// The [`WorkspaceError::Refused`] code of a commit whose tree could write
/// outside its directory (`..`, `.git`, a path below a symlink or a file, a
/// duplicate): nothing of it was written.
pub const UNSAFE_TREE: &str = "UnsafeTree";

// ---------------------------------------------------------------------------
// Clone primitives
// ---------------------------------------------------------------------------

/// How a file reached its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneKind {
    /// The target shares the source's extents (no data was copied).
    Reflinked,
    /// The file system cannot clone: the bytes were copied.
    Copied,
}

fn unsupported(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
}

/// Whether a clone error means "this file system cannot clone these":
/// `FICLONE` on Linux, `fclonefileat` on Apple targets.
fn clone_unsupported(errno: Errno) -> bool {
    #[cfg(target_os = "linux")]
    let unsupported = [
        Errno::OPNOTSUPP,
        Errno::NOTTY,
        Errno::XDEV,
        Errno::INVAL,
        Errno::NOSYS,
        Errno::PERM,
    ];
    #[cfg(not(target_os = "linux"))]
    let unsupported = [Errno::NOTSUP, Errno::OPNOTSUPP, Errno::XDEV, Errno::NOSYS];
    unsupported.contains(&errno)
}

/// The new regular file `name` in `directory`, made exclusively (never
/// over an existing name, never through a link), open for reading and
/// writing.
fn create_at(directory: &impl AsFd, name: &OsStr) -> io::Result<fs::File> {
    let flags = OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(fs::File::from(rustix::fs::openat(
        directory, name, flags, FILE_MODE,
    )?))
}

/// Copy every byte of `from`, from its start whatever its offset, into
/// `to`.
fn copy_from_start(from: &fs::File, to: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut at = 0;
    loop {
        let read = from.read_at(&mut buffer, at)?;
        if read == 0 {
            return Ok(());
        }
        to.write_all_at(&buffer[..read], at)?;
        at += read as u64;
    }
}

/// Clone the open file `from` into the new file `name` of `directory`, by
/// descriptor on both sides: `ioctl(FICLONE)` on Linux, `fclonefileat(2)`
/// on Apple targets. A target that exists is never replaced
/// ([`io::ErrorKind::AlreadyExists`]). When the file system cannot clone,
/// `copy` decides: the bytes are copied into the exclusively made target,
/// or [`io::ErrorKind::Unsupported`] is answered and no target is left.
///
/// The target is returned open: for reading and writing, except after an
/// Apple clone, which copies the source's mode (a 0444 blob makes a 0444
/// clone) and is reopened read-only; its mode and times can still be set
/// through it (the caller owns it).
pub(crate) fn clone_at(
    from: &fs::File,
    directory: &impl AsFd,
    name: &OsStr,
    copy: bool,
) -> io::Result<(fs::File, CloneKind)> {
    let remove = || rustix::fs::unlinkat(directory, name, rustix::fs::AtFlags::empty());
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        match rustix::fs::fclonefileat(from, directory, name, rustix::fs::CloneFlags::NOFOLLOW) {
            Ok(()) => {
                let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                let opened = rustix::fs::openat(directory, name, flags, Mode::empty());
                return match opened {
                    Ok(file) => Ok((fs::File::from(file), CloneKind::Reflinked)),
                    Err(error) => {
                        let _ = remove();
                        Err(error.into())
                    }
                };
            }
            Err(errno) if clone_unsupported(errno) => {}
            Err(errno) => return Err(errno.into()),
        }
        if !copy {
            return Err(unsupported("fclonefileat: the volume cannot clone"));
        }
    }
    let to = create_at(directory, name)?;
    #[cfg(target_os = "linux")]
    match rustix::fs::ioctl_ficlone(&to, from) {
        Ok(()) => return Ok((to, CloneKind::Reflinked)),
        Err(errno) if clone_unsupported(errno) && copy => {}
        Err(errno) => {
            drop(to);
            let _ = remove();
            return Err(if clone_unsupported(errno) {
                unsupported(format!("FICLONE: {errno}"))
            } else {
                errno.into()
            });
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
    if !copy {
        drop(to);
        let _ = remove();
        return Err(unsupported("no copy-on-write clone on this OS"));
    }
    match copy_from_start(from, &to) {
        Ok(()) => Ok((to, CloneKind::Copied)),
        Err(error) => {
            drop(to);
            let _ = remove();
            Err(error)
        }
    }
}

/// `source`, open for reading without following a link at its name and
/// without blocking on a FIFO, and the directory `target` goes in, open by
/// descriptor, with `target`'s last component.
fn open_pair<'t>(source: &Path, target: &'t Path) -> io::Result<(fs::File, DirFd, &'t OsStr)> {
    let from = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)?;
    let parent = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} names no file", target.display())))?;
    Ok((from, DirFd::open(parent)?, name))
}

/// Clone `source` into the new file `target` (which must not exist) so the
/// two share their data extents until either is written. A file system that
/// cannot clone answers [`io::ErrorKind::Unsupported`] and leaves no target.
pub fn reflink_file(source: &Path, target: &Path) -> io::Result<()> {
    let (from, directory, name) = open_pair(source, target)?;
    clone_at(&from, &directory.0, name, false).map(drop)
}

/// [`reflink_file`], or a copy (into an exclusively made target) when the
/// file system cannot clone.
pub fn clone_or_copy(source: &Path, target: &Path) -> io::Result<CloneKind> {
    let (from, directory, name) = open_pair(source, target)?;
    clone_at(&from, &directory.0, name, true).map(|(_, kind)| kind)
}

/// Clone the already open `from` into the new file `target`, returned open
/// ([`clone_at`]); never a copy.
pub(crate) fn reflink_new(from: &fs::File, target: &Path) -> io::Result<fs::File> {
    let parent = target.parent().unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} names no file", target.display())))?;
    let directory = DirFd::open(parent)?;
    clone_at(from, &directory.0, name, false).map(|(file, _)| file)
}

/// A name no other call of this process uses: `<pid>-<n>`.
pub(crate) fn unique_suffix() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Whether files in `directory` can be cloned copy-on-write: a probe file
/// (larger than btrfs's inline-extent limit) is cloned there and both are
/// removed.
pub fn cow_clone_supported(directory: &Path) -> bool {
    let suffix = unique_suffix();
    let source = directory.join(format!(".cow-probe-{suffix}"));
    let target = directory.join(format!(".cow-probe-{suffix}.clone"));
    let probed = fs::write(&source, vec![0x5a_u8; 16 * 1024])
        .and_then(|()| reflink_file(&source, &target))
        .is_ok();
    let _ = fs::remove_file(&target);
    let _ = fs::remove_file(&source);
    probed
}

/// Count the `*.jsonl` entries under `directory` (symlinks not followed).
/// The guard every deletion of this module goes through: a tree holding one
/// is kept, never removed. Fails closed: a directory or entry that cannot
/// be read (a mode-000 directory included) is an error, never a zero.
pub fn count_jsonl(directory: &Path) -> io::Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            count += count_jsonl(&entry.path())?;
        } else if is_jsonl(&entry.file_name()) {
            count += 1;
        }
    }
    Ok(count)
}

/// [`count_jsonl`] of a workspace without its own `.transcripts/` (which
/// every replacement and unmount keeps whole), as strictly.
fn jsonl_outside_transcripts(directory: &Path) -> io::Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir() {
            if name != crate::TRANSCRIPT_DIRECTORY {
                count += count_jsonl(&entry.path())?;
            }
        } else if is_jsonl(&name) {
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn is_jsonl(name: &OsStr) -> bool {
    name.as_bytes().ends_with(b".jsonl")
}

/// What a look at the process table found under a directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectoryOccupancy {
    /// The processes seen working there (this one included).
    pub pids: Vec<u32>,
    /// What could not be inspected, one line each (a same-user process
    /// whose links the kernel withholds, `lsof`'s warnings): never taken
    /// for "not there", and kept for the caller to report.
    pub uninspected: Vec<String>,
}

/// The processes working under `directory` ([`directory_occupancy`]).
pub fn live_users(directory: &Path) -> io::Result<Vec<u32>> {
    directory_occupancy(directory).map(|found| found.pids)
}

/// Whether a `/proc` read failed only because the process or thread is
/// gone.
#[cfg(target_os = "linux")]
fn exited(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// The processes of this host (those this user may inspect) one of whose
/// threads has its working directory or root, an open descriptor, or a
/// mapped file under `directory`; this process included. Linux reads
/// `/proc`, thread by thread. A process of this user whose links cannot be
/// read is recorded in [`DirectoryOccupancy::uninspected`]; one that exits during
/// the walk is not.
///
/// A snapshot: a process may start right after it. Teardown is gated by
/// the workspace's admission lock ([`WorkspaceLease`]); this look only adds
/// what holds no lease.
#[cfg(target_os = "linux")]
pub fn directory_occupancy(directory: &Path) -> io::Result<DirectoryOccupancy> {
    let directory = fs::canonicalize(directory)?;
    let me = crate::current_uid().to_string();
    let mut found = DirectoryOccupancy::default();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        match process_inside(&entry.path(), &directory) {
            Ok(true) => found.pids.push(pid),
            Ok(false) => {}
            Err(error) if exited(&error) => {}
            Err(error) => {
                // Another user's process is outside this check's reach
                // (documented); one of this user's is reported.
                let ours = fs::read_to_string(entry.path().join("status"))
                    .ok()
                    .and_then(|status| {
                        status
                            .lines()
                            .find_map(|line| line.strip_prefix("Uid:"))
                            .map(|ids| ids.split_whitespace().any(|id| id == me))
                    });
                if ours != Some(false) {
                    found.uninspected.push(format!("process {pid}: {error}"));
                }
            }
        }
    }
    Ok(found)
}

/// Whether a `/proc/<pid>/maps` snapshot maps `directory` or anything below
/// it, without constructing a path for every entry.
#[cfg(target_os = "linux")]
fn maps_inside(maps: &[u8], directory: &Path) -> bool {
    maps.split(|byte| *byte == b'\n').any(|line| {
        line.iter()
            .position(|byte| *byte == b'/')
            .is_some_and(|at| Path::new(OsStr::from_bytes(&line[at..])).starts_with(directory))
    })
}

#[cfg(target_os = "linux")]
enum SharedMaps {
    Unread,
    Outside,
    Fallback,
    Failed,
}

/// Whether any thread of the process at `/proc/<pid>` works under
/// `directory`. Every thread's `cwd`, `root` and open descriptors are checked
/// first so a restricted `/proc/<pid>/maps` cannot bypass `WORKSPACE_BUSY`;
/// when maps inspection is needed, `/proc/<pid>/maps` is read once per
/// process and reused across threads, falling back per task only if the
/// process-level file disappears mid-scan.
#[cfg(target_os = "linux")]
fn process_inside(base: &Path, directory: &Path) -> io::Result<bool> {
    let tasks = fs::read_dir(base.join("task"))?.map(|task| task.map(|entry| entry.path()));
    process_inside_tasks(base, tasks, directory)
}

#[cfg(target_os = "linux")]
fn process_inside_tasks(
    base: &Path,
    tasks: impl IntoIterator<Item = io::Result<PathBuf>>,
    directory: &Path,
) -> io::Result<bool> {
    let inside = |link: &Path| -> io::Result<bool> {
        match fs::read_link(link) {
            Ok(to) => Ok(to.starts_with(directory)),
            Err(error) if exited(&error) => Ok(false),
            Err(error) => Err(error),
        }
    };
    let mut shared_maps = SharedMaps::Unread;
    let mut maps_error = None;
    for task in tasks {
        let task = match task {
            Ok(task) => task,
            Err(error) if exited(&error) => continue,
            Err(error) => return Err(error),
        };
        if inside(&task.join("cwd"))? || inside(&task.join("root"))? {
            return Ok(true);
        }
        let descriptors = match fs::read_dir(task.join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error) if exited(&error) => continue,
            Err(error) => return Err(error),
        };
        for descriptor in descriptors {
            let descriptor = match descriptor {
                Ok(descriptor) => descriptor,
                Err(error) if exited(&error) => continue,
                Err(error) => return Err(error),
            };
            if inside(&descriptor.path())? {
                return Ok(true);
            }
        }
        if matches!(shared_maps, SharedMaps::Unread) {
            shared_maps = match fs::read(base.join("maps")) {
                Ok(maps) => {
                    if maps_inside(&maps, directory) {
                        return Ok(true);
                    }
                    SharedMaps::Outside
                }
                Err(error) if exited(&error) => SharedMaps::Fallback,
                Err(error) => {
                    maps_error.get_or_insert(error);
                    SharedMaps::Failed
                }
            };
        }
        if matches!(shared_maps, SharedMaps::Fallback) {
            match fs::read(task.join("maps")) {
                Ok(maps) if maps_inside(&maps, directory) => return Ok(true),
                Ok(_) => {}
                Err(error) if exited(&error) => {}
                Err(error) => {
                    maps_error.get_or_insert(error);
                }
            }
        }
    }
    if let Some(error) = maps_error {
        return Err(error);
    }
    Ok(false)
}

/// The processes with a file open (or their working directory) under
/// `directory`, as `lsof +D` reports them. `lsof` exits 1 when it found
/// nothing; any other failure, or an exit 1 with a diagnostic, is an error
/// that quotes it, so a caller fails closed. Warnings of a run that
/// succeeded are kept in [`DirectoryOccupancy::uninspected`].
#[cfg(not(target_os = "linux"))]
pub fn directory_occupancy(directory: &Path) -> io::Result<DirectoryOccupancy> {
    let output = Command::new("lsof")
        .args(["-t", "+D"])
        .arg(directory)
        .stdin(Stdio::null())
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let nothing = output.status.code() == Some(1) && output.stdout.is_empty() && stderr.is_empty();
    if !output.status.success() && !nothing {
        return Err(io::Error::other(format!(
            "lsof +D {} failed ({}): {stderr}",
            directory.display(),
            output.status
        )));
    }
    let mut found = DirectoryOccupancy::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let pid = line
            .trim()
            .parse()
            .map_err(|_| io::Error::other(format!("lsof printed {line:?}")))?;
        found.pids.push(pid);
    }
    found
        .uninspected
        .extend(stderr.lines().map(|line| format!("lsof: {line}")));
    Ok(found)
}

/// How long [`WorkspaceLease`] retries a lock that seems held before it takes
/// the holder for real.
const ADMISSION_PATIENCE: Duration = Duration::from_secs(1);

/// A workspace's admission lock: the `flock` of
/// `<parent>/.cow/locks/<name>.admission` for the workspace
/// `<parent>/<name>` ([`WorkspaceLease::lock_path`]), the same file a
/// [`CowWorkspace`] uses for its branch `<name>` under its root `<parent>`.
/// The lock is found from the workspace, never named by the caller, so the
/// guard of one workspace is never taken for another's.
///
/// A managed worker takes the shared side ([`WorkspaceLease::lease`]) before it
/// starts and holds it for its whole life (its supervisor keeps the file
/// open while any of its processes runs). Replacing or unmounting the
/// workspace, and deduplicating its artifacts ([`crate::cas_dedup`]), take
/// the exclusive side without waiting ([`WorkspaceLease::exclusive`]) and hold
/// it throughout: while a lease is held they are refused, and while they
/// run no worker is admitted. Users outside that lifecycle (an IDE, a
/// shell) are expected to be stopped first; the process-table look
/// ([`directory_occupancy`]) only adds a snapshot of them.
#[derive(Debug)]
pub struct WorkspaceLease {
    _file: fs::File,
    exclusive: bool,
    /// The workspace guarded: its parent resolved through its links.
    workspace: PathBuf,
    /// `(device, inode)` of the workspace directory when the lock was
    /// taken; `None` when it was not a directory then.
    generation: Option<(u64, u64)>,
}

impl WorkspaceLease {
    /// The workspace `<parent>/<name>` with its parent resolved, and its
    /// admission lock.
    pub fn lock_path(workspace: &Path) -> io::Result<(PathBuf, PathBuf)> {
        let name = workspace
            .file_name()
            .filter(|name| !name.as_bytes().starts_with(b"."))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} names no workspace", workspace.display()),
                )
            })?;
        let parent = match workspace.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let parent = fs::canonicalize(parent)?;
        let mut lock_name = name.to_os_string();
        lock_name.push(".admission");
        let lock = parent.join(COW_DIRECTORY).join("locks").join(lock_name);
        Ok((parent.join(name), lock))
    }

    fn take(
        workspace: &Path,
        exclusive: bool,
        lock: impl Fn(&fs::File) -> Result<(), fs::TryLockError>,
    ) -> io::Result<Option<Self>> {
        let (workspace, path) = Self::lock_path(workspace)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        if !Self::try_patiently(&file, lock)? {
            return Ok(None);
        }
        let generation = match fs::symlink_metadata(&workspace) {
            Ok(meta) if meta.is_dir() => Some((meta.dev(), meta.ino())),
            Ok(_) => None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Some(Self {
            _file: file,
            exclusive,
            workspace,
            generation,
        }))
    }

    /// `try` on `file` until it takes the lock or [`ADMISSION_PATIENCE`]
    /// has passed: a lock held only through a descriptor a child of this
    /// (or another) process inherited between `fork` and `exec` is let go
    /// within moments, and is no holder. Whether it was taken.
    fn try_patiently(
        file: &fs::File,
        lock: impl Fn(&fs::File) -> Result<(), fs::TryLockError>,
    ) -> io::Result<bool> {
        let deadline = std::time::Instant::now() + ADMISSION_PATIENCE;
        loop {
            match lock(file) {
                Ok(()) => return Ok(true),
                Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(fs::TryLockError::WouldBlock) => return Ok(false),
                Err(fs::TryLockError::Error(error)) => return Err(error),
            }
        }
    }

    /// A worker's shared lease on `workspace`;
    /// [`io::ErrorKind::WouldBlock`] while a teardown holds the exclusive
    /// side.
    pub fn lease(workspace: &Path) -> io::Result<Self> {
        Self::take(workspace, false, fs::File::try_lock_shared)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is being retired or deduplicated", workspace.display()),
            )
        })
    }

    /// The exclusive side of `workspace`'s lock, without waiting on a
    /// holder: `None` while any lease (or another exclusive holder) has
    /// it.
    pub fn exclusive(workspace: &Path) -> io::Result<Option<Self>> {
        Self::take(workspace, true, fs::File::try_lock)
    }

    /// The workspace this guards (its parent resolved through its links).
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Whether the exclusive side is held for the directory that is the
    /// workspace now (the generation the lock was taken for, not one put
    /// in its place since). [`Self::is_exclusive`] plus that generation
    /// check: what an offline pass (`cas_dedup`) must ask before it writes.
    pub fn holds_exclusive(&self) -> bool {
        self.exclusive
            && self.generation.is_some()
            && fs::symlink_metadata(&self.workspace).is_ok_and(|meta| {
                meta.is_dir() && Some((meta.dev(), meta.ino())) == self.generation
            })
    }

    /// Whether this is the exclusive side (no worker can be admitted),
    /// whatever directory is at the workspace path now; see
    /// [`Self::holds_exclusive`] for the check bound to the generation.
    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }
}

// ---------------------------------------------------------------------------
// Tree checks and descriptor-relative writes
// ---------------------------------------------------------------------------

/// Whether a path component names `.git` in a spelling git refuses on some
/// file system: any case, with trailing dots or spaces (NTFS), its 8.3 name
/// `git~1`, or with the code points HFS+ ignores.
fn is_dotgit(name: &[u8]) -> bool {
    const IGNORED: [std::ops::RangeInclusive<char>; 4] = [
        '\u{200C}'..='\u{200F}',
        '\u{202A}'..='\u{202E}',
        '\u{206A}'..='\u{206F}',
        '\u{FEFF}'..='\u{FEFF}',
    ];
    let mut folded: Vec<u8> = match std::str::from_utf8(name) {
        Ok(text) => text
            .chars()
            .filter(|character| !IGNORED.iter().any(|range| range.contains(character)))
            .collect::<String>()
            .into_bytes(),
        Err(_) => name.to_vec(),
    };
    folded.make_ascii_lowercase();
    while folded
        .last()
        .is_some_and(|byte| matches!(byte, b'.' | b' '))
    {
        folded.pop();
    }
    folded == b".git" || folded == b"git~1"
}

/// Whether one component of a tree path could leave or corrupt its tree.
fn unsafe_component(name: &[u8]) -> bool {
    matches!(name, b"" | b"." | b"..") || name.contains(&0) || is_dotgit(name)
}

/// Refuse a tree listing that could write outside its directory: a bad
/// component or mode, a duplicate path, or a path below another entry (a
/// symlink, file or gitlink as a leading directory), compared case-folded
/// so a case-insensitive volume cannot be tricked either.
fn check_tree(entries: &[(u32, String, Vec<u8>)]) -> Result<(), WorkspaceError> {
    let refused = |path: &[u8], why: &str| WorkspaceError::Refused {
        code: UNSAFE_TREE.to_string(),
        message: format!("{why}: {:?}", String::from_utf8_lossy(path)),
    };
    let mut exact = BTreeSet::new();
    let mut folded = BTreeSet::new();
    for (mode, _, path) in entries {
        if !MODES.contains(mode) {
            return Err(refused(path, "unexpected mode"));
        }
        if path.split(|byte| *byte == b'/').any(unsafe_component) {
            return Err(refused(path, "unsafe path component"));
        }
        if !exact.insert(path.as_slice()) {
            return Err(refused(path, "duplicate path"));
        }
        folded.insert(path.to_ascii_lowercase());
    }
    for (_, _, path) in entries {
        let lower = path.to_ascii_lowercase();
        for (at, _) in lower.iter().enumerate().filter(|(_, byte)| **byte == b'/') {
            if folded.contains(&lower[..at]) {
                return Err(refused(path, "path below another entry"));
            }
        }
    }
    Ok(())
}

/// A directory held by descriptor. Every path below it is walked one
/// component at a time with `O_NOFOLLOW | O_DIRECTORY`, so no symlink (nor
/// a `..`) on the way is ever followed, and a leaf is made with `O_EXCL |
/// O_NOFOLLOW`.
struct DirFd(OwnedFd);

const DIRECTORY_MODE: Mode = Mode::RWXU
    .union(Mode::RGRP)
    .union(Mode::XGRP)
    .union(Mode::ROTH)
    .union(Mode::XOTH);

const FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::ROTH);

fn split_leaf(path: &[u8]) -> (&[u8], &OsStr) {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(at) => (&path[..at], OsStr::from_bytes(&path[at + 1..])),
        None => (b"", OsStr::from_bytes(path)),
    }
}

impl DirFd {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self(rustix::fs::open(
            path,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    }

    /// The directory that holds `path`'s last component (made, one level at
    /// a time, when `create`), and that component.
    fn parent<'p>(&self, path: &'p [u8], create: bool) -> io::Result<(OwnedFd, &'p OsStr)> {
        let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC;
        let (directories, leaf) = split_leaf(path);
        let mut current = rustix::fs::openat(&self.0, ".", flags, Mode::empty())?;
        if !directories.is_empty() {
            for name in directories.split(|byte| *byte == b'/') {
                let name = OsStr::from_bytes(name);
                current = match rustix::fs::openat(&current, name, flags, Mode::empty()) {
                    Ok(next) => next,
                    Err(Errno::NOENT) if create => {
                        match rustix::fs::mkdirat(&current, name, DIRECTORY_MODE) {
                            Ok(()) | Err(Errno::EXIST) => {}
                            Err(error) => return Err(error.into()),
                        }
                        rustix::fs::openat(&current, name, flags, Mode::empty())?
                    }
                    Err(error) => return Err(error.into()),
                };
            }
        }
        Ok((current, leaf))
    }

    /// A new regular file at `path`, open for writing.
    fn create_file(&self, path: &[u8]) -> io::Result<fs::File> {
        let (directory, leaf) = self.parent(path, true)?;
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW;
        Ok(fs::File::from(rustix::fs::openat(
            &directory,
            leaf,
            flags | OFlags::CLOEXEC,
            FILE_MODE,
        )?))
    }

    /// The regular file at `path`, open for reading; anything else (a
    /// symlink, a FIFO) is refused without blocking.
    fn open_file(&self, path: &[u8]) -> io::Result<fs::File> {
        let (directory, leaf) = self.parent(path, false)?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = fs::File::from(rustix::fs::openat(&directory, leaf, flags, Mode::empty())?);
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        Ok(file)
    }

    fn symlink(&self, path: &[u8], target: &[u8]) -> io::Result<()> {
        let (directory, leaf) = self.parent(path, true)?;
        Ok(rustix::fs::symlinkat(
            OsStr::from_bytes(target),
            &directory,
            leaf,
        )?)
    }

    fn read_link(&self, path: &[u8]) -> io::Result<Vec<u8>> {
        let (directory, leaf) = self.parent(path, false)?;
        Ok(rustix::fs::readlinkat(&directory, leaf, Vec::new())?.into_bytes())
    }

    fn make_dir(&self, path: &[u8]) -> io::Result<()> {
        let (directory, leaf) = self.parent(path, true)?;
        Ok(rustix::fs::mkdirat(&directory, leaf, DIRECTORY_MODE)?)
    }
}

/// Clone the baseline file `path` of `from` into the new file `path` of
/// `to`, never through a link on either side ([`clone_at`]).
fn clone_entry(from: &DirFd, to: &DirFd, path: &[u8]) -> io::Result<(fs::File, CloneKind)> {
    let source = from.open_file(path)?;
    let (directory, leaf) = to.parent(path, true)?;
    clone_at(&source, &directory, leaf, true)
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// What `lstat` said of a file when its blob id was known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            size: meta.size(),
            ino: meta.ino(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// The later of the stamp's mtime and ctime.
    fn newest(&self) -> (i64, i64) {
        self.mtime.max(self.ctime)
    }

    /// Whether the stamp is not older than the manifest's fence, so a later
    /// change could have left it as it is.
    fn racy(&self, fence: (i64, i64)) -> bool {
        self.newest() >= fence
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    mode: u32,
    sha: String,
    stamp: Stamp,
}

/// A workspace's head and manifest (`<root>/.cow/state/<branch>`).
#[derive(Debug, Clone, Default)]
struct State {
    head: String,
    /// A commit the branch is being moved to: written before the
    /// compare-and-swap, so a state that could not be saved after it still
    /// knows the branch head is its own.
    next: Option<String>,
    /// The fence: a file-system time no trusted stamp reaches.
    recorded: (i64, i64),
    /// The mounted generation: `(device, inode)` of the directory this
    /// mount installed. Only a workspace directory that is still that one
    /// is this store's to check, replace or delete.
    mounted: (u64, u64),
    entries: BTreeMap<Vec<u8>, Entry>,
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

impl State {
    fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "cow-state 3\nhead {}\nnext {}\nrecorded {} {}\nmounted {} {}\n",
            self.head,
            self.next.as_deref().unwrap_or("-"),
            self.recorded.0,
            self.recorded.1,
            self.mounted.0,
            self.mounted.1
        );
        for (path, entry) in &self.entries {
            let stamp = entry.stamp;
            let _ = writeln!(
                out,
                "{:o} {} {} {} {} {} {} {} {}",
                entry.mode,
                entry.sha,
                stamp.size,
                stamp.ino,
                stamp.mtime.0,
                stamp.mtime.1,
                stamp.ctime.0,
                stamp.ctime.1,
                hex(path)
            );
        }
        out
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        (lines.next()? == "cow-state 3").then_some(())?;
        let head = lines.next()?.strip_prefix("head ")?.to_string();
        let next = match lines.next()?.strip_prefix("next ")? {
            "-" => None,
            next => Some(next.to_string()),
        };
        let mut recorded = lines.next()?.strip_prefix("recorded ")?.split(' ');
        let recorded = (
            recorded.next()?.parse().ok()?,
            recorded.next()?.parse().ok()?,
        );
        let mut mounted = lines.next()?.strip_prefix("mounted ")?.split(' ');
        let mounted = (mounted.next()?.parse().ok()?, mounted.next()?.parse().ok()?);
        let mut entries = BTreeMap::new();
        for line in lines {
            let fields: Vec<&str> = line.split(' ').collect();
            let [mode, sha, size, ino, ms, mns, cs, cns, path] = fields.as_slice() else {
                return None;
            };
            entries.insert(
                unhex(path)?,
                Entry {
                    mode: u32::from_str_radix(mode, 8).ok()?,
                    sha: (*sha).to_string(),
                    stamp: Stamp {
                        size: size.parse().ok()?,
                        ino: ino.parse().ok()?,
                        mtime: (ms.parse().ok()?, mns.parse().ok()?),
                        ctime: (cs.parse().ok()?, cns.parse().ok()?),
                    },
                },
            );
        }
        Some(Self {
            head,
            next,
            recorded,
            mounted,
            entries,
        })
    }
}

/// The file system's clock now, as `directory`'s file system keeps it: a
/// probe file is written there and its ctime read. File times come from the
/// kernel's (coarse) clock, not from `SystemTime::now`, so only a time read
/// this way can be compared with them.
fn fence(directory: &Path) -> io::Result<(i64, i64)> {
    let probe = directory.join(format!(".fence-{}", unique_suffix()));
    fs::write(&probe, b"")?;
    let meta = fs::symlink_metadata(&probe);
    let _ = fs::remove_file(&probe);
    let meta = meta?;
    Ok((meta.ctime(), meta.ctime_nsec()).max((meta.mtime(), meta.mtime_nsec())))
}

/// A fence strictly past `newest`: the file system's clock is read until it
/// has moved beyond it (a tick on Linux, never more than the file system's
/// time granularity).
fn fence_after(directory: &Path, newest: (i64, i64)) -> io::Result<(i64, i64)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let now = fence(directory)?;
        if now > newest || std::time::Instant::now() > deadline {
            return Ok(now);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// One file of a workspace as a checkpoint found it.
struct Found {
    path: Vec<u8>,
    absolute: PathBuf,
    mode: u32,
    stamp: Stamp,
}

/// Whether a checkpoint takes `path` from git's listing, by this store's
/// rule on top of git's: never a nested repository (`dir/`), a path inside a
/// tracked gitlink, or an untracked path inside a [`SKIPPED_DIRECTORIES`]
/// name.
fn taken(path: &[u8], tracked: &BTreeMap<Vec<u8>, Entry>) -> bool {
    if path.ends_with(b"/") {
        return false;
    }
    let mut leading = path.len();
    while let Some(slash) = path[..leading].iter().rposition(|byte| *byte == b'/') {
        if tracked
            .get(&path[..slash])
            .is_some_and(|entry| entry.mode == 0o160_000)
        {
            return false;
        }
        leading = slash;
    }
    tracked.contains_key(path)
        || !path.split(|byte| *byte == b'/').any(|name| {
            SKIPPED_DIRECTORIES
                .iter()
                .any(|skip| name == skip.as_bytes())
        })
}

/// The files of `directory` at `paths` (what git's rules let a checkpoint
/// take, [`GitWorkspace::checkpoint_paths`]): regular files and symlinks,
/// and a directory at a path `tracked` holds as a gitlink. An untracked path
/// inside a [`SKIPPED_DIRECTORIES`] name, inside a tracked gitlink, or that
/// git lists as a nested repository (`dir/`) is left out, as are special
/// files and tracked paths gone from the work tree (their deletion).
fn found_files(
    directory: &Path,
    paths: &[crate::tree_path::TreePath],
    tracked: &BTreeMap<Vec<u8>, Entry>,
) -> io::Result<Vec<Found>> {
    let mut out = Vec::new();
    for path in paths {
        let path = path.as_bytes();
        if !taken(path, tracked) {
            continue;
        }
        let known = tracked.get(path);
        let absolute = directory.join(OsStr::from_bytes(path));
        let meta = match fs::symlink_metadata(&absolute) {
            Ok(meta) => meta,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let kind = meta.file_type();
        let mode = if kind.is_dir() {
            if known.is_none_or(|entry| entry.mode != 0o160_000) {
                continue;
            }
            0o160_000
        } else if kind.is_symlink() {
            0o120_000
        } else if kind.is_file() {
            if meta.mode() & 0o111 == 0 {
                0o100_644
            } else {
                0o100_755
            }
        } else {
            continue;
        };
        out.push(Found {
            path: path.to_vec(),
            absolute,
            mode,
            stamp: Stamp::of(&meta),
        });
    }
    Ok(out)
}

/// A checkpoint's tree, the state that goes with it, and the files over the
/// size cap it left out.
type Snapshot = (String, State, Vec<(crate::tree_path::TreePath, u64)>);

/// What the last [`WorkspaceStore::commit`] or [`WorkspaceStore::mount`]
/// of a [`CowWorkspace`] did, for tests and benchmarks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CowStats {
    /// Files whose blob id came from the manifest (no read).
    pub reused: usize,
    /// Files read and hashed.
    pub hashed: usize,
    /// Files a mount cloned copy-on-write.
    pub reflinked: usize,
    /// Files a mount had to copy (no clone on this file system).
    pub copied: usize,
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// A [`WorkspaceStore`] over a bare git repository whose workspaces are
/// copy-on-write clones of one read-only baseline per commit (see the
/// module documentation).
///
/// The default store stays [`crate::GitWorkspace`]; this one is opt-in
/// (`SPAWN_WORKSPACE_BACKEND=cow` or `auto` on a file system that clones,
/// through [`crate::LocalWorkspaceStore`]), on a root of its own: a root
/// runs one engine ([`crate::claim_root`]). Its git plumbing, branch
/// creation and resolution are the Git store's ([`GitWorkspace`] on the same
/// repository), and so are the rules of what a checkpoint takes: git's
/// ignore rules, the engine's runtime files and the size cap
/// (`SPAWN_MAX_FILE_MB`) with its notices (`GitWorkspace::checkpoint_paths`).
/// Its set-aside is the Git store's rule, moving the workspace in place
/// ([`CowWorkspace::set_aside_when`]). Not implemented here (the trait's
/// defaults answer `Current`): merging `main` into a workspace.
#[derive(Debug)]
pub struct CowWorkspace {
    git: GitWorkspace,
    root: PathBuf,
    last: Mutex<CowStats>,
    /// What the last operation could not do or inspect but did not need
    /// to fail for (an uninspectable process, a cleanup left pending).
    diagnostics: Mutex<Vec<String>>,
    /// Fault injection for the rollback tests.
    #[cfg(test)]
    faults: Faults,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct Faults {
    /// The final rename of a replacing mount fails.
    swap: std::sync::atomic::AtomicBool,
    /// Saving the state after a moved branch fails.
    save_after_move: std::sync::atomic::AtomicBool,
    /// Saving the state after a mount's final rename fails.
    save_after_swap: std::sync::atomic::AtomicBool,
    /// Writing the first file of a baseline fails.
    baseline: std::sync::atomic::AtomicBool,
    /// A replacing mount stops dead (as an interrupted process would) after
    /// it carried `.transcripts/` and `.spawn/`, before its final rename.
    interrupted: std::sync::atomic::AtomicBool,
}

fn unavailable(what: impl std::fmt::Display) -> WorkspaceError {
    WorkspaceError::Unavailable(what.to_string())
}

fn io_error(what: &str, path: &Path) -> impl FnOnce(io::Error) -> WorkspaceError {
    let path = path.display().to_string();
    let what = what.to_string();
    move |error| unavailable(format!("{what} {path}: {error}"))
}

fn lock_error(path: &Path) -> impl FnOnce(io::Error) -> WorkspaceError {
    io_error("lock", path)
}

fn busy(message: String) -> WorkspaceError {
    WorkspaceError::Refused {
        code: WORKSPACE_BUSY.to_string(),
        message,
    }
}

fn holds_transcripts(directory: &Path, count: usize) -> WorkspaceError {
    WorkspaceError::Refused {
        code: WORKSPACE_HOLDS_TRANSCRIPTS.to_string(),
        message: format!(
            "{} holds {count} *.jsonl file(s) outside its .transcripts/; nothing was deleted",
            directory.display()
        ),
    }
}

fn unreadable(directory: &Path, error: &io::Error) -> WorkspaceError {
    WorkspaceError::Refused {
        code: WORKSPACE_UNREADABLE.to_string(),
        message: format!(
            "{} cannot be read whole ({error}); nothing was deleted",
            directory.display()
        ),
    }
}

/// `path`'s `lstat`, `None` only when nothing is there: any other failure
/// is an error, never taken for "absent".
fn present(path: &Path) -> Result<Option<fs::Metadata>, WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("stat", path)(error)),
    }
}

/// Remove the file `path`; one already gone is removed.
fn remove_file(path: &Path) -> Result<(), WorkspaceError> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(io_error("remove", path)(error))
        }
        _ => Ok(()),
    }
}

/// The state at `path`; `None` only when there is none. A state that cannot
/// be read or parsed is an error: nothing is decided on a guess.
fn read_state(path: &Path) -> Result<Option<State>, WorkspaceError> {
    match fs::read_to_string(path) {
        Ok(text) => State::parse(&text).map(Some).ok_or_else(|| {
            unavailable(format!(
                "{} is not a state this store can read; nothing changed",
                path.display()
            ))
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("read", path)(error)),
    }
}

/// Write `state` to `path` beside it and rename it into place.
fn write_state(path: &Path, state: &State) -> Result<(), WorkspaceError> {
    write_whole(path, &state.render())
}

/// Write `text` to `path` beside it, synced, and rename it into place.
fn write_whole(path: &Path, text: &str) -> Result<(), WorkspaceError> {
    let directory = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    fs::create_dir_all(&directory).map_err(io_error("create", &directory))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staged = directory.join(format!(".{name}.{}", unique_suffix()));
    fs::write(&staged, text)
        .and_then(|()| fs::File::open(&staged)?.sync_all())
        .map_err(io_error("write", &staged))?;
    fs::rename(&staged, path).map_err(io_error("rename", path))
}

/// Where a mount's trees are while it swaps (`.cow/swaps/<branch>`): the
/// new tree's staging directory and, for a replacement, where the old
/// workspace goes. Written before either tree moves and removed once the
/// swap is settled, so a mount interrupted in between is undone or
/// completed by the next look ([`CowWorkspace::settled_state`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Swap {
    staging: PathBuf,
    trash: Option<PathBuf>,
}

impl Swap {
    fn render(&self) -> String {
        let trash = self.trash.as_ref().map_or_else(
            || "-".to_string(),
            |trash| hex(trash.as_os_str().as_bytes()),
        );
        format!(
            "cow-swap 1\nstaging {}\ntrash {trash}\n",
            hex(self.staging.as_os_str().as_bytes())
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        (lines.next()? == "cow-swap 1").then_some(())?;
        let path = |hexed: &str| unhex(hexed).map(|bytes| PathBuf::from(OsStr::from_bytes(&bytes)));
        let staging = path(lines.next()?.strip_prefix("staging ")?)?;
        let trash = match lines.next()?.strip_prefix("trash ")? {
            "-" => None,
            hexed => Some(path(hexed)?),
        };
        Some(Self { staging, trash })
    }
}

/// Whether `meta` is the directory the mount of `state` installed.
fn is_generation(meta: &fs::Metadata, state: &State) -> bool {
    meta.is_dir() && (meta.dev(), meta.ino()) == state.mounted
}

// ---------------------------------------------------------------------------
// Set-aside: a workspace moved in place
// ---------------------------------------------------------------------------

/// The refusal code of a set-aside whose workspace cannot be moved to the
/// start's tree as it is (a file no checkpoint took where the start needs a
/// directory, a file that changed since the checkpoint): nothing was
/// changed, neither the directory nor the branch, as the Git store's
/// `read-tree -m -u` refusal.
pub const WORKSPACE_DIFFERS: &str = "WorkspaceDiffers";

fn differs(message: String) -> WorkspaceError {
    WorkspaceError::Refused {
        code: WORKSPACE_DIFFERS.to_string(),
        message,
    }
}

/// A tree path's mode and blob.
pub(crate) type TreeEntries = BTreeMap<Vec<u8>, (u32, String)>;

/// The paths a move takes away, each with its old mode and blob.
pub(crate) type Going = [(Vec<u8>, u32, String)];

/// What checks, before a move writes anything, that every path it takes
/// away is still the checkpoint's ([`apply_move`]).
pub(crate) type Verify<'a> = dyn Fn(&Going) -> Result<(), WorkspaceError> + 'a;

/// What moving a workspace from one tree to another takes ([`plan_move`]),
/// checked before anything is written.
#[derive(Debug, Default)]
pub(crate) struct MovePlan {
    /// Paths of the old tree that go (or change), with their old mode and
    /// blob, deepest first.
    pub(crate) remove: Vec<(Vec<u8>, u32, String)>,
    /// Entries of the new tree that are written.
    pub(crate) write: Vec<(u32, String, Vec<u8>)>,
}

/// What [`apply_move`] did to a workspace, by tree path.
#[derive(Debug, Default)]
pub(crate) struct Moved {
    /// Paths written from the new tree (files, links, gitlink directories).
    pub(crate) written: std::collections::HashSet<Vec<u8>>,
    /// Paths of the old tree removed.
    pub(crate) removed: Vec<Vec<u8>>,
    /// Paths in the way of the new tree (no file of the old tree), moved,
    /// bytes untouched, under the `aside` prefix.
    pub(crate) aside: Vec<Vec<u8>>,
}

/// Whether `name` exists in `directory`, never through a link.
fn stat_at(directory: &impl AsFd, name: &OsStr) -> io::Result<Option<rustix::fs::Stat>> {
    match rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn is_directory(stat: &rustix::fs::Stat) -> bool {
    rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Directory
}

/// The proper prefixes of a tree path, shortest first (`a`, `a/b` of
/// `a/b/c`).
fn prefixes(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(at, _)| &path[..at])
}

/// Check, without writing anything, that the workspace `workspace`, whose
/// committed tree is `old`, can be moved in place to the tree `new`, and
/// say what that takes. Refused ([`WORKSPACE_DIFFERS`]) when something no
/// checkpoint took (a file over the size cap, a special file, an ignored
/// file) is where `new` needs a directory, or when `.spawn/set-aside`
/// (where what is in the way of a file goes) is not a directory: the move
/// would stop half way. Something at the very path of a file of `new` is
/// not a refusal: [`apply_move`] moves it aside.
pub(crate) fn plan_move(
    workspace: &Path,
    old: &TreeEntries,
    new: &[(u32, String, Vec<u8>)],
) -> Result<MovePlan, WorkspaceError> {
    check_tree(new)?;
    let to = DirFd::open(workspace).map_err(io_error("open", workspace))?;
    let wanted: std::collections::HashMap<&[u8], (u32, &str)> = new
        .iter()
        .map(|(mode, sha, path)| (path.as_slice(), (*mode, sha.as_str())))
        .collect();
    let mut plan = MovePlan::default();
    for (path, (mode, sha)) in old.iter().rev() {
        if wanted.get(path.as_slice()) != Some(&(*mode, sha.as_str())) {
            plan.remove.push((path.clone(), *mode, sha.clone()));
        }
    }
    let removed: std::collections::HashSet<&[u8]> = plan
        .remove
        .iter()
        .map(|(path, _, _)| path.as_slice())
        .collect();
    let mut checked: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    let shown = |path: &[u8]| workspace.join(OsStr::from_bytes(path));
    for (mode, sha, path) in new {
        if old.get(path) == Some(&(*mode, sha.clone())) {
            continue;
        }
        plan.write.push((*mode, sha.clone(), path.clone()));
        for prefix in prefixes(path) {
            if !checked.insert(prefix) || removed.contains(prefix) {
                continue;
            }
            let (directory, leaf) = match to.parent(prefix, false) {
                Ok(found) => found,
                // A missing directory is made; one under a file is caught
                // at that file.
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) =>
                {
                    break;
                }
                Err(error) => return Err(io_error("open", &shown(prefix))(error)),
            };
            match stat_at(&directory, leaf).map_err(io_error("stat", &shown(prefix)))? {
                None => break,
                Some(there) if is_directory(&there) => {}
                Some(_) => {
                    return Err(differs(format!(
                        "{} is a file no checkpoint took, where {} needs a directory: \
                         nothing was changed",
                        shown(prefix).display(),
                        String::from_utf8_lossy(path)
                    )));
                }
            }
        }
    }
    for prefix in [&b".spawn"[..], b".spawn/set-aside"] {
        if let Ok((directory, leaf)) = to.parent(prefix, false)
            && let Some(there) =
                stat_at(&directory, leaf).map_err(io_error("stat", &shown(prefix)))?
            && !is_directory(&there)
        {
            return Err(differs(format!(
                "{} is not a directory, so nothing in the way can be set aside there: \
                 nothing was changed",
                shown(prefix).display()
            )));
        }
    }
    Ok(plan)
}

/// Move `name` of `from` to `name` of `into`, never over something there.
fn rename_no_replace(
    from: &impl AsFd,
    leaf: &OsStr,
    into: &impl AsFd,
    name: &OsStr,
) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
    {
        rustix::fs::renameat_with(from, leaf, into, name, rustix::fs::RenameFlags::NOREPLACE)
            .map_err(Into::into)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
    {
        if stat_at(into, name)?.is_some() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        rustix::fs::renameat(from, leaf, into, name).map_err(Into::into)
    }
}

/// Carry out `plan` ([`plan_move`]) in the workspace `workspace`: `verify`
/// is asked first, with every path that goes (its old mode and blob), and
/// refuses unless each is still the checkpoint's (nothing is written
/// then); then the old paths go, deepest first, with the directories that
/// empties, and the new tree's files are written from `source`, a
/// read-only checkout of it (a Cow baseline, an overlay snapshot layer),
/// cloned copy-on-write where the file system can. Something at such a
/// path that the old tree did not hold (a file no checkpoint took) is
/// never deleted: it makes way, moved (never over anything) to its path
/// under `aside`. Every name is opened relative to a directory descriptor,
/// one component at a time, never through a link.
///
/// The set-aside of [`CowWorkspace`] and `OverlayWorkspace` (through the
/// holder's view of its merged directory) write their workspace this way,
/// the two-tree move the Git store makes with `read-tree -m -u`.
pub(crate) fn apply_move(
    workspace: &Path,
    source: &Path,
    plan: &MovePlan,
    aside: &[u8],
    verify: &Verify<'_>,
) -> Result<Moved, WorkspaceError> {
    verify(&plan.remove)?;
    let to = DirFd::open(workspace).map_err(io_error("open", workspace))?;
    let from = DirFd::open(source).map_err(io_error("open", source))?;
    let shown = |path: &[u8]| workspace.join(OsStr::from_bytes(path));
    let mut moved = Moved::default();

    for (path, mode, _) in &plan.remove {
        let (directory, leaf) = match to.parent(path, false) {
            Ok(found) => found,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
                continue;
            }
            Err(error) => return Err(io_error("open", &shown(path))(error)),
        };
        let flags = if *mode == 0o160_000 {
            rustix::fs::AtFlags::REMOVEDIR
        } else {
            rustix::fs::AtFlags::empty()
        };
        match rustix::fs::unlinkat(&directory, leaf, flags) {
            // A gitlink directory with something in it stays.
            Ok(()) | Err(Errno::NOENT | Errno::NOTEMPTY | Errno::EXIST) => {}
            Err(error) => return Err(io_error("remove", &shown(path))(error.into())),
        }
        moved.removed.push(path.clone());
        // Directories the removal emptied go too; one with anything left
        // in it (an ignored file, a build output) stays.
        let mut parent = split_leaf(path).0;
        while !parent.is_empty() {
            let Ok((directory, leaf)) = to.parent(parent, false) else {
                break;
            };
            if rustix::fs::unlinkat(&directory, leaf, rustix::fs::AtFlags::REMOVEDIR).is_err() {
                break;
            }
            parent = split_leaf(parent).0;
        }
    }

    for (mode, _, path) in &plan.write {
        let (directory, leaf) = to
            .parent(path, true)
            .map_err(io_error("create", &shown(path)))?;
        if let Some(there) = stat_at(&directory, leaf).map_err(io_error("stat", &shown(path)))? {
            if is_directory(&there) && *mode == 0o160_000 {
                moved.written.insert(path.clone());
                continue;
            }
            let emptied = is_directory(&there)
                && rustix::fs::unlinkat(&directory, leaf, rustix::fs::AtFlags::REMOVEDIR).is_ok();
            if !emptied {
                let mut kept = aside.to_vec();
                kept.push(b'/');
                kept.extend_from_slice(path);
                let (into, name) = to
                    .parent(&kept, true)
                    .map_err(io_error("create", &shown(&kept)))?;
                rename_no_replace(&directory, leaf, &into, name)
                    .map_err(io_error("move aside", &shown(path)))?;
                moved.aside.push(path.clone());
            }
        }
        match *mode {
            0o160_000 => rustix::fs::mkdirat(&directory, leaf, DIRECTORY_MODE)
                .map_err(|error| io_error("create", &shown(path))(error.into()))?,
            0o120_000 => {
                let link = from
                    .read_link(path)
                    .map_err(io_error("read", &shown(path)))?;
                rustix::fs::symlinkat(OsStr::from_bytes(&link), &directory, leaf)
                    .map_err(|error| io_error("symlink", &shown(path))(error.into()))?;
            }
            _ => {
                let file = from
                    .open_file(path)
                    .map_err(io_error("read", &shown(path)))?;
                let (written, _) = clone_at(&file, &directory, leaf, true)
                    .map_err(io_error("clone", &shown(path)))?;
                let bits = if *mode == 0o100_755 { 0o755 } else { 0o644 };
                written
                    .set_permissions(fs::Permissions::from_mode(bits))
                    .map_err(io_error("chmod", &shown(path)))?;
            }
        }
        moved.written.insert(path.clone());
    }
    Ok(moved)
}

impl CowWorkspace {
    /// Store over the bare repository `repo`, with workspaces under `root`
    /// (resolved through its symlinks once, here).
    pub fn new(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        let _ = fs::create_dir_all(root);
        let root = crate::root::canonical_root(root);
        Self {
            git: GitWorkspace::new(repo, &root),
            root,
            last: Mutex::new(CowStats::default()),
            diagnostics: Mutex::new(Vec::new()),
            #[cfg(test)]
            faults: Faults::default(),
        }
    }

    /// The same store with a size cap of `bytes` for one file in a
    /// checkpoint ([`GitWorkspace::with_max_file_bytes`]).
    #[must_use]
    pub fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.git = self.git.with_max_file_bytes(bytes);
        self
    }

    /// The resolved root workspaces live under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether this store's root can clone copy-on-write (else every mount
    /// copies).
    pub fn cow_supported(&self) -> bool {
        let directory = self.cow_dir();
        fs::create_dir_all(&directory).is_ok() && cow_clone_supported(&directory)
    }

    /// What the last mount or commit did.
    pub fn last_stats(&self) -> CowStats {
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The read-only baseline tree of `commit`, when one was built.
    pub fn baseline_path(&self, commit: &str) -> PathBuf {
        self.cow_dir().join("baselines").join(commit).join("tree")
    }

    /// What the last mount, commit or unmount could not do or inspect but
    /// did not need to fail for: a process whose links the kernel withheld
    /// ([`DirectoryOccupancy::uninspected`]), a state or a cleanup left for the next
    /// look.
    pub fn diagnostics(&self) -> Vec<String> {
        self.diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn clear_diagnostics(&self) {
        self.diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn note(&self, what: String) {
        self.diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(what);
    }

    /// Remove the baselines no mounted workspace's head names; how many.
    /// Holds the baselines' lock, so no mount clones from one meanwhile. A
    /// state that cannot be read is an error: no baseline is removed on a
    /// guess.
    pub fn prune_baselines(&self) -> io::Result<usize> {
        let lock = self.open_lock("baselines")?;
        lock.lock()?;
        let mut live = BTreeSet::new();
        let states = match fs::read_dir(self.cow_dir().join("state")) {
            Ok(states) => Some(states),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        for state in states.into_iter().flatten() {
            let state = state?;
            if state.file_name().as_bytes().starts_with(b".") {
                // A state being written: its rename makes it whole.
                continue;
            }
            let text = match fs::read_to_string(state.path()) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let state = State::parse(&text).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a state: nothing pruned", state.path().display()),
                )
            })?;
            live.insert(state.head);
            live.extend(state.next);
        }
        let baselines = match fs::read_dir(self.cow_dir().join("baselines")) {
            Ok(baselines) => baselines,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut removed = 0;
        for baseline in baselines {
            let baseline = baseline?;
            let name = baseline.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || live.contains(&name) {
                continue;
            }
            if self.remove_guarded(&baseline.path(), "baseline")? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn cow_dir(&self) -> PathBuf {
        self.root.join(COW_DIRECTORY)
    }

    fn state_path(&self, branch: &str) -> PathBuf {
        self.cow_dir().join("state").join(branch)
    }

    /// The state a mount writes before its final rename: once the rename is
    /// done, it is the workspace's state even if it could not be moved into
    /// place ([`Self::settled_state`]).
    fn swap_path(&self, branch: &str) -> PathBuf {
        self.cow_dir().join("swaps").join(branch)
    }

    fn installing_path(&self, branch: &str) -> PathBuf {
        self.cow_dir()
            .join("state")
            .join(format!("{branch}.installing"))
    }

    /// A managed worker's lease on its workspace ([`WorkspaceLease::lease`]):
    /// taken before the worker starts and held for its life. Refused
    /// ([`WORKSPACE_BUSY`]) while the workspace is being replaced, retired
    /// or deduplicated.
    pub fn lease(&self, branch: &BranchName) -> Result<WorkspaceLease, WorkspaceError> {
        Self::checked_branch(branch.as_str())?;
        let directory = self.root.join(branch.as_str());
        WorkspaceLease::lease(&directory).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                busy(format!("{branch} admits no worker now: {error}"))
            } else {
                io_error("lease", &directory)(error)
            }
        })
    }

    /// The exclusive side of `branch`'s admission lock, or
    /// [`WORKSPACE_BUSY`] while a worker holds a lease.
    fn quiesce(&self, branch: &str) -> Result<WorkspaceLease, WorkspaceError> {
        let directory = self.root.join(branch);
        WorkspaceLease::exclusive(&directory)
            .map_err(io_error("lock", &directory))?
            .ok_or_else(|| {
                busy(format!(
                    "a worker holds {branch}'s admission lease; nothing changed"
                ))
            })
    }

    /// Take `workspace` offline for a deduplication of its build artifacts
    /// ([`crate::cas_dedup::ArtifactRoot`]): it must be this store's
    /// mounted generation, no worker may hold its lease and no process may
    /// be seen in it. The returned guard holds the exclusive side of its
    /// admission lock (no worker starts) until it is dropped.
    pub fn offline(&self, workspace: &WorkspaceId) -> Result<WorkspaceLease, WorkspaceError> {
        let _lock = self.lock_branch(workspace.sandbox())?;
        let directory = self.checked_path(workspace)?;
        let branch = workspace.sandbox();
        let state = self
            .settled_state(branch)?
            .ok_or_else(|| Self::not_owned(&directory, "no mount of this store made it"))?;
        let meta = present(&directory)?
            .ok_or_else(|| unavailable(format!("{} is gone", directory.display())))?;
        Self::owned(&directory, &meta, &state)?;
        let admission = self.quiesce(branch)?;
        self.refuse_busy(&directory)?;
        Ok(admission)
    }

    fn not_owned(directory: &Path, why: &str) -> WorkspaceError {
        WorkspaceError::Refused {
            code: WORKSPACE_NOT_OWNED.to_string(),
            message: format!(
                "{} is not a workspace this store mounted ({why}); nothing changed",
                directory.display()
            ),
        }
    }

    /// Refused unless `meta` (of `directory`) is the generation `state`
    /// recorded: a same-named directory of anyone else (a Git store's
    /// workspace, a directory put back by hand) is never this store's.
    fn owned(directory: &Path, meta: &fs::Metadata, state: &State) -> Result<(), WorkspaceError> {
        if is_generation(meta, state) {
            Ok(())
        } else {
            Err(Self::not_owned(
                directory,
                "it is not the directory its mount installed",
            ))
        }
    }

    fn set_last(&self, tally: CowStats) {
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tally;
    }

    /// `.cow/locks/<name>.lock`, open (not locked yet).
    fn open_lock(&self, name: &str) -> io::Result<fs::File> {
        let directory = self.cow_dir().join("locks");
        fs::create_dir_all(&directory)?;
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(format!("{name}.lock")))
    }

    /// The branch's `flock`, held until the file is dropped: every mount,
    /// commit and unmount of the branch, by any store or process on this
    /// root, takes it.
    fn lock_branch(&self, branch: &str) -> Result<fs::File, WorkspaceError> {
        Self::checked_branch(branch)?;
        crate::git::lock_file(&self.cow_dir().join("locks").join(format!("{branch}.lock")))
    }

    /// The commit `reference` names, when it names one.
    fn commit_of(&self, reference: &str) -> Option<String> {
        self.git.commit_id_of(reference).ok()
    }

    /// A branch name this store accepts: it names a directory under the
    /// root, so only `[A-Za-z0-9_-]`, never starting with `-`. That keeps
    /// `.cow/` and the transcript archive (`.transcripts/`) out of reach.
    fn checked_branch(branch: &str) -> Result<(), WorkspaceError> {
        let valid = !branch.is_empty()
            && !branch.starts_with('-')
            && branch
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
        if valid {
            Ok(())
        } else {
            Err(WorkspaceError::Refused {
                code: "BadBranch".to_string(),
                message: format!("branch {branch:?} is not [A-Za-z0-9_-]+"),
            })
        }
    }

    /// The workspace's directory: exactly `<root>/<sandbox>`, or refused.
    fn checked_path(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let branch = workspace.sandbox();
        Self::checked_branch(branch)?;
        let directory = self.root.join(branch);
        if Path::new(workspace.path()) != directory {
            return Err(WorkspaceError::Refused {
                code: "ForeignWorkspace".to_string(),
                message: format!("{} is not {}", workspace.path(), directory.display()),
            });
        }
        Ok(directory)
    }

    fn save_state(&self, branch: &str, state: &State) -> Result<(), WorkspaceError> {
        write_state(&self.state_path(branch), state)
    }

    /// `.cow/kept/<label>@<pid>-<n>`: `@` is in no branch name, so a kept
    /// directory is told by its branch exactly.
    fn kept_path(&self, label: &str) -> PathBuf {
        self.cow_dir()
            .join("kept")
            .join(format!("{label}@{}", unique_suffix()))
    }

    /// Delete `directory` only when it provably holds no `*.jsonl` file;
    /// one that holds one, or that cannot be read whole, is moved under
    /// `.cow/kept/` instead (a rename: all or nothing). Whether it was
    /// deleted.
    fn remove_guarded(&self, directory: &Path, label: &str) -> io::Result<bool> {
        if fs::symlink_metadata(directory)
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            return Ok(true);
        }
        if matches!(count_jsonl(directory), Ok(0)) {
            match fs::remove_dir_all(directory) {
                Ok(()) => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
                // What could not be removed is kept below.
                Err(_) => {}
            }
        }
        let kept = self.kept_path(label);
        fs::create_dir_all(kept.parent().unwrap_or(&self.root))?;
        fs::rename(directory, kept)?;
        Ok(false)
    }

    /// The listing of `commit` (`git ls-tree -r -z`), checked
    /// ([`check_tree`]).
    fn listing(&self, commit: &str) -> Result<Vec<u8>, WorkspaceError> {
        let listing = self
            .git
            .git_listing(&["ls-tree", "-r", "-z", "--full-tree", commit])?;
        check_tree(&parse_ls_tree(&listing)?)?;
        Ok(listing)
    }

    /// Layer 0 of `commit`: its files written once, read-only, under
    /// `.cow/baselines/<commit>/tree`, with the tree's listing beside it.
    /// Built beside the final name and renamed into place, so a concurrent
    /// mount of the same commit sees a whole baseline or none. The listing
    /// is checked again on every use.
    fn ensure_baseline(&self, commit: &str) -> Result<Vec<(u32, String, Vec<u8>)>, WorkspaceError> {
        let baselines = self.cow_dir().join("baselines");
        let ready = baselines.join(commit);
        let listing = if let Ok(listing) = fs::read(ready.join("entries")) {
            listing
        } else {
            fs::create_dir_all(&baselines).map_err(io_error("create", &baselines))?;
            let build = baselines.join(format!(".build-{commit}-{}", unique_suffix()));
            let listing = match self.build_baseline(commit, &build) {
                Ok(listing) => listing,
                Err(error) => {
                    let _ = self.remove_guarded(&build, "baseline-build");
                    return Err(error);
                }
            };
            if fs::rename(&build, &ready).is_err() {
                // Another mount built it first: theirs is the same tree.
                let _ = self.remove_guarded(&build, "baseline-build");
            }
            listing
        };
        let entries = parse_ls_tree(&listing)?;
        check_tree(&entries)?;
        Ok(entries)
    }

    fn build_baseline(&self, commit: &str, build: &Path) -> Result<Vec<u8>, WorkspaceError> {
        let listing = self.listing(commit)?;
        let tree_path = build.join("tree");
        fs::create_dir_all(&tree_path).map_err(io_error("create", &tree_path))?;
        let tree = DirFd::open(&tree_path).map_err(io_error("open", &tree_path))?;
        let entries = parse_ls_tree(&listing)?;
        let blobs: Vec<&(u32, String, Vec<u8>)> = entries
            .iter()
            .filter(|(mode, _, _)| *mode != 0o160_000)
            .collect();
        // One `cat-file --batch` streams every blob, exact bytes (no
        // checkout filters), so each file is the blob its id names.
        let mut child = self
            .git
            .command()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| unavailable(format!("run git cat-file: {error}")))?;
        let mut input = child.stdin.take().expect("piped stdin");
        let ids: Vec<String> = blobs.iter().map(|(_, sha, _)| sha.clone()).collect();
        let feeder = std::thread::spawn(move || {
            for id in ids {
                if writeln!(input, "{id}").is_err() {
                    break;
                }
            }
        });
        let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
        let written = (|| -> io::Result<()> {
            for (mode, sha, path) in &blobs {
                #[cfg(test)]
                if self.faults.baseline.load(Ordering::Relaxed) {
                    return Err(io::Error::other("injected: a baseline file fails"));
                }
                let mut header = String::new();
                reader.read_line(&mut header)?;
                let mut fields = header.trim_end().split(' ');
                let (Some(id), Some("blob"), Some(size)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    return Err(io::Error::other(format!("cat-file said {header:?}")));
                };
                if id != sha {
                    return Err(io::Error::other(format!(
                        "cat-file answered {id} for {sha}"
                    )));
                }
                let size: u64 = size
                    .parse()
                    .map_err(|_| io::Error::other(format!("cat-file said {header:?}")))?;
                let mut content = (&mut reader).take(size);
                let copied = if *mode == 0o120_000 {
                    let mut target = Vec::new();
                    content.read_to_end(&mut target)?;
                    tree.symlink(path, &target)?;
                    target.len() as u64
                } else {
                    // Streamed into the new file, never held whole.
                    let mut file = tree.create_file(path)?;
                    let copied = io::copy(&mut content, &mut file)?;
                    let bits = if *mode == 0o100_755 { 0o555 } else { 0o444 };
                    file.set_permissions(fs::Permissions::from_mode(bits))?;
                    copied
                };
                let mut newline = [0_u8; 1];
                if copied != size || reader.read_exact(&mut newline).is_err() || newline != *b"\n" {
                    return Err(io::Error::other(format!(
                        "cat-file's record of {sha} is not {size} bytes and a newline"
                    )));
                }
            }
            Ok(())
        })();
        let status = if written.is_ok() {
            drop(reader);
            let _ = feeder.join();
            child.wait()
        } else {
            // The child may be blocked writing into the pipe, and the feeder
            // writing into the child: close the pipe, stop and reap the
            // child, and only then join the feeder (its writes now fail).
            drop(reader);
            let _ = child.kill();
            let status = child.wait();
            let _ = feeder.join();
            status
        };
        written.map_err(|error| unavailable(format!("baseline of {commit}: {error}")))?;
        if !status.is_ok_and(|status| status.success()) {
            return Err(unavailable(format!("git cat-file failed for {commit}")));
        }
        fs::write(build.join("entries"), &listing).map_err(io_error("write", build))?;
        Ok(listing)
    }

    /// Clone the baseline of `commit` into the new directory `staging`; the
    /// manifest of what was cloned.
    fn clone_baseline(
        &self,
        commit: &str,
        staging: &Path,
        tally: &mut CowStats,
    ) -> Result<State, WorkspaceError> {
        let entries = self.ensure_baseline(commit)?;
        let tree_path = self.baseline_path(commit);
        let tree = DirFd::open(&tree_path).map_err(io_error("open", &tree_path))?;
        fs::create_dir_all(staging).map_err(io_error("create", staging))?;
        let target = DirFd::open(staging).map_err(io_error("open", staging))?;
        let mut state = State {
            head: commit.to_string(),
            next: None,
            recorded: (0, 0),
            mounted: (0, 0),
            entries: BTreeMap::new(),
        };
        for (mode, sha, path) in entries {
            let shown = staging.join(OsStr::from_bytes(&path));
            let meta = match mode {
                0o160_000 => {
                    target.make_dir(&path).map_err(io_error("create", &shown))?;
                    fs::symlink_metadata(&shown).map_err(io_error("stat", &shown))?
                }
                0o120_000 => {
                    let link = tree.read_link(&path).map_err(io_error("read", &shown))?;
                    target
                        .symlink(&path, &link)
                        .map_err(io_error("symlink", &shown))?;
                    fs::symlink_metadata(&shown).map_err(io_error("stat", &shown))?
                }
                _ => {
                    let (file, kind) =
                        clone_entry(&tree, &target, &path).map_err(io_error("clone", &shown))?;
                    match kind {
                        CloneKind::Reflinked => tally.reflinked += 1,
                        CloneKind::Copied => tally.copied += 1,
                    }
                    let bits = if mode == 0o100_755 { 0o755 } else { 0o644 };
                    file.set_permissions(fs::Permissions::from_mode(bits))
                        .map_err(io_error("chmod", &shown))?;
                    file.metadata().map_err(io_error("stat", &shown))?
                }
            };
            state.entries.insert(
                path,
                Entry {
                    mode,
                    sha,
                    stamp: Stamp::of(&meta),
                },
            );
        }
        // Nothing but this mount writes the staged tree, so every stamp is
        // final; the fence goes past the newest one before the tree is
        // handed out, and every stamp is trusted by the first checkpoint.
        let newest = state
            .entries
            .values()
            .map(|entry| entry.stamp.newest())
            .max()
            .unwrap_or((0, 0));
        let scratch = self.cow_dir().join("tmp");
        fs::create_dir_all(&scratch).map_err(io_error("create", &scratch))?;
        state.recorded = fence_after(&scratch, newest).map_err(io_error("fence", &scratch))?;
        Ok(state)
    }

    /// The tree the workspace holds now, written to the repository, and the
    /// manifest that goes with it. Only files whose stamp moved are read;
    /// a gitlink whose directory is still there keeps its commit.
    fn snapshot(
        &self,
        branch: &str,
        directory: &Path,
        previous: &State,
        tally: &mut CowStats,
    ) -> Result<Snapshot, WorkspaceError> {
        let scratch = self.cow_dir().join("tmp");
        fs::create_dir_all(&scratch).map_err(io_error("create", &scratch))?;
        let recorded = fence(&scratch).map_err(io_error("fence", &scratch))?;
        let suffix = unique_suffix();
        // What git's rules let a checkpoint take, against the head alone
        // (an index file that is never made).
        let (paths, mut oversized) = self.git.checkpoint_paths(
            directory,
            &scratch.join(format!("{branch}-{suffix}.listing")),
            &previous.head,
        )?;
        // A file this store would not take anyway (an untracked
        // transcript) is no file "left out for its size".
        oversized.retain(|(path, _)| taken(path.as_bytes(), &previous.entries));
        let found = found_files(directory, &paths, &previous.entries)
            .map_err(io_error("walk", directory))?;

        let mut shas: Vec<Option<String>> = Vec::with_capacity(found.len());
        let mut to_hash: Vec<(usize, PathBuf)> = Vec::new();
        let mut link_files = Vec::new();
        for (at, file) in found.iter().enumerate() {
            let known = previous.entries.get(&file.path).filter(|entry| {
                entry.mode == file.mode
                    && (file.mode == 0o160_000
                        || (entry.stamp == file.stamp && !entry.stamp.racy(previous.recorded)))
            });
            if let Some(entry) = known {
                tally.reused += 1;
                shas.push(Some(entry.sha.clone()));
                continue;
            }
            tally.hashed += 1;
            shas.push(None);
            if file.mode == 0o120_000 {
                // A symlink's blob is its target; hash-object would follow it.
                let target =
                    fs::read_link(&file.absolute).map_err(io_error("read", &file.absolute))?;
                let holder = scratch.join(format!("link-{suffix}-{at}"));
                fs::write(&holder, target.as_os_str().as_bytes())
                    .map_err(io_error("write", &holder))?;
                link_files.push(holder.clone());
                to_hash.push((at, holder));
            } else {
                to_hash.push((at, file.absolute.clone()));
            }
        }
        let hashed = self.hash_objects(&to_hash);
        for holder in link_files {
            let _ = fs::remove_file(holder);
        }
        for (at, sha) in hashed? {
            shas[at] = Some(sha);
        }

        let index = scratch.join(format!("{branch}-{suffix}.index"));
        let mut listing = Vec::new();
        let mut state = State {
            head: previous.head.clone(),
            next: None,
            recorded,
            mounted: previous.mounted,
            entries: BTreeMap::new(),
        };
        for (file, sha) in found.into_iter().zip(shas) {
            let sha = sha.ok_or_else(|| unavailable("a file was left unhashed"))?;
            listing.extend_from_slice(format!("{:o} {sha}\t", file.mode).as_bytes());
            listing.extend_from_slice(&file.path);
            listing.push(0);
            state.entries.insert(
                file.path,
                Entry {
                    mode: file.mode,
                    sha,
                    stamp: file.stamp,
                },
            );
        }
        let tree = self.write_tree(directory, &index, &listing);
        let removed = remove_file(&index);
        let tree = tree?;
        removed?;
        Ok((tree, state, oversized))
    }

    /// `git hash-object -w` of every path, in batches; `(index, blob id)`.
    fn hash_objects(
        &self,
        paths: &[(usize, PathBuf)],
    ) -> Result<Vec<(usize, String)>, WorkspaceError> {
        let mut out = Vec::with_capacity(paths.len());
        for chunk in paths.chunks(256) {
            let mut command = self.git.command();
            command
                .args(["hash-object", "-w", "--no-filters", "--"])
                .args(chunk.iter().map(|(_, path)| path.as_os_str()))
                .stdin(Stdio::null());
            let ids = report(command.output(), "hash-object")?;
            let ids: Vec<&str> = ids.lines().collect();
            if ids.len() != chunk.len() {
                return Err(unavailable("git hash-object answered a short list"));
            }
            out.extend(
                chunk
                    .iter()
                    .zip(ids)
                    .map(|((at, _), id)| (*at, id.to_string())),
            );
        }
        Ok(out)
    }

    /// The tree of `listing` (`git update-index -z --index-info` into the
    /// fresh index `index`, then `write-tree`).
    fn write_tree(
        &self,
        directory: &Path,
        index: &Path,
        listing: &[u8],
    ) -> Result<String, WorkspaceError> {
        remove_file(index)?;
        self.git.listing_in_with_input(
            directory,
            index,
            &["update-index", "-z", "--index-info"],
            listing,
        )?;
        self.git.output_in(directory, index, &["write-tree"])
    }

    /// Refused while a process is seen working in `directory`, or when that
    /// cannot be told (fail closed). What could not be inspected is kept in
    /// [`Self::diagnostics`] (and named in a refusal).
    fn refuse_busy(&self, directory: &Path) -> Result<(), WorkspaceError> {
        let found = directory_occupancy(directory).map_err(|error| {
            busy(format!(
                "cannot tell whether a process works in {}: {error}",
                directory.display()
            ))
        })?;
        for what in &found.uninspected {
            self.note(format!("not inspected: {what}"));
        }
        if found.pids.is_empty() {
            Ok(())
        } else {
            Err(busy(format!(
                "processes {:?} work in {}; nothing changed{}",
                found.pids,
                directory.display(),
                if found.uninspected.is_empty() {
                    String::new()
                } else {
                    format!(" (not inspected: {})", found.uninspected.join("; "))
                }
            )))
        }
    }

    /// The strict transcript look before a replacement or an unmount: a
    /// `*.jsonl` file outside the workspace's `.transcripts/`, or a part of
    /// it that cannot be read, refuses it.
    fn preflight(directory: &Path) -> Result<(), WorkspaceError> {
        match jsonl_outside_transcripts(directory) {
            Ok(0) => Ok(()),
            Ok(count) => Err(holds_transcripts(directory, count)),
            Err(error) => Err(unreadable(directory, &error)),
        }
    }

    /// Whether `directory`'s files are exactly `state`'s head (nothing
    /// uncommitted).
    fn is_clean(
        &self,
        branch: &str,
        directory: &Path,
        state: &State,
        tally: &mut CowStats,
    ) -> Result<bool, WorkspaceError> {
        let (tree, _, _) = self.snapshot(branch, directory, state, tally)?;
        let committed = self
            .git
            .git_output(&["rev-parse", &format!("{}^{{tree}}", state.head)])?;
        Ok(tree == committed)
    }

    fn dirty(directory: &Path) -> WorkspaceError {
        WorkspaceError::Refused {
            code: WORKSPACE_DIRTY.to_string(),
            message: format!(
                "{} holds work no checkpoint took; commit or unmount it first",
                directory.display()
            ),
        }
    }

    /// The state of `branch`: a mount whose final rename went through but
    /// whose state was not moved into place is taken as done (its
    /// `.installing` record names the directory there now), and a branch
    /// move whose state could not be saved after it as well (the branch is
    /// at the state's `next`). `None` only when there is no state; one
    /// that cannot be read is an error.
    fn settled_state(&self, branch: &str) -> Result<Option<State>, WorkspaceError> {
        self.recover_swap(branch)?;
        let installing = self.installing_path(branch);
        let mut state = match read_state(&installing)? {
            Some(installed)
                if present(&self.root.join(branch))?
                    .is_some_and(|meta| is_generation(&meta, &installed)) =>
            {
                let path = self.state_path(branch);
                fs::rename(&installing, &path).map_err(io_error("rename", &path))?;
                Some(installed)
            }
            Some(_) => {
                // Its rename never happened: the record is void.
                remove_file(&installing)?;
                read_state(&self.state_path(branch))?
            }
            None => read_state(&self.state_path(branch))?,
        };
        if let Some(state) = state.as_mut()
            && let Some(next) = state.next.take()
            && self.commit_of(&format!("refs/heads/{branch}")).as_deref() == Some(next.as_str())
        {
            state.head = next;
        }
        Ok(state)
    }

    /// Settle a swap a mount left unfinished (its record at
    /// [`Self::swap_path`]). If the final rename went through, the new tree
    /// is the workspace and the old one is retired; if not, what was
    /// carried goes back into the old workspace, which goes back in place
    /// (under the exclusive admission lock: the only step that moves the
    /// workspace), and the staged tree is removed. Settling a record whose
    /// workspace is in place touches only trees outside it, so a worker
    /// holding its lease (committing) is never refused for it.
    /// Nothing that holds a `*.jsonl` file is ever deleted
    /// ([`Self::remove_guarded`]).
    fn recover_swap(&self, branch: &str) -> Result<(), WorkspaceError> {
        let record = self.swap_path(branch);
        let swap = match fs::read_to_string(&record) {
            Ok(text) => Swap::parse(&text).ok_or_else(|| {
                unavailable(format!(
                    "{} is not a swap record this store can read; nothing changed",
                    record.display()
                ))
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error("read", &record)(error)),
        };
        let directory = self.root.join(branch);
        let installing = self.installing_path(branch);
        let new = read_state(&installing)?;
        let old = read_state(&self.state_path(branch))?;
        let found = present(&directory)?;
        let leftover = |path: &Path, label: &str| -> Result<(), WorkspaceError> {
            if present(path)?.is_some()
                && !self
                    .remove_guarded(path, label)
                    .map_err(io_error("remove", path))?
            {
                self.note(format!("{} was kept under .cow/kept/", path.display()));
            }
            Ok(())
        };
        let is = |state: &Option<State>| {
            found
                .as_ref()
                .zip(state.as_ref())
                .is_some_and(|(meta, state)| is_generation(meta, state))
        };
        if is(&new) {
            // The final rename went through: only the state and the old
            // tree's removal were left.
            let path = self.state_path(branch);
            fs::rename(&installing, &path).map_err(io_error("rename", &path))?;
            if let Some(trash) = &swap.trash {
                leftover(trash, "replaced")?;
            }
        } else if found.is_none() {
            // Stopped between the two renames: the old workspace goes back,
            // with what was carried out of it, while no worker is admitted.
            let _admission = self.quiesce(branch)?;
            if let Some(trash) = swap.trash.as_ref().filter(|trash| trash.exists()) {
                for name in [crate::TRANSCRIPT_DIRECTORY, ".spawn"] {
                    let carried = swap.staging.join(name);
                    if present(&carried)?.is_some() && present(&trash.join(name))?.is_none() {
                        fs::rename(&carried, trash.join(name))
                            .map_err(io_error("move", &carried))?;
                    }
                }
                fs::rename(trash, &directory).map_err(io_error("move", trash))?;
            }
            leftover(&swap.staging, "stage")?;
            remove_file(&installing)?;
        } else if is(&old) {
            // The workspace is its state's own (the swap never started, was
            // undone, or its state was already moved into place): only
            // leftovers.
            for path in [Some(&swap.staging), swap.trash.as_ref()]
                .into_iter()
                .flatten()
            {
                leftover(path, "stage")?;
            }
            remove_file(&installing)?;
        } else {
            return Err(unavailable(format!(
                "{} is neither the workspace before nor after the swap in {}; nothing changed",
                directory.display(),
                record.display()
            )));
        }
        remove_file(&record)
    }

    fn mount_locked(&self, branch: &str) -> Result<WorkspaceId, WorkspaceError> {
        let head = self
            .commit_of(&format!("refs/heads/{branch}"))
            .ok_or_else(|| unavailable(format!("branch {branch} has no commit")))?;
        let directory = self.root.join(branch);
        let id = || {
            WorkspaceId::new(format!("{branch}:{}", directory.display()))
                .map_err(|error| unavailable(error.to_string()))
        };
        let existing = self.settled_state(branch)?;
        // Held from here to the end of a replacement: no worker starts.
        let mut _admission = None;
        let previous = match (present(&directory)?, existing) {
            (None, _) => None,
            (Some(_), None) => {
                return Err(WorkspaceError::Refused {
                    code: WORKSPACE_DIRTY.to_string(),
                    message: format!("{} exists and no mount made it", directory.display()),
                });
            }
            (Some(meta), Some(state)) => {
                Self::owned(&directory, &meta, &state)?;
                let mut tally = CowStats::default();
                if !self.is_clean(branch, &directory, &state, &mut tally)? {
                    return Err(Self::dirty(&directory));
                }
                if state.head == head {
                    self.set_last(tally);
                    return id();
                }
                _admission = Some(self.quiesce(branch)?);
                self.refuse_busy(&directory)?;
                Self::preflight(&directory)?;
                Some(state)
            }
        };

        let mut tally = CowStats::default();
        let staging = self
            .cow_dir()
            .join("stage")
            .join(format!("{branch}@{}", unique_suffix()));
        let cloned = {
            let baselines = self
                .open_lock("baselines")
                .map_err(lock_error(&self.cow_dir()))?;
            baselines
                .lock_shared()
                .map_err(lock_error(&self.cow_dir()))?;
            self.clone_baseline(&head, &staging, &mut tally)
        };
        // The new generation is recorded before the final rename, so a
        // state that cannot be saved after it is still found.
        let installing = self.installing_path(branch);
        let recorded = cloned.and_then(|mut state| {
            let meta = fs::symlink_metadata(&staging).map_err(io_error("stat", &staging))?;
            state.mounted = (meta.dev(), meta.ino());
            write_state(&installing, &state)
        });
        // Both trees' places too, before either moves.
        let swap = Swap {
            staging: staging.clone(),
            trash: previous.as_ref().map(|_| {
                self.cow_dir()
                    .join("stage")
                    .join(format!("old-{branch}@{}", unique_suffix()))
            }),
        };
        let record = self.swap_path(branch);
        let recorded = recorded.and_then(|()| write_whole(&record, &swap.render()));
        if let Err(error) = recorded {
            let _ = self.remove_guarded(&staging, "stage");
            let _ = remove_file(&installing);
            return Err(error);
        }
        let swapped = match (&previous, &swap.trash) {
            (Some(previous), Some(trash)) => self
                .replace(branch, &directory, &staging, trash, previous)
                .map(|()| Some(trash.clone())),
            _ => self.swap_in(&staging, &directory).map(|()| None),
        };
        let trash = match swapped {
            Ok(trash) => trash,
            Err(error) => {
                #[cfg(test)]
                if self.faults.interrupted.load(Ordering::Relaxed) {
                    return Err(error);
                }
                let _ = self.remove_guarded(&staging, "stage");
                let cleaned = remove_file(&installing).and_then(|()| remove_file(&record));
                return Err(match cleaned {
                    Ok(()) => error,
                    Err(also) => unavailable(format!("{error}; {also}")),
                });
            }
        };
        self.settle_install(branch, trash.as_deref(), &record);
        self.set_last(tally);
        id()
    }

    /// After a mount's final rename: move its state into place, remove the
    /// retired tree and the swap record. Nothing here fails the mount: what
    /// is left is said in [`Self::diagnostics`] and settled by the next
    /// look.
    fn settle_install(&self, branch: &str, trash: Option<&Path>, record: &Path) {
        if let Err(error) = self.install_state(branch) {
            self.note(format!(
                "{branch} is mounted; its state stays at {} until the next look: {error}",
                self.installing_path(branch).display()
            ));
        }
        if let Some(trash) = trash {
            match self.remove_guarded(trash, "replaced") {
                Ok(true) => {}
                Ok(false) => self.note(format!(
                    "the replaced {branch} was kept under {}",
                    self.cow_dir().join("kept").display()
                )),
                Err(error) => self.note(format!(
                    "the replaced {branch} is left at {}: {error}",
                    trash.display()
                )),
            }
        }
        if let Err(error) = remove_file(record) {
            self.note(format!(
                "{branch}'s swap record stays until the next look: {error}"
            ));
        }
    }

    /// Move the `.installing` record of `branch` into place.
    fn install_state(&self, branch: &str) -> Result<(), WorkspaceError> {
        #[cfg(test)]
        if self.faults.save_after_swap.load(Ordering::Relaxed) {
            return Err(unavailable("injected: the state cannot be saved"));
        }
        let path = self.state_path(branch);
        fs::rename(self.installing_path(branch), &path).map_err(io_error("rename", &path))
    }

    /// The final rename of a mount: `staging` becomes `directory`.
    fn swap_in(&self, staging: &Path, directory: &Path) -> Result<(), WorkspaceError> {
        #[cfg(test)]
        if self.faults.swap.load(Ordering::Relaxed) {
            return Err(unavailable("injected: the final rename fails"));
        }
        let _ = self;
        fs::rename(staging, directory).map_err(io_error("move", staging))
    }

    /// Replace the clean workspace `directory` (at `previous`'s head) by
    /// `staging`, carrying over what no commit holds (`.transcripts/`,
    /// `.spawn/`); the old workspace goes to `trash`, for the caller to
    /// remove. Every
    /// step is undone when a later one fails: the workspace, its
    /// transcripts included, is then as it was (or the error says where it
    /// is).
    fn replace(
        &self,
        branch: &str,
        directory: &Path,
        staging: &Path,
        trash: &Path,
        previous: &State,
    ) -> Result<(), WorkspaceError> {
        fs::rename(directory, trash).map_err(io_error("move", directory))?;
        let restore = |carried: &[&str], error: WorkspaceError| {
            let mut failed = Vec::new();
            for name in carried {
                if let Err(also) = fs::rename(staging.join(name), trash.join(name)) {
                    failed.push(format!("{name}: {also}"));
                }
            }
            if let Err(also) = fs::rename(trash, directory) {
                failed.push(format!("the workspace: {also}"));
            }
            if failed.is_empty() {
                error
            } else {
                unavailable(format!(
                    "{error}; it could not be put back whole ({}), it is at {}",
                    failed.join(", "),
                    trash.display()
                ))
            }
        };
        // Out of the agent's path now: what was written since the checks
        // above is seen here, and the replacement refused.
        let mut tally = CowStats::default();
        match self.is_clean(branch, trash, previous, &mut tally) {
            Ok(true) => {}
            Ok(false) => return Err(restore(&[], Self::dirty(directory))),
            Err(error) => return Err(restore(&[], error)),
        }
        match jsonl_outside_transcripts(trash) {
            Ok(0) => {}
            Ok(count) => return Err(restore(&[], holds_transcripts(directory, count))),
            Err(error) => return Err(restore(&[], unreadable(directory, &error))),
        }
        let mut carried = Vec::new();
        for name in [crate::TRANSCRIPT_DIRECTORY, ".spawn"] {
            let from = trash.join(name);
            match fs::symlink_metadata(&from) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(restore(&carried, io_error("stat", &from)(error))),
            }
            if let Err(error) = fs::rename(&from, staging.join(name)) {
                return Err(restore(&carried, io_error("move", &from)(error)));
            }
            carried.push(name);
        }
        #[cfg(test)]
        if self.faults.interrupted.load(Ordering::Relaxed) {
            return Err(unavailable(
                "injected: the process stops before the final rename",
            ));
        }
        if let Err(error) = self.swap_in(staging, directory) {
            return Err(restore(&carried, error));
        }
        Ok(())
    }

    /// Keep `commit` (the workspace's state) on its own ref, never moved
    /// once made.
    fn keep_stale(&self, branch: &str, commit: &str) -> Result<String, WorkspaceError> {
        let reference = format!("refs/spawn-cow/stale/{branch}/{commit}");
        if self
            .git
            .git_output(&["update-ref", &reference, commit, ""])
            .is_err()
            && self.commit_of(&reference).as_deref() != Some(commit)
        {
            return Err(unavailable(format!(
                "{branch} moved under the workspace, and its state ({commit}) \
                 could not be kept on {reference}"
            )));
        }
        Ok(reference)
    }

    fn commit_locked(
        &self,
        workspace: &WorkspaceId,
        message: &str,
    ) -> Result<Commit, WorkspaceError> {
        let directory = self.checked_path(workspace)?;
        let branch = workspace.sandbox();
        let previous = self
            .settled_state(branch)?
            .ok_or_else(|| unavailable(format!("{branch} is not mounted by this store")))?;
        let meta = present(&directory)?
            .ok_or_else(|| unavailable(format!("{} is gone", directory.display())))?;
        Self::owned(&directory, &meta, &previous)?;
        self.git
            .preserve_worker_head_best_effort(branch, &directory);
        let mut tally = CowStats::default();
        let (tree, mut state, oversized) =
            self.snapshot(branch, &directory, &previous, &mut tally)?;
        // Told to the agent as the Git store tells it ([`Self::notices`]).
        self.git.record_oversized(branch, &directory, &oversized);
        self.set_last(tally);
        let reference = format!("refs/heads/{branch}");
        let current = self.commit_of(&reference);
        let head_tree = self
            .git
            .git_output(&["rev-parse", &format!("{}^{{tree}}", previous.head)])?;

        let make = |parent: &str| {
            self.git.git_output(&[
                "commit-tree",
                "--no-gpg-sign",
                &tree,
                "-p",
                parent,
                "-m",
                message,
            ])
        };
        let (commit, result) = match current {
            Some(current) if current != previous.head => {
                // Someone moved the branch: keep this state off it, refuse.
                let kept = if tree == head_tree {
                    previous.head.clone()
                } else {
                    make(&previous.head)?
                };
                let reference = self.keep_stale(branch, &kept)?;
                return Err(WorkspaceError::Refused {
                    code: WORKSPACE_STALE.to_string(),
                    message: format!(
                        "{branch} moved to {current} under the workspace (made on {}); \
                         its state is kept on {reference}",
                        previous.head
                    ),
                });
            }
            Some(_) if tree == head_tree => (None, Commit::Unchanged),
            Some(_) => {
                let commit = make(&previous.head)?;
                let id = CommitId::new(commit.clone()).map_err(unavailable)?;
                (Some((commit, previous.head.clone())), Commit::Created(id))
            }
            None => {
                // The branch is gone: made again from the workspace, never
                // over a ref someone made meanwhile (empty old value).
                let commit = if tree == head_tree {
                    previous.head.clone()
                } else {
                    make(&previous.head)?
                };
                let result = Commit::Recreated {
                    commit: CommitId::new(commit.clone()).map_err(unavailable)?,
                    parent: Some(CommitId::new(previous.head.clone()).map_err(unavailable)?),
                };
                (Some((commit, String::new())), result)
            }
        };
        if let Some((commit, old)) = commit {
            // The move is recorded before it is made, so a state that cannot
            // be saved after it is still followed (`settled_state`).
            let mut pending = previous.clone();
            pending.next = Some(commit.clone());
            self.save_state(branch, &pending)?;
            self.git
                .git_output(&["update-ref", &reference, &commit, &old])
                .map_err(|error| WorkspaceError::Refused {
                    code: WORKSPACE_STALE.to_string(),
                    message: format!("{branch} moved during the checkpoint: {error}"),
                })?;
            state.head = commit;
            #[cfg(test)]
            if self.faults.save_after_move.load(Ordering::Relaxed) {
                return Err(unavailable("injected: the state cannot be saved"));
            }
        }
        self.save_state(branch, &state)?;
        Ok(result)
    }

    fn unmount_locked(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        let directory = self.checked_path(workspace)?;
        let branch = workspace.sandbox();
        let state = self.settled_state(branch)?;
        match (present(&directory)?, &state) {
            (None, _) => {}
            (Some(_), None) => {
                return Err(Self::not_owned(
                    &directory,
                    "no mount of this store made it",
                ));
            }
            (Some(meta), Some(state)) => {
                Self::owned(&directory, &meta, state)?;
                let _admission = self.quiesce(branch)?;
                self.refuse_busy(&directory)?;
                Self::preflight(&directory)?;
                self.git
                    .preserve_worker_head_best_effort(branch, &directory);
                self.retire(branch, &directory)?;
            }
        }
        remove_file(&self.state_path(branch))
    }

    /// Take the checked workspace `directory` out of the way in one rename,
    /// then keep its `.transcripts/` under `.cow/kept/` and remove the
    /// rest. A `*.jsonl` file that appeared outside `.transcripts/` after
    /// the preflight, or a part that cannot be read any more, keeps the
    /// retired workspace whole under `.cow/kept/`.
    fn retire(&self, branch: &str, directory: &Path) -> Result<(), WorkspaceError> {
        let trash = self
            .cow_dir()
            .join("stage")
            .join(format!("gone-{branch}@{}", unique_suffix()));
        fs::create_dir_all(trash.parent().unwrap_or(&self.root))
            .map_err(io_error("create", &trash))?;
        fs::rename(directory, &trash).map_err(io_error("move", directory))?;
        let kept_whole = |why: String| {
            let kept = self.kept_path(branch);
            let moved = fs::create_dir_all(kept.parent().unwrap_or(&self.root))
                .and_then(|()| fs::rename(&trash, &kept));
            let place = if moved.is_ok() { &kept } else { &trash };
            WorkspaceError::Refused {
                code: WORKSPACE_HOLDS_TRANSCRIPTS.to_string(),
                message: format!(
                    "{} {why}; it was kept whole at {}",
                    directory.display(),
                    place.display()
                ),
            }
        };
        match jsonl_outside_transcripts(&trash) {
            Ok(0) => {}
            Ok(count) => return Err(kept_whole(format!("gained {count} *.jsonl file(s)"))),
            Err(error) => return Err(kept_whole(format!("could not be read whole ({error})"))),
        }
        let transcripts = trash.join(crate::TRANSCRIPT_DIRECTORY);
        if present(&transcripts)?.is_some() {
            let kept = self.kept_path(branch);
            fs::create_dir_all(&kept).map_err(io_error("create", &kept))?;
            fs::rename(&transcripts, kept.join(crate::TRANSCRIPT_DIRECTORY))
                .map_err(io_error("move", &transcripts))?;
        }
        if !self
            .remove_guarded(&trash, branch)
            .map_err(io_error("remove", &trash))?
        {
            return Err(WorkspaceError::Refused {
                code: WORKSPACE_HOLDS_TRANSCRIPTS.to_string(),
                message: format!(
                    "{} gained a *.jsonl file; it was kept under {}",
                    directory.display(),
                    self.cow_dir().join("kept").display()
                ),
            });
        }
        Ok(())
    }

    /// Commit the workspace (the [`WorkspaceStore::commit`] of this store),
    /// asking `take_back` first when its branch is gone, as
    /// [`GitWorkspace::commit_or_take_back`] does: under the branch's lock,
    /// so whether the branch is there is read where it is used. Made by
    /// `take_back`, the commit goes on it (refused [`WORKSPACE_STALE`] when
    /// it is not the head the workspace was made on); still absent, the
    /// branch is made again from the workspace ([`Commit::Recreated`]). An
    /// error from `take_back` is the commit's, and nothing is written.
    pub fn commit_or_take_back(
        &self,
        workspace: &WorkspaceId,
        message: &str,
        take_back: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<Commit, WorkspaceError> {
        let _lock = self.lock_branch(workspace.sandbox())?;
        self.clear_diagnostics();
        if self
            .commit_of(&format!("refs/heads/{}", workspace.sandbox()))
            .is_none()
        {
            take_back()?;
        }
        self.commit_locked(workspace, message)
    }

    /// Persists any worker-authored `.git` commits in `directory` via the
    /// underlying bare [`GitWorkspace`] (`#103`, `#110`).
    pub fn pin_worker_head(
        &self,
        branch: &str,
        directory: Option<&Path>,
        target_refs: &[&str],
    ) -> Result<Option<CommitId>, WorkspaceError> {
        self.git.pin_worker_head(branch, directory, target_refs)
    }

    /// [`WorkspaceStore::set_aside`] of this store, only while `ready` says
    /// the agent is idle, asked under the branch's lock before anything is
    /// read or written, as [`GitWorkspace::set_aside_when`] asks it (the
    /// Bridge types under the same lock: [`WorkspaceStore::hold`]). When
    /// `ready` refuses, nothing changes: the refusal is
    /// [`crate::SET_ASIDE_WAITS`] with `ready`'s reason.
    ///
    /// The Git store's rule, step for step: `start` must be an ancestor of
    /// the head (`None`: where the branch forked from `main`); a mounted
    /// workspace's work no checkpoint took is committed first (a checkpoint
    /// of this store, so the manifest stays exact); a new commit with
    /// `start`'s tree goes on top of the head; asked again with the same
    /// `message` while the head is that set-aside, the same answer comes
    /// back and nothing is written. The workspace follows in place, never
    /// replaced, so an engine idle in it keeps its directory: only the
    /// paths that differ are written, cloned from `start`'s baseline, and a
    /// file no checkpoint took that is in the way of one is moved, bytes
    /// untouched, under [`crate::SET_ASIDE_FILES`]`/<kept>/`. The branch
    /// moves last, by compare-and-swap; the manifest records the move
    /// before it (its `next`), so a state that cannot be saved after it is
    /// still followed.
    ///
    /// Checked before anything is written, the work's checkpoint included:
    /// a directory this store did not mount is refused
    /// ([`WORKSPACE_NOT_OWNED`]), and so is a workspace that cannot reach
    /// `start`'s tree in place ([`WORKSPACE_DIFFERS`]: a file no checkpoint
    /// takes, such as one over the size cap, where `start` needs a
    /// directory). A file goes only while its `lstat` stamp is still the
    /// checkpoint's. A refused set-aside changes neither the directory nor
    /// the branch, and is asked again as it was.
    pub fn set_aside_when(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        let branch = branch.as_str();
        let _lock = self.lock_branch(branch)?;
        ready().map_err(|why| WorkspaceError::Refused {
            code: crate::git::SET_ASIDE_WAITS.to_string(),
            message: why,
        })?;
        self.clear_diagnostics();
        let reference = format!("refs/heads/{branch}");
        let head = self.git.commit_id_of(&reference)?;
        let start = self.git.set_aside_start(branch, &head, start)?;
        let directory = self.root.join(branch);
        let Some(meta) = present(&directory)? else {
            let head = self.git.commit_id_of(&reference)?;
            return match self.git.set_aside_plan(&head, &start, message)? {
                Ok(plan) => {
                    let commit = self.git.set_aside_commit(&plan, message)?;
                    self.git.move_to_set_aside(branch, &plan, &commit)
                }
                Err(done) => Ok(done),
            };
        };
        // A directory this store did not mount is never written.
        let state = self
            .settled_state(branch)?
            .ok_or_else(|| Self::not_owned(&directory, "no mount of this store made it"))?;
        Self::owned(&directory, &meta, &state)?;
        // The baselines' lock, shared, from the start's baseline to the
        // last file cloned from it: a prune (exclusive) waits, so the
        // baseline the move reads is never removed under it.
        let baselines = self
            .open_lock("baselines")
            .map_err(lock_error(&self.cow_dir()))?;
        baselines
            .lock_shared()
            .map_err(lock_error(&self.cow_dir()))?;
        let new = self.ensure_baseline(&start)?;
        // Checked before anything moves: what a checkpoint would take now
        // (no commit, no ref), and whether the workspace can go to the
        // start's tree from there. A refusal changes neither the directory
        // nor the branch.
        let mut tally = CowStats::default();
        let (now, _, _) = self.snapshot(branch, &directory, &state, &mut tally)?;
        let taken =
            parse_ls_tree(
                &self
                    .git
                    .git_listing(&["ls-tree", "-r", "-z", "--full-tree", &now])?,
            )?
            .into_iter()
            .map(|(mode, sha, path)| (path, (mode, sha)))
            .collect();
        plan_move(&directory, &taken, &new)?;

        let workspace = WorkspaceId::new(format!("{branch}:{}", directory.display()))
            .map_err(|error| unavailable(error.to_string()))?;
        self.commit_locked(&workspace, &crate::git::set_aside_kept_message(branch))?;
        let head = self.git.commit_id_of(&reference)?;
        let plan = match self.git.set_aside_plan(&head, &start, message)? {
            Ok(plan) => plan,
            Err(done) => return Ok(done),
        };
        let commit = self.git.set_aside_commit(&plan, message)?;
        let mut state = self
            .settled_state(branch)?
            .ok_or_else(|| unavailable(format!("{branch} is not mounted by this store")))?;
        let old: TreeEntries = state
            .entries
            .iter()
            .map(|(path, entry)| (path.clone(), (entry.mode, entry.sha.clone())))
            .collect();
        let steps = plan_move(&directory, &old, &new)?;
        let aside = format!("{}/{}", crate::git::SET_ASIDE_FILES, plan.head);
        let moved = apply_move(
            &directory,
            &self.baseline_path(&start),
            &steps,
            aside.as_bytes(),
            &|going: &Going| Self::verify_stamps(&directory, &state, going),
        )?;
        drop(baselines);
        Self::follow_move(&directory, &mut state, new, &moved)?;
        // The stamps just taken are at or past the fence, so the next
        // checkpoint reads those files again: racy, never trusted.
        state.next = Some(commit.clone());
        self.save_state(branch, &state)?;
        let made = self.git.move_to_set_aside(branch, &plan, &commit);
        if made.is_ok() {
            state.head = commit;
        }
        state.next = None;
        self.save_state(branch, &state)?;
        made
    }

    /// Refused ([`WORKSPACE_DIFFERS`]) unless every path of `going` is
    /// still the file the checkpoint took: its `lstat` stamp is the
    /// manifest's.
    fn verify_stamps(directory: &Path, state: &State, going: &Going) -> Result<(), WorkspaceError> {
        for (path, mode, _) in going {
            if *mode == 0o160_000 {
                continue;
            }
            let shown = directory.join(OsStr::from_bytes(path));
            let same = fs::symlink_metadata(&shown).is_ok_and(|meta| {
                state
                    .entries
                    .get(path)
                    .is_some_and(|entry| entry.stamp == Stamp::of(&meta))
            });
            if !same {
                return Err(differs(format!(
                    "{} changed since the checkpoint: nothing was changed",
                    shown.display()
                )));
            }
        }
        Ok(())
    }

    /// The manifest of `state` after `moved`: what went is forgotten, what
    /// was written recorded with its new stamp.
    fn follow_move(
        directory: &Path,
        state: &mut State,
        new: Vec<(u32, String, Vec<u8>)>,
        moved: &Moved,
    ) -> Result<(), WorkspaceError> {
        for path in &moved.removed {
            state.entries.remove(path);
        }
        for (mode, sha, path) in new {
            if !moved.written.contains(&path) {
                continue;
            }
            let shown = directory.join(OsStr::from_bytes(&path));
            let meta = fs::symlink_metadata(&shown).map_err(io_error("stat", &shown))?;
            state.entries.insert(
                path,
                Entry {
                    mode,
                    sha,
                    stamp: Stamp::of(&meta),
                },
            );
        }
        Ok(())
    }

    /// Where a kept `.transcripts/` of `branch` went, oldest first.
    pub fn kept_transcripts(&self, branch: &str) -> Vec<PathBuf> {
        let mut kept: Vec<PathBuf> = fs::read_dir(self.cow_dir().join("kept"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .split_once('@')
                    .is_some_and(|(name, _)| name == branch)
            })
            .map(|entry| entry.path().join(crate::TRANSCRIPT_DIRECTORY))
            .filter(|path| fs::symlink_metadata(path).is_ok())
            .collect();
        kept.sort();
        kept
    }
}

/// `git ls-tree -r -z` output as `(mode, id, path)`; a record that does not
/// parse is an error, never skipped.
pub(crate) fn parse_ls_tree(listing: &[u8]) -> Result<Vec<(u32, String, Vec<u8>)>, WorkspaceError> {
    listing
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let parsed = (|| {
                let tab = record.iter().position(|byte| *byte == b'\t')?;
                let head = std::str::from_utf8(&record[..tab]).ok()?;
                let mut fields = head.split(' ');
                let mode = u32::from_str_radix(fields.next()?, 8).ok()?;
                let _kind = fields.next()?;
                let sha = fields.next()?.to_string();
                Some((mode, sha, record[tab + 1..].to_vec()))
            })();
            parsed.ok_or_else(|| unavailable("git ls-tree printed a record it should not"))
        })
        .collect()
}

impl WorkspaceStore for CowWorkspace {
    fn project(&self) -> &str {
        self.git.project()
    }

    /// The Git store's own branch creation ([`GitWorkspace`]), for a name
    /// this store accepts.
    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError> {
        Self::checked_branch(branch.as_str())?;
        self.git.ensure_branch(branch, from)
    }

    /// Clone the branch head's baseline into `<root>/<branch>`. A directory
    /// already there at the head with nothing uncommitted is reused as it
    /// is; one with uncommitted work is refused ([`WORKSPACE_DIRTY`]), and
    /// a clean one at an older head is replaced only while no worker holds
    /// its lease and no process is seen in it ([`WORKSPACE_BUSY`]), holding
    /// its admission lock throughout. A directory this store did not mount
    /// is never replaced ([`WORKSPACE_NOT_OWNED`]); a tree that could write
    /// outside its directory is refused ([`UNSAFE_TREE`]).
    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.mount_when(branch, &|| Ok(()))
    }

    fn mount_when(
        &self,
        branch: &BranchName,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let _lock = self.lock_branch(branch.as_str())?;
        ready().map_err(|why| WorkspaceError::Refused {
            code: MOUNT_CANCELLED.to_string(),
            message: why,
        })?;
        self.clear_diagnostics();
        self.mount_locked(branch.as_str())
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        let _lock = self.lock_branch(workspace.sandbox())?;
        self.clear_diagnostics();
        self.commit_locked(workspace, message)
    }

    /// Remove the workspace. Refused, with nothing deleted, when it is not
    /// the directory this store mounted there ([`WORKSPACE_NOT_OWNED`]: a
    /// Git store's workspace of the same name is never touched), while a
    /// worker holds its lease or a process is seen in it
    /// ([`WORKSPACE_BUSY`]), while it holds a `*.jsonl` file
    /// outside its `.transcripts/` ([`WORKSPACE_HOLDS_TRANSCRIPTS`]), or
    /// when a part of it cannot be read ([`WORKSPACE_UNREADABLE`]); a
    /// `.transcripts/` inside it is moved under `.cow/kept/`
    /// ([`CowWorkspace::kept_transcripts`]). The archive beside the
    /// workspaces (`<root>/.transcripts`) is never a workspace and is never
    /// touched.
    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.unmount_when(workspace, &|| Ok(()))
    }

    fn unmount_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<(), WorkspaceError> {
        let _lock = self.lock_branch(workspace.sandbox())?;
        ready()?;
        self.clear_diagnostics();
        self.unmount_locked(workspace)
    }

    fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
        let Ok(directory) = self.checked_path(workspace) else {
            return false;
        };
        let branch = workspace.sandbox();
        if self.swap_path(branch).exists() || self.installing_path(branch).exists() {
            return false;
        }
        let Ok(Some(meta)) = present(&directory) else {
            return false;
        };
        let Ok(Some(state)) = read_state(&self.state_path(branch)) else {
            return false;
        };
        Self::owned(&directory, &meta, &state).is_ok()
    }

    fn hold(&self, workspace: &WorkspaceId) -> Option<Hold> {
        self.lock_branch(workspace.sandbox())
            .ok()
            .map(|lock| Box::new(lock) as Hold)
    }

    /// The Git store's notices of files over the size cap a checkpoint
    /// left out.
    fn notices(&self, workspace: &WorkspaceId) -> Vec<String> {
        self.git.notices(workspace)
    }

    fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]) {
        self.git.notices_told(workspace, notices);
    }

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        self.git.branch_head(branch)
    }

    /// [`CowWorkspace::set_aside_when`] with nothing to wait for.
    fn set_aside(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_when(branch, start, message, &|| Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;

    #[test]
    fn state_round_trips_any_path_bytes() {
        let mut state = State {
            head: "a".repeat(40),
            next: Some("c".repeat(40)),
            recorded: (12, 34),
            mounted: (56, 78),
            entries: BTreeMap::new(),
        };
        state.entries.insert(
            b"dir/we ird\n\xffname".to_vec(),
            Entry {
                mode: 0o100_755,
                sha: "b".repeat(40),
                stamp: Stamp {
                    size: 5,
                    ino: 6,
                    mtime: (7, 8),
                    ctime: (9, 10),
                },
            },
        );
        let parsed = State::parse(&state.render()).expect("parses");
        assert_eq!(parsed.head, state.head);
        assert_eq!(parsed.next, state.next);
        assert_eq!(parsed.recorded, state.recorded);
        assert_eq!(parsed.mounted, state.mounted);
        assert_eq!(parsed.entries, state.entries);
    }

    fn entry(mode: u32, sha: &str, path: &str) -> (u32, String, Vec<u8>) {
        (mode, sha.repeat(40), path.as_bytes().to_vec())
    }

    fn tree(entries: &[(u32, String, Vec<u8>)]) -> TreeEntries {
        entries
            .iter()
            .map(|(mode, sha, path)| (path.clone(), (*mode, sha.clone())))
            .collect()
    }

    fn refused_code<T: std::fmt::Debug>(result: Result<T, WorkspaceError>) -> String {
        match result {
            Err(WorkspaceError::Refused { code, .. }) => code,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_file_no_checkpoint_took_where_a_directory_is_needed_refuses_the_plan() {
        let scratch = Scratch::new("cow-plan-collision");
        let workspace = scratch.join("w");
        fs::create_dir_all(&workspace).unwrap();
        // Untaken (not in `old`): an over-cap or special file at `src`.
        fs::write(workspace.join("src"), "over the cap").unwrap();
        let new = [entry(0o100_644, "a", "src/lib.rs")];
        let code = refused_code(plan_move(&workspace, &BTreeMap::new(), &new));
        assert_eq!(code, WORKSPACE_DIFFERS);
        assert_eq!(
            fs::read_to_string(workspace.join("src")).unwrap(),
            "over the cap"
        );

        // The same file taken by the checkpoint goes first: no refusal.
        let old = tree(&[entry(0o100_644, "b", "src")]);
        let plan = plan_move(&workspace, &old, &new).expect("planned");
        assert_eq!(plan.remove.len(), 1);
        assert_eq!(plan.write.len(), 1);

        // A file at the exact path of a new file is moved aside, no refusal.
        let exact = [entry(0o100_644, "a", "src")];
        assert!(plan_move(&workspace, &BTreeMap::new(), &exact).is_ok());

        // `.spawn` as a file: nothing could be set aside, refused up front.
        fs::write(workspace.join(".spawn"), "x").unwrap();
        let code = refused_code(plan_move(&workspace, &old, &new));
        assert_eq!(code, WORKSPACE_DIFFERS);
    }

    #[test]
    fn a_move_whose_files_changed_since_the_checkpoint_writes_nothing() {
        let scratch = Scratch::new("cow-apply-verify");
        let workspace = scratch.join("w");
        let source = scratch.join("s");
        fs::create_dir_all(workspace.join("dir")).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(workspace.join("dir/old.txt"), "old").unwrap();
        fs::write(source.join("new.txt"), "new").unwrap();
        let old = tree(&[entry(0o100_644, "a", "dir/old.txt")]);
        let new = [entry(0o100_644, "b", "new.txt")];
        let plan = plan_move(&workspace, &old, &new).unwrap();
        let refused = apply_move(&workspace, &source, &plan, b".spawn/set-aside/k", &|_| {
            Err(differs("changed".to_string()))
        });
        assert_eq!(refused_code(refused), WORKSPACE_DIFFERS);
        assert_eq!(
            fs::read_to_string(workspace.join("dir/old.txt")).unwrap(),
            "old"
        );
        assert!(!workspace.join("new.txt").exists());

        // Asked again with the files as the checkpoint took them: done, the
        // emptied directory gone.
        let moved = apply_move(&workspace, &source, &plan, b".spawn/set-aside/k", &|_| {
            Ok(())
        })
        .unwrap();
        assert!(moved.written.contains(b"new.txt".as_slice()));
        assert_eq!(
            fs::read_to_string(workspace.join("new.txt")).unwrap(),
            "new"
        );
        assert!(!workspace.join("dir").exists());
    }

    #[test]
    fn what_is_in_the_way_is_moved_aside_never_over_anything() {
        let scratch = Scratch::new("cow-apply-aside");
        let workspace = scratch.join("w");
        let source = scratch.join("s");
        fs::create_dir_all(workspace.join(".spawn/set-aside/k")).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), "start").unwrap();
        fs::write(workspace.join("a.txt"), "untaken").unwrap();
        // Something already where it would be set aside: never replaced.
        fs::write(workspace.join(".spawn/set-aside/k/a.txt"), "earlier").unwrap();
        let new = [entry(0o100_644, "a", "a.txt")];
        let plan = plan_move(&workspace, &BTreeMap::new(), &new).unwrap();
        assert!(
            apply_move(&workspace, &source, &plan, b".spawn/set-aside/k", &|_| Ok(
                ()
            ))
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "untaken"
        );
        assert_eq!(
            fs::read_to_string(workspace.join(".spawn/set-aside/k/a.txt")).unwrap(),
            "earlier"
        );
        let moved = apply_move(&workspace, &source, &plan, b".spawn/set-aside/j", &|_| {
            Ok(())
        })
        .unwrap();
        assert_eq!(moved.aside, [b"a.txt".to_vec()]);
        assert_eq!(
            fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "start"
        );
        assert_eq!(
            fs::read_to_string(workspace.join(".spawn/set-aside/j/a.txt")).unwrap(),
            "untaken"
        );
    }

    #[test]
    fn racy_stamps_are_not_trusted() {
        let stamp = |seconds| Stamp {
            size: 1,
            ino: 1,
            mtime: (seconds, 0),
            ctime: (seconds, 0),
        };
        assert!(stamp(100).racy((100, 0)));
        assert!(stamp(101).racy((100, 999_999_999)));
        assert!(!stamp(99).racy((100, 0)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn maps_match_only_the_workspace_and_its_descendants() {
        let directory = Path::new("/work/a");
        assert!(maps_inside(
            b"1000-2000 r--p 00000000 00:00 1 /work/a\n",
            directory
        ));
        assert!(maps_inside(
            b"1000-2000 r--p 00000000 00:00 1 /work/a/src/lib.rs\n",
            directory
        ));
        assert!(!maps_inside(
            b"1000-2000 r--p 00000000 00:00 1 /work/a-other\n",
            directory
        ));
        assert!(!maps_inside(
            b"1000-2000 rw-p 00000000 00:00 0 [anonymous]\n",
            directory
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_process_maps_falls_back_to_each_task() {
        let scratch = Scratch::new("cow-task-maps");
        let base = scratch.join("proc/123");
        let task = base.join("task/456");
        fs::create_dir_all(task.join("fd")).expect("task fd");
        fs::write(
            task.join("maps"),
            b"1000-2000 r--p 00000000 00:00 1 /work/a/lib.so\n",
        )
        .expect("task maps");
        assert!(process_inside(&base, Path::new("/work/a")).expect("task maps"));

        // A task that vanishes during the fallback is skipped, as before.
        fs::remove_file(task.join("maps")).expect("remove task maps");
        assert!(!process_inside(&base, Path::new("/work/a")).expect("gone task"));

        // Any other per-task read failure still fails closed.
        fs::create_dir(task.join("maps")).expect("unreadable maps stand-in");
        let error = process_inside(&base, Path::new("/work/a")).expect_err("maps read fails");
        assert!(!exited(&error));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreadable_process_maps_does_not_mask_task_cwd_or_descriptors() {
        // Single task whose `cwd` is inside the workspace while process-level
        // `maps` fails with a non-exited error (`EISDIR`).
        let scratch = Scratch::new("cow-unreadable-maps");
        let base = scratch.join("proc/200");
        let task1 = base.join("task/1");
        fs::create_dir_all(task1.join("fd")).expect("task1 fd");
        fs::create_dir(base.join("maps")).expect("unreadable process maps stand-in");
        std::os::unix::fs::symlink("/work/a", task1.join("cwd")).expect("cwd link");
        assert!(
            process_inside(&base, Path::new("/work/a")).expect("cwd must win before maps"),
            "restricted process maps must not hide a task working inside the workspace"
        );

        // Multi-task fixture: explicitly visit `task1` first so it sits outside
        // the workspace and triggers the deferred `base/maps` failure before
        // `task2` is checked, proving `maps` errors never mask a later task's fd;
        // also check the reversed order and real `read_dir`-backed traversal.
        fs::remove_file(task1.join("cwd")).expect("clear task1 cwd");
        std::os::unix::fs::symlink("/elsewhere", task1.join("cwd")).expect("outside cwd");
        let task2 = base.join("task/2");
        fs::create_dir_all(task2.join("fd")).expect("task2 fd");
        std::os::unix::fs::symlink("/elsewhere", task2.join("cwd")).expect("task2 cwd");
        std::os::unix::fs::symlink("/work/a/open.txt", task2.join("fd/0")).expect("task2 fd link");
        assert!(
            process_inside_tasks(
                &base,
                [Ok(task1.clone()), Ok(task2.clone())],
                Path::new("/work/a")
            )
            .expect("later task descriptor must still be detected after maps failure"),
            "an earlier task's maps error must not skip subsequent tasks' fd checks"
        );
        assert!(
            process_inside_tasks(&base, [Ok(task2.clone()), Ok(task1)], Path::new("/work/a"))
                .expect("descriptor first must succeed immediately")
        );
        assert!(
            process_inside(&base, Path::new("/work/a"))
                .expect("directory traversal must find occupied task")
        );

        // When no task holds cwd, root or fd inside the workspace, the maps
        // failure still propagates so occupancy fails closed as uninspected.
        fs::remove_file(task2.join("fd/0")).expect("clear task2 fd");
        let error = process_inside(&base, Path::new("/work/a"))
            .expect_err("unreadable process maps must fail closed when nothing else matches");
        assert!(!exited(&error));
    }

    #[test]
    fn branch_names_stay_under_the_root() {
        for bad in ["", "-x", ".cow", ".transcripts", "a/b", "..", "a b", "a@b"] {
            assert!(CowWorkspace::checked_branch(bad).is_err(), "{bad:?}");
        }
        assert!(CowWorkspace::checked_branch("agent_001-x").is_ok());
    }

    #[test]
    fn tree_checks_refuse_every_escape() {
        let entry = |mode, path: &str| (mode, "0".repeat(40), path.as_bytes().to_vec());
        let refused = |entries: &[(u32, String, Vec<u8>)]| matches!(check_tree(entries), Err(WorkspaceError::Refused { code, .. }) if code == UNSAFE_TREE);
        for path in [
            "..",
            "a/../b",
            "./a",
            "a//b",
            "/a",
            "a/",
            ".git/config",
            "x/.GIT/hooks/a",
            ".git./a",
            ".git /a",
            "GIT~1/a",
            ".g\u{200c}it/a",
            "a\0b",
        ] {
            assert!(refused(&[entry(0o100_644, path)]), "{path:?}");
        }
        assert!(refused(&[entry(0o100_644, "a"), entry(0o100_644, "a")]));
        assert!(refused(&[
            entry(0o120_000, "evil"),
            entry(0o100_644, "evil/pwn")
        ]));
        assert!(refused(&[
            entry(0o120_000, "Evil"),
            entry(0o100_644, "evil/pwn")
        ]));
        assert!(refused(&[
            entry(0o160_000, "sub"),
            entry(0o100_644, "sub/x")
        ]));
        assert!(refused(&[entry(0o040_000, "dir")]));
        assert!(
            check_tree(&[
                entry(0o100_644, "a/b"),
                entry(0o100_755, "a/c"),
                entry(0o120_000, "l"),
                entry(0o160_000, "vendor/sub"),
                entry(0o100_644, ".gitignore"),
                entry(0o100_644, "git"),
            ])
            .is_ok()
        );
    }

    fn git(directory: &Path, arguments: &[&str]) -> String {
        let output = crate::git::test_git_command()
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .output()
            .expect("git is installed");
        assert!(output.status.success(), "git {arguments:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A repository with `main`, `agent_a` mounted from it.
    fn mounted(scratch: &Scratch) -> (PathBuf, CowWorkspace, WorkspaceId) {
        let checkout = scratch.join("seed");
        fs::create_dir_all(&checkout).expect("mkdir");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        fs::write(checkout.join("README.md"), "base\n").expect("write");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);
        let repo = checkout.join(".git");
        let store = CowWorkspace::new(&repo, scratch.join("work"));
        let branch = BranchName::new("agent_a").expect("branch");
        store.ensure_branch(&branch, None).expect("ensure");
        let workspace = store.mount(&branch).expect("mount");
        (repo, store, workspace)
    }

    /// Review #557 F2: a replacing mount that stops after it carried the
    /// transcripts out of the old workspace, before its final rename, is
    /// undone by the next look: workspace, transcripts and `.spawn/` are
    /// back, and the mount then goes through.
    #[test]
    fn an_interrupted_replacement_is_settled_with_its_transcripts() {
        let scratch = Scratch::new("cow-interrupted");
        let (repo, store, workspace) = mounted(&scratch);
        let directory = PathBuf::from(workspace.path());
        fs::create_dir_all(directory.join(".transcripts")).expect("mkdir");
        fs::write(directory.join(".transcripts/t.jsonl"), "{}\n").expect("write");
        fs::create_dir_all(directory.join(".spawn")).expect("mkdir");
        fs::write(directory.join(".spawn/marker"), "m\n").expect("write");
        let tree = git(&repo, &["rev-parse", "agent_a^{tree}"]);
        let next = git(
            &repo,
            &["commit-tree", &tree, "-p", "agent_a", "-m", "next"],
        );
        git(&repo, &["update-ref", "refs/heads/agent_a", &next]);
        let branch = BranchName::new("agent_a").expect("branch");
        store.faults.interrupted.store(true, Ordering::Relaxed);
        assert!(store.mount(&branch).is_err());
        store.faults.interrupted.store(false, Ordering::Relaxed);
        // As the stopped process left it: no workspace, the transcripts in
        // the staged tree, both places on record.
        assert!(!directory.exists());
        assert!(store.swap_path("agent_a").exists());
        // The next look (taking it offline here) puts the old workspace
        // back whole.
        drop(store.offline(&workspace).expect("offline"));
        assert_eq!(
            fs::read_to_string(directory.join(".transcripts/t.jsonl")).expect("kept"),
            "{}\n"
        );
        assert!(directory.join(".spawn/marker").exists());
        assert!(directory.join("README.md").exists());
        assert!(!store.swap_path("agent_a").exists());
        assert!(!store.installing_path("agent_a").exists());
        let stage: Vec<_> = fs::read_dir(store.cow_dir().join("stage"))
            .expect("stage")
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert!(stage.is_empty(), "{stage:?}");
        // And the replacement then goes through, carrying them.
        store.mount(&branch).expect("mount");
        assert!(directory.join(".transcripts/t.jsonl").exists());
        assert!(directory.join(".spawn/marker").exists());
        assert!(!store.swap_path("agent_a").exists());
    }

    #[test]
    fn a_failed_swap_puts_the_workspace_and_its_transcripts_back() {
        let scratch = Scratch::new("cow-swap");
        let (repo, store, workspace) = mounted(&scratch);
        let directory = PathBuf::from(workspace.path());
        fs::create_dir_all(directory.join(".transcripts")).expect("mkdir");
        fs::write(directory.join(".transcripts/t.jsonl"), "{}\n").expect("write");
        fs::create_dir_all(directory.join(".spawn")).expect("mkdir");
        fs::write(directory.join(".spawn/marker"), "m\n").expect("write");
        // The branch moves on: the clean workspace is replaced at the
        // next mount, and that replacement fails at its last step.
        let tree = git(&repo, &["rev-parse", "agent_a^{tree}"]);
        let next = git(
            &repo,
            &["commit-tree", &tree, "-p", "agent_a", "-m", "next"],
        );
        git(&repo, &["update-ref", "refs/heads/agent_a", &next]);
        store.faults.swap.store(true, Ordering::Relaxed);
        let branch = BranchName::new("agent_a").expect("branch");
        assert!(store.mount(&branch).is_err());
        assert_eq!(
            fs::read_to_string(directory.join(".transcripts/t.jsonl")).expect("kept"),
            "{}\n"
        );
        assert!(directory.join(".spawn/marker").exists());
        assert!(directory.join("README.md").exists());
        let leftovers: Vec<_> = fs::read_dir(store.cow_dir().join("stage"))
            .expect("stage")
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // Without the fault the same mount goes through and carries them.
        store.faults.swap.store(false, Ordering::Relaxed);
        store.mount(&branch).expect("mount");
        assert!(directory.join(".transcripts/t.jsonl").exists());
        assert!(directory.join(".spawn/marker").exists());
    }

    #[test]
    fn a_state_lost_after_the_branch_moved_is_not_taken_for_stale() {
        let scratch = Scratch::new("cow-save");
        let (repo, store, workspace) = mounted(&scratch);
        let directory = PathBuf::from(workspace.path());
        fs::write(directory.join("work.txt"), "one\n").expect("write");
        store.faults.save_after_move.store(true, Ordering::Relaxed);
        assert!(store.commit(&workspace, "one").is_err());
        store.faults.save_after_move.store(false, Ordering::Relaxed);
        let moved = git(&repo, &["rev-parse", "agent_a"]);
        assert_eq!(git(&repo, &["show", "agent_a:work.txt"]), "one");
        // The next checkpoint follows the branch it moved itself.
        assert_eq!(
            store.commit(&workspace, "again").expect("commit"),
            Commit::Unchanged
        );
        fs::write(directory.join("work.txt"), "two\n").expect("write");
        assert!(matches!(
            store.commit(&workspace, "two"),
            Ok(Commit::Created(_))
        ));
        assert_eq!(git(&repo, &["rev-parse", "agent_a^"]), moved);
    }

    /// Review #557 r1 P3: a swap record left after the final rename (its
    /// removal failed) is settled without the exclusive admission lock, so
    /// a worker holding its lease still commits.
    #[test]
    fn a_settled_swap_record_never_refuses_a_leased_worker() {
        let scratch = Scratch::new("cow-record");
        let (_repo, store, workspace) = mounted(&scratch);
        let directory = PathBuf::from(workspace.path());
        let swap = Swap {
            staging: store.cow_dir().join("stage").join("agent_a@gone"),
            trash: Some(store.cow_dir().join("stage").join("old-agent_a@gone")),
        };
        write_whole(&store.swap_path("agent_a"), &swap.render()).expect("record");
        let lease = store
            .lease(&BranchName::new("agent_a").expect("branch"))
            .expect("lease");
        fs::write(directory.join("work.txt"), "one\n").expect("write");
        assert!(matches!(
            store.commit(&workspace, "one"),
            Ok(Commit::Created(_))
        ));
        assert!(!store.swap_path("agent_a").exists());
        drop(lease);
    }

    #[test]
    fn a_state_lost_after_the_final_rename_is_recovered() {
        let scratch = Scratch::new("cow-install");
        let (repo, store, workspace) = mounted(&scratch);
        let directory = PathBuf::from(workspace.path());
        let tree = git(&repo, &["rev-parse", "agent_a^{tree}"]);
        let next = git(
            &repo,
            &["commit-tree", &tree, "-p", "agent_a", "-m", "next"],
        );
        git(&repo, &["update-ref", "refs/heads/agent_a", &next]);
        store.faults.save_after_swap.store(true, Ordering::Relaxed);
        let branch = BranchName::new("agent_a").expect("branch");
        // The swap went through: the mount answers the workspace and says
        // where its state is.
        assert_eq!(store.mount(&branch).expect("mount"), workspace);
        store.faults.save_after_swap.store(false, Ordering::Relaxed);
        assert!(store.installing_path("agent_a").exists());
        assert!(!store.diagnostics().is_empty());
        // The next look takes the installed generation as the state.
        assert_eq!(store.mount(&branch).expect("remount"), workspace);
        assert!(!store.installing_path("agent_a").exists());
        fs::write(directory.join("work.txt"), "one\n").expect("write");
        assert!(matches!(
            store.commit(&workspace, "one"),
            Ok(Commit::Created(_))
        ));
        assert_eq!(git(&repo, &["rev-parse", "agent_a^"]), next);
        store.unmount(&workspace).expect("unmount");
    }

    #[test]
    fn a_baseline_that_fails_early_never_waits_on_a_full_pipe() {
        let scratch = Scratch::new("cow-pipe");
        let checkout = scratch.join("seed");
        fs::create_dir_all(&checkout).expect("mkdir");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        // Far more than a pipe holds, in many blobs.
        for at in 0..64_u8 {
            fs::write(checkout.join(format!("f{at}")), vec![at; 128 * 1024]).expect("write");
        }
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "big"]);
        let store = CowWorkspace::new(checkout.join(".git"), scratch.join("work"));
        store.faults.baseline.store(true, Ordering::Relaxed);
        let branch = BranchName::new("agent_a").expect("branch");
        store.ensure_branch(&branch, None).expect("ensure");
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = done.send(store.mount(&branch).is_err());
            });
            assert!(
                finished
                    .recv_timeout(Duration::from_secs(30))
                    .expect("the failed baseline returns")
            );
        });
        store.faults.baseline.store(false, Ordering::Relaxed);
        store.mount(&branch).expect("mount");
    }

    /// `cargo test -p spawn-host --lib bench_occupancy_of_a_multithreaded_process -- --ignored --nocapture`
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "micro-benchmark: prints occupancy and retirement timings"]
    fn bench_occupancy_of_a_multithreaded_process() {
        const THREADS: usize = 64;
        let scratch = Scratch::new("cow-occupancy-bench");
        let (_repo, store, _workspace) = mounted(&scratch);
        let directory = scratch.join("work/agent_a");
        let start = std::sync::Barrier::new(THREADS + 1);
        let finish = std::sync::Barrier::new(THREADS + 1);
        let (occupancy, occupancy_time, retired, retire_time) = std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    start.wait();
                    finish.wait();
                });
            }
            start.wait();
            let started = std::time::Instant::now();
            let occupancy = directory_occupancy(&directory);
            let occupancy_time = started.elapsed();
            let started = std::time::Instant::now();
            let retired = store.retire("agent_a", &directory);
            let retire_time = started.elapsed();
            finish.wait();
            (occupancy, occupancy_time, retired, retire_time)
        });
        let occupancy = occupancy.expect("occupancy");
        println!(
            "{THREADS} threads: directory_occupancy {occupancy_time:?} ({} pids, {} uninspected); retire {retire_time:?}",
            occupancy.pids.len(),
            occupancy.uninspected.len()
        );
        assert!(!occupancy.pids.contains(&std::process::id()));
        retired.expect("retire");
        assert!(!directory.exists());
    }
}

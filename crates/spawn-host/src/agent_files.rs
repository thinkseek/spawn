//! Files an agent can write, opened the one safe way.
//!
//! Everything under a workspace is written by the agent, which can be a
//! process that keeps running and swaps files while the controller reads
//! them. So the controller never opens such a file by its path: every
//! reader goes through [`open_regular`], which
//! - walks the parent directories from the workspace through held
//!   descriptors (`HeldDirectory`), refusing a symlink or a file in the way;
//! - opens the file itself `O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`
//!   relative to its parent's descriptor, so a symlink is refused and a FIFO
//!   with no writer does not block;
//! - takes the stat from the descriptor (`fstat`) and refuses anything but a
//!   regular file.
//!
//! What it read is that one file, whatever the path names a moment later:
//! callers use the descriptor's stat, never an earlier `lstat`, and read at
//! most a bound ([`read_regular`]) or the descriptor's size. Writers that
//! make or move a file in a workspace go through [`create_new`],
//! [`create_with`] (which never removes anything) and [`rename_within`],
//! which walk the same way.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Mode, OFlags};
use rustix::io::Errno;

use crate::tree_path::TreePath;

/// Why a file in a workspace was not opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentFileError {
    /// Something other than a regular file is at the path, or in the way of
    /// it: a symlink, a FIFO, a socket, a device, a directory, or a file
    /// where a parent directory should be.
    NotRegular(String),
    /// The path is not one a workspace file can have (absolute, `..`, a
    /// `.git` component, empty).
    Unsafe(String),
    /// The host could not do it.
    Failed(String),
}

impl std::fmt::Display for AgentFileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRegular(text) | Self::Unsafe(text) | Self::Failed(text) => {
                formatter.write_str(text)
            }
        }
    }
}

impl std::error::Error for AgentFileError {}

/// A regular file of a workspace, open, with the stat of the descriptor.
#[derive(Debug)]
pub struct Opened {
    /// The open file.
    pub file: File,
    /// Its `fstat`.
    pub metadata: Metadata,
}

fn checked(relative: &TreePath) -> Result<(), AgentFileError> {
    if relative.is_safe() {
        Ok(())
    } else {
        Err(AgentFileError::Unsafe(format!("unsafe path {relative}")))
    }
}

/// The directory that holds `relative` under `top`, walked without following
/// a symlink; `None` when `top` or a parent does not exist.
fn parent(top: &Path, relative: &TreePath) -> Result<Option<HeldDirectory>, AgentFileError> {
    match std::fs::symlink_metadata(top) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AgentFileError::Failed(format!(
                "stat {}: {error}",
                top.display()
            )));
        }
        Ok(_) => {}
    }
    HeldDirectory::existing_parent_of(top, relative).map_err(AgentFileError::NotRegular)
}

/// Open the regular file `relative` (a workspace path) under the directory
/// `top`, the safe way (module docs). `Ok(None)` when it, or a directory
/// above it, does not exist.
pub fn open_regular(top: &Path, relative: &TreePath) -> Result<Option<Opened>, AgentFileError> {
    checked(relative)?;
    let Some(parent) = parent(top, relative)? else {
        return Ok(None);
    };
    let shown = parent
        .shown()
        .join(std::ffi::OsStr::from_bytes(relative.file_name()));
    let descriptor = match rustix::fs::openat(
        parent.descriptor(),
        relative.file_name(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => {
            return Err(AgentFileError::NotRegular(format!(
                "{} is a symlink",
                shown.display()
            )));
        }
        Err(error) => {
            return Err(AgentFileError::Failed(format!(
                "open {}: {error}",
                shown.display()
            )));
        }
    };
    let file = File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| AgentFileError::Failed(format!("stat {}: {error}", shown.display())))?;
    if !metadata.is_file() {
        return Err(AgentFileError::NotRegular(format!(
            "{} is not a regular file",
            shown.display()
        )));
    }
    Ok(Some(Opened { file, metadata }))
}

/// The bytes of the regular file `relative` under `top`, at most `limit` of
/// them, with its stat and whether there was more. `Ok(None)` when it does
/// not exist.
pub fn read_regular(
    top: &Path,
    relative: &TreePath,
    limit: u64,
) -> Result<Option<(Vec<u8>, Metadata, bool)>, AgentFileError> {
    let Some(Opened { file, metadata }) = open_regular(top, relative)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    (&file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| AgentFileError::Failed(format!("read {relative}: {error}")))?;
    let more = bytes.len() as u64 > limit;
    bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(Some((bytes, metadata, more)))
}

/// Create the empty file `relative` under `top`, making missing parent
/// directories, none of them followed through a symlink, and the file
/// itself exclusively (`O_CREAT | O_EXCL | O_NOFOLLOW`). False when
/// something is already at the path.
pub fn create_new(top: &Path, relative: &TreePath) -> Result<bool, AgentFileError> {
    checked(relative)?;
    let parent = HeldDirectory::parent_of(top, relative).map_err(AgentFileError::NotRegular)?;
    match rustix::fs::openat(
        parent.descriptor(),
        relative.file_name(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    ) {
        Ok(_) => Ok(true),
        Err(Errno::EXIST) => Ok(false),
        Err(error) => Err(AgentFileError::Failed(format!(
            "create {relative}: {error}"
        ))),
    }
}

/// What [`create_with`] did. It never removes anything.
#[derive(Debug, PartialEq, Eq)]
pub enum Created {
    /// The file is whole at its name: the very file written and confirmed.
    Written,
    /// Something is already at this name (the file's, or its partial
    /// name): left as it is, nothing written.
    Exists(TreePath),
    /// The bytes written so far are left at the partial name: the write or
    /// its confirmation failed, or the file's name was taken before the
    /// file got it.
    Partial {
        /// Relative path to the leftover partial file.
        partial: TreePath,
        /// Human-readable reason completion or confirmation failed.
        why: String,
    },
    /// As [`Created::Partial`], but the partial name was taken too, so the
    /// bytes written have no name: nothing was named, nothing moved.
    Unkept {
        /// Human-readable reason the leftover partial bytes could not be kept.
        why: String,
    },
}

/// Create the file `relative` under `top` with what `write` writes, never
/// over anything, never removing anything (cc-astra A37: a restore is not a
/// deleter) and never naming anything but the file written (cc-astra A38).
/// Its missing parents are made the way [`create_new`] makes them (none
/// followed through a symlink). The bytes go to a file with no name
/// (`O_TMPFILE`, mode 0600) in the parent: no other entry can stand in for
/// it, as one could at a name between the write and its naming. Once
/// written and synced, `confirm` reads it back through the same descriptor
/// (from its start), and only then is that descriptor's file linked at its
/// name (`linkat`, which never replaces). So:
/// - an entry already at the file's name (a dangling symlink included) is
///   refused before anything is written: [`Created::Exists`];
/// - so is one at the partial name (`relative` with `marker` appended: an
///   earlier restore's partial file), never written over;
/// - a write or a confirmation that fails, or a name taken by another
///   process before the link, links the bytes at the partial name instead
///   (never over anything): [`Created::Partial`], or [`Created::Unkept`]
///   when that name was taken meanwhile;
/// - an entry put at the partial name while the file is written is never
///   moved, named or followed.
///
/// A file system (or a system: macOS) without `O_TMPFILE` is refused:
/// nothing is written.
/// (`spawn store get`, which restores into a directory the caller names.)
pub fn create_with(
    top: &Path,
    relative: &TreePath,
    marker: &str,
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
    confirm: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<Created, AgentFileError> {
    use std::io::{Seek, SeekFrom};
    use std::os::fd::AsRawFd;
    checked(relative)?;
    let mut partial_bytes = relative.as_bytes().to_vec();
    partial_bytes.extend_from_slice(marker.as_bytes());
    let partial = TreePath::from_bytes(partial_bytes);
    checked(&partial)?;
    let parent = HeldDirectory::parent_of(top, relative).map_err(AgentFileError::NotRegular)?;
    for name in [relative, &partial] {
        match rustix::fs::statat(
            parent.descriptor(),
            name.file_name(),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(_) => return Ok(Created::Exists(name.clone())),
            Err(Errno::NOENT) => {}
            Err(error) => return Err(AgentFileError::Failed(format!("stat {name}: {error}"))),
        }
    }
    let descriptor = unnamed_file(&parent).map_err(|error| {
        AgentFileError::Failed(format!(
            "an unnamed file (O_TMPFILE) beside {relative}: {error}; nothing written"
        ))
    })?;
    let mut file = File::from(descriptor);
    // The descriptor's own file, as the kernel names it: linkat follows
    // this one link to the inode, never a name in the directory.
    let own = format!("/proc/self/fd/{}", file.as_raw_fd());
    let link = |name: &TreePath| {
        rustix::fs::linkat(
            CWD,
            own.as_str(),
            parent.descriptor(),
            name.file_name(),
            AtFlags::SYMLINK_FOLLOW,
        )
    };
    let failed = match write(&mut file)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write: {error}"))
        .and_then(|()| {
            file.seek(SeekFrom::Start(0))
                .and_then(|_| confirm(&mut file))
                .map_err(|error| format!("read back: {error}"))
        }) {
        Err(why) => why,
        Ok(()) => match link(relative) {
            Ok(()) => return Ok(Created::Written),
            Err(error) => format!("naming it {relative}: {error}"),
        },
    };
    match link(&partial) {
        Ok(()) => Ok(Created::Partial {
            partial,
            why: failed,
        }),
        Err(error) => Ok(Created::Unkept {
            why: format!("{failed}; and {partial}: {error}"),
        }),
    }
}

/// A file with no name in `parent`, read and write, mode 0600
/// (`O_TMPFILE`, Linux only).
#[cfg(target_os = "linux")]
fn unnamed_file(parent: &HeldDirectory) -> Result<OwnedFd, Errno> {
    rustix::fs::openat(
        parent.descriptor(),
        ".",
        OFlags::RDWR | OFlags::TMPFILE | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
}

/// No file without a name here (macOS has no `O_TMPFILE`): [`create_with`]
/// refuses and writes nothing.
#[cfg(not(target_os = "linux"))]
fn unnamed_file(_parent: &HeldDirectory) -> Result<OwnedFd, Errno> {
    Err(Errno::OPNOTSUPP)
}

/// Write `bytes` as the regular file `relative` under `top`, replacing what
/// is there (a regular file or a symlink, never followed: the entry is
/// unlinked first), its missing parents made the way [`create_new`] makes
/// them, and the file created exclusively.
/// Replace `relative` atomically through a held parent directory descriptor.
pub(crate) fn replace_with(
    top: &Path,
    relative: &TreePath,
    bytes: &[u8],
) -> Result<(), AgentFileError> {
    use std::io::Write;
    checked(relative)?;
    let parent = HeldDirectory::parent_of(top, relative).map_err(AgentFileError::NotRegular)?;
    match rustix::fs::unlinkat(parent.descriptor(), relative.file_name(), AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(error) => {
            return Err(AgentFileError::Failed(format!(
                "remove {relative}: {error}"
            )));
        }
    }
    let descriptor = rustix::fs::openat(
        parent.descriptor(),
        relative.file_name(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )
    .map_err(|error| AgentFileError::Failed(format!("create {relative}: {error}")))?;
    File::from(descriptor)
        .write_all(bytes)
        .map_err(|error| AgentFileError::Failed(format!("write {relative}: {error}")))
}

/// Move the entry `from` to `to`, both under `top`: the parents of `to` are
/// made when missing, and neither side's parents are followed through a
/// symlink. The entry itself is moved as it is (a rename never follows it).
pub fn rename_within(top: &Path, from: &TreePath, to: &TreePath) -> Result<(), AgentFileError> {
    checked(from)?;
    checked(to)?;
    let Some(source) = parent(top, from)? else {
        return Err(AgentFileError::Failed(format!("{from} does not exist")));
    };
    let target = HeldDirectory::parent_of(top, to).map_err(AgentFileError::NotRegular)?;
    rustix::fs::renameat(
        source.descriptor(),
        from.file_name(),
        target.descriptor(),
        to.file_name(),
    )
    .map_err(|error| AgentFileError::Failed(format!("move {from} to {to}: {error}")))
}

/// Remove the entry `relative` under `top` (not a directory), its parents
/// walked without following a symlink. Fine when it is not there.
pub fn remove(top: &Path, relative: &TreePath) -> Result<(), AgentFileError> {
    checked(relative)?;
    let Some(parent) = parent(top, relative)? else {
        return Ok(());
    };
    match rustix::fs::unlinkat(parent.descriptor(), relative.file_name(), AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(AgentFileError::Failed(format!(
            "remove {relative}: {error}"
        ))),
    }
}

/// The names of the entries of the directory `relative` under `top`, the
/// directory and its parents walked without following a symlink (module
/// docs); `.` and `..` are left out, and nothing is said of what an entry
/// is: open each the safe way ([`open_regular`]). Empty when the directory,
/// or one above it, does not exist.
pub fn list(top: &Path, relative: &TreePath) -> Result<Vec<Vec<u8>>, AgentFileError> {
    checked(relative)?;
    let Some(parent) = parent(top, relative)? else {
        return Ok(Vec::new());
    };
    let Some(directory) = parent
        .existing_child(relative.file_name())
        .map_err(AgentFileError::NotRegular)?
    else {
        return Ok(Vec::new());
    };
    let shown = directory.shown().to_path_buf();
    let failed =
        |error: Errno| AgentFileError::Failed(format!("list {}: {error}", shown.display()));
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(directory.descriptor()).map_err(failed)? {
        let name = entry.map_err(failed)?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.push(name);
        }
    }
    Ok(names)
}

/// A directory of a workspace held open by its descriptor. Every entry
/// below it is reached through the descriptor (`openat`, `mkdirat`,
/// `renameat`, `unlinkat`), never by a path from the top, so a component
/// swapped for a symlink after it was opened is never followed: the calls
/// keep working in the directory that was opened, wherever it now is.
/// These calls exist on every Unix this builds for (Linux and macOS alike);
/// nothing depends on `/proc`.
struct HeldDirectory {
    descriptor: OwnedFd,
    /// The directory's path, for messages only.
    shown: PathBuf,
}

/// Flags for opening a directory that must not be a symlink.
fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

impl HeldDirectory {
    /// Open the directory `path` itself, refusing a symlink or anything that
    /// is not a directory at its last component.
    fn open(path: &Path) -> Result<Self, String> {
        let descriptor = rustix::fs::openat(CWD, path, directory_flags(), Mode::empty())
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        Ok(Self {
            descriptor,
            shown: path.to_path_buf(),
        })
    }

    /// The child directory `name` when it exists (`None` when it does not).
    /// A symlink, a file or anything else that is not a directory is refused.
    fn existing_child(&self, name: &[u8]) -> Result<Option<Self>, String> {
        let shown = self.shown.join(OsStr::from_bytes(name));
        match rustix::fs::openat(&self.descriptor, name, directory_flags(), Mode::empty()) {
            Ok(descriptor) => Ok(Some(Self { descriptor, shown })),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(format!(
                "{}: not a directory of the workspace (a symlink or a file is in the way): \
                 {error}",
                shown.display()
            )),
        }
    }

    /// The directory that holds `relative` when every parent of it exists,
    /// each walked without following a symlink; nothing is made.
    fn existing_parent_of(top: &Path, relative: &TreePath) -> Result<Option<Self>, String> {
        let mut parent = Self::open(top)?;
        let mut components: Vec<&[u8]> = relative.components().collect();
        components.pop();
        for component in components {
            match parent.existing_child(component)? {
                Some(child) => parent = child,
                None => return Ok(None),
            }
        }
        Ok(Some(parent))
    }

    /// The held descriptor, for calls relative to it.
    fn descriptor(&self) -> &OwnedFd {
        &self.descriptor
    }

    /// The directory's path, for messages only.
    fn shown(&self) -> &Path {
        &self.shown
    }

    /// The child directory `name`, made when missing. A symlink, a file or
    /// anything else that is not a directory is refused.
    fn child(&self, name: &[u8]) -> Result<Self, String> {
        let shown = self.shown.join(OsStr::from_bytes(name));
        // mkdir never follows a symlink in its last component.
        match rustix::fs::mkdirat(&self.descriptor, name, Mode::from_raw_mode(0o777)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => return Err(format!("create {}: {error}", shown.display())),
        }
        let descriptor =
            rustix::fs::openat(&self.descriptor, name, directory_flags(), Mode::empty()).map_err(
                |error| {
                    format!(
                        "{}: not a directory of the workspace (a symlink or a file is in the way): \
                     {error}",
                        shown.display()
                    )
                },
            )?;
        Ok(Self { descriptor, shown })
    }

    /// The directory that holds `relative` (a safe path), each component
    /// below `top` walked through the one above it ([`Self::child`]).
    fn parent_of(top: &Path, relative: &TreePath) -> Result<Self, String> {
        let mut parent = Self::open(top)?;
        let mut components: Vec<&[u8]> = relative.components().collect();
        components.pop();
        for component in components {
            parent = parent.child(component)?;
        }
        Ok(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentFileError, create_new, list, open_regular, read_regular, remove, rename_within,
    };
    use crate::tree_path::TreePath;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn scratch(tag: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("spawn-agent-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("ws")).expect("scratch");
        directory
    }

    fn path(text: &str) -> TreePath {
        text.into()
    }

    fn mkfifo(at: &Path) {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(at)
                .status()
                .expect("mkfifo")
                .success()
        );
    }

    #[test]
    fn a_directory_is_listed_by_name_and_never_through_a_symlink() {
        let base = scratch("list");
        let ws = base.join("ws");
        assert_eq!(list(&ws, &path(".spawn/acks")), Ok(Vec::new()), "none yet");
        std::fs::create_dir_all(ws.join(".spawn/acks")).expect("mkdir");
        std::fs::write(ws.join(".spawn/acks/b.json"), "{}").expect("write");
        std::fs::write(ws.join(".spawn/acks/a.json"), "{}").expect("write");
        let mut names = list(&ws, &path(".spawn/acks")).expect("listed");
        names.sort();
        assert_eq!(names, vec![b"a.json".to_vec(), b"b.json".to_vec()]);

        // A symlink in place of the directory, or of a parent, is refused.
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        std::fs::write(elsewhere.join("x.json"), "{}").expect("write");
        std::fs::remove_dir_all(ws.join(".spawn/acks")).expect("rm");
        std::os::unix::fs::symlink(&elsewhere, ws.join(".spawn/acks")).expect("symlink");
        assert!(matches!(
            list(&ws, &path(".spawn/acks")),
            Err(AgentFileError::NotRegular(_))
        ));
        std::fs::remove_file(ws.join(".spawn/acks")).expect("rm");
        std::fs::remove_dir_all(ws.join(".spawn")).expect("rm");
        std::os::unix::fs::symlink(&base, ws.join(".spawn")).expect("symlink");
        assert!(list(&ws, &path(".spawn/acks")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_regular_file_is_read_up_to_the_bound_and_its_stat_is_the_descriptors() {
        let base = scratch("read");
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join(".spawn")).expect("mkdir");
        std::fs::write(ws.join(".spawn/report.md"), "0123456789").expect("write");
        let (bytes, metadata, more) = read_regular(&ws, &path(".spawn/report.md"), 4)
            .expect("read")
            .expect("there");
        assert_eq!(
            (bytes.as_slice(), metadata.len(), more),
            (&b"0123"[..], 10, true)
        );
        let (bytes, _, more) = read_regular(&ws, &path(".spawn/report.md"), 64)
            .expect("read")
            .expect("there");
        assert_eq!((bytes.len(), more), (10, false));
        assert!(
            read_regular(&ws, &path("absent"), 4)
                .expect("read")
                .is_none()
        );
        assert!(
            read_regular(&ws, &path("no/such/dir/file"), 4)
                .expect("read")
                .is_none()
        );
        assert!(
            read_regular(&base.join("absent-ws"), &path("x"), 4)
                .expect("read")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn symlinks_are_refused_at_the_file_and_at_every_parent() {
        let base = scratch("symlink");
        let ws = base.join("ws");
        std::fs::write(base.join("secret"), "owner only").expect("secret");
        std::os::unix::fs::symlink(base.join("secret"), ws.join("link")).expect("symlink");
        std::os::unix::fs::symlink(&base, ws.join("up")).expect("symlink");
        assert!(matches!(
            open_regular(&ws, &path("link")),
            Err(AgentFileError::NotRegular(_))
        ));
        assert!(matches!(
            open_regular(&ws, &path("up/secret")),
            Err(AgentFileError::NotRegular(_))
        ));
        assert!(matches!(
            open_regular(&ws, &path("../secret")),
            Err(AgentFileError::Unsafe(_))
        ));
        // Writers do not follow one either.
        assert!(create_new(&ws, &path("up/planted")).is_err());
        assert!(!base.join("planted").exists());
        assert!(!create_new(&ws, &path("link")).expect("something is there"));
        assert_eq!(
            std::fs::read_to_string(base.join("secret")).expect("secret"),
            "owner only"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_fifo_with_no_writer_is_refused_without_blocking() {
        let base = scratch("fifo");
        let ws = base.join("ws");
        mkfifo(&ws.join("fifo"));
        let (done, finished) = std::sync::mpsc::channel();
        let reader = {
            let ws = ws.clone();
            std::thread::spawn(move || {
                let _ = done.send(open_regular(&ws, &path("fifo")).map(|opened| opened.is_some()));
            })
        };
        let answer = finished.recv_timeout(Duration::from_secs(3));
        if answer.is_err() {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .open(ws.join("fifo"));
        }
        reader.join().expect("reader");
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            matches!(answer, Ok(Err(AgentFileError::NotRegular(_)))),
            "{answer:?}"
        );
    }

    #[test]
    fn create_rename_and_remove_stay_inside_the_workspace() {
        let base = scratch("write");
        let ws = base.join("ws");
        assert!(create_new(&ws, &path(".spawn/deliveries/key-1")).expect("create"));
        assert!(!create_new(&ws, &path(".spawn/deliveries/key-1")).expect("again"));
        std::fs::write(ws.join(".spawn/report.md"), "done").expect("report");
        rename_within(
            &ws,
            &path(".spawn/report.md"),
            &path(".spawn/reports/task-1.md"),
        )
        .expect("move");
        assert_eq!(
            std::fs::read_to_string(ws.join(".spawn/reports/task-1.md")).expect("moved"),
            "done"
        );
        // A parent swapped for a symlink out of the workspace is refused.
        std::fs::create_dir_all(base.join("outside")).expect("mkdir");
        std::os::unix::fs::symlink(base.join("outside"), ws.join("out")).expect("symlink");
        assert!(
            rename_within(
                &ws,
                &path(".spawn/reports/task-1.md"),
                &path("out/task-1.md")
            )
            .is_err()
        );
        assert!(remove(&ws, &path("out/anything")).is_err());
        remove(&ws, &path(".spawn/reports/task-1.md")).expect("remove");
        remove(&ws, &path(".spawn/reports/task-1.md")).expect("gone already");
        let _ = std::fs::remove_dir_all(&base);
    }
}

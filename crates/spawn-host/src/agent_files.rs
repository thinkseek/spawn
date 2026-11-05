//! Files an agent can write, opened the one safe way.
//!
//! Everything under a workspace is written by the agent, which can be a
//! process that keeps running and swaps files while the controller reads
//! them. So the controller never opens such a file by its path: every
//! reader goes through [`open_regular`], which
//! - walks the parent directories from the workspace through held
//!   descriptors ([`HeldDirectory`]), refusing a symlink or a file in the way;
//! - opens the file itself `O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`
//!   relative to its parent's descriptor, so a symlink is refused and a FIFO
//!   with no writer does not block;
//! - takes the stat from the descriptor (`fstat`) and refuses anything but a
//!   regular file.
//!
//! What it read is that one file, whatever the path names a moment later:
//! callers use the descriptor's stat, never an earlier `lstat`, and read at
//! most a bound ([`read_regular`]) or the descriptor's size. Writers that
//! make or move a file in a workspace go through [`create_new`] and
//! [`rename_within`], which walk the same way.

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

/// Write `bytes` as the regular file `relative` under `top`, replacing what
/// is there (a regular file or a symlink, never followed: the entry is
/// unlinked first), its missing parents made the way [`create_new`] makes
/// them, and the file created exclusively.
pub fn replace_with(top: &Path, relative: &TreePath, bytes: &[u8]) -> Result<(), AgentFileError> {
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
    use super::{AgentFileError, create_new, open_regular, read_regular, remove, rename_within};
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

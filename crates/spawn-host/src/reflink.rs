//! Descriptor-safe file cloning and exclusive-copy primitives (`clone_or_copy`,
//! `reflink_file`, `reflink_supported`) used by [`crate::D6Workspace`].
//!
//! All file opens use `O_NOFOLLOW` (`O_DIRECTORY` on parent directories,
//! `O_CREAT | O_EXCL | O_NOFOLLOW` on newly created targets) so symlinks and
//! pre-existing targets are refused without following links or clobbering
//! existing files. On Linux (`ioctl_ficlone`) and Apple targets
//! (`fclonefileat`), clones share extents when the filesystem supports it and
//! fall back to byte copies inside [`clone_or_copy`] otherwise.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

const FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::ROTH);

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
fn clone_at(
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

struct DirFd(OwnedFd);

impl DirFd {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self(rustix::fs::open(
            path,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    }
}

/// `source`, open for reading without following a link at its name and
/// without blocking on a FIFO, and the directory `target` goes in, open by
/// descriptor, with `target`'s last component.
fn open_pair<'t>(source: &Path, target: &'t Path) -> io::Result<(fs::File, DirFd, &'t OsStr)> {
    use std::os::unix::fs::OpenOptionsExt as _;
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
pub fn reflink_supported(directory: &Path) -> bool {
    let suffix = unique_suffix();
    let source = directory.join(format!(".reflink-probe-{suffix}"));
    let target = directory.join(format!(".reflink-probe-{suffix}.clone"));
    let probed = fs::write(&source, vec![0x5a_u8; 16 * 1024])
        .and_then(|()| reflink_file(&source, &target))
        .is_ok();
    let _ = fs::remove_file(&target);
    let _ = fs::remove_file(&source);
    probed
}

#[cfg(test)]
mod tests {
    use super::{CloneKind, clone_or_copy, reflink_file, reflink_supported};
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "spawn-reflink-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("mkdir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn noise(seed: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from((seed.wrapping_mul(31) + i) % 251).expect("u8"))
            .collect()
    }

    #[test]
    fn reflink_refuses_an_existing_target() {
        let base = TempDir::new("existing");
        let source = base.0.join("a");
        let target = base.0.join("b");
        fs::write(&source, noise(2, 70_000)).expect("write");
        fs::write(&target, "taken").expect("write");
        assert!(reflink_file(&source, &target).is_err());
        assert_eq!(fs::read_to_string(&target).expect("read"), "taken");
        if reflink_supported(&base.0) {
            fs::remove_file(&target).expect("remove");
            reflink_file(&source, &target).expect("clone");
            assert_eq!(fs::read(&target).expect("read"), noise(2, 70_000));
        }
    }

    #[test]
    fn clone_or_copy_preserves_readonly_source_and_refuses_existing_target() {
        let base = TempDir::new("readonly");
        let source = base.0.join("blob");
        let target = base.0.join("file");
        let payload = noise(6, 70_000);
        fs::write(&source, &payload).expect("write");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o444)).expect("chmod");
        let kind = clone_or_copy(&source, &target).expect("clone");
        if reflink_supported(&base.0) {
            assert_eq!(kind, CloneKind::Reflinked);
        }
        assert_eq!(fs::read(&target).expect("read"), payload);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("chmod 755");
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        fs::File::open(&target)
            .expect("open")
            .set_times(fs::FileTimes::new().set_modified(then))
            .expect("set times");
        let meta = fs::metadata(&target).expect("stat");
        assert_eq!(meta.mode() & 0o777, 0o755);
        assert_eq!(meta.modified().expect("mtime"), then);
        let existing = clone_or_copy(&source, &target).expect_err("target exists");
        assert_eq!(existing.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).expect("read"), payload);
    }

    #[test]
    fn clone_or_copy_refuses_symlink_source_and_target() {
        let base = TempDir::new("symlinks");
        let real = base.0.join("real");
        let link_src = base.0.join("link_src");
        let victim = base.0.join("victim");
        let link_dst = base.0.join("link_dst");
        fs::write(&real, b"real").expect("write real");
        fs::write(&victim, b"keep").expect("write victim");
        symlink(&real, &link_src).expect("symlink src");
        symlink(&victim, &link_dst).expect("symlink dst");

        assert!(clone_or_copy(&link_src, &base.0.join("out")).is_err());
        assert!(clone_or_copy(&real, &link_dst).is_err());
        assert_eq!(fs::read_to_string(&victim).expect("read victim"), "keep");
    }
}

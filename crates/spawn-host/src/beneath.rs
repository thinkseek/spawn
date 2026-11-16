//! Files below one root, never through a link (objective 175 R7, cc-astra
//! A81): every file below the root is opened from a descriptor of the root
//! by `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` (no component
//! may be a symlink, nor a `/proc` magic link, and nothing resolves above
//! the root: no `..` out of it, no absolute path). Directories are made one
//! component at a time (`mkdirat`, then opened the same way); a rename, a
//! link or an unlink is done between held directory descriptors, so their
//! last component is never followed; `EAGAIN` (a rename or a mount
//! elsewhere raced the resolution) is tried again up to 32 times. A link
//! met is an error "… goes through a link or leaves its root: not
//! followed". A regular file with more than one hard link is refused the
//! same way, before a byte is read or written ("<path> has <n> hard links:
//! not followed": `cp -al` of a live archive into scratch makes every copy
//! share its inode with live). Nothing below a root is opened with
//! `O_TRUNC`, and a temporary file is made new (`O_EXCL`) after removing
//! any entry at its name.
//!
//! A root may be pinned ([`Beneath::pinned`], a loop masked by a stand-in
//! location tool): its deepest existing directory is opened, its canonical
//! path must name that same directory and lie outside the account's home
//! (unless the home is a tmpfs here, a gate's sandbox); its device and inode
//! are kept, and every later call reopens it and refuses when they differ,
//! so a root swapped for a link after it was judged is refused. The part of
//! a root not made yet is walked like everything below it.
//!
//! It needs `openat2` (Linux 5.6 or later): an older kernel's `ENOSYS`, or a
//! seccomp filter's `EPERM`, makes every open fail, loudly.
//! Not stood against: a bind mount below a root, and privileged tampering.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

use crate::home::Home;

/// How often an `EAGAIN` from `openat2` is tried again.
const AGAIN: usize = 32;

/// A root's directory as pinned: its canonical path, device and inode, and
/// the part of the root below it that did not exist when it was pinned.
#[derive(Debug, Clone)]
struct Pin {
    base: PathBuf,
    device: u64,
    inode: u64,
    rest: PathBuf,
}

/// Files below `root`, opened as the module says.
#[derive(Debug, Clone)]
pub struct Beneath {
    root: PathBuf,
    pin: Option<Pin>,
}

/// One opened directory below the root, and how to name it in an error.
pub struct Directory {
    fd: OwnedFd,
    shown: PathBuf,
}

impl Directory {
    /// The directory's descriptor.
    #[must_use]
    pub fn descriptor(&self) -> &OwnedFd {
        &self.fd
    }

    /// Make it durable (after a new entry in it).
    ///
    /// # Errors
    ///
    /// The sync's failure, naming the directory.
    pub fn sync(&self) -> Result<(), String> {
        rustix::fs::fsync(&self.fd)
            .map_err(|error| format!("sync {}: {error}", self.shown.display()))
    }
}

#[cfg(test)]
type Step = Box<dyn Fn() + Send + Sync>;

/// A step run between a pinned root's open and its canonicalization, per
/// root, so a test sees the check that both name one directory without a
/// race.
#[cfg(test)]
static BETWEEN: std::sync::Mutex<Vec<(PathBuf, Step)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn between_open_and_canonical(root: &Path, step: Step) {
    BETWEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((root.to_path_buf(), step));
}

/// The error of `errno` met at `shown`.
fn refusal(shown: &Path, what: &str, errno: Errno) -> String {
    if errno == Errno::LOOP || errno == Errno::XDEV {
        format!(
            "{} goes through a link or leaves its root: not followed",
            shown.display()
        )
    } else {
        format!(
            "{what} {}: {}",
            shown.display(),
            std::io::Error::from(errno)
        )
    }
}

/// `openat2` below `dir`, never through a link, `EAGAIN` tried again.
fn open_below(dir: &impl AsFd, path: &Path, flags: OFlags, mode: Mode) -> Result<OwnedFd, Errno> {
    let mut tries = 0;
    loop {
        match rustix::fs::openat2(
            dir,
            path,
            flags | OFlags::CLOEXEC,
            mode,
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        ) {
            Err(Errno::AGAIN) if tries < AGAIN => tries += 1,
            other => return other,
        }
    }
}

/// `relative` as plain components: no root, no `.` or `..`.
fn plain(relative: &Path) -> Result<(), String> {
    if relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Ok(())
    } else {
        Err(format!(
            "{} is not a plain relative path: not followed",
            relative.display()
        ))
    }
}

/// A file's hard links refused past one.
fn one_link(file: &File, shown: &Path) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("stat {}: {error}", shown.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", shown.display()));
    }
    if metadata.nlink() > 1 {
        return Err(format!(
            "{} has {} hard links: not followed",
            shown.display(),
            metadata.nlink()
        ));
    }
    Ok(())
}

impl Beneath {
    /// Files below `root`, its own path followed as it is at each call.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            pin: None,
        }
    }

    /// Files below `root`, pinned as the module says; refused when its
    /// directory is inside the account's `home` (unless a tmpfs), or the
    /// directory opened is not the one its canonical path names.
    ///
    /// # Errors
    ///
    /// Why the root may not be pinned: paths only.
    pub fn pinned(root: impl Into<PathBuf>, home: &Home) -> Result<Self, String> {
        let root = root.into();
        let mut base = root.clone();
        let mut rest = Vec::new();
        while !base.is_dir() {
            let (Some(parent), Some(name)) = (base.parent(), base.file_name()) else {
                return Err(format!("{} has no directory to pin", root.display()));
            };
            rest.push(name.to_os_string());
            base = parent.to_path_buf();
        }
        let opened =
            File::open(&base).map_err(|error| format!("open {}: {error}", base.display()))?;
        let held = opened
            .metadata()
            .map_err(|error| format!("stat {}: {error}", base.display()))?;
        #[cfg(test)]
        for (at, step) in BETWEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            if *at == root {
                step();
            }
        }
        let canonical = std::fs::canonicalize(&base)
            .map_err(|error| format!("resolve {}: {error}", base.display()))?;
        let named = std::fs::metadata(&canonical)
            .map_err(|error| format!("stat {}: {error}", canonical.display()))?;
        if (named.dev(), named.ino()) != (held.dev(), held.ino()) {
            return Err(format!(
                "{}: {} was replaced while it was pinned: not followed",
                root.display(),
                base.display()
            ));
        }
        if let Some(why) = home.refuses(&canonical, &root) {
            return Err(why);
        }
        Ok(Self {
            root,
            pin: Some(Pin {
                base: canonical,
                device: held.dev(),
                inode: held.ino(),
                rest: rest.iter().rev().collect(),
            }),
        })
    }

    /// The root as configured.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the root is pinned.
    #[must_use]
    pub fn is_pinned(&self) -> bool {
        self.pin.is_some()
    }

    /// `path`, below the root, as a path relative to it; `None` when it is
    /// not below it.
    #[must_use]
    pub fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
    }

    /// The directory paths are resolved from, and the part of the root
    /// below it; `None` when an unpinned root does not exist.
    fn base(&self) -> Result<Option<(OwnedFd, &Path)>, String> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        match &self.pin {
            None => match rustix::fs::open(&self.root, flags, Mode::empty()) {
                Ok(fd) => Ok(Some((fd, Path::new("")))),
                Err(Errno::NOENT) => Ok(None),
                Err(errno) => Err(refusal(&self.root, "open", errno)),
            },
            Some(pin) => {
                let fd = rustix::fs::open(&pin.base, flags | OFlags::NOFOLLOW, Mode::empty())
                    .map_err(|errno| {
                        format!(
                            "{} (the pinned directory of {}) cannot be opened: {}: not followed",
                            pin.base.display(),
                            self.root.display(),
                            std::io::Error::from(errno)
                        )
                    })?;
                let stat =
                    rustix::fs::fstat(&fd).map_err(|errno| refusal(&pin.base, "stat", errno))?;
                if (stat.st_dev, stat.st_ino) != (pin.device, pin.inode) {
                    return Err(format!(
                        "{} is no longer the directory pinned for {}: not followed",
                        pin.base.display(),
                        self.root.display()
                    ));
                }
                Ok(Some((fd, pin.rest.as_path())))
            }
        }
    }

    fn shown(&self, relative: &Path) -> PathBuf {
        self.root.join(relative)
    }

    /// The directory `relative`, opened below the root; `None` when it (or
    /// the root) does not exist.
    ///
    /// # Errors
    ///
    /// A link met, or another failure, naming the path.
    pub fn open_dir(&self, relative: &Path) -> Result<Option<Directory>, String> {
        plain(relative)?;
        let shown = self.shown(relative);
        let Some((base, rest)) = self.base()? else {
            return Ok(None);
        };
        let path = rest.join(relative);
        if path.as_os_str().is_empty() {
            return Ok(Some(Directory { fd: base, shown }));
        }
        match open_below(
            &base,
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        ) {
            Ok(fd) => Ok(Some(Directory { fd, shown })),
            Err(Errno::NOENT) => Ok(None),
            Err(errno) => Err(refusal(&shown, "open", errno)),
        }
    }

    /// The directory `relative`, made one component at a time where it is
    /// missing, each new one made durable in its parent.
    ///
    /// # Errors
    ///
    /// A link met, or another failure, naming the path.
    pub fn make_dir(&self, relative: &Path) -> Result<Directory, String> {
        plain(relative)?;
        let shown = self.shown(relative);
        let base = if let Some((base, rest)) = self.base()? {
            (base, rest.to_path_buf())
        } else {
            std::fs::create_dir_all(&self.root)
                .map_err(|error| format!("create {}: {error}", self.root.display()))?;
            self.base()?
                .map(|(base, rest)| (base, rest.to_path_buf()))
                .ok_or_else(|| format!("{} is gone", self.root.display()))?
        };
        let (mut dir, rest) = base;
        let mut walked = PathBuf::new();
        for component in rest.join(relative).components() {
            walked.push(component);
            let name = Path::new(component.as_os_str());
            let flags = OFlags::RDONLY | OFlags::DIRECTORY;
            let next = match open_below(&dir, name, flags, Mode::empty()) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => {
                    match rustix::fs::mkdirat(&dir, name, Mode::from(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(errno) => return Err(refusal(&shown, "create", errno)),
                    }
                    let made = open_below(&dir, name, flags, Mode::empty())
                        .map_err(|errno| refusal(&shown, "open", errno))?;
                    rustix::fs::fsync(&dir).map_err(|errno| refusal(&shown, "sync", errno))?;
                    made
                }
                Err(errno) => return Err(refusal(&shown, "open", errno)),
            };
            dir = next;
        }
        Ok(Directory { fd: dir, shown })
    }

    /// The regular file `relative`, opened to read; `None` when it does not
    /// exist. One with more than one hard link is refused.
    ///
    /// # Errors
    ///
    /// A link met, a second hard link, or another failure, naming the path.
    pub fn open_read(&self, relative: &Path) -> Result<Option<File>, String> {
        plain(relative)?;
        let shown = self.shown(relative);
        let Some((base, rest)) = self.base()? else {
            return Ok(None);
        };
        match open_below(
            &base,
            &rest.join(relative),
            OFlags::RDONLY | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let file = File::from(fd);
                one_link(&file, &shown)?;
                Ok(Some(file))
            }
            Err(Errno::NOENT) => Ok(None),
            Err(errno) => Err(refusal(&shown, "open", errno)),
        }
    }

    /// The whole of the regular file `relative`; `None` when it does not
    /// exist.
    ///
    /// # Errors
    ///
    /// As [`Self::open_read`], and a failed read.
    pub fn read(&self, relative: &Path) -> Result<Option<Vec<u8>>, String> {
        let Some(mut file) = self.open_read(relative)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| format!("read {}: {error}", self.shown(relative).display()))?;
        Ok(Some(bytes))
    }

    /// The file `name` in `dir`, opened to append (made when `create`):
    /// the file and whether it was made. Never truncated; one with more
    /// than one hard link is refused.
    ///
    /// # Errors
    ///
    /// A link met, a second hard link, or another failure, naming the path.
    pub fn open_append(
        &self,
        dir: &Directory,
        name: &str,
        create: bool,
    ) -> Result<Option<(File, bool)>, String> {
        let shown = dir.shown.join(name);
        let flags = OFlags::WRONLY | OFlags::APPEND | OFlags::NONBLOCK;
        let opened = match open_below(&dir.fd, Path::new(name), flags, Mode::empty()) {
            Ok(fd) => Some((fd, false)),
            Err(Errno::NOENT) if create => {
                match open_below(
                    &dir.fd,
                    Path::new(name),
                    flags | OFlags::CREATE | OFlags::EXCL,
                    Mode::from(0o666),
                ) {
                    Ok(fd) => Some((fd, true)),
                    Err(errno) => return Err(refusal(&shown, "create", errno)),
                }
            }
            Err(Errno::NOENT) => None,
            Err(errno) => return Err(refusal(&shown, "open", errno)),
        };
        let Some((fd, made)) = opened else {
            return Ok(None);
        };
        let file = File::from(fd);
        one_link(&file, &shown)?;
        Ok(Some((file, made)))
    }

    /// The names in the directory `relative` and whether each is a
    /// directory (as its entry says, not followed); empty when it does not
    /// exist.
    ///
    /// # Errors
    ///
    /// A link met, or another failure, naming the path.
    pub fn list(&self, relative: &Path) -> Result<Vec<(String, bool)>, String> {
        let Some(dir) = self.open_dir(relative)? else {
            return Ok(Vec::new());
        };
        let mut names = Vec::new();
        let mut entries = rustix::fs::Dir::read_from(&dir.fd)
            .map_err(|errno| refusal(&dir.shown, "list", errno))?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(|errno| refusal(&dir.shown, "list", errno))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            let is_dir = match entry.file_type() {
                rustix::fs::FileType::Directory => true,
                rustix::fs::FileType::Unknown => {
                    rustix::fs::statat(&dir.fd, name.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok_and(
                        |stat| {
                            rustix::fs::FileType::from_raw_mode(stat.st_mode)
                                == rustix::fs::FileType::Directory
                        },
                    )
                }
                _ => false,
            };
            names.push((name, is_dir));
        }
        Ok(names)
    }

    /// A temporary file beside `name` in `dir`, made new (`O_EXCL`) after
    /// any entry at its name is removed, with `mode` exactly (else as the
    /// umask makes it), holding `bytes`, synced: its name.
    fn temporary(
        dir: &Directory,
        name: &str,
        what: &str,
        bytes: &[u8],
        mode: Option<u32>,
    ) -> Result<String, String> {
        let temporary = format!(".{name}.{}.{what}", std::process::id());
        let shown = dir.shown.join(&temporary);
        match rustix::fs::unlinkat(&dir.fd, temporary.as_str(), AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(errno) => return Err(refusal(&shown, "remove", errno)),
        }
        let fd = open_below(
            &dir.fd,
            Path::new(&temporary),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::from(mode.unwrap_or(0o666)),
        )
        .map_err(|errno| refusal(&shown, "create", errno))?;
        let mut file = File::from(fd);
        let written = file
            .write_all(bytes)
            .and_then(|()| match mode {
                Some(mode) => {
                    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))
                }
                None => Ok(()),
            })
            .and_then(|()| file.sync_all());
        if let Err(error) = written {
            let _ = rustix::fs::unlinkat(&dir.fd, temporary.as_str(), AtFlags::empty());
            return Err(format!("write {}: {error}", shown.display()));
        }
        Ok(temporary)
    }

    /// Replace the file `name` in `dir` whole with `bytes`: a temporary
    /// file renamed over it; `durable` syncs the directory after.
    ///
    /// # Errors
    ///
    /// A link met, or another failure, naming the path.
    pub fn replace_whole(
        &self,
        dir: &Directory,
        name: &str,
        bytes: &[u8],
        durable: bool,
    ) -> Result<(), String> {
        let temporary = Self::temporary(dir, name, "tmp", bytes, None)?;
        if let Err(errno) = rustix::fs::renameat(&dir.fd, temporary.as_str(), &dir.fd, name) {
            let _ = rustix::fs::unlinkat(&dir.fd, temporary.as_str(), AtFlags::empty());
            return Err(refusal(&dir.shown.join(name), "rename", errno));
        }
        if durable {
            dir.sync()?;
        }
        Ok(())
    }

    /// Make the file `relative` hold `bytes` unless something is there:
    /// whether it was written. A temporary file is linked into place, which
    /// never replaces an entry, so the file appears whole or not at all.
    ///
    /// # Errors
    ///
    /// A link met, or another failure, naming the path.
    pub fn write_new(&self, relative: &Path, bytes: &[u8]) -> Result<bool, String> {
        let (parent, name) = split(relative)?;
        let dir = self.make_dir(parent)?;
        let temporary = Self::temporary(&dir, &name, "spawn-write", bytes, None)?;
        let linked = rustix::fs::linkat(
            &dir.fd,
            temporary.as_str(),
            &dir.fd,
            name.as_str(),
            AtFlags::empty(),
        );
        let _ = rustix::fs::unlinkat(&dir.fd, temporary.as_str(), AtFlags::empty());
        match linked {
            Ok(()) => {
                dir.sync()?;
                Ok(true)
            }
            Err(Errno::EXIST) => Ok(false),
            Err(errno) => Err(refusal(&self.shown(relative), "write", errno)),
        }
    }

    /// Replace the regular file `relative` with `bytes` if it holds exactly
    /// `current`, never with fewer bytes: whether it was replaced. The
    /// bytes go to a temporary file beside it with its mode, synced, and a
    /// rename puts it in place.
    ///
    /// # Errors
    ///
    /// A link met, a second hard link, or another failure, naming the path.
    pub fn replace_grown(
        &self,
        relative: &Path,
        bytes: &[u8],
        current: &[u8],
    ) -> Result<bool, String> {
        let shown = self.shown(relative);
        if bytes.len() < current.len() {
            return Err(format!(
                "replacing {} would shorten it: {} bytes for {}",
                shown.display(),
                bytes.len(),
                current.len()
            ));
        }
        let (parent, name) = split(relative)?;
        let Some(dir) = self.open_dir(parent)? else {
            return Ok(false);
        };
        let Some(mut file) = self.open_read(relative)? else {
            return Ok(false);
        };
        let mode = file
            .metadata()
            .map_err(|error| format!("stat {}: {error}", shown.display()))?
            .mode()
            & 0o7777;
        let mut held = Vec::new();
        file.read_to_end(&mut held)
            .map_err(|error| format!("read {}: {error}", shown.display()))?;
        if held != current {
            return Ok(false);
        }
        let temporary = Self::temporary(&dir, &name, "spawn-replace", bytes, Some(mode))?;
        if let Err(errno) =
            rustix::fs::renameat(&dir.fd, temporary.as_str(), &dir.fd, name.as_str())
        {
            let _ = rustix::fs::unlinkat(&dir.fd, temporary.as_str(), AtFlags::empty());
            return Err(refusal(&shown, "replace", errno));
        }
        dir.sync()?;
        Ok(true)
    }
}

/// `relative`'s directory and file name.
fn split(relative: &Path) -> Result<(&Path, String), String> {
    plain(relative)?;
    match (relative.parent(), relative.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, name.to_string_lossy().into_owned())),
        _ => Err(format!("{} names no file", relative.display())),
    }
}

#[cfg(test)]
mod tests;

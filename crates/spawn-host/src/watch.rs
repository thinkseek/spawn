//! Change watches on the files a running engine writes, so the Bridge reads
//! a transcript or an exit status when it changed instead of on every pass.
//!
//! On Linux this is inotify ([`rustix::fs::inotify`]): one non-blocking
//! descriptor, one watch per directory that holds a watched file, and a
//! drain that costs no system call beyond the one that reads the ready
//! events. A path that does not exist yet is watched through the deepest
//! directory above it that does, and the watch moves down as the directories
//! appear: a transcript is filed under `<projects>/<escaped workspace>/`,
//! which an engine creates when it starts, and an exit status under
//! `<workspace>/.spawn/`, which its runner creates when the engine exits.
//!
//! Everywhere else there is no adapter, and [`InotifyWatch::new`] fails: the
//! Bridge then reads its files on every pass, exactly as it did before
//! watches, and says so once.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use spawn_core::port::{FileWatch, WatchError};

/// Bytes of change events one read takes; the queue is read until it is
/// empty. The kernel drops an event that does not fit its queue and sets
/// `IN_Q_OVERFLOW`, which is reported as a change of every key.
const EVENT_BUFFER: usize = 8192;

/// A watch on engine files through the platform's change notifications.
///
/// One descriptor serves every key, so a fleet of agents costs one watch per
/// directory it writes into, not one process per agent.
#[derive(Debug)]
pub struct InotifyWatch {
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
    state: Mutex<State>,
}

/// What is watched now.
#[derive(Debug, Default)]
struct State {
    /// Watch descriptor → the directory it watches and the keys served by
    /// it.
    dirs: BTreeMap<i32, Dir>,
    /// Every `(key, path)` and the directory watched for it.
    wanted: BTreeMap<(String, String), PathBuf>,
    /// Keys with a change no [`FileWatch::drain`] took yet.
    pending: BTreeSet<String>,
}

/// One watched directory.
#[derive(Debug)]
struct Dir {
    path: PathBuf,
    keys: BTreeSet<String>,
}

/// One change event, copied out of the descriptor's buffer.
struct Change {
    descriptor: i32,
    flags: u32,
    name: Option<String>,
}

#[cfg(target_os = "linux")]
impl InotifyWatch {
    /// A watch with its own descriptor.
    pub fn new() -> Result<Self, WatchError> {
        use rustix::fs::inotify::{self, CreateFlags};
        let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .map_err(|error| WatchError(format!("inotify_init1: {error}")))?;
        Ok(Self {
            fd,
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The flags a watch reports: a write, a creation, a replacement, a
    /// removal, and a directory appearing or going on the way to the file.
    fn flags() -> rustix::fs::inotify::WatchFlags {
        use rustix::fs::inotify::WatchFlags;
        WatchFlags::CREATE
            | WatchFlags::MODIFY
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::ATTRIB
            | WatchFlags::ONLYDIR
    }

    /// Carry `key`'s `path` to the deepest directory above it that exists:
    /// [`FileWatch::watch`] calls it here and again whenever a change may
    /// have made a deeper one.
    fn carry(&self, state: &mut State, key: &str, path: &str) -> Result<(), WatchError> {
        let file = Path::new(path);
        let Some(anchor) = anchor(file) else {
            return Err(WatchError(format!(
                "no directory of {path} exists to watch"
            )));
        };
        let descriptor = match state.dirs.iter().find(|(_, dir)| dir.path == anchor) {
            Some((descriptor, _)) => *descriptor,
            None => {
                let descriptor =
                    rustix::fs::inotify::add_watch(&self.fd, &anchor, Self::flags())
                        .map_err(|error| WatchError(format!("{}: {error}", anchor.display())))?;
                state.dirs.insert(
                    descriptor,
                    Dir {
                        path: anchor.clone(),
                        keys: BTreeSet::new(),
                    },
                );
                descriptor
            }
        };
        if let Some(dir) = state.dirs.get_mut(&descriptor) {
            dir.keys.insert(key.to_string());
        }
        let previous = state
            .wanted
            .insert((key.to_string(), path.to_string()), anchor);
        if let Some(previous) = previous {
            self.release(state, key, &previous);
        }
        Ok(())
    }

    /// Stop serving `key` from `dir` when it has moved to another and no
    /// other key of the watch uses it.
    fn release(&self, state: &mut State, key: &str, dir: &Path) {
        let Some((descriptor, _)) = state
            .dirs
            .iter_mut()
            .find(|(_, known)| known.path == dir)
        else {
            return;
        };
        let descriptor = *descriptor;
        let unused = state.dirs.get_mut(&descriptor).is_some_and(|known| {
            known.keys.remove(key);
            known.keys.is_empty()
        });
        if unused {
            let _ = rustix::fs::inotify::remove_watch(&self.fd, descriptor);
            state.dirs.remove(&descriptor);
        }
    }

    /// Take every ready event, pairing each with the keys that asked for
    /// its directory.
    fn read_changes(&self) -> Vec<Change> {
        use rustix::fs::inotify::{self, Reader};
        let mut buffer = [std::mem::MaybeUninit::uninit(); EVENT_BUFFER];
        let mut reader = Reader::new(&self.fd, &mut buffer);
        let mut changes = Vec::new();
        loop {
            match reader.next() {
                Ok(event) => changes.push(Change {
                    descriptor: event.wd(),
                    flags: event.events().bits(),
                    name: event
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned()),
                }),
                Err(_) => return changes,
            }
        }
    }

    /// Whether `name`, an entry of `dir`, is on the way to `file`: the
    /// entry itself, or a directory the rest of the path goes through.
    fn leads_to(dir: &Path, name: &str, file: &Path) -> bool {
        let Ok(relative) = file.strip_prefix(dir) else {
            return false;
        };
        relative
            .components()
            .next()
            .is_some_and(|first| first.as_os_str() == name)
    }
}

#[cfg(not(target_os = "linux"))]
impl InotifyWatch {
    /// No adapter on this platform: the Bridge reads its files on every
    /// pass, as it did before watches.
    pub fn new() -> Result<Self, WatchError> {
        Err(WatchError(
            "no change watch on this platform".to_string(),
        ))
    }
}

impl FileWatch for InotifyWatch {
    fn watch(&self, key: &str, path: &str) -> Result<(), WatchError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (key, path);
            Err(WatchError(
                "no change watch on this platform".to_string(),
            ))
        }
        #[cfg(target_os = "linux")]
        {
            let mut state = self.lock();
            if state
                .wanted
                .get(&(key.to_string(), path.to_string()))
                .is_some_and(|anchor| state.dirs.values().any(|dir| dir.path == *anchor))
            {
                return Ok(());
            }
            self.carry(&mut state, key, path)
        }
    }

    fn unwatch(&self, key: &str) {
        let mut state = self.lock();
        let paths: Vec<String> = state
            .wanted
            .keys()
            .filter(|(owner, _)| owner == key)
            .map(|(_, path)| path.clone())
            .collect();
        for path in paths {
            if let Some(anchor) = state.wanted.remove(&(key.to_string(), path)) {
                self.release(&mut state, key, &anchor);
            }
        }
        state.pending.remove(key);
    }

    fn drain(&self) -> Vec<String> {
        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::inotify::ReadFlags;
            let mut state = self.lock();
            let changes = self.read_changes();
            let mut deeper: BTreeSet<String> = BTreeSet::new();
            for change in changes {
                if change.flags & ReadFlags::QUEUE_OVERFLOW.bits() != 0 {
                    state.pending.extend(state.wanted.keys().map(|(key, _)| key.clone()));
                    continue;
                }
                let Some(dir) = state.dirs.get(&change.descriptor) else {
                    continue;
                };
                if change.flags & ReadFlags::IGNORED.bits() != 0 {
                    // The directory went: every key it served is carried
                    // again by the next watch, and is due now.
                    let keys = dir.keys.clone();
                    state.dirs.remove(&change.descriptor);
                    state.pending.extend(keys.iter().cloned());
                    deeper.extend(keys);
                    continue;
                }
                let path = dir.path.clone();
                let keys = dir.keys.clone();
                for key in keys {
                    let interested = state
                        .wanted
                        .iter()
                        .filter(|((owner, _), _)| *owner == key)
                        .any(|((_, path), _)| match change.name.as_deref() {
                            Some(name) => Self::leads_to(&path_of(&path), name, Path::new(path)),
                            None => true,
                        });
                    if interested {
                        state.pending.insert(key.clone());
                        // A directory on the way may have appeared: the
                        // watch moves down to it.
                        if change.flags & ReadFlags::ISDIR.bits() != 0
                            || change.flags & ReadFlags::CREATE.bits() != 0
                            || change.flags & ReadFlags::MOVED_TO.bits() != 0
                        {
                            deeper.insert(key);
                        }
                    }
                }
                let _ = path;
            }
            for key in deeper {
                let paths: Vec<String> = state
                    .wanted
                    .keys()
                    .filter(|(owner, _)| *owner == key)
                    .map(|(_, path)| path.clone())
                    .collect();
                for path in paths {
                    // A path that cannot be carried again keeps its old
                    // watch: a change is reported, never a silence.
                    let _ = self.carry(&mut state, &key, &path);
                }
            }
            std::mem::take(&mut state.pending).into_iter().collect()
        }
    }
}

/// The deepest directory above `file` that exists, itself included when it
/// is a directory.
fn anchor(file: &Path) -> Option<PathBuf> {
    let mut candidate = if file.is_dir() {
        file.to_path_buf()
    } else {
        file.parent()?.to_path_buf()
    };
    loop {
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !candidate.pop() {
            return None;
        }
    }
}

/// A path as [`Path`].
fn path_of(path: &str) -> PathBuf {
    PathBuf::from(path)
}

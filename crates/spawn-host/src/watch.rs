//! Change watches on the files a running engine writes, so the Bridge reads
//! a transcript or an exit status when it changed instead of on every pass.
//!
//! On Linux this is inotify ([`rustix::fs::inotify`]): one non-blocking
//! descriptor, one watch per directory that holds a watched file, and a
//! drain that costs one system call, the read of the ready events. A path
//! that does not exist yet is watched through the deepest directory above it
//! that does, and the watch moves down as the directories appear: a
//! transcript is filed under `<projects>/<escaped workspace>/`, which an
//! engine creates when it starts, and an exit status under
//! `<workspace>/.spawn/`, which its runner creates when the engine exits.
//!
//! Everywhere else there is no adapter and [`InotifyWatch::new`] fails: the
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
    /// Keys whose watch moved down a directory since the last drain: what
    /// the change was may have followed the move, so they are due once more.
    recheck: BTreeSet<String>,
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

impl InotifyWatch {
    /// A watch with its own descriptor.
    #[cfg(target_os = "linux")]
    pub fn new() -> Result<Self, WatchError> {
        use rustix::fs::inotify::{self, CreateFlags};
        let fd = inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .map_err(|error| WatchError(format!("inotify_init1: {error}")))?;
        Ok(Self {
            fd,
            state: Mutex::new(State::default()),
        })
    }

    /// No adapter on this platform: the Bridge reads its files on every
    /// pass, as it did before watches.
    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Result<Self, WatchError> {
        Err(WatchError(
            "no change watch on this platform".to_string(),
        ))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(target_os = "linux")]
impl InotifyWatch {
    /// What a watch reports: a write, a creation, a replacement, a removal,
    /// and a directory appearing or going on the way to the file.
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
    /// [`FileWatch::watch`] calls it, and so does the drain whenever a
    /// change may have put a deeper one in place.
    fn carry(&self, state: &mut State, key: &str, path: &str) -> Result<(), WatchError> {
        let file = Path::new(path);
        let Some(anchor) = anchor(file) else {
            return Err(WatchError(format!("no directory of {path} exists to watch")));
        };
        let descriptor = match state.dirs.iter().find(|(_, dir)| dir.path == anchor) {
            Some((descriptor, _)) => *descriptor,
            None => {
                let descriptor = rustix::fs::inotify::add_watch(&self.fd, &anchor, Self::flags())
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
            .insert((key.to_string(), path.to_string()), anchor.clone());
        match previous {
            Some(previous) if previous != anchor => {
                self.release(state, key, &previous);
                // The change that moved the watch may have landed in the
                // directory just joined: it is due once more.
                state.recheck.insert(key.to_string());
            }
            Some(_) => {}
            None => {}
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

    /// Take every ready event, copying what it says out of the buffer.
    fn read_changes(&self) -> Vec<Change> {
        use rustix::fs::inotify::Reader;
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

    /// Whether `name`, an entry of `dir`, is on the way to `file`: the entry
    /// itself, or a directory the rest of the path goes through.
    fn leads_to(dir: &Path, name: &str, file: &Path) -> bool {
        file.strip_prefix(dir)
            .ok()
            .and_then(|relative| relative.components().next())
            .is_some_and(|first| first.as_os_str() == name)
    }

    fn drain_linux(&self) -> Vec<String> {
        use rustix::fs::inotify::ReadFlags;
        let mut state = self.lock();
        let recheck = std::mem::take(&mut state.recheck);
        state.pending.extend(recheck);
        let changes = self.read_changes();
        let mut moved = BTreeSet::new();
        for change in changes {
            if change.flags & ReadFlags::QUEUE_OVERFLOW.bits() != 0 {
                // Events were dropped: every key is due.
                let keys: Vec<String> = state.wanted.keys().map(|(key, _)| key.clone()).collect();
                state.pending.extend(keys);
                continue;
            }
            let Some(dir) = state.dirs.get(&change.descriptor) else {
                continue;
            };
            let anchor = dir.path.clone();
            let keys = dir.keys.clone();
            if change.flags & ReadFlags::IGNORED.bits() != 0 {
                // The directory went: every key it served is due again, and
                // the next watch carries it where it now lives.
                state.dirs.remove(&change.descriptor);
                for key in keys {
                    state.pending.insert(key.clone());
                    moved.insert(key);
                }
                continue;
            }
            for key in keys {
                let interested = state
                    .wanted
                    .iter()
                    .filter(|((owner, _), _)| *owner == key)
                    .any(|((_, path), _)| match change.name.as_deref() {
                        Some(name) => Self::leads_to(&anchor, name, Path::new(path)),
                        None => true,
                    });
                if !interested {
                    continue;
                }
                state.pending.insert(key.clone());
                // A directory may have appeared on the way to the file.
                if change.flags & ReadFlags::ISDIR.bits() != 0
                    || change.flags & ReadFlags::CREATE.bits() != 0
                    || change.flags & ReadFlags::MOVED_TO.bits() != 0
                {
                    moved.insert(key);
                }
            }
        }
        for key in moved {
            let paths: Vec<String> = state
                .wanted
                .keys()
                .filter(|(owner, _)| *owner == key)
                .map(|(_, path)| path.clone())
                .collect();
            for path in paths {
                // One that cannot be carried again keeps its watch where it
                // is: a change is reported, never a silence.
                let _ = self.carry(&mut state, &key, &path);
            }
        }
        std::mem::take(&mut state.pending).into_iter().collect()
    }
}

impl FileWatch for InotifyWatch {
    fn watch(&self, key: &str, path: &str) -> Result<(), WatchError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (key, path);
            Err(WatchError("no change watch on this platform".to_string()))
        }
        #[cfg(target_os = "linux")]
        {
            let mut state = self.lock();
            let current = state
                .wanted
                .get(&(key.to_string(), path.to_string()))
                .cloned();
            if let Some(anchor) = current
                && state.dirs.values().any(|dir| dir.path == anchor)
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
        state.recheck.remove(key);
    }

    fn drain(&self) -> Vec<String> {
        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }
        #[cfg(target_os = "linux")]
        {
            self.drain_linux()
        }
    }
}

/// The deepest directory above `file` that exists, itself included when it
/// is one.
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

#[cfg(test)]
mod tests {
    use super::{InotifyWatch, anchor};
    use spawn_core::port::FileWatch;
    use std::path::Path;

    /// A directory of this test's own, removed at the end.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "spawn-watch-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(std::fs::canonicalize(path).expect("canonical scratch"))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A change to a watched file is reported for its key, once per drain.
    #[test]
    fn a_write_to_a_watched_file_is_reported_once() {
        let scratch = Scratch::new("write");
        let file = scratch.path().join("transcript.jsonl");
        let watch = InotifyWatch::new().expect("a watch");
        watch
            .watch("a", &file.display().to_string())
            .expect("watch a missing file");
        assert!(watch.drain().is_empty(), "nothing changed yet");
        std::fs::write(&file, "one\n").expect("write");
        assert_eq!(watch.drain(), vec!["a".to_string()]);
        assert!(watch.drain().is_empty(), "a change is reported once");
        std::fs::write(&file, "one\ntwo\n").expect("write");
        assert_eq!(watch.drain(), vec!["a".to_string()]);
    }

    /// A file that appears in a directory created after the watch was set is
    /// reported: the watch follows the path down.
    #[test]
    fn a_file_whose_directory_appears_later_is_reported() {
        let scratch = Scratch::new("nested");
        let file = scratch.path().join("projects").join("ws").join("x.jsonl");
        let watch = InotifyWatch::new().expect("a watch");
        watch
            .watch("b", &file.display().to_string())
            .expect("watch a path no directory of which exists yet");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("directories");
        // The directory's creation is a change of the key, and so is the
        // file's own write that follows it.
        assert_eq!(watch.drain(), vec!["b".to_string()]);
        std::fs::write(&file, "line\n").expect("write");
        assert!(
            watch.drain().contains(&"b".to_string()),
            "the file's own write is reported after the watch moved down"
        );
    }

    /// Every key of a directory hears about the file it asked for, and none
    /// hears about another file of it.
    #[test]
    fn only_the_file_a_key_asked_for_is_reported() {
        let scratch = Scratch::new("keys");
        let mine = scratch.path().join("mine.jsonl");
        let other = scratch.path().join("other.jsonl");
        let watch = InotifyWatch::new().expect("a watch");
        watch.watch("mine", &mine.display().to_string()).expect("watch");
        std::fs::write(&other, "not mine\n").expect("write");
        assert!(watch.drain().is_empty(), "another file is no change");
        std::fs::write(&mine, "mine\n").expect("write");
        assert_eq!(watch.drain(), vec!["mine".to_string()]);
    }

    /// A key that stops watching hears nothing more.
    #[test]
    fn an_unwatched_key_hears_nothing_more() {
        let scratch = Scratch::new("unwatch");
        let file = scratch.path().join("exit-1");
        let watch = InotifyWatch::new().expect("a watch");
        watch.watch("c", &file.display().to_string()).expect("watch");
        watch.unwatch("c");
        std::fs::write(&file, "0").expect("write");
        assert!(watch.drain().is_empty(), "an unwatched key is quiet");
    }

    /// The anchor of a path is the deepest directory above it that exists.
    #[test]
    fn the_anchor_is_the_deepest_existing_directory() {
        let scratch = Scratch::new("anchor");
        assert_eq!(
            anchor(&scratch.path().join("a").join("b").join("f")).as_deref(),
            Some(scratch.path())
        );
        assert_eq!(anchor(scratch.path()).as_deref(), Some(scratch.path()));
    }
}

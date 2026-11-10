//! A judging's identity and directory (gate v2 step 2,
//! design-gate-survives-restart.md D1 and D4).
//!
//! Each judging (a task's, or an owner's landing request's) gets a fresh id
//! and a directory `<root>/judgings/<id>/`, made before its gate starts and
//! holding `judging.json`, what the judging is (written whole, before any
//! gate runs). The gate's private directories (its verdict file, its
//! environment file, the runner's outcome files) are made in it rather than
//! under the temporary directory ([`crate::gate`]'s `judge`).
//!
//! Every outcome the judging records (`task.gated`, `task.landed`,
//! `source.moved`, `source.push_refused`) names its id (`judging`): the
//! store records one of each kind per judging and returns the first to any
//! later one (spawn_integrator_record, and the unique index
//! `spawn_events_control_judging_outcome`). That is what lets a later integrator
//! reap a gate an earlier one started without recording it twice.
//!
//! Order of a reap (D4): read the gate's files, decide, record, and only
//! then remove the directory. In this commit the gate is still the
//! integrator's child and dies with it, so nothing outlives a restart: the
//! directory is removed when the judging's thread ends, whatever it
//! recorded, and a start removes any left behind ([`sweep`]). Adoption
//! (D5) replaces both.

use std::cell::RefCell;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// `judging.json`'s version: a later integrator reads every version an
/// earlier one may have left running (D7).
pub(crate) const VERSION: u64 = 1;

/// The directory under the workspace root that holds every judging's.
pub(crate) const DIRECTORY: &str = "judgings";

/// The record types that name the judging that wrote them; any other is
/// recorded as it is.
const OUTCOMES: [&str; 4] = [
    "task.gated",
    "task.landed",
    "source.moved",
    "source.push_refused",
];

/// One judging: its id and its directory.
#[derive(Debug, Clone)]
pub(crate) struct Judging {
    id: String,
    directory: PathBuf,
}

impl Judging {
    /// A new judging under `root`: a fresh id, its directory (0700, as is
    /// `<root>/judgings`), and `judging.json` holding `what` with its
    /// version, id and start time, written whole (a temporary file renamed
    /// over it).
    pub(crate) fn create(root: &Path, what: &Value) -> Result<Self, String> {
        let id = new_id()?;
        let parent = root.join(DIRECTORY);
        make_private(&parent, true)?;
        let directory = parent.join(&id);
        make_private(&directory, false)?;
        let judging = Self { id, directory };
        let mut described = json!({"version": VERSION, "id": judging.id,
                                   "started_ms": super::now_ms()});
        if let (Some(target), Some(what)) = (described.as_object_mut(), what.as_object()) {
            for (key, value) in what {
                target.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        if let Err(error) = judging.write_description(&described) {
            judging.remove();
            return Err(error);
        }
        Ok(judging)
    }

    #[cfg(test)]
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    #[cfg(test)]
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    fn write_description(&self, described: &Value) -> Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let temporary = self.directory.join("judging.json.tmp");
        let text = format!("{described}\n");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&temporary, self.directory.join("judging.json")))
            .map_err(|error| format!("write {}: {error}", temporary.display()))
    }

    /// Remove the judging's directory and everything in it: the last step
    /// of a reap, after its outcome is recorded.
    pub(crate) fn remove(&self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "integrate: remove the judging directory {}: {error}",
                self.directory.display()
            );
        }
    }

    /// Run `body` as this judging, on this thread: the records it writes
    /// through [`super::record`] name it ([`named`]), and the gates it runs
    /// make their private directories in its directory ([`directory`]).
    /// The directory is removed when `body` returns (or panics).
    pub(crate) fn run<T>(&self, body: impl FnOnce() -> T) -> T {
        struct Done<'a>(&'a Judging);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                CURRENT.with(|current| current.borrow_mut().take());
                self.0.remove();
            }
        }
        CURRENT.with(|current| *current.borrow_mut() = Some(self.clone()));
        let _done = Done(self);
        body()
    }
}

thread_local! {
    /// The judging this thread runs ([`Judging::run`]); None on the main
    /// thread's passes and in every test that runs a gate directly.
    static CURRENT: RefCell<Option<Judging>> = const { RefCell::new(None) };
}

/// The directory of the judging this thread runs, where its gates make
/// their private directories; None outside one (the temporary directory
/// then, as before).
pub(crate) fn directory() -> Option<PathBuf> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map(|judging| judging.directory.clone())
    })
}

/// `record` naming the judging this thread runs, when it is one of a
/// judging's outcomes (`OUTCOMES`) and names none yet; otherwise as it is.
pub(crate) fn named(record: &Value) -> std::borrow::Cow<'_, Value> {
    let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
    if !OUTCOMES.contains(&kind) || record.get("judging").is_some() {
        return std::borrow::Cow::Borrowed(record);
    }
    CURRENT.with(|current| match current.borrow().as_ref() {
        Some(judging) => {
            let mut named = record.clone();
            named["judging"] = json!(judging.id);
            std::borrow::Cow::Owned(named)
        }
        None => std::borrow::Cow::Borrowed(record),
    })
}

/// A fresh judging id: the time in ms (12 hex digits, so ids sort by start)
/// and 8 random bytes, 28 lower-case hex digits in all.
fn new_id() -> Result<String, String> {
    let mut random = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|error| format!("/dev/urandom: {error}"))?;
    let millis = u64::try_from(super::now_ms()).unwrap_or(0) & 0xffff_ffff_ffff;
    let mut id = format!("{millis:012x}");
    for byte in random {
        use std::fmt::Write as _;
        let _ = write!(id, "{byte:02x}");
    }
    Ok(id)
}

/// Make `path` a directory of mode 0700 (`existing`: one already there is
/// kept, if it is a directory of ours with no access for anyone else).
fn make_private(path: &Path, existing: bool) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(error) if existing && error.kind() == std::io::ErrorKind::AlreadyExists => {
            if spawn_host::process::private_to_us(path) {
                Ok(())
            } else {
                Err(format!(
                    "{} is not a directory of this user's with no access for others",
                    path.display()
                ))
            }
        }
        Err(error) => Err(format!("create {}: {error}", path.display())),
    }
}

/// Remove every judging directory under `root`, at the integrator's start:
/// in this commit a gate never outlives the integrator that started it, so
/// any left behind is one whose integrator stopped before its cleanup. The
/// number removed.
pub(crate) fn sweep(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root.join(DIRECTORY)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let is_judging = name
            .to_str()
            .is_some_and(|name| name.len() >= 16 && name.bytes().all(|b| b.is_ascii_hexdigit()));
        if is_judging && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => removed += 1,
                Err(error) => eprintln!("integrate: remove {}: {error}", path.display()),
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(case: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "spawn-judging-{case}-{}-{}",
            std::process::id(),
            new_id().expect("id")
        ));
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn a_judging_has_a_private_directory_and_its_description_whole() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = root("create");
        let judging = Judging::create(&root, &json!({"kind": "task", "task": 7, "id": "not this"}))
            .expect("create");
        assert_eq!(judging.id().len(), 28);
        assert!(
            judging
                .id()
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        for path in [root.join(DIRECTORY), judging.directory().to_path_buf()] {
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", path.display());
        }
        let described: Value = serde_json::from_str(
            &std::fs::read_to_string(judging.directory().join("judging.json")).expect("read"),
        )
        .expect("json");
        assert_eq!(described["version"], json!(VERSION));
        assert_eq!(
            described["id"],
            json!(judging.id()),
            "its own id, not the caller's"
        );
        assert_eq!(described["kind"], json!("task"));
        assert_eq!(described["task"], json!(7));
        assert!(described["started_ms"].as_i64().is_some_and(|at| at > 0));
        assert!(!judging.directory().join("judging.json.tmp").exists());
        let other = Judging::create(&root, &json!({})).expect("another");
        assert_ne!(other.id(), judging.id());
        std::fs::remove_dir_all(&root).expect("clean");
    }

    #[test]
    fn a_judgings_parent_open_to_others_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = root("open");
        std::fs::create_dir(root.join(DIRECTORY)).expect("parent");
        std::fs::set_permissions(root.join(DIRECTORY), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let error = Judging::create(&root, &json!({})).expect_err("refused");
        assert!(error.contains("no access for others"), "{error}");
        std::fs::remove_dir_all(&root).expect("clean");
    }

    #[test]
    fn records_written_inside_a_judging_name_it_and_its_directory_goes_with_it() {
        let root = root("run");
        let judging = Judging::create(&root, &json!({})).expect("create");
        let gated = json!({"type": "task.gated", "id": 1});
        assert_eq!(*named(&gated), gated, "outside a judging: as it is");
        assert_eq!(directory(), None);
        let seen = judging.run(|| {
            assert_eq!(directory().as_deref(), Some(judging.directory()));
            (
                named(&gated).into_owned(),
                named(&json!({"type": "task.landed", "id": 1})).into_owned(),
                named(&json!({"type": "source.moved"})).into_owned(),
                named(&json!({"type": "source.push_refused"})).into_owned(),
                named(&json!({"type": "mission.halted", "id": 2})).into_owned(),
                named(&json!({"type": "task.gated", "judging": "abc"})).into_owned(),
            )
        });
        for outcome in [&seen.0, &seen.1, &seen.2, &seen.3] {
            assert_eq!(outcome["judging"], json!(judging.id()), "{outcome}");
        }
        assert_eq!(seen.4.get("judging"), None, "a halt is no gate's outcome");
        assert_eq!(seen.5["judging"], json!("abc"), "one already named is kept");
        assert!(
            !judging.directory().exists(),
            "removed when the judging ends"
        );
        assert_eq!(directory(), None);
        assert_eq!(*named(&gated), gated);
        // A panic inside ends it too.
        let judging = Judging::create(&root, &json!({})).expect("create");
        let dir = judging.directory().to_path_buf();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            judging.run(|| panic!("inside"));
        }));
        assert!(panicked.is_err());
        assert!(!dir.exists());
        assert_eq!(directory(), None);
        std::fs::remove_dir_all(&root).expect("clean");
    }

    #[test]
    fn a_start_sweeps_judging_directories_and_nothing_else() {
        let root = root("sweep");
        let left = Judging::create(&root, &json!({})).expect("create");
        std::fs::write(left.directory().join("status"), "0\n").expect("status");
        let other = root.join(DIRECTORY).join("notes");
        std::fs::create_dir(&other).expect("other");
        assert_eq!(sweep(&root), 1);
        assert!(!left.directory().exists());
        assert!(other.exists(), "not a judging's: kept");
        assert_eq!(sweep(&root.join("none")), 0);
        std::fs::remove_dir_all(&root).expect("clean");
    }
}

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
//! `source.moved`, `source.push_refused`, an owner judging's
//! `source.push_no_verdict`) names its id (`judging`): the
//! store records one of each kind per judging and returns the first to any
//! later one (spawn_integrator_record, and the unique index
//! `spawn_events_control_judging_outcome`). That is what lets a later integrator
//! reap a gate an earlier one started without recording it twice.
//!
//! Order of a reap (D4): read the gate's files, decide, record, and only
//! then remove the directory: when the judging's thread ends ([`Judging::run`],
//! which first stops any unit of its gates that wrote no status).
//! Two reapers of one gate (an adopter beside the integrator that started
//! it) record it once: an adopter that finds the gate's directory gone
//! when its runner ends records nothing (`GateFailure::Reaped`), and the
//! store returns a judging's first verdict to any later one, `abandoned` too.
//!
//! A gate in a unit (crate::gate's `run_unit`) outlives the integrator that
//! started it. The next one finds its judging among the [`left`] ones at
//! each pass and adopts it (integrate.rs `Integrator::adopt`, D5): a
//! judging thread whose first gate waits on the running one
//! ([`Judging::run_adopted`], [`take_adopted`]) instead of starting
//! another. A left judging with no gate in it, or one no one adopts, is
//! abandoned ([`Judging::abandon`]): its units stopped, its directory
//! removed.

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
const OUTCOMES: [&str; 5] = [
    "task.gated",
    "task.landed",
    "source.moved",
    "source.push_refused",
    "source.push_no_verdict",
];

/// One judging: its id and its directory.
#[derive(Debug, Clone)]
pub(crate) struct Judging {
    id: String,
    directory: PathBuf,
    /// Its gates run in units of their own ([`crate::gate`]'s `run_unit`),
    /// which outlive the integrator; else as its children.
    units: bool,
}

impl Judging {
    /// A new judging under `root`: a fresh id, its directory (0700, as is
    /// `<root>/judgings`), and `judging.json` holding `what` with its
    /// version, id and start time, written whole (a temporary file renamed
    /// over it).
    pub(crate) fn create(root: &Path, what: &Value, units: bool) -> Result<Self, String> {
        let id = new_id()?;
        let parent = root.join(DIRECTORY);
        make_private(&parent, true)?;
        let directory = parent.join(&id);
        make_private(&directory, false)?;
        let judging = Self {
            id,
            directory,
            units,
        };
        let mut described = json!({"version": VERSION, "id": judging.id,
                                   "started_ms": super::now_ms(), "units": units});
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

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    #[cfg(test)]
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// Stop the units of its gates still running, and remove it: a judging
    /// left behind that no one adopts.
    pub(crate) fn abandon(&self) {
        stop_units(&self.directory);
        self.remove();
    }

    /// [`Judging::run`] for a judging an earlier integrator left (D5): the
    /// first gate `body` runs adopts the one running in `gate` (its private
    /// directory) instead of starting another ([`take_adopted`]).
    pub(crate) fn run_adopted<T>(&self, gate: PathBuf, body: impl FnOnce() -> T) -> T {
        ADOPTED.with(|adopted| *adopted.borrow_mut() = Some(gate));
        let result = self.run(body);
        ADOPTED.with(|adopted| adopted.borrow_mut().take());
        result
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
                // A gate still running in its unit (the judging ended
                // without waiting for it: an error, a panic) goes first.
                stop_units(&self.0.directory);
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
    /// The gate an adopted judging takes over ([`Judging::run_adopted`]),
    /// until its first gate takes it.
    static ADOPTED: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The private directory of the gate this thread's judging adopts, once:
/// the first gate it runs waits on that one (gate.rs `judge_in`); every
/// later one starts its own.
pub(crate) fn take_adopted() -> Option<PathBuf> {
    ADOPTED.with(|adopted| adopted.borrow_mut().take())
}

/// Add `more`'s keys to the description (`judging.json`) of the judging
/// this thread runs, written whole: what a later integrator needs to adopt
/// its gate (the candidate and the branch it judges), before that gate
/// starts. Nothing outside a judging.
pub(crate) fn describe(more: &Value) -> Result<(), String> {
    let Some(judging) = CURRENT.with(|current| current.borrow().clone()) else {
        return Ok(());
    };
    let path = judging.directory.join("judging.json");
    let mut described: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .ok_or_else(|| format!("read {}", path.display()))?;
    if let (Some(target), Some(more)) = (described.as_object_mut(), more.as_object()) {
        target.extend(more.clone());
    }
    let _ = std::fs::remove_file(judging.directory.join("judging.json.tmp"));
    judging.write_description(&described)
}

/// A judging an earlier integrator left under `<root>/judgings`: it, what
/// its `judging.json` says (None when it cannot be read), and the private
/// directory of its gate that has a runner (`runner.json`), if any.
pub(crate) struct Left {
    pub(crate) judging: Judging,
    pub(crate) described: Option<Value>,
    pub(crate) gate: Option<PathBuf>,
}

/// The judgings under `root` that are not `owned` (by id: the ones this
/// integrator runs), oldest first.
pub(crate) fn left(root: &Path, owned: &std::collections::HashSet<String>) -> Vec<Left> {
    let Ok(entries) = std::fs::read_dir(root.join(DIRECTORY)) else {
        return Vec::new();
    };
    let mut found: Vec<Left> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let id = entry.file_name().to_str()?.to_string();
            (is_id(&id) && !owned.contains(&id)).then_some((id, entry.path()))
        })
        .map(|(id, directory)| {
            let described = std::fs::read_to_string(directory.join("judging.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok());
            let units = described
                .as_ref()
                .and_then(|value| value.get("units"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // The candidate's gate, the one gate an adopter may wait on: the
            // only one with a runner, while judging.json does not say the
            // candidate was judged. Once it was, a gate still running is a
            // later one of the judging's (a rejection's head re-judging, on
            // another base), which no adopter resumes (rev-e G1): no gate,
            // so the judging is abandoned, its units stopped, and the task
            // judged anew. Two gates with runners say the same.
            let judged = described
                .as_ref()
                .and_then(|value| value.get("candidate_judged"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let gates: Vec<PathBuf> = std::fs::read_dir(&directory)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.join("runner.json").is_file())
                        .collect()
                })
                .unwrap_or_default();
            let gate = match gates.as_slice() {
                [gate] if !judged => Some(gate.clone()),
                _ => None,
            };
            Left {
                judging: Judging {
                    id,
                    directory,
                    units,
                },
                described,
                gate,
            }
        })
        .collect();
    found.sort_by(|a, b| a.judging.id.cmp(&b.judging.id));
    found
}

/// A judging id as [`new_id`] makes them, or longer: lower-case hex.
fn is_id(name: &str) -> bool {
    name.len() >= 16
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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

/// Whether the judging this thread runs starts its gates in units of their
/// own; false outside one.
pub(crate) fn units() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|judging| judging.units)
    })
}

/// Keep the gate files of `gate`, a gate's private directory about to be
/// removed, when it is in the directory of the judging this thread runs:
/// copied to `<root>/gate-logs/<judging>/` ([`super::retained::keep_gate`]),
/// whatever the gate ended with. Outside a judging, or for a directory
/// elsewhere, nothing. A failure is said and the removal goes on.
pub(crate) fn keep_gate(gate: &Path) {
    let Some(judging) = CURRENT.with(|current| current.borrow().clone()) else {
        return;
    };
    if gate.parent() != Some(judging.directory.as_path()) {
        return;
    }
    let Some(root) = judging.directory.parent().and_then(Path::parent) else {
        return;
    };
    if let Err(error) = super::retained::keep_gate(
        root,
        &judging.id,
        gate,
        super::retained::GATE_KEEP,
        super::retained::GATE_TOTAL_BYTES,
    ) {
        eprintln!(
            "integrate: judging {}: keep its gate's files: {error}",
            judging.id
        );
    }
}

/// The id of the judging this thread runs; None outside one.
pub(crate) fn current_id() -> Option<String> {
    CURRENT.with(|current| current.borrow().as_ref().map(|judging| judging.id.clone()))
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
            // Where its gates' files are kept, once one has been.
            if let Some(root) = judging.directory.parent().and_then(Path::parent) {
                let kept = root.join(super::retained::GATE_LOGS).join(&judging.id);
                if kept.is_dir() {
                    named["gate_logs"] = json!(kept.to_string_lossy());
                }
            }
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

/// Stop the unit of every gate in the judging directory `directory` that
/// has not written its status (`<private>/runner.json` names its unit).
fn stop_units(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let private = entry.path();
        if private.join("status").exists() {
            continue;
        }
        if let Some(run) = crate::gate::UnitRun::read(&private) {
            crate::gate::stop_unit(&run.unit);
        }
    }
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
        let judging = Judging::create(
            &root,
            &json!({"kind": "task", "task": 7, "id": "not this"}),
            false,
        )
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
        let other = Judging::create(&root, &json!({}), false).expect("another");
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
        let error = Judging::create(&root, &json!({}), false).expect_err("refused");
        assert!(error.contains("no access for others"), "{error}");
        std::fs::remove_dir_all(&root).expect("clean");
    }

    #[test]
    fn records_written_inside_a_judging_name_it_and_its_directory_goes_with_it() {
        let root = root("run");
        let judging = Judging::create(&root, &json!({}), false).expect("create");
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
        let judging = Judging::create(&root, &json!({}), false).expect("create");
        let dir = judging.directory().to_path_buf();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            judging.run(|| panic!("inside"));
        }));
        assert!(panicked.is_err());
        assert!(!dir.exists());
        assert_eq!(directory(), None);
        std::fs::remove_dir_all(&root).expect("clean");
    }

    /// A left judging no one adopts (its task moved on, an owner's, one
    /// this integrator cannot read) is abandoned: its gate's unit is stopped
    /// before its directory goes, so nothing writes into a directory that is
    /// gone.
    #[test]
    fn abandoning_a_left_judging_stops_its_gates_unit_first() {
        if !crate::gate::units_available() {
            eprintln!("no systemd user manager answers here: not tried");
            return;
        }
        let root = root("abandon-unit");
        let left = Judging::create(&root, &json!({}), true).expect("create");
        let private = left.directory().join("spawn-gate-test");
        std::fs::create_dir(&private).expect("private");
        let unit = format!(
            "spawn-gate-left-{}-{}.service",
            std::process::id(),
            new_id().expect("id")
        );
        let started = std::process::Command::new("systemd-run")
            .args([
                "--user",
                "--quiet",
                "--collect",
                &format!("--unit={unit}"),
                "sleep",
                "300",
            ])
            .status()
            .expect("systemd-run");
        assert!(started.success());
        std::fs::write(
            private.join("runner.json"),
            json!({"unit": unit, "runner": null, "started_ms": 1, "limit_s": 300}).to_string(),
        )
        .expect("runner.json");
        let active = |unit: &str| {
            let out = std::process::Command::new("systemctl")
                .args(["--user", "is-active", unit])
                .output()
                .expect("systemctl");
            String::from_utf8_lossy(&out.stdout).trim() == "active"
        };
        assert!(active(&unit), "the left gate runs");
        let found = left_judgings(&root);
        assert_eq!(found.len(), 1);
        found[0].judging.abandon();
        assert!(!active(&unit), "its unit was stopped");
        assert!(!left.directory().exists());
        std::fs::remove_dir_all(&root).expect("clean");
    }

    fn left_judgings(root: &Path) -> Vec<Left> {
        left(root, &std::collections::HashSet::new())
    }

    /// The judgings left under a root: every judging directory but the ones
    /// this integrator owns, oldest first, each with what it says and its
    /// gate that has a runner; anything else there is no judging.
    #[test]
    fn the_left_judgings_are_the_ones_not_owned_with_their_gates() {
        let root = root("left");
        let older = Judging::create(&root, &json!({"kind": "task", "task": 1}), true).expect("one");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newer =
            Judging::create(&root, &json!({"kind": "task", "task": 2}), false).expect("two");
        let owned = Judging::create(&root, &json!({}), false).expect("owned");
        let gate = newer.directory().join("spawn-gate-x");
        std::fs::create_dir(&gate).expect("gate");
        std::fs::write(gate.join("runner.json"), "{}").expect("runner");
        std::fs::create_dir(older.directory().join("spawn-gate-no-runner")).expect("no runner");
        std::fs::create_dir(root.join(DIRECTORY).join("notes")).expect("other");
        let found = left(&root, &[owned.id().to_string()].into_iter().collect());
        let ids: Vec<&str> = found.iter().map(|left| left.judging.id()).collect();
        assert_eq!(
            ids,
            vec![older.id(), newer.id()],
            "oldest first, the owned one left out"
        );
        assert_eq!(found[0].gate, None, "a gate with no runner is none");
        assert_eq!(found[1].gate.as_deref(), Some(gate.as_path()));
        assert_eq!(
            found[0].described.as_ref().map(|d| d["task"].clone()),
            Some(json!(1))
        );
        assert!(found[0].judging.units && !found[1].judging.units);
        assert!(left(&root.join("none"), &std::collections::HashSet::new()).is_empty());
        std::fs::remove_dir_all(&root).expect("clean");
    }

    /// rev-e G1: a judging that ran two gates (its candidate's, then a
    /// rejection's head re-judging) has no gate an adopter may resume,
    /// whichever the directory lists first; nor has one whose description
    /// says the candidate was judged, though one gate runs. Only a
    /// judging's one gate, before its candidate was judged, is adopted.
    #[test]
    fn a_judging_past_its_candidates_gate_has_no_gate_to_adopt() {
        let root = root("left-g1");
        let gate_in = |judging: &Judging, name: &str| {
            let gate = judging.directory().join(name);
            std::fs::create_dir(&gate).expect("gate");
            std::fs::write(gate.join("runner.json"), "{}").expect("runner");
            gate
        };
        // Both listing orders: the candidate's gate named to sort first,
        // then last.
        for (first, second) in [("a-candidate", "z-head"), ("a-head", "z-candidate")] {
            let two = Judging::create(&root, &json!({"kind": "task"}), true).expect("two gates");
            gate_in(&two, first);
            gate_in(&two, second);
            let found = left(&root, &std::collections::HashSet::new());
            let found = found
                .iter()
                .find(|left| left.judging.id() == two.id())
                .expect("left");
            assert_eq!(
                found.gate, None,
                "{first}, {second}: two gates, none adopted"
            );
            two.remove();
        }
        let judged = Judging::create(
            &root,
            &json!({"kind": "task", "candidate_judged": true}),
            true,
        )
        .expect("judged");
        gate_in(&judged, "head");
        let one = Judging::create(&root, &json!({"kind": "task"}), true).expect("one");
        let candidate = gate_in(&one, "candidate");
        let found = left(&root, &std::collections::HashSet::new());
        let gate_of = |id: &str| {
            found
                .iter()
                .find(|left| left.judging.id() == id)
                .and_then(|left| left.gate.clone())
        };
        assert_eq!(gate_of(judged.id()), None, "past its candidate's gate");
        assert_eq!(gate_of(one.id()), Some(candidate), "its candidate's gate");
        std::fs::remove_dir_all(&root).expect("clean");
    }
}

//! `spawn backup restore-drill --only controllers` (objective 198): each
//! live controller is its own commit's tree.
//!
//! A controller is the tree a mission's live system runs from (the gate,
//! its scenarios, the backlog, the maintenance script): `git archive` of
//! one commit of the mission's source, built by deploy/controller.sh as
//! `$SPAWN_LIVE/controllers/<name>-<commit>`, with the symlinks `<name>`
//! (current) and `<name>.prev` (the one before it).
//!
//! Which controllers: each open or active mission whose gate lies under
//! `$SPAWN_LIVE/controllers/<name>/` (`SPAWN_LIVE`, else
//! `~/.local/share/spawn-rsi`) names controller `<name>`, and its first
//! source is the live repository the controller was built from. A mission
//! with no gate, a gate elsewhere, under `<name>.prev` (the previous
//! controller) or under a built tree (`<name>-<commit>`, not a link) is
//! skipped and named. Missions that share a controller but name different
//! sources fail it.
//!
//! LIVE, per controller, its current tree and its previous one (when its
//! link is there): the live directory, held open while it is read (so a
//! switch keeps it: deploy/controller.sh's /proc rule), equals `git archive`
//! of the commit its `.controller` names, from the live source, extracted
//! into the drill's scratch directory. Left out ([`LEFT_OUT`]): what
//! deploy/controller.sh adds and git does not hold. Every file and symlink
//! is compared by path: a file's executable bit and sha256, a symlink's
//! target; a path on one side only, or a file on one side and a symlink on
//! the other, differs. Directories are not entries. Nothing live is
//! written.
//!
//! HELD (objective 199), per controller, its current tree and its previous
//! one: the sources are first restored from the newest bundle chain, as
//! `--only missions` restores them (decrypted in a pipe, the manifest and
//! `repos/*` alone, never a transcript: [`super::restore_sources`]). X is
//! the first commit of `git rev-list --topo-order <commit>` in the live
//! source that the restored mirror holds (reachable from its refs); `git
//! archive X` from the mirror equals `git archive X` from the live source,
//! both extracted into scratch and compared as LIVE compares. The lag from
//! X to the tree's commit (`lag_commits`: `X..commit`; `lag_hours`: whole
//! hours by committer time) is recorded, never a failure: deploys are
//! several a day and bundles one. What HELD proves: the same commit id pins
//! the same tree, and objects that pass `fsck --full` give the same
//! archive, so it adds that X extracts whole from the off-site copy into a
//! usable tree. A tree fails also when its HELD trees differ, or when the
//! mirror holds no commit of its history (its `held` null).
//!
//! One `backup.drilled.controllers` per controller (see [`record`]), naming
//! the bundle; exit 0 only when every controller passed, 1 when one failed
//! or no mission names a controller, 2 when the drill cannot run or a
//! record is refused. A run that cannot restore the backup records one
//! failed record of the drill record's fields alone (its reason "the backup
//! cannot be restored: ...") and exits 1.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

use super::{
    Bundle, DrillDir, Mirrors, NAMED, REASON_CHARS, elapsed_ms, git, restore_sources, run_git,
};
use crate::backup::{Settings, sha256_file};

/// What deploy/controller.sh adds to a controller and git does not hold:
/// never compared (a directory's whole subtree).
pub(super) const LEFT_OUT: [&str; 3] = [".controller", ".deps", "web/node_modules"];

/// `--only`'s value for this drill.
pub(super) const CONTROLLERS: &str = "controllers";

/// The record's type.
const TYPE: &str = "backup.drilled.controllers";

/// One entry of a tree: a file (its executable bit and sha256), a symlink
/// (its target) or anything else (a fifo, a socket: never equal).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File { executable: bool, sha256: String },
    Link(PathBuf),
    Other,
}

/// A controller its missions name.
#[derive(Debug, Default)]
struct Named {
    missions: Vec<i64>,
    sources: BTreeSet<String>,
}

/// Where a mission's gate lies.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    /// Under the current link of this controller.
    Controller(String),
    /// Not a controller's: why.
    Skipped(String),
}

/// One tree drilled (current or previous).
struct Tree {
    commit: String,
    files: usize,
    differing: Vec<String>,
    /// Why it could not be compared, when it could not.
    error: Option<String>,
    /// HELD: the newest commit of its history the backup holds, compared.
    held: Option<Held>,
    /// Why HELD failed (no commit held, a mirror that cannot be read), when
    /// it did.
    held_error: Option<String>,
}

/// HELD for one tree (objective 199).
struct Held {
    commit: String,
    lag_commits: u64,
    lag_hours: i64,
    files: usize,
    differing: Vec<String>,
}

impl Tree {
    /// LIVE alone.
    fn live_ok(&self) -> bool {
        self.error.is_none() && self.differing.is_empty()
    }

    fn unread(commit: &str, error: String) -> Self {
        Self {
            commit: commit.to_string(),
            files: 0,
            differing: Vec::new(),
            error: Some(error),
            held: None,
            held_error: None,
        }
    }

    fn mismatches(&self) -> usize {
        self.differing.len() + self.held.as_ref().map_or(0, |held| held.differing.len())
    }

    fn json(&self) -> Value {
        let held = self.held.as_ref().map_or(Value::Null, |held| {
            json!({
                "commit": held.commit,
                "lag_commits": held.lag_commits,
                "lag_hours": held.lag_hours,
                "ok": self.held_error.is_none() && held.differing.is_empty(),
                "files": held.files,
                "differing": held.differing.iter().take(NAMED).collect::<Vec<_>>(),
            })
        });
        json!({
            "commit": self.commit,
            "live": {
                "ok": self.live_ok(),
                "files": self.files,
                "differing": self.differing.iter().take(NAMED).collect::<Vec<_>>(),
            },
            "held": held,
        })
    }

    /// Why it failed, for the record's reason; None when it passed.
    fn why(&self, which: &str) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(format!("{which}: {error}"));
        }
        let mut why = Vec::new();
        if let Some(first) = self.differing.first() {
            why.push(format!(
                "{which} {}: {} of {} live paths differ from the commit's tree; the first: {first}",
                short(&self.commit),
                self.differing.len(),
                self.files
            ));
        }
        if let Some(error) = &self.held_error {
            why.push(format!("{which} {}: {error}", short(&self.commit)));
        } else if let Some(held) = &self.held
            && let Some(first) = held.differing.first()
        {
            why.push(format!(
                "{which} {}: held {}: {} of {} paths differ between git archive from the \
                 backup's mirror and from the live source; the first: {first}",
                short(&self.commit),
                short(&held.commit),
                held.differing.len(),
                held.files
            ));
        }
        (!why.is_empty()).then(|| why.join("; "))
    }

    /// HELD said for the console.
    fn held_said(&self) -> String {
        match &self.held {
            Some(held) => format!(
                " (held {}, lag {} commit(s), {} h)",
                short(&held.commit),
                held.lag_commits,
                held.lag_hours
            ),
            None => " (held: none)".to_string(),
        }
    }
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The drill: `parent` is the scratch directory's resolved parent, `live`
/// is `$SPAWN_LIVE`.
pub(super) fn drill(
    store: &PostgresStore,
    settings: &Settings,
    parent: &Path,
    live: &Path,
) -> Result<i32, String> {
    println!(
        "restore-drill: --only controllers: each controller an open or active mission's gate \
         names, current and previous, compared with git archive of its commit from the live \
         source; read only ({} left out); and the newest commit of its history the newest \
         bundle chain holds, restored (manifest.json and repos/* alone, decrypted in a pipe), \
         compared with the live source's",
        LEFT_OUT.join(", ")
    );
    let started = Instant::now();
    let run = store.now_ms().map_err(|error| error.to_string())?;
    let listed = store
        .alert_mission_gates()
        .map_err(|error| format!("the missions' gates: {error}"))?;
    let controllers = live.join("controllers");
    let named = named_controllers(&listed, &controllers);
    if named.is_empty() {
        println!(
            "restore-drill: FAILED: no open or active mission's gate names a controller under {}",
            controllers.display()
        );
        return Ok(1);
    }
    let scratch = DrillDir::create(parent)?;
    let sources: BTreeSet<String> = named
        .values()
        .filter(|named| named.sources.len() == 1)
        .flat_map(|named| named.sources.iter().cloned())
        .filter(|source| !source.is_empty())
        .collect();
    let (bundle, mirrors) = match restore_sources(store, settings, &sources, scratch.path()) {
        Ok(restored) => restored,
        Err((bundle, reason)) => {
            let reason = format!("the backup cannot be restored: {reason}");
            println!("restore-drill: FAILED: {reason}");
            scratch.remove()?;
            let record = json!({
                "type": TYPE,
                "ok": false,
                "reason": reason.chars().take(REASON_CHARS).collect::<String>(),
                "bundle": bundle.key,
                "bundle_event_id": bundle.event_id,
                "mismatches": 0,
                "duration_ms": elapsed_ms(started),
            });
            if let Err(error) = store.backup_record(&record) {
                eprintln!("spawn backup restore-drill: {TYPE} was not recorded: {error}");
                return Ok(2);
            }
            return Ok(1);
        }
    };
    println!(
        "restore-drill: --only controllers: restored {} source(s) from the bundle {} (record {})",
        sources.len(),
        bundle.key,
        bundle.event_id
    );
    let mut records = Vec::new();
    for (name, named) in &named {
        let drilled = Drilled {
            controllers: &controllers,
            scratch: scratch.path(),
            mirrors: &mirrors,
            bundle: &bundle,
            run,
            started,
        };
        let (record, said) = drill_one(&drilled, name, named);
        let ok = record["ok"].as_bool() == Some(true);
        if ok {
            println!("controllers: {name}: passed: {said}");
        } else {
            println!(
                "controllers: {name}: FAILED: {}",
                record["reason"].as_str().unwrap_or_default()
            );
        }
        records.push(record);
    }
    scratch.remove()?;
    let passed = records
        .iter()
        .filter(|record| record["ok"].as_bool() == Some(true))
        .count();
    let mut code = i32::from(passed != records.len());
    for record in &records {
        if let Err(error) = store.backup_record(record) {
            eprintln!(
                "spawn backup restore-drill: {TYPE} of {} was not recorded: {error}",
                record["controller"].as_str().unwrap_or_default()
            );
            code = 2;
        }
    }
    println!(
        "restore-drill: --only controllers: {passed} of {} controllers are their commits' trees",
        records.len()
    );
    Ok(code)
}

/// The controllers the missions' gates name (said: each mission skipped),
/// by name.
fn named_controllers(listed: &Value, controllers: &Path) -> BTreeMap<String, Named> {
    let resolved = controllers.canonicalize().ok();
    let mut named: BTreeMap<String, Named> = BTreeMap::new();
    for mission in listed.as_array().into_iter().flatten() {
        let id = mission["mission"].as_i64().unwrap_or_default();
        let Some(gate) = mission["gate"].as_str() else {
            println!("controllers: skipped mission {id}: it has no gate");
            continue;
        };
        match gate_of(Path::new(gate), controllers, resolved.as_deref()) {
            Gate::Controller(name) => {
                let entry = named.entry(name).or_default();
                entry.missions.push(id);
                entry
                    .sources
                    .insert(mission["source"].as_str().unwrap_or_default().to_string());
            }
            Gate::Skipped(why) => println!("controllers: skipped mission {id}: {why}"),
        }
    }
    named
}

/// Which controller a gate names: the first component under
/// `controllers` (as given, or resolved) when that is a controller's
/// current link (or a name whose link is not there).
fn gate_of(gate: &Path, controllers: &Path, resolved: Option<&Path>) -> Gate {
    let under = gate
        .strip_prefix(controllers)
        .ok()
        .or_else(|| resolved.and_then(|resolved| gate.strip_prefix(resolved).ok()));
    let first = under.and_then(|rest| match rest.components().next() {
        Some(Component::Normal(name)) => name.to_str().map(str::to_string),
        _ => None,
    });
    let Some(name) = first else {
        return Gate::Skipped(format!(
            "its gate {} is not under {}",
            gate.display(),
            controllers.display()
        ));
    };
    if name.strip_suffix(".prev").is_some() {
        return Gate::Skipped(format!(
            "its gate {} is under {name}, the previous controller, not a current one",
            gate.display()
        ));
    }
    let path = controllers.join(&name);
    let built = match fs::symlink_metadata(&path) {
        Ok(meta) => !meta.file_type().is_symlink() && meta.is_dir(),
        Err(_) => is_built_name(&name),
    };
    if built {
        return Gate::Skipped(format!(
            "its gate {} is under {name}, a built tree, not a controller's current link",
            gate.display()
        ));
    }
    Gate::Controller(name)
}

/// `<name>-<commit>`: a built tree's name (deploy/controller.sh).
fn is_built_name(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(stem, commit)| {
        !stem.is_empty() && commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// What every controller of one run is drilled with.
struct Drilled<'a> {
    controllers: &'a Path,
    scratch: &'a Path,
    mirrors: &'a Mirrors,
    bundle: &'a Bundle,
    run: i64,
    started: Instant,
}

/// One controller drilled, as its record, and its trees said.
fn drill_one(drilled: &Drilled<'_>, name: &str, named: &Named) -> (Value, String) {
    let Drilled {
        controllers,
        scratch,
        mirrors,
        bundle,
        run,
        started,
    } = *drilled;
    let link = controllers.join(name);
    let source = if named.sources.len() == 1 {
        named.sources.iter().next().cloned().unwrap_or_default()
    } else {
        String::new()
    };
    let (current, prev, reason) = if named.sources.len() != 1 {
        let reason = format!(
            "its missions name {} sources, not one: {}",
            named.sources.len(),
            named
                .sources
                .iter()
                .map(|source| format!("{source:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        (Tree::unread("", reason.clone()), None, Some(reason))
    } else if source.is_empty() {
        let reason = "its missions name no source".to_string();
        (Tree::unread("", reason.clone()), None, Some(reason))
    } else if fs::symlink_metadata(&link).is_err() {
        let reason = format!("{} is not there", link.display());
        (Tree::unread("", reason.clone()), None, Some(reason))
    } else {
        let mirror = match mirrors.get(&source) {
            Some(Ok(mirror)) => Ok(mirror.as_path()),
            Some(Err(error)) => Err(error.as_str()),
            None => Err("it was not restored"),
        };
        let current = drill_tree(
            &link,
            &source,
            mirror,
            &scratch.join(format!("{name}.current")),
        );
        let prev_link = controllers.join(format!("{name}.prev"));
        let prev = fs::symlink_metadata(&prev_link).is_ok().then(|| {
            drill_tree(
                &prev_link,
                &source,
                mirror,
                &scratch.join(format!("{name}.prev")),
            )
        });
        let reasons: Vec<String> = [
            current.why("current"),
            prev.as_ref().and_then(|p| p.why("prev")),
        ]
        .into_iter()
        .flatten()
        .collect();
        let reason = (!reasons.is_empty()).then(|| reasons.join("; "));
        (current, prev, reason)
    };
    let mismatches = current.mismatches() + prev.as_ref().map_or(0, Tree::mismatches);
    let said = format!(
        "current {}{}{}",
        current.commit,
        current.held_said(),
        prev.as_ref().map_or_else(String::new, |prev| format!(
            ", prev {}{}",
            prev.commit,
            prev.held_said()
        ))
    );
    let reason = reason.map_or_else(String::new, |reason| {
        format!("{name}: {reason}")
            .chars()
            .take(REASON_CHARS)
            .collect()
    });
    let record = record(&Record {
        controller: name,
        missions: &named.missions,
        source: &source,
        reason: &reason,
        bundle,
        mismatches,
        current: &current,
        prev: prev.as_ref(),
        run,
        duration_ms: elapsed_ms(started),
    });
    (record, said)
}

/// What one record is made of.
struct Record<'a> {
    controller: &'a str,
    missions: &'a [i64],
    source: &'a str,
    reason: &'a str,
    bundle: &'a Bundle,
    mismatches: usize,
    current: &'a Tree,
    prev: Option<&'a Tree>,
    run: i64,
    duration_ms: i64,
}

/// A `backup.drilled.controllers` record: the drill record's fields (the
/// bundle its sources were restored from), the run's start, the
/// controller, its missions and source, its current tree and its previous
/// one (null when there is none; each with `live` and `held`), what was
/// left out.
fn record(record: &Record<'_>) -> Value {
    json!({
        "type": TYPE,
        "ok": record.reason.is_empty(),
        "reason": record.reason,
        "bundle": record.bundle.key,
        "bundle_event_id": record.bundle.event_id,
        "mismatches": record.mismatches,
        "duration_ms": record.duration_ms,
        "run": record.run,
        "controller": record.controller,
        "missions": record.missions,
        "source": record.source,
        "current": record.current.json(),
        "prev": record.prev.map_or(Value::Null, Tree::json),
        "left_out": LEFT_OUT,
    })
}

/// The tree a link points at, compared with `git archive` of the commit
/// its `.controller` names, extracted into `into` (LIVE); then HELD
/// against `mirror`, the source restored from the backup (or why it could
/// not be).
fn drill_tree(link: &Path, source: &str, mirror: Result<&Path, &str>, into: &Path) -> Tree {
    let dir = match link.canonicalize() {
        Ok(dir) => dir,
        Err(error) => return Tree::unread("", format!("{}: {error}", link.display())),
    };
    // Held open while it is read: deploy/controller.sh removes no
    // controller a process has an open file in.
    let _held = match File::open(&dir) {
        Ok(held) => held,
        Err(error) => return Tree::unread("", format!("{}: {error}", dir.display())),
    };
    let commit = match fs::read_to_string(dir.join(".controller")) {
        Ok(text) => text.trim().to_string(),
        Err(error) => {
            return Tree::unread("", format!("{}/.controller: {error}", dir.display()));
        }
    };
    if !(7..=64).contains(&commit.len()) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Tree::unread(
            "",
            format!("{}/.controller does not name a commit", dir.display()),
        );
    }
    if let Err(error) = archive(source, &commit, into) {
        return Tree::unread(&commit, error);
    }
    let mut tree = match (entries(&dir), entries(into)) {
        (Ok(live), Ok(archived)) => Tree {
            commit,
            files: live.len(),
            differing: differences(&live, &archived),
            error: None,
            held: None,
            held_error: None,
        },
        (Err(error), _) | (_, Err(error)) => return Tree::unread(&commit, error),
    };
    match mirror {
        Ok(mirror) => match held(source, mirror, &tree.commit, into) {
            Ok(held) => tree.held = Some(held),
            Err(Unheld::None(why)) => tree.held_error = Some(why),
            Err(Unheld::Failed(held, why)) => {
                tree.held = Some(held);
                tree.held_error = Some(why);
            }
        },
        Err(error) => {
            tree.held_error = Some(format!(
                "the backup holds no commit of its history: {error}"
            ));
        }
    }
    tree
}

/// Why HELD failed.
enum Unheld {
    /// No commit compared (none held, or the histories cannot be read).
    None(String),
    /// X found, but its trees could not be compared.
    Failed(Held, String),
}

/// HELD for `commit`: X, the first commit of its history in the live
/// source (topological order) the restored `mirror` holds; `git archive X`
/// from each extracted beside `into` and compared.
fn held(source: &str, mirror: &Path, commit: &str, into: &Path) -> Result<Held, Unheld> {
    let mirror_text = mirror.display().to_string();
    let listed = run_git(
        {
            let mut command = git();
            command
                .arg("--git-dir")
                .arg(mirror)
                .args(["rev-list", "--all"]);
            command
        },
        "git rev-list --all (the restored mirror)",
    )
    .map_err(Unheld::None)?;
    let holds: BTreeSet<&str> = listed.lines().collect();
    let history = run_git(
        {
            let mut command = git();
            command
                .args(["-C", source, "rev-list", "--topo-order"])
                .arg(commit);
            command
        },
        "git rev-list --topo-order (the live source)",
    )
    .map_err(Unheld::None)?;
    let Some(x) = history.lines().find(|line| holds.contains(line)) else {
        return Err(Unheld::None(
            "the backup holds no commit of its history".to_string(),
        ));
    };
    let lag_commits = run_git(
        {
            let mut command = git();
            command
                .args(["-C", source, "rev-list", "--count"])
                .arg(format!("{x}..{commit}"));
            command
        },
        "git rev-list --count",
    )
    .map_err(Unheld::None)?
    .trim()
    .parse::<u64>()
    .map_err(|error| Unheld::None(format!("git rev-list --count: {error}")))?;
    let times = run_git(
        {
            let mut command = git();
            command
                .args(["-C", source, "show", "-s", "--format=%ct"])
                .args([commit, x]);
            command
        },
        "git show --format=%ct",
    )
    .map_err(Unheld::None)?;
    let times: Vec<i64> = times
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();
    let lag_hours = match times.as_slice() {
        [at, held_at] => ((at - held_at) / 3600).max(0),
        _ => 0,
    };
    let mut held = Held {
        commit: x.to_string(),
        lag_commits,
        lag_hours,
        files: 0,
        differing: Vec::new(),
    };
    let base = into.as_os_str().to_string_lossy();
    let (from_mirror, from_source) = (
        PathBuf::from(format!("{base}.held-backup")),
        PathBuf::from(format!("{base}.held-live")),
    );
    if let Err(error) = archive(&mirror_text, x, &from_mirror) {
        return Err(Unheld::Failed(held, format!("held {}: {error}", short(x))));
    }
    if let Err(error) = archive(source, x, &from_source) {
        return Err(Unheld::Failed(held, format!("held {}: {error}", short(x))));
    }
    match (entries(&from_source), entries(&from_mirror)) {
        (Ok(live), Ok(backup)) => {
            held.files = backup.len();
            held.differing = differences(&live, &backup);
            Ok(held)
        }
        (Err(error), _) | (_, Err(error)) => {
            Err(Unheld::Failed(held, format!("held {}: {error}", short(x))))
        }
    }
}

/// `git archive <commit>` of `source`, extracted into the new directory
/// `into`.
fn archive(source: &str, commit: &str, into: &Path) -> Result<(), String> {
    let what = format!("git archive {commit} from {source}");
    fs::create_dir(into).map_err(|error| format!("{}: {error}", into.display()))?;
    let mut git = git();
    git.arg("-C")
        .arg(source)
        .args(["archive", "--format=tar", commit])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut archiver = git.spawn().map_err(|error| format!("{what}: {error}"))?;
    let Some(stream) = archiver.stdout.take() else {
        return Err(format!("{what}: no output"));
    };
    let tar = std::process::Command::new("tar")
        .args(["-x", "-p", "-f", "-", "-C"])
        .arg(into)
        .stdin(stream)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("tar -x: {error}"))?;
    let git_out = archiver
        .wait_with_output()
        .map_err(|error| format!("{what}: {error}"))?;
    if !git_out.status.success() {
        return Err(format!(
            "{what}: {}",
            String::from_utf8_lossy(&git_out.stderr).trim()
        ));
    }
    if !tar.status.success() {
        return Err(format!(
            "{what}: tar -x: {}",
            String::from_utf8_lossy(&tar.stderr).trim()
        ));
    }
    Ok(())
}

/// Whether `path` (relative) is left out of the compare.
fn left_out(path: &str) -> bool {
    LEFT_OUT.iter().any(|left| {
        path == *left
            || path
                .strip_prefix(left)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Every file, symlink and other non-directory under `root`, by its path
/// relative to it, symlinks never followed; [`LEFT_OUT`] not read.
fn entries(root: &Path) -> Result<BTreeMap<String, Entry>, String> {
    let mut found = BTreeMap::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let listing = fs::read_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        for item in listing {
            let item = item.map_err(|error| format!("{}: {error}", dir.display()))?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            let relative = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            if left_out(&relative) {
                continue;
            }
            let path = item.path();
            let meta = fs::symlink_metadata(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let kind = meta.file_type();
            let entry = if kind.is_dir() {
                pending.push((path, relative));
                continue;
            } else if kind.is_symlink() {
                Entry::Link(
                    fs::read_link(&path).map_err(|error| format!("{}: {error}", path.display()))?,
                )
            } else if kind.is_file() {
                Entry::File {
                    executable: executable(&meta),
                    sha256: sha256_file(&path)?,
                }
            } else {
                Entry::Other
            };
            found.insert(relative, entry);
        }
    }
    Ok(found)
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_meta: &fs::Metadata) -> bool {
    false
}

/// Each path whose entry differs between the live tree and the commit's,
/// in path order, named with how.
fn differences(live: &BTreeMap<String, Entry>, archived: &BTreeMap<String, Entry>) -> Vec<String> {
    let paths: BTreeSet<&String> = live.keys().chain(archived.keys()).collect();
    let mut differing = Vec::new();
    for path in paths {
        let how = match (live.get(path), archived.get(path)) {
            (Some(_), None) => "not in the commit".to_string(),
            (None, Some(_)) => "not in the live tree".to_string(),
            (Some(a), Some(b)) if a == b && *a != Entry::Other => continue,
            (
                Some(Entry::File {
                    executable: x1,
                    sha256: s1,
                }),
                Some(Entry::File {
                    executable: x2,
                    sha256: s2,
                }),
            ) => {
                let mut how = Vec::new();
                if s1 != s2 {
                    how.push("other bytes");
                }
                if x1 != x2 {
                    how.push("the executable bit differs");
                }
                how.join(", ")
            }
            (Some(Entry::Link(_)), Some(Entry::Link(_))) => {
                "the symlink's target differs".to_string()
            }
            (Some(Entry::File { .. }), Some(Entry::Link(_))) => {
                "a file live, a symlink in the commit".to_string()
            }
            (Some(Entry::Link(_)), Some(Entry::File { .. })) => {
                "a symlink live, a file in the commit".to_string()
            }
            _ => "neither a file nor a symlink".to_string(),
        };
        differing.push(format!("{path}: {how}"));
    }
    differing
}

#[cfg(test)]
mod tests {
    use super::{differences, entries, left_out};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    fn write(root: &Path, path: &str, text: &str, mode: u32) {
        let file = root.join(path);
        fs::create_dir_all(file.parent().expect("a parent")).expect("mkdir");
        fs::write(&file, text).expect("write");
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).expect("chmod");
    }

    fn tree(root: &Path) {
        write(root, "README.md", "readme\n", 0o644);
        write(root, "rsi/scenarios/001.sh", "#!/bin/sh\n", 0o755);
        write(root, "a/same.txt", "same\n", 0o644);
        symlink("README.md", root.join("LINK")).expect("symlink");
        symlink("README.md", root.join("kind")).expect("symlink");
    }

    #[test]
    fn the_tree_compare_names_every_kind_of_difference() {
        let base = std::env::temp_dir().join(format!(
            "spawn-198-compare-{}-{}",
            std::process::id(),
            super::super::super::random_hex(4).expect("random")
        ));
        let (live, archived) = (base.join("live"), base.join("archived"));
        tree(&live);
        tree(&archived);
        // What the controller adds is left out, on either side.
        write(&live, ".controller", "abc\n", 0o644);
        write(&live, ".deps", "0\n", 0o644);
        write(&live, "web/node_modules/x/index.js", "x\n", 0o644);
        assert!(left_out("web/node_modules") && left_out("web/node_modules/x/index.js"));
        assert!(!left_out("web/node_modules2") && !left_out(".depsx"));
        let equal = differences(
            &entries(&live).expect("live"),
            &entries(&archived).expect("archived"),
        );
        assert_eq!(equal, Vec::<String>::new(), "equal trees: none");

        write(&live, "README.md", "readme, changed\n", 0o644);
        fs::set_permissions(
            live.join("rsi/scenarios/001.sh"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        fs::remove_file(live.join("LINK")).expect("rm");
        symlink("rsi", live.join("LINK")).expect("symlink");
        fs::remove_file(live.join("a/same.txt")).expect("rm");
        write(&live, "b/extra.txt", "extra\n", 0o644);
        fs::remove_file(live.join("kind")).expect("rm");
        write(&live, "kind", "readme\n", 0o644);
        let differing = differences(
            &entries(&live).expect("live"),
            &entries(&archived).expect("archived"),
        );
        assert_eq!(
            differing,
            vec![
                "LINK: the symlink's target differs".to_string(),
                "README.md: other bytes".to_string(),
                "a/same.txt: not in the live tree".to_string(),
                "b/extra.txt: not in the commit".to_string(),
                "kind: a file live, a symlink in the commit".to_string(),
                "rsi/scenarios/001.sh: the executable bit differs".to_string(),
            ]
        );
        fs::remove_dir_all(&base).expect("clean up");
    }
}

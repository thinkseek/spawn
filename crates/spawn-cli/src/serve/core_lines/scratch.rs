//! Scratch nobody owns (objective 094): the top-level entries of the
//! scratch roots whose owner is gone or which nothing touched in a day.
//!
//! Each top-level entry of a root is one of:
//! - owned: its `.spawn-owner` file, `<pid> <start ticks>` (the integrator's
//!   pid file's form), names a process that still runs at that start time;
//!   or, with no marker, something in it changed in the last 24 h;
//! - gone: its marker names a process that does not run, or a pid now used
//!   by another process (other start ticks);
//! - idle: no marker, and nothing in it changed in 24 h.
//!
//! A claimed folder (objective 140, `spawn scratch claim`: its `.spawn-owner`
//! holds a claim, crate::scratch_owner) is owned while its lease runs, and
//! nobody's after ("lease ended"). An unmarked top-level directory (a
//! session's folder) may hold claimed folders one level below: each is
//! listed, and the entry itself is judged as above. Every claimed folder is
//! listed ([`Claimed`]) with its bytes and its build paths' bytes, those
//! reached through real directories only.
//!
//! Gone, idle and lease ended are "nobody owns". Never counted, whatever the
//! configuration says: symlinks (never followed, at any depth); an entry
//! holding any `*.jsonl` file (a transcript tree); `.transcripts`, names
//! starting `incident-`, names ending `.pending` or `-pending`;
//! `/data/recovery`, `/var/backups/spawn-sessions` and Claude Code's projects
//! directory (and anything under them, or holding them); the configured
//! excludes. Bytes are file lengths, as the storage walk counts them.
//!
//! Read only: nothing here removes, moves, renames or touches anything.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// The owner marker an entry may hold.
pub(super) const MARKER: &str = ".spawn-owner";

/// How long an unmarked entry with no change stays its maker's.
const IDLE_MS: i64 = 86_400_000;

/// Paths never counted, whatever a root takes in: the session backups'
/// recovery copies and the backups themselves (Claude Code's projects
/// directory is added at the walk, [`never_counted`]).
const NEVER: [&str; 2] = ["/data/recovery", "/var/backups/spawn-sessions"];

/// Why an entry is nobody's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reason {
    /// Its marker names a process that does not run (or a pid used again).
    Gone,
    /// No marker, and nothing in it changed in 24 h.
    Idle,
    /// Its claim's lease ended (objective 140).
    LeaseEnded,
}

impl Reason {
    pub(super) fn word(self) -> &'static str {
        match self {
            Reason::Gone => "gone",
            Reason::Idle => "idle",
            Reason::LeaseEnded => "lease ended",
        }
    }
}

/// A top-level entry nobody owns: its path, bytes and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Unowned {
    pub(super) path: PathBuf,
    pub(super) bytes: u64,
    pub(super) reason: Reason,
}

/// A claimed folder (objective 140): its path, its claim, its regular
/// files' bytes and its build paths' bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Claimed {
    pub(super) path: PathBuf,
    pub(super) claim: crate::scratch_owner::Claim,
    pub(super) bytes: u64,
    pub(super) build_bytes: u64,
}

/// What a walk found: the entries nobody owns and every claimed folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Walk {
    pub(super) unowned: Vec<Unowned>,
    pub(super) claimed: Vec<Claimed>,
}

/// How far one root's walk may go: entries visited and time spent. A walk
/// past either gives no count at all, never a partial one.
#[derive(Debug, Clone, Copy)]
pub(super) struct Bound {
    pub(super) entries: u64,
    pub(super) time: Duration,
}

impl Bound {
    /// 5 s and a million entries per root.
    pub(super) const DEFAULT: Bound = Bound {
        entries: 1_000_000,
        time: Duration::from_secs(5),
    };
}

/// The paths never counted: [`NEVER`] and Claude Code's projects directory
/// `projects`.
pub(super) fn never_counted(projects: &Path) -> Vec<PathBuf> {
    NEVER
        .iter()
        .map(PathBuf::from)
        .chain(std::iter::once(projects.to_path_buf()))
        .collect()
}

/// Whether a top-level name is never counted by its name: the transcript
/// archive, an incident's evidence, something pending.
fn fixed_name(name: &str) -> bool {
    name == ".transcripts"
        || name.starts_with("incident-")
        || name.ends_with(".pending")
        || name.ends_with("-pending")
}

/// Whether `entry` is, lies under, or holds one of `paths`.
fn related(entry: &Path, paths: &[PathBuf]) -> bool {
    paths
        .iter()
        .any(|path| entry.starts_with(path) || path.starts_with(entry))
}

/// Whether the claimed folder `folder`, met by the walk of one of `roots`,
/// may be swept (objective 145): none of its names below its root is one
/// the walk never counts ([`fixed_name`]; the walk checks only the top
/// level), and it is not, does not lie under and does not hold an exclude
/// or a never-counted place, each compared by its resolved path too.
pub(super) fn sweepable(
    folder: &Path,
    roots: &[PathBuf],
    excludes: &[PathBuf],
    never: &[PathBuf],
) -> bool {
    let Some(below) = roots.iter().find_map(|root| folder.strip_prefix(root).ok()) else {
        return false;
    };
    let mut places = with_resolved(excludes);
    places.extend(with_resolved(never));
    below
        .components()
        .all(|part| !fixed_name(&part.as_os_str().to_string_lossy()))
        && !related(folder, &places)
        && std::fs::canonicalize(folder).is_ok_and(|resolved| !related(&resolved, &places))
}

/// Why `root` is never read, when it is not: a symlink at the root or at
/// any parent component (cc-astra A27), so an alias can never take a walk
/// into a protected place under a name the protections do not know. A root
/// that is not there is not refused (it has nothing).
fn refused_root(root: &Path) -> Option<String> {
    std::fs::symlink_metadata(root).ok()?;
    for part in root.ancestors().filter(|part| !part.as_os_str().is_empty()) {
        if std::fs::symlink_metadata(part).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Some(format!(
                "{} is a symlink: a scratch root is never read through one",
                part.display()
            ));
        }
    }
    match std::fs::canonicalize(root) {
        Ok(resolved) if resolved == root => None,
        Ok(resolved) => Some(format!(
            "{} resolves to {}: a scratch root is named by its resolved path",
            root.display(),
            resolved.display()
        )),
        Err(error) => Some(format!("{}: {error}", root.display())),
    }
}

/// `paths` and, beside each, its resolved path, so a protected or excluded
/// location named through a symlink still matches the real entries a walk
/// meets.
fn with_resolved(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut all = paths.to_vec();
    for path in paths {
        if let Ok(resolved) = std::fs::canonicalize(path)
            && !all.contains(&resolved)
        {
            all.push(resolved);
        }
    }
    all
}

/// What one entry's walk found.
#[derive(Debug, Default)]
struct Found {
    bytes: u64,
    newest_ms: i64,
    jsonl: bool,
}

fn mtime_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The walk's budget: entries left and when it must end.
struct Budget {
    left: u64,
    until: Instant,
}

impl Budget {
    /// One more entry: false when the walk is over its bound.
    fn take(&mut self) -> bool {
        if self.left == 0 || Instant::now() >= self.until {
            return false;
        }
        self.left -= 1;
        true
    }
}

/// Walk `entry` (not following symlinks): its regular files' lengths, the
/// newest change of anything in it but symlinks (itself included), and
/// whether it holds a `*.jsonl` file. `None` when the budget runs out.
fn found(entry: &Path, metadata: &std::fs::Metadata, budget: &mut Budget) -> Option<Found> {
    let mut found = Found {
        newest_ms: mtime_ms(metadata),
        ..Found::default()
    };
    if metadata.is_file() {
        found.bytes = metadata.len();
        found.jsonl = entry.extension().is_some_and(|ext| ext == "jsonl");
        return Some(found);
    }
    if !metadata.is_dir() {
        return Some(found);
    }
    let mut pending = vec![entry.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(children) = std::fs::read_dir(&directory) else {
            continue;
        };
        for child in children.flatten() {
            if !budget.take() {
                return None;
            }
            let path = child.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            // A symlink is never followed, nor dated: making one changes
            // the directory that holds it.
            if metadata.file_type().is_symlink() {
                continue;
            }
            found.newest_ms = found.newest_ms.max(mtime_ms(&metadata));
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                found.bytes = found.bytes.saturating_add(metadata.len());
                if path.extension().is_some_and(|ext| ext == "jsonl") {
                    found.jsonl = true;
                }
            }
        }
    }
    Some(found)
}

/// The bytes of `folder`'s build paths (each counted once), only those
/// reached through real directories: a part that is a symlink, or missing,
/// gives nothing. `None` when the budget runs out.
fn build_bytes(folder: &Path, build: &[String], budget: &mut Budget) -> Option<u64> {
    let mut total = 0_u64;
    let mut seen: Vec<&str> = Vec::new();
    for path in build {
        if seen.contains(&path.as_str()) {
            continue;
        }
        seen.push(path);
        let mut at = folder.to_path_buf();
        let mut real = true;
        for part in Path::new(path).components() {
            at.push(part);
            if !std::fs::symlink_metadata(&at)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            {
                real = false;
                break;
            }
        }
        if real {
            let metadata = std::fs::symlink_metadata(&at).ok()?;
            total = total.saturating_add(found(&at, &metadata, budget)?.bytes);
        }
    }
    Some(total)
}

/// `folder`'s claim as a [`Claimed`], its bytes `bytes` (the walk's count),
/// or `None` without one. `Err(())` when the budget runs out.
fn claimed(folder: &Path, bytes: u64, budget: &mut Budget) -> Result<Option<Claimed>, ()> {
    let Some(claim) = crate::scratch_owner::claim_in(folder) else {
        return Ok(None);
    };
    let build_bytes = build_bytes(folder, &claim.build, budget).ok_or(())?;
    Ok(Some(Claimed {
        path: folder.to_path_buf(),
        claim,
        bytes,
        build_bytes,
    }))
}

/// The claimed folders one level below the unmarked directory `entry`.
/// `Err(())` when the budget runs out.
fn claimed_below(entry: &Path, budget: &mut Budget) -> Result<Vec<Claimed>, ()> {
    let mut below = Vec::new();
    let Ok(children) = std::fs::read_dir(entry) else {
        return Ok(below);
    };
    for child in children.flatten() {
        let path = child.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        if crate::scratch_owner::claim_in(&path).is_none() {
            continue;
        }
        let bytes = found(&path, &metadata, budget).ok_or(())?.bytes;
        below.extend(claimed(&path, bytes, budget)?);
    }
    Ok(below)
}

/// Whose `entry` is by its marker: `Some(true)` a process that still runs
/// at the marker's start time, `Some(false)` one gone (or a pid used
/// again), `None` without a marker that reads as `<pid> <start ticks>`.
fn marker_runs(entry: &Path, runs: &dyn Fn(u32, u64) -> bool) -> Option<bool> {
    let marker = entry.join(MARKER);
    let metadata = std::fs::symlink_metadata(&marker).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&marker).ok()?;
    let (pid, ticks) = crate::integrate::read_pid_file(&text)?;
    Some(runs(pid, ticks))
}

/// The entries of `root` nobody owns at `now_ms`, those `excludes` and
/// `never` name left out (each compared on its resolved path too), `runs`
/// saying whether a marker's process still runs, with every claimed folder
/// it met (objective 140): at the top level, owned while its lease runs and
/// nobody's after; one level below an unmarked directory, listed while the
/// entry is judged as 094 judges it. `Err` (why) when the walk goes past
/// `bound` (no partial count) or the root is refused ([`refused_root`]).
pub(super) fn walk_in(
    root: &Path,
    excludes: &[PathBuf],
    never: &[PathBuf],
    now_ms: i64,
    runs: &dyn Fn(u32, u64) -> bool,
    bound: Bound,
) -> Result<Walk, String> {
    let mut budget = Budget {
        left: bound.entries,
        until: Instant::now() + bound.time,
    };
    let over = || {
        format!(
            "the walk of {} ran over its bound ({} entries, {} ms)",
            root.display(),
            bound.entries,
            bound.time.as_millis()
        )
    };
    if let Some(why) = refused_root(root) {
        return Err(why);
    }
    let excludes = &with_resolved(excludes);
    let never = &with_resolved(never);
    let entries =
        std::fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let mut walk = Walk::default();
    for entry in entries.flatten() {
        if !budget.take() {
            return Err(over());
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        let name = entry.file_name();
        if fixed_name(&name.to_string_lossy()) || related(&path, never) || related(&path, excludes)
        {
            continue;
        }
        let Some(found) = found(&path, &metadata, &mut budget) else {
            return Err(over());
        };
        if found.jsonl {
            continue;
        }
        if metadata.is_dir() {
            match claimed(&path, found.bytes, &mut budget).map_err(|()| over())? {
                Some(claim) => {
                    let ended = claim.claim.lease_until_ms <= now_ms;
                    walk.claimed.push(claim);
                    if ended {
                        walk.unowned.push(Unowned {
                            path,
                            bytes: found.bytes,
                            reason: Reason::LeaseEnded,
                        });
                    }
                    continue;
                }
                None if std::fs::symlink_metadata(path.join(MARKER)).is_err() => {
                    walk.claimed
                        .extend(claimed_below(&path, &mut budget).map_err(|()| over())?);
                }
                None => {}
            }
        }
        let reason = match marker_runs(&path, runs) {
            Some(true) => continue,
            Some(false) => Reason::Gone,
            None if now_ms.saturating_sub(found.newest_ms) < IDLE_MS => continue,
            None => Reason::Idle,
        };
        walk.unowned.push(Unowned {
            path,
            bytes: found.bytes,
            reason,
        });
    }
    Ok(walk)
}

/// The roots of `roots` that are refused ([`refused_root`]), each with why,
/// in order: the answer names them, and the figure is not collected while
/// any is (a count without them is not the whole).
pub(super) fn refused_roots(roots: &[PathBuf]) -> Vec<(PathBuf, String)> {
    roots
        .iter()
        .filter_map(|root| refused_root(root).map(|why| (root.clone(), why)))
        .collect()
}

/// Every root's entries nobody owns, largest first (then by path): `Err`
/// when a root's walk goes past its bound. A root that cannot be read has
/// none, and so has a refused one ([`refused_root`]): its error is reported
/// and it counts nothing.
#[cfg(test)]
pub(super) fn unowned(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    never: &[PathBuf],
    now_ms: i64,
    runs: &dyn Fn(u32, u64) -> bool,
    bound: Bound,
) -> Result<Vec<Unowned>, String> {
    walk(roots, excludes, never, now_ms, runs, bound).map(|walk| walk.unowned)
}

/// [`unowned`]'s walk of every root, with every claimed folder it met, by
/// agent then oldest first (then by path).
pub(super) fn walk(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    never: &[PathBuf],
    now_ms: i64,
    runs: &dyn Fn(u32, u64) -> bool,
    bound: Bound,
) -> Result<Walk, String> {
    let mut all = Vec::new();
    let mut claimed = Vec::new();
    for root in roots {
        if let Some(why) = refused_root(root) {
            eprintln!(
                "core-lines: scratch root {}: {why}; counted as nothing",
                root.display()
            );
            continue;
        }
        match walk_in(root, excludes, never, now_ms, runs, bound) {
            Ok(found) => {
                all.extend(found.unowned);
                claimed.extend(found.claimed);
            }
            Err(why) if std::fs::symlink_metadata(root).is_err() => {
                eprintln!("core-lines: scratch root {}: {why}", root.display());
            }
            Err(why) => return Err(why),
        }
    }
    all.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    claimed.sort_by(|left: &Claimed, right: &Claimed| {
        left.claim
            .agent
            .cmp(&right.claim.agent)
            .then_with(|| left.claim.created_ms.cmp(&right.claim.created_ms))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(Walk {
        unowned: all,
        claimed,
    })
}

#[cfg(test)]
mod tests {
    use super::{Bound, MARKER, Reason, never_counted, refused_roots, sweepable, unowned, walk};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    const DAY_MS: i64 = 86_400_000;

    /// A scratch directory under a scratch HOME, removed when the test
    /// ends. The test refuses the real home.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root = crate::storage::tests::scratch("unowned");
            if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
                assert!(
                    !root.starts_with(&home),
                    "the scratch must not be in the real home: {}",
                    root.display()
                );
            }
            std::fs::create_dir_all(&root).expect("mkdir");
            // Resolved, so a symlink in the temporary directory's own path
            // cannot make every root here a refused one.
            Scratch(std::fs::canonicalize(&root).expect("resolve"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fill(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(path, vec![0_u8; bytes]).expect("write");
    }

    /// Set every path under `path` (and itself, not following symlinks) to
    /// `at`.
    fn age(path: &Path, at: SystemTime) {
        let mut pending = vec![path.to_path_buf()];
        let mut all = Vec::new();
        while let Some(next) = pending.pop() {
            all.push(next.clone());
            let metadata = std::fs::symlink_metadata(&next).expect("lstat");
            if metadata.is_dir() {
                for child in std::fs::read_dir(&next).expect("read").flatten() {
                    pending.push(child.path());
                }
            }
        }
        // Children first, so a directory's own time is set last.
        for path in all.iter().rev() {
            let metadata = std::fs::symlink_metadata(path).expect("lstat");
            if metadata.file_type().is_symlink() {
                continue;
            }
            std::fs::File::options()
                .read(true)
                .open(path)
                .and_then(|file| file.set_modified(at))
                .expect("set the time");
        }
    }

    fn ms(at: SystemTime) -> i64 {
        i64::try_from(
            at.duration_since(SystemTime::UNIX_EPOCH)
                .expect("after the epoch")
                .as_millis(),
        )
        .expect("ms")
    }

    /// The classification: owned by its marker, gone, a pid used again,
    /// idle, fresh unmarked, each fixed exclusion, a configured exclude, a
    /// symlink never followed.
    #[test]
    fn each_entry_is_owned_gone_or_idle_and_the_exclusions_never_count() {
        let scratch = Scratch::new();
        let root = scratch.0.join("root");
        let outside = scratch.0.join("outside");
        let projects = scratch.0.join("projects");
        fill(&outside.join("big"), 50_000);
        fill(&root.join("owned/data"), 1000);
        std::fs::write(root.join("owned").join(MARKER), "41 100\n").expect("marker");
        fill(&root.join("gone/data"), 3000);
        std::fs::write(root.join("gone").join(MARKER), "42 100\n").expect("marker");
        fill(&root.join("reused/data"), 2000);
        std::fs::write(root.join("reused").join(MARKER), "41 101\n").expect("marker");
        fill(&root.join("idle/a/data"), 4000);
        fill(&root.join("idle/b"), 10);
        std::os::unix::fs::symlink(&outside, root.join("idle/a/elsewhere")).expect("link");
        fill(&root.join("fresh/data"), 500);
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("link");
        for name in [
            ".transcripts/x",
            "incident-2026-11-12-x/x",
            "x.rebuilt-pending/x",
            "y.pending/x",
            "sessions/deep/s.jsonl",
            "notes/x",
            "projects-in-root/x",
        ] {
            fill(&root.join(name), 700);
        }
        fill(&root.join("loose.txt"), 60);
        let old = SystemTime::now() - Duration::from_hours(48);
        for name in [
            "owned",
            "gone",
            "reused",
            "idle",
            ".transcripts",
            "incident-2026-11-12-x",
            "x.rebuilt-pending",
            "y.pending",
            "sessions",
            "notes",
            "projects-in-root",
            "loose.txt",
        ] {
            age(&root.join(name), old);
        }
        let now = ms(SystemTime::now());
        // Process 41 runs, started at tick 100; 42 does not.
        let runs = |pid: u32, ticks: u64| pid == 41 && ticks == 100;
        let mut never = never_counted(&projects);
        never.push(root.join("projects-in-root"));
        let found = unowned(
            std::slice::from_ref(&root),
            &[root.join("notes")],
            &never,
            now,
            &runs,
            Bound::DEFAULT,
        )
        .expect("within the bound");
        let got: Vec<(String, u64, Reason)> = found
            .iter()
            .map(|entry| {
                (
                    entry
                        .path
                        .strip_prefix(&root)
                        .expect("under the root")
                        .display()
                        .to_string(),
                    entry.bytes,
                    entry.reason,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("idle".to_string(), 4010, Reason::Idle),
                ("gone".to_string(), 3007, Reason::Gone),
                ("reused".to_string(), 2007, Reason::Gone),
                ("loose.txt".to_string(), 60, Reason::Idle),
            ],
            "the symlink inside idle is not followed; owned, fresh, the link and \
             every excluded entry are left out"
        );
        // A fresh change inside an unmarked entry makes it someone's again.
        fill(&root.join("idle/b"), 10);
        let found = unowned(
            std::slice::from_ref(&root),
            &[root.join("notes")],
            &never,
            now,
            &runs,
            Bound::DEFAULT,
        )
        .expect("within the bound");
        assert!(found.iter().all(|entry| !entry.path.ends_with("idle")));
        // The fixed paths hold whatever a root takes in.
        assert!(never.contains(&PathBuf::from("/data/recovery")));
        assert!(never.contains(&PathBuf::from("/var/backups/spawn-sessions")));
        assert!(never.contains(&projects));
    }

    #[test]
    fn a_walk_past_its_bound_gives_no_count() {
        let scratch = Scratch::new();
        let root = scratch.0.join("root");
        for n in 0..20 {
            fill(&root.join(format!("e{n}/f")), 10);
        }
        let runs = |_: u32, _: u64| false;
        let over = unowned(
            std::slice::from_ref(&root),
            &[],
            &[],
            0,
            &runs,
            Bound {
                entries: 5,
                time: Duration::from_secs(5),
            },
        );
        assert!(
            over.as_ref().is_err_and(|why| why.contains("ran over")),
            "{over:?}"
        );
        let timed = unowned(
            std::slice::from_ref(&root),
            &[],
            &[],
            0,
            &runs,
            Bound {
                entries: 1_000,
                time: Duration::ZERO,
            },
        );
        assert!(timed.is_err(), "{timed:?}");
        // A root that is not there has nothing.
        let none = unowned(
            &[scratch.0.join("absent")],
            &[],
            &[],
            DAY_MS,
            &runs,
            Bound::DEFAULT,
        );
        assert_eq!(none, Ok(Vec::new()));
    }

    /// cc-astra A27: a root that is a symlink, or that has a symlinked
    /// parent component, is refused and counts nothing, though what it
    /// leads to holds an old idle entry that would count. A protected
    /// location named through an alias still protects the real path.
    #[test]
    fn a_root_reached_through_a_symlink_counts_nothing() {
        let scratch = Scratch::new();
        let protected = scratch.0.join("protected");
        let old = SystemTime::now() - Duration::from_hours(720);
        fill(&protected.join("old/f"), 100);
        age(&protected.join("old"), old);
        let runs = |_: u32, _: u64| false;
        let now = ms(SystemTime::now());
        let count = |roots: &[PathBuf], never: &[PathBuf]| {
            unowned(roots, &[], never, now, &runs, Bound::DEFAULT)
        };
        // The stand-in counts when read as itself and unprotected, so the
        // refusals below are not vacuous.
        let real = count(std::slice::from_ref(&protected), &[]).expect("read");
        assert_eq!(real.len(), 1, "{real:?}");

        // A root alias onto the protected stand-in.
        let alias = scratch.0.join("alias");
        std::os::unix::fs::symlink(&protected, &alias).expect("symlink");
        assert_eq!(
            count(
                std::slice::from_ref(&alias),
                std::slice::from_ref(&protected)
            ),
            Ok(Vec::new())
        );
        assert_eq!(count(std::slice::from_ref(&alias), &[]), Ok(Vec::new()));

        // A parent alias: the root's own name is real, a component above it
        // is a symlink.
        let parent = scratch.0.join("parent");
        std::os::unix::fs::symlink(&scratch.0, &parent).expect("symlink");
        let through = parent.join("protected");
        assert_eq!(
            count(
                std::slice::from_ref(&through),
                std::slice::from_ref(&protected)
            ),
            Ok(Vec::new())
        );
        assert_eq!(count(std::slice::from_ref(&through), &[]), Ok(Vec::new()));

        // The refused ones are named, with why; a real root and one that is
        // not there are not refused.
        let absent = scratch.0.join("absent");
        let refused = refused_roots(&[protected.clone(), alias.clone(), through.clone(), absent]);
        assert_eq!(
            refused
                .iter()
                .map(|(root, _)| root.clone())
                .collect::<Vec<_>>(),
            vec![alias.clone(), through.clone()]
        );
        assert!(refused[0].1.contains("is a symlink"), "{refused:?}");
        assert!(
            refused[1].1.contains(&parent.display().to_string()),
            "{refused:?}"
        );

        // A real root, its protected entry named through an alias: compared
        // on the resolved path, so it never counts.
        let named = alias.join("old");
        assert_eq!(
            count(
                std::slice::from_ref(&protected),
                std::slice::from_ref(&named)
            ),
            Ok(Vec::new())
        );
    }

    /// Writes a claim (objective 140) into `folder`, its lease until
    /// `until`: the owner file's size.
    fn claim(folder: &Path, agent: &str, created: i64, until: i64, build: &[&str]) -> u64 {
        let line = crate::scratch_owner::Claim {
            agent: agent.to_string(),
            objective: "4/140".to_string(),
            created_ms: created,
            lease_until_ms: until,
            build: build.iter().map(ToString::to_string).collect(),
        }
        .line();
        std::fs::create_dir_all(folder).expect("mkdir");
        std::fs::write(folder.join(MARKER), format!("{line}\n")).expect("claim");
        u64::try_from(line.len() + 1).expect("size")
    }

    /// Objective 140: a claimed folder is owned while its lease runs and
    /// nobody's ("lease ended") after; claims are listed at the top level and
    /// one level below an unmarked directory (never deeper, never below a
    /// marked one, never through a symlink), with their build paths' bytes
    /// reached through real directories only.
    #[test]
    fn a_claim_is_owned_while_its_lease_runs_and_listed_by_owner() {
        let scratch = Scratch::new();
        let root = scratch.0.join("root");
        let outside = scratch.0.join("outside");
        fill(&outside.join("target/big"), 50_000);
        let now = ms(SystemTime::now());
        let old = SystemTime::now() - Duration::from_hours(48);

        fill(&root.join("leased/data"), 1000);
        fill(&root.join("leased/t/target/x"), 300);
        std::fs::create_dir_all(root.join("leased/via")).expect("mkdir");
        std::os::unix::fs::symlink(&outside, root.join("leased/link")).expect("link");
        std::os::unix::fs::symlink(outside.join("target"), root.join("leased/via/target"))
            .expect("link");
        let leased = claim(
            &root.join("leased"),
            "rev-e",
            now - 10,
            now + 3_600_000,
            &[
                "t/target",
                "t/target",
                "link/target",
                "via/target",
                "absent/target",
            ],
        );
        fill(&root.join("ended/data"), 2000);
        let ended = claim(&root.join("ended"), "rev-b", now - 20, now, &[]);
        fill(&root.join("session/fresh"), 10);
        let below = claim(&root.join("session/a"), "rev-e", now - 30, now + 1, &[]);
        fill(&root.join("session/a/data"), 70);
        claim(&root.join("session/b/deep"), "rev-x", 1, now + 1, &[]);
        fill(&root.join("old/data"), 40);
        let old_below = claim(&root.join("old/c"), "rev-a", 5, now + 1, &[]);
        fill(&root.join("marked/data"), 10);
        std::fs::write(root.join("marked").join(MARKER), "41 100\n").expect("marker");
        claim(&root.join("marked/d"), "rev-x", 1, now + 1, &[]);
        claim(&outside.join("held"), "rev-x", 1, now + 1, &[]);
        std::os::unix::fs::symlink(outside.join("held"), root.join("link")).expect("link");
        for name in ["leased", "ended", "old", "marked"] {
            age(&root.join(name), old);
        }

        let found = walk(
            std::slice::from_ref(&root),
            &[],
            &[],
            now,
            &|pid, _| pid == 41,
            Bound::DEFAULT,
        )
        .expect("a walk");
        let unowned: Vec<(String, u64, Reason)> = found
            .unowned
            .iter()
            .map(|entry| {
                let name = entry.path.strip_prefix(&root).expect("in the root");
                (name.display().to_string(), entry.bytes, entry.reason)
            })
            .collect();
        assert_eq!(
            unowned,
            [
                ("ended".to_string(), 2000 + ended, Reason::LeaseEnded),
                ("old".to_string(), 40 + old_below, Reason::Idle),
            ]
        );
        let claimed: Vec<(String, &str, u64, u64)> = found
            .claimed
            .iter()
            .map(|folder| {
                let name = folder.path.strip_prefix(&root).expect("in the root");
                (
                    name.display().to_string(),
                    folder.claim.agent.as_str(),
                    folder.bytes,
                    folder.build_bytes,
                )
            })
            .collect();
        assert_eq!(
            claimed,
            [
                ("old/c".to_string(), "rev-a", old_below, 0),
                ("ended".to_string(), "rev-b", 2000 + ended, 0),
                ("session/a".to_string(), "rev-e", 70 + below, 0),
                ("leased".to_string(), "rev-e", 1300 + leased, 300),
            ],
            "by agent, then oldest first"
        );
        assert_eq!(
            found.claimed[1].claim.lease_until_ms, now,
            "an ended lease is still listed"
        );
        assert!(
            ["leased", "ended", "old", "marked", "session"]
                .iter()
                .all(|name| root.join(name).exists()),
            "nothing removed"
        );
    }

    /// A claimed folder under a name or in a place the walk never counts is
    /// never swept (objective 145), whatever the walk's top level allowed.
    #[test]
    fn a_folder_the_walk_never_counts_is_never_swept() {
        let scratch = Scratch::new();
        let root = scratch.0.join("root");
        let projects = scratch.0.join("projects");
        let excluded = root.join("sess/kept");
        for folder in [
            "sess/deep",
            "sess/incident-9",
            "sess/x.pending",
            "sess/x-pending",
            "sess/.transcripts",
            "sess/kept",
            "projects-link/a",
        ] {
            std::fs::create_dir_all(root.join(folder)).expect("mkdir");
        }
        std::fs::create_dir_all(&projects).expect("mkdir");
        std::os::unix::fs::symlink(&root.join("projects-link"), projects.join("alias"))
            .expect("symlink");
        let roots = [root.clone()];
        let excludes = [excluded];
        let never = never_counted(&projects.join("alias"));
        let swept = |folder: &str| sweepable(&root.join(folder), &roots, &excludes, &never);
        assert!(swept("sess/deep"));
        for kept in [
            "sess/incident-9",
            "sess/x.pending",
            "sess/x-pending",
            "sess/.transcripts",
            "sess/kept",
            "projects-link/a",
        ] {
            assert!(!swept(kept), "{kept}");
        }
        assert!(
            !sweepable(&scratch.0.join("elsewhere"), &roots, &excludes, &never),
            "under no root"
        );
    }
}

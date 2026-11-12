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
//! Gone and idle are "nobody owns". Never counted, whatever the
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
}

impl Reason {
    pub(super) fn word(self) -> &'static str {
        match self {
            Reason::Gone => "gone",
            Reason::Idle => "idle",
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
/// saying whether a marker's process still runs. `Err` (why) when the walk
/// goes past `bound` (no partial count) or the root is refused
/// ([`refused_root`]).
pub(super) fn unowned_in(
    root: &Path,
    excludes: &[PathBuf],
    never: &[PathBuf],
    now_ms: i64,
    runs: &dyn Fn(u32, u64) -> bool,
    bound: Bound,
) -> Result<Vec<Unowned>, String> {
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
    let mut unowned = Vec::new();
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
        let reason = match marker_runs(&path, runs) {
            Some(true) => continue,
            Some(false) => Reason::Gone,
            None if now_ms.saturating_sub(found.newest_ms) < IDLE_MS => continue,
            None => Reason::Idle,
        };
        unowned.push(Unowned {
            path,
            bytes: found.bytes,
            reason,
        });
    }
    Ok(unowned)
}

/// Every root's entries nobody owns, largest first (then by path): `Err`
/// when a root's walk goes past its bound. A root that cannot be read has
/// none, and so has a refused one ([`refused_root`]): its error is reported
/// and it counts nothing.
pub(super) fn unowned(
    roots: &[PathBuf],
    excludes: &[PathBuf],
    never: &[PathBuf],
    now_ms: i64,
    runs: &dyn Fn(u32, u64) -> bool,
    bound: Bound,
) -> Result<Vec<Unowned>, String> {
    let mut all = Vec::new();
    for root in roots {
        if let Some(why) = refused_root(root) {
            eprintln!(
                "core-lines: scratch root {}: {why}; counted as nothing",
                root.display()
            );
            continue;
        }
        match unowned_in(root, excludes, never, now_ms, runs, bound) {
            Ok(found) => all.extend(found),
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
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::{Bound, MARKER, Reason, never_counted, unowned};
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
}

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
//! nobody's after ("lease ended"). Every claimed folder is listed
//! ([`Claimed`]) with its build paths' bytes, those reached through real
//! directories only, and its bytes once its lease has ended.
//!
//! So that a large root is collected within its bound (objective 155):
//! - a folder whose claim's lease runs is never walked whole: its build
//!   paths only (its bytes are `None`);
//! - an entry whose marker's process runs is never walked; an unmarked,
//!   unclaimed entry's walk stops at its first change in the last 24 h (it
//!   is its maker's: its bytes are never needed);
//! - an unmarked, unclaimed directory holding a claim whose lease runs one
//!   level below is a session folder: no entry itself, each of its children
//!   judged as an entry of its own, by the rules above, so one claim hides
//!   none of its unclaimed siblings. An unmarked directory holding only
//!   ended claims is judged as an entry, and those claims are listed;
//! - a root's walk skips an entry that is, or holds, another root (a root
//!   inside a root is walked as its own); an exclude that is the root walked
//!   is for the roots around it, and an exclude that holds a root refuses
//!   that root, said ([`refused_roots`]).
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
/// files' bytes (`None` while its lease runs: never walked whole, objective
/// 155) and its build paths' bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Claimed {
    pub(super) path: PathBuf,
    pub(super) claim: crate::scratch_owner::Claim,
    pub(super) bytes: Option<u64>,
    pub(super) build_bytes: u64,
}

/// What a walk found: the entries nobody owns and every claimed folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Walk {
    pub(super) unowned: Vec<Unowned>,
    pub(super) claimed: Vec<Claimed>,
}

/// How far one root's walk may go: entries visited and time spent. A walk
/// past either gives no count at all, never a partial one. The Core lines
/// file's `storage.scratch_bound` sets it (objective 155).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl Default for Bound {
    fn default() -> Self {
        Bound::DEFAULT
    }
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
/// into a protected place under a name the protections do not know; or an
/// exclude of `excludes` that holds it (objective 155: never a silent
/// nothing). A root that is not there is not refused (it has nothing).
fn refused_root(root: &Path, excludes: &[PathBuf]) -> Option<String> {
    std::fs::symlink_metadata(root).ok()?;
    if let Some(why) = refused_link(root) {
        return Some(why);
    }
    with_resolved(excludes)
        .iter()
        .find(|exclude| root != exclude.as_path() && root.starts_with(exclude))
        .map(|exclude| {
            format!(
                "exclude {} hides root {}: a root under an exclude is never read",
                exclude.display(),
                root.display()
            )
        })
}

/// Why `root` is refused through a symlink ([`refused_root`]).
fn refused_link(root: &Path) -> Option<String> {
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
    /// The walk stopped at a change after its `recent_after` (objective
    /// 155): its bytes and `jsonl` are then partial, and never used.
    recent: bool,
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
/// whether it holds a `*.jsonl` file. With `recent_after`, the walk stops at
/// the first change after it (`recent`). `None` when the budget runs out.
fn found(
    entry: &Path,
    metadata: &std::fs::Metadata,
    budget: &mut Budget,
    recent_after: Option<i64>,
) -> Option<Found> {
    let recent = |ms: i64| recent_after.is_some_and(|after| ms > after);
    let mut found = Found {
        newest_ms: mtime_ms(metadata),
        ..Found::default()
    };
    if recent(found.newest_ms) {
        found.recent = true;
        return Some(found);
    }
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
            if recent(found.newest_ms) {
                found.recent = true;
                return Some(found);
            }
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
            total = total.saturating_add(found(&at, &metadata, budget, None)?.bytes);
        }
    }
    Some(total)
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

/// One root's walk in progress (objective 155): what it leaves out, its
/// budget, and what it found so far. A session folder's children go through
/// the same rules ([`Judging::judge`]) as the root's entries.
struct Judging<'a> {
    root: &'a Path,
    /// The other roots, each with its resolved path: an entry that is, or
    /// holds, one is that root's, never this one's.
    others: Vec<PathBuf>,
    /// The excludes, each with its resolved path, but the root itself (an
    /// exclude that is a root is for the roots around it).
    excludes: Vec<PathBuf>,
    never: Vec<PathBuf>,
    now_ms: i64,
    runs: &'a dyn Fn(u32, u64) -> bool,
    bound: Bound,
    budget: Budget,
    walk: Walk,
}

impl Judging<'_> {
    /// Why the walk stops: past its bound.
    fn over(&self) -> String {
        format!(
            "the walk of {} ran over its bound ({} entries, {} ms)",
            self.root.display(),
            self.bound.entries,
            self.bound.time.as_millis()
        )
    }

    /// Whether the entry `path` (named `name`) is never counted: a fixed
    /// name, a never-counted place, an exclude, or another root (it is, or
    /// holds, one).
    fn left_out(&self, path: &Path, name: &str) -> bool {
        fixed_name(name)
            || related(path, &self.never)
            || related(path, &self.excludes)
            || self.others.iter().any(|other| other.starts_with(path))
    }

    /// Judges every entry of `directory`: the root's (`top`; a directory
    /// that cannot be read is an error) or a session folder's (one that
    /// cannot be read has none).
    fn entries_of(&mut self, directory: &Path, top: bool) -> Result<(), String> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if top => return Err(format!("{}: {error}", directory.display())),
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            if !self.budget.take() {
                return Err(self.over());
            }
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink()
                || self.left_out(&path, &entry.file_name().to_string_lossy())
            {
                continue;
            }
            self.judge(path, &metadata, top)?;
        }
        Ok(())
    }

    /// One entry: claimed (owned while its lease runs, its build paths
    /// only; lease ended after, walked whole), its marker's process running
    /// (never walked), gone, or unmarked (fresh, its walk stopped at its
    /// first recent change; idle). A top-level unmarked directory holding a
    /// running claim one level below is a session folder: its children are
    /// judged instead, and it is none.
    fn judge(&mut self, path: PathBuf, metadata: &std::fs::Metadata, top: bool) -> Result<(), String> {
        if metadata.is_dir()
            && let Some(claim) = crate::scratch_owner::claim_in(&path)
        {
            return self.claimed(path, metadata, claim);
        }
        let reason = match marker_runs(&path, self.runs) {
            Some(true) => return Ok(()),
            Some(false) => Reason::Gone,
            None => Reason::Idle,
        };
        let unmarked = metadata.is_dir() && std::fs::symlink_metadata(path.join(MARKER)).is_err();
        if top && unmarked && self.session_folder(&path)? {
            return self.entries_of(&path, false);
        }
        let recent_after = (reason == Reason::Idle).then(|| self.now_ms.saturating_sub(IDLE_MS));
        let Some(found) = found(&path, metadata, &mut self.budget, recent_after) else {
            return Err(self.over());
        };
        if top && unmarked {
            self.claimed_below(&path)?;
        }
        if found.recent || found.jsonl {
            return Ok(());
        }
        self.walk.unowned.push(Unowned {
            path,
            bytes: found.bytes,
            reason,
        });
        Ok(())
    }

    /// A claimed folder: while its lease runs, its build paths only (its
    /// bytes `None`); after, walked whole and nobody's ("lease ended"),
    /// unless it holds a transcript.
    fn claimed(
        &mut self,
        path: PathBuf,
        metadata: &std::fs::Metadata,
        claim: crate::scratch_owner::Claim,
    ) -> Result<(), String> {
        let bytes = if claim.lease_until_ms > self.now_ms {
            None
        } else {
            let Some(found) = found(&path, metadata, &mut self.budget, None) else {
                return Err(self.over());
            };
            if found.jsonl {
                return Ok(());
            }
            self.walk.unowned.push(Unowned {
                path: path.clone(),
                bytes: found.bytes,
                reason: Reason::LeaseEnded,
            });
            Some(found.bytes)
        };
        let Some(build_bytes) = build_bytes(&path, &claim.build, &mut self.budget) else {
            return Err(self.over());
        };
        self.walk.claimed.push(Claimed {
            path,
            claim,
            bytes,
            build_bytes,
        });
        Ok(())
    }

    /// The child directories of `entry` that hold a claim, each with it.
    fn claims_below(
        &mut self,
        entry: &Path,
    ) -> Result<Vec<(PathBuf, std::fs::Metadata, crate::scratch_owner::Claim)>, String> {
        let mut below = Vec::new();
        let Ok(children) = std::fs::read_dir(entry) else {
            return Ok(below);
        };
        for child in children.flatten() {
            if !self.budget.take() {
                return Err(self.over());
            }
            let path = child.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            if let Some(claim) = crate::scratch_owner::claim_in(&path) {
                below.push((path, metadata, claim));
            }
        }
        Ok(below)
    }

    /// Whether the unmarked directory `entry` is a session folder: a child
    /// of it holds a claim whose lease runs. Only the claim files one level
    /// down are read; nothing is walked.
    fn session_folder(&mut self, entry: &Path) -> Result<bool, String> {
        let now_ms = self.now_ms;
        Ok(self
            .claims_below(entry)?
            .iter()
            .any(|(_, _, claim)| claim.lease_until_ms > now_ms))
    }

    /// The claimed folders one level below the unmarked directory `entry`,
    /// listed (their leases have ended: a running one makes `entry` a
    /// session folder). Their bytes are not `entry`'s count's own: the
    /// listing is beside it.
    fn claimed_below(&mut self, entry: &Path) -> Result<(), String> {
        for (path, metadata, claim) in self.claims_below(entry)? {
            let bytes = if claim.lease_until_ms > self.now_ms {
                None
            } else {
                let Some(found) = found(&path, &metadata, &mut self.budget, None) else {
                    return Err(self.over());
                };
                Some(found.bytes)
            };
            let Some(build_bytes) = build_bytes(&path, &claim.build, &mut self.budget) else {
                return Err(self.over());
            };
            self.walk.claimed.push(Claimed {
                path,
                claim,
                bytes,
                build_bytes,
            });
        }
        Ok(())
    }
}

/// The entries of `root` nobody owns at `now_ms`, with every claimed folder
/// it met: those `excludes` and `never` name left out (each compared on its
/// resolved path too; an exclude that is `root` itself is for the roots
/// around it), and any entry that is, or holds, another of `roots`; `runs`
/// saying whether a marker's process still runs ([`Judging`]). `Err` (why)
/// when the walk goes past `bound` (no partial count) or the root cannot be
/// read.
fn walk_in(
    root: &Path,
    roots: &[PathBuf],
    excludes: &[PathBuf],
    never: &[PathBuf],
    now_ms: i64,
    runs: &dyn Fn(u32, u64) -> bool,
    bound: Bound,
) -> Result<Walk, String> {
    let mut judging = Judging {
        root,
        others: with_resolved(roots)
            .into_iter()
            .filter(|other| other != root)
            .collect(),
        excludes: with_resolved(excludes)
            .into_iter()
            .filter(|exclude| exclude != root)
            .collect(),
        never: with_resolved(never),
        now_ms,
        runs,
        bound,
        budget: Budget {
            left: bound.entries,
            until: Instant::now() + bound.time,
        },
        walk: Walk::default(),
    };
    judging.entries_of(root, true)?;
    Ok(judging.walk)
}

/// The roots of `roots` that are refused ([`refused_root`]: a symlink at or
/// above them, or an exclude of `excludes` that holds them), each with why,
/// in order: the answer names them, and the figure is not collected while
/// any is (a count without them is not the whole).
pub(super) fn refused_roots(roots: &[PathBuf], excludes: &[PathBuf]) -> Vec<(PathBuf, String)> {
    roots
        .iter()
        .filter_map(|root| refused_root(root, excludes).map(|why| (root.clone(), why)))
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
        if let Some(why) = refused_root(root, excludes) {
            eprintln!(
                "core-lines: scratch root {}: {why}; counted as nothing",
                root.display()
            );
            continue;
        }
        match walk_in(root, roots, excludes, never, now_ms, runs, bound) {
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
        let refused = refused_roots(
            &[protected.clone(), alias.clone(), through.clone(), absent],
            &[],
        );
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
    /// reached through real directories only. Objective 155: a leased
    /// folder is never walked whole (its bytes `None`), and an unmarked
    /// directory holding a running claim (old, session) is no entry, each of
    /// its other children its own (old/data idle).
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
        claim(
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
        claim(&root.join("session/a"), "rev-e", now - 30, now + 1, &[]);
        fill(&root.join("session/a/data"), 70);
        claim(&root.join("session/b/deep"), "rev-x", 1, now + 1, &[]);
        fill(&root.join("old/data"), 40);
        claim(&root.join("old/c"), "rev-a", 5, now + 1, &[]);
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
                ("old/data".to_string(), 40, Reason::Idle),
            ]
        );
        let claimed: Vec<(String, &str, Option<u64>, u64)> = found
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
                ("old/c".to_string(), "rev-a", None, 0),
                ("ended".to_string(), "rev-b", Some(2000 + ended), 0),
                ("session/a".to_string(), "rev-e", None, 0),
                ("leased".to_string(), "rev-e", None, 300),
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
        let kept_path = root.join("sess/kept");
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
        std::os::unix::fs::symlink(root.join("projects-link"), projects.join("alias"))
            .expect("symlink");
        let roots = [root.clone()];
        let excludes = [kept_path];
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

    fn many(dir: &Path, count: usize) {
        std::fs::create_dir_all(dir).expect("mkdir");
        for n in 0..count {
            std::fs::write(dir.join(format!("f{n}")), b"x").expect("write");
        }
    }

    fn names(found: &super::Walk, root: &Path) -> Vec<(String, u64, Reason)> {
        let mut unowned: Vec<(String, u64, Reason)> = found
            .unowned
            .iter()
            .map(|entry| {
                let name = entry.path.strip_prefix(root).unwrap_or(&entry.path);
                (name.display().to_string(), entry.bytes, entry.reason)
            })
            .collect();
        unowned.sort();
        unowned
    }

    /// Objective 155 (a), (b): a root that fits 60 entries only because a
    /// folder whose lease runs is walked by its build path and a fresh one
    /// stops at its own change; with the lease ended, the same root runs
    /// over the bound.
    #[test]
    fn a_running_lease_and_a_fresh_folder_are_never_walked_whole() {
        let scratch = Scratch::new();
        let root = scratch.0.join("root");
        let now = ms(SystemTime::now());
        let old = SystemTime::now() - Duration::from_hours(48);
        many(&root.join("held/data"), 200);
        fill(&root.join("held/t/target/x"), 3000);
        claim(&root.join("held"), "rev-e", now - 10, now + 3_600_000, &["t/target"]);
        many(&root.join("fresh/data"), 200);
        fill(&root.join("idle/a"), 600);
        age(&root.join("held"), old);
        age(&root.join("fresh/data"), old);
        age(&root.join("idle"), old);
        let runs = |_: u32, _: u64| false;
        let bound = Bound {
            entries: 60,
            time: Duration::from_secs(30),
        };
        let found = walk(std::slice::from_ref(&root), &[], &[], now, &runs, bound)
            .expect("within 60 entries");
        assert_eq!(names(&found, &root), [("idle".to_string(), 600, Reason::Idle)]);
        assert_eq!(found.claimed.len(), 1);
        assert_eq!(found.claimed[0].bytes, None, "never walked whole");
        assert_eq!(found.claimed[0].build_bytes, 3000);
        // The lease ended: walked whole, nobody's, and over the same bound.
        let over = walk(
            std::slice::from_ref(&root),
            &[],
            &[],
            now + 7_200_000,
            &runs,
            bound,
        );
        assert!(
            over.as_ref().is_err_and(|why| why.contains("ran over")),
            "{over:?}"
        );
        let whole = walk(
            std::slice::from_ref(&root),
            &[],
            &[],
            now + 7_200_000,
            &runs,
            Bound::DEFAULT,
        )
        .expect("a walk");
        assert!(
            whole
                .unowned
                .iter()
                .any(|entry| entry.path.ends_with("held") && entry.reason == Reason::LeaseEnded),
            "{whole:?}"
        );
    }

    /// Objective 155 (d): roots /x and /x/ccd, with the inner root excluded
    /// and without: /x/b, /x/ccd/a and /x/ccd/c are each counted once, and
    /// an exclude inside the inner root still holds.
    #[test]
    fn a_root_inside_a_root_is_its_own_and_counted_once() {
        let scratch = Scratch::new();
        let x = scratch.0.join("x");
        let ccd = x.join("ccd");
        fill(&x.join("b/f"), 50);
        fill(&ccd.join("a/f"), 7);
        fill(&ccd.join("c/f"), 11);
        fill(&ccd.join("kept/f"), 13);
        let runs = |_: u32, _: u64| false;
        let far = i64::MAX / 2;
        let roots = [x.clone(), ccd.clone()];
        let want = [
            ("b".to_string(), 50, Reason::Idle),
            ("ccd/a".to_string(), 7, Reason::Idle),
            ("ccd/c".to_string(), 11, Reason::Idle),
        ];
        for excludes in [vec![ccd.clone(), ccd.join("kept")], vec![ccd.join("kept")]] {
            let found = walk(&roots, &excludes, &[], far, &runs, Bound::DEFAULT).expect("a walk");
            assert_eq!(names(&found, &x), want, "excludes {excludes:?}");
            assert!(refused_roots(&roots, &excludes).is_empty());
        }
        // The inner root alone, its exclude held.
        let found = walk(
            std::slice::from_ref(&ccd),
            &[ccd.join("kept")],
            &[],
            far,
            &runs,
            Bound::DEFAULT,
        )
        .expect("a walk");
        assert_eq!(names(&found, &x), want[1..]);
    }

    /// Objective 155 (b), R2: a session folder (unmarked, unclaimed, holding
    /// a running claim one level below) is no entry; each of its other
    /// children is its own: old-tree and loose (a file) idle, gone (a dead
    /// marker) gone, ended (a lease ended) lease ended; fresh is its
    /// maker's; the leased one is listed, never walked whole.
    #[test]
    fn a_session_folder_hides_none_of_its_unclaimed_children() {
        let scratch = Scratch::new();
        let root = scratch.0.join("root");
        let sess = root.join("sess");
        let now = ms(SystemTime::now());
        let old = SystemTime::now() - Duration::from_hours(48);
        fill(&sess.join("o/z"), 500);
        claim(&sess.join("o"), "rev-e", now - 10, now + 3_600_000, &[]);
        fill(&sess.join("old-tree/f"), 1000);
        fill(&sess.join("loose"), 20);
        fill(&sess.join("gone/f"), 30);
        std::fs::write(sess.join("gone").join(MARKER), "42 100\n").expect("marker");
        fill(&sess.join("ended/f"), 40);
        let ended = claim(&sess.join("ended"), "rev-b", now - 20, now - 1, &[]);
        age(&sess, old);
        fill(&sess.join("fresh/f"), 50);
        let runs = |pid: u32, _: u64| pid == 41;
        let found = walk(
            std::slice::from_ref(&root),
            &[],
            &[],
            now,
            &runs,
            Bound::DEFAULT,
        )
        .expect("a walk");
        assert_eq!(
            names(&found, &root),
            [
                ("sess/ended".to_string(), 40 + ended, Reason::LeaseEnded),
                ("sess/gone".to_string(), 30 + 7, Reason::Gone),
                ("sess/loose".to_string(), 20, Reason::Idle),
                ("sess/old-tree".to_string(), 1000, Reason::Idle),
            ]
        );
        let mut claimed: Vec<(String, Option<u64>)> = found
            .claimed
            .iter()
            .map(|folder| {
                let name = folder.path.strip_prefix(&root).expect("in the root");
                (name.display().to_string(), folder.bytes)
            })
            .collect();
        claimed.sort();
        assert_eq!(
            claimed,
            [
                ("sess/ended".to_string(), Some(40 + ended)),
                ("sess/o".to_string(), None),
            ]
        );
    }

    /// Objective 155 (d), R2: an exclude that holds a root refuses that
    /// root, said, and it counts nothing; an exclude equal to the root
    /// refuses nothing.
    #[test]
    fn an_exclude_above_a_root_refuses_it_and_says_so() {
        let scratch = Scratch::new();
        let n = scratch.0.join("n");
        let ccd = n.join("ccd");
        fill(&ccd.join("x/f"), 7);
        let runs = |_: u32, _: u64| false;
        let far = i64::MAX / 2;
        let refused = refused_roots(std::slice::from_ref(&ccd), std::slice::from_ref(&n));
        assert_eq!(
            refused,
            [(
                ccd.clone(),
                format!(
                    "exclude {} hides root {}: a root under an exclude is never read",
                    n.display(),
                    ccd.display()
                )
            )]
        );
        let found = walk(
            std::slice::from_ref(&ccd),
            std::slice::from_ref(&n),
            &[],
            far,
            &runs,
            Bound::DEFAULT,
        )
        .expect("a walk");
        assert!(found.unowned.is_empty(), "{found:?}");
        assert!(refused_roots(std::slice::from_ref(&ccd), std::slice::from_ref(&ccd)).is_empty());
        let found = walk(
            std::slice::from_ref(&ccd),
            std::slice::from_ref(&ccd),
            &[],
            far,
            &runs,
            Bound::DEFAULT,
        )
        .expect("a walk");
        assert_eq!(names(&found, &n), [("ccd/x".to_string(), 7, Reason::Idle)]);
    }
}

//! `/api/fleet`'s `health.storage` (docs/storage.md, "What uses the disk"):
//! what uses the disk, what the transcript archive grows by, how long the
//! disk lasts at that rate, and the fall of Claude Code's own transcripts.
//!
//! The free space of the workspace root's file system is read at each
//! request. The sizes (three walks and the database's figures) are read at
//! most every [`READ_EVERY_MS`] and kept between: the page polls
//! `/api/fleet` every 3 s in every tab, and a walk of the live root takes
//! about 0.3 s. `read_at_ms` says when they were read.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

use crate::storage::{Sizes, archive, archive_path, projects_path, walk, workspaces};

/// How long the sizes are kept before they are read again.
pub(crate) const READ_EVERY_MS: i64 = 600_000;

const DAY_MS: i64 = 86_400_000;

/// The archive's growth is measured from the oldest sample of this window.
const GROWTH_WINDOW_MS: i64 = 7 * DAY_MS;

/// The archive's growth needs a sample at least this old.
const GROWTH_MIN_MS: i64 = DAY_MS;

/// `before` is the count of the newest sample at least this old.
const BEFORE_MS: i64 = 3_600_000;

/// The sizes of the last read, and when.
pub(crate) struct Kept {
    read_at_ms: i64,
    sizes: Value,
}

/// The sizes, read at most every [`READ_EVERY_MS`] and kept between.
static KEPT: Mutex<Option<Kept>> = Mutex::new(None);

/// The kept sizes when they are younger than [`READ_EVERY_MS`] at
/// `now_ms`; else those `read` gives at `now_ms`, kept. The lock is held
/// through a read, so requests at once walk the disk once.
pub(crate) fn kept_or_read(
    kept: &Mutex<Option<Kept>>,
    now_ms: i64,
    read: impl FnOnce(i64) -> Result<Value, String>,
) -> Result<Value, String> {
    let mut kept = kept
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(sizes) = kept
        .as_ref()
        .filter(|kept| (0..READ_EVERY_MS).contains(&now_ms.saturating_sub(kept.read_at_ms)))
    {
        return Ok(sizes.sizes.clone());
    }
    let sizes = read(now_ms)?;
    *kept = Some(Kept {
        read_at_ms: now_ms,
        sizes: sizes.clone(),
    });
    Ok(sizes)
}

/// What the walks found.
pub(crate) struct Walked {
    pub(crate) root: Option<PathBuf>,
    pub(crate) archive: Sizes,
    pub(crate) workspaces: Sizes,
    pub(crate) projects_path: PathBuf,
    pub(crate) projects: Sizes,
}

impl Walked {
    /// Walk the archive and the workspaces of `root` (when there is one)
    /// and Claude Code's projects directory.
    pub(crate) fn read(root: Option<&Path>, projects_path: PathBuf) -> Self {
        Self {
            root: root.map(Path::to_path_buf),
            archive: root.map(archive).unwrap_or_default(),
            workspaces: root.map(workspaces).unwrap_or_default(),
            projects: walk(&projects_path, &[]),
            projects_path,
        }
    }
}

/// The bytes appended to the archive per day: its `bytes` now less the
/// `archive_bytes` of `oldest` (the oldest sample of the last 7 days), over
/// the days between them. `None` without a sample at least a day old.
pub(crate) fn growth(bytes: u64, oldest: &Value, read_at_ms: i64) -> Option<i64> {
    let at = oldest["at_ms"].as_i64()?;
    let then = oldest["archive_bytes"].as_i64()?;
    let elapsed = read_at_ms.saturating_sub(at);
    if elapsed < GROWTH_MIN_MS {
        return None;
    }
    let appended = i128::from(bytes) - i128::from(then);
    i64::try_from(appended * i128::from(DAY_MS) / i128::from(elapsed)).ok()
}

/// How many days `free` bytes last at `growth` bytes a day, rounded down;
/// `None` when either is unknown or the archive does not grow.
pub(crate) fn days_left(free: Option<u64>, growth: Option<i64>) -> Option<u64> {
    let growth = u64::try_from(growth?).ok().filter(|growth| *growth > 0)?;
    Some(free? / growth)
}

/// The kept part of the figures, read at `read_at_ms`: the sizes, the
/// database's, the archive's growth and Claude Code's count before, from
/// `samples` (`{oldest, before}`, as `PostgresStore::host_storage` reads
/// them for this moment).
pub(crate) fn sizes(read_at_ms: i64, walked: &Walked, samples: &Value, database: &Value) -> Value {
    let path = |path: &Path| path.display().to_string();
    let root = walked.root.as_deref();
    json!({
        "read_at_ms": read_at_ms,
        "archive": root.map(|root| json!({
            "path": path(&archive_path(root)),
            "bytes": walked.archive.bytes,
            "files": walked.archive.files,
        })),
        "projects": {
            "path": path(&walked.projects_path),
            "bytes": walked.projects.bytes,
            "files": walked.projects.files,
            "before": samples["before"]["projects_files"].as_u64(),
        },
        "workspaces": root.map(|root| json!({
            "path": path(root),
            "bytes": walked.workspaces.bytes,
        })),
        "database": database.clone(),
        "growth_bytes_per_day": root.and_then(|_| growth(walked.archive.bytes, &samples["oldest"], read_at_ms)),
    })
}

/// The figures with the free space now: `free_bytes` and `days_left`.
pub(crate) fn with_free(mut sizes: Value, free: Option<u64>) -> Value {
    let days = days_left(free, sizes["growth_bytes_per_day"].as_i64());
    sizes["free_bytes"] = json!(free);
    sizes["days_left"] = json!(days);
    sizes
}

/// `health.storage` now: the free space of the workspace root
/// (`SPAWN_WORKSPACE_ROOT`) read now, the sizes kept or read.
pub(crate) fn report(store: &PostgresStore, now_ms: i64) -> Result<Value, String> {
    let root = crate::env_value("SPAWN_WORKSPACE_ROOT").map(PathBuf::from);
    let sizes = kept_or_read(&KEPT, now_ms, |read_at_ms| {
        let walked = Walked::read(root.as_deref(), projects_path());
        let samples = store
            .host_storage(
                read_at_ms.saturating_sub(GROWTH_WINDOW_MS),
                read_at_ms.saturating_sub(BEFORE_MS),
            )
            .map_err(|error| error.to_string())?;
        let database = store.database_size().map_err(|error| error.to_string())?;
        Ok(sizes(read_at_ms, &walked, &samples, &database))
    })?;
    let free = root
        .as_deref()
        .and_then(|root| crate::disk::free_bytes(root).ok());
    Ok(with_free(sizes, free))
}

#[cfg(test)]
mod tests {
    use super::{
        DAY_MS, Kept, READ_EVERY_MS, Walked, days_left, growth, kept_or_read, sizes, with_free,
    };
    use crate::storage::tests::{file, scratch};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn a_second_request_within_ten_minutes_walks_nothing() {
        let kept: Mutex<Option<Kept>> = Mutex::new(None);
        let mut reads = Vec::new();
        let mut ask = |now: i64| {
            kept_or_read(&kept, now, |at| {
                reads.push(at);
                Ok(json!({"read_at_ms": at}))
            })
            .expect("read")
        };
        assert_eq!(ask(1000)["read_at_ms"], 1000);
        assert_eq!(ask(4000)["read_at_ms"], 1000, "kept: 3 s later");
        assert_eq!(ask(1000 + READ_EVERY_MS - 1)["read_at_ms"], 1000);
        assert_eq!(
            ask(1000 + READ_EVERY_MS)["read_at_ms"],
            1000 + READ_EVERY_MS
        );
        // A clock set back reads again rather than keep a read from ahead.
        assert_eq!(ask(500)["read_at_ms"], 500);
        assert_eq!(reads, [1000, 1000 + READ_EVERY_MS, 500]);
        // A failed read keeps nothing and says so.
        let empty: Mutex<Option<Kept>> = Mutex::new(None);
        assert!(kept_or_read(&empty, 0, |_| Err("down".to_string())).is_err());
        assert!(empty.lock().expect("lock").is_none());
    }

    #[test]
    fn the_growth_is_what_was_appended_per_day_since_the_oldest_sample() {
        let now = 100 * DAY_MS;
        let oldest = |ago: i64, bytes: u64| json!({"at_ms": now - ago, "archive_bytes": bytes});
        // 7 MiB appended in 6.5 days.
        let six_and_a_half = 13 * DAY_MS / 2;
        let got = growth(12 * MIB, &oldest(six_and_a_half, 5 * MIB), now).expect("a growth");
        assert_eq!(got, i64::try_from(7 * MIB * 2 / 13).expect("fits"));
        // A sample under a day old, or none, measures nothing.
        assert_eq!(growth(12 * MIB, &oldest(DAY_MS - 1, 5 * MIB), now), None);
        assert_eq!(growth(12 * MIB, &Value::Null, now), None);
        // A shrinking archive grows by less than nothing.
        assert_eq!(
            growth(MIB, &oldest(DAY_MS, 3 * MIB), now),
            Some(-2 * i64::try_from(MIB).expect("fits"))
        );
        assert_eq!(
            days_left(Some(10 * MIB), Some(i64::try_from(3 * MIB).expect("fits"))),
            Some(3)
        );
        assert_eq!(days_left(Some(10 * MIB), Some(0)), None);
        assert_eq!(days_left(Some(10 * MIB), Some(-5)), None);
        assert_eq!(days_left(Some(10 * MIB), None), None);
        assert_eq!(days_left(None, Some(5)), None);
    }

    #[test]
    fn the_figures_carry_the_growth_and_the_count_before_from_the_samples() {
        let root = scratch("figures");
        let projects = root.join("projects-elsewhere");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), 3000);
        file(&root.join(".transcripts/agent_a/a2.jsonl"), 9000);
        file(&root.join(".transcripts-progress/agent_a/a1.json"), 10);
        file(&root.join("agent_c/big.bin"), 2000);
        file(&projects.join("-one/s1.jsonl"), 100);
        file(&projects.join("-two/s2.jsonl"), 200);
        let walked = Walked::read(Some(&root), projects.clone());
        std::fs::remove_dir_all(&root).expect("clean up");
        let now = 50 * DAY_MS;
        let samples = json!({
            "oldest": {"at_ms": now - 2 * DAY_MS, "archive_bytes": 2000, "projects_files": 40},
            "before": {"at_ms": now - 7_200_000, "archive_bytes": 11_000, "projects_files": 40},
        });
        let database = json!({"bytes": 9, "tables": [{"name": "spawn_events", "bytes": 8}]});
        let kept = sizes(now, &walked, &samples, &database);
        let figures = with_free(kept, Some(100_000));
        assert_eq!(
            figures,
            json!({
                "read_at_ms": now,
                "archive": {"path": root.join(".transcripts").display().to_string(),
                            "bytes": 12_000, "files": 2},
                "projects": {"path": projects.display().to_string(), "bytes": 300, "files": 2,
                             "before": 40},
                "workspaces": {"path": root.display().to_string(), "bytes": 2000},
                "database": database,
                "growth_bytes_per_day": 5000,
                "free_bytes": 100_000,
                "days_left": 20,
            })
        );
        // Without samples: no growth, no count before, no days.
        let bare = with_free(
            sizes(
                now,
                &walked,
                &json!({"oldest": null, "before": null}),
                &Value::Null,
            ),
            Some(1),
        );
        assert_eq!(bare["growth_bytes_per_day"], Value::Null);
        assert_eq!(bare["projects"]["before"], Value::Null);
        assert_eq!(bare["days_left"], Value::Null);
    }
}

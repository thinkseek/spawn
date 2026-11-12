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
    /// The session backups' `estimated_bytes` (objective 097); `None`
    /// without a status file that reads with that number.
    pub(crate) session_backups: Option<u64>,
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
            session_backups: crate::storage::session_backup_bytes(
                crate::storage::session_backup_status().as_deref(),
            ),
        }
    }
}

/// The bytes a grower gains per day (objective 065 for the archive, 085
/// for every grower that persists): its `bytes` now less the `field` of
/// `oldest` (the oldest sample of the last 7 days that carries it), over
/// the days between them, rounded down. `None` without such a sample at
/// least a day old.
pub(crate) fn growth_of(bytes: u64, oldest: &Value, field: &str, read_at_ms: i64) -> Option<i64> {
    let at = oldest["at_ms"].as_i64()?;
    let then = oldest[field].as_i64()?;
    let elapsed = read_at_ms.saturating_sub(at);
    if elapsed < GROWTH_MIN_MS {
        return None;
    }
    let appended = i128::from(bytes) - i128::from(then);
    i64::try_from((appended * i128::from(DAY_MS)).div_euclid(i128::from(elapsed))).ok()
}

/// The growth of every grower that persists (objective 085), in bytes a
/// day: the archive's (`archive_bytes`, without a root none), Claude Code's
/// transcripts' (`projects_bytes`) and the database's (`database_bytes`,
/// against `database`, this answer's database bytes), each from the oldest
/// sample of the window that carries it (`carrying`, as
/// `PostgresStore::host_storage` reads it).
pub(crate) fn growth_parts(
    archive: Option<u64>,
    projects: u64,
    database: Option<u64>,
    carrying: &Value,
    read_at_ms: i64,
) -> Value {
    let part = |bytes: Option<u64>, field: &str| {
        bytes.and_then(|bytes| growth_of(bytes, &carrying[field], field, read_at_ms))
    };
    json!({
        "archive": part(archive, "archive_bytes"),
        "projects": part(Some(projects), "projects_bytes"),
        "database": part(database, "database_bytes"),
    })
}

/// The persistent growth's parts, each by the sample field it is measured
/// from: the three of objective 085 and the session backups' (097).
const PARTS: [(&str, &str); 4] = [
    ("archive", "archive_bytes"),
    ("projects", "projects_bytes"),
    ("database", "database_bytes"),
    ("session_backups", "session_backup_bytes"),
];

/// The session backups' growth (objective 097), measured as the other
/// parts are ([`growth_of`]): `bytes` (this answer's `estimated_bytes`, the
/// status file's) less the oldest sample of the window carrying
/// `session_backup_bytes`, over the days between. Null without `bytes` or
/// such a sample a day old.
pub(crate) fn session_backups_part(bytes: Option<u64>, carrying: &Value, read_at_ms: i64) -> Value {
    json!(bytes.and_then(|bytes| growth_of(
        bytes,
        &carrying["session_backup_bytes"],
        "session_backup_bytes",
        read_at_ms
    )))
}

/// The parts left out of the sum because their source cannot be read now
/// (objective 097): the session backups' while no status file reads, or
/// the one that reads holds no `estimated_bytes` number (`session_backups`
/// is that number). A format change excludes the part; it never nulls the
/// whole forecast.
pub(crate) fn growth_excluded(session_backups: Option<u64>) -> Vec<&'static str> {
    if session_backups.is_some() {
        Vec::new()
    } else {
        vec!["session_backups"]
    }
}

/// The counted parts' sum ([`growth_parts`], [`session_backups_part`]),
/// those `excluded` left out; `None` while any counted part is.
pub(crate) fn persistent_growth(parts: &Value, excluded: &[&str]) -> Option<i64> {
    PARTS
        .iter()
        .filter(|(part, _)| !excluded.contains(part))
        .try_fold(0_i64, |sum, (part, _)| sum.checked_add(parts[*part].as_i64()?))
}

/// The moment the archive's growth is first measured (objective 083): the
/// `at_ms` of `oldest` (the oldest sample of the last 7 days carrying the
/// archive's size, the one its growth is measured from, [`growth_parts`])
/// plus [`GROWTH_MIN_MS`]. `None` without a sample.
pub(crate) fn growth_from(oldest: &Value) -> Option<i64> {
    oldest["at_ms"].as_i64()?.checked_add(GROWTH_MIN_MS)
}

/// The moment the persistent growth is first measured (objective 091): the
/// latest of its counted parts' moments ([`growth_from`] of the oldest
/// sample of the window carrying each part's size, `carrying` as
/// [`growth_parts`] reads it), those `excluded` left out. `None` while any
/// counted part has no such sample.
pub(crate) fn persistent_from(carrying: &Value, excluded: &[&str]) -> Option<i64> {
    PARTS
        .iter()
        .filter(|(part, _)| !excluded.contains(part))
        .try_fold(i64::MIN, |latest, (_, field)| {
            Some(latest.max(growth_from(&carrying[*field])?))
        })
}

/// How many days `free` bytes last at `growth` bytes a day, rounded down;
/// `None` when either is unknown or what persists does not grow.
pub(crate) fn days_left(free: Option<u64>, growth: Option<i64>) -> Option<u64> {
    let growth = u64::try_from(growth?).ok().filter(|growth| *growth > 0)?;
    Some(free? / growth)
}

/// The test databases' entries (objective 074): `{role, name, bytes,
/// schemas}` for each `(role, reading)`, in the order given (the gates'
/// first); one that could not be read has its name, bytes and schemas
/// null, and says why (`error`).
pub(crate) fn test_databases(readings: Vec<(&'static str, Result<Value, String>)>) -> Value {
    readings
        .into_iter()
        .map(|(role, reading)| match reading {
            Ok(reading) => json!({
                "role": role, "name": reading["name"], "bytes": reading["bytes"],
                "schemas": reading["schemas"],
            }),
            Err(error) => json!({
                "role": role, "name": null, "bytes": null, "schemas": null,
                "error": crate::alerts::scrub(&error),
            }),
        })
        .collect()
}

/// The kept part of the figures, read at `read_at_ms`: the sizes, the
/// database's, the test databases' ([`test_databases`]), the archive's
/// growth, the growth of every grower that persists ([`growth_parts`]) and
/// their sum, and Claude Code's count before, from `samples` (`{carrying,
/// before}`, as `PostgresStore::host_storage` reads them for this moment).
pub(crate) fn sizes(
    read_at_ms: i64,
    walked: &Walked,
    samples: &Value,
    database: &Value,
    tests: &Value,
) -> Value {
    let path = |path: &Path| path.display().to_string();
    let root = walked.root.as_deref();
    let mut parts = growth_parts(
        root.map(|_| walked.archive.bytes),
        walked.projects.bytes,
        database["bytes"].as_u64(),
        &samples["carrying"],
        read_at_ms,
    );
    parts["session_backups"] =
        session_backups_part(walked.session_backups, &samples["carrying"], read_at_ms);
    let excluded = growth_excluded(walked.session_backups);
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
        "test_databases": tests.clone(),
        "growth_bytes_per_day": parts["archive"],
        "growth_from_ms": root.and_then(|_| growth_from(&samples["carrying"]["archive_bytes"])),
        "persistent_growth_bytes_per_day": persistent_growth(&parts, &excluded),
        "persistent_from_ms": root.and_then(|_| persistent_from(&samples["carrying"], &excluded)),
        "growth_parts": parts,
        "growth_excluded": excluded,
    })
}

/// The figures with the free space now: `free_bytes` and `days_left` (at
/// the persistent growth, objective 085).
pub(crate) fn with_free(mut sizes: Value, free: Option<u64>) -> Value {
    let days = days_left(free, sizes["persistent_growth_bytes_per_day"].as_i64());
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
        // The gates' test database and the engines', each when its URL is
        // set (objective 074).
        let tests = test_databases(
            crate::alerts::test_database_urls()
                .into_iter()
                .map(|(role, url)| {
                    (
                        role,
                        spawn_postgres::test_database_size(&url.0)
                            .map_err(|error| error.to_string()),
                    )
                })
                .collect(),
        );
        Ok(sizes(read_at_ms, &walked, &samples, &database, &tests))
    })?;
    let free = root
        .as_deref()
        .and_then(|root| crate::disk::free_bytes(root).ok());
    Ok(with_free(sizes, free))
}

#[cfg(test)]
mod tests {
    use super::{
        DAY_MS, Kept, READ_EVERY_MS, Walked, days_left, growth_excluded, growth_from, growth_of,
        growth_parts, kept_or_read, persistent_from, persistent_growth, session_backups_part,
        sizes, test_databases, with_free,
    };
    use crate::storage::tests::{file, scratch};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    const MIB: u64 = 1024 * 1024;

    /// The session backups left out, as without a status file: objective
    /// 085's three parts.
    const BACKUPS_LEFT_OUT: &[&str] = &["session_backups"];

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
        let growth = |bytes: u64, oldest: &Value| growth_of(bytes, oldest, "archive_bytes", now);
        // 7 MiB appended in 6.5 days.
        let six_and_a_half = 13 * DAY_MS / 2;
        let got = growth(12 * MIB, &oldest(six_and_a_half, 5 * MIB)).expect("a growth");
        assert_eq!(got, i64::try_from(7 * MIB * 2 / 13).expect("fits"));
        // A sample under a day old, or none, measures nothing.
        assert_eq!(growth(12 * MIB, &oldest(DAY_MS - 1, 5 * MIB)), None);
        assert_eq!(growth(12 * MIB, &Value::Null), None);
        // A shrinking archive grows by less than nothing.
        assert_eq!(
            growth(MIB, &oldest(DAY_MS, 3 * MIB)),
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
        let projects = scratch("figures_projects");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), 3000);
        file(&root.join(".transcripts/agent_a/a2.jsonl"), 9000);
        file(&root.join(".transcripts-progress/agent_a/a1.json"), 10);
        file(&root.join("agent_c/big.bin"), 2000);
        file(&projects.join("-one/s1.jsonl"), 100);
        file(&projects.join("-two/s2.jsonl"), 200);
        let walked = Walked::read(Some(&root), projects.clone());
        std::fs::remove_dir_all(&root).expect("clean up");
        std::fs::remove_dir_all(&projects).expect("clean up");
        let now = 50 * DAY_MS;
        let oldest = json!({"at_ms": now - 2 * DAY_MS, "archive_bytes": 2000,
                            "projects_bytes": 100, "projects_files": 40});
        let samples = json!({
            "oldest": oldest,
            "carrying": {
                "archive_bytes": oldest, "projects_bytes": oldest,
                "database_bytes": {"at_ms": now - DAY_MS, "archive_bytes": 9000,
                                   "projects_bytes": 200, "database_bytes": 6000},
            },
            "before": {"at_ms": now - 7_200_000, "archive_bytes": 11_000, "projects_files": 40},
        });
        let database = json!({"bytes": 9000, "tables": [{"name": "spawn_events", "bytes": 8}]});
        let tests = json!([{"role": "test", "name": "spawn_test", "bytes": 7, "schemas": 0}]);
        let kept = sizes(now, &walked, &samples, &database, &tests);
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
                "test_databases": tests,
                "growth_bytes_per_day": 5000,
                "growth_from_ms": now - DAY_MS,
                "growth_parts": {"archive": 5000, "projects": 100, "database": 3000},
                "persistent_growth_bytes_per_day": 8100,
                "persistent_from_ms": now,
                "free_bytes": 100_000,
                "days_left": 12,
            })
        );
        // Without samples: no growth, no count before, no days.
        let bare = with_free(
            sizes(
                now,
                &walked,
                &json!({"oldest": null, "before": null}),
                &Value::Null,
                &json!([]),
            ),
            Some(1),
        );
        assert_eq!(bare["growth_bytes_per_day"], Value::Null);
        assert_eq!(bare["persistent_growth_bytes_per_day"], Value::Null);
        assert_eq!(bare["projects"]["before"], Value::Null);
        assert_eq!(bare["days_left"], Value::Null);
    }

    /// Objective 085: each grower that persists is measured from the oldest
    /// sample carrying its size; the sum is null while a part is, and a part
    /// that shrinks counts against the others.
    #[test]
    fn the_persistent_growth_is_the_sum_of_the_parts_each_from_its_own_sample() {
        let now = 100 * DAY_MS;
        let mib = i64::try_from(MIB).expect("fits");
        let old_shape = json!({"at_ms": now - 3 * DAY_MS, "archive_bytes": 9 * MIB,
                               "projects_bytes": MIB / 4});
        let with_database = json!({"at_ms": now - 2 * DAY_MS, "archive_bytes": 10 * MIB,
                                   "projects_bytes": MIB / 2, "database_bytes": 100 * MIB});
        let carrying = json!({"archive_bytes": old_shape, "projects_bytes": old_shape,
                              "database_bytes": with_database});
        let parts = growth_parts(Some(12 * MIB), MIB, Some(108 * MIB), &carrying, now);
        assert_eq!(
            parts,
            json!({"archive": mib, "projects": mib / 4, "database": 4 * mib})
        );
        assert_eq!(persistent_growth(&parts, BACKUPS_LEFT_OUT), Some(mib + mib / 4 + 4 * mib));
        // Without a sample carrying the database's size (only the old
        // shape in the window): its part is null, and so is the sum.
        let young = json!({"archive_bytes": old_shape, "projects_bytes": old_shape,
                           "database_bytes": null});
        let parts = growth_parts(Some(12 * MIB), MIB, Some(108 * MIB), &young, now);
        assert_eq!(parts["database"], Value::Null);
        assert_eq!(parts["archive"], mib);
        assert_eq!(persistent_growth(&parts, BACKUPS_LEFT_OUT), None);
        // No database bytes in this answer, or no root: that part is null.
        let parts = growth_parts(None, MIB, None, &carrying, now);
        assert_eq!(parts["archive"], Value::Null);
        assert_eq!(parts["database"], Value::Null);
        assert_eq!(parts["projects"], mib / 4);
        // A shrinking database (a vacuum) counts against the rest.
        let parts = growth_parts(Some(12 * MIB), MIB, Some(96 * MIB), &carrying, now);
        assert_eq!(parts["database"], -2 * mib);
        assert_eq!(persistent_growth(&parts, BACKUPS_LEFT_OUT), Some(mib + mib / 4 - 2 * mib));
        // Days left over a sum at or below 0: none.
        let figures = with_free(
            json!({"persistent_growth_bytes_per_day": persistent_growth(&parts, BACKUPS_LEFT_OUT)}),
            Some(10 * MIB),
        );
        assert_eq!(figures["days_left"], Value::Null);
        let figures = with_free(
            json!({"persistent_growth_bytes_per_day": 3 * mib}),
            Some(10 * MIB),
        );
        assert_eq!(figures["days_left"], 3);
    }

    /// Objective 097: the session backups' growth is a fourth part,
    /// measured as the others are, and counted in the sum and days left;
    /// with no status file, or one without `estimated_bytes`, it is null,
    /// listed as left out, and the other three still give the sum.
    #[test]
    fn the_session_backups_are_a_part_or_are_said_to_be_left_out() {
        let now = 100 * DAY_MS;
        let mib = i64::try_from(MIB).expect("fits");
        let old_shape = json!({"at_ms": now - 3 * DAY_MS, "archive_bytes": 9 * MIB,
                               "projects_bytes": MIB / 4});
        let with_backups = json!({"at_ms": now - 2 * DAY_MS, "archive_bytes": 10 * MIB,
                                  "projects_bytes": MIB / 2, "database_bytes": 100 * MIB,
                                  "session_backup_bytes": 36 * MIB});
        let samples = json!({
            "oldest": old_shape,
            "carrying": {"archive_bytes": old_shape, "projects_bytes": old_shape,
                         "database_bytes": with_backups, "session_backup_bytes": with_backups},
            "before": null,
        });
        assert_eq!(
            session_backups_part(Some(40 * MIB), &samples["carrying"], now),
            json!(2 * mib),
            "4 MiB over 2 days"
        );
        let root = scratch("session_backups");
        let projects = scratch("session_backups_projects");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), usize::try_from(12 * MIB).expect("fits"));
        file(&projects.join("p/s.jsonl"), usize::try_from(MIB).expect("fits"));
        let status = root.join("status.json");
        let without = root.join("without.json");
        std::fs::write(&status, r#"{"at": 1, "estimated_bytes": 41943040}"#).expect("status");
        std::fs::write(&without, r#"{"at": 1, "accounts": []}"#).expect("status");
        let read = |path: Option<&std::path::Path>| Walked {
            session_backups: crate::storage::session_backup_bytes(path),
            ..Walked::read(Some(&root), projects.clone())
        };
        let counted = read(Some(&status));
        let none = read(None);
        let absent = read(Some(&root.join("absent.json")));
        let unnumbered = read(Some(&without));
        std::fs::remove_dir_all(&root).expect("clean up");
        std::fs::remove_dir_all(&projects).expect("clean up");
        let database = json!({"bytes": 108 * MIB});

        let figures = with_free(sizes(now, &counted, &samples, &database, &json!([])), Some(70 * MIB));
        assert_eq!(
            figures["growth_parts"],
            json!({"archive": mib, "projects": mib / 4, "database": 4 * mib,
                   "session_backups": 2 * mib})
        );
        assert_eq!(figures["growth_excluded"], json!([]));
        let four = mib + mib / 4 + 4 * mib + 2 * mib;
        assert_eq!(figures["persistent_growth_bytes_per_day"], four);
        assert_eq!(figures["days_left"], 70 * mib / four);

        for (walked, why) in [(&none, "no file named"), (&absent, "no file there"), (&unnumbered, "no number")] {
            let figures = with_free(sizes(now, walked, &samples, &database, &json!([])), Some(70 * MIB));
            assert_eq!(figures["growth_parts"]["session_backups"], Value::Null, "{why}");
            assert_eq!(figures["growth_excluded"], json!(["session_backups"]), "{why}");
            let three = mib + mib / 4 + 4 * mib;
            assert_eq!(figures["persistent_growth_bytes_per_day"], three, "{why}: the other three");
            assert_eq!(figures["days_left"], 70 * mib / three, "{why}");
        }
        assert_eq!(growth_excluded(Some(0)), Vec::<&str>::new());

        // Counted, but no sample a day old carries it: the part is null, and
        // so is the sum.
        let young = json!({"at_ms": now - 3_600_000, "session_backup_bytes": 39 * MIB});
        let mut fresh = samples.clone();
        fresh["carrying"]["session_backup_bytes"] = young;
        let figures = sizes(now, &counted, &fresh, &database, &json!([]));
        assert_eq!(figures["growth_parts"]["session_backups"], Value::Null);
        assert_eq!(figures["growth_excluded"], json!([]));
        assert_eq!(figures["persistent_growth_bytes_per_day"], Value::Null);
        assert_eq!(
            persistent_from(&fresh["carrying"], &[]),
            Some(now - 3_600_000 + DAY_MS),
            "measured from the backups' first sample plus a day"
        );
    }

    /// Objective 083: while the growth cannot be measured yet, the figures
    /// say from when it will be: the oldest sample plus a day; null with no
    /// sample, or no workspace root.
    #[test]
    fn the_figures_say_from_when_the_growth_is_measured() {
        let root = scratch("growth_from");
        let projects = scratch("growth_from_projects");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), 3000);
        let walked = Walked::read(Some(&root), projects.clone());
        let rootless = Walked::read(None, projects.clone());
        std::fs::remove_dir_all(&root).expect("clean up");
        std::fs::remove_dir_all(&projects).expect("clean up");
        let now = 50 * DAY_MS;
        let ten_hours = 36_000_000;
        let oldest = json!({"at_ms": now - ten_hours, "archive_bytes": 1000,
                            "projects_bytes": 0, "projects_files": 0});
        let samples = json!({
            "oldest": oldest,
            "carrying": {"archive_bytes": oldest, "projects_bytes": oldest, "database_bytes": null},
            "before": {"at_ms": now - 7_200_000, "archive_bytes": 2000, "projects_files": 0},
        });
        assert_eq!(growth_from(&oldest), Some(now - ten_hours + DAY_MS));
        assert_eq!(growth_from(&Value::Null), None);
        let pending = sizes(now, &walked, &samples, &Value::Null, &json!([]));
        assert_eq!(pending["growth_bytes_per_day"], Value::Null);
        assert_eq!(pending["growth_from_ms"], json!(now - ten_hours + DAY_MS));
        let none = json!({"oldest": null, "carrying": null, "before": null});
        let bare = sizes(now, &walked, &none, &Value::Null, &json!([]));
        assert_eq!(bare["growth_from_ms"], Value::Null);
        let unrooted = sizes(now, &rootless, &samples, &Value::Null, &json!([]));
        assert_eq!(unrooted["growth_from_ms"], Value::Null);
    }

    /// Objective 091: the persistent growth is first measured at the latest
    /// of its parts' moments (each part's oldest sample plus a day); null
    /// while a part has no sample, or without a workspace root.
    #[test]
    fn the_figures_say_from_when_the_persistent_growth_is_measured() {
        let root = scratch("persistent_from");
        let projects = scratch("persistent_from_projects");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), 3000);
        let walked = Walked::read(Some(&root), projects.clone());
        let rootless = Walked::read(None, projects.clone());
        std::fs::remove_dir_all(&root).expect("clean up");
        std::fs::remove_dir_all(&projects).expect("clean up");
        let now = 50 * DAY_MS;
        let old = json!({"at_ms": now - 30 * 3_600_000, "archive_bytes": 1000,
                         "projects_bytes": 0, "projects_files": 0});
        let young = json!({"at_ms": now - 10 * 3_600_000, "archive_bytes": 2000,
                           "projects_bytes": 0, "projects_files": 0, "database_bytes": 500});
        // A part without a sample: none.
        let partial = json!({"archive_bytes": old, "projects_bytes": old, "database_bytes": null});
        assert_eq!(persistent_from(&partial, BACKUPS_LEFT_OUT), None);
        assert_eq!(persistent_from(&Value::Null, BACKUPS_LEFT_OUT), None);
        // All three: the latest of them.
        let carrying =
            json!({"archive_bytes": old, "projects_bytes": old, "database_bytes": young});
        assert_eq!(
            persistent_from(&carrying, BACKUPS_LEFT_OUT),
            Some(now - 10 * 3_600_000 + DAY_MS)
        );
        let first = json!({"archive_bytes": young, "projects_bytes": old, "database_bytes": old});
        assert_eq!(persistent_from(&first, BACKUPS_LEFT_OUT), Some(now - 10 * 3_600_000 + DAY_MS));
        let samples = json!({"oldest": old, "carrying": carrying, "before": null});
        let pending = with_free(
            sizes(now, &walked, &samples, &json!({"bytes": 900}), &json!([])),
            Some(1),
        );
        assert_eq!(pending["persistent_growth_bytes_per_day"], Value::Null);
        assert_eq!(pending["days_left"], Value::Null);
        assert_eq!(
            pending["persistent_from_ms"],
            json!(now - 10 * 3_600_000 + DAY_MS)
        );
        let samples = json!({"oldest": old, "carrying": partial, "before": null});
        let missing = sizes(now, &walked, &samples, &Value::Null, &json!([]));
        assert_eq!(missing["persistent_from_ms"], Value::Null);
        let samples = json!({"oldest": old, "carrying": carrying, "before": null});
        let unrooted = sizes(now, &rootless, &samples, &Value::Null, &json!([]));
        assert_eq!(unrooted["persistent_from_ms"], Value::Null);
    }

    /// Objective 074: the test databases, the gates' first, each as read;
    /// one that cannot be read keeps its role and says why, without its URL.
    #[test]
    fn the_test_databases_are_listed_in_order_each_as_read() {
        let listed = test_databases(vec![
            (
                "test",
                Ok(json!({"name": "spawn_test", "bytes": 9 * MIB, "schemas": 0})),
            ),
            (
                "fleet_test",
                Ok(json!({"name": "spawn_fleet_test", "bytes": 2765 * MIB, "schemas": 2533})),
            ),
        ]);
        assert_eq!(
            listed,
            json!([
                {"role": "test", "name": "spawn_test", "bytes": 9 * MIB, "schemas": 0},
                {"role": "fleet_test", "name": "spawn_fleet_test", "bytes": 2765 * MIB,
                 "schemas": 2533},
            ])
        );
        let down = test_databases(vec![(
            "fleet_test",
            Err("error connecting to server: Connection refused".to_string()),
        )]);
        assert_eq!(down[0]["role"], "fleet_test");
        assert_eq!(down[0]["schemas"], Value::Null);
        assert_eq!(
            down[0]["error"],
            "error connecting to server: Connection refused"
        );
        assert_eq!(test_databases(Vec::new()), json!([]));
    }
}

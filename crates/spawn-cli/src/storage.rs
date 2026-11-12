//! What uses the disk (docs/storage.md, "What uses the disk").
//!
//! The transcript archive (`<root>/.transcripts`) is kept whole, by the
//! owner's ruling, so it is the one steady grower; Claude Code's own
//! transcripts (its projects directory, as [`crate::runtime`] resolves it)
//! are what the archive is copied from, and a fall in their count is a
//! loss. The loop samples both as a `host.storage` record at its start and
//! then whenever its latest sample is an hour old ([`Sampler`]), with the
//! live database's size: the page's storage figures (`/api/fleet`'s
//! `health.storage`) measure the growth of the archive, of Claude Code's
//! transcripts and of the database, and the fall of Claude Code's files,
//! against those samples.
//!
//! A size is the sum of the apparent sizes of the regular files under a
//! directory ([`walk`]): symbolic links are not followed, and a directory
//! that cannot be read is skipped, never an error.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

/// How old the loop's latest sample is when it takes another.
pub(crate) const SAMPLE_EVERY_MS: i64 = 3_600_000;

/// How soon a sample that could not be recorded is tried again.
const RETRY_MS: i64 = 60_000;

/// The archive's directory under the workspace root.
pub(crate) const ARCHIVE: &str = ".transcripts";

/// The archive's progress directory under the workspace root: neither the
/// archive nor a workspace.
pub(crate) const PROGRESS: &str = ".transcripts-progress";

/// The regular files under a directory: their apparent sizes summed, and
/// how many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Sizes {
    pub(crate) bytes: u64,
    pub(crate) files: u64,
}

/// The regular files under `path`, leaving out the directories named in
/// `skip` (full paths) and everything under them. A missing `path` is
/// empty; a directory that cannot be read is skipped.
pub(crate) fn walk(path: &Path, skip: &[PathBuf]) -> Sizes {
    let mut sizes = Sizes::default();
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let child = entry.path();
                if !skip.contains(&child) {
                    pending.push(child);
                }
            } else if kind.is_file()
                && let Ok(metadata) = entry.metadata()
            {
                sizes.bytes = sizes.bytes.saturating_add(metadata.len());
                sizes.files += 1;
            }
        }
    }
    sizes
}

/// The transcript archive of `root`.
pub(crate) fn archive_path(root: &Path) -> PathBuf {
    root.join(ARCHIVE)
}

/// The archive's sizes.
pub(crate) fn archive(root: &Path) -> Sizes {
    walk(&archive_path(root), &[])
}

/// The workspaces: everything under `root` outside the archive and its
/// progress directory.
pub(crate) fn workspaces(root: &Path) -> Sizes {
    walk(root, &[root.join(ARCHIVE), root.join(PROGRESS)])
}

/// Claude Code's projects directory, as the Bridge reads it
/// (`SPAWN_CLAUDE_PROJECTS`, else `$CLAUDE_CONFIG_DIR/projects`, else
/// `$HOME/.claude/projects`).
pub(crate) fn projects_path() -> PathBuf {
    PathBuf::from(crate::runtime::claude_projects())
}

/// The variable naming the session backups' status file (objective 097),
/// as deploy/session-backup.sh writes it; no default.
pub(crate) const SESSION_BACKUP_STATUS: &str = "SPAWN_SESSION_BACKUP_STATUS";

/// The session backups' status file `SPAWN_SESSION_BACKUP_STATUS` names;
/// `None` when it names none.
pub(crate) fn session_backup_status() -> Option<PathBuf> {
    crate::env_value(SESSION_BACKUP_STATUS)
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
}

/// What the kept session backup versions really cost, reflinks counted
/// once (objective 097): the status file's `estimated_bytes` (the sum of
/// each kept version's `new_bytes`). `None` without a file, one that cannot
/// be read, or one without that number.
pub(crate) fn session_backup_bytes(status: Option<&Path>) -> Option<u64> {
    let text = std::fs::read_to_string(status?).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["estimated_bytes"].as_u64()
}

/// A `host.storage` record of these sizes, with the live database's
/// (`database_bytes`, objective 085) and the session backups'
/// (`session_backup_bytes`, objective 097) when each could be read.
pub(crate) fn record(
    archive: Sizes,
    projects: Sizes,
    database: Option<u64>,
    session_backups: Option<u64>,
) -> Value {
    let mut record = json!({
        "type": "host.storage",
        "archive_bytes": archive.bytes,
        "archive_files": archive.files,
        "projects_bytes": projects.bytes,
        "projects_files": projects.files,
    });
    if let Some(database) = database {
        record["database_bytes"] = json!(database);
    }
    if let Some(session_backups) = session_backups {
        record["session_backup_bytes"] = json!(session_backups);
    }
    record
}

/// The live database's size, as `health.storage.database` reads it
/// (`pg_database_size`); `None` when it cannot be read.
pub(crate) fn database_bytes(store: &PostgresStore) -> Option<u64> {
    store.database_size().ok()?["bytes"].as_u64()
}

/// The loop's samples: one at its start, then one whenever its latest is
/// [`SAMPLE_EVERY_MS`] old.
pub(crate) struct Sampler {
    root: Option<PathBuf>,
    projects: PathBuf,
    /// The session backups' status file (objective 097); `None`: none read.
    status: Option<PathBuf>,
    /// When this loop's latest sample was recorded; `None` before its first.
    last_ms: Option<i64>,
}

impl Sampler {
    pub(crate) fn new(root: Option<&Path>) -> Self {
        Self::with(root, projects_path()).with_status(session_backup_status())
    }

    pub(crate) fn with(root: Option<&Path>, projects: PathBuf) -> Self {
        Self {
            root: root.map(Path::to_path_buf),
            projects,
            status: None,
            last_ms: None,
        }
    }

    /// The sampler reading the session backups' status file `status`.
    pub(crate) fn with_status(mut self, status: Option<PathBuf>) -> Self {
        self.status = status;
        self
    }

    /// Whether a sample is due at `now_ms`.
    pub(crate) fn due(&self, now_ms: i64) -> bool {
        self.last_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= SAMPLE_EVERY_MS)
    }

    /// The sample of the disk now, with the database's size `database`
    /// and the session backups' `estimated_bytes` (each left out when it
    /// could not be read).
    pub(crate) fn measure(&self, database: Option<u64>) -> Value {
        let archive = self.root.as_deref().map(archive).unwrap_or_default();
        record(
            archive,
            walk(&self.projects, &[]),
            database,
            session_backup_bytes(self.status.as_deref()),
        )
    }

    /// When due, measure (the database's size through `database`) and
    /// write a sample through `write`; returns the line for the loop's log.
    /// One that could not be written is tried again in a minute.
    pub(crate) fn check_with(
        &mut self,
        now_ms: i64,
        database: impl FnOnce() -> Option<u64>,
        write: impl FnOnce(&Value) -> Result<i64, String>,
    ) -> Result<Option<String>, String> {
        if !self.due(now_ms) {
            return Ok(None);
        }
        let sample = self.measure(database());
        if let Err(error) = write(&sample) {
            self.last_ms = Some(now_ms.saturating_sub(SAMPLE_EVERY_MS - RETRY_MS));
            return Err(format!("storage sample: {error}"));
        }
        self.last_ms = Some(now_ms);
        Ok(Some(format!(
            "storage: the archive holds {} in {} files, Claude Code's projects {} in {} files",
            crate::disk::gib(sample["archive_bytes"].as_u64().unwrap_or_default()),
            sample["archive_files"],
            crate::disk::gib(sample["projects_bytes"].as_u64().unwrap_or_default()),
            sample["projects_files"],
        )))
    }

    /// [`Sampler::check_with`] through the store.
    pub(crate) fn check(
        &mut self,
        store: &PostgresStore,
        now_ms: i64,
    ) -> Result<Option<String>, String> {
        self.check_with(
            now_ms,
            || database_bytes(store),
            |sample| store.host_record(sample).map_err(|error| error.to_string()),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{SAMPLE_EVERY_MS, Sampler, Sizes, archive, walk, workspaces};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    /// A fresh scratch directory of the test's own.
    pub(crate) fn scratch(case: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "spawn-storage-{}-{nanos:x}-{case}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a scratch directory");
        path
    }

    pub(crate) fn file(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("its directory");
        std::fs::write(path, vec![0_u8; bytes]).expect("write");
    }

    #[test]
    fn the_workspaces_leave_out_the_archive_and_its_progress() {
        let root = scratch("walk");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), 3000);
        file(&root.join(".transcripts/agent_b/b1.jsonl"), 5000);
        file(&root.join(".transcripts-progress/agent_a/x.json"), 70);
        file(&root.join("agent_c/src/big.bin"), 2000);
        file(&root.join("agent_c/README"), 10);
        // A link is not followed, nor counted.
        std::os::unix::fs::symlink(root.join(".transcripts"), root.join("agent_c/link"))
            .expect("a link");
        assert_eq!(
            archive(&root),
            Sizes {
                bytes: 8000,
                files: 2
            }
        );
        assert_eq!(
            workspaces(&root),
            Sizes {
                bytes: 2010,
                files: 2
            }
        );
        assert_eq!(walk(&root.join("missing"), &[]), Sizes::default());
        std::fs::remove_dir_all(&root).expect("clean up");
    }

    #[test]
    fn an_unreadable_directory_is_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("unreadable");
        file(&root.join("open/a"), 100);
        file(&root.join("shut/b"), 400);
        let shut = root.join("shut");
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let readable = std::fs::read_dir(&shut).is_ok();
        let sizes = walk(&root, &[]);
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::remove_dir_all(&root).expect("clean up");
        // Root reads any directory: then the walk counts it, and nothing fails.
        let want = if readable {
            Sizes {
                bytes: 500,
                files: 2,
            }
        } else {
            Sizes {
                bytes: 100,
                files: 1,
            }
        };
        assert_eq!(sizes, want);
    }

    #[test]
    fn the_loop_samples_at_its_start_and_then_hourly() {
        let root = scratch("sampler");
        let projects = root.join("projects");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), 300);
        file(&projects.join("-one/s1.jsonl"), 20);
        file(&projects.join("-two/s2.jsonl"), 30);
        let mut sampler = Sampler::with(Some(&root), projects);
        let mut written = Vec::new();
        let mut at = |sampler: &mut Sampler, now: i64| {
            sampler
                .check_with(
                    now,
                    || Some(7000),
                    |sample| {
                        written.push((now, sample.clone()));
                        Ok(1)
                    },
                )
                .expect("checked")
                .is_some()
        };
        let start = 1_000_000;
        assert!(at(&mut sampler, start), "one at its start");
        assert!(!at(&mut sampler, start + 1), "none within the hour");
        assert!(
            !at(&mut sampler, start + SAMPLE_EVERY_MS - 1),
            "none within the hour"
        );
        assert!(at(&mut sampler, start + SAMPLE_EVERY_MS), "one after it");
        assert!(!at(&mut sampler, start + SAMPLE_EVERY_MS + 1000));
        std::fs::remove_dir_all(&root).expect("clean up");
        assert_eq!(
            written.iter().map(|(now, _)| *now).collect::<Vec<_>>(),
            [start, start + SAMPLE_EVERY_MS]
        );
        assert_eq!(
            written[0].1,
            json!({"type": "host.storage", "archive_bytes": 300, "archive_files": 1,
                   "projects_bytes": 50, "projects_files": 2, "database_bytes": 7000})
        );
    }

    /// Objective 085: the sample carries the live database's size; one
    /// whose size cannot be read leaves the field out and is still written.
    #[test]
    fn the_sample_carries_the_database_size_when_it_can_be_read() {
        let root = scratch("sampler_database");
        let projects = root.join("projects");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), 100);
        let mut written = Vec::new();
        let mut sampler = Sampler::with(Some(&root), projects.clone());
        sampler
            .check_with(
                0,
                || Some(8 * 1024 * 1024),
                |sample| {
                    written.push(sample.clone());
                    Ok(1)
                },
            )
            .expect("written");
        let mut unread = Sampler::with(Some(&root), projects);
        unread
            .check_with(
                0,
                || None,
                |sample| {
                    written.push(sample.clone());
                    Ok(2)
                },
            )
            .expect("written without the database's size");
        std::fs::remove_dir_all(&root).expect("clean up");
        assert_eq!(written.len(), 2);
        assert_eq!(written[0]["database_bytes"], 8 * 1024 * 1024);
        assert_eq!(written[0]["archive_bytes"], 100);
        assert!(written[1].get("database_bytes").is_none(), "{}", written[1]);
        assert_eq!(written[1]["archive_bytes"], 100);
    }

    #[test]
    fn a_sample_that_could_not_be_written_is_tried_again_in_a_minute() {
        let root = scratch("sampler_retry");
        let mut sampler = Sampler::with(Some(&root), root.join("projects"));
        assert!(
            sampler
                .check_with(0, || None, |_| Err("down".to_string()))
                .is_err()
        );
        assert!(!sampler.due(59_999));
        assert!(sampler.due(60_000));
        std::fs::remove_dir_all(&root).expect("clean up");
    }
}

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

/// Opt-in shared host build caches under the workspace root (`#105`):
/// tracked independently via [`shared_caches_with`] and excluded from single
/// workspace disk totals in [`workspaces`].
pub(crate) const SHARED_CACHES: &str = ".caches";

/// Standard Cache Directory Tagging Specification signature (matching Cargo's
/// `CACHEDIR.TAG`).
const CACHEDIR_TAG_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// Maximum entries scanned across all workspaces by [`workspace_breakdown`] to
/// keep `/api/fleet` `health.storage` snapshots bounded.
pub(crate) const WORKSPACE_BREAKDOWN_MAX_ENTRIES: usize = 200_000;

/// Maximum per-workspace breakdown entries returned inline in
/// [`WorkspaceBreakdown::workspaces`] (top consumers by `total_bytes`).
pub(crate) const WORKSPACE_BREAKDOWN_TOP_LIMIT: usize = 32;

const BUILD_CACHE_DIR_NAMES: &[&str] = &[
    "target",
    ".zig-cache",
    "zig-out",
    "node_modules",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".cache",
    "dist",
    "build",
];

/// Sizes of one shared host build cache directory (`#105`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SharedCacheSizes {
    pub(crate) path: PathBuf,
    pub(crate) sizes: Sizes,
    pub(crate) enabled: bool,
}

/// Per-workspace classified disk usage (`repo_bytes` / `git_bytes` /
/// `build_cache_bytes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceDiskUsage {
    pub(crate) name: String,
    pub(crate) total_bytes: u64,
    pub(crate) repo_bytes: u64,
    pub(crate) git_bytes: u64,
    pub(crate) build_cache_bytes: u64,
    pub(crate) files: u64,
    pub(crate) truncated: bool,
}

/// Bounded summary and top consumers across non-dot workspaces under a root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceBreakdown {
    pub(crate) workspace_count: usize,
    pub(crate) total_bytes: u64,
    pub(crate) repo_bytes: u64,
    pub(crate) git_bytes: u64,
    pub(crate) build_cache_bytes: u64,
    pub(crate) files: u64,
    pub(crate) truncated: bool,
    pub(crate) workspaces: Vec<WorkspaceDiskUsage>,
}

/// Whether `dir` holds a `CACHEDIR.TAG` regular file (not a symlink or FIFO)
/// starting with [`CACHEDIR_TAG_SIGNATURE`].
pub(crate) fn is_cachedir_tagged(dir: &Path) -> bool {
    use rustix::fs::{FileType, Mode, OFlags};
    let Ok(tag) = rustix::fs::open(
        dir.join("CACHEDIR.TAG"),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return false;
    };
    if !rustix::fs::fstat(&tag)
        .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile)
    {
        return false;
    }
    let mut head = [0_u8; CACHEDIR_TAG_SIGNATURE.len()];
    let mut got = 0;
    while got < head.len() {
        match rustix::io::read(&tag, &mut head[got..]) {
            Ok(0) | Err(_) => break,
            Ok(read) => got += read,
        }
    }
    head[..got] == *CACHEDIR_TAG_SIGNATURE
}

/// The regular files under a directory: their apparent sizes summed, and
/// how many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Sizes {
    pub(crate) bytes: u64,
    pub(crate) files: u64,
}

/// The regular files under `path`, leaving out the directories and files
/// named in `skip` (full paths) and everything under those directories. A
/// missing `path` is empty; a directory that cannot be read is skipped.
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
                && !skip.contains(&entry.path())
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

/// The test schemas' first sights, kept under the workspace root by the
/// Core lines reader (objective 094): its bookkeeping, no workspace.
pub(crate) const TEST_SCHEMAS_SEEN: &str = "test-schemas-seen.json";

/// The workspaces: everything under `root` outside the archive and its
/// progress directory, the shared build caches directory (`.caches`), the
/// workspace engine marker (issue #95), and the test schemas' first sights
/// (and the file it is written through).
pub(crate) fn workspaces(root: &Path) -> Sizes {
    walk(
        root,
        &[
            root.join(ARCHIVE),
            root.join(PROGRESS),
            root.join(SHARED_CACHES),
            root.join(spawn_host::workspace::BACKEND_MARKER),
            root.join(TEST_SCHEMAS_SEEN),
            root.join(format!("{TEST_SCHEMAS_SEEN}.partial")),
        ],
    )
}

/// Apparent regular-file bytes under `path` without following symlinks at the
/// cache root or inside it. Returns empty sizes if `path` is missing or is a
/// symlink.
pub(crate) fn shared_cache_sizes(path: &Path) -> Sizes {
    if !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir()) {
        return Sizes::default();
    }
    walk(path, &[])
}

/// Measure the active shared host build cache directory resolved via
/// [`crate::runtime::resolve_shared_build_cache`], or a residual real
/// `<root>/.caches` directory when present on disk (`enabled: false`).
pub(crate) fn shared_caches_with(
    root: Option<&Path>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<SharedCacheSizes> {
    if let Ok(Some(configured)) = crate::runtime::resolve_shared_build_cache(lookup, root) {
        return Some(SharedCacheSizes {
            sizes: shared_cache_sizes(&configured),
            path: configured,
            enabled: true,
        });
    }
    let default_dir = root?.join(SHARED_CACHES);
    std::fs::symlink_metadata(&default_dir)
        .is_ok_and(|meta| meta.file_type().is_dir())
        .then(|| SharedCacheSizes {
            sizes: walk(&default_dir, &[]),
            path: default_dir,
            enabled: false,
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageBucket {
    Repo,
    Git,
    BuildCache,
}

fn is_build_cache_dir(dir: &Path, name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|raw| BUILD_CACHE_DIR_NAMES.contains(&raw))
        || is_cachedir_tagged(dir)
}

fn measure_workspace_dir(name: String, dir: PathBuf, remaining: &mut usize) -> WorkspaceDiskUsage {
    let mut usage = WorkspaceDiskUsage {
        name,
        total_bytes: 0,
        repo_bytes: 0,
        git_bytes: 0,
        build_cache_bytes: 0,
        files: 0,
        truncated: false,
    };
    let mut pending = vec![(dir, UsageBucket::Repo)];
    while let Some((current, bucket)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            if *remaining == 0 {
                usage.truncated = true;
                return usage;
            }
            *remaining -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let child_name = entry.file_name();
            if kind.is_dir() {
                let child_path = entry.path();
                let next_bucket = match bucket {
                    UsageBucket::Git => UsageBucket::Git,
                    UsageBucket::BuildCache => UsageBucket::BuildCache,
                    UsageBucket::Repo if child_name == ".git" => UsageBucket::Git,
                    UsageBucket::Repo if is_build_cache_dir(&child_path, &child_name) => {
                        UsageBucket::BuildCache
                    }
                    UsageBucket::Repo => UsageBucket::Repo,
                };
                pending.push((child_path, next_bucket));
            } else if kind.is_file()
                && let Ok(meta) = entry.metadata()
            {
                let len = meta.len();
                let file_bucket = if bucket == UsageBucket::Repo && child_name == ".git" {
                    UsageBucket::Git
                } else {
                    bucket
                };
                match file_bucket {
                    UsageBucket::Repo => usage.repo_bytes = usage.repo_bytes.saturating_add(len),
                    UsageBucket::Git => usage.git_bytes = usage.git_bytes.saturating_add(len),
                    UsageBucket::BuildCache => {
                        usage.build_cache_bytes = usage.build_cache_bytes.saturating_add(len);
                    }
                }
                usage.total_bytes = usage.total_bytes.saturating_add(len);
                usage.files += 1;
            }
        }
    }
    usage
}

/// Bounded, symlink-safe per-workspace classification under `root` across all
/// non-`.` directories, keeping exact aggregate totals and returning the top
/// [`WORKSPACE_BREAKDOWN_TOP_LIMIT`] entries ordered by `total_bytes` descending.
pub(crate) fn workspace_breakdown(root: &Path, max_entries: usize) -> WorkspaceBreakdown {
    workspace_breakdown_with_limit(root, max_entries, WORKSPACE_BREAKDOWN_TOP_LIMIT)
}

fn workspace_breakdown_with_limit(
    root: &Path,
    max_entries: usize,
    top_limit: usize,
) -> WorkspaceBreakdown {
    let Ok(entries) = std::fs::read_dir(root) else {
        return WorkspaceBreakdown::default();
    };
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let raw_name = entry.file_name();
        let name = raw_name.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        candidates.push((name, entry.path()));
    }
    candidates.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut remaining = max_entries;
    let mut measured = Vec::with_capacity(candidates.len());
    let mut out = WorkspaceBreakdown {
        workspace_count: candidates.len(),
        ..WorkspaceBreakdown::default()
    };
    for (name, path) in candidates {
        let ws = measure_workspace_dir(name, path, &mut remaining);
        out.total_bytes = out.total_bytes.saturating_add(ws.total_bytes);
        out.repo_bytes = out.repo_bytes.saturating_add(ws.repo_bytes);
        out.git_bytes = out.git_bytes.saturating_add(ws.git_bytes);
        out.build_cache_bytes = out.build_cache_bytes.saturating_add(ws.build_cache_bytes);
        out.files = out.files.saturating_add(ws.files);
        out.truncated |= ws.truncated;
        measured.push(ws);
    }
    measured.sort_by(|a, b| {
        b.total_bytes
            .cmp(&a.total_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    measured.truncate(top_limit);
    out.workspaces = measured;
    out
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

    /// A fresh scratch directory of the test's own: each call its own name
    /// (the pid, the clock and this process's count of calls), made with
    /// `create_dir`, so a name already taken fails loudly and is never
    /// shared (objective 107: two drills tests once ran one fake).
    pub(crate) fn scratch(case: &str) -> PathBuf {
        static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let parent = std::env::temp_dir();
        std::fs::create_dir_all(&parent).expect("the temporary directory");
        let path = parent.join(format!(
            "spawn-storage-{}-{nanos:x}-{call}-{case}",
            std::process::id()
        ));
        std::fs::create_dir(&path)
            .unwrap_or_else(|error| panic!("a scratch directory {}: {error}", path.display()));
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
        // The workspace engine marker (issue #95) and the Core lines
        // reader's first sights (objective 094) are no workspace, nor
        // the file they are written through.
        file(&root.join(spawn_host::workspace::BACKEND_MARKER), 4);
        file(&root.join(super::TEST_SCHEMAS_SEEN), 2558);
        file(
            &root.join(format!("{}.partial", super::TEST_SCHEMAS_SEEN)),
            40,
        );
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
    /// Objective 097: the sample carries the session backups'
    /// `estimated_bytes` from their status file; without a file, or with
    /// one that has no such number, it leaves the field out and is still
    /// written.
    #[test]
    fn the_sample_carries_the_session_backups_estimated_bytes_when_their_status_reads() {
        let root = scratch("sampler_session_backups");
        let projects = root.join("projects");
        file(&root.join(".transcripts/agent_a/a1.jsonl"), 100);
        let status = root.join("status.json");
        let without = root.join("without.json");
        std::fs::write(
            &status,
            r#"{"at": 1, "accounts": [], "estimated_bytes": 41943040, "free_bytes": 1}"#,
        )
        .expect("the status file");
        std::fs::write(
            &without,
            r#"{"at": 1, "accounts": [], "estimated": "40 MiB"}"#,
        )
        .expect("a status file without the number");
        let mut written = Vec::new();
        for path in [
            Some(status),
            None,
            Some(root.join("none.json")),
            Some(without),
        ] {
            let mut sampler = Sampler::with(Some(&root), projects.clone()).with_status(path);
            sampler
                .check_with(
                    0,
                    || None,
                    |sample| {
                        written.push(sample.clone());
                        Ok(1)
                    },
                )
                .expect("written");
        }
        std::fs::remove_dir_all(&root).expect("clean up");
        assert_eq!(written.len(), 4, "every sample is written");
        assert_eq!(written[0]["session_backup_bytes"], 41_943_040);
        for (sample, why) in
            written[1..]
                .iter()
                .zip(["no file named", "no file there", "no number"])
        {
            assert!(
                sample.get("session_backup_bytes").is_none(),
                "{why}: {sample}"
            );
            assert_eq!(sample["archive_bytes"], 100, "{why}");
        }
    }

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

    const CARGO_TAG_SIG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";

    #[test]
    fn shared_caches_are_excluded_from_workspaces_and_counted_without_symlinks() {
        let root = scratch("shared_caches");
        let external = scratch("shared_caches_external");
        file(&root.join(".caches/zig/a.o"), 1200);
        file(&root.join(".caches/ccache/b.o"), 800);
        file(&root.join("agent_1/src/lib.rs"), 500);
        // Symlink inside `.caches` must never be followed.
        file(&external.join("outside.bin"), 9999);
        std::os::unix::fs::symlink(external.join("outside.bin"), root.join(".caches/link.bin"))
            .expect("symlink file");

        assert_eq!(
            super::workspaces(&root),
            Sizes {
                bytes: 500,
                files: 1
            },
            ".caches must be excluded from workspaces(&root)"
        );

        let residual = super::shared_caches_with(Some(&root), &|_| None).expect("residual");
        assert!(!residual.enabled);
        assert_eq!(residual.path, root.join(".caches"));
        assert_eq!(
            residual.sizes,
            Sizes {
                bytes: 2000,
                files: 2
            }
        );

        let enabled = super::shared_caches_with(Some(&root), &|k| {
            (k == "SPAWN_SHARED_BUILD_CACHES").then(|| "1".to_string())
        })
        .expect("enabled");
        assert!(enabled.enabled);
        assert_eq!(enabled.sizes.bytes, 2000);

        // Symlink as cache root itself must return zero sizes.
        let sym_cache = external.join("sym-cache");
        std::os::unix::fs::symlink(&external, &sym_cache).expect("symlink dir");
        assert_eq!(super::shared_cache_sizes(&sym_cache), Sizes::default());
        let custom_sym = super::shared_caches_with(Some(&root), &|k| {
            (k == "SPAWN_SHARED_BUILD_CACHE_DIR").then(|| sym_cache.display().to_string())
        })
        .expect("custom symlink cache");
        assert!(custom_sym.enabled);
        assert_eq!(custom_sym.sizes, Sizes::default());

        std::fs::remove_dir_all(&root).expect("clean root");
        std::fs::remove_dir_all(&external).expect("clean external");
    }

    #[test]
    fn workspace_breakdown_classifies_git_caches_tags_symlinks_bounds_and_top32() {
        let root = scratch("breakdown");
        let outside = scratch("breakdown_outside");
        file(&outside.join("secret_or_outside.bin"), 50_000);

        // Dot directories, files under root, and top-level symlinks are ignored.
        file(&root.join(".caches/zig/obj"), 7000);
        file(&root.join(".transcripts/t1.jsonl"), 6000);
        file(&root.join("test-schemas-seen.json"), 900);
        std::os::unix::fs::symlink(&outside, root.join("agent_symlink"))
            .expect("top-level symlink");

        // `ws_big`: `.git`, repo files, all named cache dirs, custom `CACHEDIR.TAG`, and internal symlinks.
        file(&root.join("ws_big/.git/objects/pack"), 400);
        file(&root.join("ws_big/.git/build/hook_meta"), 50); // sticky Git bucket
        file(&root.join("ws_big/src/main.rs"), 150);
        file(&root.join("ws_big/README.md"), 25);
        for cache_dir in [
            "target",
            ".zig-cache",
            "zig-out",
            "node_modules",
            ".venv",
            "__pycache__",
            ".pytest_cache",
            ".mypy_cache",
            ".ruff_cache",
            ".cache",
            "dist",
            "build",
        ] {
            file(&root.join(format!("ws_big/{cache_dir}/artifact")), 10);
        }
        // Custom directory tagged by valid CACHEDIR.TAG (tag + payload both in build_cache_bytes).
        let tagged = root.join("ws_big/custom_tagged_cache");
        std::fs::create_dir_all(&tagged).expect("mkdir tagged");
        std::fs::write(tagged.join("CACHEDIR.TAG"), CARGO_TAG_SIG).expect("write tag");
        file(&tagged.join("blob.bin"), 300);
        // Invalid CACHEDIR.TAG keeps directory in repo_bytes.
        file(&root.join("ws_big/not_cache/CACHEDIR.TAG"), 10);
        file(&root.join("ws_big/not_cache/data.txt"), 15);
        // Internal symlinks (including CACHEDIR.TAG symlink) must not be followed.
        std::fs::create_dir_all(root.join("ws_big/sym_tag_dir")).expect("mkdir sym_tag_dir");
        std::os::unix::fs::symlink(
            tagged.join("CACHEDIR.TAG"),
            root.join("ws_big/sym_tag_dir/CACHEDIR.TAG"),
        )
        .expect("symlink tag");
        file(&root.join("ws_big/sym_tag_dir/real.txt"), 12);
        std::os::unix::fs::symlink(&outside, root.join("ws_big/outside_link"))
            .expect("internal symlink");

        // `ws_worktree`: git worktree uses a regular `.git` file.
        file(&root.join("ws_worktree/.git"), 45);
        file(&root.join("ws_worktree/lib.rs"), 55);

        let full = super::workspace_breakdown(&root, 10_000);
        assert!(!full.truncated);
        assert_eq!(full.workspace_count, 2);
        assert_eq!(full.workspaces.len(), 2);
        assert_eq!(full.workspaces[0].name, "ws_big");
        assert_eq!(full.workspaces[1].name, "ws_worktree");

        let expected_tag_len = u64::try_from(CARGO_TAG_SIG.len()).expect("len");
        assert_eq!(full.workspaces[0].git_bytes, 450);
        assert_eq!(
            full.workspaces[0].build_cache_bytes,
            12 * 10 + 300 + expected_tag_len
        );
        assert_eq!(full.workspaces[0].repo_bytes, 150 + 25 + 10 + 15 + 12);
        assert_eq!(
            full.workspaces[0].total_bytes,
            full.workspaces[0].git_bytes
                + full.workspaces[0].build_cache_bytes
                + full.workspaces[0].repo_bytes
        );
        assert_eq!(full.workspaces[1].git_bytes, 45);
        assert_eq!(full.workspaces[1].repo_bytes, 55);
        assert_eq!(full.workspaces[1].build_cache_bytes, 0);
        assert_eq!(
            full.total_bytes,
            full.workspaces[0].total_bytes + full.workspaces[1].total_bytes
        );

        // `max_entries` truncation stops cleanly without infinite walks and sets `truncated: true`.
        let bounded = super::workspace_breakdown(&root, 5);
        assert!(bounded.truncated);
        assert!(bounded.files <= 5);
        assert_eq!(bounded.workspace_count, 2);

        std::fs::remove_dir_all(&root).expect("clean root");
        std::fs::remove_dir_all(&outside).expect("clean outside");
    }

    #[test]
    fn workspace_breakdown_sorts_descending_with_tie_breaker_and_caps_top32() {
        let top_root = scratch("breakdown_top32");
        let mut expected_sum = 0_u64;
        for idx in 0..35_u64 {
            let bytes = if idx < 2 { 500 } else { 100 + idx };
            let name = format!("ws_{idx:02}");
            file(
                &top_root.join(&name).join("f.bin"),
                usize::try_from(bytes).expect("fit"),
            );
            expected_sum += bytes;
        }
        let capped = super::workspace_breakdown(&top_root, 10_000);
        assert!(!capped.truncated);
        assert_eq!(capped.workspace_count, 35);
        assert_eq!(capped.total_bytes, expected_sum);
        assert_eq!(capped.repo_bytes, expected_sum);
        assert_eq!(capped.workspaces.len(), 32);
        // Tie break on `500` bytes: `ws_00` comes before `ws_01`.
        assert_eq!(capped.workspaces[0].name, "ws_00");
        assert_eq!(capped.workspaces[0].total_bytes, 500);
        assert_eq!(capped.workspaces[1].name, "ws_01");
        assert_eq!(capped.workspaces[1].total_bytes, 500);
        // Next largest is `ws_34` (`134` bytes) down through `ws_05` (`105` bytes).
        assert_eq!(capped.workspaces[2].name, "ws_34");
        assert_eq!(capped.workspaces[31].name, "ws_05");

        std::fs::remove_dir_all(&top_root).expect("clean top_root");
    }
}

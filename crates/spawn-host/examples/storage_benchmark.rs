//! End-to-end storage benchmark for Spawn's real host adapters.
//!
//! The benchmark creates a new scratch directory, drives GitWorkspace and
//! LocalTranscriptArchive through their public traits, checks every result and
//! prints one JSON report. Optionally SPAWN_BENCH_DATABASE_URL names a dedicated
//! benchmark database; a unique schema is created and the real host_record path
//! is measured. The live SPAWN_DATABASE_URL is never read.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use spawn_core::id::{AgentId, BranchName, WorkspaceId};
use spawn_core::port::{Commit, TranscriptArchive, WorkspaceStore};
use spawn_host::{GitWorkspace, LocalTranscriptArchive, git_command};
use spawn_postgres::PostgresStore;

const USAGE: &str = "spawn storage benchmark\n\
\n\
Usage: cargo run --release -p spawn-host --example storage_benchmark -- [options]\n\
\n\
  --root <new-directory>     Scratch root (default: a unique temporary path)\n\
  --source-revision <sha>    Spawn source revision recorded in the report\n\
  --iterations <n>           Measured cycles (default: 40)\n\
  --warmup <n>               Warm-up cycles (default: 5)\n\
  --files <n>                Workspace files (default: 512)\n\
  --file-bytes <n>           Bytes per workspace file (default: 4096)\n\
  --edits <n>                Files changed before each checkpoint (default: 4)\n\
  --transcript-bytes <n>     Durable transcript append per cycle (default: 16384)\n\
  --oversized-file-bytes <n> Add a file over the cap (must exceed --file-bytes)\n\
  --recreate-every <n>       Drop the branch before every nth checkpoint; 0 disables\n\
  --remount-every <n>        Unmount/mount every n cycles; 0 disables (default: 0)\n\
  --keep                     Keep the scratch root after a successful run\n\
  --help                     Show this help\n\
\n\
SPAWN_BENCH_DATABASE_URL optionally enables PostgreSQL writes. It must name a\n\
dedicated benchmark database, never the live store. A unique schema is used.\n";

#[derive(Debug)]
struct Config {
    root: Option<PathBuf>,
    source_revision: Option<String>,
    iterations: usize,
    warmup: usize,
    files: usize,
    file_bytes: usize,
    edits: usize,
    transcript_bytes: usize,
    oversized_file_bytes: Option<usize>,
    recreate_every: usize,
    remount_every: usize,
    keep: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: None,
            source_revision: None,
            iterations: 40,
            warmup: 5,
            files: 512,
            file_bytes: 4096,
            edits: 4,
            transcript_bytes: 16 * 1024,
            oversized_file_bytes: None,
            recreate_every: 0,
            remount_every: 0,
            keep: false,
        }
    }
}

fn number<T>(arguments: &mut impl Iterator<Item = String>, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    arguments
        .next()
        .ok_or_else(|| format!("{name} needs a value"))?
        .parse()
        .map_err(|_| format!("{name} needs a positive whole number"))
}

fn config() -> Result<Option<Config>, String> {
    let mut config = Config::default();
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => {
                config.root = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--root needs a path".to_string())?,
                ));
            }
            "--source-revision" => {
                config.source_revision =
                    Some(arguments.next().ok_or_else(|| {
                        "--source-revision needs a full Git commit id".to_string()
                    })?);
            }
            "--iterations" => config.iterations = number(&mut arguments, "--iterations")?,
            "--warmup" => config.warmup = number(&mut arguments, "--warmup")?,
            "--files" => config.files = number(&mut arguments, "--files")?,
            "--file-bytes" => config.file_bytes = number(&mut arguments, "--file-bytes")?,
            "--edits" => config.edits = number(&mut arguments, "--edits")?,
            "--transcript-bytes" => {
                config.transcript_bytes = number(&mut arguments, "--transcript-bytes")?;
            }
            "--oversized-file-bytes" => {
                config.oversized_file_bytes =
                    Some(number(&mut arguments, "--oversized-file-bytes")?);
            }
            "--recreate-every" => {
                config.recreate_every = number(&mut arguments, "--recreate-every")?;
            }
            "--remount-every" => {
                config.remount_every = number(&mut arguments, "--remount-every")?;
            }
            "--keep" => config.keep = true,
            "--help" | "-h" => return Ok(None),
            _ => return Err(format!("unknown option {argument:?}\n\n{USAGE}")),
        }
    }
    if config.iterations == 0
        || config.files == 0
        || config.file_bytes == 0
        || config.edits == 0
        || config.edits > config.files
        || config.transcript_bytes == 0
    {
        return Err(
            "iterations, files, file-bytes, edits and transcript-bytes must be positive; edits cannot exceed files"
                .to_string(),
        );
    }
    if config
        .oversized_file_bytes
        .is_some_and(|bytes| bytes <= config.file_bytes)
    {
        return Err("--oversized-file-bytes must exceed --file-bytes".to_string());
    }
    if config.oversized_file_bytes.is_some() && config.remount_every > 0 {
        return Err(
            "--oversized-file-bytes cannot be combined with --remount-every: an oversized file is intentionally not preserved by unmount"
                .to_string(),
        );
    }
    if let Some(revision) = &config.source_revision
        && (revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err("--source-revision must be a full 40-character Git commit id".to_string());
    }
    let last_cycle = config
        .warmup
        .checked_add(config.iterations)
        .and_then(|cycles| cycles.checked_sub(1))
        .ok_or_else(|| "warmup plus iterations is too large".to_string())?;
    let marker_bytes = format!("cycle={last_cycle} edit={}", config.edits - 1).len();
    if config.file_bytes < marker_bytes {
        return Err(format!(
            "--file-bytes must be at least {marker_bytes} to hold the edit marker"
        ));
    }
    Ok(Some(config))
}

struct Scratch {
    root: PathBuf,
    keep: bool,
}

impl Scratch {
    fn new(configured: Option<PathBuf>) -> Result<Self, String> {
        let root = configured.unwrap_or_else(|| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            env::temp_dir().join(format!(
                "spawn-storage-benchmark-{}-{nanos:x}",
                std::process::id()
            ))
        });
        fs::create_dir(&root).map_err(|error| {
            format!(
                "create new scratch root {}: {error}; an existing path is never reused",
                root.display()
            )
        })?;
        Ok(Self { root, keep: false })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

#[derive(Default)]
struct Samples(BTreeMap<&'static str, Vec<f64>>);

impl Samples {
    fn push(&mut self, operation: &'static str, elapsed: Duration) {
        self.0
            .entry(operation)
            .or_default()
            .push(elapsed.as_secs_f64() * 1000.0);
    }

    fn report(&self) -> Result<Value, String> {
        self.0
            .iter()
            .map(|(name, values)| Ok(((*name).to_string(), summary(values)?)))
            .collect()
    }
}

fn sample_count(count: usize) -> Result<f64, String> {
    let count = u32::try_from(count)
        .map_err(|_| "sample count exceeds the exactly representable range".to_string())?;
    Ok(f64::from(count))
}

fn percentile(sorted: &[f64], percent: u32) -> Result<f64, String> {
    let len = u128::try_from(sorted.len())
        .map_err(|_| "sample count does not fit the percentile calculation".to_string())?;
    let rank = (len * u128::from(percent)).div_ceil(100);
    let index = usize::try_from(rank.saturating_sub(1).min(len - 1))
        .map_err(|_| "percentile rank does not fit this platform".to_string())?;
    Ok(sorted[index])
}

fn summary(values: &[f64]) -> Result<Value, String> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let sum: f64 = sorted.iter().sum();
    let count = sample_count(sorted.len())?;
    Ok(json!({
        "n": sorted.len(),
        "min_ms": sorted[0],
        "p50_ms": percentile(&sorted, 50)?,
        "p95_ms": percentile(&sorted, 95)?,
        "p99_ms": percentile(&sorted, 99)?,
        "max_ms": sorted[sorted.len() - 1],
        "mean_ms": sum / count,
    }))
}

fn directory_size(root: &Path) -> Result<Value, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0_u64;
    let mut files = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("read directory entry: {error}"))?;
            let kind = entry
                .file_type()
                .map_err(|error| format!("stat {}: {error}", entry.path().display()))?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                bytes = bytes.saturating_add(
                    entry
                        .metadata()
                        .map_err(|error| format!("stat {}: {error}", entry.path().display()))?
                        .len(),
                );
                files += 1;
            }
        }
    }
    Ok(json!({"bytes": bytes, "files": files}))
}

fn seed_workspace(workspace: &Path, config: &Config) -> Result<(), String> {
    let payload = vec![b's'; config.file_bytes];
    for index in 0..config.files {
        let path = workspace.join(format!("tree/{:04}/{:06}.txt", index % 32, index));
        fs::create_dir_all(path.parent().expect("a seeded file has a parent"))
            .map_err(|error| format!("create {}: {error}", path.display()))?;
        fs::write(&path, &payload).map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    if let Some(bytes) = config.oversized_file_bytes {
        fs::write(workspace.join("oversized.bin"), vec![b'o'; bytes])
            .map_err(|error| format!("write the oversized benchmark file: {error}"))?;
    }
    Ok(())
}

fn edit_workspace(workspace: &Path, config: &Config, cycle: usize) -> Result<(), String> {
    for edit in 0..config.edits {
        let index = (cycle * config.edits + edit) % config.files;
        let path = workspace.join(format!("tree/{:04}/{:06}.txt", index % 32, index));
        let fill = b'a'
            + u8::try_from(cycle % 26)
                .map_err(|_| "cycle fill byte does not fit u8".to_string())?;
        let mut payload = vec![fill; config.file_bytes];
        let marker = format!("cycle={cycle} edit={edit}");
        payload[..marker.len()].copy_from_slice(marker.as_bytes());
        fs::write(&path, payload).map_err(|error| format!("edit {}: {error}", path.display()))?;
    }
    Ok(())
}

fn git_version() -> String {
    git_command("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unknown".to_string(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )
}

fn binary_sha256() -> Result<String, String> {
    let executable =
        env::current_exe().map_err(|error| format!("find the benchmark binary: {error}"))?;
    let output = Command::new("sha256sum")
        .arg(&executable)
        .output()
        .map_err(|error| format!("hash {}: {error}", executable.display()))?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let answer = String::from_utf8_lossy(&output.stdout);
    let hash = answer.split_whitespace().next().unwrap_or_default();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("sha256sum returned an invalid binary hash".to_string());
    }
    Ok(hash.to_string())
}

fn assert_oversized_excluded(
    repo: &Path,
    branch: &BranchName,
    required: bool,
) -> Result<(), String> {
    if !required {
        return Ok(());
    }
    let output = git_command("git")
        .arg(format!("--git-dir={}", repo.display()))
        .args([
            "ls-tree",
            "--name-only",
            &format!("refs/heads/{branch}"),
            "--",
            "oversized.bin",
        ])
        .output()
        .map_err(|error| format!("inspect the committed oversized path: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-tree failed for oversized.bin: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if output.stdout.iter().any(|byte| !byte.is_ascii_whitespace()) {
        return Err("oversized.bin was included in a checkpoint".to_string());
    }
    Ok(())
}

fn check_repository(repo: &Path) -> Result<(), String> {
    let output = git_command("git")
        .arg(format!("--git-dir={}", repo.display()))
        .args(["fsck", "--full", "--no-dangling"])
        .output()
        .map_err(|error| format!("run git fsck: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git fsck failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn drop_branch(repo: &Path, branch: &BranchName) -> Result<(), String> {
    let output = git_command("git")
        .arg(format!("--git-dir={}", repo.display()))
        .args(["update-ref", "-d", &format!("refs/heads/{branch}")])
        .output()
        .map_err(|error| format!("drop the benchmark branch: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "dropping the benchmark branch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn initialize_repository(repo: &Path) -> Result<(), String> {
    let initialized = git_command("git")
        .args(["init", "--bare", "--quiet"])
        .arg(repo)
        .output()
        .map_err(|error| format!("run git init: {error}"))?;
    if initialized.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git init failed: {}",
            String::from_utf8_lossy(&initialized.stderr).trim()
        ))
    }
}

fn connect_database() -> Result<Option<PostgresStore>, String> {
    let database_url = env::var("SPAWN_BENCH_DATABASE_URL").ok();
    let schema = format!(
        "spawn_storage_benchmark_{}_{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    );
    database_url
        .as_deref()
        .map(|url| PostgresStore::connect_in_schema(url, &schema))
        .transpose()
        .map_err(|error| format!("connect the dedicated benchmark database: {error}"))
}

struct Benchmark {
    config: Config,
    scratch: Scratch,
    repo: PathBuf,
    store: GitWorkspace,
    branch: BranchName,
    workspace: WorkspaceId,
    archive: LocalTranscriptArchive,
    agent: AgentId,
    transcript_offset: u64,
    transcript_payload: Vec<u8>,
    database: Option<PostgresStore>,
    last_database_event: Option<i64>,
    samples: Samples,
    cycle_times: Vec<f64>,
}

impl Benchmark {
    fn new(mut config: Config) -> Result<Self, String> {
        let mut scratch = Scratch::new(config.root.take())?;
        scratch.keep = config.keep;
        let repo = scratch.root.join("store.git");
        let workspaces = scratch.root.join("workspaces");
        fs::create_dir(&workspaces).map_err(|error| format!("create workspace root: {error}"))?;
        initialize_repository(&repo)?;

        let store = GitWorkspace::new(&repo, &workspaces);
        let store = if config.oversized_file_bytes.is_some() {
            store.with_max_file_bytes(u64::try_from(config.file_bytes).map_err(|_| {
                "the maximum file size does not fit the workspace limit".to_string()
            })?)
        } else {
            store
        };
        let branch =
            BranchName::new("agent_storage_benchmark").map_err(|error| error.to_string())?;
        store
            .ensure_branch(&branch, None)
            .map_err(|error| error.to_string())?;
        let workspace = store.mount(&branch).map_err(|error| error.to_string())?;
        seed_workspace(Path::new(workspace.path()), &config)?;
        if !matches!(
            store
                .commit(&workspace, "benchmark: seed")
                .map_err(|error| error.to_string())?,
            Commit::Created(_)
        ) {
            return Err("the seeded workspace did not produce a commit".to_string());
        }
        assert_oversized_excluded(&repo, &branch, config.oversized_file_bytes.is_some())?;

        let archive = LocalTranscriptArchive::new(scratch.root.join(".transcripts"));
        let agent = AgentId::new("agent_storage_benchmark").map_err(|error| error.to_string())?;
        let mut transcript_payload = vec![b't'; config.transcript_bytes];
        transcript_payload[config.transcript_bytes - 1] = b'\n';
        let database = connect_database()?;
        let cycle_times = Vec::with_capacity(config.iterations);
        Ok(Self {
            config,
            scratch,
            repo,
            store,
            branch,
            workspace,
            archive,
            agent,
            transcript_offset: 0,
            transcript_payload,
            database,
            last_database_event: None,
            samples: Samples::default(),
            cycle_times,
        })
    }

    fn checkpoint_changed(
        &mut self,
        cycle: usize,
        recreate: bool,
        measured: bool,
    ) -> Result<(), String> {
        if recreate {
            drop_branch(&self.repo, &self.branch)?;
        }
        let clock = Instant::now();
        let committed = self
            .store
            .commit(&self.workspace, &format!("benchmark: cycle {cycle}"))
            .map_err(|error| error.to_string())?;
        let elapsed = clock.elapsed();
        let valid = if recreate {
            matches!(committed, Commit::Recreated { .. })
        } else {
            matches!(committed, Commit::Created(_))
        };
        if !valid {
            return Err(format!(
                "cycle {cycle}: checkpoint result did not match recreate={recreate}"
            ));
        }
        self.check_oversized_excluded()?;
        if measured {
            self.samples.push("checkpoint_changed", elapsed);
        }
        Ok(())
    }

    fn check_oversized_excluded(&self) -> Result<(), String> {
        assert_oversized_excluded(
            &self.repo,
            &self.branch,
            self.config.oversized_file_bytes.is_some(),
        )
    }

    fn checkpoint_noop(&mut self, cycle: usize, measured: bool) -> Result<(), String> {
        let clock = Instant::now();
        let unchanged = self
            .store
            .commit(&self.workspace, "benchmark: unchanged")
            .map_err(|error| error.to_string())?;
        let elapsed = clock.elapsed();
        if !matches!(unchanged, Commit::Unchanged) {
            return Err(format!("cycle {cycle}: the no-op checkpoint made a commit"));
        }
        self.check_oversized_excluded()?;
        if measured {
            self.samples.push("checkpoint_noop", elapsed);
        }
        Ok(())
    }

    fn append_transcript(&mut self, measured: bool) -> Result<(), String> {
        let clock = Instant::now();
        self.archive
            .append(
                &self.agent,
                "storage-benchmark",
                0,
                self.transcript_offset,
                &self.transcript_payload,
            )
            .map_err(|error| error.to_string())?;
        let elapsed = clock.elapsed();
        let appended = u64::try_from(self.config.transcript_bytes)
            .map_err(|_| "transcript append length does not fit u64".to_string())?;
        self.transcript_offset = self
            .transcript_offset
            .checked_add(appended)
            .ok_or_else(|| "transcript offset overflow".to_string())?;
        if measured {
            self.samples.push("transcript_append", elapsed);
        }
        Ok(())
    }

    fn database_cycle(&mut self, cycle: usize, measured: bool) -> Result<(), String> {
        let Some(database) = &self.database else {
            return Ok(());
        };
        let record = json!({
            "type": "host.storage",
            "archive_bytes": self.transcript_offset,
            "archive_files": 1,
            "projects_bytes": 0,
            "projects_files": 0,
            "benchmark_cycle": cycle,
        });
        let clock = Instant::now();
        let event = database
            .host_record(&record)
            .map_err(|error| format!("benchmark database append: {error}"))?;
        let elapsed = clock.elapsed();
        if self
            .last_database_event
            .is_some_and(|previous| event <= previous)
        {
            return Err(format!("cycle {cycle}: database event ids did not advance"));
        }
        self.last_database_event = Some(event);
        if measured {
            self.samples.push("postgres_append", elapsed);
        }

        let clock = Instant::now();
        let reading = database
            .host_storage(0, i64::MAX)
            .map_err(|error| format!("benchmark database read: {error}"))?;
        let elapsed = clock.elapsed();
        if reading["before"]["archive_bytes"].as_u64() != Some(self.transcript_offset) {
            return Err(format!(
                "cycle {cycle}: the database read did not return the latest storage sample"
            ));
        }
        if measured {
            self.samples.push("postgres_read", elapsed);
        }
        Ok(())
    }

    fn remount(&mut self, measured: bool) -> Result<(), String> {
        let clock = Instant::now();
        self.store
            .unmount(&self.workspace)
            .map_err(|error| error.to_string())?;
        let elapsed = clock.elapsed();
        if measured {
            self.samples.push("unmount", elapsed);
        }
        let clock = Instant::now();
        self.workspace = self
            .store
            .mount(&self.branch)
            .map_err(|error| error.to_string())?;
        let elapsed = clock.elapsed();
        if measured {
            self.samples.push("mount", elapsed);
        }
        Ok(())
    }

    fn run_cycle(&mut self, cycle: usize, measured: bool) -> Result<Duration, String> {
        let cycle_clock = Instant::now();
        edit_workspace(Path::new(self.workspace.path()), &self.config, cycle)?;
        let recreate = self.config.recreate_every > 0
            && (cycle + 1).is_multiple_of(self.config.recreate_every);
        self.checkpoint_changed(cycle, recreate, measured)?;
        self.checkpoint_noop(cycle, measured)?;
        self.append_transcript(measured)?;
        self.database_cycle(cycle, measured)?;
        if self.config.remount_every > 0 && (cycle + 1).is_multiple_of(self.config.remount_every) {
            self.remount(measured)?;
        }
        Ok(cycle_clock.elapsed())
    }

    fn run_cycles(&mut self) -> Result<f64, String> {
        let total_cycles = self
            .config
            .warmup
            .checked_add(self.config.iterations)
            .ok_or_else(|| "warmup plus iterations is too large".to_string())?;
        let progress_every = (self.config.iterations / 10).max(1);
        let wall = Instant::now();
        for cycle in 0..total_cycles {
            let measured = cycle >= self.config.warmup;
            let elapsed = self.run_cycle(cycle, measured)?;
            if measured {
                self.cycle_times.push(elapsed.as_secs_f64() * 1000.0);
                let completed = cycle - self.config.warmup + 1;
                if completed.is_multiple_of(progress_every) {
                    eprintln!(
                        "storage benchmark: {completed}/{} measured cycles",
                        self.config.iterations
                    );
                }
            }
        }
        Ok(wall.elapsed().as_secs_f64() * 1000.0)
    }

    fn verify(&self) -> Result<(), String> {
        let restored = self
            .archive
            .read(
                &self.agent,
                "storage-benchmark",
                0,
                0,
                usize::try_from(self.transcript_offset).unwrap_or(usize::MAX),
            )
            .map_err(|error| error.to_string())?;
        if u64::try_from(restored.len()) != Ok(self.transcript_offset)
            || restored.get(restored.len() - self.config.transcript_bytes..)
                != Some(self.transcript_payload.as_slice())
        {
            return Err("the transcript archive failed its final content check".to_string());
        }
        check_repository(&self.repo)
    }

    fn report(&self, wall_ms: f64) -> Result<Value, String> {
        let created_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("read the system clock: {error}"))
            .and_then(|elapsed| {
                u64::try_from(elapsed.as_millis())
                    .map_err(|_| "system clock milliseconds do not fit u64".to_string())
            })?;
        let cycle_seconds: f64 = self.cycle_times.iter().sum::<f64>() / 1000.0;
        let iterations = sample_count(self.config.iterations)?;
        let benchmark_binary_sha256 = binary_sha256()?;
        let mut operations = self.samples.report()?;
        operations["cycle"] = summary(&self.cycle_times)?;
        Ok(json!({
            "schema_version": 1,
            "benchmark": "spawn-storage",
            "created_at_ms": created_at_ms,
            "config": {
                "iterations": self.config.iterations,
                "warmup": self.config.warmup,
                "files": self.config.files,
                "file_bytes": self.config.file_bytes,
                "edits": self.config.edits,
                "transcript_bytes": self.config.transcript_bytes,
                "oversized_file_bytes": self.config.oversized_file_bytes,
                "recreate_every": self.config.recreate_every,
                "remount_every": self.config.remount_every,
                "database": self.database.is_some(),
            },
            "environment": {
                "git_version": git_version(),
                "spawn_source_revision": self.config.source_revision.as_deref(),
                "benchmark_binary_sha256": benchmark_binary_sha256,
                "scratch_root": &self.scratch.root,
            },
            "operations": operations,
            "totals": {
                "wall_ms": wall_ms,
                "measured_cycles_per_second": iterations / cycle_seconds,
                "transcript_bytes_verified": self.transcript_offset,
                "last_database_event": self.last_database_event,
                "git_fsck": "passed",
            },
            "disk": {
                "repository": directory_size(&self.repo)?,
                "workspace": directory_size(Path::new(self.workspace.path()))?,
                "transcripts": directory_size(&self.scratch.root.join(".transcripts"))?,
            },
            "scratch_kept": self.config.keep,
        }))
    }
}

fn run(config: Config) -> Result<Value, String> {
    let mut benchmark = Benchmark::new(config)?;
    let wall_ms = benchmark.run_cycles()?;
    benchmark.verify()?;
    benchmark.report(wall_ms)
}

fn main() {
    match config() {
        Ok(None) => print!("{USAGE}"),
        Ok(Some(config)) => match run(config) {
            Ok(report) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).expect("JSON report")
                );
                let _ = std::io::stdout().flush();
            }
            Err(error) => {
                eprintln!("storage benchmark: {error}");
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!("storage benchmark: {error}");
            std::process::exit(2);
        }
    }
}

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
use spawn_core::id::{AgentId, BranchName};
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
    if let Some(revision) = &config.source_revision {
        if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("--source-revision must be a full 40-character Git commit id".to_string());
        }
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

    fn report(&self) -> Value {
        self.0
            .iter()
            .map(|(name, values)| ((*name).to_string(), summary(values)))
            .collect()
    }
}

fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    let rank = (percentile * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn summary(values: &[f64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let sum: f64 = sorted.iter().sum();
    json!({
        "n": sorted.len(),
        "min_ms": sorted[0],
        "p50_ms": percentile(&sorted, 0.50),
        "p95_ms": percentile(&sorted, 0.95),
        "p99_ms": percentile(&sorted, 0.99),
        "max_ms": sorted[sorted.len() - 1],
        "mean_ms": sum / sorted.len() as f64,
    })
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
        let mut payload = vec![b'a' + (cycle % 26) as u8; config.file_bytes];
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
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
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

fn run(mut config: Config) -> Result<Value, String> {
    let mut scratch = Scratch::new(config.root.take())?;
    scratch.keep = config.keep;
    let repo = scratch.root.join("store.git");
    let workspaces = scratch.root.join("workspaces");
    fs::create_dir(&workspaces).map_err(|error| format!("create workspace root: {error}"))?;
    let initialized = git_command("git")
        .args(["init", "--bare", "--quiet"])
        .arg(&repo)
        .output()
        .map_err(|error| format!("run git init: {error}"))?;
    if !initialized.status.success() {
        return Err(format!(
            "git init failed: {}",
            String::from_utf8_lossy(&initialized.stderr).trim()
        ));
    }

    let store = GitWorkspace::new(&repo, &workspaces);
    let store = if config.oversized_file_bytes.is_some() {
        store.with_max_file_bytes(config.file_bytes as u64)
    } else {
        store
    };
    let branch = BranchName::new("agent_storage_benchmark").map_err(|error| error.to_string())?;
    store
        .ensure_branch(&branch, None)
        .map_err(|error| error.to_string())?;
    let mut workspace = store.mount(&branch).map_err(|error| error.to_string())?;
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
    let native = "storage-benchmark";
    let mut transcript_offset = 0_u64;
    let transcript_payload = {
        let mut payload = vec![b't'; config.transcript_bytes];
        payload[config.transcript_bytes - 1] = b'\n';
        payload
    };

    let database_url = env::var("SPAWN_BENCH_DATABASE_URL").ok();
    let schema = format!(
        "spawn_storage_benchmark_{}_{:x}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    );
    let database = database_url
        .as_deref()
        .map(|url| PostgresStore::connect_in_schema(url, &schema))
        .transpose()
        .map_err(|error| format!("connect the dedicated benchmark database: {error}"))?;
    let mut last_database_event = None;

    let total_cycles = config.warmup + config.iterations;
    let progress_every = (config.iterations / 10).max(1);
    let mut samples = Samples::default();
    let mut cycle_times = Vec::with_capacity(config.iterations);
    let wall = Instant::now();
    for cycle in 0..total_cycles {
        let measured = cycle >= config.warmup;
        let cycle_clock = Instant::now();
        edit_workspace(Path::new(workspace.path()), &config, cycle)?;
        let recreate = config.recreate_every > 0 && (cycle + 1) % config.recreate_every == 0;
        if recreate {
            drop_branch(&repo, &branch)?;
        }

        let clock = Instant::now();
        let committed = store
            .commit(&workspace, &format!("benchmark: cycle {cycle}"))
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
        assert_oversized_excluded(&repo, &branch, config.oversized_file_bytes.is_some())?;
        if measured {
            samples.push("checkpoint_changed", elapsed);
        }

        let clock = Instant::now();
        let unchanged = store
            .commit(&workspace, "benchmark: unchanged")
            .map_err(|error| error.to_string())?;
        let elapsed = clock.elapsed();
        if !matches!(unchanged, Commit::Unchanged) {
            return Err(format!("cycle {cycle}: the no-op checkpoint made a commit"));
        }
        assert_oversized_excluded(&repo, &branch, config.oversized_file_bytes.is_some())?;
        if measured {
            samples.push("checkpoint_noop", elapsed);
        }

        let clock = Instant::now();
        archive
            .append(&agent, native, 0, transcript_offset, &transcript_payload)
            .map_err(|error| error.to_string())?;
        let elapsed = clock.elapsed();
        transcript_offset += config.transcript_bytes as u64;
        if measured {
            samples.push("transcript_append", elapsed);
        }

        if let Some(database) = &database {
            let record = json!({
                "type": "host.storage",
                "archive_bytes": transcript_offset,
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
            if last_database_event.is_some_and(|previous| event <= previous) {
                return Err(format!("cycle {cycle}: database event ids did not advance"));
            }
            last_database_event = Some(event);
            if measured {
                samples.push("postgres_append", elapsed);
            }

            let clock = Instant::now();
            let reading = database
                .host_storage(0, i64::MAX)
                .map_err(|error| format!("benchmark database read: {error}"))?;
            let elapsed = clock.elapsed();
            if reading["before"]["archive_bytes"].as_u64() != Some(transcript_offset) {
                return Err(format!(
                    "cycle {cycle}: the database read did not return the latest storage sample"
                ));
            }
            if measured {
                samples.push("postgres_read", elapsed);
            }
        }

        if config.remount_every > 0 && (cycle + 1) % config.remount_every == 0 {
            let clock = Instant::now();
            store
                .unmount(&workspace)
                .map_err(|error| error.to_string())?;
            let elapsed = clock.elapsed();
            if measured {
                samples.push("unmount", elapsed);
            }
            let clock = Instant::now();
            workspace = store.mount(&branch).map_err(|error| error.to_string())?;
            let elapsed = clock.elapsed();
            if measured {
                samples.push("mount", elapsed);
            }
        }

        if measured {
            cycle_times.push(cycle_clock.elapsed().as_secs_f64() * 1000.0);
            if (cycle - config.warmup + 1) % progress_every == 0 {
                eprintln!(
                    "storage benchmark: {}/{} measured cycles",
                    cycle - config.warmup + 1,
                    config.iterations
                );
            }
        }
    }
    let wall_ms = wall.elapsed().as_secs_f64() * 1000.0;

    let restored = archive
        .read(
            &agent,
            native,
            0,
            0,
            usize::try_from(transcript_offset).unwrap_or(usize::MAX),
        )
        .map_err(|error| error.to_string())?;
    if restored.len() as u64 != transcript_offset
        || restored.get(restored.len() - config.transcript_bytes..)
            != Some(transcript_payload.as_slice())
    {
        return Err("the transcript archive failed its final content check".to_string());
    }
    check_repository(&repo)?;

    let cycle_seconds: f64 = cycle_times.iter().sum::<f64>() / 1000.0;
    let benchmark_binary_sha256 = binary_sha256()?;
    let mut operations = samples.report();
    operations["cycle"] = summary(&cycle_times);
    Ok(json!({
        "schema_version": 1,
        "benchmark": "spawn-storage",
        "created_at_ms": SystemTime::now().duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64),
        "config": {
            "iterations": config.iterations,
            "warmup": config.warmup,
            "files": config.files,
            "file_bytes": config.file_bytes,
            "edits": config.edits,
            "transcript_bytes": config.transcript_bytes,
            "oversized_file_bytes": config.oversized_file_bytes,
            "recreate_every": config.recreate_every,
            "remount_every": config.remount_every,
            "database": database.is_some(),
        },
        "environment": {
            "git_version": git_version(),
            "spawn_source_revision": config.source_revision,
            "benchmark_binary_sha256": benchmark_binary_sha256,
            "scratch_root": scratch.root,
        },
        "operations": operations,
        "totals": {
            "wall_ms": wall_ms,
            "measured_cycles_per_second": config.iterations as f64 / cycle_seconds,
            "transcript_bytes_verified": transcript_offset,
            "last_database_event": last_database_event,
            "git_fsck": "passed",
        },
        "disk": {
            "repository": directory_size(&repo)?,
            "workspace": directory_size(Path::new(workspace.path()))?,
            "transcripts": directory_size(&scratch.root.join(".transcripts"))?,
        },
        "scratch_kept": config.keep,
    }))
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

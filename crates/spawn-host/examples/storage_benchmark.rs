//! End-to-end storage benchmark for Spawn's real host adapters (`git`, `d6`, `shadow`).
//!
//! Creates an isolated scratch directory, drives [`GitWorkspace`], pure-Rust [`D6Workspace`],
//! or dual-track [`ShadowD6Workspace`] alongside [`LocalTranscriptArchive`] through their
//! public traits, verifies every invariant, and prints one JSON report.
//!
//! Optionally `SPAWN_BENCH_DATABASE_URL` names a dedicated benchmark PostgreSQL database
//! (required for `--backend d6|shadow`); a unique schema is created and measured. The live
//! `SPAWN_DATABASE_URL` is NEVER read.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use spawn_cas::{Digest, GitOid, Snapshot, git_blob_oid_of_bytes, git_pointer_bytes};
use spawn_core::id::{AgentId, BranchName, WorkspaceId};
use spawn_core::port::{BaseMerge, Commit, TranscriptArchive, WorkspaceStore};
use spawn_host::d6_workspace::L1TmpfsPolicy;
use spawn_host::{
    CasMerkleStore, D6RefKind, D6StoreError, D6Workspace, GitWorkspace, LedgerRef, LedgerSnapshot,
    LocalTranscriptArchive, RefLedger, ShadowD6Workspace, ShadowOracleMetrics, format_commit_body,
    git_command,
};
use spawn_postgres::{PostgresStore, RefKind};

const USAGE: &str = "spawn storage benchmark\n\
\n\
Usage: cargo run --release -p spawn-host --example storage_benchmark -- [options]\n\
\n\
  --root <new-directory>     Scratch root (default: a unique temporary path)\n\
  --source-revision <sha>    Spawn source revision recorded in the report\n\
  --backend <git|d6|shadow>  Storage backend (default: git)\n\
  --concurrency <n>          Concurrent branch workers (default: 1)\n\
  --tmpfs-root <directory>   Enable bounded L1 tmpfs upperdir root for d6/shadow\n\
  --tmpfs-budget-mb <n>      L1 tmpfs budget in MiB when --tmpfs-root is set (default: 2048)\n\
  --iterations <n>           Measured cycles per worker (default: 40)\n\
  --warmup <n>               Warm-up cycles per worker (default: 5)\n\
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
SPAWN_BENCH_DATABASE_URL optionally enables PostgreSQL writes (and is required for\n\
--backend d6|shadow). It must name a dedicated benchmark database, never the live store.\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Git,
    D6,
    Shadow,
}

impl Backend {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::D6 => "d6",
            Self::Shadow => "shadow",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "git" => Ok(Self::Git),
            "d6" => Ok(Self::D6),
            "shadow" => Ok(Self::Shadow),
            _ => Err(format!(
                "invalid --backend {raw:?}; expected git, d6 or shadow"
            )),
        }
    }
}

#[derive(Debug)]
struct Config {
    root: Option<PathBuf>,
    source_revision: Option<String>,
    backend: Backend,
    concurrency: usize,
    tmpfs_root: Option<PathBuf>,
    tmpfs_budget_mb: u64,
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
            backend: Backend::Git,
            concurrency: 1,
            tmpfs_root: None,
            tmpfs_budget_mb: 2048,
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

fn validate_config(config: &Config) -> Result<(), String> {
    if config.concurrency == 0
        || config.iterations == 0
        || config.files == 0
        || config.file_bytes == 0
        || config.edits == 0
        || config.edits > config.files
        || config.transcript_bytes == 0
    {
        return Err(
            "concurrency, iterations, files, file-bytes, edits and transcript-bytes must be positive; edits cannot exceed files"
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
    if config.oversized_file_bytes.is_some() && config.recreate_every > 0 {
        return Err("--oversized-file-bytes cannot be combined with --recreate-every".to_string());
    }
    if config.recreate_every > 0 && config.backend != Backend::Git {
        return Err("--recreate-every is only supported with --backend git".to_string());
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
    Ok(())
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
            "--backend" => {
                let raw = arguments
                    .next()
                    .ok_or_else(|| "--backend needs git, d6 or shadow".to_string())?;
                config.backend = Backend::parse(&raw)?;
            }
            "--concurrency" => config.concurrency = number(&mut arguments, "--concurrency")?,
            "--tmpfs-root" => {
                config.tmpfs_root = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--tmpfs-root needs a path".to_string())?,
                ));
            }
            "--tmpfs-budget-mb" => {
                config.tmpfs_budget_mb = number(&mut arguments, "--tmpfs-budget-mb")?;
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
    validate_config(&config)?;
    Ok(Some(config))
}

struct Scratch {
    root: PathBuf,
    tmpfs_root: Option<PathBuf>,
    keep: bool,
}

impl Scratch {
    fn new(
        configured: Option<PathBuf>,
        tmpfs: Option<PathBuf>,
        keep: bool,
    ) -> Result<Self, String> {
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
        if let Some(t) = &tmpfs {
            fs::create_dir_all(t)
                .map_err(|error| format!("create tmpfs benchmark root {}: {error}", t.display()))?;
        }
        Ok(Self {
            root,
            tmpfs_root: tmpfs,
            keep,
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.root);
            if let Some(t) = &self.tmpfs_root {
                let _ = fs::remove_dir_all(t);
            }
        }
    }
}

#[derive(Default, Clone)]
struct Samples(BTreeMap<&'static str, Vec<f64>>);

impl Samples {
    fn push(&mut self, operation: &'static str, elapsed: Duration) {
        self.0
            .entry(operation)
            .or_default()
            .push(elapsed.as_secs_f64() * 1000.0);
    }

    fn merge_from(&mut self, other: Self) {
        for (k, mut v) in other.0 {
            self.0.entry(k).or_default().append(&mut v);
        }
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

#[derive(Default, Clone, Copy)]
struct DirStats {
    bytes: u64,
    files: u64,
}

impl DirStats {
    fn to_json(self) -> Value {
        json!({"bytes": self.bytes, "files": self.files})
    }
}

fn directory_size(root: &Path) -> Result<DirStats, String> {
    if !root.exists() {
        return Ok(DirStats::default());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut stats = DirStats::default();
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
                stats.bytes = stats.bytes.saturating_add(
                    entry
                        .metadata()
                        .map_err(|error| format!("stat {}: {error}", entry.path().display()))?
                        .len(),
                );
                stats.files += 1;
            }
        }
    }
    Ok(stats)
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

fn git_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_command("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn initialize_repository_with_main(repo: &Path, stage_seed: &Path) -> Result<GitOid, String> {
    let initialized = git_command("git")
        .args(["init", "--bare", "--quiet"])
        .arg(repo)
        .output()
        .map_err(|error| format!("run git init: {error}"))?;
    if !initialized.status.success() {
        return Err(format!(
            "git init failed: {}",
            String::from_utf8_lossy(&initialized.stderr).trim()
        ));
    }
    fs::create_dir_all(stage_seed).map_err(|e| format!("mkdir stage: {e}"))?;
    git_in(stage_seed, &["init", "-q", "--initial-branch=main"])?;
    fs::write(stage_seed.join(".gitignore"), "*.tmp\n").map_err(|e| e.to_string())?;
    fs::write(stage_seed.join("README.md"), "benchmark base\n").map_err(|e| e.to_string())?;
    git_in(stage_seed, &["add", "-A"])?;
    git_in(stage_seed, &["commit", "-qm", "benchmark: base main"])?;
    let oid_hex = git_in(stage_seed, &["rev-parse", "main"])?;
    let push = git_command("git")
        .current_dir(stage_seed)
        .args(["push", "-q"])
        .arg(repo)
        .arg("main:refs/heads/main")
        .output()
        .map_err(|e| format!("push base main: {e}"))?;
    if !push.status.success() {
        return Err(format!(
            "push base main failed: {}",
            String::from_utf8_lossy(&push.stderr).trim()
        ));
    }
    git_command("git")
        .arg(format!("--git-dir={}", repo.display()))
        .args(["symbolic-ref", "HEAD", "refs/heads/main"])
        .status()
        .map_err(|e| e.to_string())?;
    GitOid::parse_hex(&oid_hex).map_err(|e| e.to_string())
}

fn advance_git_main(
    repo: &Path,
    stage_seed: &Path,
    step: usize,
) -> Result<(GitOid, GitOid), String> {
    let prev = GitOid::parse_hex(
        &git_in(
            repo,
            &[
                &format!("--git-dir={}", repo.display()),
                "rev-parse",
                "refs/heads/main",
            ],
        )
        .or_else(|_| {
            let out = git_command("git")
                .arg(format!("--git-dir={}", repo.display()))
                .args(["rev-parse", "refs/heads/main"])
                .output()
                .map_err(|e| e.to_string())?;
            Ok::<String, String>(String::from_utf8_lossy(&out.stdout).trim().to_string())
        })?,
    )
    .map_err(|e| e.to_string())?;

    let rel_dir = stage_seed.join("merge_bench");
    fs::create_dir_all(&rel_dir).map_err(|e| e.to_string())?;
    fs::write(
        rel_dir.join(format!("step_{step}.txt")),
        format!("merge step {step}\n"),
    )
    .map_err(|e| e.to_string())?;
    git_in(stage_seed, &["add", "-A"])?;
    git_in(stage_seed, &["commit", "-qm", &format!("main step {step}")])?;
    let next_hex = git_in(stage_seed, &["rev-parse", "main"])?;
    let push = git_command("git")
        .current_dir(stage_seed)
        .args(["push", "-q"])
        .arg(repo)
        .arg("main:refs/heads/main")
        .output()
        .map_err(|e| format!("push main step: {e}"))?;
    if !push.status.success() {
        return Err(format!(
            "push main step failed: {}",
            String::from_utf8_lossy(&push.stderr).trim()
        ));
    }
    let next = GitOid::parse_hex(&next_hex).map_err(|e| e.to_string())?;
    Ok((prev, next))
}

fn check_git_repository(repo: &Path) -> Result<(), String> {
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

fn assert_git_oversized_excluded(
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
        return Err("oversized.bin was included in a git checkpoint".to_string());
    }
    Ok(())
}

fn drop_git_branch(repo: &Path, branch: &BranchName) -> Result<(), String> {
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

struct PgRefLedger {
    pg: PostgresStore,
}

impl PgRefLedger {
    fn connect_schema(url: &str, schema: &str) -> Result<Self, String> {
        let pg = PostgresStore::connect_in_schema(url, schema)
            .map_err(|e| format!("connect benchmark schema {schema}: {e}"))?;
        Ok(Self { pg })
    }
}

fn to_pg_kind(kind: D6RefKind) -> RefKind {
    match kind {
        D6RefKind::Main => RefKind::Main,
        D6RefKind::Branch => RefKind::Branch,
        D6RefKind::Wip => RefKind::Wip,
        D6RefKind::Tag => RefKind::Tag,
    }
}

fn from_pg_kind(kind: RefKind) -> D6RefKind {
    match kind {
        RefKind::Main => D6RefKind::Main,
        RefKind::Branch => D6RefKind::Branch,
        RefKind::Wip => D6RefKind::Wip,
        RefKind::Tag => D6RefKind::Tag,
    }
}

impl RefLedger for PgRefLedger {
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError> {
        let parent_strs: Vec<String> = parents.iter().map(Digest::to_string).collect();
        let out = self
            .pg
            .record_snapshot(
                &id.to_string(),
                &git_oid.to_hex(),
                &tree.to_string(),
                &parent_strs,
                project,
                created_at_ms,
            )
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        Ok(Digest::parse(&out)?)
    }

    fn get_snapshot(&self, id: &Digest) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        let row = self
            .pg
            .get_snapshot(&id.to_string())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        row.map(|r| {
            let parents = r
                .parents
                .iter()
                .map(|p| Digest::parse(p))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LedgerSnapshot {
                id: Digest::parse(&r.id)?,
                git_oid: GitOid::parse_hex(&r.git_oid)?,
                tree: Digest::parse(&r.tree)?,
                parents,
                project: r.project,
                created_at_ms: r.created_at_ms,
            })
        })
        .transpose()
    }

    fn get_snapshot_by_oid(
        &self,
        git_oid: &GitOid,
    ) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        let row = self
            .pg
            .get_snapshot_by_oid(&git_oid.to_hex())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        row.map(|r| {
            let parents = r
                .parents
                .iter()
                .map(|p| Digest::parse(p))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LedgerSnapshot {
                id: Digest::parse(&r.id)?,
                git_oid: GitOid::parse_hex(&r.git_oid)?,
                tree: Digest::parse(&r.tree)?,
                parents,
                project: r.project,
                created_at_ms: r.created_at_ms,
            })
        })
        .transpose()
    }

    fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: D6RefKind,
        expected: Option<&Digest>,
        new_snapshot: &Digest,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        let exp_str = expected.map(Digest::to_string);
        let new_str = new_snapshot.to_string();
        let seq = self
            .pg
            .cas_ref(
                project,
                name,
                to_pg_kind(kind),
                exp_str.as_deref(),
                &new_str,
                reason,
            )
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        u64::try_from(seq).map_err(|_| D6StoreError::Ledger("negative seq".to_string()))
    }

    fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        let exp_str = expected_base.map(Digest::to_string);
        let new_str = new_snapshot.to_string();
        let seq = self
            .pg
            .land_main_ref(project, name, exp_str.as_deref(), &new_str, task_id, reason)
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        u64::try_from(seq).map_err(|_| D6StoreError::Ledger("negative seq".to_string()))
    }

    fn list_refs(&self, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError> {
        let rows = self
            .pg
            .list_refs(project)
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                Ok(LedgerRef {
                    project: r.project,
                    name: r.name,
                    kind: from_pg_kind(r.kind),
                    snapshot: Digest::parse(&r.snapshot)?,
                    updated_at_ms: r.updated_at_ms,
                })
            })
            .collect()
    }
}

fn setup_accepted_task(
    store: &PostgresStore,
    source: &str,
    base_oid: &GitOid,
    candidate_oid: &GitOid,
) -> Result<i64, String> {
    let mission = store
        .mission_add("D6 benchmark gate", &json!([source]), "human")
        .map_err(|e| format!("mission_add: {e}"))?;
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .map_err(|e| format!("mission_configure: {e}"))?;
    let task = store
        .task_assign(
            mission,
            "ephemeral",
            "w_bench",
            "land benchmark main",
            "default",
        )
        .map_err(|e| format!("task_assign: {e}"))?;
    store
        .task_delivered(task, 1)
        .map_err(|e| format!("task_delivered: {e}"))?;
    let _ = store
        .task_reported(task, "done")
        .map_err(|e| format!("task_reported: {e}"))?;
    store
        .integrator_record(&json!({
            "type": "task.gated",
            "id": task,
            "revision": 0,
            "verdict": "accepted",
            "base": base_oid.to_hex(),
            "candidate": candidate_oid.to_hex(),
        }))
        .map_err(|e| format!("integrator_record: {e}"))?;
    Ok(task)
}

fn d6_seed_initial_main(
    store: &D6Workspace<PgRefLedger>,
    ledger: &PgRefLedger,
    cas: &CasMerkleStore,
    project: &str,
) -> Result<LedgerSnapshot, String> {
    let dummy = BranchName::new("agent_bench_bootstrap").map_err(|e| e.to_string())?;
    let _ = store
        .ensure_branch(&dummy, None)
        .map_err(|e| e.to_string())?;
    let main0 = store
        .main_snapshot()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "bootstrap main missing".to_string())?;

    let (gi_d, gi_len) = cas.put_bytes_blob(b"*.tmp\n").map_err(|e| e.to_string())?;
    let (rd_d, rd_len) = cas
        .put_bytes_blob(b"benchmark base main\n")
        .map_err(|e| e.to_string())?;
    let tree = cas
        .put_tree_cached(
            &spawn_cas::Tree::from_entries(vec![
                spawn_cas::TreeEntry::file(b".gitignore".to_vec(), &gi_d, gi_len),
                spawn_cas::TreeEntry::file(b"README.md".to_vec(), &rd_d, rd_len),
            ])
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    let body = format_commit_body(
        "spawn <bench@localhost> 1700000010 +0000",
        "spawn <bench@localhost> 1700000010 +0000",
        "benchmark: seed main\n",
    );
    let snap = Snapshot {
        tree: tree.clone(),
        parents: vec![main0.id.clone()],
        commit_body: body,
    };
    let snap_id = cas
        .cas()
        .put_merkle_snapshot(&snap)
        .map_err(|e| e.to_string())?;
    let oid = cas
        .project_snapshot_oid(&snap, &[main0.git_oid])
        .map_err(|e| e.to_string())?;
    let _ = ledger
        .record_snapshot(
            &snap_id,
            &oid,
            &tree,
            &snap.parents,
            project,
            1_700_000_010_000,
        )
        .map_err(|e| e.to_string())?;
    let task_id = setup_accepted_task(&ledger.pg, "/tmp/d6-bench-seed.git", &main0.git_oid, &oid)?;
    let _ = store
        .land_main_snapshot(Some(&main0.id), &snap_id, Some(task_id), "seed main")
        .map_err(|e| e.to_string())?;
    store
        .main_snapshot()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "seeded main vanished".to_string())
}

fn d6_advance_main_step(
    store: &D6Workspace<PgRefLedger>,
    ledger: &PgRefLedger,
    cas: &CasMerkleStore,
    project: &str,
    step: usize,
) -> Result<(), String> {
    let main_now = store
        .main_snapshot()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "main snapshot missing".to_string())?;
    let (bd, blen) = cas
        .put_bytes_blob(format!("merge step {step}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let next_tree = cas
        .apply_mutations(
            &main_now.tree,
            &[spawn_cas::TreeMutation::Upsert {
                path: vec![
                    b"merge_bench".to_vec(),
                    format!("step_{step}.txt").into_bytes(),
                ],
                kind: spawn_cas::TreeEntryKind::File,
                id: bd.as_bytes(),
                size: blen,
            }],
        )
        .map_err(|e| e.to_string())?;
    let ts_ms = 1_700_000_100_000_i64 + i64::try_from(step).unwrap_or(0) * 1_000;
    let body = format_commit_body(
        "spawn <bench@localhost> 1700000100 +0000",
        "spawn <bench@localhost> 1700000100 +0000",
        &format!("main step {step}\n"),
    );
    let snap = Snapshot {
        tree: next_tree.clone(),
        parents: vec![main_now.id.clone()],
        commit_body: body,
    };
    let snap_id = cas
        .cas()
        .put_merkle_snapshot(&snap)
        .map_err(|e| e.to_string())?;
    let oid = cas
        .project_snapshot_oid(&snap, &[main_now.git_oid])
        .map_err(|e| e.to_string())?;
    let _ = ledger
        .record_snapshot(&snap_id, &oid, &next_tree, &snap.parents, project, ts_ms)
        .map_err(|e| e.to_string())?;
    let task_id = setup_accepted_task(
        &ledger.pg,
        &format!("/tmp/d6-bench-merge-{step}.git"),
        &main_now.git_oid,
        &oid,
    )?;
    let _ = store
        .land_main_snapshot(
            Some(&main_now.id),
            &snap_id,
            Some(task_id),
            "bench merge advance",
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

struct WorkerState {
    branch: BranchName,
    workspace: WorkspaceId,
    archive: LocalTranscriptArchive,
    agent: AgentId,
    transcript_offset: u64,
    transcript_payload: Vec<u8>,
    database: Option<PostgresStore>,
    single_worker: bool,
    last_database_event: Option<i64>,
    samples: Samples,
    cycle_times: Vec<f64>,
}

impl WorkerState {
    fn new(
        idx: usize,
        config: &Config,
        archive_root: &Path,
        branch: BranchName,
        workspace: WorkspaceId,
        database: Option<PostgresStore>,
    ) -> Result<Self, String> {
        let agent =
            AgentId::new(format!("agent_storage_benchmark_{idx}")).map_err(|e| e.to_string())?;
        let mut transcript_payload = vec![b't'; config.transcript_bytes];
        transcript_payload[config.transcript_bytes - 1] = b'\n';
        Ok(Self {
            branch,
            workspace,
            archive: LocalTranscriptArchive::new(archive_root.join(format!("w{idx}"))),
            agent,
            transcript_offset: 0,
            transcript_payload,
            database,
            single_worker: config.concurrency == 1,
            last_database_event: None,
            samples: Samples::default(),
            cycle_times: Vec::with_capacity(config.iterations),
        })
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
        let appended = u64::try_from(self.transcript_payload.len())
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
        let read_bytes = reading["before"]["archive_bytes"].as_u64();
        let read_valid = if self.single_worker {
            read_bytes == Some(self.transcript_offset)
        } else {
            read_bytes.is_some_and(|b| b > 0)
        };
        if !read_valid {
            return Err(format!(
                "cycle {cycle}: the database read did not return the latest storage sample"
            ));
        }
        if measured {
            self.samples.push("postgres_read", elapsed);
        }
        Ok(())
    }

    fn verify_transcript(&self) -> Result<(), String> {
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
            || restored.get(restored.len() - self.transcript_payload.len()..)
                != Some(self.transcript_payload.as_slice())
        {
            return Err("the transcript archive failed its final content check".to_string());
        }
        Ok(())
    }
}

trait BenchDriver: Send + Sync {
    fn seed_branch(
        &self,
        idx: usize,
        branch: &BranchName,
        config: &Config,
    ) -> Result<WorkspaceId, String>;
    fn commit(&self, ws: &WorkspaceId, msg: &str) -> Result<Commit, String>;
    fn drop_for_recreate(&self, branch: &BranchName) -> Result<(), String>;
    fn remount(
        &self,
        ws: &WorkspaceId,
        branch: &BranchName,
    ) -> Result<(WorkspaceId, Duration, Duration), String>;
    fn merge_base(&self, ws: &WorkspaceId) -> Result<BaseMerge, String>;
    fn after_checkpoint(&self, branch: &BranchName, config: &Config) -> Result<(), String>;
    fn advance_main(&self, step: usize) -> Result<(), String>;
    fn verify_final(&self, branches: &[BranchName], config: &Config) -> Result<(), String>;
    fn disk_breakdown(&self, scratch: &Scratch, active_paths: &[PathBuf]) -> Result<Value, String>;
    fn extra_totals(&self, config: &Config) -> Value;
    fn shadow_metrics_json(&self) -> Option<Value> {
        None
    }
}

struct GitDriver {
    repo: PathBuf,
    stage_seed: PathBuf,
    workspaces_root: PathBuf,
    store: GitWorkspace,
}

impl BenchDriver for GitDriver {
    fn seed_branch(
        &self,
        _idx: usize,
        branch: &BranchName,
        config: &Config,
    ) -> Result<WorkspaceId, String> {
        let _ = self
            .store
            .ensure_branch(branch, None)
            .map_err(|e| e.to_string())?;
        let ws = self.store.mount(branch).map_err(|e| e.to_string())?;
        seed_workspace(Path::new(ws.path()), config)?;
        if !matches!(
            self.store
                .commit(&ws, "benchmark: seed")
                .map_err(|e| e.to_string())?,
            Commit::Created(_)
        ) {
            return Err("seeded workspace produced no commit".to_string());
        }
        assert_git_oversized_excluded(&self.repo, branch, config.oversized_file_bytes.is_some())?;
        Ok(ws)
    }

    fn commit(&self, ws: &WorkspaceId, msg: &str) -> Result<Commit, String> {
        self.store.commit(ws, msg).map_err(|e| e.to_string())
    }

    fn drop_for_recreate(&self, branch: &BranchName) -> Result<(), String> {
        drop_git_branch(&self.repo, branch)
    }

    fn remount(
        &self,
        ws: &WorkspaceId,
        branch: &BranchName,
    ) -> Result<(WorkspaceId, Duration, Duration), String> {
        let t0 = Instant::now();
        self.store.unmount(ws).map_err(|e| e.to_string())?;
        let u_dur = t0.elapsed();
        let t1 = Instant::now();
        let next = self.store.mount(branch).map_err(|e| e.to_string())?;
        let m_dur = t1.elapsed();
        Ok((next, u_dur, m_dur))
    }

    fn merge_base(&self, ws: &WorkspaceId) -> Result<BaseMerge, String> {
        self.store.merge_base(ws).map_err(|e| e.to_string())
    }

    fn after_checkpoint(&self, branch: &BranchName, config: &Config) -> Result<(), String> {
        assert_git_oversized_excluded(&self.repo, branch, config.oversized_file_bytes.is_some())
    }

    fn advance_main(&self, step: usize) -> Result<(), String> {
        let _ = advance_git_main(&self.repo, &self.stage_seed, step)?;
        Ok(())
    }

    fn verify_final(&self, _branches: &[BranchName], _config: &Config) -> Result<(), String> {
        check_git_repository(&self.repo)
    }

    fn disk_breakdown(&self, scratch: &Scratch, active_paths: &[PathBuf]) -> Result<Value, String> {
        let repo = directory_size(&self.repo)?;
        let ws_root = directory_size(&self.workspaces_root)?;
        let transcripts = directory_size(&scratch.root.join(".transcripts"))?;
        let mut active_ws = DirStats::default();
        for p in active_paths {
            let s = directory_size(p)?;
            active_ws.bytes = active_ws.bytes.saturating_add(s.bytes);
            active_ws.files = active_ws.files.saturating_add(s.files);
        }
        let total = repo
            .bytes
            .saturating_add(ws_root.bytes)
            .saturating_add(transcripts.bytes);
        Ok(json!({
            "repository": repo.to_json(),
            "workspace": active_ws.to_json(),
            "workspaces_root": ws_root.to_json(),
            "cas": DirStats::default().to_json(),
            "layer0": DirStats::default().to_json(),
            "l1_tmpfs": DirStats::default().to_json(),
            "transcripts": transcripts.to_json(),
            "total_bytes": total,
        }))
    }

    fn extra_totals(&self, config: &Config) -> Value {
        json!({
            "git_fsck": "passed",
            "integrity": "passed",
            "oversized_ingested": false,
            "oversized_present_requested": config.oversized_file_bytes.is_some(),
        })
    }
}

struct D6Driver {
    project: String,
    disk_root: PathBuf,
    cas_root: PathBuf,
    cas: Arc<CasMerkleStore>,
    coord_ledger: Arc<PgRefLedger>,
    coord_store: D6Workspace<PgRefLedger>,
    worker_stores: Vec<D6Workspace<PgRefLedger>>,
    oversized_ingested: bool,
}

impl D6Driver {
    fn new(
        project: String,
        disk_root: PathBuf,
        cas_root: PathBuf,
        cas: Arc<CasMerkleStore>,
        bench_url: &str,
        schema: &str,
        config: &Config,
    ) -> Result<Self, String> {
        let make_ws = |ledger: Arc<PgRefLedger>| {
            let mut ws = D6Workspace::new(&project, &disk_root, cas.clone(), ledger);
            if config.oversized_file_bytes.is_some() {
                ws = ws.with_max_file_bytes(u64::MAX);
            }
            if let Some(t) = &config.tmpfs_root {
                ws = ws.with_l1_policy(L1TmpfsPolicy::new(t, config.tmpfs_budget_mb));
            }
            ws
        };
        let coord_ledger = Arc::new(PgRefLedger::connect_schema(bench_url, schema)?);
        let coord_store = make_ws(coord_ledger.clone());
        let _ = d6_seed_initial_main(&coord_store, &coord_ledger, &cas, &project)?;

        let mut worker_stores = Vec::with_capacity(config.concurrency);
        for _ in 0..config.concurrency {
            let l = Arc::new(PgRefLedger::connect_schema(bench_url, schema)?);
            worker_stores.push(make_ws(l));
        }

        Ok(Self {
            project,
            disk_root,
            cas_root,
            cas,
            coord_ledger,
            coord_store,
            worker_stores,
            oversized_ingested: config.oversized_file_bytes.is_some(),
        })
    }

    fn verify_oversized_chunked_in_cas(
        &self,
        branch: &BranchName,
        expected_size: usize,
    ) -> Result<(), String> {
        let head = self
            .coord_store
            .branch_snapshot(branch.as_str())
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("branch {branch} missing snapshot"))?;
        let flat = self
            .cas
            .flatten_tree(&head.tree)
            .map_err(|e| e.to_string())?;
        let ent = flat
            .get(b"oversized.bin".as_slice())
            .ok_or_else(|| "D6 checkpoint omitted oversized.bin".to_string())?;
        let want_size = u64::try_from(expected_size).map_err(|_| "size overflow".to_string())?;
        if ent.size != want_size {
            return Err(format!("oversized.bin size {} != {want_size}", ent.size));
        }
        let digest = ent
            .digest()
            .ok_or_else(|| "oversized.bin missing digest".to_string())?;
        if expected_size > 4 * 1024 * 1024 && !self.cas.cas().chunk_index_path(&digest).is_file() {
            return Err("oversized.bin was not FastCDC chunked into chunk-index".to_string());
        }
        let projected_blob = spawn_cas::project_blob_oid(self.cas.cas(), &digest, want_size)
            .map_err(|e| e.to_string())?;
        let expected_pointer = git_blob_oid_of_bytes(&git_pointer_bytes(&digest, want_size));
        if projected_blob != expected_pointer {
            return Err(format!(
                "oversized.bin OID {projected_blob} != pointer OID {expected_pointer}"
            ));
        }
        Ok(())
    }
}

impl BenchDriver for D6Driver {
    fn seed_branch(
        &self,
        idx: usize,
        branch: &BranchName,
        config: &Config,
    ) -> Result<WorkspaceId, String> {
        let store = &self.worker_stores[idx];
        let _ = store
            .ensure_branch(branch, None)
            .map_err(|e| e.to_string())?;
        let ws = store.mount(branch).map_err(|e| e.to_string())?;
        seed_workspace(Path::new(ws.path()), config)?;
        if !matches!(
            store
                .commit(&ws, "benchmark: seed")
                .map_err(|e| e.to_string())?,
            Commit::Created(_)
        ) {
            return Err("seeded D6 workspace produced no commit".to_string());
        }
        if let Some(sz) = config.oversized_file_bytes {
            self.verify_oversized_chunked_in_cas(branch, sz)?;
        }
        Ok(ws)
    }

    fn commit(&self, ws: &WorkspaceId, msg: &str) -> Result<Commit, String> {
        let idx = worker_index_from_branch(ws.sandbox());
        self.worker_stores[idx]
            .commit(ws, msg)
            .map_err(|e| e.to_string())
    }

    fn drop_for_recreate(&self, branch: &BranchName) -> Result<(), String> {
        Err(format!(
            "cannot drop append-only D6 ref {branch} during benchmark"
        ))
    }

    fn remount(
        &self,
        ws: &WorkspaceId,
        branch: &BranchName,
    ) -> Result<(WorkspaceId, Duration, Duration), String> {
        let idx = worker_index_from_branch(branch.as_str());
        let store = &self.worker_stores[idx];
        let t0 = Instant::now();
        store.unmount(ws).map_err(|e| e.to_string())?;
        let u_dur = t0.elapsed();
        let t1 = Instant::now();
        let next = store.mount(branch).map_err(|e| e.to_string())?;
        let m_dur = t1.elapsed();
        Ok((next, u_dur, m_dur))
    }

    fn merge_base(&self, ws: &WorkspaceId) -> Result<BaseMerge, String> {
        let idx = worker_index_from_branch(ws.sandbox());
        self.worker_stores[idx]
            .merge_base(ws)
            .map_err(|e| e.to_string())
    }

    fn after_checkpoint(&self, branch: &BranchName, config: &Config) -> Result<(), String> {
        if let Some(sz) = config.oversized_file_bytes {
            self.verify_oversized_chunked_in_cas(branch, sz)?;
        }
        Ok(())
    }

    fn advance_main(&self, step: usize) -> Result<(), String> {
        d6_advance_main_step(
            &self.coord_store,
            &self.coord_ledger,
            &self.cas,
            &self.project,
            step,
        )
    }

    fn verify_final(&self, branches: &[BranchName], config: &Config) -> Result<(), String> {
        for b in branches {
            if let Some(sz) = config.oversized_file_bytes {
                self.verify_oversized_chunked_in_cas(b, sz)?;
            }
            let head = self
                .coord_store
                .branch_snapshot(b.as_str())
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("missing branch head for {b}"))?;
            if !self.cas.cas().has(&head.id) {
                return Err(format!("snapshot {} missing in CAS", head.id));
            }
        }
        let journal = self
            .cas
            .cas()
            .read_ref_journal()
            .map_err(|e| e.to_string())?;
        if journal.is_empty() {
            return Err("refs.journal is empty after D6 benchmark".to_string());
        }
        Ok(())
    }

    fn disk_breakdown(&self, scratch: &Scratch, active_paths: &[PathBuf]) -> Result<Value, String> {
        let cas = directory_size(&self.cas_root)?;
        let layer0 = directory_size(&self.disk_root.join(".d6/layers"))?;
        let disk_ws_root = directory_size(&self.disk_root)?;
        let l1_tmpfs = match &scratch.tmpfs_root {
            Some(p) => directory_size(p)?,
            None => DirStats::default(),
        };
        let transcripts = directory_size(&scratch.root.join(".transcripts"))?;
        let mut active_ws = DirStats::default();
        for p in active_paths {
            let s = directory_size(p)?;
            active_ws.bytes = active_ws.bytes.saturating_add(s.bytes);
            active_ws.files = active_ws.files.saturating_add(s.files);
        }
        let total = cas
            .bytes
            .saturating_add(disk_ws_root.bytes)
            .saturating_add(l1_tmpfs.bytes)
            .saturating_add(transcripts.bytes);
        Ok(json!({
            "repository": DirStats::default().to_json(),
            "workspace": active_ws.to_json(),
            "workspaces_root": disk_ws_root.to_json(),
            "cas": cas.to_json(),
            "layer0": layer0.to_json(),
            "l1_tmpfs": l1_tmpfs.to_json(),
            "transcripts": transcripts.to_json(),
            "total_bytes": total,
        }))
    }

    fn extra_totals(&self, _config: &Config) -> Value {
        let l1_reserved = self.worker_stores.iter().fold(0_u64, |acc, s| {
            acc.saturating_add(s.l1_total_reserved_bytes())
        });
        json!({
            "integrity": "passed",
            "oversized_ingested": self.oversized_ingested,
            "oversized_chunked": self.oversized_ingested,
            "l1_reserved_bytes": l1_reserved,
        })
    }
}

struct ShadowDriver {
    repo: PathBuf,
    stage_seed: PathBuf,
    git_root: PathBuf,
    d6_root: PathBuf,
    cas_root: PathBuf,
    cas: Arc<CasMerkleStore>,
    coord_ledger: Arc<PgRefLedger>,
    oracles: Vec<ShadowD6Workspace<GitWorkspace, PgRefLedger>>,
}

impl ShadowDriver {
    #[expect(clippy::too_many_arguments)]
    fn new(
        repo: PathBuf,
        stage_seed: PathBuf,
        git_root: PathBuf,
        d6_root: PathBuf,
        cas_root: PathBuf,
        cas: Arc<CasMerkleStore>,
        bench_url: &str,
        schema: &str,
        config: &Config,
    ) -> Result<Self, String> {
        let mut oracles = Vec::with_capacity(config.concurrency);
        let coord_ledger = Arc::new(PgRefLedger::connect_schema(bench_url, schema)?);
        for _ in 0..config.concurrency {
            let mut primary = GitWorkspace::new(&repo, &git_root);
            if config.oversized_file_bytes.is_some() {
                let cap = u64::try_from(config.file_bytes)
                    .map_err(|_| "file_bytes overflow".to_string())?;
                primary = primary.with_max_file_bytes(cap);
            }
            let ledger = Arc::new(PgRefLedger::connect_schema(bench_url, schema)?);
            let mut shadow = D6Workspace::new(
                repo.to_string_lossy().into_owned(),
                &d6_root,
                cas.clone(),
                ledger,
            );
            if config.oversized_file_bytes.is_some() {
                let cap = u64::try_from(config.file_bytes)
                    .map_err(|_| "file_bytes overflow".to_string())?;
                shadow = shadow.with_max_file_bytes(cap);
            }
            if let Some(t) = &config.tmpfs_root {
                shadow = shadow.with_l1_policy(L1TmpfsPolicy::new(t, config.tmpfs_budget_mb));
            }
            let oracle = ShadowD6Workspace::new(primary, shadow);
            let _ = oracle.sync_main_from_git(None).map_err(|e| e.to_string())?;
            oracles.push(oracle);
        }
        Ok(Self {
            repo,
            stage_seed,
            git_root,
            d6_root,
            cas_root,
            cas,
            coord_ledger,
            oracles,
        })
    }

    fn aggregate_metrics(&self) -> ShadowOracleMetrics {
        let mut agg = ShadowOracleMetrics::default();
        for o in &self.oracles {
            let m = o.metrics();
            agg.checkpoints_agreed += m.checkpoints_agreed;
            agg.tree_mismatches += m.tree_mismatches;
            agg.tracked_set_leaks += m.tracked_set_leaks;
            agg.shadow_errors += m.shadow_errors;
            agg.merge_summary.total += m.merge_summary.total;
            agg.merge_summary.agree_clean += m.merge_summary.agree_clean;
            agg.merge_summary.agree_conflict += m.merge_summary.agree_conflict;
            agg.merge_summary.blockers += m.merge_summary.blockers;
            agg.merge_summary.observed_non_exact_rename +=
                m.merge_summary.observed_non_exact_rename;
            agg.merge_summary.rejected_multi_merge_base +=
                m.merge_summary.rejected_multi_merge_base;
        }
        agg
    }
}

impl BenchDriver for ShadowDriver {
    fn seed_branch(
        &self,
        idx: usize,
        branch: &BranchName,
        config: &Config,
    ) -> Result<WorkspaceId, String> {
        let oracle = &self.oracles[idx];
        let _ = oracle
            .ensure_branch(branch, None)
            .map_err(|e| e.to_string())?;
        let ws = oracle.mount(branch).map_err(|e| e.to_string())?;
        seed_workspace(Path::new(ws.path()), config)?;
        if !matches!(
            oracle
                .commit(&ws, "benchmark: seed")
                .map_err(|e| e.to_string())?,
            Commit::Created(_)
        ) {
            return Err("seeded shadow workspace produced no commit".to_string());
        }
        assert_git_oversized_excluded(&self.repo, branch, config.oversized_file_bytes.is_some())?;
        Ok(ws)
    }

    fn commit(&self, ws: &WorkspaceId, msg: &str) -> Result<Commit, String> {
        let idx = worker_index_from_branch(ws.sandbox());
        self.oracles[idx].commit(ws, msg).map_err(|e| e.to_string())
    }

    fn drop_for_recreate(&self, branch: &BranchName) -> Result<(), String> {
        drop_git_branch(&self.repo, branch)
    }

    fn remount(
        &self,
        ws: &WorkspaceId,
        branch: &BranchName,
    ) -> Result<(WorkspaceId, Duration, Duration), String> {
        let idx = worker_index_from_branch(branch.as_str());
        let oracle = &self.oracles[idx];
        let t0 = Instant::now();
        oracle.unmount(ws).map_err(|e| e.to_string())?;
        let u_dur = t0.elapsed();
        let t1 = Instant::now();
        let next = oracle.mount(branch).map_err(|e| e.to_string())?;
        let m_dur = t1.elapsed();
        Ok((next, u_dur, m_dur))
    }

    fn merge_base(&self, ws: &WorkspaceId) -> Result<BaseMerge, String> {
        let idx = worker_index_from_branch(ws.sandbox());
        self.oracles[idx].merge_base(ws).map_err(|e| e.to_string())
    }

    fn after_checkpoint(&self, branch: &BranchName, config: &Config) -> Result<(), String> {
        assert_git_oversized_excluded(&self.repo, branch, config.oversized_file_bytes.is_some())
    }

    fn advance_main(&self, step: usize) -> Result<(), String> {
        let (prev, next) = advance_git_main(&self.repo, &self.stage_seed, step)?;
        let task_id = setup_accepted_task(
            &self.coord_ledger.pg,
            &format!("/tmp/shadow-bench-{step}.git"),
            &prev,
            &next,
        )?;
        let _ = self.oracles[0]
            .sync_main_from_git(Some(task_id))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn verify_final(&self, _branches: &[BranchName], _config: &Config) -> Result<(), String> {
        check_git_repository(&self.repo)?;
        let m = self.aggregate_metrics();
        if m.has_divergences() {
            return Err(format!(
                "shadow oracle reported divergences: tree_mismatches={}, tracked_set_leaks={}, shadow_errors={}, blockers={}",
                m.tree_mismatches, m.tracked_set_leaks, m.shadow_errors, m.merge_summary.blockers
            ));
        }
        let _ = &self.cas;
        Ok(())
    }

    fn disk_breakdown(&self, scratch: &Scratch, active_paths: &[PathBuf]) -> Result<Value, String> {
        let repo = directory_size(&self.repo)?;
        let git_ws = directory_size(&self.git_root)?;
        let cas = directory_size(&self.cas_root)?;
        let layer0 = directory_size(&self.d6_root.join(".d6/layers"))?;
        let d6_ws = directory_size(&self.d6_root)?;
        let l1_tmpfs = match &scratch.tmpfs_root {
            Some(p) => directory_size(p)?,
            None => DirStats::default(),
        };
        let transcripts = directory_size(&scratch.root.join(".transcripts"))?;
        let mut active_ws = DirStats::default();
        for p in active_paths {
            let s = directory_size(p)?;
            active_ws.bytes = active_ws.bytes.saturating_add(s.bytes);
            active_ws.files = active_ws.files.saturating_add(s.files);
        }
        let total = repo
            .bytes
            .saturating_add(git_ws.bytes)
            .saturating_add(cas.bytes)
            .saturating_add(d6_ws.bytes)
            .saturating_add(l1_tmpfs.bytes)
            .saturating_add(transcripts.bytes);
        Ok(json!({
            "repository": repo.to_json(),
            "workspace": active_ws.to_json(),
            "workspaces_root": git_ws.to_json(),
            "cas": cas.to_json(),
            "layer0": layer0.to_json(),
            "l1_tmpfs": l1_tmpfs.to_json(),
            "transcripts": transcripts.to_json(),
            "total_bytes": total,
        }))
    }

    fn extra_totals(&self, config: &Config) -> Value {
        json!({
            "git_fsck": "passed",
            "integrity": "passed",
            "oversized_ingested": false,
            "oversized_present_requested": config.oversized_file_bytes.is_some(),
        })
    }

    fn shadow_metrics_json(&self) -> Option<Value> {
        let m = self.aggregate_metrics();
        Some(json!({
            "checkpoints_agreed": m.checkpoints_agreed,
            "tree_mismatches": m.tree_mismatches,
            "tracked_set_leaks": m.tracked_set_leaks,
            "shadow_errors": m.shadow_errors,
            "merge_agree_clean": m.merge_summary.agree_clean,
            "merge_agree_conflict": m.merge_summary.agree_conflict,
            "merge_blockers": m.merge_summary.blockers,
            "has_divergences": m.has_divergences(),
        }))
    }
}

fn worker_branch_name(idx: usize, total: usize) -> Result<BranchName, String> {
    let raw = if total == 1 {
        "agent_storage_benchmark".to_string()
    } else {
        format!("agent_storage_benchmark_{idx}")
    };
    BranchName::new(raw).map_err(|e| e.to_string())
}

fn worker_index_from_branch(branch: &str) -> usize {
    branch
        .rsplit('_')
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
}

fn run_worker_cycles(
    driver: &dyn BenchDriver,
    config: &Config,
    worker: &mut WorkerState,
) -> Result<(), String> {
    let total_cycles = config
        .warmup
        .checked_add(config.iterations)
        .ok_or_else(|| "warmup plus iterations is too large".to_string())?;
    let progress_every = (config.iterations / 10).max(1);
    for cycle in 0..total_cycles {
        let measured = cycle >= config.warmup;
        let cycle_clock = Instant::now();

        edit_workspace(Path::new(worker.workspace.path()), config, cycle)?;
        let recreate =
            config.recreate_every > 0 && (cycle + 1).is_multiple_of(config.recreate_every);
        if recreate {
            driver.drop_for_recreate(&worker.branch)?;
        }

        let ck0 = Instant::now();
        let committed = driver.commit(&worker.workspace, &format!("benchmark: cycle {cycle}"))?;
        let ck_dur = ck0.elapsed();
        let valid = if recreate {
            matches!(committed, Commit::Recreated { .. })
        } else {
            matches!(committed, Commit::Created(_))
        };
        if !valid {
            return Err(format!(
                "cycle {cycle}: checkpoint result {committed:?} mismatch recreate={recreate}"
            ));
        }
        driver.after_checkpoint(&worker.branch, config)?;
        if measured {
            worker.samples.push("checkpoint_changed", ck_dur);
        }

        let ck1 = Instant::now();
        let noop = driver.commit(&worker.workspace, "benchmark: unchanged")?;
        let noop_dur = ck1.elapsed();
        if !matches!(noop, Commit::Unchanged) {
            return Err(format!("cycle {cycle}: noop checkpoint returned {noop:?}"));
        }
        driver.after_checkpoint(&worker.branch, config)?;
        if measured {
            worker.samples.push("checkpoint_noop", noop_dur);
        }

        worker.append_transcript(measured)?;
        worker.database_cycle(cycle, measured)?;

        if config.remount_every > 0 && (cycle + 1).is_multiple_of(config.remount_every) {
            let (next_ws, u_dur, m_dur) = driver.remount(&worker.workspace, &worker.branch)?;
            worker.workspace = next_ws;
            if measured {
                worker.samples.push("unmount", u_dur);
                worker.samples.push("mount", m_dur);
            }
        }

        let elapsed = cycle_clock.elapsed();
        if measured {
            worker.cycle_times.push(elapsed.as_secs_f64() * 1000.0);
            let completed = cycle - config.warmup + 1;
            if completed.is_multiple_of(progress_every)
                && (worker.branch.as_str().ends_with("benchmark")
                    || worker.branch.as_str().ends_with("_0"))
            {
                eprintln!(
                    "storage benchmark: {completed}/{} measured cycles",
                    config.iterations
                );
            }
        }
    }
    Ok(())
}

fn build_driver(
    config: &Config,
    scratch: &Scratch,
    bench_url: Option<&str>,
    schema: &str,
) -> Result<Arc<dyn BenchDriver>, String> {
    match config.backend {
        Backend::Git => {
            let repo = scratch.root.join("store.git");
            let stage_seed = scratch.root.join(".seed-git");
            let workspaces_root = scratch.root.join("workspaces");
            fs::create_dir(&workspaces_root).map_err(|e| format!("create workspace root: {e}"))?;
            let _ = initialize_repository_with_main(&repo, &stage_seed)?;
            let mut store = GitWorkspace::new(&repo, &workspaces_root);
            if config.oversized_file_bytes.is_some() {
                store = store.with_max_file_bytes(
                    u64::try_from(config.file_bytes).map_err(|_| "overflow".to_string())?,
                );
            }
            Ok(Arc::new(GitDriver {
                repo,
                stage_seed,
                workspaces_root,
                store,
            }))
        }
        Backend::D6 => {
            let url = bench_url.ok_or_else(|| {
                "SPAWN_BENCH_DATABASE_URL is required for --backend d6".to_string()
            })?;
            let disk_root = scratch.root.join("workspaces");
            let cas_root = scratch.root.join("cas");
            fs::create_dir_all(&disk_root).map_err(|e| e.to_string())?;
            let cas =
                Arc::new(CasMerkleStore::open(&cas_root).map_err(|e| format!("open cas: {e}"))?);
            Ok(Arc::new(D6Driver::new(
                "proj-bench".to_string(),
                disk_root,
                cas_root,
                cas,
                url,
                schema,
                config,
            )?))
        }
        Backend::Shadow => {
            let url = bench_url.ok_or_else(|| {
                "SPAWN_BENCH_DATABASE_URL is required for --backend shadow".to_string()
            })?;
            let repo = scratch.root.join("store.git");
            let stage_seed = scratch.root.join(".seed-git");
            let git_root = scratch.root.join("git-workspaces");
            let d6_root = scratch.root.join("d6-workspaces");
            let cas_root = scratch.root.join("cas");
            fs::create_dir_all(&git_root).map_err(|e| e.to_string())?;
            fs::create_dir_all(&d6_root).map_err(|e| e.to_string())?;
            let _ = initialize_repository_with_main(&repo, &stage_seed)?;
            let cas =
                Arc::new(CasMerkleStore::open(&cas_root).map_err(|e| format!("open cas: {e}"))?);
            Ok(Arc::new(ShadowDriver::new(
                repo, stage_seed, git_root, d6_root, cas_root, cas, url, schema, config,
            )?))
        }
    }
}

fn run(config: &Config) -> Result<Value, String> {
    static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);
    let bench_url = env::var("SPAWN_BENCH_DATABASE_URL").ok();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed);
    let schema = format!(
        "spawn_storage_benchmark_{}_{nanos:x}_{seq}",
        std::process::id()
    );

    let scratch = Scratch::new(config.root.clone(), config.tmpfs_root.clone(), config.keep)?;
    let archive_root = scratch.root.join(".transcripts");
    fs::create_dir_all(&archive_root).map_err(|e| e.to_string())?;

    let driver = build_driver(config, &scratch, bench_url.as_deref(), &schema)?;

    let mut workers = Vec::with_capacity(config.concurrency);
    for idx in 0..config.concurrency {
        let branch = worker_branch_name(idx, config.concurrency)?;
        let ws = driver.seed_branch(idx, &branch, config)?;
        let db = match &bench_url {
            Some(url) => Some(
                PostgresStore::connect_in_schema(url, &schema)
                    .map_err(|e| format!("connect worker pg: {e}"))?,
            ),
            None => None,
        };
        workers.push(WorkerState::new(
            idx,
            config,
            &archive_root,
            branch,
            ws,
            db,
        )?);
    }

    let wall = Instant::now();
    let mut final_workers = run_checkpoint_phase(&driver, config, workers)?;
    run_merge_phase(&driver, config, &mut final_workers)?;
    let wall_ms = wall.elapsed().as_secs_f64() * 1000.0;

    finalize_and_build_report(
        &driver,
        config,
        &scratch,
        &final_workers,
        wall_ms,
        bench_url.is_some(),
    )
}

fn finalize_and_build_report(
    driver: &Arc<dyn BenchDriver>,
    config: &Config,
    scratch: &Scratch,
    final_workers: &[WorkerState],
    wall_ms: f64,
    has_database: bool,
) -> Result<Value, String> {
    let mut branches = Vec::with_capacity(final_workers.len());
    let mut active_paths = Vec::with_capacity(final_workers.len());
    let mut merged_samples = Samples::default();
    let mut all_cycle_times = Vec::with_capacity(config.concurrency * config.iterations);
    let mut total_transcript_bytes = 0_u64;
    let mut last_database_event = None;

    for w in final_workers {
        w.verify_transcript()?;
        branches.push(w.branch.clone());
        active_paths.push(PathBuf::from(w.workspace.path()));
        merged_samples.merge_from(w.samples.clone());
        all_cycle_times.extend_from_slice(&w.cycle_times);
        total_transcript_bytes = total_transcript_bytes.saturating_add(w.transcript_offset);
        if let Some(ev) = w.last_database_event {
            last_database_event = Some(last_database_event.map_or(ev, |cur: i64| cur.max(ev)));
        }
    }
    driver.verify_final(&branches, config)?;

    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system clock: {error}"))
        .and_then(|elapsed| {
            u64::try_from(elapsed.as_millis()).map_err(|_| "clock ms do not fit u64".to_string())
        })?;
    let mut operations = merged_samples.report()?;
    operations["cycle"] = summary(&all_cycle_times)?;
    let cycle_seconds: f64 = all_cycle_times.iter().sum::<f64>() / 1000.0;
    let per_worker_iters = sample_count(config.iterations)?;
    let total_measured_cycles = config.concurrency.saturating_mul(config.iterations);
    let benchmark_binary_sha256 = binary_sha256()?;

    let mut disk = driver.disk_breakdown(scratch, &active_paths)?;
    let logical_workspace_bytes = u64::try_from(
        config
            .files
            .saturating_mul(config.file_bytes)
            .saturating_add(config.oversized_file_bytes.unwrap_or(0))
            .saturating_mul(config.concurrency),
    )
    .unwrap_or(u64::MAX);
    let total_bytes = disk["total_bytes"].as_u64().unwrap_or(0);
    #[expect(clippy::cast_precision_loss)]
    let amp = (total_bytes as f64) / (logical_workspace_bytes.max(1) as f64);
    disk["logical_workspace_bytes"] = json!(logical_workspace_bytes);
    disk["disk_amplification_ratio"] = json!(amp);

    let cfg_json = build_cfg_json(config, has_database);

    let mut totals = json!({
        "wall_ms": wall_ms,
        "measured_cycles_per_second": sample_count(total_measured_cycles)? / cycle_seconds.max(1e-6),
        "aggregate_cycles_per_second": (per_worker_iters * f64::from(u32::try_from(config.concurrency).unwrap_or(1))) / (wall_ms / 1000.0).max(1e-6),
        "total_measured_cycles": total_measured_cycles,
        "transcript_bytes_verified": total_transcript_bytes,
        "last_database_event": last_database_event,
    });
    if let Value::Object(extra) = driver.extra_totals(config)
        && let Value::Object(map) = &mut totals
    {
        for (k, v) in extra {
            map.insert(k, v);
        }
    }

    let mut report = json!({
        "schema_version": 1,
        "benchmark": "spawn-storage",
        "created_at_ms": created_at_ms,
        "config": cfg_json,
        "environment": {
            "backend": config.backend.as_str(),
            "concurrency": config.concurrency,
            "tmpfs_root": config.tmpfs_root,
            "tmpfs_budget_mb": config.tmpfs_root.as_ref().map(|_| config.tmpfs_budget_mb),
            "git_version": git_version(),
            "spawn_source_revision": config.source_revision.as_deref(),
            "benchmark_binary_sha256": benchmark_binary_sha256,
            "scratch_root": &scratch.root,
        },
        "operations": operations,
        "totals": totals,
        "disk": disk,
        "scratch_kept": config.keep,
    });
    if let Some(shadow) = driver.shadow_metrics_json() {
        report["shadow_oracle"] = shadow;
    }
    Ok(report)
}

fn build_cfg_json(config: &Config, has_database: bool) -> Value {
    let mut cfg_json = json!({
        "iterations": config.iterations,
        "warmup": config.warmup,
        "files": config.files,
        "file_bytes": config.file_bytes,
        "edits": config.edits,
        "transcript_bytes": config.transcript_bytes,
        "oversized_file_bytes": config.oversized_file_bytes,
        "recreate_every": config.recreate_every,
        "remount_every": config.remount_every,
        "database": has_database,
    });
    if config.backend != Backend::Git || config.concurrency != 1 || config.tmpfs_root.is_some() {
        cfg_json["backend"] = json!(config.backend.as_str());
        cfg_json["concurrency"] = json!(config.concurrency);
        if config.tmpfs_root.is_some() {
            cfg_json["tmpfs_budget_mb"] = json!(config.tmpfs_budget_mb);
        }
    }
    cfg_json
}

fn run_checkpoint_phase(
    driver: &Arc<dyn BenchDriver>,
    config: &Config,
    workers: Vec<WorkerState>,
) -> Result<Vec<WorkerState>, String> {
    let workers_mutex: Vec<Mutex<Option<WorkerState>>> =
        workers.into_iter().map(|w| Mutex::new(Some(w))).collect();
    let barrier = Arc::new(Barrier::new(config.concurrency));
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(config.concurrency);
        for slot in &workers_mutex {
            let drv = driver.clone();
            let bar = barrier.clone();
            handles.push(scope.spawn(move || -> Result<(), String> {
                let mut w = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("worker slot occupied");
                bar.wait();
                run_worker_cycles(drv.as_ref(), config, &mut w)?;
                *slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(w);
                Ok(())
            }));
        }
        for h in handles {
            h.join()
                .map_err(|_| "benchmark worker panicked".to_string())??;
        }
        Ok::<(), String>(())
    })?;
    Ok(workers_mutex
        .into_iter()
        .map(|s| {
            s.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("worker present")
        })
        .collect())
}

fn run_merge_phase(
    driver: &Arc<dyn BenchDriver>,
    config: &Config,
    workers: &mut [WorkerState],
) -> Result<(), String> {
    let merge_steps = (config.iterations / 10).clamp(2, 6);
    for step in 0..merge_steps {
        driver.advance_main(step)?;
        let m_bar = Arc::new(Barrier::new(config.concurrency));
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(config.concurrency);
            for w in workers.iter_mut() {
                let drv = driver.clone();
                let bar = m_bar.clone();
                handles.push(scope.spawn(move || -> Result<(), String> {
                    bar.wait();
                    let t0 = Instant::now();
                    let res = drv.merge_base(&w.workspace)?;
                    let dur = t0.elapsed();
                    if !matches!(res, BaseMerge::Merged { .. }) {
                        return Err(format!("merge_bench step {step} returned {res:?}"));
                    }
                    w.samples.push("merge_base", dur);
                    Ok(())
                }));
            }
            for h in handles {
                h.join()
                    .map_err(|_| "merge worker panicked".to_string())??;
            }
            Ok::<(), String>(())
        })?;
    }
    Ok(())
}

fn main() {
    match config() {
        Ok(None) => print!("{USAGE}"),
        Ok(Some(config)) => match run(&config) {
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
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

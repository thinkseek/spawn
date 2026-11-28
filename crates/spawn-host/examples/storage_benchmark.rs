//! End-to-end storage benchmark for Spawn's real host adapters (`git`, `d6`, `shadow`).
//!
//! Creates an isolated scratch directory, drives [`GitWorkspace`], pure-Rust [`D6Workspace`],
//! or dual-track [`ShadowD6Workspace`] alongside [`LocalTranscriptArchive`] through their
//! public traits, verifies every invariant, and prints one JSON report.
//!
//! Optionally `SPAWN_BENCH_DATABASE_URL` names a dedicated benchmark PostgreSQL database
//! (required for `--backend d6|shadow`); a unique schema is created and measured. The live
//! `SPAWN_DATABASE_URL` is NEVER read.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spawn_cas::{
    CHUNK_AVG, CHUNK_MAX, Digest, GIT_POINTER_THRESHOLD, GitOid, Snapshot, git_blob_oid_of_bytes,
    git_pointer_bytes, sha256_of,
};
use spawn_core::id::{AgentId, BranchName, WorkspaceId};
use spawn_core::port::{BaseMerge, Commit, TranscriptArchive, WorkspaceStore};
use spawn_host::d6_cutover::reconcile_refs_journal;
use spawn_host::d6_workspace::L1TmpfsPolicy;
use spawn_host::merkle_merge::merge_snapshots;
use spawn_host::{
    CasMerkleStore, D6RefKind, D6StoreError, D6Workspace, GitWorkspace, LedgerRef, LedgerSnapshot,
    LocalTranscriptArchive, RefLedger, ShadowD6Workspace, ShadowOracleMetrics, format_commit_body,
    git_command,
};
use spawn_postgres::{ConnectOptions, PostgresStore, RefKind};

const USAGE: &str = "spawn storage benchmark\n\
\n\
Usage: cargo run --release -p spawn-host --example storage_benchmark -- [options]\n\
\n\
  --root <new-directory>        Scratch root (default: a unique temporary path)\n\
  --source-revision <sha>       Spawn source revision recorded in the report\n\
  --backend <git|d6|shadow>     Storage backend (default: git)\n\
  --concurrency <n>             Concurrent branch workers (default: 1)\n\
  --pg-pool-size <n>            Shared PG connection pool size for d6 (0 = 1/worker)\n\
  --tmpfs-root <directory>      Enable bounded L1 tmpfs upperdir root for d6/shadow\n\
  --tmpfs-budget-mb <n>         L1 tmpfs budget in MiB when --tmpfs-root is set (default: 2048)\n\
  --iterations <n>              Measured cycles per worker (default: 40)\n\
  --warmup <n>                  Warm-up cycles per worker (default: 5)\n\
  --files <n>                   Workspace files (default: 512)\n\
  --file-bytes <n>              Bytes per workspace file (default: 4096)\n\
  --edits <n>                   Files changed before each checkpoint (default: 4)\n\
  --transcript-bytes <n>        Durable transcript append per cycle (default: 16384)\n\
  --oversized-file-bytes <n>    Add a file over the cap (must exceed --file-bytes)\n\
  --recreate-every <n>          Drop the branch before every nth checkpoint; 0 disables\n\
  --remount-every <n>           Unmount/mount every n cycles; 0 disables (default: 0)\n\
  --land-branches               Concurrently 3-way merge and land all worker branches onto main\n\
  --land-payload-bytes <n>      Unique branch deliverable bytes per landing (default: 131072)\n\
  --crash-restart-drill         Run cross-process SIGKILL + torn-journal + reopen drill\n\
  --save-crash-manifest <path>  Write crash manifest JSON for post-PG-restart verification\n\
  --keep                        Keep the scratch root after a successful run\n\
  --help                        Show this help\n\
\n\
SPAWN_BENCH_DATABASE_URL optionally enables PostgreSQL writes (and is required for\n\
--backend d6|shadow). It must name a dedicated benchmark database, never the live store.\n";

const LAND_DELTA_SLO_BYTES: u64 = 50 * 1024 * 1024;
const DEFAULT_LAND_PAYLOAD_BYTES: usize = 128 * 1024;
const BRANCH_INIT_FILES: usize = 4;
const CDC_DEDUP_MIN_RATIO: f64 = 2.0;
const CDC_FIXED_BLOCK_MAX_RATIO: f64 = 1.2;
const TORN_JOURNAL_TAIL: &[u8] =
    b"{\"seq\":99999999,\"project\":\"proj-bench\",\"name\":\"branch/torn";

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
    pg_pool_size: usize,
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
    land_branches: bool,
    land_payload_bytes: usize,
    crash_restart_drill: bool,
    save_crash_manifest: Option<PathBuf>,
    keep: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: None,
            source_revision: None,
            backend: Backend::Git,
            concurrency: 1,
            pg_pool_size: 0,
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
            land_branches: false,
            land_payload_bytes: DEFAULT_LAND_PAYLOAD_BYTES,
            crash_restart_drill: false,
            save_crash_manifest: None,
            keep: false,
        }
    }
}

fn u64_to_f64(value: u64) -> f64 {
    let hi = u32::try_from(value >> 32).unwrap_or(u32::MAX);
    let lo = u32::try_from(value & 0xffff_ffff).unwrap_or(u32::MAX);
    f64::from(hi) * 4_294_967_296.0 + f64::from(lo)
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
        || config.land_payload_bytes == 0
    {
        return Err(
            "concurrency, iterations, files, file-bytes, edits, transcript-bytes and land-payload-bytes must be positive; edits cannot exceed files"
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
    if (config.land_branches || config.crash_restart_drill) && config.backend != Backend::D6 {
        return Err("--land-branches and --crash-restart-drill require --backend d6".to_string());
    }
    if config.save_crash_manifest.is_some() && !config.crash_restart_drill {
        return Err("--save-crash-manifest requires --crash-restart-drill".to_string());
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
    let marker_bytes = format!(
        "// cycle={last_cycle} edit={} worker=999\n",
        config.edits - 1
    )
    .len();
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
            "--pg-pool-size" => config.pg_pool_size = number(&mut arguments, "--pg-pool-size")?,
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
            "--land-branches" => config.land_branches = true,
            "--land-payload-bytes" => {
                config.land_payload_bytes = number(&mut arguments, "--land-payload-bytes")?;
            }
            "--crash-restart-drill" => config.crash_restart_drill = true,
            "--save-crash-manifest" => {
                config.save_crash_manifest =
                    Some(PathBuf::from(arguments.next().ok_or_else(|| {
                        "--save-crash-manifest needs a path".to_string()
                    })?));
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

fn strict_directory_size(root: &Path) -> Result<DirStats, String> {
    let meta =
        fs::symlink_metadata(root).map_err(|error| format!("stat {}: {error}", root.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let mut pending = vec![root.to_path_buf()];
    let mut stats = DirStats::default();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("read directory entry: {error}"))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| format!("stat {}: {error}", path.display()))?;
            if kind.is_symlink() {
                return Err(format!("unexpected symlink in CAS: {}", path.display()));
            }
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                stats.bytes = stats.bytes.saturating_add(
                    entry
                        .metadata()
                        .map_err(|error| format!("stat {}: {error}", path.display()))?
                        .len(),
                );
                stats.files += 1;
            } else {
                return Err(format!("unexpected file type in CAS: {}", path.display()));
            }
        }
    }
    Ok(stats)
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let word = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

/// Generate mixed structured-header + high-entropy PRNG bytes so neither intra-repo
/// files nor oversized FastCDC payloads collapse via single-byte repetition.
fn fill_mixed_payload(seed: u64, marker: &str, size: usize) -> Vec<u8> {
    let mut out = vec![0_u8; size];
    let mut pos = 0_usize;
    let marker_bytes = marker.as_bytes();
    let take_marker = marker_bytes.len().min(size);
    out[..take_marker].copy_from_slice(&marker_bytes[..take_marker]);
    pos += take_marker;

    let mut rng = SplitMix64::new(seed);
    let mut block_idx = 0_u64;
    while pos < size {
        if block_idx.is_multiple_of(3) {
            let header = format!(
                "// block={block_idx:06} seed={seed:016x} token={:016x}\n",
                rng.next_u64()
            );
            let n = header.len().min(size - pos);
            out[pos..pos + n].copy_from_slice(&header.as_bytes()[..n]);
            pos += n;
        } else {
            let len = (size - pos).min(1536);
            rng.fill_bytes(&mut out[pos..pos + len]);
            pos += len;
        }
        block_idx += 1;
    }
    out
}

/// Inject a branch-unique variable-length front shift-edit into `base` so fixed-offset
/// `CHUNK_AVG` blocking loses alignment across branches (`~1.00x`) while FastCDC v1
/// resynchronizes cut points on the unchanged tail (`>= 2.00x`).
fn build_shifted_oversized_payload(base: &[u8], branch_idx: usize) -> Vec<u8> {
    let offset = (4096 + branch_idx * 137).min(base.len());
    let insert_len = 383 + branch_idx * 97;
    let seed = 0x5348_4946_u64 ^ ((branch_idx as u64).wrapping_add(1) << 24);
    let marker = format!("// fastcdc-shift branch={branch_idx} offset={offset}\n");
    let insertion = fill_mixed_payload(seed, &marker, insert_len);

    let mut shifted = Vec::with_capacity(base.len().saturating_add(insertion.len()));
    shifted.extend_from_slice(&base[..offset]);
    shifted.extend_from_slice(&insertion);
    shifted.extend_from_slice(&base[offset..]);
    shifted
}

fn seed_workspace(workspace: &Path, config: &Config) -> Result<u64, String> {
    let mut bytes_written = 0_u64;
    for index in 0..config.files {
        let path = workspace.join(format!("tree/{:04}/{:06}.txt", index % 32, index));
        fs::create_dir_all(path.parent().expect("a seeded file has a parent"))
            .map_err(|error| format!("create {}: {error}", path.display()))?;
        let seed = 0x5345_4544_u64.wrapping_mul((index as u64).wrapping_add(1));
        let marker = format!("// seed index={index}\n");
        let payload = fill_mixed_payload(seed, &marker, config.file_bytes);
        bytes_written =
            bytes_written.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        fs::write(&path, &payload).map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    if let Some(bytes) = config.oversized_file_bytes {
        let payload = fill_mixed_payload(0x4F56_4552_5349_5A45, "BINv1\n", bytes);
        bytes_written =
            bytes_written.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        fs::write(workspace.join("oversized.bin"), payload)
            .map_err(|error| format!("write the oversized benchmark file: {error}"))?;
    }
    Ok(bytes_written)
}

fn edit_workspace(
    workspace: &Path,
    config: &Config,
    worker_idx: usize,
    cycle: usize,
    d6_disjoint: bool,
) -> Result<u64, String> {
    let mut bytes_written = 0_u64;
    for edit in 0..config.edits {
        let slot = (cycle * config.edits + edit) % config.files.max(1);
        let path = if d6_disjoint {
            let dir_slot = slot % 16;
            workspace.join(format!("branches/w{worker_idx:03}/work_{dir_slot:04}.txt"))
        } else {
            workspace.join(format!("tree/{:04}/{:06}.txt", slot % 32, slot))
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let seed = ((worker_idx as u64).wrapping_add(1) << 40)
            ^ ((cycle as u64).wrapping_add(1) << 16)
            ^ ((edit as u64).wrapping_add(1) << 1)
            ^ 0x4544_4954;
        let marker = format!("// cycle={cycle} edit={edit} worker={worker_idx}\n");
        let payload = fill_mixed_payload(seed, &marker, config.file_bytes);
        bytes_written =
            bytes_written.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        fs::write(&path, payload).map_err(|error| format!("edit {}: {error}", path.display()))?;
    }
    Ok(bytes_written)
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
    pg: Arc<PostgresStore>,
}

impl PgRefLedger {
    fn connect_schema(url: &str, schema: &str) -> Result<Self, String> {
        let pg =
            PostgresStore::connect_in_schema_with_options(url, schema, &ConnectOptions::default())
                .map_err(|e| format!("connect benchmark schema {schema}: {e}"))?;
        Ok(Self { pg: Arc::new(pg) })
    }

    fn from_store(pg: Arc<PostgresStore>) -> Self {
        Self { pg }
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
    create_accepted_landing_task(store, mission, "w_bench", base_oid, candidate_oid)
}

fn create_accepted_landing_task(
    store: &PostgresStore,
    mission: i64,
    worker: &str,
    base_oid: &GitOid,
    candidate_oid: &GitOid,
) -> Result<i64, String> {
    let task = store
        .task_assign(
            mission,
            "ephemeral",
            worker,
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
    config: &Config,
) -> Result<(LedgerSnapshot, u64), String> {
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

    let mut owned_blobs: Vec<(Vec<Vec<u8>>, Digest, u64)> =
        Vec::with_capacity(config.files.saturating_add(3));
    owned_blobs.push((vec![b".gitignore".to_vec()], gi_d, gi_len));
    owned_blobs.push((vec![b"README.md".to_vec()], rd_d, rd_len));

    for index in 0..config.files {
        let seed = 0x5345_4544_u64.wrapping_mul((index as u64).wrapping_add(1));
        let marker = format!("// base-main index={index}\n");
        let payload = fill_mixed_payload(seed, &marker, config.file_bytes);
        let (digest, len) = cas.put_bytes_blob(&payload).map_err(|e| e.to_string())?;
        owned_blobs.push((
            vec![
                b"tree".to_vec(),
                format!("{:04}", index % 32).into_bytes(),
                format!("{index:06}.txt").into_bytes(),
            ],
            digest,
            len,
        ));
    }

    if let Some(bytes) = config.oversized_file_bytes {
        let payload = fill_mixed_payload(0x4F56_4552_5349_5A45, "BINv1\n", bytes);
        let (digest, len) = cas.put_bytes_blob(&payload).map_err(|e| e.to_string())?;
        owned_blobs.push((vec![b"oversized.bin".to_vec()], digest, len));
    }

    let seeded_logical_bytes = owned_blobs
        .iter()
        .fold(0_u64, |acc, (_, _, size)| acc.saturating_add(*size));

    let mutations: Vec<spawn_cas::TreeMutation> = owned_blobs
        .iter()
        .map(|(path, digest, size)| spawn_cas::TreeMutation::Upsert {
            path: path.clone(),
            kind: spawn_cas::TreeEntryKind::File,
            id: digest.as_bytes(),
            size: *size,
        })
        .collect();
    let tree = cas
        .apply_mutations(&main0.tree, &mutations)
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
    let head = store
        .main_snapshot()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "seeded main vanished".to_string())?;
    Ok((head, seeded_logical_bytes))
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
    let payload = fill_mixed_payload(
        0x4D41_494E_0000_u64.wrapping_add(step as u64),
        &format!("// merge step {step}\n"),
        512,
    );
    let (bd, blen) = cas.put_bytes_blob(&payload).map_err(|e| e.to_string())?;
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
    idx: usize,
    branch: BranchName,
    workspace: WorkspaceId,
    archive: LocalTranscriptArchive,
    agent: AgentId,
    transcript_offset: u64,
    transcript_payload: Vec<u8>,
    database: Option<Arc<PostgresStore>>,
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
        database: Option<Arc<PostgresStore>>,
    ) -> Result<Self, String> {
        let agent =
            AgentId::new(format!("agent_storage_benchmark_{idx}")).map_err(|e| e.to_string())?;
        let mut transcript_payload = fill_mixed_payload(
            0x5452_414E ^ (idx as u64),
            &format!("// transcript worker={idx}\n"),
            config.transcript_bytes,
        );
        transcript_payload[config.transcript_bytes - 1] = b'\n';
        Ok(Self {
            idx,
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
        let Some(database) = self.database.as_deref() else {
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
    fn post_merge_phase(
        &self,
        _config: &Config,
        _workers: &mut [WorkerState],
    ) -> Result<(), String> {
        Ok(())
    }
    fn verify_final(&self, branches: &[BranchName], config: &Config) -> Result<(), String>;
    fn disk_breakdown(&self, scratch: &Scratch, active_paths: &[PathBuf]) -> Result<Value, String>;
    fn extra_totals(&self, config: &Config) -> Value;
    fn shadow_metrics_json(&self) -> Option<Value> {
        None
    }
    fn record_edit_bytes(&self, bytes: u64);
    fn logical_byte_totals(&self) -> (u64, u64);
    fn always_on_d6_json(
        &self,
        _config: &Config,
        _logical_written_bytes: u64,
    ) -> Result<Option<Value>, String> {
        Ok(None)
    }
}

struct GitDriver {
    repo: PathBuf,
    stage_seed: PathBuf,
    workspaces_root: PathBuf,
    store: GitWorkspace,
    logical_workspace_bytes: AtomicU64,
    logical_written_bytes: AtomicU64,
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
        let written = seed_workspace(Path::new(ws.path()), config)?;
        self.logical_workspace_bytes
            .fetch_add(written, Ordering::Relaxed);
        self.logical_written_bytes
            .fetch_add(written, Ordering::Relaxed);
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

    fn record_edit_bytes(&self, bytes: u64) {
        self.logical_written_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    fn logical_byte_totals(&self) -> (u64, u64) {
        (
            self.logical_workspace_bytes.load(Ordering::Relaxed),
            self.logical_written_bytes.load(Ordering::Relaxed),
        )
    }
}

#[derive(Default)]
struct AlwaysOnD6State {
    land_deltas: Vec<u64>,
    land_cas_retries: usize,
    merged_files_verified: usize,
    fastcdc_chunked_files: usize,
    fastcdc_chunks_count: usize,
    cdc_shifted_files: usize,
    cdc_shifted_logical_bytes: u64,
    cdc_unique_stored_bytes: u64,
    cdc_fixed_block_unique_bytes: u64,
    cdc_dedup_ratio: f64,
    cdc_fixed_block_dedup_ratio: f64,
    crash_drill: Option<Value>,
}

fn cas_payload_bytes(cas_root: &Path) -> Result<u64, String> {
    let objs_dir = cas_root.join("objects");
    if !objs_dir.is_dir() {
        return Err(format!(
            "CAS objects directory missing: {}",
            objs_dir.display()
        ));
    }
    let objs = strict_directory_size(&objs_dir)?.bytes;
    let idxs_dir = cas_root.join("chunk-index");
    let idxs = if idxs_dir.exists() {
        strict_directory_size(&idxs_dir)?.bytes
    } else {
        0
    };
    Ok(objs.saturating_add(idxs))
}

fn evaluate_land_delta_slo(
    landings_completed: usize,
    expected_landings: usize,
    merged_files_verified: usize,
    max_delta: u64,
) -> bool {
    expected_landings > 0
        && landings_completed == expected_landings
        && merged_files_verified == expected_landings
        && max_delta > 0
        && max_delta < LAND_DELTA_SLO_BYTES
}

fn file_byte_len(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| format!("stat {}: {e}", path.display()))
}

fn cas_blob_disk_bytes(
    cas: &spawn_cas::Store,
    digest: &Digest,
    counted_chunks: &mut BTreeSet<Digest>,
) -> Result<u64, String> {
    if !counted_chunks.insert(digest.clone()) {
        return Ok(0);
    }
    let idx_path = cas.chunk_index_path(digest);
    if idx_path.is_file() {
        let mut bytes = file_byte_len(&idx_path)?;
        let index = cas
            .read_chunk_index(digest)
            .map_err(|e| format!("read chunk index {digest}: {e}"))?;
        for chunk in index.chunks {
            if counted_chunks.insert(chunk.id.clone()) {
                bytes = bytes.saturating_add(file_byte_len(&cas.object_path(&chunk.id))?);
            }
        }
        Ok(bytes)
    } else {
        file_byte_len(&cas.object_path(digest))
    }
}

#[derive(Default, Clone, Copy)]
struct ShiftedCdcMetrics {
    shifted_files: usize,
    shifted_logical_bytes: u64,
    cdc_unique_stored_bytes: u64,
    fixed_block_unique_bytes: u64,
    cdc_dedup_ratio: f64,
    fixed_block_dedup_ratio: f64,
}

fn evaluate_cdc_dedup_slo(metrics: ShiftedCdcMetrics, expected_files: usize) -> bool {
    expected_files > 0
        && metrics.shifted_files == expected_files
        && metrics.shifted_logical_bytes > 0
        && metrics.cdc_unique_stored_bytes > 0
        && metrics.fixed_block_unique_bytes > 0
        && metrics.cdc_dedup_ratio >= CDC_DEDUP_MIN_RATIO
        && metrics.fixed_block_dedup_ratio > 0.0
        && metrics.fixed_block_dedup_ratio < CDC_FIXED_BLOCK_MAX_RATIO
}

struct FixedBlockHasher {
    block_size: usize,
    buf: Vec<u8>,
    unique_blocks: BTreeMap<Digest, u64>,
}

impl FixedBlockHasher {
    fn new(block_size: usize) -> Self {
        Self {
            block_size: block_size.max(1),
            buf: Vec::with_capacity(block_size.min(CHUNK_AVG)),
            unique_blocks: BTreeMap::new(),
        }
    }

    fn flush_block(&mut self) {
        if !self.buf.is_empty() {
            let digest = sha256_of(&self.buf);
            let len = u64::try_from(self.buf.len()).unwrap_or(u64::MAX);
            self.unique_blocks.entry(digest).or_insert(len);
            self.buf.clear();
        }
    }

    fn unique_stored_bytes(&self) -> u64 {
        self.unique_blocks.values().copied().sum()
    }
}

impl Write for FixedBlockHasher {
    fn write(&mut self, mut src: &[u8]) -> std::io::Result<usize> {
        let total = src.len();
        while !src.is_empty() {
            let need = self.block_size.saturating_sub(self.buf.len());
            let take = src.len().min(need);
            self.buf.extend_from_slice(&src[..take]);
            src = &src[take..];
            if self.buf.len() == self.block_size {
                self.flush_block();
            }
        }
        Ok(total)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn measure_shifted_cdc_dedup(
    cas: &spawn_cas::Store,
    shifted_files: &[(Digest, u64)],
) -> Result<ShiftedCdcMetrics, String> {
    if shifted_files.is_empty() {
        return Ok(ShiftedCdcMetrics::default());
    }
    let mut cdc_seen = BTreeSet::new();
    let mut fixed_hasher = FixedBlockHasher::new(CHUNK_AVG);
    let mut shifted_logical_bytes = 0_u64;
    let mut cdc_unique_stored_bytes = 0_u64;

    for (digest, logical_len) in shifted_files {
        if !cas.chunk_index_path(digest).is_file() {
            return Err(format!(
                "shifted file {digest} missing FastCDC chunk-index for dedup measurement"
            ));
        }
        shifted_logical_bytes = shifted_logical_bytes.saturating_add(*logical_len);
        cdc_unique_stored_bytes = cdc_unique_stored_bytes.saturating_add(cas_blob_disk_bytes(
            cas,
            digest,
            &mut cdc_seen,
        )?);
        let streamed = cas
            .stream_blob(digest, &mut fixed_hasher)
            .map_err(|e| format!("stream shifted blob {digest} for fixed-block comparison: {e}"))?;
        if streamed != *logical_len {
            return Err(format!(
                "shifted blob {digest} streamed {streamed} bytes != expected {logical_len}"
            ));
        }
        fixed_hasher.flush_block();
    }

    let fixed_block_unique_bytes = fixed_hasher.unique_stored_bytes();
    let cdc_dedup_ratio =
        u64_to_f64(shifted_logical_bytes) / u64_to_f64(cdc_unique_stored_bytes.max(1));
    let fixed_block_dedup_ratio =
        u64_to_f64(shifted_logical_bytes) / u64_to_f64(fixed_block_unique_bytes.max(1));

    Ok(ShiftedCdcMetrics {
        shifted_files: shifted_files.len(),
        shifted_logical_bytes,
        cdc_unique_stored_bytes,
        fixed_block_unique_bytes,
        cdc_dedup_ratio,
        fixed_block_dedup_ratio,
    })
}

fn collect_tree_digests(
    store: &CasMerkleStore,
    root: &Digest,
    out: &mut BTreeSet<Digest>,
) -> Result<(), String> {
    if !out.insert(root.clone()) {
        return Ok(());
    }
    let tree = store
        .read_tree_cached(root)
        .map_err(|e| format!("read tree {root}: {e}"))?;
    for entry in &tree.entries {
        if entry.kind == spawn_cas::TreeEntryKind::Dir
            && let Some(sub) = entry.digest()
        {
            collect_tree_digests(store, &sub, out)?;
        }
    }
    Ok(())
}

struct PreparedLand {
    deliverables: Vec<(String, Digest)>,
    task_ids: Vec<i64>,
}

fn main_journal_line_bytes(
    seq: u64,
    project: &str,
    old_snapshot: &Digest,
    new_snapshot: &Digest,
    git_oid: GitOid,
    at_ms: i64,
) -> Result<u64, String> {
    let encoded = serde_json::to_string(&spawn_cas::JournalEntry {
        seq,
        project: project.to_string(),
        name: "main".to_string(),
        kind: "main".to_string(),
        old_snapshot: Some(old_snapshot.clone()),
        new_snapshot: new_snapshot.clone(),
        git_oid,
        at_ms,
    })
    .map_err(|e| e.to_string())?;
    Ok(u64::try_from(encoded.len() + 1).unwrap_or(0))
}

fn measure_landing_closure_bytes(
    cas: &CasMerkleStore,
    before_tree: &Digest,
    after_tree: &Digest,
    snap_id: &Digest,
    journal_line_bytes: u64,
) -> Result<u64, String> {
    let mut before_trees = BTreeSet::new();
    collect_tree_digests(cas, before_tree, &mut before_trees)?;
    let mut after_trees = BTreeSet::new();
    collect_tree_digests(cas, after_tree, &mut after_trees)?;

    let before_flat = cas
        .flatten_tree(before_tree)
        .map_err(|e| format!("flatten before tree: {e}"))?;
    let after_flat = cas
        .flatten_tree(after_tree)
        .map_err(|e| format!("flatten after tree: {e}"))?;

    let mut counted = BTreeSet::new();
    let mut total = file_byte_len(&cas.cas().object_path(snap_id))?;
    total = total.saturating_add(journal_line_bytes);

    for tree_digest in after_trees.difference(&before_trees) {
        let p = cas.cas().object_path(tree_digest);
        if p.exists() {
            total = total.saturating_add(file_byte_len(&p)?);
        }
    }

    for (path, entry) in after_flat {
        let unchanged = before_flat.get(&path).is_some_and(|prev| {
            prev.kind == entry.kind && prev.size == entry.size && prev.digest() == entry.digest()
        });
        if unchanged {
            continue;
        }
        if let Some(d) = entry.digest() {
            total = total.saturating_add(cas_blob_disk_bytes(cas.cas(), &d, &mut counted)?);
        }
    }
    Ok(total)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CrashAck {
    branch: String,
    snapshot: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CrashManifest {
    project: String,
    schema: String,
    cas_root: PathBuf,
    disk_root: PathBuf,
    ack_path: PathBuf,
    expected_main: String,
    expected_branches: Vec<(String, String)>,
    expected_crash_branches: Vec<(String, String)>,
    acked_snapshots: Vec<String>,
    min_journal_entries: usize,
    torn_tail_bytes: u64,
}

fn is_snapshot_ancestor(
    cas: &CasMerkleStore,
    descendant: &Digest,
    ancestor: &Digest,
) -> Result<bool, String> {
    if descendant == ancestor {
        return Ok(true);
    }
    let mut seen = BTreeSet::new();
    let mut stack = vec![descendant.clone()];
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur.clone()) {
            continue;
        }
        let commit = cas
            .cas()
            .get_commit_object(&cur)
            .map_err(|e| format!("read commit {cur} during ancestor walk: {e}"))?;
        for parent in commit.parents() {
            if parent == ancestor {
                return Ok(true);
            }
            stack.push(parent.clone());
        }
    }
    Ok(false)
}

fn inject_torn_journal_tail(journal_path: &Path) -> Result<u64, String> {
    let mut f = fs::OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(journal_path)
        .map_err(|e| format!("open refs.journal for torn injection: {e}"))?;
    f.write_all(TORN_JOURNAL_TAIL)
        .map_err(|e| format!("write torn journal tail: {e}"))?;
    f.sync_all()
        .map_err(|e| format!("sync torn journal: {e}"))?;
    u64::try_from(TORN_JOURNAL_TAIL.len()).map_err(|_| "torn tail length overflows u64".to_string())
}

fn read_complete_crash_acks(ack_path: &Path) -> Result<Vec<CrashAck>, String> {
    if !ack_path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(ack_path).map_err(|e| format!("read {}: {e}", ack_path.display()))?;
    let Some(last_nl) = bytes.iter().rposition(|&b| b == b'\n') else {
        return Ok(Vec::new());
    };
    let mut acks = Vec::new();
    for raw in bytes[..last_nl].split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let parsed: CrashAck = serde_json::from_slice(raw)
            .map_err(|e| format!("parse ack line in {}: {e}", ack_path.display()))?;
        acks.push(parsed);
    }
    Ok(acks)
}

#[allow(clippy::disallowed_methods)]
fn internal_crash_writer_main(manifest_path: &Path) -> Result<(), String> {
    let url = env::var("SPAWN_BENCH_DATABASE_URL")
        .map_err(|_| "SPAWN_BENCH_DATABASE_URL required for internal crash writer".to_string())?;
    let raw = fs::read_to_string(manifest_path)
        .map_err(|e| format!("read manifest {}: {e}", manifest_path.display()))?;
    let manifest: CrashManifest =
        serde_json::from_str(&raw).map_err(|e| format!("parse manifest: {e}"))?;

    let cas = Arc::new(
        CasMerkleStore::open(&manifest.cas_root).map_err(|e| format!("writer open cas: {e}"))?,
    );
    let ledger = Arc::new(PgRefLedger::connect_schema(&url, &manifest.schema)?);
    let ws_store = D6Workspace::new(
        &manifest.project,
        &manifest.disk_root,
        cas,
        ledger,
        u64::MAX,
    );

    let mut branch_ws = Vec::with_capacity(2);
    for slot in 0..2 {
        let name = format!("crash_drill_{slot}");
        let branch = BranchName::new(name.clone()).map_err(|e| e.to_string())?;
        let _ = ws_store
            .ensure_branch(&branch, None)
            .map_err(|e| e.to_string())?;
        let ws = ws_store.mount(&branch).map_err(|e| e.to_string())?;
        branch_ws.push((name, ws));
    }

    let mut ack_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&manifest.ack_path)
        .map_err(|e| format!("open ack file: {e}"))?;

    let mut step = 0_usize;
    loop {
        let slot = step % branch_ws.len();
        let (branch_name, ws) = &branch_ws[slot];
        let payload_len = if step < 6 { 8 * 1024 } else { 128 * 1024 };
        let rel = format!("crash/step_{step:05}.dat");
        let target = Path::new(ws.path()).join(&rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let payload = fill_mixed_payload(
            0x4352_4153_0000_u64.wrapping_add(step as u64),
            &format!("// crash-writer step={step} branch={branch_name}\n"),
            payload_len,
        );
        fs::write(&target, payload).map_err(|e| e.to_string())?;

        match ws_store
            .commit(ws, &format!("crash drill commit {step}"))
            .map_err(|e| e.to_string())?
        {
            Commit::Created(commit_id)
            | Commit::Recreated {
                commit: commit_id, ..
            } => {
                let live = ws_store
                    .branch_snapshot(branch_name)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("missing snapshot for {branch_name}"))?;
                let _ = commit_id;
                let ack_line = serde_json::to_string(&CrashAck {
                    branch: branch_name.clone(),
                    snapshot: live.id.to_string(),
                })
                .map_err(|e| e.to_string())?;
                ack_file
                    .write_all(format!("{ack_line}\n").as_bytes())
                    .map_err(|e| format!("write ack: {e}"))?;
                ack_file.sync_all().map_err(|e| format!("fsync ack: {e}"))?;
            }
            Commit::Unchanged => {
                return Err(format!("unexpected unchanged commit on crash step {step}"));
            }
        }
        step = step.saturating_add(1);
    }
}

struct ManifestRefsCheck {
    lost_refs: usize,
    acknowledged_recovered: usize,
    missing_pg_ack: usize,
    snapshots: BTreeSet<Digest>,
}

fn verify_manifest_refs_and_acks(
    fresh_cas: &CasMerkleStore,
    fresh_ledger: &PgRefLedger,
    fresh_ws: &D6Workspace<PgRefLedger>,
    manifest: &CrashManifest,
    initial_lost_refs: usize,
) -> Result<ManifestRefsCheck, String> {
    let mut lost_refs = initial_lost_refs;
    let mut snapshots = BTreeSet::new();

    let expected_main = Digest::parse(&manifest.expected_main).map_err(|e| e.to_string())?;
    snapshots.insert(expected_main.clone());
    match fresh_ws.main_snapshot() {
        Ok(Some(live)) if live.id == expected_main => {}
        _ => lost_refs += 1,
    }

    for (branch, expected_hex) in &manifest.expected_branches {
        let expected_id = Digest::parse(expected_hex).map_err(|e| e.to_string())?;
        snapshots.insert(expected_id.clone());
        match fresh_ws.branch_snapshot(branch) {
            Ok(Some(live)) if live.id == expected_id => {}
            _ => lost_refs += 1,
        }
    }

    for (branch, expected_acked_hex) in &manifest.expected_crash_branches {
        let acked_id = Digest::parse(expected_acked_hex).map_err(|e| e.to_string())?;
        snapshots.insert(acked_id.clone());
        match fresh_ws.branch_snapshot(branch) {
            Ok(Some(live)) => {
                snapshots.insert(live.id.clone());
                if !is_snapshot_ancestor(fresh_cas, &live.id, &acked_id).unwrap_or(false) {
                    lost_refs += 1;
                }
            }
            _ => lost_refs += 1,
        }
    }

    let mut acknowledged_recovered = 0_usize;
    let mut missing_pg_ack = 0_usize;
    for snap_hex in &manifest.acked_snapshots {
        let d = Digest::parse(snap_hex).map_err(|e| e.to_string())?;
        snapshots.insert(d.clone());
        match fresh_ledger.get_snapshot(&d) {
            Ok(Some(_)) => acknowledged_recovered += 1,
            _ => missing_pg_ack += 1,
        }
    }

    Ok(ManifestRefsCheck {
        lost_refs,
        acknowledged_recovered,
        missing_pg_ack,
        snapshots,
    })
}

struct PreTornJournalState {
    entries_before: usize,
    pre_max_journal_seq: u64,
    pre_reconcile_last_journal_seq: u64,
    pg_seq_pre: u64,
}

struct PostTornCommitReport {
    branch: String,
    post_commit_snapshot: String,
    entries_before: usize,
    entries_after: usize,
    torn_tail_healed_on_disk: bool,
    committed_new_entry: bool,
    pg_seq_post: u64,
    reconcile_post_entries_considered: usize,
    reconcile_post_entries_replayed: usize,
    reconcile_post_already_applied: usize,
    reconcile_post_last_journal_seq: u64,
    reconcile_post_last_ledger_seq: u64,
    reconcile_converged_entries_considered: usize,
    post_torn_commit_reconciled: bool,
}

fn commit_post_torn_healing_change(
    fresh_ws: &D6Workspace<PgRefLedger>,
    branch_name: &str,
) -> Result<String, String> {
    let branch = BranchName::new(branch_name.to_string()).map_err(|e| e.to_string())?;
    let ws = fresh_ws
        .mount(&branch)
        .map_err(|e| format!("post-torn mount {branch_name}: {e}"))?;
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let target = Path::new(ws.path()).join(format!("crash/post_torn_heal_{pid}_{nanos:x}.dat"));
    let commit_result = (|| -> Result<String, String> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("post-torn create_dir_all: {e}"))?;
        }
        let seed = 0x504F_5354_544F_524E_u64
            ^ u64::from(pid)
            ^ u64::try_from(nanos & 0xFFFF_FFFF).unwrap_or(0);
        let payload = fill_mixed_payload(
            seed,
            &format!("// post-torn heal commit pid={pid} nanos={nanos}\n"),
            4096,
        );
        fs::write(&target, payload)
            .map_err(|e| format!("post-torn write {}: {e}", target.display()))?;
        match fresh_ws
            .commit(&ws, &format!("post-torn heal commit pid={pid}"))
            .map_err(|e| format!("post-torn D6Workspace::commit: {e}"))?
        {
            Commit::Created(_) | Commit::Recreated { .. } => {
                let live = fresh_ws
                    .branch_snapshot(branch_name)
                    .map_err(|e| format!("post-torn branch_snapshot: {e}"))?
                    .ok_or_else(|| format!("post-torn missing snapshot for {branch_name}"))?;
                Ok(live.id.to_string())
            }
            Commit::Unchanged => Err(
                "post-torn D6Workspace::commit unexpectedly returned Commit::Unchanged".to_string(),
            ),
        }
    })();
    let _ = fresh_ws.unmount(&ws);
    commit_result
}

fn execute_post_torn_commit_and_reconcile(
    fresh_cas: &CasMerkleStore,
    fresh_ledger: &PgRefLedger,
    fresh_ws: &D6Workspace<PgRefLedger>,
    manifest: &CrashManifest,
    pre: &PreTornJournalState,
) -> Result<PostTornCommitReport, String> {
    let branch_name = manifest
        .expected_crash_branches
        .first()
        .map(|(branch, _)| branch.clone())
        .ok_or_else(|| "crash manifest missing crash branch for post-torn commit".to_string())?;
    let post_commit_snapshot = commit_post_torn_healing_change(fresh_ws, &branch_name)?;

    let raw_after = fs::read(fresh_cas.cas().refs_journal_path())
        .map_err(|e| format!("read post-torn refs.journal: {e}"))?;
    let torn_tail_healed_on_disk = raw_after.ends_with(b"\n")
        && !raw_after.ends_with(TORN_JOURNAL_TAIL)
        && !raw_after
            .windows(TORN_JOURNAL_TAIL.len())
            .any(|w| w == TORN_JOURNAL_TAIL);

    let journal_after = fresh_cas
        .cas()
        .read_ref_journal()
        .map_err(|e| format!("post-torn read_ref_journal: {e}"))?;
    let entries_after = journal_after.len();
    let post_max_journal_seq = journal_after.last().map_or(0, |e| e.seq);
    let committed_new_entry = entries_after == pre.entries_before.saturating_add(1)
        && post_max_journal_seq > pre.pre_max_journal_seq;

    let reconcile_post = reconcile_refs_journal(
        fresh_cas,
        fresh_ledger,
        pre.pg_seq_pre,
        "bench:crash-verify-post-torn",
    )
    .map_err(|e| format!("post-torn reconcile_refs_journal(pg_seq_pre): {e}"))?;

    let pg_seq_post = fresh_ledger
        .pg
        .latest_ref_log_seq()
        .map_err(|e| format!("post-torn latest_ref_log_seq: {e}"))?;
    let reconcile_converged = reconcile_refs_journal(
        fresh_cas,
        fresh_ledger,
        pg_seq_post,
        "bench:crash-verify-converged",
    )
    .map_err(|e| format!("post-torn reconcile_refs_journal(pg_seq_post): {e}"))?;

    let post_torn_commit_reconciled = reconcile_post.entries_considered >= 1
        && reconcile_post.already_applied >= 1
        && reconcile_post.entries_replayed == 0
        && reconcile_post.last_journal_seq > pre.pre_reconcile_last_journal_seq
        && reconcile_post.last_journal_seq == post_max_journal_seq
        && reconcile_converged.entries_considered == 0
        && reconcile_converged.last_journal_seq == post_max_journal_seq
        && reconcile_converged.last_ledger_seq == pg_seq_post;

    Ok(PostTornCommitReport {
        branch: branch_name,
        post_commit_snapshot,
        entries_before: pre.entries_before,
        entries_after,
        torn_tail_healed_on_disk,
        committed_new_entry,
        pg_seq_post,
        reconcile_post_entries_considered: reconcile_post.entries_considered,
        reconcile_post_entries_replayed: reconcile_post.entries_replayed,
        reconcile_post_already_applied: reconcile_post.already_applied,
        reconcile_post_last_journal_seq: reconcile_post.last_journal_seq,
        reconcile_post_last_ledger_seq: reconcile_post.last_ledger_seq,
        reconcile_converged_entries_considered: reconcile_converged.entries_considered,
        post_torn_commit_reconciled,
    })
}

fn prepare_torn_journal_for_verify(
    jpath: &Path,
    manifest: &CrashManifest,
    inject_torn: bool,
) -> Result<bool, String> {
    if inject_torn {
        let raw_pre_inject = fs::read(jpath)
            .map_err(|e| format!("verifier read refs.journal before re-inject: {e}"))?;
        if !raw_pre_inject.ends_with(b"\n") || raw_pre_inject.ends_with(TORN_JOURNAL_TAIL) {
            return Err(
                "expected healed \\n-terminated refs.journal before --inject-torn-tail".to_string(),
            );
        }
        let _ = inject_torn_journal_tail(jpath)?;
    }
    let raw_journal =
        fs::read(jpath).map_err(|e| format!("verifier read raw refs.journal: {e}"))?;
    Ok(manifest.torn_tail_bytes > 0 && raw_journal.ends_with(TORN_JOURNAL_TAIL))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CrashVerifyMode {
    FreshKill,
    ReInjectTornTail,
}

impl CrashVerifyMode {
    fn is_reinject(self) -> bool {
        self == Self::ReInjectTornTail
    }
}

struct CrashVerifyJsonInput<'a> {
    manifest: &'a CrashManifest,
    refs_check: &'a ManifestRefsCheck,
    post_torn: &'a PostTornCommitReport,
    mode: CrashVerifyMode,
    torn_tail_present_before_commit: bool,
    torn_tail_healed_and_committed: bool,
    pg_latest_ref_seq: u64,
    reconcile_pre_entries_considered: usize,
    reconcile_pre_last_journal_seq: u64,
    reconcile_pre_last_ledger_seq: u64,
    blobs_verified: usize,
    lost_commits: usize,
    corrupt_objects: usize,
    recovery_ms: f64,
    verified: bool,
}

fn build_crash_verify_json(input: &CrashVerifyJsonInput<'_>) -> Value {
    let branches_verified =
        input.manifest.expected_branches.len() + input.manifest.expected_crash_branches.len();
    json!({
        "verified": input.verified,
        "verifier_pid": std::process::id(),
        "inject_torn_tail": input.mode.is_reinject(),
        "branches_verified": branches_verified,
        "refs_verified": branches_verified + 1,
        "snapshots_verified": input.refs_check.snapshots.len(),
        "blobs_verified": input.blobs_verified,
        "acknowledged_recovered": input.refs_check.acknowledged_recovered,
        "journal_entries_before_commit": input.post_torn.entries_before,
        "journal_entries_after_commit": input.post_torn.entries_after,
        "torn_tail_present_before_commit": input.torn_tail_present_before_commit,
        "torn_tail_healed_on_disk": input.post_torn.torn_tail_healed_on_disk,
        "torn_tail_healed_and_committed": input.torn_tail_healed_and_committed,
        "post_torn_commit_reconciled": input.post_torn.post_torn_commit_reconciled,
        "post_torn_commit_branch": input.post_torn.branch,
        "post_torn_commit_snapshot": input.post_torn.post_commit_snapshot,
        "pg_latest_ref_seq_before_commit": input.pg_latest_ref_seq,
        "pg_latest_ref_seq_after_commit": input.post_torn.pg_seq_post,
        "reconcile_pre_entries_considered": input.reconcile_pre_entries_considered,
        "reconcile_pre_last_journal_seq": input.reconcile_pre_last_journal_seq,
        "reconcile_pre_last_ledger_seq": input.reconcile_pre_last_ledger_seq,
        "reconcile_post_entries_considered": input.post_torn.reconcile_post_entries_considered,
        "reconcile_post_entries_replayed": input.post_torn.reconcile_post_entries_replayed,
        "reconcile_post_already_applied": input.post_torn.reconcile_post_already_applied,
        "reconcile_post_last_journal_seq": input.post_torn.reconcile_post_last_journal_seq,
        "reconcile_post_last_ledger_seq": input.post_torn.reconcile_post_last_ledger_seq,
        "reconcile_converged_entries_considered": input.post_torn.reconcile_converged_entries_considered,
        "lost_commits": input.lost_commits,
        "lost_refs": input.refs_check.lost_refs,
        "corrupt_objects": input.corrupt_objects,
        "recovery_ms": input.recovery_ms,
    })
}

#[allow(clippy::disallowed_methods)]
fn internal_crash_verify_main(manifest_path: &Path, inject_torn: bool) -> Result<Value, String> {
    let url = env::var("SPAWN_BENCH_DATABASE_URL")
        .map_err(|_| "SPAWN_BENCH_DATABASE_URL required for internal crash verify".to_string())?;
    let raw = fs::read_to_string(manifest_path)
        .map_err(|e| format!("read manifest {}: {e}", manifest_path.display()))?;
    let manifest: CrashManifest =
        serde_json::from_str(&raw).map_err(|e| format!("parse manifest: {e}"))?;

    let t0 = Instant::now();
    let fresh_cas = Arc::new(
        CasMerkleStore::open(&manifest.cas_root).map_err(|e| format!("verifier open cas: {e}"))?,
    );
    let fresh_ledger = Arc::new(PgRefLedger::connect_schema(&url, &manifest.schema)?);
    let fresh_ws = D6Workspace::new(
        &manifest.project,
        &manifest.disk_root,
        fresh_cas.clone(),
        fresh_ledger.clone(),
        u64::MAX,
    );

    let torn_tail_present_before_commit = prepare_torn_journal_for_verify(
        &fresh_cas.cas().refs_journal_path(),
        &manifest,
        inject_torn,
    )?;
    let journal = fresh_cas
        .cas()
        .read_ref_journal()
        .map_err(|e| format!("verifier read_ref_journal: {e}"))?;
    let pg_latest_ref_seq = fresh_ledger
        .pg
        .latest_ref_log_seq()
        .map_err(|e| format!("verifier latest_ref_log_seq: {e}"))?;
    let reconcile_pre = reconcile_refs_journal(
        &fresh_cas,
        fresh_ledger.as_ref(),
        pg_latest_ref_seq,
        "bench:crash-verify-pre",
    )
    .map_err(|e| format!("verifier reconcile_refs_journal: {e}"))?;

    let min_expected_journal = manifest
        .min_journal_entries
        .max(manifest.acked_snapshots.len());
    let torn_tolerated_before_commit = torn_tail_present_before_commit
        && journal.len() >= min_expected_journal
        && reconcile_pre.last_journal_seq > 0
        && reconcile_pre.last_ledger_seq >= pg_latest_ref_seq;

    let refs_check = verify_manifest_refs_and_acks(
        &fresh_cas,
        &fresh_ledger,
        &fresh_ws,
        &manifest,
        usize::from(!torn_tolerated_before_commit),
    )?;
    let (lost_commits, corrupt_objects, blobs_verified) =
        verify_reopened_objects(&fresh_cas, &refs_check.snapshots);
    let lost_commits = lost_commits.saturating_add(refs_check.missing_pg_ack);

    let post_torn = execute_post_torn_commit_and_reconcile(
        &fresh_cas,
        &fresh_ledger,
        &fresh_ws,
        &manifest,
        &PreTornJournalState {
            entries_before: journal.len(),
            pre_max_journal_seq: journal.last().map_or(0, |e| e.seq),
            pre_reconcile_last_journal_seq: reconcile_pre.last_journal_seq,
            pg_seq_pre: pg_latest_ref_seq,
        },
    )?;

    let torn_tail_healed_and_committed = torn_tolerated_before_commit
        && post_torn.torn_tail_healed_on_disk
        && post_torn.committed_new_entry;
    let verified = refs_check.lost_refs == 0
        && lost_commits == 0
        && corrupt_objects == 0
        && refs_check.acknowledged_recovered == manifest.acked_snapshots.len()
        && torn_tail_healed_and_committed
        && post_torn.post_torn_commit_reconciled;

    Ok(build_crash_verify_json(&CrashVerifyJsonInput {
        manifest: &manifest,
        refs_check: &refs_check,
        post_torn: &post_torn,
        mode: if inject_torn {
            CrashVerifyMode::ReInjectTornTail
        } else {
            CrashVerifyMode::FreshKill
        },
        torn_tail_present_before_commit,
        torn_tail_healed_and_committed,
        pg_latest_ref_seq,
        reconcile_pre_entries_considered: reconcile_pre.entries_considered,
        reconcile_pre_last_journal_seq: reconcile_pre.last_journal_seq,
        reconcile_pre_last_ledger_seq: reconcile_pre.last_ledger_seq,
        blobs_verified,
        lost_commits,
        corrupt_objects,
        recovery_ms: t0.elapsed().as_secs_f64() * 1000.0,
        verified,
    }))
}

struct ChunkedBlobInfo {
    digest: Digest,
    size: u64,
    chunks: usize,
}

struct VerifiedBranchChunked {
    base: ChunkedBlobInfo,
    shifted: ChunkedBlobInfo,
}

struct D6Driver {
    project: String,
    disk_root: PathBuf,
    cas_root: PathBuf,
    scratch_root: PathBuf,
    cas: Arc<CasMerkleStore>,
    coord_ledger: Arc<PgRefLedger>,
    coord_store: D6Workspace<PgRefLedger>,
    worker_ledgers: Vec<Arc<PgRefLedger>>,
    worker_stores: Vec<D6Workspace<PgRefLedger>>,
    pg_url: String,
    schema: String,
    oversized_ingested: bool,
    save_crash_manifest: Option<PathBuf>,
    base_main_bytes: u64,
    logical_workspace_bytes: AtomicU64,
    logical_written_bytes: AtomicU64,
    always_on: Mutex<AlwaysOnD6State>,
}

struct D6DriverInit<'a> {
    project: String,
    disk_root: PathBuf,
    cas_root: PathBuf,
    scratch_root: PathBuf,
    cas: Arc<CasMerkleStore>,
    pg_pool: &'a [Arc<PostgresStore>],
    pg_url: String,
    schema: String,
    config: &'a Config,
}

fn verify_reopened_objects(
    fresh_cas: &CasMerkleStore,
    snapshots: &BTreeSet<Digest>,
) -> (usize, usize, usize) {
    let mut lost_commits = 0_usize;
    let mut corrupt_objects = 0_usize;
    let mut blob_digests = BTreeSet::new();
    for snap_id in snapshots {
        match fresh_cas.cas().get_commit_object(snap_id) {
            Ok(commit) => match fresh_cas.flatten_tree(commit.tree()) {
                Ok(entries) => {
                    for entry in entries.into_values() {
                        if let Some(d) = entry.digest() {
                            blob_digests.insert(d);
                        }
                    }
                }
                Err(_) => corrupt_objects += 1,
            },
            Err(_) => lost_commits += 1,
        }
    }
    for blob in &blob_digests {
        if fresh_cas
            .cas()
            .stream_blob(blob, &mut std::io::sink())
            .is_err()
        {
            corrupt_objects += 1;
        }
    }
    (lost_commits, corrupt_objects, blob_digests.len())
}

impl D6Driver {
    fn new(init: D6DriverInit<'_>) -> Result<Self, String> {
        let project = init.project;
        let disk_root = init.disk_root;
        let cas_root = init.cas_root;
        let scratch_root = init.scratch_root;
        let cas = init.cas;
        let config = init.config;
        let make_ws = |ledger: Arc<PgRefLedger>| {
            let max_file_bytes =
                if config.oversized_file_bytes.is_some() || config.land_payload_bytes > 0 {
                    u64::MAX
                } else {
                    spawn_host::DEFAULT_MAX_FILE_BYTES
                };
            let mut ws =
                D6Workspace::new(&project, &disk_root, cas.clone(), ledger, max_file_bytes);
            if let Some(t) = &config.tmpfs_root {
                ws = ws.with_l1_policy(L1TmpfsPolicy::new(t, config.tmpfs_budget_mb));
            }
            ws
        };
        let coord_ledger = Arc::new(PgRefLedger::from_store(init.pg_pool[0].clone()));
        let coord_store = make_ws(coord_ledger.clone());
        let (_, base_main_bytes) =
            d6_seed_initial_main(&coord_store, &coord_ledger, &cas, &project, config)?;

        let mut worker_ledgers = Vec::with_capacity(config.concurrency);
        let mut worker_stores = Vec::with_capacity(config.concurrency);
        for idx in 0..config.concurrency {
            let l = Arc::new(PgRefLedger::from_store(
                init.pg_pool[idx % init.pg_pool.len()].clone(),
            ));
            worker_ledgers.push(l.clone());
            worker_stores.push(make_ws(l));
        }

        Ok(Self {
            project,
            disk_root,
            cas_root,
            scratch_root,
            cas,
            coord_ledger,
            coord_store,
            worker_ledgers,
            worker_stores,
            pg_url: init.pg_url,
            schema: init.schema,
            oversized_ingested: config.oversized_file_bytes.is_some(),
            save_crash_manifest: config.save_crash_manifest.clone(),
            base_main_bytes,
            logical_workspace_bytes: AtomicU64::new(0),
            logical_written_bytes: AtomicU64::new(base_main_bytes),
            always_on: Mutex::new(AlwaysOnD6State::default()),
        })
    }

    fn verify_chunked_blob_entry(
        &self,
        flat: &BTreeMap<Vec<u8>, spawn_cas::TreeEntry>,
        rel_path: &[u8],
        label: &str,
        expected_size: usize,
    ) -> Result<ChunkedBlobInfo, String> {
        let ent = flat
            .get(rel_path)
            .ok_or_else(|| format!("D6 checkpoint omitted {label}"))?;
        let want_size = u64::try_from(expected_size).map_err(|_| "size overflow".to_string())?;
        if ent.size != want_size {
            return Err(format!("{label} size {} != {want_size}", ent.size));
        }
        let digest = ent
            .digest()
            .ok_or_else(|| format!("{label} missing digest"))?;
        let mut chunks_count = 0_usize;
        if expected_size > CHUNK_MAX {
            if !self.cas.cas().chunk_index_path(&digest).is_file() {
                return Err(format!("{label} was not FastCDC chunked into chunk-index"));
            }
            let idx = self
                .cas
                .cas()
                .read_chunk_index(&digest)
                .map_err(|e| format!("read {label} chunk index: {e}"))?;
            chunks_count = idx.chunks.len();
            if chunks_count < 2 {
                return Err(format!(
                    "heterogeneous {label} expected >= 2 FastCDC chunks, got {chunks_count}"
                ));
            }
        }
        let projected_blob = spawn_cas::project_blob_oid(self.cas.cas(), &digest, want_size)
            .map_err(|e| e.to_string())?;
        if want_size > GIT_POINTER_THRESHOLD {
            let expected_pointer = git_blob_oid_of_bytes(&git_pointer_bytes(&digest, want_size));
            if projected_blob != expected_pointer {
                return Err(format!(
                    "{label} OID {projected_blob} != pointer OID {expected_pointer}"
                ));
            }
        } else {
            let mut blob_bytes = Vec::new();
            self.cas
                .cas()
                .stream_blob(&digest, &mut blob_bytes)
                .map_err(|e| format!("stream {label} <= threshold: {e}"))?;
            let expected_blob_oid = git_blob_oid_of_bytes(&blob_bytes);
            if projected_blob != expected_blob_oid {
                return Err(format!(
                    "{label} OID {projected_blob} != blob OID {expected_blob_oid}"
                ));
            }
        }
        Ok(ChunkedBlobInfo {
            digest,
            size: want_size,
            chunks: chunks_count,
        })
    }

    fn verify_oversized_chunked_in_cas(
        &self,
        branch: &BranchName,
        expected_size: usize,
    ) -> Result<VerifiedBranchChunked, String> {
        let idx = worker_index_from_branch(branch.as_str());
        let head = self
            .coord_store
            .branch_snapshot(branch.as_str())
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("branch {branch} missing snapshot"))?;
        let flat = self
            .cas
            .flatten_tree(&head.tree)
            .map_err(|e| e.to_string())?;
        let base = self.verify_chunked_blob_entry(
            &flat,
            b"oversized.bin",
            "oversized.bin",
            expected_size,
        )?;
        let shift_rel = format!("branches/w{idx:03}/oversized_shift.bin");
        let shift_expected_size = expected_size.saturating_add(383 + idx * 97);
        let shifted = self.verify_chunked_blob_entry(
            &flat,
            shift_rel.as_bytes(),
            &shift_rel,
            shift_expected_size,
        )?;
        Ok(VerifiedBranchChunked { base, shifted })
    }

    fn prepare_land_deliverables(
        &self,
        config: &Config,
        workers: &[WorkerState],
    ) -> Result<PreparedLand, String> {
        let mut expected_deliverables: Vec<(String, Digest)> = Vec::with_capacity(workers.len());
        let land_mission = self
            .coord_ledger
            .pg
            .mission_add(
                "D6 benchmark concurrent land",
                &json!(["/tmp/d6-bench-land.git"]),
                "human",
            )
            .map_err(|e| format!("land mission_add: {e}"))?;
        self.coord_ledger
            .pg
            .mission_configure(land_mission, Some("/scenarios"), "gate", None, None)
            .map_err(|e| format!("land mission_configure: {e}"))?;
        let mut task_ids = Vec::with_capacity(workers.len());

        for w in workers {
            let store = &self.worker_stores[w.idx];
            let rel_path = format!("branches/w{:03}/deliverable.bin", w.idx);
            let full_path = Path::new(w.workspace.path()).join(&rel_path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let deliv_payload = fill_mixed_payload(
                0x4445_4C49_u64 ^ ((w.idx as u64).wrapping_add(1)),
                &format!("// deliverable branch={} worker={}\n", w.branch, w.idx),
                config.land_payload_bytes,
            );
            self.logical_written_bytes.fetch_add(
                u64::try_from(deliv_payload.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            fs::write(&full_path, &deliv_payload).map_err(|e| e.to_string())?;
            match store
                .commit(&w.workspace, &format!("deliverable for {}", w.branch))
                .map_err(|e| e.to_string())?
            {
                Commit::Created(_) => {}
                other => {
                    return Err(format!(
                        "deliverable commit on {} returned {other:?}",
                        w.branch
                    ));
                }
            }
            let head = store
                .branch_snapshot(w.branch.as_str())
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("branch {} missing deliverable head", w.branch))?;
            let flat = self
                .cas
                .flatten_tree(&head.tree)
                .map_err(|e| e.to_string())?;
            let digest = flat
                .get(rel_path.as_bytes())
                .and_then(spawn_cas::TreeEntry::digest)
                .ok_or_else(|| format!("deliverable {rel_path} missing in branch tree"))?;
            expected_deliverables.push((rel_path, digest));

            let worker_name = format!("w_bench_{}", w.idx);
            let task_id = self
                .coord_ledger
                .pg
                .task_assign(
                    land_mission,
                    "ephemeral",
                    &worker_name,
                    &format!("land benchmark branch {}", w.branch),
                    "default",
                )
                .map_err(|e| format!("land task_assign: {e}"))?;
            self.coord_ledger
                .pg
                .task_delivered(task_id, 1)
                .map_err(|e| format!("land task_delivered: {e}"))?;
            let _ = self
                .coord_ledger
                .pg
                .task_reported(task_id, "done")
                .map_err(|e| format!("land task_reported: {e}"))?;
            task_ids.push(task_id);
        }

        Ok(PreparedLand {
            deliverables: expected_deliverables,
            task_ids,
        })
    }

    fn try_land_worker_attempt(
        &self,
        worker: &WorkerState,
        task_id: i64,
        attempt: usize,
    ) -> Result<Option<(Duration, u64)>, String> {
        let store = &self.worker_stores[worker.idx];
        let ledger = &self.worker_ledgers[worker.idx];
        let t0 = Instant::now();
        let main_now = store
            .main_snapshot()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "main snapshot missing before land".to_string())?;
        let branch_head = store
            .branch_snapshot(worker.branch.as_str())
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("branch {} missing before land", worker.branch))?;
        let (_base, merge_report) = merge_snapshots(&self.cas, &main_now.id, &branch_head.id, None)
            .map_err(|e| format!("merge_snapshots {}: {e}", worker.branch))?;
        if !merge_report.conflicts.is_empty() {
            return Err(format!(
                "unexpected {} conflicts merging {}",
                merge_report.conflicts.len(),
                worker.branch
            ));
        }
        let ts_ms = 1_700_000_500_000_i64
            + i64::try_from(worker.idx).unwrap_or(0) * 1_000
            + i64::try_from(attempt).unwrap_or(0);
        let body = format_commit_body(
            "spawn <bench@localhost> 1700000500 +0000",
            "spawn <bench@localhost> 1700000500 +0000",
            &format!("land branch {}\n", worker.branch),
        );
        let snap = Snapshot {
            tree: merge_report.tree.clone(),
            parents: vec![main_now.id.clone(), branch_head.id.clone()],
            commit_body: body,
        };
        let snap_id = self
            .cas
            .cas()
            .put_merkle_snapshot(&snap)
            .map_err(|e| e.to_string())?;
        let oid = self
            .cas
            .project_snapshot_oid(&snap, &[main_now.git_oid, branch_head.git_oid])
            .map_err(|e| e.to_string())?;
        let _ = ledger
            .record_snapshot(
                &snap_id,
                &oid,
                &merge_report.tree,
                &snap.parents,
                &self.project,
                ts_ms,
            )
            .map_err(|e| e.to_string())?;
        ledger
            .pg
            .integrator_record(&json!({
                "type": "task.gated",
                "id": task_id,
                "revision": 0,
                "verdict": "accepted",
                "base": main_now.git_oid.to_hex(),
                "candidate": oid.to_hex(),
            }))
            .map_err(|e| format!("land integrator_record: {e}"))?;
        match store.land_main_snapshot(Some(&main_now.id), &snap_id, Some(task_id), "bench land") {
            Ok(seq) => {
                let dur = t0.elapsed();
                let line_bytes = main_journal_line_bytes(
                    seq,
                    &self.project,
                    &main_now.id,
                    &snap_id,
                    oid,
                    ts_ms,
                )?;
                let delta_bytes = measure_landing_closure_bytes(
                    &self.cas,
                    &main_now.tree,
                    &merge_report.tree,
                    &snap_id,
                    line_bytes,
                )?;
                Ok(Some((dur, delta_bytes)))
            }
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("CAS mismatch") {
                    Ok(None)
                } else {
                    Err(format!("land_main {}: {msg}", worker.branch))
                }
            }
        }
    }

    fn land_worker_with_retries(
        &self,
        worker: &mut WorkerState,
        task_id: i64,
        max_attempts: usize,
    ) -> Result<(u64, usize), String> {
        for (retries, attempt) in (0..max_attempts).enumerate() {
            if let Some((dur, delta_bytes)) =
                self.try_land_worker_attempt(worker, task_id, attempt)?
            {
                worker.samples.push("land_main", dur);
                return Ok((delta_bytes, retries));
            }
            let backoff_ms = u64::try_from((worker.idx % 4) + 1).unwrap_or(1)
                * u64::try_from((retries + 1).min(4)).unwrap_or(1)
                * 2;
            std::thread::sleep(Duration::from_millis(backoff_ms));
        }
        Err(format!(
            "branch {} exceeded {max_attempts} concurrent CAS land retries",
            worker.branch
        ))
    }

    fn verify_landed_deliverables(
        &self,
        expected_deliverables: &[(String, Digest)],
    ) -> Result<usize, String> {
        let final_main = self
            .coord_store
            .main_snapshot()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "main snapshot missing after land".to_string())?;
        let final_flat = self
            .cas
            .flatten_tree(&final_main.tree)
            .map_err(|e| e.to_string())?;
        let mut merged_files_verified = 0_usize;
        for (rel_path, expected_digest) in expected_deliverables {
            let actual = final_flat
                .get(rel_path.as_bytes())
                .and_then(spawn_cas::TreeEntry::digest)
                .ok_or_else(|| format!("landed main missing merged deliverable {rel_path}"))?;
            if &actual != expected_digest {
                return Err(format!(
                    "landed main deliverable {rel_path} digest mismatch: {actual} != {expected_digest}"
                ));
            }
            merged_files_verified += 1;
        }
        Ok(merged_files_verified)
    }

    fn run_land_phase(&self, config: &Config, workers: &mut [WorkerState]) -> Result<(), String> {
        let prepared = self.prepare_land_deliverables(config, workers)?;
        let concurrency = workers.len();
        let barrier = Arc::new(Barrier::new(concurrency));
        let results: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::with_capacity(concurrency));
        let max_attempts = concurrency.saturating_mul(12).max(32);

        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(concurrency);
            for (worker, &task_id) in workers.iter_mut().zip(&prepared.task_ids) {
                let bar = barrier.clone();
                let res_ref = &results;
                handles.push(scope.spawn(move || -> Result<(), String> {
                    bar.wait();
                    let outcome = self.land_worker_with_retries(worker, task_id, max_attempts)?;
                    res_ref
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(outcome);
                    Ok(())
                }));
            }
            for h in handles {
                h.join()
                    .map_err(|_| "concurrent land worker panicked".to_string())??;
            }
            Ok::<(), String>(())
        })?;

        let merged_files_verified = self.verify_landed_deliverables(&prepared.deliverables)?;
        let items = results
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut guard = self
            .always_on
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.land_deltas = items.iter().map(|(d, _)| *d).collect();
        guard.land_cas_retries = items.iter().map(|(_, r)| *r).sum();
        guard.merged_files_verified = merged_files_verified;
        Ok(())
    }

    fn build_initial_crash_manifest(
        &self,
        workers: &[WorkerState],
        ack_path: &Path,
    ) -> Result<(CrashManifest, usize), String> {
        let expected_main = self
            .coord_store
            .main_snapshot()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "main snapshot missing before crash drill".to_string())?;
        let mut expected_branches = Vec::with_capacity(workers.len());
        for w in workers {
            let head = self
                .coord_store
                .branch_snapshot(w.branch.as_str())
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("branch {} missing before crash drill", w.branch))?;
            expected_branches.push((w.branch.as_str().to_string(), head.id.to_string()));
        }
        let pre_journal_len = self
            .cas
            .cas()
            .read_ref_journal()
            .map_err(|e| e.to_string())?
            .len();
        let manifest = CrashManifest {
            project: self.project.clone(),
            schema: self.schema.clone(),
            cas_root: self.cas_root.clone(),
            disk_root: self.disk_root.clone(),
            ack_path: ack_path.to_path_buf(),
            expected_main: expected_main.id.to_string(),
            expected_branches,
            expected_crash_branches: Vec::new(),
            acked_snapshots: Vec::new(),
            min_journal_entries: pre_journal_len,
            torn_tail_bytes: 0,
        };
        Ok((manifest, pre_journal_len))
    }

    fn spawn_and_sigkill_crash_writer(
        exe: &Path,
        manifest_path: &Path,
        ack_path: &Path,
        pg_url: &str,
        target_acks: usize,
    ) -> Result<(u32, Vec<CrashAck>), String> {
        let mut writer = Command::new(exe)
            .arg("--internal-crash-writer")
            .arg(manifest_path)
            .env("SPAWN_BENCH_DATABASE_URL", pg_url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn crash writer child: {e}"))?;
        let killed_child_pid = writer.id();
        if killed_child_pid == std::process::id() {
            return Err("crash writer child PID unexpectedly matched parent".to_string());
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = writer.try_wait().map_err(|e| e.to_string())? {
                let mut err_buf = String::new();
                if let Some(mut se) = writer.stderr.take() {
                    let _ = se.read_to_string(&mut err_buf);
                }
                return Err(format!(
                    "crash writer exited early ({status}) before SIGKILL: {}",
                    err_buf.trim()
                ));
            }
            let acks = read_complete_crash_acks(ack_path)?;
            if acks.len() >= target_acks {
                break;
            }
            if Instant::now() >= deadline {
                let _ = writer.kill();
                let _ = writer.wait();
                return Err(format!(
                    "timed out waiting for {target_acks} committed acks from crash writer (got {})",
                    acks.len()
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        writer
            .kill()
            .map_err(|e| format!("send SIGKILL to crash writer pid {killed_child_pid}: {e}"))?;
        let kill_status = writer
            .wait()
            .map_err(|e| format!("wait killed crash writer: {e}"))?;
        let exit_signal = kill_status.signal();
        if exit_signal != Some(9) {
            return Err(format!(
                "crash writer did not terminate via SIGKILL (expected signal 9, got {exit_signal:?}, status {kill_status})"
            ));
        }

        let acks = read_complete_crash_acks(ack_path)?;
        if acks.len() < target_acks {
            return Err(format!(
                "lost acked commits before SIGKILL: expected >= {target_acks}, got {}",
                acks.len()
            ));
        }
        Ok((killed_child_pid, acks))
    }

    fn run_crash_verifier_child(
        &self,
        exe: &Path,
        manifest_path: &Path,
        killed_child_pid: u32,
        manifest: &CrashManifest,
    ) -> Result<Value, String> {
        let ver_out = Command::new(exe)
            .arg("--internal-crash-verify")
            .arg(manifest_path)
            .env("SPAWN_BENCH_DATABASE_URL", &self.pg_url)
            .output()
            .map_err(|e| format!("spawn crash verifier child: {e}"))?;
        if !ver_out.status.success() {
            return Err(format!(
                "separate verifier process failed ({}): stderr={}; stdout={}",
                ver_out.status,
                String::from_utf8_lossy(&ver_out.stderr).trim(),
                String::from_utf8_lossy(&ver_out.stdout).trim(),
            ));
        }
        let mut report: Value = serde_json::from_slice(&ver_out.stdout).map_err(|e| {
            format!(
                "parse verifier JSON failed ({e}): {}",
                String::from_utf8_lossy(&ver_out.stdout)
            )
        })?;
        let verifier_pid = report["verifier_pid"].as_u64().unwrap_or(0);
        if verifier_pid == 0
            || verifier_pid == u64::from(std::process::id())
            || verifier_pid == u64::from(killed_child_pid)
        {
            return Err(format!(
                "invalid verifier_pid={verifier_pid} (parent={}, killed={killed_child_pid})",
                std::process::id()
            ));
        }
        if let Value::Object(map) = &mut report {
            map.insert("sigkill_executed".to_string(), json!(true));
            map.insert("killed_child_pid".to_string(), json!(killed_child_pid));
            map.insert("exit_signal".to_string(), json!(9));
            map.insert(
                "committed_before_kill".to_string(),
                json!(manifest.acked_snapshots.len()),
            );
        }
        Ok(report)
    }

    fn run_crash_restart_drill(&self, workers: &[WorkerState]) -> Result<(), String> {
        let manifest_path = self.scratch_root.join("crash-manifest.json");
        let ack_path = self.scratch_root.join("crash-acks.jsonl");
        let _ = fs::remove_file(&ack_path);
        let (mut manifest, pre_journal_len) =
            self.build_initial_crash_manifest(workers, &ack_path)?;
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("write crash manifest: {e}"))?;

        let exe = env::current_exe().map_err(|e| format!("resolve current_exe: {e}"))?;
        let (killed_child_pid, acks) =
            Self::spawn_and_sigkill_crash_writer(&exe, &manifest_path, &ack_path, &self.pg_url, 6)?;
        let mut latest_by_branch: BTreeMap<String, String> = BTreeMap::new();
        let mut acked_snapshots_set = BTreeSet::new();
        for ack in &acks {
            latest_by_branch.insert(ack.branch.clone(), ack.snapshot.clone());
            acked_snapshots_set.insert(ack.snapshot.clone());
        }
        let torn_tail_bytes = inject_torn_journal_tail(&self.cas.cas().refs_journal_path())?;

        manifest.expected_crash_branches = latest_by_branch.into_iter().collect();
        manifest.acked_snapshots = acked_snapshots_set.into_iter().collect();
        manifest.min_journal_entries = pre_journal_len.saturating_add(acks.len());
        manifest.torn_tail_bytes = torn_tail_bytes;
        let manifest_json = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        fs::write(&manifest_path, &manifest_json)
            .map_err(|e| format!("update crash manifest: {e}"))?;
        if let Some(save_path) = &self.save_crash_manifest {
            if let Some(parent) = save_path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::write(save_path, &manifest_json)
                .map_err(|e| format!("write saved crash manifest {}: {e}", save_path.display()))?;
        }

        let report =
            self.run_crash_verifier_child(&exe, &manifest_path, killed_child_pid, &manifest)?;
        let verified = report["verified"].as_bool() == Some(true);
        {
            let mut guard = self
                .always_on
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.crash_drill = Some(report.clone());
        }
        if !verified {
            return Err(format!("crash-restart drill failed verification: {report}"));
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
        let mut branch_written = 0_u64;
        for file_idx in 0..BRANCH_INIT_FILES {
            let path = Path::new(ws.path()).join(format!("branches/w{idx:03}/init_{file_idx}.dat"));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let seed = ((idx as u64).wrapping_add(1) << 32)
                ^ ((file_idx as u64).wrapping_add(1))
                ^ 0x494E_4954;
            let marker = format!("// branch-init worker={idx} slot={file_idx}\n");
            let payload = fill_mixed_payload(seed, &marker, config.file_bytes);
            branch_written =
                branch_written.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
            fs::write(&path, payload).map_err(|e| e.to_string())?;
        }
        if config.oversized_file_bytes.is_some() {
            let base_path = Path::new(ws.path()).join("oversized.bin");
            let base = fs::read(&base_path)
                .map_err(|e| format!("read mounted oversized.bin {}: {e}", base_path.display()))?;
            let shifted = build_shifted_oversized_payload(&base, idx);
            let shift_path =
                Path::new(ws.path()).join(format!("branches/w{idx:03}/oversized_shift.bin"));
            if let Some(parent) = shift_path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            branch_written =
                branch_written.saturating_add(u64::try_from(shifted.len()).unwrap_or(u64::MAX));
            fs::write(&shift_path, shifted).map_err(|e| format!("write shifted file: {e}"))?;
        }
        self.logical_written_bytes
            .fetch_add(branch_written, Ordering::Relaxed);
        self.logical_workspace_bytes.fetch_add(
            self.base_main_bytes.saturating_add(branch_written),
            Ordering::Relaxed,
        );
        if !matches!(
            store
                .commit(&ws, "benchmark: seed unique branch files")
                .map_err(|e| e.to_string())?,
            Commit::Created(_)
        ) {
            return Err("seeded D6 workspace produced no commit".to_string());
        }
        if let Some(sz) = config.oversized_file_bytes {
            let _ = self.verify_oversized_chunked_in_cas(branch, sz)?;
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
            let _ = self.verify_oversized_chunked_in_cas(branch, sz)?;
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

    fn post_merge_phase(&self, config: &Config, workers: &mut [WorkerState]) -> Result<(), String> {
        if config.land_branches {
            self.run_land_phase(config, workers)?;
        }
        if config.crash_restart_drill {
            self.run_crash_restart_drill(workers)?;
        }
        Ok(())
    }

    fn verify_final(&self, branches: &[BranchName], config: &Config) -> Result<(), String> {
        let mut unique_chunked = BTreeSet::new();
        let mut total_chunks = 0_usize;
        let mut shifted_entries = Vec::with_capacity(branches.len());
        for b in branches {
            if let Some(sz) = config.oversized_file_bytes {
                let verified = self.verify_oversized_chunked_in_cas(b, sz)?;
                if verified.base.chunks > 0 && unique_chunked.insert(verified.base.digest) {
                    total_chunks = total_chunks.saturating_add(verified.base.chunks);
                }
                if verified.shifted.chunks > 0 {
                    if unique_chunked.insert(verified.shifted.digest.clone()) {
                        total_chunks = total_chunks.saturating_add(verified.shifted.chunks);
                    }
                    shifted_entries.push((verified.shifted.digest, verified.shifted.size));
                }
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
        let cdc_metrics = measure_shifted_cdc_dedup(self.cas.cas(), &shifted_entries)?;
        let journal = self
            .cas
            .cas()
            .read_ref_journal()
            .map_err(|e| e.to_string())?;
        if journal.is_empty() {
            return Err("refs.journal is empty after D6 benchmark".to_string());
        }
        let mut guard = self
            .always_on
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.fastcdc_chunked_files = unique_chunked.len();
        guard.fastcdc_chunks_count = total_chunks;
        guard.cdc_shifted_files = cdc_metrics.shifted_files;
        guard.cdc_shifted_logical_bytes = cdc_metrics.shifted_logical_bytes;
        guard.cdc_unique_stored_bytes = cdc_metrics.cdc_unique_stored_bytes;
        guard.cdc_fixed_block_unique_bytes = cdc_metrics.fixed_block_unique_bytes;
        guard.cdc_dedup_ratio = cdc_metrics.cdc_dedup_ratio;
        guard.cdc_fixed_block_dedup_ratio = cdc_metrics.fixed_block_dedup_ratio;
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

    fn record_edit_bytes(&self, bytes: u64) {
        self.logical_written_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    fn logical_byte_totals(&self) -> (u64, u64) {
        (
            self.logical_workspace_bytes.load(Ordering::Relaxed),
            self.logical_written_bytes.load(Ordering::Relaxed),
        )
    }

    fn always_on_d6_json(
        &self,
        config: &Config,
        logical_written_bytes: u64,
    ) -> Result<Option<Value>, String> {
        if !config.land_branches && !config.crash_restart_drill {
            return Ok(None);
        }
        let cas_bytes = cas_payload_bytes(&self.cas_root)?;
        if logical_written_bytes > 0 && cas_bytes == 0 {
            return Err("cas_payload_bytes measured 0 after non-empty D6 workload".to_string());
        }
        let guard = self
            .always_on
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dedup_ratio = u64_to_f64(logical_written_bytes) / u64_to_f64(cas_bytes.max(1));
        let cdc_metrics = ShiftedCdcMetrics {
            shifted_files: guard.cdc_shifted_files,
            shifted_logical_bytes: guard.cdc_shifted_logical_bytes,
            cdc_unique_stored_bytes: guard.cdc_unique_stored_bytes,
            fixed_block_unique_bytes: guard.cdc_fixed_block_unique_bytes,
            cdc_dedup_ratio: guard.cdc_dedup_ratio,
            fixed_block_dedup_ratio: guard.cdc_fixed_block_dedup_ratio,
        };
        let cdc_slo_passed = evaluate_cdc_dedup_slo(cdc_metrics, config.concurrency);
        let landings = guard.land_deltas.len();
        let total_delta: u64 = guard.land_deltas.iter().copied().sum();
        let max_delta = guard.land_deltas.iter().copied().max().unwrap_or(0);
        let mean_delta = if landings == 0 {
            0.0
        } else {
            u64_to_f64(total_delta) / u64_to_f64(u64::try_from(landings).unwrap_or(1))
        };
        let slo_passed = evaluate_land_delta_slo(
            landings,
            if config.land_branches {
                config.concurrency
            } else {
                0
            },
            guard.merged_files_verified,
            max_delta,
        );
        Ok(Some(json!({
            "expected_landings": if config.land_branches { config.concurrency } else { 0 },
            "landings_completed": landings,
            "land_cas_retries": guard.land_cas_retries,
            "merged_files_verified": guard.merged_files_verified,
            "bytes_per_landing": mean_delta,
            "max_bytes_per_landing": max_delta,
            "total_land_delta_bytes": total_delta,
            "slo_bytes_per_landing_limit": LAND_DELTA_SLO_BYTES,
            "slo_bytes_per_landing_passed": slo_passed,
            "logical_written_bytes": logical_written_bytes,
            "cas_stored_bytes": cas_bytes,
            "dedup_ratio": dedup_ratio,
            "cdc_shifted_files": guard.cdc_shifted_files,
            "cdc_shifted_logical_bytes": guard.cdc_shifted_logical_bytes,
            "cdc_unique_stored_bytes": guard.cdc_unique_stored_bytes,
            "cdc_fixed_block_unique_bytes": guard.cdc_fixed_block_unique_bytes,
            "cdc_dedup_ratio": guard.cdc_dedup_ratio,
            "cdc_fixed_block_dedup_ratio": guard.cdc_fixed_block_dedup_ratio,
            "slo_cdc_dedup_passed": cdc_slo_passed,
            "fastcdc_chunked_files": guard.fastcdc_chunked_files,
            "fastcdc_chunks_count": guard.fastcdc_chunks_count,
            "crash_restart_drill": guard.crash_drill,
        })))
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
    logical_workspace_bytes: AtomicU64,
    logical_written_bytes: AtomicU64,
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
            let cap = if config.oversized_file_bytes.is_some() {
                u64::try_from(config.file_bytes).map_err(|_| "file_bytes overflow".to_string())?
            } else {
                spawn_host::DEFAULT_MAX_FILE_BYTES
            };
            let primary = GitWorkspace::new(&repo, &git_root, cap);
            let ledger = Arc::new(PgRefLedger::connect_schema(bench_url, schema)?);
            let mut shadow = D6Workspace::new(
                repo.to_string_lossy().into_owned(),
                &d6_root,
                cas.clone(),
                ledger,
                cap,
            );
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
            logical_workspace_bytes: AtomicU64::new(0),
            logical_written_bytes: AtomicU64::new(0),
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
        let written = seed_workspace(Path::new(ws.path()), config)?;
        self.logical_workspace_bytes
            .fetch_add(written, Ordering::Relaxed);
        self.logical_written_bytes
            .fetch_add(written, Ordering::Relaxed);
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

    fn record_edit_bytes(&self, bytes: u64) {
        self.logical_written_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    fn logical_byte_totals(&self) -> (u64, u64) {
        (
            self.logical_workspace_bytes.load(Ordering::Relaxed),
            self.logical_written_bytes.load(Ordering::Relaxed),
        )
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

        let edited_bytes = edit_workspace(
            Path::new(worker.workspace.path()),
            config,
            worker.idx,
            cycle,
            config.backend == Backend::D6,
        )?;
        driver.record_edit_bytes(edited_bytes);
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

fn connect_pg_pool(
    url: &str,
    schema: &str,
    count: usize,
) -> Result<Vec<Arc<PostgresStore>>, String> {
    let first =
        PostgresStore::connect_in_schema_with_options(url, schema, &ConnectOptions::default())
            .map_err(|e| format!("connect benchmark schema {schema}: {e}"))?;
    let mut pool = Vec::with_capacity(count.max(1));
    for _ in 1..count.max(1) {
        let conn = first
            .new_connection()
            .map_err(|e| format!("open pooled benchmark connection in {schema}: {e}"))?;
        pool.push(Arc::new(conn));
    }
    pool.insert(0, Arc::new(first));
    Ok(pool)
}

fn build_driver(
    config: &Config,
    scratch: &Scratch,
    pg_pool: Option<&[Arc<PostgresStore>]>,
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
            let cap = if config.oversized_file_bytes.is_some() {
                u64::try_from(config.file_bytes).map_err(|_| "overflow".to_string())?
            } else {
                spawn_host::DEFAULT_MAX_FILE_BYTES
            };
            let store = GitWorkspace::new(&repo, &workspaces_root, cap);
            Ok(Arc::new(GitDriver {
                repo,
                stage_seed,
                workspaces_root,
                store,
                logical_workspace_bytes: AtomicU64::new(0),
                logical_written_bytes: AtomicU64::new(0),
            }))
        }
        Backend::D6 => {
            let url = bench_url.ok_or_else(|| {
                "SPAWN_BENCH_DATABASE_URL is required for --backend d6".to_string()
            })?;
            let pool = pg_pool.ok_or_else(|| "PG pool missing for d6".to_string())?;
            let disk_root = scratch.root.join("workspaces");
            let cas_root = scratch.root.join("cas");
            fs::create_dir_all(&disk_root).map_err(|e| e.to_string())?;
            let cas =
                Arc::new(CasMerkleStore::open(&cas_root).map_err(|e| format!("open cas: {e}"))?);
            Ok(Arc::new(D6Driver::new(D6DriverInit {
                project: "proj-bench".to_string(),
                disk_root,
                cas_root,
                scratch_root: scratch.root.clone(),
                cas,
                pg_pool: pool,
                pg_url: url.to_string(),
                schema: schema.to_string(),
                config,
            })?))
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

#[allow(clippy::disallowed_methods)]
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

    let desired_pool = if config.pg_pool_size > 0 {
        config.pg_pool_size
    } else {
        config.concurrency.max(1)
    };
    let pg_pool = match &bench_url {
        Some(url) => Some(connect_pg_pool(url, &schema, desired_pool)?),
        None => None,
    };

    let driver = build_driver(
        config,
        &scratch,
        pg_pool.as_deref(),
        bench_url.as_deref(),
        &schema,
    )?;

    let mut workers = Vec::with_capacity(config.concurrency);
    for idx in 0..config.concurrency {
        let branch = worker_branch_name(idx, config.concurrency)?;
        let ws = driver.seed_branch(idx, &branch, config)?;
        let db = pg_pool.as_ref().map(|pool| pool[idx % pool.len()].clone());
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
    driver.post_merge_phase(config, &mut final_workers)?;
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
    let (logical_workspace_bytes, logical_written_bytes) = driver.logical_byte_totals();
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
            "pg_pool_size": (config.pg_pool_size > 0).then_some(config.pg_pool_size),
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
    if let Some(always_on) = driver.always_on_d6_json(config, logical_written_bytes)? {
        report["always_on_d6"] = always_on;
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
    if config.pg_pool_size > 0 {
        cfg_json["pg_pool_size"] = json!(config.pg_pool_size);
    }
    if config.land_branches {
        cfg_json["land_branches"] = json!(true);
        cfg_json["land_payload_bytes"] = json!(config.land_payload_bytes);
    }
    if config.crash_restart_drill {
        cfg_json["crash_restart_drill"] = json!(true);
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
    let mut raw_args = env::args().skip(1);
    match raw_args.next().as_deref() {
        Some("--internal-crash-writer") => {
            let Some(path) = raw_args.next() else {
                eprintln!("--internal-crash-writer requires a manifest path");
                std::process::exit(2);
            };
            if let Err(err) = internal_crash_writer_main(Path::new(&path)) {
                eprintln!("storage benchmark internal-crash-writer: {err}");
                std::process::exit(1);
            }
            return;
        }
        Some("--internal-crash-verify") => {
            let mut inject_torn = false;
            let mut manifest_arg = raw_args.next();
            if manifest_arg.as_deref() == Some("--inject-torn-tail") {
                inject_torn = true;
                manifest_arg = raw_args.next();
            }
            let Some(path) = manifest_arg else {
                eprintln!("--internal-crash-verify requires a manifest path");
                std::process::exit(2);
            };
            match internal_crash_verify_main(Path::new(&path), inject_torn) {
                Ok(report) => {
                    println!(
                        "{}",
                        serde_json::to_string(&report).expect("JSON verifier report")
                    );
                    let _ = std::io::stdout().flush();
                    if report["verified"].as_bool() != Some(true) {
                        std::process::exit(1);
                    }
                }
                Err(err) => {
                    eprintln!("storage benchmark internal-crash-verify: {err}");
                    std::process::exit(1);
                }
            }
            return;
        }
        _ => {}
    }

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

#[cfg(test)]
mod tests {
    use super::{
        LAND_DELTA_SLO_BYTES, TORN_JOURNAL_TAIL, build_shifted_oversized_payload,
        cas_payload_bytes, evaluate_cdc_dedup_slo, evaluate_land_delta_slo, fill_mixed_payload,
        inject_torn_journal_tail, is_snapshot_ancestor, measure_shifted_cdc_dedup,
        verify_reopened_objects,
    };
    use spawn_cas::{
        CommitActor, CommitIntent, CommitOrigin, CommitV2, NS_BLOB, Snapshot, Store, WorkBoundary,
        WorkChange, empty_tree_digest, sha256_of,
    };
    use spawn_host::d6_cutover::reconcile_refs_journal;
    use spawn_host::{
        CasMerkleStore, D6RefKind, InMemoryRefLedger, PublishRequest, RefLedger, format_commit_body,
    };
    use std::collections::BTreeSet;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "spawn-storage-bench-test-{tag}-{}-{nanos:x}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn cas_payload_bytes_fails_closed_when_unreadable_or_missing() {
        let dir = unique_tmp_dir("missing-cas");
        assert!(cas_payload_bytes(&dir).is_err());
        fs::write(dir.join("objects"), b"not-a-dir").expect("write file");
        assert!(cas_payload_bytes(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixed_payload_corpus_is_heterogeneous_across_seeds_and_blocks() {
        let a = fill_mixed_payload(1, "// branch-0\n", 16 * 1024);
        let b = fill_mixed_payload(2, "// branch-1\n", 16 * 1024);
        assert_ne!(sha256_of(&a), sha256_of(&b));
        let unique_bytes: BTreeSet<u8> = a.iter().copied().collect();
        assert!(
            unique_bytes.len() > 200,
            "expected high-entropy heterogeneous payload, got {} unique byte values",
            unique_bytes.len()
        );
    }

    #[test]
    fn torn_journal_tail_tolerated_by_production_journal_and_reconcile_without_truncation() {
        let dir = unique_tmp_dir("torn-journal-reconcile");
        let cas = CasMerkleStore::open(&dir).expect("open cas");
        let writer_ledger = InMemoryRefLedger::new();
        let lagging_ledger = InMemoryRefLedger::new();
        let empty_tree = empty_tree_digest();
        let body = format_commit_body(
            "spawn <bench@localhost> 1700000010 +0000",
            "spawn <bench@localhost> 1700000010 +0000",
            "torn journal unit test\n",
        );

        for seq in 1..=3_u64 {
            let (blob_d, blob_len) = cas
                .put_bytes_blob(format!("branch-payload-{seq}").as_bytes())
                .expect("put blob");
            let tree = cas
                .apply_mutations(
                    &empty_tree,
                    &[spawn_cas::TreeMutation::Upsert {
                        path: vec![format!("file_{seq}.txt").into_bytes()],
                        kind: spawn_cas::TreeEntryKind::File,
                        id: blob_d.as_bytes(),
                        size: blob_len,
                    }],
                )
                .expect("apply tree");
            let branch = format!("b{seq}");
            let pub_res = cas
                .publish_snapshot(
                    &writer_ledger,
                    &PublishRequest {
                        project: "proj-bench",
                        ref_name: &branch,
                        kind: D6RefKind::Branch,
                        expected_ref: None,
                        tree: &tree,
                        parents: &[],
                        commit_body: &body,
                        created_at_ms: 1_700_000_000_000 + i64::try_from(seq).expect("i64"),
                        reason: "unit-test",
                        landing_task_id: None,
                    },
                )
                .expect("publish snapshot");
            if seq == 1 {
                let _ = lagging_ledger
                    .record_snapshot(
                        &pub_res.snapshot,
                        &pub_res.git_oid,
                        &pub_res.tree,
                        &[],
                        "proj-bench",
                        1_700_000_000_001,
                    )
                    .expect("record seq 1 snapshot");
                let lseq = lagging_ledger
                    .cas_ref(
                        "proj-bench",
                        &branch,
                        D6RefKind::Branch,
                        None,
                        &pub_res.snapshot,
                        "unit-test",
                    )
                    .expect("cas_ref seq 1");
                assert_eq!(lseq, 1);
            }
        }

        let jpath = cas.cas().refs_journal_path();
        let tail_len = inject_torn_journal_tail(&jpath).expect("inject torn tail");
        assert!(tail_len > 0);
        let raw_before = fs::read(&jpath).expect("read torn journal");
        assert!(raw_before.ends_with(TORN_JOURNAL_TAIL));

        let entries = cas.cas().read_ref_journal().expect("read_ref_journal");
        assert_eq!(entries.len(), 3);

        let report = reconcile_refs_journal(&cas, &lagging_ledger, 1, "unit-test:reconcile")
            .expect("reconcile_refs_journal");
        assert_eq!(report.entries_considered, 2);
        assert_eq!(report.entries_replayed, 2);
        assert_eq!(report.last_journal_seq, 3);
        assert_eq!(
            lagging_ledger
                .list_refs(Some("proj-bench"))
                .expect("list lagging refs")
                .len(),
            3
        );
        let raw_after = fs::read(&jpath).expect("reread torn journal without truncation");
        assert!(raw_after.ends_with(TORN_JOURNAL_TAIL));

        // Now execute a production publish_snapshot after the torn tail: PR #276 must heal
        // the unterminated trailing fragment on disk under flock before appending seq=4.
        let (blob4, blob4_len) = cas
            .put_bytes_blob(b"branch-payload-4-after-torn")
            .expect("put blob 4");
        let tree4 = cas
            .apply_mutations(
                &empty_tree,
                &[spawn_cas::TreeMutation::Upsert {
                    path: vec![b"file_4.txt".to_vec()],
                    kind: spawn_cas::TreeEntryKind::File,
                    id: blob4.as_bytes(),
                    size: blob4_len,
                }],
            )
            .expect("apply tree 4");
        let pub4 = cas
            .publish_snapshot(
                &writer_ledger,
                &PublishRequest {
                    project: "proj-bench",
                    ref_name: "b4",
                    kind: D6RefKind::Branch,
                    expected_ref: None,
                    tree: &tree4,
                    parents: &[],
                    commit_body: &body,
                    created_at_ms: 1_700_000_000_004,
                    reason: "unit-test-heal",
                    landing_task_id: None,
                },
            )
            .expect("publish snapshot 4 over torn tail");
        assert!(!pub4.snapshot.hex().is_empty());

        let raw_healed = fs::read(&jpath).expect("reread healed refs.journal");
        assert!(raw_healed.ends_with(b"\n"));
        assert!(!raw_healed.ends_with(TORN_JOURNAL_TAIL));
        assert!(
            !raw_healed
                .windows(TORN_JOURNAL_TAIL.len())
                .any(|w| w == TORN_JOURNAL_TAIL)
        );

        let healed_entries = cas
            .cas()
            .read_ref_journal()
            .expect("read_ref_journal after heal");
        assert_eq!(healed_entries.len(), 4);
        assert_eq!(healed_entries.last().map(|e| e.seq), Some(4));

        let report_post =
            reconcile_refs_journal(&cas, &lagging_ledger, 3, "unit-test:reconcile-post-heal")
                .expect("reconcile_refs_journal after heal");
        assert_eq!(report_post.entries_considered, 1);
        assert_eq!(report_post.entries_replayed, 1);
        assert_eq!(report_post.last_journal_seq, 4);
        assert_eq!(
            lagging_ledger
                .list_refs(Some("proj-bench"))
                .expect("list lagging refs after heal")
                .len(),
            4
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fastcdc_shift_edit_dedup_beats_fixed_block_and_rejects_incompressible_corpus() {
        let dir = unique_tmp_dir("cdc-shift-dedup");
        let cas = Store::open(&dir).expect("open store");
        let base = fill_mixed_payload(0x4F56_4552_5349_5A45, "BINv1\n", 5 * 1024 * 1024);

        let mut shifted_entries = Vec::with_capacity(4);
        for idx in 0..4_usize {
            let shifted = build_shifted_oversized_payload(&base, idx);
            let mut cursor = shifted.as_slice();
            let stored = cas
                .put_blob(NS_BLOB, &mut cursor)
                .expect("put shifted blob");
            shifted_entries.push((stored.digest, stored.size));
        }
        let shifted_metrics =
            measure_shifted_cdc_dedup(&cas, &shifted_entries).expect("measure shifted cdc");
        assert!(
            evaluate_cdc_dedup_slo(shifted_metrics, 4),
            "expected real shift-edit FastCDC >=2.0x vs fixed-block <1.2x, got cdc={:.2}x fixed={:.2}x",
            shifted_metrics.cdc_dedup_ratio,
            shifted_metrics.fixed_block_dedup_ratio
        );
        assert!(shifted_metrics.cdc_dedup_ratio >= 2.0);
        assert!(shifted_metrics.fixed_block_dedup_ratio < 1.2);

        // Incompressible corpus: independent random payloads per branch must fail NO-GO.
        let mut random_entries = Vec::with_capacity(4);
        for idx in 0..4_usize {
            let rand_payload = fill_mixed_payload(
                0xBAD0_0000_u64.wrapping_add(idx as u64),
                &format!("// independent-{idx}\n"),
                4_500_000,
            );
            let mut cursor = rand_payload.as_slice();
            let stored = cas.put_blob(NS_BLOB, &mut cursor).expect("put random blob");
            random_entries.push((stored.digest, stored.size));
        }
        let rand_metrics =
            measure_shifted_cdc_dedup(&cas, &random_entries).expect("measure random cdc");
        assert!(
            !evaluate_cdc_dedup_slo(rand_metrics, 4),
            "incompressible corpus unexpectedly passed: cdc={:.2}x",
            rand_metrics.cdc_dedup_ratio
        );

        // Static identical-copy corpus: fixed-block dedup reaches ~4.0x and must fail NO-GO.
        let mut static_entries = Vec::with_capacity(4);
        let static_stored = cas
            .put_blob(NS_BLOB, &mut base.as_slice())
            .expect("put static blob");
        for _ in 0..4 {
            static_entries.push((static_stored.digest.clone(), static_stored.size));
        }
        let static_metrics =
            measure_shifted_cdc_dedup(&cas, &static_entries).expect("measure static cdc");
        assert!(
            !evaluate_cdc_dedup_slo(static_metrics, 4),
            "static copy cheating corpus unexpectedly passed: fixed={:.2}x",
            static_metrics.fixed_block_dedup_ratio
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn land_delta_slo_rejects_over_50_mib_and_empty_landings() {
        assert!(evaluate_land_delta_slo(8, 8, 8, 256 * 1024));
        assert!(!evaluate_land_delta_slo(8, 8, 8, LAND_DELTA_SLO_BYTES + 1));
        assert!(!evaluate_land_delta_slo(8, 8, 8, LAND_DELTA_SLO_BYTES));
        assert!(!evaluate_land_delta_slo(7, 8, 7, 256 * 1024));
        assert!(!evaluate_land_delta_slo(0, 8, 0, 0));
    }

    #[test]
    fn crash_verifier_helpers_support_v1_snapshots_and_commit_v2() {
        let dir = unique_tmp_dir("v1-v2-verify");
        let cas = CasMerkleStore::open(&dir).expect("open cas");
        let empty = empty_tree_digest();
        let make_tree = |name: &str, payload: &[u8]| {
            let (blob_d, blob_len) = cas.put_bytes_blob(payload).expect("put blob");
            cas.apply_mutations(
                &empty,
                &[spawn_cas::TreeMutation::Upsert {
                    path: vec![name.as_bytes().to_vec()],
                    kind: spawn_cas::TreeEntryKind::File,
                    id: blob_d.as_bytes(),
                    size: blob_len,
                }],
            )
            .expect("apply tree")
        };

        let body = format_commit_body(
            "spawn <bench@localhost> 1700000010 +0000",
            "spawn <bench@localhost> 1700000010 +0000",
            "v1 snapshot in mixed v1/v2 regression test\n",
        );
        let t_v1_root = make_tree("v1_root.txt", b"v1-root-payload");
        let v1_root = cas
            .cas()
            .put_merkle_snapshot(&Snapshot {
                tree: t_v1_root,
                parents: Vec::new(),
                commit_body: body.clone(),
            })
            .expect("put v1 root");

        let t_v1_child = make_tree("v1_child.txt", b"v1-child-payload");
        let v1_child = cas
            .cas()
            .put_merkle_snapshot(&Snapshot {
                tree: t_v1_child,
                parents: vec![v1_root.clone()],
                commit_body: body,
            })
            .expect("put v1 child");

        let make_v2 = |tree, parent, at_ms, msg: &str| {
            cas.cas()
                .put_commit_v2(&CommitV2 {
                    tree,
                    parents: vec![parent],
                    at_ms,
                    actor: CommitActor::System {
                        id: "bench-test".to_string(),
                    },
                    intent: CommitIntent {
                        origin: CommitOrigin::Work {
                            change: WorkChange::Edit,
                            boundary: WorkBoundary::Checkpoint,
                        },
                        task_id: None,
                        transcript_ref: None,
                        message: msg.to_string(),
                    },
                    git_compat_body: None,
                })
                .expect("put commit v2")
        };

        let t_v2_mid = make_tree("v2_mid.txt", b"v2-mid-payload");
        let v2_mid = make_v2(
            t_v2_mid,
            v1_child.clone(),
            1_700_000_000_003,
            "v2 mid checkpoint",
        );

        let t_v2_child = make_tree("v2_child.txt", b"v2-child-payload");
        let v2_child = make_v2(
            t_v2_child,
            v2_mid.clone(),
            1_700_000_000_004,
            "v2 child checkpoint",
        );

        assert!(is_snapshot_ancestor(&cas, &v1_child, &v1_root).expect("v1->v1 ancestor"));
        assert!(!is_snapshot_ancestor(&cas, &v1_root, &v1_child).expect("v1 reverse"));
        assert!(is_snapshot_ancestor(&cas, &v2_child, &v2_mid).expect("v2->v2 ancestor"));
        assert!(is_snapshot_ancestor(&cas, &v2_child, &v1_child).expect("v2->v1 child"));
        assert!(is_snapshot_ancestor(&cas, &v2_child, &v1_root).expect("v2->v1 root"));
        assert!(!is_snapshot_ancestor(&cas, &v1_root, &v2_child).expect("v1->v2 reverse"));

        let mut all_four = BTreeSet::new();
        all_four.insert(v1_root);
        all_four.insert(v1_child);
        all_four.insert(v2_mid);
        all_four.insert(v2_child);
        assert_eq!(verify_reopened_objects(&cas, &all_four), (0, 0, 4));

        all_four.insert(sha256_of(b"missing-commit-sentinel"));
        assert_eq!(verify_reopened_objects(&cas, &all_four), (1, 0, 4));

        let _ = fs::remove_dir_all(&dir);
    }
}

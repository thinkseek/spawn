//! Remote execution on spawn-node v1 (PR S4; design "spawn-node v1 and
//! host-adaptive storage" §3.4, §3.5, §6): what the integrator does for a
//! gate a node runs.
//!
//! - **Artifacts** ([`Artifacts`]): at enqueue the integrator writes, each
//!   named by its digest under `SPAWN_NODE_ARTIFACTS/<algo>/<2 hex>/<hex>`
//!   (the gateway serves that directory), the `delta_bundle` (`git bundle
//!   create` of the candidate with the base as its prerequisite), the
//!   `baseline_bundle` (a whole bundle of the base, cached per base), the
//!   `controller_pack` (a deterministic ustar of the controller's
//!   `scripts/`, its scenarios directory and `web/package-lock.json`,
//!   cached per controller and set) and the registry pack with its
//!   `registry-manifest` (`tree_sha256_v1` of `$CARGO_HOME/registry` and
//!   `$CARGO_HOME/git`, cargo's lock files skipped).
//! - **The grant's environment** ([`grant_env`]): [`gate::GATE_ENV`] minus
//!   what the node sets itself ([`NODE_OWNED_ENV`]: `PATH`, `HOME`,
//!   `TMPDIR`, `CARGO_HOME`, `RUSTUP_HOME`, `SPAWN_TEST_DATABASE_URL`,
//!   `SPAWN_GATE_PG_IMAGE`, `SPAWN_GATE_PG_BIN`). A node never receives a
//!   database URL: a value that names one is dropped too.
//! - **Queue and fallback** ([`step`], [`wait_for_lease`]): a job is queued
//!   with `spawn_remote_job_enqueue`; a job no node leased within
//!   `SPAWN_REMOTE_QUEUE_WAIT_SECS` (30 s) is taken back with
//!   `spawn_remote_job_take_local`, the fenced compare-and-set a node's
//!   result also goes through: exactly one of the two wins.
//! - **A stored result** ([`read_stored`], [`judge_stored`]): the gateway's
//!   `SPAWN_NODE_RESULTS/<job>.<epoch>/{meta.json,log}` is read back into a
//!   [`gate::Run`] (the log split at `stdout_bytes`, so stderr's lines are
//!   marked `! ` as for a local run) and the verdict file's bytes, and
//!   judged by the very [`gate::decide`] a local gate's run is. Its outcome
//!   is the row of §6.2 ([`Judged`]): a verdict, or the node's fault with
//!   or without a strike, and where the job goes next
//!   (`spawn_remote_job_fault`, [`settle`]). Nothing measures this host for
//!   a remote run: a node's exit 5 without its fact is an abort, never a
//!   `no_room` from the primary's own disk (NB9).
//! - **The sweep** ([`sweep`]): `spawn_node_sweep`, from the integrator's
//!   pass (never from `spawn loop`).
//!
//! A remote run never records an abort, a `host_fault` or a `no_room` for a
//! task: those count toward halts and back-offs, and a node's trouble must
//! not spend a mission's budget. Only a local gate's outcome does. What a
//! remote run leaves on the record is the ledger's own `remote_job.*`
//! records and, for a shadow, `task.shadow_gated`; a remote job names the
//! judging it serves `judging_id`, never `judging` (the unique index
//! `spawn_events_control_judging_outcome` keys on `record.judging`, F11).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use spawn_node_proto::ContentDigest;
use spawn_node_proto::grant::{ArtifactRole, ArtifactUrl, NODE_OWNED_ENV};
use spawn_node_proto::result::{ResultMeta, Signal};
use spawn_postgres::{
    FaultNext, FaultOutcome, PostgresStore, RemoteJob, RemoteJobKind, RemoteJobSpec,
    RemoteJobState, SweepOutcome,
};

use crate::gate::{self, GateFailure, Shortfall};

/// How long a queued job waits for a lease before the primary takes it
/// back, in whole seconds (default [`DEFAULT_QUEUE_WAIT`]).
pub(super) const QUEUE_WAIT_ENV: &str = "SPAWN_REMOTE_QUEUE_WAIT_SECS";

/// The default of [`QUEUE_WAIT_ENV`] (design §6.1, `remote_queue_wait_s`).
pub(super) const DEFAULT_QUEUE_WAIT: Duration = Duration::from_secs(30);

/// The artifact directory the gateway serves (shared with it).
pub(super) const ARTIFACTS_ENV: &str = "SPAWN_NODE_ARTIFACTS";

/// The results directory the gateway writes (shared with it).
pub(super) const RESULTS_ENV: &str = "SPAWN_NODE_RESULTS";

/// A remote gate's wall clock, as `deploy/gate-runner.sh`'s (the daemon
/// enforces it and reports `timed_out`).
pub(super) const GATE_TIMEOUT_S: u64 = 7200;

/// What a gate takes of a node: one gate sandbox and its tmpfs, the 16 GiB
/// cap plus 4 GiB headroom (F3).
pub(super) const GATE_TMPFS_MB: i64 = 20_480;

/// The free-space floor a node's gate checks in its tmpfs (F4: a 16 GiB
/// cap, nothing seeded, a floor of 8).
pub(super) const NODE_MIN_FREE_GB: &str = "8";

/// The major version of the gate's private PostgreSQL a node must hold.
pub(super) const PG_MAJOR: &str = "16";

/// The name of the registry manifest in the artifact directory
/// (`GET /node/v1/registry-manifest`).
pub(super) const REGISTRY_MANIFEST: &str = "registry-manifest";

/// [`QUEUE_WAIT_ENV`]'s text as a wait: whole seconds from 1; anything
/// else is the default.
pub(super) fn queue_wait(text: Option<&str>) -> Duration {
    text.and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|secs| *secs >= 1)
        .map_or(DEFAULT_QUEUE_WAIT, Duration::from_secs)
}

/// Whether `value` names a database (a URL a node must never receive).
fn names_a_database(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("postgres://") || lower.contains("postgresql://")
}

/// The environment a grant carries: the gate's own ([`gate::GATE_ENV`],
/// read into `gate_env` by [`gate::gate_env`]) minus every variable the
/// node sets itself ([`NODE_OWNED_ENV`]; a grant naming one is refused by
/// the node), minus anything that names a database; with the node's floor
/// ([`NODE_MIN_FREE_GB`]) and a `LANG`.
pub(super) fn grant_env(gate_env: &[(String, String)]) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = gate_env
        .iter()
        .filter(|(name, _)| gate::GATE_ENV.contains(&name.as_str()))
        .filter(|(name, _)| !NODE_OWNED_ENV.contains(&name.as_str()))
        .filter(|(_, value)| !names_a_database(value))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    env.insert(
        "SPAWN_GATE_MIN_FREE_GB".to_string(),
        NODE_MIN_FREE_GB.to_string(),
    );
    env.entry("LANG".to_string())
        .or_insert_with(|| "C.UTF-8".to_string());
    env
}

/// An artifact written into the artifact directory: its digest and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Published {
    pub(super) digest: ContentDigest,
    pub(super) size: u64,
}

impl Published {
    /// The grant's entry for this artifact in `role` (a gateway path).
    pub(super) fn url(&self, role: ArtifactRole) -> ArtifactUrl {
        ArtifactUrl {
            role,
            digest: self.digest.to_string(),
            size: self.size,
            url: self.digest.artifact_path(),
        }
    }
}

/// The registry's manifest: its `tree_sha256_v1` and the pack a node
/// unpacks into `cargo-homes/<registry>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RegistryManifest {
    pub(super) registry: String,
    pub(super) pack: Published,
}

impl RegistryManifest {
    /// `GET /node/v1/registry-manifest`'s body.
    pub(super) fn to_json(&self) -> Value {
        json!({"registry": self.registry,
               "pack": {"digest": self.pack.digest.to_string(), "size": self.pack.size}})
    }
}

/// A fresh name for a scratch file or directory (this process, a counter
/// and the time), never reused.
fn unique(tag: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        ".{tag}-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        super::now_ms()
    )
}

/// The sha256 of a file, streamed, and its size.
fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((hex(&hasher.finalize()), size))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// A command's failure, in one line.
fn ran(command: &mut Command, what: &str) -> Result<Vec<u8>, String> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run {what}: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "{what} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The artifact directory: content-addressed files the gateway serves, and
/// the integrator's small caches of what it already wrote.
#[derive(Clone, Debug)]
pub(super) struct Artifacts {
    root: PathBuf,
}

impl Artifacts {
    pub(super) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Where the artifact of `digest` lives: `<root>/<algo>/<2 hex>/<hex>`.
    pub(super) fn path_of(&self, digest: &ContentDigest) -> PathBuf {
        self.root
            .join(digest.algo.as_str())
            .join(&digest.hex[..2])
            .join(&digest.hex)
    }

    /// A scratch path inside the directory (same filesystem: a rename
    /// moves it into place whole).
    fn scratch(&self, tag: &str) -> Result<PathBuf, String> {
        let dir = self.root.join(".tmp");
        std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        Ok(dir.join(unique(tag)))
    }

    /// Move `file` (a scratch file of this directory) into place by its
    /// digest; an artifact already there is kept (same bytes).
    pub(super) fn put_file(&self, file: &Path) -> Result<Published, String> {
        let (hex, size) = hash_file(file).map_err(|error| format!("{}: {error}", file.display()))?;
        let digest = ContentDigest::parse(&format!("sha256:{hex}"))
            .map_err(|error| format!("digest: {error:?}"))?;
        let target = self.path_of(&digest);
        if target.is_file() {
            let _ = std::fs::remove_file(file);
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            std::fs::rename(file, &target)
                .map_err(|error| format!("{}: {error}", target.display()))?;
        }
        Ok(Published { digest, size })
    }

    /// Write `bytes` as an artifact.
    pub(super) fn put_bytes(&self, bytes: &[u8]) -> Result<Published, String> {
        let scratch = self.scratch("bytes")?;
        std::fs::write(&scratch, bytes).map_err(|error| format!("{}: {error}", scratch.display()))?;
        self.put_file(&scratch)
    }

    fn cache_path(&self, kind: &str, key: &str) -> PathBuf {
        self.root.join(".cache").join(format!("{kind}-{key}"))
    }

    /// What a cache entry names, when its artifact is still there whole.
    fn cached(&self, kind: &str, key: &str) -> Option<Published> {
        let text = std::fs::read_to_string(self.cache_path(kind, key)).ok()?;
        let mut words = text.split_whitespace();
        let digest = ContentDigest::parse(words.next()?).ok()?;
        let size: u64 = words.next()?.parse().ok()?;
        let present = std::fs::metadata(self.path_of(&digest)).ok()?.len();
        (present == size).then_some(Published { digest, size })
    }

    fn remember(&self, kind: &str, key: &str, published: &Published) {
        let path = self.cache_path(kind, key);
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let scratch = parent.join(unique("entry"));
        if std::fs::write(
            &scratch,
            format!("{} {}\n", published.digest, published.size),
        )
        .is_ok()
        {
            let _ = std::fs::rename(&scratch, &path);
        }
    }

    /// A bare repository of refs alone, its objects borrowed from `repo`
    /// (`deploy/gate-runner.sh`'s recipe: `git bundle` wants refs, not bare
    /// ids), with `refs` set; removed by the caller.
    fn refs_repo(&self, repo: &Path, refs: &[(&str, &str)]) -> Result<PathBuf, String> {
        let objects = ran(
            Command::new("git")
                .arg("--git-dir")
                .arg(repo)
                .args(["rev-parse", "--absolute-git-dir"]),
            "git rev-parse",
        )?;
        let objects = PathBuf::from(String::from_utf8_lossy(&objects).trim()).join("objects");
        let dir = self.scratch("refs")?;
        let result = (|| {
            ran(
                Command::new("git").args(["init", "-q", "--bare"]).arg(&dir),
                "git init",
            )?;
            std::fs::write(
                dir.join("objects/info/alternates"),
                [objects.as_os_str().as_bytes(), b"\n"].concat(),
            )
            .map_err(|error| format!("alternates: {error}"))?;
            for (name, commit) in refs {
                ran(
                    Command::new("git")
                        .arg("--git-dir")
                        .arg(&dir)
                        .args(["update-ref", name, commit]),
                    "git update-ref",
                )?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(dir),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                Err(error)
            }
        }
    }

    fn bundle(&self, repo: &Path, refs: &[(&str, &str)], args: &[&str]) -> Result<Published, String> {
        let dir = self.refs_repo(repo, refs)?;
        let out = self.scratch("bundle");
        let result = out.and_then(|out| {
            ran(
                Command::new("git")
                    .arg("--git-dir")
                    .arg(&dir)
                    .args(["bundle", "create", "-q"])
                    .arg(&out)
                    .args(args)
                    .stdout(Stdio::null()),
                "git bundle create",
            )?;
            self.put_file(&out)
        });
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    /// The `delta_bundle`: the candidate (`refs/gate/candidate`) with the
    /// base as its prerequisite; kilobytes. A candidate with nothing past
    /// the base has no delta.
    pub(super) fn delta_bundle(
        &self,
        repo: &Path,
        base: &str,
        candidate: &str,
    ) -> Result<Published, String> {
        if base == candidate {
            return Err("the candidate is the base: no delta to ship".to_string());
        }
        self.bundle(
            repo,
            &[("refs/gate/candidate", candidate)],
            &["refs/gate/candidate", &format!("^{base}")],
        )
    }

    /// The `baseline_bundle`: the base's whole history
    /// (`refs/gate/base`), made once per base and cached.
    pub(super) fn baseline_bundle(&self, repo: &Path, base: &str) -> Result<Published, String> {
        if let Some(published) = self.cached("baseline", base) {
            return Ok(published);
        }
        let published = self.bundle(repo, &[("refs/gate/base", base)], &["refs/gate/base"])?;
        self.remember("baseline", base, &published);
        Ok(published)
    }

    /// The `controller_pack`: a deterministic ustar (sorted names, mtime 0,
    /// owner and group 0) of the controller's `scripts/`, the scenario set
    /// `scenarios` under its place in the controller `scenarios_at` (e.g.
    /// `rsi/scenarios`), and `web/package-lock.json`. Cached under `key`
    /// when one is given (the controller's commit and the set's digest).
    pub(super) fn controller_pack(
        &self,
        controller: &Path,
        scenarios: &Path,
        scenarios_at: &Path,
        key: Option<&str>,
    ) -> Result<Published, String> {
        if scenarios_at.is_absolute()
            || scenarios_at
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
            || scenarios_at.as_os_str().is_empty()
        {
            return Err(format!(
                "the scenarios' place {} is not a relative path",
                scenarios_at.display()
            ));
        }
        if let Some(published) = key.and_then(|key| self.cached("controller", key)) {
            return Ok(published);
        }
        let stage = self.scratch("pack")?;
        let result = self.stage_and_pack(controller, scenarios, scenarios_at, &stage);
        super::shadow::remove_tree(&stage);
        let published = result?;
        if let Some(key) = key {
            self.remember("controller", key, &published);
        }
        Ok(published)
    }

    fn stage_and_pack(
        &self,
        controller: &Path,
        scenarios: &Path,
        scenarios_at: &Path,
        stage: &Path,
    ) -> Result<Published, String> {
        let copy = |from: &Path, to: &Path| -> Result<(), String> {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            ran(
                Command::new("cp").arg("-R").arg("--").arg(from).arg(to),
                "cp",
            )
            .map(|_| ())
        };
        std::fs::create_dir_all(stage).map_err(|error| format!("{}: {error}", stage.display()))?;
        let mut members: Vec<PathBuf> = Vec::new();
        if controller.join("scripts").is_dir() {
            copy(&controller.join("scripts"), &stage.join("scripts"))?;
            members.push(PathBuf::from("scripts"));
        }
        if !scenarios.is_dir() {
            return Err(format!("{} is not a directory", scenarios.display()));
        }
        copy(scenarios, &stage.join(scenarios_at))?;
        members.push(scenarios_at.to_path_buf());
        let lock = controller.join("web/package-lock.json");
        if lock.is_file() {
            copy(&lock, &stage.join("web/package-lock.json"))?;
            members.push(PathBuf::from("web/package-lock.json"));
        }
        // A deployed controller is read-only and cp keeps the modes: the
        // stage must stay removable.
        let _ = Command::new("chmod")
            .args(["-R", "u+rwX", "--"])
            .arg(stage)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        members.sort();
        self.pack(stage, &members)
    }

    /// A deterministic ustar of `members` (relative to `dir`) written as
    /// an artifact.
    fn pack(&self, dir: &Path, members: &[PathBuf]) -> Result<Published, String> {
        let out = self.scratch("tar")?;
        let result = ran(
            Command::new("tar")
                .env_remove("TAR_OPTIONS")
                .args([
                    "--format=ustar",
                    "--sort=name",
                    "--mtime=@0",
                    "--owner=0",
                    "--group=0",
                    "--numeric-owner",
                    "--exclude=.package-cache",
                    "--exclude=.package-cache-mutate",
                    "-C",
                ])
                .arg(dir)
                .arg("-cf")
                .arg(&out)
                .arg("--")
                .args(members),
            "tar",
        )
        .and_then(|_| self.put_file(&out));
        if result.is_err() {
            let _ = std::fs::remove_file(&out);
        }
        result
    }

    /// The registry's manifest: `tree_sha256_v1` of `$CARGO_HOME/registry`
    /// and `$CARGO_HOME/git` ([`registry_sha256`]) and its pack (cached per
    /// digest), written to [`REGISTRY_MANIFEST`] in the directory.
    pub(super) fn registry_manifest(&self, cargo_home: &Path) -> Result<RegistryManifest, String> {
        let registry = registry_sha256(cargo_home)
            .map_err(|error| format!("{}: {error}", cargo_home.display()))?;
        let pack = if let Some(pack) = self.cached("registry", &registry) {
            pack
        } else {
            let members: Vec<PathBuf> = ["git", "registry"]
                .into_iter()
                .filter(|name| cargo_home.join(name).is_dir())
                .map(PathBuf::from)
                .collect();
            if members.is_empty() {
                return Err(format!(
                    "{} has neither registry/ nor git/",
                    cargo_home.display()
                ));
            }
            let pack = self.pack(cargo_home, &members)?;
            self.remember("registry", &registry, &pack);
            pack
        };
        let manifest = RegistryManifest { registry, pack };
        let scratch = self.scratch("manifest")?;
        std::fs::write(&scratch, format!("{}\n", manifest.to_json()))
            .map_err(|error| format!("{}: {error}", scratch.display()))?;
        std::fs::rename(&scratch, self.root.join(REGISTRY_MANIFEST))
            .map_err(|error| format!("{REGISTRY_MANIFEST}: {error}"))?;
        Ok(manifest)
    }
}

/// `tree_sha256_v1` (design §7) over `$CARGO_HOME/registry` and
/// `$CARGO_HOME/git` together, paths relative to `$CARGO_HOME`, cargo's
/// lock files ([`spawn_node_proto::TREE_SKIP_REGISTRY`]) skipped: the digest
/// a node computes over the `cargo-homes/<registry>/` it unpacked the pack
/// into. A missing one of the two adds nothing.
pub(super) fn registry_sha256(cargo_home: &Path) -> std::io::Result<String> {
    let mut files: Vec<(Vec<u8>, PathBuf)> = Vec::new();
    for top in ["git", "registry"] {
        let dir = cargo_home.join(top);
        if dir.is_dir() {
            walk(&dir, Path::new(top), &mut files)?;
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut all = Sha256::new();
    for (relative, path) in files {
        let (digest, _) = hash_file(&path)?;
        all.update(digest.as_bytes());
        all.update(b"  ");
        all.update(&relative);
        all.update(b"\n");
    }
    Ok(hex(&all.finalize()))
}

fn walk(dir: &Path, relative: &Path, files: &mut Vec<(Vec<u8>, PathBuf)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = relative.join(entry.file_name());
        if kind.is_dir() {
            walk(&entry.path(), &name, files)?;
        } else if kind.is_file()
            && !spawn_node_proto::TREE_SKIP_REGISTRY
                .iter()
                .any(|skip| entry.file_name().as_bytes() == skip.as_bytes())
        {
            files.push((name.as_os_str().as_bytes().to_vec(), entry.path()));
        }
    }
    Ok(())
}

/// The `channel` of a controller's `rust-toolchain.toml`, when it names one.
pub(super) fn toolchain_channel(controller: &Path) -> Option<String> {
    let text = std::fs::read_to_string(controller.join("rust-toolchain.toml")).ok()?;
    text.lines().find_map(|line| {
        let value = line.trim().strip_prefix("channel")?.trim().strip_prefix('=')?;
        let value = value.trim().trim_matches('"').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// The sha256 of the controller's `web/package-lock.json`, when it has one.
pub(super) fn web_lock(controller: &Path) -> Option<String> {
    hash_file(&controller.join("web/package-lock.json"))
        .ok()
        .map(|(digest, _)| digest)
}

/// A fresh job id: `<prefix>-<the candidate's first 12 hex>-<8 random
/// lower-case letters and digits>` (`^[a-z0-9][a-z0-9._-]{7,63}$`).
pub(super) fn job_id(prefix: &str, candidate: &str) -> String {
    let mut random = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut random));
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let tail: String = random
        .iter()
        .map(|byte| char::from(alphabet[usize::from(*byte) % alphabet.len()]))
        .collect();
    let head: String = candidate
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();
    format!("{prefix}-{head}-{tail}")
}

/// What a remote gate is given: its immutable inputs (what the node must
/// judge, and say it judged), its environment and its artifacts.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct GateJob {
    pub(super) kind: RemoteJobKind,
    pub(super) job_id: String,
    /// The judging it serves (a remote job's field is `judging_id`, never
    /// `judging`: F11).
    pub(super) judging_id: Option<String>,
    pub(super) task_id: Option<i64>,
    /// The repository's name for the node's mirror.
    pub(super) repo: String,
    /// `{base, candidate, controller, scenarios_sha256, scenario}`.
    pub(super) inputs: Value,
    pub(super) env: BTreeMap<String, String>,
    pub(super) artifacts: Vec<ArtifactUrl>,
    /// What a node must hold (`registry`, `web_lock`, `toolchain`, `pg`).
    pub(super) requires: BTreeMap<String, String>,
}

impl GateJob {
    /// The job as `spawn_remote_job_enqueue` takes it: its `spec` is what
    /// the gateway puts in the grant beside the row's own fields.
    pub(super) fn spec(&self) -> RemoteJobSpec {
        let mut inputs = json!({"repo": self.repo});
        for field in super::inputs::FIELDS {
            inputs[field] = self.inputs.get(field).cloned().unwrap_or(Value::Null);
        }
        RemoteJobSpec {
            job_id: self.job_id.clone(),
            kind: self.kind,
            cell_id: None,
            priority: None,
            judging_id: self.judging_id.clone(),
            mission_id: None,
            task_id: self.task_id,
            spec: json!({
                "timeout_s": GATE_TIMEOUT_S,
                "inputs": inputs,
                "env": self.env,
                "artifact_urls": self.artifacts,
            }),
            requires: self.requires.clone(),
            cost: BTreeMap::from([
                ("gate_sandbox".to_string(), 1),
                ("tmpfs_mb".to_string(), GATE_TMPFS_MB),
            ]),
            base_commit: self.inputs["base"].as_str().map(str::to_string),
            max_attempts: None,
            queue_wait_ms: None,
        }
    }
}

/// Queue `job` (`spawn_remote_job_enqueue`).
pub(super) fn enqueue(store: &PostgresStore, job: &GateJob) -> Result<String, String> {
    store
        .remote_job_enqueue(&job.spec())
        .map_err(|error| error.to_string())
}

/// The level-triggered sweep (`spawn_node_sweep`), once per integrator
/// pass.
pub(super) fn sweep(store: &PostgresStore) -> Result<SweepOutcome, String> {
    store.node_sweep().map_err(|error| error.to_string())
}

/// Where a queued job is, read once ([`step`]).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Waited {
    /// Still queued, within its wait.
    Queued,
    /// A node holds it (leased, or its result is pending).
    Leased(i64),
    /// A node's result is stored: judge it.
    Completed(Box<RemoteJob>),
    /// No node leased it in time: the primary took it back (and runs it
    /// itself, for a gate).
    TakenLocal,
    /// It ended otherwise (fallback_local, cancelled, dead_letter, gone).
    Ended(Option<RemoteJobState>),
}

/// One look at job `job_id`, queued `since` and allowed to wait `wait` for
/// a lease: past the wait a job still queued is taken back
/// (`spawn_remote_job_take_local` at the epoch read; a node that won the
/// race keeps it).
pub(super) fn step(
    store: &PostgresStore,
    job_id: &str,
    since: Instant,
    wait: Duration,
) -> Result<Waited, String> {
    let Some(job) = store.remote_job(job_id).map_err(|error| error.to_string())? else {
        return Ok(Waited::Ended(None));
    };
    Ok(match job.state {
        RemoteJobState::Queued if since.elapsed() < wait => Waited::Queued,
        RemoteJobState::Queued => {
            if store
                .remote_job_take_local(job_id, job.lease_epoch)
                .map_err(|error| error.to_string())?
            {
                Waited::TakenLocal
            } else {
                // A node was granted it meanwhile: read it again.
                return step(store, job_id, Instant::now(), wait);
            }
        }
        RemoteJobState::Leased | RemoteJobState::ResultPending => Waited::Leased(job.lease_epoch),
        RemoteJobState::Completed => Waited::Completed(Box::new(job)),
        other => Waited::Ended(Some(other)),
    })
}

/// Wait for job `job_id` to be leased, up to `wait` from now, looking every
/// `every`; past it, take it back ([`step`]). Returns the first state that
/// is not `Queued`.
pub(super) fn wait_for_lease(
    store: &PostgresStore,
    job_id: &str,
    wait: Duration,
    every: Duration,
) -> Result<Waited, String> {
    let since = Instant::now();
    loop {
        match step(store, job_id, since, wait)? {
            Waited::Queued => std::thread::sleep(every.min(wait.saturating_sub(since.elapsed()))),
            other => return Ok(other),
        }
    }
}

/// A node's result as the gateway stored it.
#[derive(Clone, Debug)]
pub(super) struct Stored {
    pub(super) meta: ResultMeta,
    pub(super) log: Vec<u8>,
}

/// Read `<results>/<job_id>.<lease_epoch>/{meta.json,log}`: the metadata
/// line (strict, for this job at this epoch) and the log it binds.
pub(super) fn read_stored(results: &Path, job_id: &str, lease_epoch: i64) -> Result<Stored, String> {
    let dir = results.join(format!("{job_id}.{lease_epoch}"));
    let line = std::fs::read(dir.join("meta.json"))
        .map_err(|error| format!("{}: {error}", dir.join("meta.json").display()))?;
    let line = line.strip_suffix(b"\n").unwrap_or(&line);
    let meta = ResultMeta::parse(line).map_err(|error| format!("meta.json: {error:?}"))?;
    if meta.job_id != job_id || i64::try_from(meta.lease_epoch).ok() != Some(lease_epoch) {
        return Err(format!(
            "meta.json is for {} at epoch {}, not {job_id} at {lease_epoch}",
            meta.job_id, meta.lease_epoch
        ));
    }
    let log = std::fs::read(dir.join("log"))
        .map_err(|error| format!("{}: {error}", dir.join("log").display()))?;
    meta.check_log(&log)
        .map_err(|error| format!("log: {error:?}"))?;
    Ok(Stored { meta, log })
}

/// The exit status of a gate that exited `code` (its low eight bits, as a
/// process's status holds it).
fn exited(code: i64) -> ExitStatus {
    ExitStatus::from_raw(i32::try_from(code & 0xff).unwrap_or(0xff) << 8)
}

/// The [`gate::Run`] a stored result stands for: the gate's status, its
/// stdout and its stderr (the log split at `stdout_bytes`). None when the
/// gate has no status (it died of a signal, never started, or the node
/// said none).
pub(super) fn run_of(stored: &Stored) -> Option<gate::Run> {
    let code = stored.meta.exit_code?;
    let (stdout, stderr) = stored.meta.streams(&stored.log).ok()?;
    Some(gate::Run {
        status: exited(code),
        stdout: String::from_utf8_lossy(stdout).into_owned(),
        stderr: String::from_utf8_lossy(stderr).into_owned(),
    })
}

/// What a finished remote run means (design §6.2): a verdict to compare
/// (or, from M3, to believe), or the node's fault, struck or not, and
/// where the job goes next.
#[derive(Debug)]
pub(super) struct Judged {
    /// The row's name: `accepted`, `rejected`, `aborted`, `unequal_inputs`,
    /// `no_room`, `no_room_slot`, `base_disagrees`, `host_fault`,
    /// `validator_defect`, `main_red`, `cannot_judge`, `unread_version`,
    /// `signal`, `timed_out`, `wrapper_error`, `self_fenced`.
    pub(super) evidence: &'static str,
    /// The gate's verdict (exit 0 with its file), when the node judged the
    /// job's own inputs.
    pub(super) verdict: Option<gate::Verdict>,
    /// What [`gate::decide`] said otherwise.
    pub(super) failure: Option<GateFailure>,
    /// The verdict file's `verdict`, as written, even when it is not
    /// believed (another commit, unequal inputs).
    pub(super) stated: Option<String>,
    /// A node strike (three make it degraded).
    pub(super) strike: bool,
    /// Where the job goes (`spawn_remote_job_fault`): None for a verdict or
    /// a self-fenced run, which is judged by nothing here.
    pub(super) next: Option<FaultNext>,
    /// In words, for the log.
    pub(super) reason: String,
}

impl Judged {
    fn fault(
        evidence: &'static str,
        strike: bool,
        next: FaultNext,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            evidence,
            verdict: None,
            failure: None,
            stated: None,
            strike,
            next: Some(next),
            reason: reason.into(),
        }
    }

    fn with_failure(mut self, failure: GateFailure) -> Self {
        self.failure = Some(failure);
        self
    }
}

/// The `"v"` of a verdict file's fact when it is another than
/// [`gate::FACT_VERSION`] (a fact with none is read as that one).
fn unread_fact_version(verdict: Option<&str>) -> Option<Value> {
    let version = verdict
        .map(str::trim)
        .and_then(|line| serde_json::from_str::<Value>(line).ok())?
        .get("v")
        .cloned()?;
    (version != json!(gate::FACT_VERSION)).then_some(version)
}

/// The verdict file's `verdict`, when it states one.
fn stated_verdict(verdict: Option<&str>) -> Option<String> {
    let value: Value = serde_json::from_str(verdict?.trim()).ok()?;
    value["verdict"]
        .as_str()
        .filter(|verdict| matches!(*verdict, "accepted" | "rejected"))
        .map(str::to_string)
}

/// Judge a node's stored result for a job whose inputs are `inputs`
/// (`{base, candidate, controller, scenarios_sha256, scenario}`), as
/// [`gate::decide`] judges a local run and section 6.2 routes it.
#[allow(clippy::too_many_lines)] // one ordered list: the rows of §6.2
pub(super) fn judge_stored(stored: &Stored, inputs: &Value) -> Judged {
    let meta = &stored.meta;
    if meta.self_fenced {
        return Judged {
            evidence: "self_fenced",
            verdict: None,
            failure: None,
            stated: None,
            strike: false,
            next: None,
            reason: "the node fenced itself: the epoch has moved".to_string(),
        };
    }
    if let Some(error) = &meta.wrapper_error {
        return Judged::fault(
            "wrapper_error",
            true,
            FaultNext::Queued,
            format!("the gate never started on the node: {error}"),
        );
    }
    if meta.timed_out {
        return Judged::fault(
            "timed_out",
            true,
            FaultNext::FallbackLocal,
            "the gate ran past its wall clock on the node",
        );
    }
    let verdict = meta.verdict_raw.as_deref();
    let Some(run) = run_of(stored) else {
        let reason = match &meta.signal {
            Some(Signal::Number(number)) => format!("the gate died of signal {number}"),
            Some(Signal::Name(name)) => format!("the gate died of {name}"),
            None => "the node gave no exit status".to_string(),
        };
        let evidence = if meta.signal.is_some() {
            "signal"
        } else {
            "wrapper_error"
        };
        return Judged::fault(evidence, true, FaultNext::Queued, reason);
    };
    // A fact of a version this integrator does not read: a skew, not a
    // fault; judged locally.
    if let Some(version) = unread_fact_version(verdict) {
        let failure = gate::decide(&run, verdict).err();
        let mut judged = Judged::fault(
            "unread_version",
            false,
            FaultNext::FallbackLocal,
            format!("the node's gate wrote a fact of version {version}"),
        );
        judged.failure = failure;
        return judged;
    }
    let code = run.status.code();
    match gate::decide(&run, verdict) {
        Ok(decided) => {
            // Believed only when the node judged this job's own commits and
            // inputs: the verdict's base and candidate, then all five fields.
            let line: Value = verdict
                .and_then(|line| serde_json::from_str(line.trim()).ok())
                .unwrap_or(Value::Null);
            let stated = Some(if decided.accepted { "accepted" } else { "rejected" }.to_string());
            let commits_differ = ["base", "candidate"]
                .into_iter()
                .any(|field| line[field].as_str() != inputs[field].as_str());
            let theirs = meta
                .inputs
                .as_ref()
                .and_then(|inputs| serde_json::to_value(inputs).ok())
                .unwrap_or(Value::Null);
            let unequal = super::inputs::unequal(&theirs, inputs);
            if commits_differ || !unequal.is_empty() {
                let mut judged = Judged::fault(
                    "unequal_inputs",
                    true,
                    FaultNext::Queued,
                    if commits_differ {
                        "the node's verdict names other commits".to_string()
                    } else {
                        format!("the node judged other inputs: {}", unequal.join(", "))
                    },
                );
                judged.stated = stated;
                return judged;
            }
            Judged {
                evidence: if decided.accepted {
                    "accepted"
                } else {
                    "rejected"
                },
                stated,
                verdict: Some(decided),
                failure: None,
                strike: false,
                next: None,
                reason: "the node's gate gave a verdict".to_string(),
            }
        }
        Err(failure) => route_failure(code, failure, stated_verdict(verdict)),
    }
}

/// The rows of §6.2 for a run [`gate::decide`] gave no verdict for.
fn route_failure(code: Option<i32>, failure: GateFailure, stated: Option<String>) -> Judged {
    let mut judged = match (&failure, code) {
        (GateFailure::NoRoom { short, .. }, _) => match short {
            // A slot wait: the node's capacity, not its fault.
            Shortfall::Slot { .. } => Judged::fault(
                "no_room_slot",
                false,
                FaultNext::Queued,
                format!("no room on the node: {}", short.line()),
            ),
            // Its tmpfs budget was wrong: a strike.
            _ => Judged::fault(
                "no_room",
                true,
                FaultNext::Queued,
                format!("no room on the node: {}", short.line()),
            ),
        },
        (GateFailure::BaseDisagrees { scenario, .. }, _) => Judged::fault(
            "base_disagrees",
            true,
            FaultNext::Queued,
            format!("the base disagreed with itself on the node ({scenario})"),
        ),
        (GateFailure::HostFault { on, scenario, .. }, _) => {
            let elsewhere = on == "sandbox" || on == "test server";
            Judged::fault(
                "host_fault",
                true,
                if elsewhere {
                    FaultNext::Queued
                } else {
                    FaultNext::FallbackLocal
                },
                format!("a host fault on the node, on {on} ({scenario})"),
            )
        }
        (GateFailure::ValidatorDefect { scenario, .. }, _) => Judged::fault(
            "validator_defect",
            true,
            FaultNext::FallbackLocal,
            format!("a host fault on the node, on candidate and base ({scenario})"),
        ),
        // The primary confirms a red main itself.
        (GateFailure::MainRed { scenario, .. }, _) => Judged::fault(
            "main_red",
            false,
            FaultNext::FallbackLocal,
            format!("main is red on {scenario} on the node"),
        ),
        // The node cannot judge this scenario: excluded, no strike.
        (GateFailure::CannotJudge { scenario, .. }, _) => Judged::fault(
            "cannot_judge",
            false,
            FaultNext::FallbackLocal,
            format!("the node cannot judge {scenario}"),
        ),
        // Setup refused: struck and excluded, judged locally.
        (_, Some(3)) => Judged::fault(
            "aborted",
            true,
            FaultNext::FallbackLocal,
            "the node's gate refused its setup (exit 3)",
        ),
        // Exit 5 or 6 without a readable fact. Never measured here (NB9):
        // the primary's disk says nothing of the node's.
        (_, Some(gate::NO_ROOM_EXIT | gate::HOST_FAULT_EXIT)) => Judged::fault(
            "aborted",
            true,
            FaultNext::FallbackLocal,
            format!("the node's gate exited {} with no fact", code.unwrap_or(-1)),
        ),
        (_, Some(code)) if code >= 128 => Judged::fault(
            "signal",
            true,
            FaultNext::Queued,
            format!("the node's gate ended with {code} (signal {})", code - 128),
        ),
        // Exit 0 with no verdict, exit 2, exit 4 without its pair, any
        // other status: an abort, struck, on another node.
        _ => Judged::fault(
            "aborted",
            true,
            FaultNext::Queued,
            match &failure {
                GateFailure::Aborted { reason, .. } => format!("the node's gate: {reason}"),
                other => format!("the node's gate: {other:?}"),
            },
        ),
    };
    judged.stated = stated;
    judged.with_failure(failure)
}

/// Where a job of `kind` goes when the judging asked `next`: a shadow is
/// never run locally (it would decide nothing), so its row is kept.
pub(super) fn next_for(kind: RemoteJobKind, next: FaultNext) -> FaultNext {
    match (kind, next) {
        (RemoteJobKind::GateShadow, FaultNext::FallbackLocal | FaultNext::DeadLetter) => {
            FaultNext::Keep
        }
        _ => next,
    }
}

/// Apply a judging to the ledger: a fault (struck or not) is judged once
/// per completion by `spawn_remote_job_fault`, which also excludes the
/// node from the job, makes a third strike `degraded` and refunds the
/// strike of a poison pill. A verdict and a self-fenced run change nothing
/// here. Never anything toward a task's halts or back-offs.
pub(super) fn settle(
    store: &PostgresStore,
    job: &RemoteJob,
    judged: &Judged,
) -> Result<Option<FaultOutcome>, String> {
    let Some(next) = judged.next else {
        return Ok(None);
    };
    store
        .remote_job_fault(
            &job.job_id,
            job.lease_epoch,
            judged.evidence,
            judged.strike,
            next_for(job.kind, next),
        )
        .map(Some)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;

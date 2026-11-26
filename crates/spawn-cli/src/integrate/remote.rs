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
//!   `registry-manifest.json` (`tree_sha256_v1` of `$CARGO_HOME/registry` and
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

/// The name of the registry manifest in the artifact directory, the file
/// the gateway answers `GET /node/v1/registry-manifest` with (S3's
/// `artifacts.rs`, `docs/node-protocol-v1.md`).
pub(super) const REGISTRY_MANIFEST: &str = "registry-manifest.json";

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
        let (hex, size) =
            hash_file(file).map_err(|error| format!("{}: {error}", file.display()))?;
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

    /// A copy of `file` as an artifact (the file itself stays where it is):
    /// what a node fetches by a digest the job names, such as the
    /// controller's `web/package-lock.json` for `requires.web_lock`.
    pub(super) fn put_copy(&self, file: &Path) -> Result<Published, String> {
        let scratch = self.scratch("copy")?;
        if let Err(error) = std::fs::copy(file, &scratch) {
            let _ = std::fs::remove_file(&scratch);
            return Err(format!("{}: {error}", file.display()));
        }
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
            crate::sources::controller_git()
                .arg("--git-dir")
                .arg(repo)
                .args(["rev-parse", "--absolute-git-dir"]),
            "git rev-parse",
        )?;
        let objects = PathBuf::from(String::from_utf8_lossy(&objects).trim()).join("objects");
        let dir = self.scratch("refs")?;
        let result = (|| {
            ran(
                crate::sources::controller_git()
                    .args(["init", "-q", "--bare"])
                    .arg(&dir),
                "git init",
            )?;
            std::fs::write(
                dir.join("objects/info/alternates"),
                [objects.as_os_str().as_bytes(), b"\n"].concat(),
            )
            .map_err(|error| format!("alternates: {error}"))?;
            for (name, commit) in refs {
                ran(
                    crate::sources::controller_git()
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

    fn bundle(
        &self,
        repo: &Path,
        refs: &[(&str, &str)],
        args: &[&str],
    ) -> Result<Published, String> {
        let dir = self.refs_repo(repo, refs)?;
        let out = self.scratch("bundle");
        let result = out.and_then(|out| {
            ran(
                crate::sources::controller_git()
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
        let value = line
            .trim()
            .strip_prefix("channel")?
            .trim()
            .strip_prefix('=')?;
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
                // The grant's `requires`, as the gateway builds it from the
                // spec; the row's own copy schedules.
                "requires": self.requires,
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
    let Some(job) = store
        .remote_job(job_id)
        .map_err(|error| error.to_string())?
    else {
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
/// is not `Queued`. The primary path (`SPAWN_GATE_REMOTE=node`, PR S5)
/// follows its job with [`Primary::follow`] instead, which also sees a stop
/// and a completion; the shadows look once a pass ([`step`]), never waiting.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "a plain wait for a lease, tested here")
)]
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
pub(super) fn read_stored(
    results: &Path,
    job_id: &str,
    lease_epoch: i64,
) -> Result<Stored, String> {
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
/// said none, or one no process exits with).
pub(super) fn run_of(stored: &Stored) -> Option<gate::Run> {
    let code = stored
        .meta
        .exit_code
        .filter(|code| (0..=255).contains(code))?;
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
    /// `signal`, `timed_out`, `wrapper_error`, `self_fenced`,
    /// `unreadable_result`.
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

    /// A completed job whose stored result cannot be read (missing, not
    /// this job's, its log not the one it binds): the gateway's or this
    /// host's trouble, not the node's; no strike, the row kept.
    pub(super) fn unreadable(reason: impl Into<String>) -> Self {
        Self::fault("unreadable_result", false, FaultNext::Keep, reason)
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
pub(super) fn judge_stored(stored: &Stored, inputs: &Value) -> Judged {
    if let Some(early) = judge_without_a_run(&stored.meta) {
        return early;
    }
    let meta = &stored.meta;
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
        let mut judged = Judged::fault(
            "unread_version",
            false,
            FaultNext::FallbackLocal,
            format!("the node's gate wrote a fact of version {version}"),
        );
        judged.failure = gate::decide(&run, verdict).err();
        return judged;
    }
    match gate::decide(&run, verdict) {
        Ok(decided) => judge_verdict(decided, verdict, meta, inputs),
        Err(failure) => route_failure(run.status.code(), failure, stated_verdict(verdict)),
    }
}

/// The rows of §6.2 that need no run: a self-fenced job, a gate that never
/// started, one past its wall clock.
fn judge_without_a_run(meta: &ResultMeta) -> Option<Judged> {
    if meta.self_fenced {
        return Some(Judged {
            evidence: "self_fenced",
            verdict: None,
            failure: None,
            stated: None,
            strike: false,
            next: None,
            reason: "the node fenced itself: the epoch has moved".to_string(),
        });
    }
    if let Some(error) = &meta.wrapper_error {
        return Some(Judged::fault(
            "wrapper_error",
            true,
            FaultNext::Queued,
            format!("the gate never started on the node: {error}"),
        ));
    }
    meta.timed_out.then(|| {
        Judged::fault(
            "timed_out",
            true,
            FaultNext::FallbackLocal,
            "the gate ran past its wall clock on the node",
        )
    })
}

/// Exit 0 with a verdict: believed only when the node judged this job's
/// own commits and inputs (the verdict's base and candidate, then all five
/// fields); otherwise an abort of the node's, struck.
fn judge_verdict(
    decided: gate::Verdict,
    verdict: Option<&str>,
    meta: &ResultMeta,
    inputs: &Value,
) -> Judged {
    let line: Value = verdict
        .and_then(|line| serde_json::from_str(line.trim()).ok())
        .unwrap_or(Value::Null);
    let word = if decided.accepted {
        "accepted"
    } else {
        "rejected"
    };
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
        judged.stated = Some(word.to_string());
        return judged;
    }
    Judged {
        evidence: word,
        stated: Some(word.to_string()),
        verdict: Some(decided),
        failure: None,
        strike: false,
        next: None,
        reason: "the node's gate gave a verdict".to_string(),
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
/// strike of a poison pill. A verdict (believed or not: the run was valid)
/// is judged once per completion by `spawn_remote_job_valid`, which puts
/// the node's strikes back to 0 and leaves `degraded` as it is; it answers
/// `None` like a self-fenced run, which changes nothing here. Never
/// anything toward a task's halts or back-offs.
pub(super) fn settle(
    store: &PostgresStore,
    job: &RemoteJob,
    judged: &Judged,
) -> Result<Option<FaultOutcome>, String> {
    let Some(next) = judged.next else {
        if judged.verdict.is_some() {
            store
                .remote_job_valid(&job.job_id, job.lease_epoch)
                .map_err(|error| format!("its valid run not judged: {error}"))?;
        }
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

/// Where a task's candidate gate is judged (design §12.2 S5): unset, empty
/// or `local`, on this host as ever; `node`, on spawn-node v1 first
/// ([`Primary`]). Any other value is refused, and the gates stay local.
pub(super) const GATE_REMOTE_ENV: &str = "SPAWN_GATE_REMOTE";

/// `1` beside `SPAWN_GATE_REMOTE=node`: a node's rejection on the job's own
/// inputs is believed too (M3b). Unset, empty or `0`: only an acceptance is
/// (M3a), and a node's rejection is judged again here.
pub(super) const BELIEVE_REJECT_ENV: &str = "SPAWN_GATE_REMOTE_BELIEVE_REJECT";

/// How often the primary path looks at its job while it waits.
const PRIMARY_EVERY: Duration = Duration::from_secs(1);

/// How long past a remote gate's own wall clock ([`GATE_TIMEOUT_S`]) the
/// primary path waits for a leased job's result before it judges here
/// (the sweep fences a silent lease long before).
const PRIMARY_GRACE: Duration = Duration::from_mins(15);

/// Ledger reads that may fail in a row before the primary path gives the
/// job up and judges here.
const PRIMARY_READ_FAILURES: u32 = 5;

/// How long the registry's manifest is reused before it is read again (it
/// hashes the whole cargo registry).
const PRIMARY_REGISTRY_EVERY: Duration = Duration::from_hours(1);

/// How the primary path runs: what it believes (M3a, or M3b too), the
/// directories it shares with the gateway, the cargo home whose registry a
/// node must hold, how long a job waits for a lease, how often it is looked
/// at, and how long a leased job may take in all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PrimarySettings {
    pub(super) believe_reject: bool,
    pub(super) artifacts: PathBuf,
    pub(super) results: PathBuf,
    pub(super) cargo_home: PathBuf,
    pub(super) queue_wait: Duration,
    pub(super) every: Duration,
    pub(super) limit: Duration,
}

/// The primary path's settings from the environment (`lookup`): None
/// unless [`GATE_REMOTE_ENV`] is `node` (unset, empty and `local` are this
/// host's gate, unchanged); an error, and no remote judging, for another
/// value of it or of [`BELIEVE_REJECT_ENV`], or without an artifact or
/// results directory.
pub(super) fn primary_settings(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<PrimarySettings>, String> {
    match lookup(GATE_REMOTE_ENV).as_deref().map(str::trim) {
        None | Some("" | "local") => return Ok(None),
        Some("node") => {}
        Some(other) => {
            return Err(format!("{GATE_REMOTE_ENV} is local or node, not {other:?}"));
        }
    }
    let believe_reject = match lookup(BELIEVE_REJECT_ENV).as_deref().map(str::trim) {
        None | Some("" | "0") => false,
        Some("1") => true,
        Some(other) => {
            return Err(format!("{BELIEVE_REJECT_ENV} is 1 or 0, not {other:?}"));
        }
    };
    let directory = |name: &str| -> Result<PathBuf, String> {
        lookup(name)
            .map(|text| PathBuf::from(text.trim()))
            .filter(|path| path.is_absolute())
            .ok_or_else(|| format!("{GATE_REMOTE_ENV}=node needs {name}, an absolute directory"))
    };
    let artifacts = directory(ARTIFACTS_ENV)?;
    let results = directory(RESULTS_ENV)?;
    let cargo_home = lookup("CARGO_HOME")
        .filter(|text| !text.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| lookup("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .ok_or_else(|| format!("{GATE_REMOTE_ENV}=node needs CARGO_HOME or HOME"))?;
    Ok(Some(PrimarySettings {
        believe_reject,
        artifacts,
        results,
        cargo_home,
        queue_wait: queue_wait(lookup(QUEUE_WAIT_ENV).as_deref()),
        every: PRIMARY_EVERY,
        limit: Duration::from_secs(GATE_TIMEOUT_S) + PRIMARY_GRACE,
    }))
}

/// One candidate's gate to judge on a node: the task, the judging it
/// serves, the mirror holding both commits, the gate directory and the
/// task's scenario, and the gate's environment for this judging.
pub(super) struct PrimaryRequest<'a> {
    pub(super) task: i64,
    pub(super) judging_id: Option<String>,
    pub(super) repo: &'a Path,
    pub(super) base: &'a str,
    pub(super) candidate: &'a str,
    pub(super) scenarios: &'a Path,
    pub(super) scenario: Option<&'a Path>,
    pub(super) gate_env: &'a [(String, String)],
}

/// What the primary path came to.
#[derive(Debug)]
pub(super) enum PrimaryOutcome {
    /// A node's verdict on the job's own inputs, believed (M3a: an
    /// acceptance; M3b: a rejection too): judged as a local run's verdict
    /// is.
    Believed(Box<Believed>),
    /// Judged here instead (fallback_local), for `why`: no node leased the
    /// job in time and the primary took it back, the node's run was its
    /// fault or skew, a rejection M3a does not believe, or the job could
    /// not be queued. Nothing is recorded for the task by this path.
    Local { why: String },
    /// The judging was stopped (an owner's urgent landing) while it
    /// waited: a job still queued is taken back, a leased one's result is
    /// never read.
    Stopped,
}

/// A node's verdict the primary path believed: `given` is what the node
/// judged, `env` where (`runner` `node:<name>`), `belief` the rule (`M3a`,
/// `M3b`). No red check is ever run on a node (a grant cannot carry
/// `SPAWN_GATE_BASE_CHECK`): a gate on a red main is judged here
/// (integrate.rs `Judge::remote_primary`).
#[derive(Debug)]
pub(super) struct Believed {
    pub(super) job_id: String,
    pub(super) verdict: gate::Verdict,
    pub(super) given: Value,
    pub(super) env: Value,
    pub(super) belief: &'static str,
}

/// Where one look at a job leaves it ([`look`]).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Look {
    /// Still queued, within its wait.
    Queued,
    /// A node holds it (leased, or its result is pending).
    Leased(i64),
    /// Its wait was over, but a node was granted it between the read and
    /// the take-back: read it again (the node keeps it).
    Raced,
    /// The primary took it back (`spawn_remote_job_take_local`).
    TakenLocal,
    /// A node's result is stored.
    Completed,
    /// It ended otherwise.
    Ended(RemoteJobState),
}

/// One look at `row`, read just before, queued `since` and allowed to wait
/// `wait` for a lease: past the wait a job still queued is taken back at
/// the epoch read, the fenced compare-and-set a node's grant also goes
/// through. Only the take-back's win is [`Look::TakenLocal`]; a node that
/// won is [`Look::Raced`], never a local judging.
pub(super) fn look(
    store: &PostgresStore,
    row: &RemoteJob,
    since: Instant,
    wait: Duration,
) -> Result<Look, String> {
    Ok(match row.state {
        RemoteJobState::Queued if since.elapsed() < wait => Look::Queued,
        RemoteJobState::Queued => {
            if store
                .remote_job_take_local(&row.job_id, row.lease_epoch)
                .map_err(|error| error.to_string())?
            {
                Look::TakenLocal
            } else {
                Look::Raced
            }
        }
        RemoteJobState::Leased | RemoteJobState::ResultPending => Look::Leased(row.lease_epoch),
        RemoteJobState::Completed => Look::Completed,
        other => Look::Ended(other),
    })
}

/// The name of the node that ran `row`: the ledger's, else what the node
/// said, else its id's first 12 digits.
pub(super) fn node_name_of(store: &PostgresStore, row: &RemoteJob, stored: &Stored) -> String {
    let id = row.node_id.clone().unwrap_or_default();
    store
        .nodes_v1()
        .ok()
        .and_then(|nodes| {
            nodes
                .into_iter()
                .find(|node| node.node_id.as_deref() == Some(id.as_str()))
        })
        .map(|node| node.name)
        .or_else(|| {
            stored
                .meta
                .env
                .as_ref()
                .and_then(|env| env.get("node").cloned())
        })
        .unwrap_or_else(|| id.chars().take(12).collect())
}

/// The `env` of a node's run: its host and toolchain as it said them (the
/// toolchain line of its log otherwise), on runner `node:<name>`.
fn node_env(stored: &Stored, node: &str) -> Value {
    let meta = &stored.meta;
    let said = |name: &str| meta.env.as_ref().and_then(|env| env.get(name).cloned());
    let toolchain = said("toolchain").or_else(|| {
        meta.streams(&stored.log)
            .ok()
            .and_then(|(stdout, _)| super::inputs::toolchain_of(&String::from_utf8_lossy(stdout)))
    });
    json!({"host": said("host"), "toolchain": toolchain, "runner": format!("node:{node}")})
}

/// What one completed run leaves the primary path ([`Primary::completed`]).
#[derive(Debug)]
pub(super) enum Settled {
    /// Believed: the outcome to return.
    Believed(PrimaryOutcome),
    /// Back in the queue for another node (this one excluded): waited for
    /// again.
    Requeued,
    /// Judged here, for this reason.
    Local(String),
}

/// What M3a or M3b makes of a node's judged run when it gave a verdict:
/// the rule that believes it, or None (M3a's rejection: a valid run,
/// judged again here, nothing recorded for the task).
pub(super) fn belief(judged: &Judged, believe_reject: bool) -> Option<&'static str> {
    let verdict = judged.verdict.as_ref()?;
    if verdict.accepted {
        Some("M3a")
    } else if believe_reject {
        Some("M3b")
    } else {
        None
    }
}

/// The primary path (design §12.2 S5, §12.4 M3a/M3b): a task's candidate
/// gate queued as a `gate` job for spawn-node v1, its artifacts published
/// for the gateway, waited for, and its node's verdict believed by the
/// rules of M3a (an acceptance on the job's own inputs) or M3b (a
/// rejection too); every other outcome is judged here, and a node's fault
/// is settled on the ledger alone (`spawn_remote_job_fault`), never on
/// the task, its halts or its back-offs. Shared by the judging threads.
pub(super) struct Primary {
    pub(super) settings: PrimarySettings,
    artifacts: Artifacts,
    /// Snapshots of gate directories, while their pack is made.
    work: PathBuf,
    registry: std::sync::Mutex<Option<(Instant, RegistryManifest)>>,
}

impl Primary {
    pub(super) fn new(settings: PrimarySettings, work: PathBuf) -> Self {
        let artifacts = Artifacts::new(settings.artifacts.clone());
        Self {
            settings,
            artifacts,
            work,
            registry: std::sync::Mutex::new(None),
        }
    }

    /// The registry's manifest (and [`REGISTRY_MANIFEST`] written), read
    /// again after [`PRIMARY_REGISTRY_EVERY`].
    fn registry(&self) -> Result<RegistryManifest, String> {
        let mut cached = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((read, manifest)) = cached.as_ref()
            && read.elapsed() < PRIMARY_REGISTRY_EVERY
        {
            return Ok(manifest.clone());
        }
        let manifest = self
            .artifacts
            .registry_manifest(&self.settings.cargo_home)?;
        *cached = Some((Instant::now(), manifest.clone()));
        Ok(manifest)
    }

    /// What a node must hold for a gate of `controller`: the registry, the
    /// gate's PostgreSQL, the web lockfile (published for the node to
    /// fetch by its digest) and the toolchain, when the controller names
    /// them.
    fn requires_of(
        &self,
        controller: &Path,
        registry: String,
    ) -> Result<BTreeMap<String, String>, String> {
        let mut requires = BTreeMap::from([
            ("registry".to_string(), registry),
            ("pg".to_string(), PG_MAJOR.to_string()),
        ]);
        if let Some(lock) = web_lock(controller) {
            let published = self
                .artifacts
                .put_copy(&controller.join("web/package-lock.json"))?;
            if published.digest.hex != lock {
                return Err("web/package-lock.json changed while it was published".to_string());
            }
            requires.insert("web_lock".to_string(), lock);
        }
        if let Some(channel) = toolchain_channel(controller) {
            requires.insert("toolchain".to_string(), channel);
        }
        Ok(requires)
    }

    /// The job for `request`, its artifacts written: the delta and the
    /// baseline bundles, the controller pack of a snapshot of the gate
    /// directory (so `scenarios_sha256` is the set the node receives), the
    /// registry's pack and manifest; its inputs the five a verdict binds.
    pub(super) fn publish(&self, request: &PrimaryRequest<'_>) -> Result<GateJob, String> {
        let scenarios = request.scenarios;
        let controller = super::inputs::controller_of(scenarios)
            .ok_or_else(|| format!("{} has no controller above it", scenarios.display()))?;
        let controller_commit = super::inputs::controller_commit(&controller).ok_or_else(|| {
            format!(
                "{} names no controller commit: a node could not say what it judged",
                controller.join(".controller").display()
            )
        })?;
        let scenarios_at = scenarios
            .strip_prefix(&controller)
            .map_err(|_| format!("{} is not inside its controller", scenarios.display()))?
            .to_path_buf();
        let registry = self.registry()?;
        std::fs::create_dir_all(&self.work)
            .map_err(|error| format!("{}: {error}", self.work.display()))?;
        let snapshot = self.work.join(unique(&format!("task-{}", request.task)));
        let copied = ran(
            Command::new("cp")
                .arg("-R")
                .arg("--")
                .arg(scenarios)
                .arg(&snapshot),
            "cp",
        );
        if let Err(error) = copied {
            super::shadow::remove_tree(&snapshot);
            return Err(format!("cannot copy {}: {error}", scenarios.display()));
        }
        let scenarios_sha256 = super::inputs::scenarios_sha256(&snapshot);
        let pack = self.artifacts.controller_pack(
            &controller,
            &snapshot,
            &scenarios_at,
            scenarios_sha256
                .as_deref()
                .map(|digest| format!("{controller_commit}-{digest}"))
                .as_deref(),
        );
        super::shadow::remove_tree(&snapshot);
        let pack = pack?;
        let scenarios_sha256 = scenarios_sha256
            .ok_or_else(|| format!("{} cannot be read whole", scenarios.display()))?;
        let delta = self
            .artifacts
            .delta_bundle(request.repo, request.base, request.candidate)?;
        let baseline = self.artifacts.baseline_bundle(request.repo, request.base)?;
        let inputs = json!({
            "base": request.base,
            "candidate": request.candidate,
            "controller": controller_commit,
            "scenarios_sha256": scenarios_sha256,
            "scenario": request
                .scenario
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().to_string()),
        });
        let requires = self.requires_of(&controller, registry.registry)?;
        let repo = request
            .repo
            .file_name()
            .map(|name| name.to_string_lossy().trim_end_matches(".git").to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "repo".to_string());
        Ok(GateJob {
            kind: RemoteJobKind::Gate,
            job_id: job_id("gate", request.candidate),
            judging_id: request.judging_id.clone(),
            task_id: Some(request.task),
            repo,
            inputs,
            env: grant_env(request.gate_env),
            artifacts: vec![
                delta.url(ArtifactRole::DeltaBundle),
                baseline.url(ArtifactRole::BaselineBundle),
                pack.url(ArtifactRole::ControllerPack),
            ],
            requires,
        })
    }

    /// Judge `request` on a node: publish and queue its job, then follow it
    /// ([`Self::follow`]). `stopped` says whether the judging was stopped.
    pub(super) fn judge(
        &self,
        store: &PostgresStore,
        request: &PrimaryRequest<'_>,
        stopped: &dyn Fn() -> bool,
    ) -> PrimaryOutcome {
        let task = request.task;
        let job = match self.publish(request) {
            Ok(job) => job,
            Err(error) => {
                return PrimaryOutcome::Local {
                    why: format!("not queued for a node: {error}"),
                };
            }
        };
        let job_id = match enqueue(store, &job) {
            Ok(job_id) => job_id,
            Err(error) => {
                return PrimaryOutcome::Local {
                    why: format!("not queued for a node: {error}"),
                };
            }
        };
        println!(
            "integrate: task {task}: its gate queued for a node as {job_id} (a lease within {} s)",
            self.settings.queue_wait.as_secs()
        );
        self.follow(store, task, &job_id, &job.inputs, stopped)
    }

    /// Follow queued job `job_id` (granted `given`) to its end: a job no
    /// node leased within the queue wait is taken back and judged here
    /// (only when the take-back wins: a node granted it meanwhile keeps
    /// it, and its result is waited for); a lease fenced back to the queue
    /// gets a whole wait again; a completed run is judged ([`Self::completed`]).
    /// The nodes' sweep is the integrator pass's own, never run here.
    pub(super) fn follow(
        &self,
        store: &PostgresStore,
        task: i64,
        job_id: &str,
        given: &Value,
        stopped: &dyn Fn() -> bool,
    ) -> PrimaryOutcome {
        let started = Instant::now();
        let mut since = Instant::now();
        let mut failures = 0u32;
        loop {
            if stopped() {
                Self::abandon(store, job_id);
                return PrimaryOutcome::Stopped;
            }
            if started.elapsed() >= self.settings.limit {
                Self::abandon(store, job_id);
                return PrimaryOutcome::Local {
                    why: format!(
                        "no node's result for {job_id} within {} s",
                        self.settings.limit.as_secs()
                    ),
                };
            }
            let looked = store
                .remote_job(job_id)
                .map_err(|error| error.to_string())
                .and_then(|row| match row {
                    Some(row) => look(store, &row, since, self.settings.queue_wait)
                        .map(|looked| Some((looked, row))),
                    None => Ok(None),
                });
            let (looked, row) = match looked {
                Ok(Some(looked)) => {
                    failures = 0;
                    looked
                }
                Ok(None) => {
                    return PrimaryOutcome::Local {
                        why: format!("{job_id} is gone from the ledger"),
                    };
                }
                Err(error) => {
                    failures += 1;
                    eprintln!("integrate: task {task}'s node gate {job_id}: {error}");
                    if failures >= PRIMARY_READ_FAILURES {
                        Self::abandon(store, job_id);
                        return PrimaryOutcome::Local {
                            why: format!("the ledger cannot be read: {error}"),
                        };
                    }
                    std::thread::sleep(self.settings.every);
                    continue;
                }
            };
            match looked {
                Look::Queued => std::thread::sleep(self.settings.every),
                Look::Leased(_) => {
                    since = Instant::now();
                    std::thread::sleep(self.settings.every);
                }
                Look::Raced => {
                    println!(
                        "integrate: task {task}'s node gate {job_id}: a node leased it as it \
                         was taken back; its result is waited for"
                    );
                    since = Instant::now();
                }
                Look::TakenLocal => {
                    return PrimaryOutcome::Local {
                        why: format!(
                            "no node leased {job_id} within {} s: taken back (fallback_local)",
                            self.settings.queue_wait.as_secs()
                        ),
                    };
                }
                Look::Completed => match self.completed(store, task, &row, given) {
                    Settled::Believed(outcome) => return outcome,
                    Settled::Requeued => since = Instant::now(),
                    Settled::Local(why) => return PrimaryOutcome::Local { why },
                },
                Look::Ended(state) => {
                    return PrimaryOutcome::Local {
                        why: format!("{job_id} ended {state:?}"),
                    };
                }
            }
        }
    }

    /// A job this judging no longer waits for: taken back while still
    /// queued, so no node runs it for nothing (a node that won the race
    /// keeps it, and its result is never read).
    fn abandon(store: &PostgresStore, job_id: &str) {
        if let Ok(Some(row)) = store.remote_job(job_id)
            && row.state == RemoteJobState::Queued
        {
            let _ = store.remote_job_take_local(job_id, row.lease_epoch);
        }
    }

    /// A completed run of `row`: its stored result judged (§6.2). A verdict
    /// on the job's own inputs is believed by M3a or M3b; a rejection M3a
    /// does not believe is a valid run, judged again here; anything else
    /// is settled on the ledger (strike, exclusion, poison pill) and
    /// either waits for another node or is judged here.
    pub(super) fn completed(
        &self,
        store: &PostgresStore,
        task: i64,
        row: &RemoteJob,
        given: &Value,
    ) -> Settled {
        let job_id = &row.job_id;
        let stored = match read_stored(&self.settings.results, job_id, row.lease_epoch) {
            Ok(stored) => stored,
            Err(error) => {
                let unread = Judged::unreadable(error.clone());
                if let Err(error) = settle(store, row, &unread) {
                    eprintln!("integrate: task {task}'s node gate {job_id}: {error}");
                }
                return Settled::Local(format!("{job_id}'s result cannot be read: {error}"));
            }
        };
        let judged = judge_stored(&stored, given);
        let node = node_name_of(store, row, &stored);
        if let Some(verdict) = judged.verdict.clone() {
            // A valid run: the node's strikes go back to 0. A ledger error
            // is logged, never a reason to drop the verdict.
            if let Err(error) = settle(store, row, &judged) {
                eprintln!("integrate: task {task}'s node gate {job_id} on {node}: {error}");
            }
            let word = if verdict.accepted {
                "accepted"
            } else {
                "rejected"
            };
            return match belief(&judged, self.settings.believe_reject) {
                Some(rule) => {
                    println!(
                        "integrate: task {task}: node {node}'s verdict on {job_id} believed \
                         ({rule}): {word}"
                    );
                    Settled::Believed(PrimaryOutcome::Believed(Box::new(Believed {
                        job_id: job_id.clone(),
                        env: node_env(&stored, &node),
                        given: given.clone(),
                        verdict,
                        belief: rule,
                    })))
                }
                None => Settled::Local(format!(
                    "node {node} rejected it on {job_id}, a valid run (M3a believes only an \
                     acceptance): judged again here"
                )),
            };
        }
        let settled = settle(store, row, &judged);
        match &settled {
            Ok(Some(outcome)) => println!(
                "integrate: task {task}'s node gate {job_id} on {node}: {} ({}): strike {}, \
                 next {:?}{}",
                judged.evidence,
                judged.reason,
                judged.strike,
                outcome.next,
                if outcome.poison == Some(true) {
                    ", the job is the poison"
                } else {
                    ""
                }
            ),
            Ok(None) => {}
            Err(error) => eprintln!(
                "integrate: task {task}'s node gate {job_id}: its fault not judged: {error}"
            ),
        }
        match settled {
            Ok(Some(outcome)) if outcome.ok && outcome.next == Some(FaultNext::Queued) => {
                Settled::Requeued
            }
            _ => Settled::Local(format!(
                "node {node} gave no verdict to believe on {job_id}: {} ({})",
                judged.evidence, judged.reason
            )),
        }
    }

    /// Take back every queued `gate` job no judging of this integrator
    /// waits for (a predecessor's, left by a restart): nobody would read
    /// its result. A node that won the race keeps it. Run before any
    /// judging of this integrator starts.
    pub(super) fn reclaim_orphans(store: &PostgresStore) {
        let queued = match store.remote_jobs_queued(RemoteJobKind::Gate) {
            Ok(queued) => queued,
            Err(error) => {
                eprintln!("integrate: the queued node gates not read: {error}");
                return;
            }
        };
        for orphan in queued {
            match store.remote_job_take_local(&orphan.job_id, orphan.lease_epoch) {
                Ok(true) => println!(
                    "integrate: node gate {} (task {:?}) queued by an earlier integrator: taken \
                     back",
                    orphan.job_id, orphan.task_id
                ),
                Ok(false) => {}
                Err(error) => eprintln!(
                    "integrate: node gate {} not taken back: {error}",
                    orphan.job_id
                ),
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use spawn_node_proto::json::OrderedMap;
    use spawn_node_proto::result::ResultInputs;
    use spawn_postgres::NodeEnroll;
    use std::cell::RefCell;
    use std::collections::HashMap;

    pub(in crate::integrate) const BASE: &str = "1111111111111111111111111111111111111111";
    pub(in crate::integrate) const CANDIDATE: &str = "2222222222222222222222222222222222222222";
    const CONTROLLER: &str = "3333333333333333333333333333333333333333";
    const SCENARIOS: &str = "4444444444444444444444444444444444444444444444444444444444444444";
    pub(in crate::integrate) const SPEC_SHA: &str =
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A scratch directory, removed when dropped.
    pub(in crate::integrate) struct Scratch(pub(in crate::integrate) PathBuf);
    impl Scratch {
        pub(in crate::integrate) fn new(case: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "spawn-remote-test-{case}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            super::super::shadow::remove_tree(&self.0);
        }
    }

    pub(in crate::integrate) fn job_inputs() -> Value {
        json!({"base": BASE, "candidate": CANDIDATE, "controller": CONTROLLER,
               "scenarios_sha256": SCENARIOS, "scenario": null})
    }

    fn result_inputs(inputs: &Value) -> ResultInputs {
        serde_json::from_value(inputs.clone()).unwrap()
    }

    /// A node's result: the gate's `exit`, its verdict file `verdict`, its
    /// `stdout` and `stderr`, as judging `inputs`.
    pub(in crate::integrate) fn meta(
        job_id: &str,
        epoch: i64,
        exit: Option<i64>,
        verdict: Option<&str>,
        stdout: &str,
        stderr: &str,
        inputs: Option<&Value>,
    ) -> (ResultMeta, Vec<u8>) {
        let log = [stdout.as_bytes(), stderr.as_bytes()].concat();
        let meta = ResultMeta {
            v: 1,
            job_id: job_id.to_string(),
            lease_epoch: u64::try_from(epoch).unwrap(),
            exit_code: exit,
            signal: None,
            timed_out: false,
            self_fenced: false,
            seconds: 42,
            verdict_raw: verdict.map(|line| format!("{line}\n")),
            stdout_bytes: stdout.len() as u64,
            log_bytes: log.len() as u64,
            log_sha256: spawn_node_proto::codec::sha256_hex(&log),
            truncated: None,
            inputs: inputs.map(result_inputs),
            env: Some(OrderedMap(vec![
                ("host".to_string(), "n252-host".to_string()),
                ("toolchain".to_string(), "rustc 1.95.0 x86_64".to_string()),
                ("node".to_string(), "n252".to_string()),
                ("tier".to_string(), "tmpfs-bwrap".to_string()),
            ])),
            wrapper_error: None,
        };
        (meta, log)
    }

    fn stored(exit: Option<i64>, verdict: Option<&str>, stdout: &str, stderr: &str) -> Stored {
        let (meta, log) = meta(
            "gate-0123456789ab-k3x9q2ma",
            1,
            exit,
            verdict,
            stdout,
            stderr,
            Some(&job_inputs()),
        );
        Stored { meta, log }
    }

    pub(in crate::integrate) fn verdict_on(word: &str, base: &str, candidate: &str) -> String {
        format!(
            r#"{{"v":1,"verdict":"{word}","reason":"r","base":"{base}","candidate":"{candidate}"}}"#
        )
    }

    fn verdict(word: &str) -> String {
        verdict_on(word, BASE, CANDIDATE)
    }

    /// Judged as the row it is: evidence, strike, next.
    fn row(judged: &Judged) -> (&'static str, bool, Option<FaultNext>) {
        (judged.evidence, judged.strike, judged.next)
    }

    #[test]
    fn the_grant_env_is_the_gate_env_without_what_the_node_sets_or_any_database() {
        let mut gate_env: Vec<(String, String)> = gate::GATE_ENV
            .iter()
            .map(|name| ((*name).to_string(), format!("value-of-{name}")))
            .collect();
        // Never a real URL: the shape alone (no password).
        let url = ["postgres", "://", "owner@db.example/spawn"].concat();
        gate_env.push(("SPAWN_DATABASE_URL".into(), url.clone()));
        gate_env.push(("ANTHROPIC_API_KEY".into(), "not-a-real-key".into()));
        // A database URL smuggled in under a name the gate may get.
        gate_env.retain(|(name, _)| name != "SUPPLY_PA_DEPS");
        gate_env.push((
            "SUPPLY_PA_DEPS".into(),
            ["PostgreSQL", "://", "user@db.example/x"].concat(),
        ));
        let env = grant_env(&gate_env);
        for owned in NODE_OWNED_ENV {
            assert!(!env.contains_key(owned), "{owned} reaches the node");
        }
        for name in ["SPAWN_DATABASE_URL", "ANTHROPIC_API_KEY", "SUPPLY_PA_DEPS"] {
            assert!(!env.contains_key(name), "{name} reaches the node");
        }
        assert!(env.values().all(|value| !names_a_database(value)));
        assert!(env.values().all(|value| !value.contains("not-a-real-key")));
        // Only GATE_ENV's names, every one the node does not own.
        assert!(
            env.keys()
                .all(|name| gate::GATE_ENV.contains(&name.as_str()))
        );
        assert_eq!(env["SPAWN_GATE_JOBS"], "value-of-SPAWN_GATE_JOBS");
        assert_eq!(env["SPAWN_GATE_HOST_JOBS"], "value-of-SPAWN_GATE_HOST_JOBS");
        assert_eq!(
            env["SPAWN_GATE_BUILD_JOBS"],
            "value-of-SPAWN_GATE_BUILD_JOBS"
        );
        assert_eq!(env["LANG"], "value-of-LANG");
        // The node's floor (F4), whatever the primary's.
        assert_eq!(env["SPAWN_GATE_MIN_FREE_GB"], NODE_MIN_FREE_GB);
        // A grant of it is one the node takes (its own refusal list).
        let grant = spawn_node_proto::grant::Grant {
            v: 1,
            job_id: "gate-0123456789ab-k3x9q2ma".into(),
            lease_epoch: 1,
            node_id: "a".repeat(64),
            kind: spawn_node_proto::grant::JobKind::Gate,
            expires_ms: 1,
            attempt: None,
            cost: None,
            timeout_s: None,
            inputs: None,
            env: Some(env.clone().into_iter().collect()),
            requires: None,
            artifact_urls: None,
        };
        grant.validate().unwrap();
        // An empty environment still gets a LANG and the floor.
        let bare = grant_env(&[]);
        assert_eq!(bare["LANG"], "C.UTF-8");
        assert_eq!(bare.len(), 2);
    }

    #[test]
    fn the_queue_wait_is_whole_seconds_with_a_default_of_30() {
        assert_eq!(queue_wait(None), Duration::from_secs(30));
        assert_eq!(queue_wait(Some(" 5 ")), Duration::from_secs(5));
        for bad in ["0", "-1", "x", "1.5", ""] {
            assert_eq!(queue_wait(Some(bad)), DEFAULT_QUEUE_WAIT, "{bad:?}");
        }
    }

    #[test]
    fn a_job_spec_carries_the_grant_and_names_its_judging_judging_id() {
        let job = GateJob {
            kind: RemoteJobKind::GateShadow,
            job_id: job_id("gs", CANDIDATE),
            judging_id: Some("0123456789ab0123456789abcdef".into()),
            task_id: Some(7),
            repo: "spawn".into(),
            inputs: job_inputs(),
            env: grant_env(&[("SPAWN_GATE_JOBS".into(), "8".into())]),
            artifacts: vec![
                Published {
                    digest: ContentDigest::sha256_of(b"delta"),
                    size: 5,
                }
                .url(ArtifactRole::DeltaBundle),
            ],
            requires: BTreeMap::from([("pg".to_string(), "16".to_string())]),
        };
        assert!(job.job_id.starts_with("gs-222222222222-"), "{}", job.job_id);
        assert!(spawn_node_proto::envelope::is_job_id(&job.job_id));
        let spec = job.spec();
        let value = serde_json::to_value(&spec).unwrap();
        // F11: `judging_id`, never `judging`.
        assert_eq!(value["judging_id"], "0123456789ab0123456789abcdef");
        assert!(value.get("judging").is_none());
        assert!(!value.to_string().contains("\"judging\""));
        assert_eq!(value["kind"], "gate_shadow");
        assert_eq!(value["base_commit"], BASE);
        assert_eq!(value["cost"], json!({"gate_sandbox": 1, "tmpfs_mb": 20480}));
        assert_eq!(value["spec"]["timeout_s"], 7200);
        assert_eq!(value["spec"]["inputs"]["repo"], "spawn");
        assert_eq!(value["spec"]["inputs"]["candidate"], CANDIDATE);
        assert_eq!(value["spec"]["env"]["SPAWN_GATE_JOBS"], "8");
        assert_eq!(value["spec"]["artifact_urls"][0]["role"], "delta_bundle");
        let url = value["spec"]["artifact_urls"][0]["url"].as_str().unwrap();
        assert!(url.starts_with("/node/v1/artifacts/sha256:"), "{url}");
        spawn_node_proto::check_target(url).unwrap();
    }

    /// Section 6.2, row by row, as `gate::decide` reads a stored result:
    /// exit 0 (this test), exits 2 to 5, exit 6, and the rows of no exit
    /// (the tests after it).
    #[test]
    fn every_row_of_the_outcomes_matrix() {
        let inputs = job_inputs();
        let judge = |stored: &Stored| judge_stored(stored, &inputs);

        // Exit 0 with a verdict on the job's own inputs: believed (no
        // fault, no strike), stderr marked `! `.
        let accepted = judge(&stored(
            Some(0),
            Some(&verdict("accepted")),
            "| gate ok\n",
            "warn\n",
        ));
        assert_eq!(row(&accepted), ("accepted", false, None));
        let said = accepted.verdict.as_ref().unwrap();
        assert!(said.accepted);
        assert!(said.output.contains("! warn"), "{}", said.output);
        assert!(said.output.contains("| gate ok"));
        let rejected = judge(&stored(Some(0), Some(&verdict("rejected")), "", ""));
        assert_eq!(row(&rejected), ("rejected", false, None));
        assert!(!rejected.verdict.unwrap().accepted);

        // Exit 0 on other commits, other inputs, or none said: an abort.
        let other = verdict_on("accepted", BASE, &"9".repeat(40));
        let other = judge(&stored(Some(0), Some(&other), "", ""));
        assert_eq!(
            row(&other),
            ("unequal_inputs", true, Some(FaultNext::Queued))
        );
        assert!(other.verdict.is_none());
        assert_eq!(other.stated.as_deref(), Some("accepted"));
        let mut theirs = inputs.clone();
        theirs["scenarios_sha256"] = json!("5".repeat(64));
        let (m, log) = meta(
            "gate-0123456789ab-k3x9q2ma",
            1,
            Some(0),
            Some(&verdict("accepted")),
            "",
            "",
            Some(&theirs),
        );
        let unequal = judge(&Stored { meta: m, log });
        assert_eq!(
            row(&unequal),
            ("unequal_inputs", true, Some(FaultNext::Queued))
        );
        assert!(
            unequal.reason.contains("scenarios_sha256"),
            "{}",
            unequal.reason
        );
        let (m, log) = meta(
            "gate-0123456789ab-k3x9q2ma",
            1,
            Some(0),
            Some(&verdict("accepted")),
            "",
            "",
            None,
        );
        assert_eq!(row(&judge(&Stored { meta: m, log })).0, "unequal_inputs");
        // Exit 0 with no verdict file, or two lines in it.
        let none = judge(&stored(Some(0), None, "", ""));
        assert_eq!(row(&none), ("aborted", true, Some(FaultNext::Queued)));
        assert!(matches!(none.failure, Some(GateFailure::Aborted { .. })));
        let two = format!("{}\n{}", verdict("accepted"), verdict("rejected"));
        assert_eq!(
            row(&judge(&stored(Some(0), Some(&two), "", ""))).0,
            "aborted"
        );
    }

    #[test]
    fn exits_2_to_5_are_the_nodes_aborts_its_base_or_its_room() {
        let inputs = job_inputs();
        let judge = |stored: &Stored| judge_stored(stored, &inputs);

        // Exit 2: not a gate.sh status.
        assert_eq!(
            row(&judge(&stored(Some(2), None, "", "boom\n"))),
            ("aborted", true, Some(FaultNext::Queued))
        );
        // Exit 3: setup refused; struck, excluded (by the fault), local.
        assert_eq!(
            row(&judge(&stored(Some(3), None, "", "no bwrap\n"))),
            ("aborted", true, Some(FaultNext::FallbackLocal))
        );
        // Exit 4 with the gate's pair, and without.
        let pair = "| check-x.sh exited 1, then 0 on the base: a harness error, not a verdict; \
                    the gate aborts\n| stage gate aborted 12ms base: check-x.sh again \
                    (unsolved needs two runs that agree)\n";
        let disagrees = judge(&stored(Some(4), None, pair, ""));
        assert_eq!(
            row(&disagrees),
            ("base_disagrees", true, Some(FaultNext::Queued))
        );
        assert!(matches!(
            disagrees.failure,
            Some(GateFailure::BaseDisagrees { .. })
        ));
        assert_eq!(
            row(&judge(&stored(Some(4), None, "| nothing\n", ""))),
            ("aborted", true, Some(FaultNext::Queued))
        );

        // Exit 5: the node's tmpfs (a strike), a slot wait (none).
        let disk =
            r#"{"v":1,"no_room":true,"free_kb":10,"min_gb":8,"tmpdir":"/data01/spawn-node/w/0/t"}"#;
        let disk = judge(&stored(Some(5), Some(disk), "", ""));
        assert_eq!(row(&disk), ("no_room", true, Some(FaultNext::Queued)));
        match &disk.failure {
            Some(GateFailure::NoRoom {
                short: Shortfall::Disk {
                    tmpdir, free_kb, ..
                },
                ..
            }) => {
                // The node's own measure, never this host's.
                assert_eq!(tmpdir, "/data01/spawn-node/w/0/t");
                assert_eq!(*free_kb, 10);
            }
            other => panic!("{other:?}"),
        }
        let slot =
            r#"{"v":1,"no_room":true,"check":"host scenario slot","waited_ms":5000,"budget":32}"#;
        assert_eq!(
            row(&judge(&stored(Some(5), Some(slot), "", ""))),
            ("no_room_slot", false, Some(FaultNext::Queued))
        );
    }

    #[test]
    fn exit_6_is_routed_by_the_fact_the_node_wrote() {
        let inputs = job_inputs();
        let judge = |stored: &Stored| judge_stored(stored, &inputs);

        // Exit 6: where the host fault was, a red main, a scenario the node
        // cannot judge, a validator's defect, no fact.
        let sandbox = r#"{"v":1,"host_fault":true,"scenario":"check-a.sh","on":"sandbox","runs":"","reason":"r","evidence":"sandbox_never_started","tree":"candidate"}"#;
        assert_eq!(
            row(&judge(&stored(Some(6), Some(sandbox), "", ""))),
            ("host_fault", true, Some(FaultNext::Queued))
        );
        let server = r#"{"v":1,"host_fault":true,"scenario":"check-a.sh","on":"test server","runs":"","reason":"r","evidence":"test_server_never_started","tree":"gate"}"#;
        assert_eq!(
            row(&judge(&stored(Some(6), Some(server), "", ""))),
            ("host_fault", true, Some(FaultNext::Queued))
        );
        let base = r#"{"v":1,"host_fault":true,"scenario":"check-a.sh","on":"base","runs":"2","reason":"r"}"#;
        assert_eq!(
            row(&judge(&stored(Some(6), Some(base), "", ""))),
            ("host_fault", true, Some(FaultNext::FallbackLocal))
        );
        let both = r#"{"v":1,"host_fault":true,"scenario":"check-a.sh","on":"candidate and base","runs":"1, 1","reason":"r"}"#;
        assert_eq!(
            row(&judge(&stored(Some(6), Some(both), "", ""))),
            ("host_fault", true, Some(FaultNext::FallbackLocal))
        );
        let defect = r#"{"v":1,"host_fault":true,"scenario":"check-a.sh","on":"candidate and base","runs":"1, 1","reason":"r","evidence":"validator_defect","validator_defects":["check-a.sh"]}"#;
        assert_eq!(
            row(&judge(&stored(Some(6), Some(defect), "", ""))),
            ("validator_defect", true, Some(FaultNext::FallbackLocal))
        );
        let red = r#"{"v":1,"host_fault":true,"on":"main","scenario":"check-tests.sh","runs":"1, 1","failures":[{"test":"a::b","message":"m"}]}"#;
        let red = judge(&stored(Some(6), Some(red), "", ""));
        assert_eq!(
            row(&red),
            ("main_red", false, Some(FaultNext::FallbackLocal))
        );
        assert!(matches!(red.failure, Some(GateFailure::MainRed { .. })));
        let cannot =
            r#"{"v":1,"cannot_judge":true,"scenario":"t029.sh","reason":"no user scopes"}"#;
        let cannot = judge(&stored(Some(6), Some(cannot), "", ""));
        assert_eq!(
            row(&cannot),
            ("cannot_judge", false, Some(FaultNext::FallbackLocal))
        );
        assert!(matches!(
            cannot.failure,
            Some(GateFailure::CannotJudge { .. })
        ));
        assert_eq!(
            row(&judge(&stored(Some(6), None, "", ""))),
            ("aborted", true, Some(FaultNext::FallbackLocal))
        );
    }

    #[test]
    fn a_skew_a_signal_a_timeout_a_wrapper_error_and_a_self_fence() {
        let inputs = job_inputs();
        let judge = |stored: &Stored| judge_stored(stored, &inputs);

        // A fact of another version: a skew, not a fault.
        for exit in [0, 5, 6] {
            let future = r#"{"v":2,"verdict":"accepted"}"#;
            let skew = judge(&stored(Some(exit), Some(future), "", ""));
            assert_eq!(
                row(&skew),
                ("unread_version", false, Some(FaultNext::FallbackLocal)),
                "{exit}"
            );
            assert!(skew.verdict.is_none());
        }

        // A signal: as a status 128+N, or reported as a signal.
        assert_eq!(
            row(&judge(&stored(Some(137), None, "", ""))),
            ("signal", true, Some(FaultNext::Queued))
        );
        let mut killed = stored(None, None, "", "");
        killed.meta.signal = Some(Signal::Number(9));
        let killed = judge(&killed);
        assert_eq!(row(&killed), ("signal", true, Some(FaultNext::Queued)));
        assert!(killed.reason.contains("signal 9"));
        // Past its wall clock; never started; fenced itself.
        let mut late = stored(None, None, "", "");
        late.meta.timed_out = true;
        assert_eq!(
            row(&judge(&late)),
            ("timed_out", true, Some(FaultNext::FallbackLocal))
        );
        let mut never = stored(None, None, "", "");
        never.meta.wrapper_error = Some("digest_mismatch".into());
        let never = judge(&never);
        assert_eq!(
            row(&never),
            ("wrapper_error", true, Some(FaultNext::Queued))
        );
        assert!(never.reason.contains("digest_mismatch"));
        let mut fenced = stored(Some(0), Some(&verdict("accepted")), "", "");
        fenced.meta.self_fenced = true;
        assert_eq!(row(&judge(&fenced)), ("self_fenced", false, None));
        // A status no process exits with: the node's fault.
        assert_eq!(
            row(&judge(&stored(Some(300), None, "", ""))).0,
            "wrapper_error"
        );
    }

    /// NB9: a node's exit 5 without its fact is an abort of the node's,
    /// never a `no_room` measured on this host's disk (which `judge_in`
    /// does for a local gate), even with a no-room line in its stdout.
    #[test]
    fn a_remote_exit_5_is_never_measured_on_this_host() {
        let inputs = job_inputs();
        let judged = judge_stored(
            &stored(
                Some(5),
                None,
                "| x\n{\"v\":1,\"no_room\":true,\"free_kb\":1,\"min_gb\":8,\"tmpdir\":\"/t\"}\n",
                "",
            ),
            &inputs,
        );
        assert_eq!(
            row(&judged),
            ("aborted", true, Some(FaultNext::FallbackLocal))
        );
        match &judged.failure {
            Some(GateFailure::Aborted { reason, log }) => {
                assert!(reason.contains("exit status: 5"), "{reason}");
                assert!(!log.contains("integrate: "), "{log}");
                assert!(!log.contains("measured"), "{log}");
            }
            other => panic!("not an abort: {other:?}"),
        }
    }

    #[test]
    fn a_shadow_is_never_judged_locally_and_its_row_is_kept() {
        for next in [FaultNext::FallbackLocal, FaultNext::DeadLetter] {
            assert_eq!(next_for(RemoteJobKind::GateShadow, next), FaultNext::Keep);
            assert_eq!(next_for(RemoteJobKind::Gate, next), next);
        }
        assert_eq!(
            next_for(RemoteJobKind::GateShadow, FaultNext::Queued),
            FaultNext::Queued
        );
    }

    /// Write a node's result where the gateway does.
    pub(in crate::integrate) fn store_result(results: &Path, meta: &ResultMeta, log: &[u8]) {
        let dir = results.join(format!("{}.{}", meta.job_id, meta.lease_epoch));
        std::fs::create_dir_all(&dir).unwrap();
        let mut line = meta.to_bytes();
        line.push(b'\n');
        std::fs::write(dir.join("meta.json"), line).unwrap();
        std::fs::write(dir.join("log"), log).unwrap();
    }

    #[test]
    fn a_stored_result_is_read_for_its_job_and_epoch_and_split_into_its_streams() {
        let dir = Scratch::new("stored");
        let job = "gate-0123456789ab-k3x9q2ma";
        let (m, log) = meta(
            job,
            3,
            Some(1),
            None,
            "| out\n",
            "err one\nerr two\n",
            Some(&job_inputs()),
        );
        store_result(&dir.0, &m, &log);
        let read = read_stored(&dir.0, job, 3).unwrap();
        let run = run_of(&read).unwrap();
        assert_eq!(run.status.code(), Some(1));
        assert_eq!(run.stdout, "| out\n");
        assert_eq!(run.stderr, "err one\nerr two\n");
        match gate::decide(&run, None).unwrap_err() {
            GateFailure::Aborted { log, .. } => {
                assert_eq!(log, "| out\n! err one\n! err two\n");
            }
            other => panic!("{other:?}"),
        }
        // Another epoch, another job, a log that is not the one it binds.
        assert!(read_stored(&dir.0, job, 2).is_err());
        let other = dir.0.join("gate-other-00000.3");
        std::fs::create_dir_all(&other).unwrap();
        let mine = dir.0.join(format!("{job}.3"));
        std::fs::copy(mine.join("meta.json"), other.join("meta.json")).unwrap();
        std::fs::copy(mine.join("log"), other.join("log")).unwrap();
        assert!(
            read_stored(&dir.0, "gate-other-00000", 3)
                .unwrap_err()
                .contains("is for")
        );
        std::fs::write(mine.join("log"), "changed").unwrap();
        assert!(read_stored(&dir.0, job, 3).unwrap_err().contains("log"));
    }

    pub(in crate::integrate) fn git(dir: &Path, args: &[&str]) -> String {
        let output = spawn_host::test_git_command()
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A bare repository with a base and a candidate on it: `(repo, base,
    /// candidate)`.
    pub(in crate::integrate) fn repository(dir: &Path) -> (PathBuf, String, String) {
        let seed = dir.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(seed.join("a"), "base\n").unwrap();
        git(&seed, &["add", "a"]);
        git(&seed, &["commit", "-q", "-m", "base"]);
        let base = git(&seed, &["rev-parse", "HEAD"]);
        std::fs::write(seed.join("b"), "candidate\n").unwrap();
        git(&seed, &["add", "b"]);
        git(&seed, &["commit", "-q", "-m", "candidate"]);
        let candidate = git(&seed, &["rev-parse", "HEAD"]);
        let repo = dir.join("source.git");
        let output = spawn_host::test_git_command()
            .args(["clone", "-q", "--bare"])
            .arg(&seed)
            .arg(&repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        (repo, base, candidate)
    }

    #[test]
    fn bundles_ship_the_delta_on_its_base_and_the_baseline_once_per_base() {
        let dir = Scratch::new("bundles");
        let (repo, base, candidate) = repository(&dir.0);
        let artifacts = Artifacts::new(dir.0.join("artifacts"));
        let delta = artifacts.delta_bundle(&repo, &base, &candidate).unwrap();
        let path = artifacts.path_of(&delta.digest);
        assert!(path.starts_with(dir.0.join("artifacts/sha256")));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), delta.size);
        delta.digest.verify(&std::fs::read(&path).unwrap()).unwrap();
        // Its prerequisite is the base; its one head the candidate.
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        assert!(text.contains(&format!("-{base}")));
        assert!(text.contains(&format!("{candidate} refs/gate/candidate")));
        // A mirror that fetched the baseline takes it.
        let baseline = artifacts.baseline_bundle(&repo, &base).unwrap();
        let mirror = dir.0.join("mirror.git");
        std::fs::create_dir_all(&mirror).unwrap();
        git(&mirror, &["init", "-q", "--bare"]);
        let whole = artifacts.path_of(&baseline.digest);
        git(
            &mirror,
            &[
                "fetch",
                "-q",
                &whole.to_string_lossy(),
                "refs/gate/base:refs/b",
            ],
        );
        assert_eq!(git(&mirror, &["rev-parse", "refs/b"]), base);
        let bundle = path.to_string_lossy().to_string();
        git(&mirror, &["bundle", "verify", "-q", &bundle]);
        git(
            &mirror,
            &["fetch", "-q", &bundle, "refs/gate/candidate:refs/c"],
        );
        assert_eq!(git(&mirror, &["rev-parse", "refs/c"]), candidate);
        // Cached per base: the same artifact, not made again.
        std::fs::remove_dir_all(dir.0.join("artifacts/.tmp")).unwrap();
        assert_eq!(artifacts.baseline_bundle(&repo, &base).unwrap(), baseline);
        assert!(!dir.0.join("artifacts/.tmp").exists());
        // No delta when there is nothing past the base.
        assert!(artifacts.delta_bundle(&repo, &base, &base).is_err());
    }

    /// A controller: scripts, a scenario set and a web lockfile.
    pub(in crate::integrate) fn controller(dir: &Path) -> PathBuf {
        let ctl = dir.join("ctl");
        std::fs::create_dir_all(ctl.join("scripts/rsi")).unwrap();
        std::fs::create_dir_all(ctl.join("rsi/scenarios/sub")).unwrap();
        std::fs::create_dir_all(ctl.join("web")).unwrap();
        std::fs::write(ctl.join("scripts/rsi/gate.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(ctl.join("rsi/scenarios/check-ok.sh"), "exit 0\n").unwrap();
        std::fs::write(ctl.join("rsi/scenarios/sub/001.sh"), "exit 0\n").unwrap();
        std::fs::write(
            ctl.join("web/package-lock.json"),
            "{\"lockfileVersion\":3}\n",
        )
        .unwrap();
        std::fs::write(ctl.join("web/package.json"), "{}\n").unwrap();
        std::fs::write(
            ctl.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.95.0\"\n",
        )
        .unwrap();
        std::fs::write(ctl.join(".controller"), format!("{CONTROLLER}\n")).unwrap();
        ctl
    }

    fn tar_list(path: &Path) -> Vec<String> {
        let output = Command::new("tar")
            .args(["--numeric-owner", "-tvf"])
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn names(listing: &[String]) -> Vec<String> {
        listing
            .iter()
            .map(|line| line.split_whitespace().last().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_controller_pack_is_deterministic_and_holds_only_what_the_gate_needs() {
        let dir = Scratch::new("pack");
        let ctl = controller(&dir.0);
        let artifacts = Artifacts::new(dir.0.join("artifacts"));
        let scenarios = ctl.join("rsi/scenarios");
        let at = Path::new("rsi/scenarios");
        let first = artifacts
            .controller_pack(&ctl, &scenarios, at, None)
            .unwrap();
        // Touched files, a later time: the same bytes.
        std::thread::sleep(Duration::from_millis(1100));
        for file in ["scripts/rsi/gate.sh", "rsi/scenarios/check-ok.sh"] {
            let text = std::fs::read(ctl.join(file)).unwrap();
            std::fs::write(ctl.join(file), text).unwrap();
        }
        let second = artifacts
            .controller_pack(&ctl, &scenarios, at, None)
            .unwrap();
        assert_eq!(first, second);
        let listing = tar_list(&artifacts.path_of(&first.digest));
        assert_eq!(
            names(&listing),
            [
                "rsi/scenarios/",
                "rsi/scenarios/check-ok.sh",
                "rsi/scenarios/sub/",
                "rsi/scenarios/sub/001.sh",
                "scripts/",
                "scripts/rsi/",
                "scripts/rsi/gate.sh",
                "web/package-lock.json",
            ]
        );
        for line in &listing {
            assert!(line.contains(" 0/0 "), "{line}");
            assert!(line.contains("1970-01-01"), "{line}");
        }
        // ustar.
        let bytes = std::fs::read(artifacts.path_of(&first.digest)).unwrap();
        assert_eq!(&bytes[257..263], b"ustar\0");
        // A snapshot elsewhere, packed under the controller's place.
        let snapshot = dir.0.join("snapshot");
        let copied = Command::new("cp")
            .arg("-R")
            .arg(&scenarios)
            .arg(&snapshot)
            .status()
            .unwrap();
        assert!(copied.success());
        let from_snapshot = artifacts
            .controller_pack(&ctl, &snapshot, at, None)
            .unwrap();
        assert_eq!(from_snapshot, first);
        // Another set, another pack; a key caches it.
        std::fs::write(snapshot.join("check-new.sh"), "exit 0\n").unwrap();
        let keyed = artifacts
            .controller_pack(&ctl, &snapshot, at, Some("k1"))
            .unwrap();
        assert_ne!(keyed, first);
        assert_eq!(
            artifacts
                .controller_pack(&ctl, &scenarios, at, Some("k1"))
                .unwrap(),
            keyed
        );
        assert!(
            artifacts
                .controller_pack(&ctl, &scenarios, Path::new("../x"), None)
                .is_err()
        );
        assert!(
            artifacts
                .controller_pack(&ctl, &scenarios, Path::new("/abs"), None)
                .is_err()
        );
        assert_eq!(toolchain_channel(&ctl).as_deref(), Some("1.95.0"));
        assert_eq!(web_lock(&ctl).unwrap().len(), 64);
        assert_eq!(web_lock(&dir.0), None);
    }

    #[test]
    fn the_registry_manifest_is_the_tree_of_registry_and_git_without_cargos_locks() {
        let dir = Scratch::new("registry");
        let home = dir.0.join("cargo");
        for (path, text) in [
            ("registry/index/x/config.json", "{}"),
            ("registry/cache/x/a-1.0.crate", "crate"),
            ("registry/.package-cache", "lock"),
            ("git/db/repo/HEAD", "ref"),
            ("bin/cargo", "not shipped"),
            ("config.toml", "not shipped"),
        ] {
            let file = home.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        let digest = registry_sha256(&home).unwrap();
        // The same as tree_sha256_v1 over a directory holding just the two.
        let only = dir.0.join("only");
        std::fs::create_dir_all(&only).unwrap();
        for top in ["registry", "git"] {
            let copied = Command::new("cp")
                .arg("-R")
                .arg(home.join(top))
                .arg(only.join(top))
                .status()
                .unwrap();
            assert!(copied.success());
        }
        assert_eq!(digest, spawn_node_proto::registry_sha256(&only).unwrap());
        // Cargo's lock files never move it; a crate does.
        std::fs::write(home.join("registry/.package-cache"), "other").unwrap();
        std::fs::write(home.join("registry/.package-cache-mutate"), "x").unwrap();
        assert_eq!(registry_sha256(&home).unwrap(), digest);
        let artifacts = Artifacts::new(dir.0.join("artifacts"));
        let manifest = artifacts.registry_manifest(&home).unwrap();
        assert_eq!(manifest.registry, digest);
        // The name the gateway serves (S3's artifacts.rs).
        assert!(dir.0.join("artifacts/registry-manifest.json").is_file());
        assert!(!dir.0.join("artifacts/registry-manifest").exists());
        let written: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.0.join("artifacts").join(REGISTRY_MANIFEST)).unwrap(),
        )
        .unwrap();
        assert_eq!(written, manifest.to_json());
        assert_eq!(written["pack"]["digest"], manifest.pack.digest.to_string());
        let packed = names(&tar_list(&artifacts.path_of(&manifest.pack.digest)));
        assert!(
            packed.contains(&"registry/cache/x/a-1.0.crate".to_string()),
            "{packed:?}"
        );
        assert!(packed.contains(&"git/db/repo/HEAD".to_string()));
        assert!(
            packed.iter().all(|name| !name.contains(".package-cache")),
            "{packed:?}"
        );
        assert!(
            packed
                .iter()
                .all(|name| !name.starts_with("bin") && name != "config.toml")
        );
        std::fs::write(home.join("registry/cache/x/b-1.0.crate"), "new").unwrap();
        let moved = artifacts.registry_manifest(&home).unwrap();
        assert_ne!(moved.registry, digest);
        assert_ne!(moved.pack, manifest.pack);
        assert!(artifacts.registry_manifest(&dir.0.join("none")).is_err());
    }

    /// A ledger in a schema of its own, with nodes a test enrolls and
    /// beats for, as the gateway would.
    pub(in crate::integrate) struct Ledger {
        pub(in crate::integrate) store: std::sync::Arc<PostgresStore>,
        seqs: RefCell<HashMap<String, i64>>,
        // Dropped after the store.
        schema: spawn_postgres::testing::TestSchema,
    }

    impl Ledger {
        pub(in crate::integrate) fn new(case: &str) -> Self {
            let schema = spawn_postgres::testing::TestSchema::new(case);
            let store = PostgresStore::connect_in_schema(schema.url(), &schema).expect("connect");
            Self {
                store: std::sync::Arc::new(store),
                seqs: RefCell::new(HashMap::new()),
                schema,
            }
        }

        fn boot() -> &'static str {
            "00000000000000018796a5b4c3d2e1f0"
        }

        /// Enroll node `name` (key from `seed`) holding `holds`; its id.
        pub(in crate::integrate) fn enroll(&self, name: &str, seed: u8, holds: &Value) -> String {
            let token = [seed; 32];
            let pubkey = [seed.wrapping_add(100); 32];
            self.store
                .node_enroll_token(name, None, &token, 600_000)
                .expect("a token");
            let spec = json!({"v": 1, "name": name, "holds": holds});
            let enrolled = self
                .store
                .node_gw_enroll(
                    &NodeEnroll {
                        token_sha256: &token,
                        pubkey: &pubkey,
                        boot_id: Self::boot(),
                        spec_sha256: SPEC_SHA,
                        spec: &spec,
                    },
                    |answer| answer["ok"] == true,
                )
                .expect("enroll");
            assert!(enrolled.committed, "{}", enrolled.answer);
            enrolled.answer["node_id"].as_str().unwrap().to_string()
        }

        /// One beat of `node`, holding nothing: the grants it got.
        pub(in crate::integrate) fn beat(&self, node: &str) -> Vec<(String, i64)> {
            self.beat_grants(node)
                .iter()
                .map(|grant| {
                    (
                        grant["job_id"].as_str().unwrap().to_string(),
                        grant["lease_epoch"].as_i64().unwrap(),
                    )
                })
                .collect()
        }

        /// One beat of `node`, holding nothing: the grants as the ledger
        /// gives them to the gateway.
        pub(in crate::integrate) fn beat_grants(&self, node: &str) -> Vec<Value> {
            let seq = {
                let mut seqs = self.seqs.borrow_mut();
                let seq = seqs.entry(node.to_string()).or_insert(0);
                *seq += 1;
                *seq
            };
            let body = json!({"v": 1, "cell_id": "c0", "state": "idle", "spec_sha256": SPEC_SHA,
                              "capacities": {"gate_sandbox": {"free": 4, "total": 4}},
                              "tmpfs_budget_mb": {"free": 262_144, "total": 262_144},
                              "leases": [], "lease_results_pending": [],
                              "warm_bases": [], "edge": "timer"});
            let answer = self
                .store
                .node_gw_beat(node, Self::boot(), seq, &body, |_| true)
                .expect("a beat")
                .answer;
            answer["grants"].as_array().unwrap().clone()
        }

        /// `node`'s result, stored and completed as the gateway does it.
        pub(in crate::integrate) fn complete(
            &self,
            results: &Path,
            node: &str,
            meta: &ResultMeta,
            log: &[u8],
        ) {
            store_result(results, meta, log);
            let line = meta.to_bytes();
            let mut result = serde_json::to_value(meta).unwrap();
            result["result_sha256"] = json!(spawn_node_proto::codec::sha256_hex(&line));
            result["log_bytes"] = json!(meta.log_bytes.to_string());
            let done = self
                .store
                .node_gw_result(
                    node,
                    &meta.job_id,
                    i64::try_from(meta.lease_epoch).unwrap(),
                    &result,
                    |_| true,
                )
                .expect("a result");
            assert!(done.answer, "{} completes", meta.job_id);
        }

        pub(in crate::integrate) fn strikes(&self, name: &str) -> (i32, bool) {
            let node = self.store.node_v1(name).unwrap().unwrap();
            (node.host_fault_strikes, node.degraded)
        }

        /// An owner's connection in the schema.
        pub(in crate::integrate) fn admin(&self) -> postgres::Client {
            let mut client = postgres::Client::connect(self.schema.url(), postgres::NoTls)
                .expect("an admin connection");
            client
                .batch_execute(&format!("set search_path to {}", self.schema))
                .unwrap();
            client
        }

        /// The records of `kind` in the log, in order.
        pub(in crate::integrate) fn records(&self, kind: &str) -> Vec<Value> {
            self.admin()
                .query(
                    "select kind #> '{detail,record}' from spawn_events
                      where kind #>> '{detail,record,type}' = $1 order by id",
                    &[&kind],
                )
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect()
        }
    }

    fn gate_job(judging: &str) -> GateJob {
        GateJob {
            kind: RemoteJobKind::Gate,
            job_id: job_id("gate", CANDIDATE),
            judging_id: Some(judging.to_string()),
            task_id: Some(7),
            repo: "spawn".into(),
            inputs: job_inputs(),
            env: grant_env(&[]),
            artifacts: Vec::new(),
            requires: BTreeMap::from([("pg".to_string(), "16".to_string())]),
        }
    }

    const JUDGING: &str = "0123456789ab0123456789abcdef";

    /// Run `job` on `node` to a stored result of `exit` and `verdict`,
    /// judged and settled: the judging and the fault's outcome.
    fn run_on(
        ledger: &Ledger,
        results: &Path,
        node: &str,
        job: &str,
        exit: Option<i64>,
        verdict: Option<&str>,
    ) -> (Judged, Option<FaultOutcome>) {
        let grants = ledger.beat(node);
        let epoch = grants
            .iter()
            .find(|(id, _)| id == job)
            .unwrap_or_else(|| panic!("{job} not granted to {node}: {grants:?}"))
            .1;
        assert_eq!(
            step(&ledger.store, job, Instant::now(), DEFAULT_QUEUE_WAIT).unwrap(),
            Waited::Leased(epoch)
        );
        let (m, log) = meta(
            job,
            epoch,
            exit,
            verdict,
            "| out\n",
            "err\n",
            Some(&job_inputs()),
        );
        ledger.complete(results, node, &m, &log);
        let Waited::Completed(row) =
            step(&ledger.store, job, Instant::now(), DEFAULT_QUEUE_WAIT).unwrap()
        else {
            panic!("{job} not completed");
        };
        let stored = read_stored(results, job, row.lease_epoch).unwrap();
        let judged = judge_stored(&stored, &job_inputs());
        let outcome = settle(&ledger.store, &row, &judged).unwrap();
        (judged, outcome)
    }

    /// The ledger end to end, without a strike: a verdict leaves no fault;
    /// a scenario the node cannot judge excludes it unstruck; a skewed fact
    /// is no strike. Every job names its judging `judging_id` (F11).
    #[test]
    fn remote_verdicts_exclusions_and_skews_strike_nothing_on_the_ledger() {
        let ledger = Ledger::new("remote_outcomes");
        let results = Scratch::new("outcomes-results");
        let node = ledger.enroll("n252", 1, &json!({"pg": "16"}));

        let accepted = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        let (judged, outcome) = run_on(
            &ledger,
            &results.0,
            &node,
            &accepted,
            Some(0),
            Some(&verdict("accepted")),
        );
        assert_eq!(row(&judged), ("accepted", false, None));
        assert!(outcome.is_none());
        assert_eq!(ledger.strikes("n252"), (0, false));
        let after = ledger.store.remote_job(&accepted).unwrap().unwrap();
        assert_eq!(after.state, RemoteJobState::Completed);
        assert_eq!(after.judging_id.as_deref(), Some(JUDGING));

        // A scenario the node cannot judge: excluded, unstruck, local.
        let cannot = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        let (judged, outcome) = run_on(
            &ledger,
            &results.0,
            &node,
            &cannot,
            Some(6),
            Some(r#"{"v":1,"cannot_judge":true,"scenario":"t029.sh"}"#),
        );
        assert_eq!(judged.evidence, "cannot_judge");
        let outcome = outcome.unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.next, Some(FaultNext::FallbackLocal));
        let job = ledger.store.remote_job(&cannot).unwrap().unwrap();
        assert_eq!(job.state, RemoteJobState::FallbackLocal);
        assert_eq!(job.excluded_nodes, vec![node.clone()]);
        assert!(job.fault_nodes.is_empty());
        assert_eq!(ledger.strikes("n252"), (0, false));

        // An unread fact version: a skew, no strike.
        let skew = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        let (judged, outcome) = run_on(
            &ledger,
            &results.0,
            &node,
            &skew,
            Some(0),
            Some(r#"{"v":2,"verdict":"accepted"}"#),
        );
        assert_eq!(judged.evidence, "unread_version");
        assert_eq!(outcome.unwrap().next, Some(FaultNext::FallbackLocal));
        assert_eq!(ledger.strikes("n252"), (0, false));
        for record in ledger.records("remote_job.host_fault") {
            assert!(record.get("judging").is_none(), "{record}");
            assert_eq!(record["strike"], false);
        }
    }

    /// The ledger end to end, with strikes: a node's exit 5 without a fact
    /// goes local with a strike and nothing measured (NB9); three strikes
    /// make the node degraded (no grants) until an undrain; and nothing of
    /// it is recorded for a task. Several jobs of one judging, each naming
    /// it `judging_id` (F11), never meet the judging outcome's unique index.
    #[test]
    fn remote_outcomes_on_the_ledger_strike_exclude_and_degrade() {
        let ledger = Ledger::new("remote_strikes");
        let results = Scratch::new("strikes-results");
        let node = ledger.enroll("n252", 1, &json!({"pg": "16"}));

        // NB9: exit 5, no fact: struck, local, nothing measured here.
        let room = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        let (judged, outcome) = run_on(&ledger, &results.0, &node, &room, Some(5), None);
        assert_eq!(
            row(&judged),
            ("aborted", true, Some(FaultNext::FallbackLocal))
        );
        assert!(matches!(judged.failure, Some(GateFailure::Aborted { .. })));
        let outcome = outcome.unwrap();
        assert_eq!((outcome.strikes, outcome.degraded), (Some(1), Some(false)));
        assert_eq!(
            ledger.store.remote_job(&room).unwrap().unwrap().state,
            RemoteJobState::FallbackLocal
        );

        // Exit 2, then exit 3: the second and the third strike; degraded.
        let two = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        let (_, outcome) = run_on(&ledger, &results.0, &node, &two, Some(2), None);
        let outcome = outcome.unwrap();
        assert_eq!((outcome.strikes, outcome.degraded), (Some(2), Some(false)));
        // Back in the queue, never to this node again.
        let job = ledger.store.remote_job(&two).unwrap().unwrap();
        assert_eq!(job.state, RemoteJobState::Queued);
        assert_eq!(job.excluded_nodes, vec![node.clone()]);
        let three = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        let (_, outcome) = run_on(&ledger, &results.0, &node, &three, Some(3), None);
        let outcome = outcome.unwrap();
        assert_eq!((outcome.strikes, outcome.degraded), (Some(3), Some(true)));
        assert_eq!(outcome.next, Some(FaultNext::FallbackLocal));
        assert_eq!(ledger.strikes("n252"), (3, true));
        // A degraded node is granted nothing, until the owner's undrain.
        enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        assert!(ledger.beat(&node).is_empty());
        assert!(
            ledger
                .store
                .node_admin("n252", spawn_postgres::NodeAction::Undrain)
                .unwrap()
        );
        assert_eq!(ledger.strikes("n252"), (0, false));

        // F11: the ledger's records name the job, never a `judging`; and no
        // task record came of any remote run (no abort, host fault or
        // no_room toward a halt or a back-off).
        let faults = ledger.records("remote_job.host_fault");
        assert_eq!(faults.len(), 3, "{faults:?}");
        for record in faults.iter().chain(&ledger.records("remote_job.completed")) {
            assert!(record.get("judging").is_none(), "{record}");
            assert_eq!(record["proto"], "node-v1");
        }
        for kind in ["task.gated", "task.halted", "task.shadow_gated"] {
            assert!(ledger.records(kind).is_empty(), "{kind}");
        }
    }

    /// Poison pill: a job that faults on two nodes is the cause; the first
    /// node's strike is given back and the job goes local.
    #[test]
    fn a_job_that_faults_on_two_nodes_gives_the_first_its_strike_back() {
        let ledger = Ledger::new("remote_poison");
        let results = Scratch::new("poison-results");
        let first = ledger.enroll("na", 1, &json!({"pg": "16"}));
        let second = ledger.enroll("nb", 2, &json!({"pg": "16"}));
        let job = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        let (judged, outcome) = run_on(&ledger, &results.0, &first, &job, Some(2), None);
        assert!(judged.strike);
        let outcome = outcome.unwrap();
        assert_eq!(outcome.next, Some(FaultNext::Queued));
        assert_eq!(outcome.poison, Some(false));
        assert_eq!(ledger.strikes("na"), (1, false));
        // Never back to the first node.
        assert!(ledger.beat(&first).is_empty());
        let (_, outcome) = run_on(&ledger, &results.0, &second, &job, Some(137), None);
        let outcome = outcome.unwrap();
        assert_eq!(outcome.poison, Some(true));
        assert_eq!(outcome.next, Some(FaultNext::FallbackLocal));
        assert_eq!(ledger.strikes("na"), (0, false));
        assert_eq!(ledger.strikes("nb"), (0, false));
        let row = ledger.store.remote_job(&job).unwrap().unwrap();
        assert_eq!(row.state, RemoteJobState::FallbackLocal);
        assert_eq!(row.fault_nodes, vec![first, second]);
        let faults = ledger.records("remote_job.host_fault");
        assert_eq!(faults.last().unwrap()["poison"], true);
        assert_eq!(faults.last().unwrap()["strike"], false);
    }

    /// No node leases it within the wait: the primary takes it back, once;
    /// a job a node holds is never taken back.
    #[test]
    fn a_job_no_node_leases_in_time_is_taken_back_by_the_primary() {
        let ledger = Ledger::new("remote_take_local");
        let job = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        assert_eq!(
            step(&ledger.store, &job, Instant::now(), Duration::from_secs(30)).unwrap(),
            Waited::Queued
        );
        let waited = wait_for_lease(
            &ledger.store,
            &job,
            Duration::from_millis(300),
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(waited, Waited::TakenLocal);
        let row = ledger.store.remote_job(&job).unwrap().unwrap();
        assert_eq!(row.state, RemoteJobState::FallbackLocal);
        assert_eq!(
            step(&ledger.store, &job, Instant::now(), Duration::ZERO).unwrap(),
            Waited::Ended(Some(RemoteJobState::FallbackLocal))
        );
        // Leased first: the wait ends with the lease, nothing taken back.
        let node = ledger.enroll("n1", 3, &json!({"pg": "16"}));
        let leased = enqueue(&ledger.store, &gate_job(JUDGING)).unwrap();
        let grants = ledger.beat(&node);
        assert_eq!(grants.len(), 1);
        let waited =
            wait_for_lease(&ledger.store, &leased, Duration::ZERO, Duration::ZERO).unwrap();
        assert_eq!(waited, Waited::Leased(grants[0].1));
        assert_eq!(
            step(
                &ledger.store,
                "no-such-job-0000",
                Instant::now(),
                Duration::ZERO
            )
            .unwrap(),
            Waited::Ended(None)
        );
        // The sweep runs as the owner, as the integrator's pass does.
        let swept = sweep(&ledger.store).unwrap();
        assert_eq!(swept.fenced, 0);
    }

    /// The sweep runs from the integrator's own pass: a node silent past
    /// a lease's time is recorded lost there, once (level-triggered).
    #[test]
    fn the_integrators_pass_sweeps_the_nodes() {
        let ledger = Ledger::new("remote_pass_sweeps");
        let dir = Scratch::new("pass-sweeps");
        ledger.enroll("quiet", 4, &json!({"pg": "16"}));
        ledger
            .admin()
            .batch_execute("update spawn_node_state set last_beat_ms = 1 where name = 'quiet'")
            .unwrap();
        let gate = dir.0.join("gate.sh");
        std::fs::write(&gate, "exit 0\n").unwrap();
        let room = gate::Room::from_env(|_| None).measured_by(
            |_| Some(u64::MAX),
            || Some(u64::MAX),
            |_| Ok(()),
        );
        let integrator = super::super::Integrator::new(
            dir.0.join("host"),
            gate,
            Vec::new(),
            room,
            super::super::DEFAULT_GATES,
        );
        integrator.pass_on(&ledger.store).expect("a pass");
        let lost = ledger.records("node.lost");
        assert_eq!(lost.len(), 1, "{lost:?}");
        assert_eq!(lost[0]["name"], "quiet");
        assert!(
            ledger
                .store
                .node_v1("quiet")
                .unwrap()
                .unwrap()
                .lost_recorded
        );
        integrator.pass_on(&ledger.store).expect("the next pass");
        assert_eq!(ledger.records("node.lost").len(), 1);
    }

    /// The grant a node is given carries the job's `requires` in its spec
    /// (the gateway builds the grant's `requires` from it), equal to the
    /// row's own, which the ledger schedules by.
    #[test]
    fn a_grant_carries_the_jobs_requires() {
        let ledger = Ledger::new("remote_grant_requires");
        let mut job = gate_job(JUDGING);
        job.requires.insert("registry".to_string(), "r".repeat(64));
        job.requires.insert("web_lock".to_string(), "w".repeat(64));
        let id = enqueue(&ledger.store, &job).unwrap();
        // A node without the registry is granted nothing.
        let short = ledger.enroll("short", 5, &json!({"pg": "16"}));
        assert!(ledger.beat_grants(&short).is_empty());
        let holds = serde_json::to_value(&job.requires).unwrap();
        let node = ledger.enroll("holder", 6, &holds);
        let grants = ledger.beat_grants(&node);
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0]["job_id"], id);
        assert_eq!(grants[0]["spec"]["requires"], holds);
        let row = ledger.store.remote_job(&id).unwrap().unwrap();
        assert_eq!(row.requires, holds);
        assert_eq!(row.spec["requires"], row.requires);
    }
}

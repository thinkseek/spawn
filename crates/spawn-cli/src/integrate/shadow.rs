//! Shadow judging on a gate runner (milestone M2): a copy of a judged
//! verdict is judged again on a runner, on the same immutable inputs, and
//! recorded beside the primary's (`task.shadow_gated`). It never decides
//! anything: a landing never waits for it, and it changes no task, no
//! landing, no halt, no count and no budget. Before a runner's verdict may
//! decide a landing (M3), the records must show that it agrees with the
//! primary's.
//!
//! With `SPAWN_GATE_SHADOW_RUNNER` naming the runner (the transport's name
//! for it; unset or empty: no shadow, the runner is never called) and
//! `deploy/gate-runner.sh` in the controller of the mission's gate directory
//! (two levels above it), every `accepted` or `rejected` verdict a gate run
//! gave (its `env` is not null) is offered here by the judging thread
//! ([`Want`]); the main thread's pass starts it ([`Shadows::offer`]):
//! `<controller>/deploy/gate-runner.sh <runner> <the gate's repository>
//! <inputs.base> <inputs.candidate> <scenarios> <out>`, in the integrator's
//! own environment (the dispatcher reads its GATE_RUNNER_* there).
//! `<scenarios>` is a snapshot of the gate directory taken as the shadow
//! starts, so the digest recorded is the set the runner received even if
//! the gate directory changes while the dispatcher packs it (it then ships
//! the scripts of its own controller, the one named above).
//!
//! One shadow at a time: a verdict offered while one runs is skipped, never
//! queued, and `SPAWN_GATE_SHADOW_EVERY=N` shadows every N-th verdict
//! offered. Its end is recorded once ([`record_of`]); `agrees` compares the
//! two verdicts only when every `inputs` field is equal (`unequal` empty):
//! two verdicts on different inputs are no comparison.
//!
//! A second source, beside the runner and independent of it (both may run
//! at once, each recording its own `task.shadow_gated`): with
//! `SPAWN_GATE_SHADOW_NODE=1` ([`NODE_ENV`]) the same verdicts, every N-th
//! of them by the same `SPAWN_GATE_SHADOW_EVERY`, are queued as
//! `gate_shadow` jobs for spawn-node v1 ([`NodeShadows`], design §12.4 M2):
//! the artifacts are written for the gateway and the job is queued
//! ([`super::remote`]); each pass looks at the jobs in flight, takes back
//! one no node leased within `SPAWN_REMOTE_QUEUE_WAIT_SECS`, and reads a
//! node's stored result into the very record a runner's end makes
//! ([`node_record_of`]: `runner` is `node:<name>`, `inputs` what the node
//! says it judged). A node's fault is judged on the ledger
//! ([`super::remote::settle`]: strikes, exclusion, re-queue), never on a
//! task. Nothing waits for it: the primary's verdict was recorded before
//! it was offered.
//!
//! Like the base warm ([`super::warm`]) it cannot outlive the integrator
//! unaccounted for: started from the main thread under `setpriv --pdeathsig
//! TERM`, in its own process group in the integrator's tree, its pid and
//! start time in [`PID_FILE`]; a starting integrator stops the shadow that
//! file names when it is still that process ([`Shadows::take_over`]), and
//! removes what its predecessor left in the shadow directory.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use spawn_node_proto::grant::ArtifactRole;
use spawn_postgres::PostgresStore;

use super::inputs;
use super::remote;
use super::warm::{exited_unreaped, setpriv_available, signal_group, start_ticks};

/// The runner's name for the transport; unset or empty: no shadow.
pub(super) const RUNNER_ENV: &str = "SPAWN_GATE_SHADOW_RUNNER";

/// Shadow every N-th verdict (a whole number from 1; default 1).
pub(super) const EVERY_ENV: &str = "SPAWN_GATE_SHADOW_EVERY";

/// `1`: shadow on spawn-node v1 too ([`NodeShadows`]); unset, empty or
/// `0`: not.
pub(super) const NODE_ENV: &str = "SPAWN_GATE_SHADOW_NODE";

/// Node shadows in flight at most: a verdict offered beyond is skipped,
/// never queued.
const NODE_IN_FLIGHT: usize = 4;

/// How long the registry's manifest is reused before it is read again (it
/// hashes the whole cargo registry).
const REGISTRY_EVERY: Duration = Duration::from_hours(1);

/// The shadow's pid file, in the shadow directory.
pub(super) const PID_FILE: &str = "shadow.pid";

/// The dispatcher, relative to the controller.
const DISPATCHER: &str = "deploy/gate-runner.sh";

/// How long a stopped shadow has after TERM before its group is killed.
const GRACE: Duration = Duration::from_secs(10);

/// The dispatcher's status when the runner is not ready and it wrote
/// nothing (deploy/gate-runner/README.md).
const REFUSED_EXIT: i64 = 3;

/// Who shadows and how often.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Settings {
    pub(super) runner: String,
    pub(super) every: u64,
}

/// The settings from [`RUNNER_ENV`] and [`EVERY_ENV`]'s texts: None when no
/// runner is named; an error for an EVERY that is not a whole number from 1.
pub(super) fn settings(
    runner: Option<&str>,
    every: Option<&str>,
) -> Result<Option<Settings>, String> {
    let Some(runner) = runner.map(str::trim).filter(|runner| !runner.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(Settings {
        runner: runner.to_string(),
        every: every_of(every)?,
    }))
}

/// [`EVERY_ENV`]'s text: unset or empty is 1.
fn every_of(every: Option<&str>) -> Result<u64, String> {
    match every.map(str::trim).filter(|every| !every.is_empty()) {
        None => Ok(1),
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|every| *every >= 1)
            .ok_or_else(|| format!("{EVERY_ENV} is a whole number from 1, not {text:?}")),
    }
}

/// How the node source shadows: how often, the directories it shares with
/// the gateway, the cargo home whose registry nodes must hold, and how
/// long a job waits for a lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NodeSettings {
    pub(super) every: u64,
    pub(super) artifacts: PathBuf,
    pub(super) results: PathBuf,
    pub(super) cargo_home: PathBuf,
    pub(super) queue_wait: Duration,
}

/// The node source's settings from the environment (`lookup`): None unless
/// [`NODE_ENV`] is `1`; an error for another value, an EVERY that is not a
/// whole number from 1, or no artifact or results directory.
pub(super) fn node_settings(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<NodeSettings>, String> {
    match lookup(NODE_ENV).as_deref().map(str::trim) {
        None | Some("" | "0") => return Ok(None),
        Some("1") => {}
        Some(other) => return Err(format!("{NODE_ENV} is 1 or 0, not {other:?}")),
    }
    let every = every_of(lookup(EVERY_ENV).as_deref())?;
    let directory = |name: &str| -> Result<PathBuf, String> {
        lookup(name)
            .map(|text| PathBuf::from(text.trim()))
            .filter(|path| path.is_absolute())
            .ok_or_else(|| format!("{NODE_ENV}=1 needs {name}, an absolute directory"))
    };
    let artifacts = directory(remote::ARTIFACTS_ENV)?;
    let results = directory(remote::RESULTS_ENV)?;
    let cargo_home = lookup("CARGO_HOME")
        .filter(|text| !text.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| lookup("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .ok_or_else(|| format!("{NODE_ENV}=1 needs CARGO_HOME or HOME"))?;
    Ok(Some(NodeSettings {
        every,
        artifacts,
        results,
        cargo_home,
        queue_wait: remote::queue_wait(lookup(remote::QUEUE_WAIT_ENV).as_deref()),
    }))
}

/// A verdict to shadow: the primary's record, as the judging wrote it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Want {
    pub(super) task: i64,
    pub(super) revision: Value,
    /// The primary's verdict: `accepted` or `rejected`.
    pub(super) verdict: String,
    /// The primary's `inputs` and `env`.
    pub(super) inputs: Value,
    pub(super) env: Value,
    /// The gate's repository (its first argument) and the gate directory.
    pub(super) repo: PathBuf,
    pub(super) scenarios: PathBuf,
    /// The judging that gave the verdict, when one did (a node's job names
    /// it `judging_id`).
    pub(super) judging_id: Option<String>,
}

impl Want {
    /// The shadow of a `task.gated` record the judging just wrote, when it
    /// is one to shadow: an `accepted` or `rejected` verdict a gate run gave
    /// (its `env` is not null) with a base and a candidate.
    pub(super) fn of(record: &Value, repo: &Path, scenarios: &Path) -> Option<Self> {
        let verdict = record["verdict"].as_str()?;
        if !matches!(verdict, "accepted" | "rejected") || !record["env"].is_object() {
            return None;
        }
        let inputs = &record["inputs"];
        inputs["base"].as_str()?;
        inputs["candidate"].as_str()?;
        Some(Self {
            task: record["id"].as_i64()?,
            revision: record["revision"].clone(),
            verdict: verdict.to_string(),
            inputs: inputs.clone(),
            env: record["env"].clone(),
            repo: repo.to_path_buf(),
            scenarios: scenarios.to_path_buf(),
            judging_id: super::judging::current_id(),
        })
    }
}

/// The shadow in flight.
struct Shadow {
    want: Want,
    child: Child,
    started: Instant,
    /// Its own directory: the snapshot, the out directory and the log.
    work: PathBuf,
    /// What the runner was given and received.
    given: Value,
}

/// A shadow that ended: its record, and the directory to remove once the
/// record is written.
pub(super) struct Ended {
    pub(super) record: Value,
    pub(super) work: PathBuf,
}

/// The integrator's shadows.
pub(super) struct Shadows {
    settings: Settings,
    dir: PathBuf,
    /// Verdicts offered so far (for [`Settings::every`]).
    offered: u64,
    /// Shadows started so far (each its own directory).
    started: u64,
    running: Option<Shadow>,
    /// A shadow that ended whose record could not be written yet.
    unrecorded: Option<Ended>,
}

impl Shadows {
    pub(super) fn new(settings: Settings, dir: PathBuf) -> Self {
        Self {
            settings,
            dir,
            offered: 0,
            started: 0,
            running: None,
            unrecorded: None,
        }
    }

    /// Write the record of the shadow that ended ([`Self::poll`]) with
    /// `write`, once: its directory is removed when it is written; one that
    /// could not be is kept and written at the next call.
    pub(super) fn record_ended(&mut self, write: impl Fn(&Value) -> Result<(), String>) {
        let Some(ended) = self.unrecorded.take().or_else(|| self.poll()) else {
            return;
        };
        match write(&ended.record) {
            Ok(()) => remove_tree(&ended.work),
            Err(error) => {
                eprintln!(
                    "integrate: task {}'s shadow record: {error}; tried again at the next pass",
                    ended.record["id"]
                );
                self.unrecorded = Some(ended);
            }
        }
    }

    /// A verdict to shadow, on the main thread: started now, unless this is
    /// not an N-th verdict, a shadow runs already (skipped, never queued),
    /// or the controller has no dispatcher.
    pub(super) fn offer(&mut self, want: Want) {
        self.offered += 1;
        let task = want.task;
        if !self.offered.is_multiple_of(self.settings.every) {
            println!(
                "integrate: task {task}: verdict not shadowed (every {}-th is)",
                self.settings.every
            );
            return;
        }
        if let Some(running) = &self.running {
            println!(
                "integrate: task {task}: verdict not shadowed: task {}'s shadow runs on {}",
                running.want.task, self.settings.runner
            );
            return;
        }
        match self.start(want) {
            Ok(shadow) => {
                println!(
                    "integrate: task {task}: shadow on {} started (pid {}), in {}",
                    self.settings.runner,
                    shadow.child.id(),
                    shadow.work.display()
                );
                self.running = Some(shadow);
            }
            Err(error) => println!("integrate: task {task}: verdict not shadowed: {error}"),
        }
    }

    fn start(&mut self, want: Want) -> Result<Shadow, String> {
        let controller = inputs::controller_of(&want.scenarios)
            .ok_or_else(|| format!("{} has no controller above it", want.scenarios.display()))?;
        let dispatcher = controller.join(DISPATCHER);
        if !dispatcher.is_file() {
            return Err(format!("no {}", dispatcher.display()));
        }
        self.started += 1;
        let work = self.dir.join(format!(
            "{}-{}-{}",
            want.task,
            std::process::id(),
            self.started
        ));
        remove_tree(&work);
        std::fs::create_dir_all(&work).map_err(|error| format!("{}: {error}", work.display()))?;
        let (child, given) = match self.launch(&want, &controller, &dispatcher, &work) {
            Ok(started) => started,
            Err(error) => {
                remove_tree(&work);
                return Err(error);
            }
        };
        let pid = child.id();
        if let Some(ticks) = start_ticks(pid) {
            let record = format!("pid {pid}\nstart {ticks}\ntask {}\n", want.task);
            if let Err(error) = std::fs::write(self.dir.join(PID_FILE), record) {
                eprintln!(
                    "integrate: task {}'s shadow: {}: {error}; a crash of this integrator leaves \
                     it to its parent-death signal",
                    want.task,
                    self.dir.join(PID_FILE).display()
                );
            }
        }
        Ok(Shadow {
            want,
            child,
            started: Instant::now(),
            work,
            given,
        })
    }

    /// Snapshot the gate directory into `work`, read what the runner is
    /// given, and start the dispatcher on it: the child and those `inputs`.
    fn launch(
        &self,
        want: &Want,
        controller: &Path,
        dispatcher: &Path,
        work: &Path,
    ) -> Result<(Child, Value), String> {
        let base = want.inputs["base"].as_str().unwrap_or_default();
        let candidate = want.inputs["candidate"].as_str().unwrap_or_default();
        // The controller, read as the shadow starts; the set, a snapshot
        // read once: what the dispatcher packs is this copy.
        let controller_commit = inputs::controller_commit(controller);
        let snapshot = work.join("scenarios");
        let copied = Command::new("cp")
            .arg("-R")
            .arg("--")
            .arg(&want.scenarios)
            .arg(&snapshot)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("cannot run cp: {error}"))?;
        if !copied.status.success() {
            return Err(format!(
                "cannot copy {}: {}",
                want.scenarios.display(),
                String::from_utf8_lossy(&copied.stderr).trim()
            ));
        }
        let given = json!({
            "base": base,
            "candidate": candidate,
            "controller": controller_commit,
            "scenarios_sha256": inputs::scenarios_sha256(&snapshot),
            "scenario": null,
        });
        let out = work.join("out");
        std::fs::create_dir_all(&out).map_err(|error| format!("{}: {error}", out.display()))?;
        let log = work.join("dispatch.log");
        let stdout =
            std::fs::File::create(&log).map_err(|error| format!("{}: {error}", log.display()))?;
        let stderr = stdout
            .try_clone()
            .map_err(|error| format!("{}: {error}", log.display()))?;
        let mut command = if setpriv_available() {
            let mut command = Command::new("setpriv");
            command.args(["--pdeathsig", "TERM", "bash"]);
            command
        } else {
            Command::new("bash")
        };
        command
            .arg(dispatcher)
            .arg(&self.settings.runner)
            .arg(&want.repo)
            .arg(base)
            .arg(candidate)
            .arg(&snapshot)
            .arg(&out)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0);
        let child = command
            .spawn()
            .map_err(|error| format!("cannot run {}: {error}", dispatcher.display()))?;
        Ok((child, given))
    }

    /// The shadow that ended since the last call, with its record; None
    /// while it runs (or when none does).
    pub(super) fn poll(&mut self) -> Option<Ended> {
        let shadow = self.running.as_mut()?;
        let status = match shadow.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(status)) => status,
            Err(error) => {
                eprintln!(
                    "integrate: task {}'s shadow: cannot wait for it: {error}",
                    shadow.want.task
                );
                self.stop("cannot wait for it");
                return None;
            }
        };
        let shadow = self.running.take()?;
        let _ = std::fs::remove_file(self.dir.join(PID_FILE));
        let exit = status
            .code()
            .unwrap_or_else(|| status.signal().map_or(-1, |signal| 128 + signal));
        let record = record_of(
            &shadow.want,
            &self.settings.runner,
            &shadow.given,
            i64::from(exit),
            shadow.started.elapsed().as_secs(),
            &shadow.work.join("out"),
        );
        if exit != 0 {
            let log = std::fs::read_to_string(shadow.work.join("dispatch.log")).unwrap_or_default();
            let lines: Vec<&str> = log.lines().rev().take(5).collect();
            for line in lines.into_iter().rev() {
                println!("integrate: task {}'s shadow: | {line}", shadow.want.task);
            }
        }
        Some(Ended {
            record,
            work: shadow.work,
        })
    }

    /// Stop the shadow in flight (TERM to its group, KILL after the grace):
    /// nothing is recorded, and its directory is removed.
    pub(super) fn stop(&mut self, why: &str) {
        let Some(mut shadow) = self.running.take() else {
            return;
        };
        let group = shadow.child.id();
        signal_group(group, "TERM");
        let deadline = Instant::now() + GRACE;
        while !exited_unreaped(group) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        signal_group(group, "KILL");
        let _ = shadow.child.wait();
        let _ = std::fs::remove_file(self.dir.join(PID_FILE));
        remove_tree(&shadow.work);
        println!(
            "integrate: task {}'s shadow on {} stopped after {} s: {why}; nothing recorded",
            shadow.want.task,
            self.settings.runner,
            shadow.started.elapsed().as_secs()
        );
    }

    /// At startup: stop the shadow a predecessor left, when [`PID_FILE`]
    /// names one that is still that process (this user's, started at the
    /// recorded time, running the dispatcher), and remove whatever else it
    /// left in the shadow directory.
    pub(super) fn take_over(&self) {
        let path = self.dir.join(PID_FILE);
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Some((pid, start)) = parse_pid_file(&text)
        {
            match still_running(pid, start) {
                Ok(()) => {
                    signal_group(pid, "TERM");
                    let deadline = Instant::now() + GRACE;
                    while still_running(pid, start).is_ok() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if still_running(pid, start).is_ok() {
                        signal_group(pid, "KILL");
                    }
                    println!(
                        "integrate: stopped the shadow (pid {pid}) a previous integrator started"
                    );
                }
                Err(why) => println!(
                    "integrate: the shadow (pid {pid}) a previous integrator started is gone \
                     ({why}); nothing stopped"
                ),
            }
        }
        let _ = std::fs::remove_file(&path);
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    remove_tree(&entry.path());
                }
            }
        }
    }

    /// The shadow in flight's task, for tests.
    #[cfg(test)]
    pub(super) fn running_task(&self) -> Option<i64> {
        self.running.as_ref().map(|shadow| shadow.want.task)
    }
}

fn parse_pid_file(text: &str) -> Option<(u32, u64)> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .map(str::trim)
    };
    let pid = field("pid")?.parse().ok().filter(|pid| *pid > 1)?;
    let start = field("start")?.parse().ok()?;
    Some((pid, start))
}

/// Ok when `pid` is still the shadow a pid file names: this user's, started
/// at `start`, running the dispatcher.
fn still_running(pid: u32, start: u64) -> Result<(), String> {
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let owner = std::fs::metadata(&proc_dir)
        .map_err(|_| "no such process".to_string())?
        .uid();
    let me = std::fs::metadata("/proc/self")
        .map_err(|error| error.to_string())?
        .uid();
    if owner != me {
        return Err(format!("pid {pid} is another user's"));
    }
    if start_ticks(pid) != Some(start) {
        return Err(format!("pid {pid} is another process now"));
    }
    let cmdline = std::fs::read(proc_dir.join("cmdline")).map_err(|error| error.to_string())?;
    let runs = cmdline
        .split(|byte| *byte == 0)
        .any(|word| word.ends_with(DISPATCHER.as_bytes()));
    if !runs {
        return Err(format!("pid {pid} runs no {DISPATCHER}"));
    }
    Ok(())
}

/// A tree removed whole, read-only directories included (a deployed
/// controller is read-only, and the snapshot keeps its modes).
pub(super) fn remove_tree(path: &Path) {
    if std::fs::remove_dir_all(path).is_ok() || !path.exists() {
        return;
    }
    let _ = Command::new("chmod")
        .args(["-R", "u+rwX", "--"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_dir_all(path);
}

/// The job directory the dispatcher placed in `out` (`<out>/<job>`), if any.
fn job_dir(out: &Path) -> Option<PathBuf> {
    let mut jobs: Vec<PathBuf> = std::fs::read_dir(out)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    jobs.sort();
    jobs.into_iter().next()
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(text.lines().find(|line| !line.trim().is_empty())?).ok()
}

/// The `task.shadow_gated` record of a shadow of `want` on `runner` that
/// was given `given` (its `inputs`), ended with the dispatcher's `exit`
/// after `seconds`, and left `out`.
pub(super) fn record_of(
    want: &Want,
    runner: &str,
    given: &Value,
    exit: i64,
    seconds: u64,
    out: &Path,
) -> Value {
    let job = job_dir(out);
    let placed = job
        .as_ref()
        .and_then(|job| read_json(&job.join("record.json")));
    let result = match placed.as_ref().and_then(|placed| placed["result"].as_str()) {
        Some(result) => json!(result),
        None if placed.is_none() && exit == REFUSED_EXIT => json!("refused"),
        None => Value::Null,
    };
    let verdict = job
        .as_ref()
        .and_then(|job| read_json(&job.join("verdict.json")))
        .and_then(|verdict| verdict["verdict"].as_str().map(str::to_string))
        .filter(|verdict| matches!(verdict.as_str(), "accepted" | "rejected"));
    let toolchain = job
        .as_ref()
        .and_then(|job| std::fs::read_to_string(job.join("gate.log")).ok())
        .and_then(|log| inputs::toolchain_of(&log));
    // The machine that judged, when the dispatcher's record names it
    // (`host`, the runner's own `uname -n`); `runner` is the name dialled.
    let host = placed
        .as_ref()
        .and_then(|placed| placed["host"].as_str().map(str::to_string));
    shadow_record(
        want,
        runner,
        given,
        Shadowed {
            result,
            verdict,
            exit,
            seconds,
            host,
            toolchain,
        },
    )
}

/// How a shadow ended, whatever judged it: the record's own fields.
struct Shadowed {
    result: Value,
    verdict: Option<String>,
    exit: i64,
    seconds: u64,
    host: Option<String>,
    toolchain: Option<String>,
}

/// The one shape of `task.shadow_gated`, for a runner's end and a node's.
fn shadow_record(want: &Want, runner: &str, given: &Value, ended: Shadowed) -> Value {
    let Shadowed {
        result,
        verdict,
        exit,
        seconds,
        host,
        toolchain,
    } = ended;
    let unequal = inputs::unequal(given, &want.inputs);
    let agrees = match &verdict {
        Some(verdict)
            if unequal.is_empty() && matches!(want.verdict.as_str(), "accepted" | "rejected") =>
        {
            json!(*verdict == want.verdict)
        }
        _ => Value::Null,
    };
    json!({
        "type": "task.shadow_gated",
        "id": want.task,
        "revision": want.revision,
        "base": want.inputs["base"],
        "candidate": want.inputs["candidate"],
        "runner": runner,
        "result": result,
        "verdict": verdict,
        "exit": exit,
        "seconds": seconds,
        "inputs": given,
        "env": {"host": host, "toolchain": toolchain, "runner": runner},
        "primary": want.verdict,
        "primary_env": want.env,
        "unequal": unequal,
        "agrees": agrees,
    })
}

/// A shadow queued for a node: the verdict it shadows, its job, the inputs
/// it was granted, since when it waits for a lease, and when it was queued.
struct NodeShadow {
    want: Want,
    job_id: String,
    given: Value,
    since: Instant,
}

/// The node source's shadows ([`NODE_ENV`]): queued, looked at once a pass,
/// recorded when a node's result is read.
pub(super) struct NodeShadows {
    settings: NodeSettings,
    /// The gate's environment, of which a grant carries a part
    /// ([`remote::grant_env`]).
    gate_env: Vec<(String, String)>,
    artifacts: remote::Artifacts,
    /// Snapshots of gate directories, while their pack is made.
    work: PathBuf,
    offered: u64,
    jobs: Vec<NodeShadow>,
    registry: Option<(Instant, remote::RegistryManifest)>,
    /// Records that could not be written yet, tried again each pass.
    unrecorded: Vec<Value>,
}

impl NodeShadows {
    pub(super) fn new(
        settings: NodeSettings,
        gate_env: Vec<(String, String)>,
        work: PathBuf,
    ) -> Self {
        let artifacts = remote::Artifacts::new(settings.artifacts.clone());
        Self {
            settings,
            gate_env,
            artifacts,
            work,
            offered: 0,
            jobs: Vec::new(),
            registry: None,
            unrecorded: Vec::new(),
        }
    }

    /// A verdict to shadow on a node: queued now, unless this is not an
    /// N-th verdict or [`NODE_IN_FLIGHT`] shadows wait already (skipped,
    /// never queued later).
    pub(super) fn offer(&mut self, store: &PostgresStore, want: Want) {
        self.offered += 1;
        let task = want.task;
        if !self.offered.is_multiple_of(self.settings.every) {
            println!(
                "integrate: task {task}: verdict not shadowed on a node (every {}-th is)",
                self.settings.every
            );
            return;
        }
        if self.jobs.len() >= NODE_IN_FLIGHT {
            println!(
                "integrate: task {task}: verdict not shadowed on a node: {NODE_IN_FLIGHT} \
                 node shadows wait already"
            );
            return;
        }
        match self.enqueue(store, &want) {
            Ok((job_id, given)) => {
                println!("integrate: task {task}: shadow queued for a node as {job_id}");
                self.jobs.push(NodeShadow {
                    want,
                    job_id,
                    given,
                    since: Instant::now(),
                });
            }
            Err(error) => {
                println!("integrate: task {task}: verdict not shadowed on a node: {error}");
            }
        }
    }

    /// The registry's manifest, read again after [`REGISTRY_EVERY`].
    fn registry(&mut self) -> Result<remote::RegistryManifest, String> {
        if let Some((read, manifest)) = &self.registry
            && read.elapsed() < REGISTRY_EVERY
        {
            return Ok(manifest.clone());
        }
        let manifest = self
            .artifacts
            .registry_manifest(&self.settings.cargo_home)?;
        self.registry = Some((Instant::now(), manifest.clone()));
        Ok(manifest)
    }

    /// What a node must hold for a gate of `controller`: the registry, the
    /// gate's PostgreSQL, the web lockfile (published for the node to fetch
    /// by its digest) and the toolchain, when the controller names them.
    fn requires_of(
        &self,
        controller: &Path,
        registry: String,
    ) -> Result<std::collections::BTreeMap<String, String>, String> {
        let mut requires = std::collections::BTreeMap::from([
            ("registry".to_string(), registry),
            ("pg".to_string(), remote::PG_MAJOR.to_string()),
        ]);
        if let Some(lock) = remote::web_lock(controller) {
            // The node installs its web dependencies from this lockfile,
            // fetched by the digest it requires.
            let published = self
                .artifacts
                .put_copy(&controller.join("web/package-lock.json"))?;
            if published.digest.hex != lock {
                return Err("web/package-lock.json changed while it was published".to_string());
            }
            requires.insert("web_lock".to_string(), lock);
        }
        if let Some(channel) = remote::toolchain_channel(controller) {
            requires.insert("toolchain".to_string(), channel);
        }
        Ok(requires)
    }

    /// Write the job's artifacts and queue it: its id and the inputs it
    /// was granted.
    fn enqueue(&mut self, store: &PostgresStore, want: &Want) -> Result<(String, Value), String> {
        let controller = inputs::controller_of(&want.scenarios)
            .ok_or_else(|| format!("{} has no controller above it", want.scenarios.display()))?;
        let controller_commit = inputs::controller_commit(&controller).ok_or_else(|| {
            format!(
                "{} names no controller commit: a node could not say what it judged",
                controller.join(".controller").display()
            )
        })?;
        let scenarios_at = want
            .scenarios
            .strip_prefix(&controller)
            .map_err(|_| format!("{} is not inside its controller", want.scenarios.display()))?
            .to_path_buf();
        let base = want.inputs["base"].as_str().unwrap_or_default();
        let candidate = want.inputs["candidate"].as_str().unwrap_or_default();
        let registry = self.registry()?;
        // The set, a snapshot read once: what the pack holds is this copy.
        std::fs::create_dir_all(&self.work)
            .map_err(|error| format!("{}: {error}", self.work.display()))?;
        let snapshot = self.work.join(format!(
            "{}-{}-{}",
            want.task,
            std::process::id(),
            self.offered
        ));
        remove_tree(&snapshot);
        let copied = Command::new("cp")
            .arg("-R")
            .arg("--")
            .arg(&want.scenarios)
            .arg(&snapshot)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("cannot run cp: {error}"))?;
        if !copied.status.success() {
            remove_tree(&snapshot);
            return Err(format!(
                "cannot copy {}: {}",
                want.scenarios.display(),
                String::from_utf8_lossy(&copied.stderr).trim()
            ));
        }
        let scenarios_sha256 = inputs::scenarios_sha256(&snapshot);
        let pack = self.artifacts.controller_pack(
            &controller,
            &snapshot,
            &scenarios_at,
            scenarios_sha256
                .as_deref()
                .and_then(|digest| {
                    remote::controller_pack_key(
                        &controller,
                        &controller_commit,
                        &snapshot,
                        &scenarios_at,
                        digest,
                    )
                    .ok()
                })
                .as_deref(),
        );
        remove_tree(&snapshot);
        let pack = pack?;
        let delta = self.artifacts.delta_bundle(&want.repo, base, candidate)?;
        let baseline = self.artifacts.baseline_bundle(&want.repo, base)?;
        let given = json!({
            "base": base,
            "candidate": candidate,
            "controller": controller_commit,
            "scenarios_sha256": scenarios_sha256,
            "scenario": null,
        });
        let requires = self.requires_of(&controller, registry.registry)?;
        let repo = want
            .repo
            .file_name()
            .map(|name| name.to_string_lossy().trim_end_matches(".git").to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "repo".to_string());
        let job = remote::GateJob {
            kind: spawn_postgres::RemoteJobKind::GateShadow,
            job_id: remote::job_id("gs", candidate),
            judging_id: want.judging_id.clone(),
            task_id: Some(want.task),
            repo,
            inputs: given.clone(),
            env: remote::grant_env(&self.gate_env),
            artifacts: vec![
                delta.url(ArtifactRole::DeltaBundle),
                baseline.url(ArtifactRole::BaselineBundle),
                pack.url(ArtifactRole::ControllerPack),
            ],
            requires,
        };
        let job_id = remote::enqueue(store, &job)?;
        Ok((job_id, given))
    }

    /// Each pass: the jobs in flight looked at once. One no node leased in
    /// time is taken back (nothing recorded); a node's result is read,
    /// judged on the ledger (a fault re-queued waits again) and recorded
    /// with `write`, once (a record that could not be written is tried
    /// again at the next pass).
    pub(super) fn poll(
        &mut self,
        store: &PostgresStore,
        write: &dyn Fn(&Value) -> Result<(), String>,
    ) {
        self.unrecorded.retain(|record| write(record).is_err());
        self.reclaim_orphans(store);
        let wait = self.settings.queue_wait;
        let mut kept = Vec::new();
        for mut job in std::mem::take(&mut self.jobs) {
            let task = job.want.task;
            match remote::step(store, &job.job_id, job.since, wait) {
                Ok(remote::Waited::Queued) => kept.push(job),
                // Its wait starts again from here: a lease fenced back to
                // the queue gets a whole wait, not an immediate take-back.
                Ok(remote::Waited::Leased(_)) => {
                    job.since = Instant::now();
                    kept.push(job);
                }
                Ok(remote::Waited::TakenLocal) => println!(
                    "integrate: task {task}'s node shadow {}: no node leased it within {} s; \
                     nothing recorded",
                    job.job_id,
                    wait.as_secs()
                ),
                Ok(remote::Waited::Ended(state)) => println!(
                    "integrate: task {task}'s node shadow {} ended {state:?}; nothing recorded",
                    job.job_id
                ),
                Ok(remote::Waited::Completed(row)) => match self.completed(store, &job, &row) {
                    Completion::Requeued => {
                        job.since = Instant::now();
                        kept.push(job);
                    }
                    Completion::Record(record) => {
                        if let Err(error) = write(&record) {
                            eprintln!(
                                "integrate: task {task}'s node shadow record: {error}; \
                                     tried again at the next pass"
                            );
                            self.unrecorded.push(record);
                        }
                    }
                    Completion::Nothing => {}
                },
                Err(error) => {
                    eprintln!(
                        "integrate: task {task}'s node shadow {}: {error}; looked at again",
                        job.job_id
                    );
                    kept.push(job);
                }
            }
        }
        self.jobs = kept;
    }

    /// Take back every queued shadow this integrator does not wait for (a
    /// predecessor's, left by a restart): nobody would read its result, and
    /// it would hold a node's slot and its `need_sync` on a registry that
    /// may be gone. A node that won the race keeps it.
    fn reclaim_orphans(&self, store: &PostgresStore) {
        let queued = match store.remote_jobs_queued(spawn_postgres::RemoteJobKind::GateShadow) {
            Ok(queued) => queued,
            Err(error) => {
                eprintln!("integrate: the queued node shadows not read: {error}");
                return;
            }
        };
        for orphan in queued
            .iter()
            .filter(|job| !self.jobs.iter().any(|mine| mine.job_id == job.job_id))
        {
            match store.remote_job_take_local(&orphan.job_id, orphan.lease_epoch) {
                Ok(true) => println!(
                    "integrate: node shadow {} (task {:?}) queued by an earlier integrator: \
                     taken back",
                    orphan.job_id, orphan.task_id
                ),
                Ok(false) => {}
                Err(error) => eprintln!(
                    "integrate: node shadow {} not taken back: {error}",
                    orphan.job_id
                ),
            }
        }
    }

    /// A completed job: its stored result judged (§6.2) and settled on the
    /// ledger; its record, unless the job went to another node.
    fn completed(
        &self,
        store: &PostgresStore,
        job: &NodeShadow,
        row: &spawn_postgres::RemoteJob,
    ) -> Completion {
        let task = job.want.task;
        let stored = match remote::read_stored(&self.settings.results, &row.job_id, row.lease_epoch)
        {
            Ok(stored) => stored,
            Err(error) => {
                println!(
                    "integrate: task {task}'s node shadow {}: its result cannot be read: \
                     {error}; nothing recorded",
                    row.job_id
                );
                let unread = remote::Judged::unreadable(error);
                if let Err(error) = remote::settle(store, row, &unread) {
                    eprintln!(
                        "integrate: task {task}'s node shadow {}: {error}",
                        row.job_id
                    );
                }
                return Completion::Nothing;
            }
        };
        let judged = remote::judge_stored(&stored, &job.given);
        // A rejection naming only test-database errors is no valid run:
        // the node's strikes are not reset (nor struck); still recorded.
        let no_work_failure = judged.verdict.as_ref().is_some_and(|verdict| {
            remote::names_no_work_failure(verdict, super::needs_test_database(&job.want.scenarios))
        });
        let settled = if no_work_failure {
            Ok(None)
        } else {
            remote::settle(store, row, &judged)
        };
        let requeued = matches!(
            &settled,
            Ok(Some(outcome)) if outcome.ok && outcome.next == Some(spawn_postgres::FaultNext::Queued)
        );
        match &settled {
            Ok(Some(outcome)) => println!(
                "integrate: task {task}'s node shadow {}: {} ({}): strike {}, next {:?}{}",
                row.job_id,
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
                "integrate: task {task}'s node shadow {}: {error}",
                row.job_id
            ),
        }
        if requeued {
            return Completion::Requeued;
        }
        if judged.evidence == "self_fenced" {
            return Completion::Nothing;
        }
        let runner = format!("node:{}", node_name(store, row, &stored));
        Completion::Record(node_record_of(&job.want, &runner, &stored, &judged))
    }

    /// The node shadows in flight's jobs, for tests.
    #[cfg(test)]
    pub(super) fn waiting(&self) -> Vec<String> {
        self.jobs.iter().map(|job| job.job_id.clone()).collect()
    }
}

/// What a completed node shadow leaves.
enum Completion {
    /// Its record.
    Record(Value),
    /// Back in the queue for another node: waited for again.
    Requeued,
    /// Nothing (a self-fenced run, a result that cannot be read).
    Nothing,
}

/// The name of the node that ran `row`: the ledger's, else what the node
/// said, else its id's first 12 digits.
fn node_name(
    store: &PostgresStore,
    row: &spawn_postgres::RemoteJob,
    stored: &remote::Stored,
) -> String {
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

/// The `task.shadow_gated` record of a node's stored result for `want`,
/// judged `judged`, on `runner` (`node:<name>`): the shape a runner's end
/// has ([`record_of`]); `inputs` what the node says it judged, a verdict
/// only when the node's gate wrote one, and `agrees` only for a verdict on
/// the job's own commits with every input equal to the primary's.
pub(super) fn node_record_of(
    want: &Want,
    runner: &str,
    stored: &remote::Stored,
    judged: &remote::Judged,
) -> Value {
    let meta = &stored.meta;
    // A node that said no inputs (its fetch failed before it read them):
    // an empty object, never null, which the record's writer refuses.
    let given = meta
        .inputs
        .as_ref()
        .and_then(|inputs| serde_json::to_value(inputs).ok())
        .unwrap_or_else(|| json!({}));
    let exit = meta.exit_code.unwrap_or_else(|| match &meta.signal {
        Some(spawn_node_proto::result::Signal::Number(number)) => {
            128 + i64::try_from(*number).unwrap_or(0)
        }
        _ => -1,
    });
    let env = |name: &str| meta.env.as_ref().and_then(|env| env.get(name).cloned());
    let toolchain = env("toolchain").or_else(|| {
        meta.streams(&stored.log)
            .ok()
            .and_then(|(stdout, _)| inputs::toolchain_of(&String::from_utf8_lossy(stdout)))
    });
    let believed = judged.verdict.is_some();
    shadow_record(
        want,
        runner,
        &given,
        Shadowed {
            result: if believed {
                json!("verdict")
            } else {
                json!(judged.evidence)
            },
            // A verdict on other commits or inputs is no verdict here.
            verdict: judged.stated.clone().filter(|_| believed),
            exit,
            seconds: meta.seconds,
            host: env("host"),
            toolchain,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{PID_FILE, Settings, Shadows, Want, record_of, settings};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(case: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "spawn-shadow-test-{case}-{}-{}",
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
            super::remove_tree(&self.0);
        }
    }

    const BASE: &str = "1111111111111111111111111111111111111111";
    const CANDIDATE: &str = "2222222222222222222222222222222222222222";

    fn primary_inputs(scenarios: &Path, controller: Option<&str>) -> Value {
        json!({"base": BASE, "candidate": CANDIDATE, "controller": controller,
               "scenarios_sha256": super::inputs::scenarios_sha256(scenarios),
               "scenario": null})
    }

    fn want(task: i64, verdict: &str, inputs: Value, scenarios: &Path) -> Want {
        Want {
            task,
            revision: json!(1),
            verdict: verdict.to_string(),
            inputs,
            env: json!({"host": "primary", "toolchain": "rustc p", "runner": null}),
            repo: PathBuf::from("/nonexistent/repo.git"),
            scenarios: scenarios.to_path_buf(),
            judging_id: None,
        }
    }

    /// A placed job: `<out>/<job>/{record.json,verdict.json,gate.log}`.
    fn place(out: &Path, result: &str, verdict: Option<&str>, log: &str) {
        let job = out.join("222222222222-abc");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(
            job.join("record.json"),
            format!(
                "{}\n",
                json!({"job": "222222222222-abc", "host": "runner-host", "result": result})
            ),
        )
        .unwrap();
        if let Some(verdict) = verdict {
            std::fs::write(
                job.join("verdict.json"),
                format!("{}\n", json!({"verdict": verdict})),
            )
            .unwrap();
        }
        std::fs::write(job.join("gate.log"), log).unwrap();
    }

    #[test]
    fn the_settings_need_a_runner_and_every_is_a_whole_number() {
        assert_eq!(settings(None, Some("2")).unwrap(), None);
        assert_eq!(settings(Some(" "), None).unwrap(), None);
        assert_eq!(
            settings(Some("bb-runner"), None).unwrap(),
            Some(Settings {
                runner: "bb-runner".into(),
                every: 1
            })
        );
        assert_eq!(settings(Some("r"), Some("3")).unwrap().unwrap().every, 3);
        assert!(settings(Some("r"), Some("0")).is_err());
        assert!(settings(Some("r"), Some("x")).is_err());
    }

    #[test]
    fn only_a_judged_verdict_with_its_env_is_shadowed() {
        let inputs = json!({"base": BASE, "candidate": CANDIDATE});
        let record = |verdict: &str, env: Value| {
            json!({"type": "task.gated", "id": 7, "revision": 2, "verdict": verdict,
                   "inputs": inputs, "env": env})
        };
        let env = json!({"host": "h", "toolchain": null, "runner": null});
        let at = Path::new("/x");
        assert!(Want::of(&record("accepted", env.clone()), at, at).is_some());
        assert!(Want::of(&record("rejected", env.clone()), at, at).is_some());
        assert!(Want::of(&record("accepted", Value::Null), at, at).is_none());
        for other in [
            "conflict",
            "incompatible",
            "aborted",
            "stale",
            "no_room",
            "host_fault",
        ] {
            assert!(
                Want::of(&record(other, env.clone()), at, at).is_none(),
                "{other}"
            );
        }
    }

    #[test]
    fn equal_inputs_compare_the_verdicts_and_unequal_ones_never_do() {
        let dir = Scratch::new("record");
        let out = dir.0.join("out");
        place(
            &out,
            "verdict",
            Some("rejected"),
            "| == setup\n| base cache: toolchain rustc r\n",
        );
        let primary = primary_inputs(&dir.0, None);
        let wanted = want(7, "accepted", primary.clone(), &dir.0);
        let record = record_of(&wanted, "bb", &primary, 0, 12, &out);
        assert_eq!(record["type"], "task.shadow_gated");
        assert_eq!(record["id"], 7);
        assert_eq!(record["result"], "verdict");
        assert_eq!(record["verdict"], "rejected");
        assert_eq!(record["primary"], "accepted");
        assert_eq!(record["unequal"], json!([]));
        assert_eq!(record["agrees"], json!(false));
        assert_eq!(
            record["env"],
            json!({"host": "runner-host", "toolchain": "rustc r", "runner": "bb"})
        );
        assert_eq!(record["primary_env"]["toolchain"], "rustc p");
        assert_eq!(record["base"], BASE);
        assert_eq!(record["seconds"], 12);

        let mut other = primary.clone();
        other["scenarios_sha256"] = json!("another set");
        let record = record_of(&wanted, "bb", &other, 0, 12, &out);
        assert_eq!(record["unequal"], json!(["scenarios_sha256"]));
        assert_eq!(record["agrees"], Value::Null);

        let agreeing = want(7, "rejected", primary.clone(), &dir.0);
        assert_eq!(
            record_of(&agreeing, "bb", &primary, 0, 1, &out)["agrees"],
            json!(true)
        );
    }

    #[test]
    fn a_refusal_or_a_missing_verdict_is_no_comparison() {
        let dir = Scratch::new("refused");
        let out = dir.0.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let primary = primary_inputs(&dir.0, None);
        let wanted = want(7, "accepted", primary.clone(), &dir.0);
        let refused = record_of(&wanted, "bb", &primary, 3, 1, &out);
        assert_eq!(refused["result"], "refused");
        assert_eq!(refused["verdict"], Value::Null);
        assert_eq!(refused["agrees"], Value::Null);
        assert_eq!(refused["env"]["toolchain"], Value::Null);
        assert_eq!(
            record_of(&wanted, "bb", &primary, 2, 1, &out)["result"],
            Value::Null
        );
        place(&out, "transport", None, "");
        let transport = record_of(&wanted, "bb", &primary, 2, 1, &out);
        assert_eq!(transport["result"], "transport");
        assert_eq!(transport["agrees"], Value::Null);
    }

    /// A controller with a fake dispatcher that notes its arguments and the
    /// set it got, holds while `hold` exists, and places a verdict.
    fn controller(dir: &Path, verdict: &str) -> PathBuf {
        let ctl = dir.join("ctl");
        std::fs::create_dir_all(ctl.join("rsi/scenarios")).unwrap();
        std::fs::create_dir_all(ctl.join("deploy")).unwrap();
        std::fs::write(ctl.join("rsi/scenarios/check-ok.sh"), "exit 0\n").unwrap();
        std::fs::write(
            ctl.join(".controller"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        let notes = dir.join("notes");
        let hold = dir.join("hold");
        std::fs::write(
            ctl.join("deploy/gate-runner.sh"),
            format!(
                "#!/usr/bin/env bash\n\
                 printf '%s\\n' \"$@\" >'{notes}'\n\
                 while [ -e '{hold}' ]; do sleep 0.05; done\n\
                 job=\"$6/$(printf %s \"$4\" | cut -c1-12)-x\"\n\
                 mkdir -p \"$job\"\n\
                 printf '{{\"result\":\"verdict\"}}\\n' >\"$job/record.json\"\n\
                 printf '{{\"verdict\":\"{verdict}\"}}\\n' >\"$job/verdict.json\"\n\
                 printf '| base cache: toolchain runner-tc\\n' >\"$job/gate.log\"\n\
                 exit 0\n",
                notes = notes.display(),
                hold = hold.display(),
            ),
        )
        .unwrap();
        ctl
    }

    fn wait_ended(shadows: &mut Shadows) -> super::Ended {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(ended) = shadows.poll() {
                return ended;
            }
            assert!(Instant::now() < deadline, "the shadow never ended");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn one_shadow_at_a_time_runs_the_dispatcher_on_a_snapshot_and_records_its_end() {
        let dir = Scratch::new("run");
        let ctl = controller(&dir.0, "accepted");
        let scenarios = ctl.join("rsi/scenarios");
        let marker = Some("0123456789abcdef0123456789abcdef01234567");
        std::fs::write(dir.0.join("hold"), "").unwrap();
        let mut shadows = Shadows::new(
            Settings {
                runner: "bb".into(),
                every: 1,
            },
            dir.0.join("shadow"),
        );
        shadows.offer(want(
            1,
            "accepted",
            primary_inputs(&scenarios, marker),
            &scenarios,
        ));
        assert_eq!(shadows.running_task(), Some(1));
        assert!(dir.0.join("shadow").join(PID_FILE).is_file());
        // Skipped while one runs, never queued.
        shadows.offer(want(
            2,
            "accepted",
            primary_inputs(&scenarios, marker),
            &scenarios,
        ));
        assert_eq!(shadows.running_task(), Some(1));
        // The gate directory changes after the snapshot: the runner got the
        // set as it was when the shadow started.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !dir.0.join("notes").exists() {
            assert!(Instant::now() < deadline, "the dispatcher never ran");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::write(scenarios.join("check-later.sh"), "exit 0\n").unwrap();
        assert!(shadows.poll().is_none());
        std::fs::remove_file(dir.0.join("hold")).unwrap();
        let ended = wait_ended(&mut shadows);
        let notes = std::fs::read_to_string(dir.0.join("notes")).unwrap();
        let arguments: Vec<&str> = notes.lines().collect();
        assert_eq!(arguments[0], "bb");
        assert_eq!(arguments[1], "/nonexistent/repo.git");
        assert_eq!(arguments[2], BASE);
        assert_eq!(arguments[3], CANDIDATE);
        assert_eq!(Path::new(arguments[4]), ended.work.join("scenarios"));
        assert_eq!(ended.record["id"], 1);
        assert_eq!(ended.record["agrees"], json!(true));
        assert_eq!(ended.record["unequal"], json!([]));
        assert_eq!(ended.record["inputs"]["controller"], json!(marker));
        assert_eq!(ended.record["env"]["toolchain"], "runner-tc");
        assert!(!dir.0.join("shadow").join(PID_FILE).exists());
        super::remove_tree(&ended.work);
        // The next verdict is shadowed again, and sees the new set.
        shadows.offer(want(
            3,
            "accepted",
            primary_inputs(&dir.0, marker),
            &scenarios,
        ));
        let ended = wait_ended(&mut shadows);
        assert_eq!(ended.record["unequal"], json!(["scenarios_sha256"]));
        assert_eq!(ended.record["agrees"], Value::Null);
        assert_eq!(
            ended.record["inputs"]["scenarios_sha256"],
            json!(super::super::inputs::scenarios_sha256(&scenarios))
        );
    }

    #[test]
    fn every_n_th_verdict_is_shadowed_and_a_controller_without_a_dispatcher_none() {
        let dir = Scratch::new("every");
        let ctl = controller(&dir.0, "rejected");
        let scenarios = ctl.join("rsi/scenarios");
        let mut shadows = Shadows::new(
            Settings {
                runner: "bb".into(),
                every: 2,
            },
            dir.0.join("shadow"),
        );
        shadows.offer(want(
            1,
            "accepted",
            primary_inputs(&scenarios, None),
            &scenarios,
        ));
        assert_eq!(shadows.running_task(), None);
        shadows.offer(want(
            2,
            "accepted",
            primary_inputs(&scenarios, None),
            &scenarios,
        ));
        assert_eq!(shadows.running_task(), Some(2));
        let ended = wait_ended(&mut shadows);
        // The primary's controller was unknown, the runner's is named.
        assert_eq!(ended.record["unequal"], json!(["controller"]));
        assert_eq!(ended.record["agrees"], Value::Null);
        std::fs::remove_file(ctl.join("deploy/gate-runner.sh")).unwrap();
        shadows.offer(want(
            3,
            "accepted",
            primary_inputs(&scenarios, None),
            &scenarios,
        ));
        shadows.offer(want(
            4,
            "accepted",
            primary_inputs(&scenarios, None),
            &scenarios,
        ));
        assert_eq!(shadows.running_task(), None);
    }

    #[test]
    fn a_stopped_shadow_is_gone_and_records_nothing() {
        let dir = Scratch::new("stop");
        let ctl = controller(&dir.0, "accepted");
        let scenarios = ctl.join("rsi/scenarios");
        std::fs::write(dir.0.join("hold"), "").unwrap();
        let mut shadows = Shadows::new(
            Settings {
                runner: "bb".into(),
                every: 1,
            },
            dir.0.join("shadow"),
        );
        shadows.offer(want(
            1,
            "accepted",
            primary_inputs(&scenarios, None),
            &scenarios,
        ));
        assert_eq!(shadows.running_task(), Some(1));
        shadows.stop("the test");
        assert_eq!(shadows.running_task(), None);
        assert!(shadows.poll().is_none());
        let left: Vec<_> = std::fs::read_dir(dir.0.join("shadow"))
            .unwrap()
            .flatten()
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn the_node_source_is_its_own_switch_and_leaves_the_runners_alone() {
        let env = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect();
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        let dirs = [
            ("SPAWN_NODE_ARTIFACTS", "/srv/artifacts"),
            ("SPAWN_NODE_RESULTS", "/srv/results"),
            ("HOME", "/home/u"),
        ];
        assert_eq!(super::node_settings(env(&dirs)).unwrap(), None);
        for off in ["", "0", " 0 "] {
            let mut all = dirs.to_vec();
            all.push((super::NODE_ENV, off));
            assert_eq!(super::node_settings(env(&all)).unwrap(), None, "{off:?}");
        }
        let mut on = dirs.to_vec();
        on.push((super::NODE_ENV, "1"));
        on.push((super::EVERY_ENV, "3"));
        on.push(("SPAWN_REMOTE_QUEUE_WAIT_SECS", "7"));
        let settings = super::node_settings(env(&on)).unwrap().unwrap();
        assert_eq!(settings.every, 3);
        assert_eq!(settings.artifacts, PathBuf::from("/srv/artifacts"));
        assert_eq!(settings.results, PathBuf::from("/srv/results"));
        assert_eq!(settings.cargo_home, PathBuf::from("/home/u/.cargo"));
        assert_eq!(settings.queue_wait, Duration::from_secs(7));
        let mut cargo = on.clone();
        cargo.push(("CARGO_HOME", "/opt/cargo"));
        assert_eq!(
            super::node_settings(env(&cargo))
                .unwrap()
                .unwrap()
                .cargo_home,
            PathBuf::from("/opt/cargo")
        );
        // Refused: another value, no directories, a relative one, a bad EVERY.
        assert!(super::node_settings(env(&[(super::NODE_ENV, "yes")])).is_err());
        assert!(super::node_settings(env(&[(super::NODE_ENV, "1")])).is_err());
        let mut relative = vec![(super::NODE_ENV, "1"), ("SPAWN_NODE_ARTIFACTS", "a")];
        relative.push(("SPAWN_NODE_RESULTS", "/r"));
        assert!(super::node_settings(env(&relative)).is_err());
        let mut every = on.clone();
        every.retain(|(key, _)| *key != super::EVERY_ENV);
        every.push((super::EVERY_ENV, "0"));
        assert!(super::node_settings(env(&every)).is_err());
        // The runner's settings read nothing of the node source's.
        assert_eq!(settings_of(None, Some("3")), None);
        assert_eq!(settings_of(Some("bb"), Some("3")).unwrap().every, 3);
    }

    fn settings_of(runner: Option<&str>, every: Option<&str>) -> Option<Settings> {
        settings(runner, every).unwrap()
    }

    /// A world for the node source: a controller (with its fake dispatcher
    /// for the runner), a source with a base and a candidate, a cargo
    /// home, a results directory and a ledger.
    struct NodeWorld {
        dir: Scratch,
        repo: PathBuf,
        base: String,
        candidate: String,
        scenarios: PathBuf,
        ctl: PathBuf,
        primary: Value,
        ledger: super::super::remote::tests::Ledger,
    }

    impl NodeWorld {
        fn new(case: &str) -> Self {
            use super::super::remote::tests as remote;
            let dir = Scratch::new(case);
            let ctl = controller(&dir.0, "accepted");
            std::fs::create_dir_all(ctl.join("scripts")).unwrap();
            std::fs::write(ctl.join("scripts/gate.sh"), "exit 0\n").unwrap();
            std::fs::create_dir_all(ctl.join("web")).unwrap();
            std::fs::write(
                ctl.join("web/package-lock.json"),
                "{\"lockfileVersion\":3}\n",
            )
            .unwrap();
            let scenarios = ctl.join("rsi/scenarios");
            let (repo, base, candidate) = remote::repository(&dir.0);
            let cargo_home = dir.0.join("cargo");
            std::fs::create_dir_all(cargo_home.join("registry/index")).unwrap();
            std::fs::write(cargo_home.join("registry/index/config.json"), "{}").unwrap();
            let primary = json!({"base": base, "candidate": candidate, "controller": MARKER,
                                 "scenarios_sha256": super::inputs::scenarios_sha256(&scenarios),
                                 "scenario": null});
            Self {
                ledger: remote::Ledger::new(&format!("shadow_{}", case.replace('-', "_"))),
                dir,
                repo,
                base,
                candidate,
                scenarios,
                ctl,
                primary,
            }
        }

        fn want(&self, task: i64) -> Want {
            Want {
                task,
                revision: json!(1),
                verdict: "accepted".to_string(),
                inputs: self.primary.clone(),
                env: json!({"host": "primary", "toolchain": "rustc p", "runner": null}),
                repo: self.repo.clone(),
                scenarios: self.scenarios.clone(),
                judging_id: Some(JUDGING.to_string()),
            }
        }

        /// A real task (the record's writer refuses one it does not hold).
        fn task(&self) -> i64 {
            let source = self.repo.to_string_lossy().to_string();
            let mission = self
                .ledger
                .store
                .mission_add("gated", &json!([source]), "human")
                .expect("a mission");
            self.ledger
                .store
                .task_assign(mission, "ephemeral", "worker", "brief", "default")
                .expect("a task")
        }

        fn results(&self) -> PathBuf {
            self.dir.0.join("results")
        }

        /// The node source of an integrator (a restarted one is another).
        fn nodes(&self) -> super::NodeShadows {
            super::NodeShadows::new(
                super::NodeSettings {
                    every: 1,
                    artifacts: self.dir.0.join("artifacts"),
                    results: self.results(),
                    cargo_home: self.dir.0.join("cargo"),
                    queue_wait: Duration::from_mins(1),
                },
                vec![
                    ("PATH".to_string(), "/usr/bin".to_string()),
                    ("SPAWN_GATE_JOBS".to_string(), "8".to_string()),
                    (
                        "SPAWN_TEST_DATABASE_URL".to_string(),
                        ["postgres", "://", "gate@db.example/t"].concat(),
                    ),
                ],
                self.dir.0.join("node-work"),
            )
        }

        fn artifact(&self, digest: &spawn_node_proto::ContentDigest) -> PathBuf {
            self.dir
                .0
                .join("artifacts/sha256")
                .join(&digest.hex[..2])
                .join(&digest.hex)
        }

        /// The inputs a job granted, as a node says it judged them.
        fn given(row: &spawn_postgres::RemoteJob) -> Value {
            let mut given = row.spec["inputs"].clone();
            given.as_object_mut().unwrap().remove("repo");
            given
        }

        /// `node` is granted `job_id` and stores a result of `exit`.
        fn run(&self, node: &str, job_id: &str, exit: i64, verdict: Option<&str>) {
            use super::super::remote::tests as remote;
            let grants = self.ledger.beat(node);
            assert_eq!(grants.len(), 1, "{grants:?}");
            assert_eq!(grants[0].0, job_id);
            let row = self.ledger.store.remote_job(job_id).unwrap().unwrap();
            let (meta, log) = remote::meta(
                job_id,
                grants[0].1,
                Some(exit),
                verdict,
                "| base cache: toolchain node-tc\n| gate ok\n",
                "boom\n",
                Some(&Self::given(&row)),
            );
            self.ledger.complete(&self.results(), node, &meta, &log);
        }
    }

    const MARKER: &str = "0123456789abcdef0123456789abcdef01234567";
    const JUDGING: &str = "0123456789ab0123456789abcdef";

    /// What a records writer was given.
    struct Written(std::cell::RefCell<Vec<Value>>);
    impl Written {
        fn new() -> Self {
            Self(std::cell::RefCell::new(Vec::new()))
        }
        fn write(&self, record: &Value) {
            self.0.borrow_mut().push(record.clone());
        }
        fn all(&self) -> Vec<Value> {
            self.0.borrow().clone()
        }
    }

    /// The job a node source queued: its row, and every artifact it names
    /// written where the gateway serves it, the web lockfile included.
    fn queued_and_published(world: &NodeWorld, job_id: &str) -> spawn_postgres::RemoteJob {
        let row = world.ledger.store.remote_job(job_id).unwrap().unwrap();
        assert_eq!(row.kind, spawn_postgres::RemoteJobKind::GateShadow);
        assert_eq!(row.judging_id.as_deref(), Some(JUDGING));
        assert_eq!(row.base_commit.as_deref(), Some(world.base.as_str()));
        // The grant's environment: no node-owned variable, no database.
        assert_eq!(row.spec["env"]["SPAWN_GATE_JOBS"], "8");
        assert!(row.spec["env"].get("PATH").is_none());
        assert!(row.spec["env"].get("SPAWN_TEST_DATABASE_URL").is_none());
        assert_eq!(row.spec["inputs"]["controller"], MARKER);
        assert_eq!(row.spec["inputs"]["repo"], "source");
        // The grant's own `requires`, the row's too.
        assert_eq!(row.spec["requires"], row.requires);
        let roles: Vec<&str> = row.spec["artifact_urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|url| url["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            ["delta_bundle", "baseline_bundle", "controller_pack"]
        );
        for url in row.spec["artifact_urls"].as_array().unwrap() {
            let digest =
                spawn_node_proto::ContentDigest::parse(url["digest"].as_str().unwrap()).unwrap();
            digest
                .verify(&std::fs::read(world.artifact(&digest)).unwrap())
                .unwrap();
        }
        assert!(
            world
                .dir
                .0
                .join("artifacts/registry-manifest.json")
                .is_file()
        );
        assert_eq!(row.requires["pg"], "16");
        assert_eq!(row.requires["registry"].as_str().unwrap().len(), 64);
        assert_eq!(row.requires["toolchain"], json!(null));
        // The lockfile the node installs from, fetchable by its digest.
        let lock = row.requires["web_lock"].as_str().expect("a web_lock");
        let digest = spawn_node_proto::ContentDigest::parse(&format!("sha256:{lock}")).unwrap();
        assert_eq!(
            std::fs::read(world.artifact(&digest)).unwrap(),
            std::fs::read(world.ctl.join("web/package-lock.json")).unwrap()
        );
        row
    }

    /// M2 on both sources at once: one verdict offered to the runner (its
    /// fake dispatcher) and to the node source; the node source writes the
    /// artifacts, queues a `gate_shadow` job a node holding what it
    /// requires is granted, reads the node's stored result, and records the
    /// same `task.shadow_gated` the runner does (`runner` `node:<name>`).
    #[test]
    fn the_node_source_records_what_the_runner_records_beside_it() {
        let world = NodeWorld::new("node-source");
        let mut nodes = world.nodes();
        let mut runner = Shadows::new(
            Settings {
                runner: "bb".into(),
                every: 1,
            },
            world.dir.0.join("shadow"),
        );
        let task = world.task();
        runner.offer(world.want(task));
        nodes.offer(&world.ledger.store, world.want(task));
        let waiting = nodes.waiting();
        assert_eq!(waiting.len(), 1);
        let row = queued_and_published(&world, &waiting[0]);
        assert_eq!(row.task_id, Some(task));
        let node = world.ledger.enroll("n252", 9, &row.requires);
        let verdict =
            super::super::remote::tests::verdict_on("accepted", &world.base, &world.candidate);
        world.run(&node, &row.job_id, 0, Some(&verdict));
        let written = Written::new();
        nodes.poll(&world.ledger.store, &|record| {
            written.write(record);
            Ok(())
        });
        assert!(nodes.waiting().is_empty());
        let records = written.all();
        assert_eq!(records.len(), 1);
        let from_node = &records[0];
        assert_eq!(from_node["type"], "task.shadow_gated");
        assert_eq!(from_node["id"], task);
        assert_eq!(from_node["runner"], "node:n252");
        assert_eq!(from_node["result"], "verdict");
        assert_eq!(from_node["verdict"], "accepted");
        assert_eq!(from_node["primary"], "accepted");
        assert_eq!(from_node["exit"], 0);
        assert_eq!(from_node["seconds"], 42);
        assert_eq!(from_node["unequal"], json!([]));
        assert_eq!(from_node["agrees"], json!(true));
        assert_eq!(from_node["inputs"], world.primary);
        assert_eq!(
            from_node["env"],
            json!({"host": "n252-host", "toolchain": "rustc 1.95.0 x86_64",
                   "runner": "node:n252"})
        );
        assert!(from_node.get("judging").is_none());
        // The runner's record of the same verdict: the same shape.
        let ended = wait_ended(&mut runner);
        let keys = |record: &Value| -> Vec<String> {
            let mut keys: Vec<String> = record.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(keys(&ended.record), keys(from_node));
        assert_eq!(ended.record["runner"], "bb");
        assert_eq!(ended.record["agrees"], json!(true));
        super::remove_tree(&ended.work);
        assert_eq!(world.ledger.strikes("n252"), (0, false));
        // The record the writer takes as it is.
        super::super::record(&world.ledger.store, from_node).expect("recorded");
        assert_eq!(world.ledger.records("task.shadow_gated").len(), 1);
    }

    /// The node source's `controller_pack` is cached under a key covering
    /// what it packs: the same controller is served from the cache, a
    /// changed `scripts/` (contents or executable bit) or web lockfile is
    /// packed afresh, never a stale pack.
    #[test]
    fn the_node_sources_controller_pack_follows_scripts_and_the_web_lock() {
        use std::os::unix::fs::PermissionsExt as _;
        let world = NodeWorld::new("node-pack-key");
        // Each offer from a restarted integrator (one shadow in flight
        // each, under the cap), all sharing the one artifacts cache.
        let pack = || -> String {
            let mut nodes = world.nodes();
            let task = world.task();
            nodes.offer(&world.ledger.store, world.want(task));
            let waiting = nodes.waiting();
            assert_eq!(waiting.len(), 1, "{waiting:?}");
            let row = world
                .ledger
                .store
                .remote_job(waiting.last().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(row.task_id, Some(task));
            row.spec["artifact_urls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|url| url["role"] == "controller_pack")
                .unwrap()["digest"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let gate = world.ctl.join("scripts/gate.sh");
        std::fs::set_permissions(&gate, std::fs::Permissions::from_mode(0o644)).unwrap();
        let first = pack();
        assert_eq!(pack(), first, "the same controller, the same pack");
        std::fs::write(&gate, "exit 1\n").unwrap();
        let edited = pack();
        assert_ne!(edited, first, "an edited gate.sh");
        std::fs::set_permissions(&gate, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = pack();
        assert!(
            ![&first, &edited].contains(&&executable),
            "an executable gate.sh"
        );
        std::fs::write(world.ctl.join("web/package-lock.json"), "{}\n").unwrap();
        let locked = pack();
        assert!(
            ![&first, &edited, &executable].contains(&&locked),
            "another web lock"
        );
        assert_eq!(pack(), locked);
    }

    /// A node shadow's valid verdict puts the node's strikes back to 0
    /// (`spawn_remote_job_valid`) and leaves `degraded` as it was.
    #[test]
    fn a_node_shadows_valid_verdict_resets_the_nodes_strikes() {
        let world = NodeWorld::new("node-valid");
        let mut nodes = world.nodes();
        let task = world.task();
        nodes.offer(&world.ledger.store, world.want(task));
        let job_id = nodes.waiting()[0].clone();
        let row = world.ledger.store.remote_job(&job_id).unwrap().unwrap();
        let node = world.ledger.enroll("n252", 9, &row.requires);
        let verdict =
            super::super::remote::tests::verdict_on("accepted", &world.base, &world.candidate);
        world.run(&node, &job_id, 0, Some(&verdict));
        world
            .ledger
            .admin()
            .execute(
                "update spawn_node_state set host_fault_strikes = 2, degraded = true
                  where name = 'n252'",
                &[],
            )
            .unwrap();
        let written = Written::new();
        nodes.poll(&world.ledger.store, &|record| {
            written.write(record);
            Ok(())
        });
        assert!(nodes.waiting().is_empty());
        let records = written.all();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["result"], "verdict");
        assert_eq!(world.ledger.strikes("n252"), (0, true));
        let row = world.ledger.store.remote_job(&job_id).unwrap().unwrap();
        assert_eq!(row.state, spawn_postgres::RemoteJobState::Completed);
        assert!(
            !world
                .ledger
                .store
                .remote_job_valid(&job_id, row.lease_epoch - 1)
                .unwrap(),
            "judged once"
        );
    }

    /// A node shadow's rejection naming only test-database errors (maybe
    /// the node's own test server crashed) resets no strikes, and leaves
    /// the job's epoch; it is still recorded.
    #[test]
    fn a_node_shadows_database_error_only_rejection_resets_no_strikes() {
        let world = NodeWorld::new("node-db-only");
        let mut nodes = world.nodes();
        let task = world.task();
        nodes.offer(&world.ledger.store, world.want(task));
        let job_id = nodes.waiting()[0].clone();
        let row = world.ledger.store.remote_job(&job_id).unwrap().unwrap();
        let node = world.ledger.enroll("n252", 9, &row.requires);
        let grants = world.ledger.beat(&node);
        assert_eq!(grants.len(), 1, "{grants:?}");
        let verdict = json!({"v": 1, "verdict": "rejected",
                             "reason": crate::integrate::host::CHECK_TESTS_REGRESSION,
                             "base": world.base, "candidate": world.candidate})
        .to_string();
        let (meta, log) = super::super::remote::tests::meta(
            &job_id,
            grants[0].1,
            Some(0),
            Some(&verdict),
            "| == regression: check-tests.sh must still pass\n\
             | FAILED api::tests::one: error connecting to server: Connection refused (os \
             error 111)\n",
            "",
            Some(&NodeWorld::given(&row)),
        );
        world.ledger.complete(&world.results(), &node, &meta, &log);
        world
            .ledger
            .admin()
            .execute(
                "update spawn_node_state set host_fault_strikes = 2 where name = 'n252'",
                &[],
            )
            .unwrap();
        let written = Written::new();
        nodes.poll(&world.ledger.store, &|record| {
            written.write(record);
            Ok(())
        });
        assert_eq!(written.all().len(), 1, "still recorded");
        assert_eq!(world.ledger.strikes("n252"), (2, false));
        let after = world.ledger.store.remote_job(&job_id).unwrap().unwrap();
        assert_eq!(after.lease_epoch, grants[0].1, "not judged");
    }

    /// A node's fault is struck on the ledger and the job queued again,
    /// nothing recorded for the task; one no node leases in time is taken
    /// back, nothing recorded; and every N-th verdict is shadowed.
    #[test]
    fn a_node_fault_is_struck_and_queued_again_never_recorded_for_the_task() {
        let world = NodeWorld::new("node-fault");
        let mut nodes = world.nodes();
        let written = Written::new();
        nodes.offer(&world.ledger.store, world.want(8));
        let job_id = nodes.waiting()[0].clone();
        let row = world.ledger.store.remote_job(&job_id).unwrap().unwrap();
        let node = world.ledger.enroll("n252", 9, &row.requires);
        world.run(&node, &job_id, 2, None);
        nodes.poll(&world.ledger.store, &|record| {
            written.write(record);
            Ok(())
        });
        assert_eq!(nodes.waiting(), vec![job_id.clone()]);
        assert!(written.all().is_empty());
        assert_eq!(world.ledger.strikes("n252"), (1, false));
        let row = world.ledger.store.remote_job(&job_id).unwrap().unwrap();
        assert_eq!(row.state, spawn_postgres::RemoteJobState::Queued);
        assert_eq!(row.excluded_nodes, vec![node.clone()]);
        nodes.settings.queue_wait = Duration::ZERO;
        nodes.poll(&world.ledger.store, &|record| {
            written.write(record);
            Ok(())
        });
        assert!(nodes.waiting().is_empty());
        assert!(written.all().is_empty());
        assert_eq!(
            world
                .ledger
                .store
                .remote_job(&job_id)
                .unwrap()
                .unwrap()
                .state,
            spawn_postgres::RemoteJobState::FallbackLocal
        );
        assert!(world.ledger.records("task.gated").is_empty());

        // Every N-th: with every 2, the first of two is skipped.
        nodes.settings.every = 2;
        nodes.offered = 0;
        nodes.offer(&world.ledger.store, world.want(9));
        assert!(nodes.waiting().is_empty());
        nodes.offer(&world.ledger.store, world.want(10));
        assert_eq!(nodes.waiting().len(), 1);
    }

    /// A restarted integrator takes back the shadows its predecessor
    /// queued (nobody waits for them any more), and keeps its own.
    #[test]
    fn a_restarted_integrator_takes_back_the_shadows_it_no_longer_waits_for() {
        let world = NodeWorld::new("node-restart");
        let mut before = world.nodes();
        before.offer(&world.ledger.store, world.want(11));
        let orphan = before.waiting()[0].clone();
        drop(before);
        let mut after = world.nodes();
        after.offer(&world.ledger.store, world.want(12));
        let mine = after.waiting()[0].clone();
        let written = Written::new();
        after.poll(&world.ledger.store, &|record| {
            written.write(record);
            Ok(())
        });
        assert_eq!(
            world
                .ledger
                .store
                .remote_job(&orphan)
                .unwrap()
                .unwrap()
                .state,
            spawn_postgres::RemoteJobState::FallbackLocal
        );
        assert_eq!(
            world.ledger.store.remote_job(&mine).unwrap().unwrap().state,
            spawn_postgres::RemoteJobState::Queued
        );
        assert_eq!(after.waiting(), vec![mine.clone()]);
        let queued = world
            .ledger
            .store
            .remote_jobs_queued(spawn_postgres::RemoteJobKind::GateShadow)
            .unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|job| job.job_id.as_str())
                .collect::<Vec<_>>(),
            [mine.as_str()]
        );
        assert!(written.all().is_empty());
    }

    /// A lease fenced back to the queue gets a whole wait again: the job's
    /// wait restarts each time it is seen leased.
    #[test]
    fn a_lease_fenced_back_to_the_queue_gets_a_whole_wait() {
        let world = NodeWorld::new("node-fenced");
        let mut nodes = world.nodes();
        nodes.settings.queue_wait = Duration::from_millis(800);
        nodes.offer(&world.ledger.store, world.want(13));
        let job_id = nodes.waiting()[0].clone();
        let row = world.ledger.store.remote_job(&job_id).unwrap().unwrap();
        let node = world.ledger.enroll("n252", 9, &row.requires);
        assert_eq!(world.ledger.beat(&node).len(), 1);
        std::thread::sleep(Duration::from_millis(900));
        let written = Written::new();
        nodes.poll(&world.ledger.store, &|record| {
            written.write(record);
            Ok(())
        });
        assert_eq!(nodes.waiting(), vec![job_id.clone()]);
        // Fenced back to the queue (as a new boot or the sweep does).
        world
            .ledger
            .admin()
            .execute(
                "update spawn_remote_jobs
                    set state = 'queued', lease_epoch = lease_epoch + 1, node_id = null,
                        boot_id = null, lease_expires_ms = null, grant_seq = null
                  where job_id = $1",
                &[&job_id],
            )
            .unwrap();
        nodes.poll(&world.ledger.store, &|record| {
            written.write(record);
            Ok(())
        });
        assert_eq!(nodes.waiting(), vec![job_id.clone()]);
        assert_eq!(
            world
                .ledger
                .store
                .remote_job(&job_id)
                .unwrap()
                .unwrap()
                .state,
            spawn_postgres::RemoteJobState::Queued
        );
    }

    /// A node that said no inputs (its fetch failed first): its record
    /// names an empty `inputs`, which the record's writer takes.
    #[test]
    fn a_node_that_said_no_inputs_is_recorded_with_empty_inputs() {
        use super::super::remote::tests as remote;
        let world = NodeWorld::new("node-no-inputs");
        let (meta, log) = remote::meta("gs-222222222222-abcdefgh", 1, Some(2), None, "", "", None);
        let stored = super::remote::Stored { meta, log };
        let judged = super::remote::judge_stored(&stored, &world.primary);
        let record = super::node_record_of(&world.want(world.task()), "node:n1", &stored, &judged);
        assert_eq!(record["inputs"], json!({}));
        assert_eq!(record["agrees"], Value::Null);
        assert!(!record["unequal"].as_array().unwrap().is_empty());
        super::super::record(&world.ledger.store, &record).expect("recorded");
        assert_eq!(world.ledger.records("task.shadow_gated").len(), 1);
    }

    /// A node's verdict on other inputs than the primary's is said, but
    /// compared with nothing; a node's fault on its last attempt is
    /// recorded with no verdict.
    #[test]
    fn a_node_record_compares_only_equal_inputs() {
        use super::super::remote::tests as remote;
        let dir = Scratch::new("node-record");
        let primary = primary_inputs(&dir.0, Some("3333333333333333333333333333333333333333"));
        let wanted = want(7, "rejected", primary.clone(), &dir.0);
        let (base, candidate) = ("5".repeat(40), "6".repeat(40));
        let mut granted = primary.clone();
        granted["base"] = json!(base);
        granted["candidate"] = json!(candidate);
        let verdict = remote::verdict_on("accepted", &base, &candidate);
        let (meta, log) = remote::meta(
            "gs-222222222222-abcdefgh",
            1,
            Some(0),
            Some(&verdict),
            "",
            "",
            Some(&granted),
        );
        let stored = super::remote::Stored { meta, log };
        let judged = super::remote::judge_stored(&stored, &granted);
        assert_eq!(judged.evidence, "accepted");
        let record = super::node_record_of(&wanted, "node:n1", &stored, &judged);
        assert_eq!(record["verdict"], "accepted");
        // The primary judged other commits.
        assert_eq!(record["unequal"], json!(["base", "candidate"]));
        assert_eq!(record["agrees"], Value::Null);
        let same = want(7, "rejected", granted.clone(), &dir.0);
        let record = super::node_record_of(&same, "node:n1", &stored, &judged);
        assert_eq!(record["unequal"], json!([]));
        assert_eq!(record["agrees"], json!(false));
        // A fault: no verdict, the evidence as its result.
        let (meta, log) = remote::meta(
            "gs-222222222222-abcdefgh",
            1,
            Some(6),
            None,
            "",
            "",
            Some(&granted),
        );
        let stored = super::remote::Stored { meta, log };
        let judged = super::remote::judge_stored(&stored, &granted);
        let record = super::node_record_of(&same, "node:n1", &stored, &judged);
        assert_eq!(record["result"], "aborted");
        assert_eq!(record["verdict"], Value::Null);
        assert_eq!(record["agrees"], Value::Null);
        assert_eq!(record["exit"], 6);
    }

    #[test]
    fn node_shadow_repacks_when_gate_script_changes_with_same_controller_commit() {
        let world = NodeWorld::new("node-repack");
        let gate = world.ctl.join("scripts/rsi/gate.sh");
        std::fs::create_dir_all(gate.parent().unwrap()).unwrap();
        std::fs::write(&gate, "#!/bin/sh\necho shadow-v1\nexit 0\n").unwrap();

        let pack_digest = |job_id: &str| -> (spawn_postgres::RemoteJob, String) {
            let row = world.ledger.store.remote_job(job_id).unwrap().unwrap();
            let digest = row.spec["artifact_urls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|url| url["role"] == "controller_pack")
                .expect("controller_pack")["digest"]
                .as_str()
                .unwrap()
                .to_string();
            (row, digest)
        };

        let mut nodes1 = world.nodes();
        let task1 = world.task();
        nodes1.offer(&world.ledger.store, world.want(task1));
        assert_eq!(nodes1.waiting().len(), 1);
        let (job1, pack1) = pack_digest(&nodes1.waiting()[0]);

        let updated_gate = "#!/bin/sh\necho shadow-v2\nexit 0\n";
        std::fs::write(&gate, updated_gate).unwrap();

        let mut nodes2 = world.nodes();
        let task2 = world.task();
        let mut want2 = world.want(task2);
        want2.judging_id = Some("fedcba9876543210fedcba987654".to_string());
        nodes2.offer(&world.ledger.store, want2);
        assert_eq!(nodes2.waiting().len(), 1);
        let (job2, pack2) = pack_digest(&nodes2.waiting()[0]);

        assert_eq!(
            job1.spec["inputs"]["controller"],
            job2.spec["inputs"]["controller"]
        );
        assert_eq!(
            job1.spec["inputs"]["scenarios_sha256"],
            job2.spec["inputs"]["scenarios_sha256"]
        );
        assert_ne!(
            pack1, pack2,
            "editing scripts/rsi/gate.sh must invalidate controller_pack cache for node shadow"
        );

        let parsed = spawn_node_proto::ContentDigest::parse(&pack2).unwrap();
        let extracted = std::process::Command::new("tar")
            .args(["-xOf"])
            .arg(world.artifact(&parsed))
            .arg("scripts/rsi/gate.sh")
            .output()
            .unwrap();
        assert!(extracted.status.success(), "{extracted:?}");
        assert_eq!(String::from_utf8_lossy(&extracted.stdout), updated_gate);
    }
}

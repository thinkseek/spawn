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

use super::inputs;
use super::warm::{exited_unreaped, setpriv_available, signal_group, start_ticks};

/// The runner's name for the transport; unset or empty: no shadow.
pub(super) const RUNNER_ENV: &str = "SPAWN_GATE_SHADOW_RUNNER";

/// Shadow every N-th verdict (a whole number from 1; default 1).
pub(super) const EVERY_ENV: &str = "SPAWN_GATE_SHADOW_EVERY";

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
pub(super) fn settings(runner: Option<&str>, every: Option<&str>) -> Result<Option<Settings>, String> {
    let Some(runner) = runner.map(str::trim).filter(|runner| !runner.is_empty()) else {
        return Ok(None);
    };
    let every = match every.map(str::trim).filter(|every| !every.is_empty()) {
        None => 1,
        Some(text) => text
            .parse::<u64>()
            .ok()
            .filter(|every| *every >= 1)
            .ok_or_else(|| format!("{EVERY_ENV} is a whole number from 1, not {text:?}"))?,
    };
    Ok(Some(Settings {
        runner: runner.to_string(),
        every,
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
}

impl Shadows {
    pub(super) fn new(settings: Settings, dir: PathBuf) -> Self {
        Self {
            settings,
            dir,
            offered: 0,
            started: 0,
            running: None,
        }
    }

    /// A verdict to shadow, on the main thread: started now, unless this is
    /// not an N-th verdict, a shadow runs already (skipped, never queued),
    /// or the controller has no dispatcher.
    pub(super) fn offer(&mut self, want: Want) {
        self.offered += 1;
        let task = want.task;
        if self.offered % self.settings.every != 0 {
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
        let base = want.inputs["base"].as_str().unwrap_or_default().to_string();
        let candidate = want.inputs["candidate"].as_str().unwrap_or_default().to_string();
        self.started += 1;
        let work = self
            .dir
            .join(format!("{}-{}-{}", want.task, std::process::id(), self.started));
        remove_tree(&work);
        std::fs::create_dir_all(&work).map_err(|error| format!("{}: {error}", work.display()))?;
        let started = (|| {
            // The controller, read as the shadow starts; the set, a snapshot
            // read once: what the dispatcher packs is this copy.
            let controller_commit = inputs::controller_commit(&controller);
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
            let stdout = std::fs::File::create(&log)
                .map_err(|error| format!("{}: {error}", log.display()))?;
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
                .arg(&dispatcher)
                .arg(&self.settings.runner)
                .arg(&want.repo)
                .arg(&base)
                .arg(&candidate)
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
        })();
        let (child, given) = match started {
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
            .map_or_else(|| status.signal().map_or(-1, |signal| 128 + signal), |code| code);
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
    let placed = job.as_ref().and_then(|job| read_json(&job.join("record.json")));
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
    let unequal = inputs::unequal(given, &want.inputs);
    let agrees = match &verdict {
        Some(verdict) if unequal.is_empty() && matches!(want.verdict.as_str(), "accepted" | "rejected") => {
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
        }
    }

    /// A placed job: `<out>/<job>/{record.json,verdict.json,gate.log}`.
    fn place(out: &Path, result: &str, verdict: Option<&str>, log: &str) {
        let job = out.join("222222222222-abc");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(
            job.join("record.json"),
            format!("{}\n", json!({"job": "222222222222-abc", "host": "runner-host", "result": result})),
        )
        .unwrap();
        if let Some(verdict) = verdict {
            std::fs::write(job.join("verdict.json"), format!("{}\n", json!({"verdict": verdict}))).unwrap();
        }
        std::fs::write(job.join("gate.log"), log).unwrap();
    }

    #[test]
    fn the_settings_need_a_runner_and_every_is_a_whole_number() {
        assert_eq!(settings(None, Some("2")).unwrap(), None);
        assert_eq!(settings(Some(" "), None).unwrap(), None);
        assert_eq!(
            settings(Some("bb-runner"), None).unwrap(),
            Some(Settings { runner: "bb-runner".into(), every: 1 })
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
        for other in ["conflict", "incompatible", "aborted", "stale", "no_room", "host_fault"] {
            assert!(Want::of(&record(other, env.clone()), at, at).is_none(), "{other}");
        }
    }

    #[test]
    fn equal_inputs_compare_the_verdicts_and_unequal_ones_never_do() {
        let dir = Scratch::new("record");
        let out = dir.0.join("out");
        place(&out, "verdict", Some("rejected"), "| == setup\n| base cache: toolchain rustc r\n");
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
        assert_eq!(record["env"], json!({"host": "runner-host", "toolchain": "rustc r", "runner": "bb"}));
        assert_eq!(record["primary_env"]["toolchain"], "rustc p");
        assert_eq!(record["base"], BASE);
        assert_eq!(record["seconds"], 12);

        let mut other = primary.clone();
        other["scenarios_sha256"] = json!("another set");
        let record = record_of(&wanted, "bb", &other, 0, 12, &out);
        assert_eq!(record["unequal"], json!(["scenarios_sha256"]));
        assert_eq!(record["agrees"], Value::Null);

        let agreeing = want(7, "rejected", primary.clone(), &dir.0);
        assert_eq!(record_of(&agreeing, "bb", &primary, 0, 1, &out)["agrees"], json!(true));
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
        assert_eq!(record_of(&wanted, "bb", &primary, 2, 1, &out)["result"], Value::Null);
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
        std::fs::write(ctl.join(".controller"), "0123456789abcdef0123456789abcdef01234567\n").unwrap();
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
            Settings { runner: "bb".into(), every: 1 },
            dir.0.join("shadow"),
        );
        shadows.offer(want(1, "accepted", primary_inputs(&scenarios, marker), &scenarios));
        assert_eq!(shadows.running_task(), Some(1));
        assert!(dir.0.join("shadow").join(PID_FILE).is_file());
        // Skipped while one runs, never queued.
        shadows.offer(want(2, "accepted", primary_inputs(&scenarios, marker), &scenarios));
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
        shadows.offer(want(3, "accepted", primary_inputs(&dir.0, marker), &scenarios));
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
            Settings { runner: "bb".into(), every: 2 },
            dir.0.join("shadow"),
        );
        shadows.offer(want(1, "accepted", primary_inputs(&scenarios, None), &scenarios));
        assert_eq!(shadows.running_task(), None);
        shadows.offer(want(2, "accepted", primary_inputs(&scenarios, None), &scenarios));
        assert_eq!(shadows.running_task(), Some(2));
        let ended = wait_ended(&mut shadows);
        // The primary's controller was unknown, the runner's is named.
        assert_eq!(ended.record["unequal"], json!(["controller"]));
        assert_eq!(ended.record["agrees"], Value::Null);
        std::fs::remove_file(ctl.join("deploy/gate-runner.sh")).unwrap();
        shadows.offer(want(3, "accepted", primary_inputs(&scenarios, None), &scenarios));
        shadows.offer(want(4, "accepted", primary_inputs(&scenarios, None), &scenarios));
        assert_eq!(shadows.running_task(), None);
    }

    #[test]
    fn a_stopped_shadow_is_gone_and_records_nothing() {
        let dir = Scratch::new("stop");
        let ctl = controller(&dir.0, "accepted");
        let scenarios = ctl.join("rsi/scenarios");
        std::fs::write(dir.0.join("hold"), "").unwrap();
        let mut shadows = Shadows::new(
            Settings { runner: "bb".into(), every: 1 },
            dir.0.join("shadow"),
        );
        shadows.offer(want(1, "accepted", primary_inputs(&scenarios, None), &scenarios));
        assert_eq!(shadows.running_task(), Some(1));
        shadows.stop("the test");
        assert_eq!(shadows.running_task(), None);
        assert!(shadows.poll().is_none());
        let left: Vec<_> = std::fs::read_dir(dir.0.join("shadow")).unwrap().flatten().collect();
        assert!(left.is_empty(), "{left:?}");
    }
}

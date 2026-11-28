//! The landing-time base warm: after a task lands (`task.landed`) or the
//! owner moves main (`source.moved`), one `gate.sh --base-only` on the new
//! main, in the background, so the next gate on that main finds its base
//! results in the base cache instead of judging the base again
//! (docs/fleet-architecture.md §10.2, "Base only").
//!
//! A warm is never on a landing's path. [`Warms::want`] only notes the
//! commit, and the pass that follows starts it ([`Warms::poll`]) and never
//! waits for it. Its exit status decides nothing and is recorded nowhere
//! but in its log and one line of the integrator's output. It runs the
//! same gate script the integrator's gates run, so it takes the host's
//! scenario budget the same way, and gate.sh makes a `--base-only` run a
//! low-priority claimant with one job: at most one host scenario slot that
//! no gate is waiting for, and one compiler job.
//!
//! One warm at a time, and one wanted per source, kept until it runs. A
//! warm is abandoned only when its own source's main has moved past it (its
//! process group gets TERM, then KILL after [`ABANDON_GRACE`]); the entries
//! it finished stay valid for the commit they name, and the new commit gets
//! its own warm. A landing on another source never cuts a warm off: that
//! source's warm waits, kept, until the running one ends, and the wanted
//! ones start in the order they were wanted (cc-astra A33: a Flake Doctor
//! landing abandoned a still-current Spawn warm as "main moved on"). A
//! commit warmed once is not warmed again by the same integrator.
//!
//! It cannot outlive the integrator unaccounted for:
//! - the gate runs under `setpriv --pdeathsig TERM`, started from the
//!   integrator's main thread (the parent-death signal follows the parent
//!   *thread*). An integrator that dies by any means TERMs its warm, whose
//!   trap stops its children and removes its scratch;
//! - it stays in the integrator's process tree and cgroup (its own process
//!   group, never a new session), so stopping the integrator's unit stops
//!   it too;
//! - its pid, start time, commit, source and scenarios are in
//!   [`PID_FILE`]. A starting integrator ([`Warms::take_over`]) stops the
//!   warm that file names only when the process is this user's, started at
//!   the recorded time (a pid used again never matches), and runs
//!   `gate.sh --base-only` of the recorded commit. Anything else signals
//!   nothing.
//!
//! A restart cuts the warm in flight off (the parent-death signal, or the
//! take-over). The next integrator finishes the work: the warm its pid file
//! names, gone or stopped, is kept as lost ([`Taken::Again`]), and at its
//! first passes, once the pass has applied its halts, it wants the base of
//! that warm's source's main as it is then, with that warm's scenarios
//! ([`Warms::want_lost`]): the lost commit when main has not moved, main's
//! new commit when it has, and nothing when every mission on the source is
//! halted. It is wanted like any warm: no unit, no detached process. A pid
//! file that names no source (an older integrator's) warms nothing again.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long an abandoned warm has after TERM before its group is killed.
pub(super) const ABANDON_GRACE: Duration = Duration::from_secs(10);

/// The warm's pid file, in the warm directory.
pub(super) const PID_FILE: &str = "warm.pid";

/// How many days a warm's log is kept.
const LOG_DAYS: u64 = 7;

/// A commit to warm: the source, its new main, and the scenarios directory
/// the gates of its missions judge with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Want {
    pub(super) source: String,
    pub(super) commit: String,
    pub(super) scenarios: PathBuf,
}

/// The warm in flight.
struct Warm {
    want: Want,
    child: Child,
    started: Instant,
    log: PathBuf,
}

/// The integrator's warms: the gate and its environment, where the logs
/// and the pid file go, the commit wanted, the warm running and the last
/// one finished.
pub(super) struct Warms {
    gate: PathBuf,
    env: Vec<(String, String)>,
    dir: PathBuf,
    /// At most one per source, in the order they were first wanted.
    wanted: Vec<Want>,
    running: Option<Warm>,
    /// The last warm finished, per source.
    done: Vec<Want>,
    lost: Option<Lost>,
}

/// How many passes may fail to read a lost warm's source's main before it
/// is given up.
const LOST_TRIES: u32 = 3;

/// A warm a predecessor's restart cut off, to warm again for its source's
/// main as it is when a pass wants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Lost {
    pub(super) source: String,
    pub(super) commit: String,
    pub(super) scenarios: PathBuf,
    tries: u32,
}

/// What [`Warms::take_over`] found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Taken {
    /// No pid file.
    Nothing,
    /// A pid file that is not a warm's: nothing stopped, nothing warmed.
    NotAWarm,
    /// A warm's pid file that names no source (an older integrator's):
    /// whatever it was, nothing warms it again.
    NoSource { commit: String },
    /// A warm a restart cut off (gone, or stopped by the take-over): its
    /// source's main is warmed again ([`Warms::want_lost`]).
    Again { lost: Lost, stopped: bool },
}

impl Warms {
    pub(super) fn new(gate: PathBuf, env: Vec<(String, String)>, dir: PathBuf) -> Self {
        Self {
            gate,
            env,
            dir,
            wanted: Vec::new(),
            running: None,
            done: Vec::new(),
            lost: None,
        }
    }

    /// The lost warm's source, while one waits to be warmed again.
    pub(super) fn lost_source(&self) -> Option<&str> {
        self.lost.as_ref().map(|lost| lost.source.as_str())
    }

    /// At a pass, after it applied its halts: want the warm a restart cut
    /// off again, for its source's main as it is now (`main` reads it).
    /// `live`: whether the source has a mission that is not halted; when
    /// not, it is warmed for no gate, and nothing is wanted. It waits while
    /// another warm is wanted or running (one at a time; that one is of a
    /// main this integrator saw land).
    pub(super) fn want_lost(&mut self, live: bool, main: &dyn Fn(&str) -> Result<String, String>) {
        let Some(lost) = self.lost.clone() else {
            return;
        };
        if !live {
            self.lost = None;
            println!(
                "integrate: the base warm of {} a previous integrator started was cut off: \
                 every mission on {} is halted, so it is not warmed again",
                lost.commit, lost.source
            );
            return;
        }
        if !self.wanted.is_empty() || self.running.is_some() {
            return;
        }
        match main(&lost.source) {
            Ok(commit) => {
                self.lost = None;
                println!(
                    "integrate: the base warm of {} a previous integrator started was cut off: \
                     warming {}'s main {commit}{}",
                    lost.commit,
                    lost.source,
                    if commit == lost.commit {
                        ""
                    } else {
                        " (main has moved since)"
                    }
                );
                self.want(Want {
                    source: lost.source,
                    commit,
                    scenarios: lost.scenarios,
                });
            }
            Err(error) => {
                let tries = lost.tries + 1;
                if tries >= LOST_TRIES {
                    self.lost = None;
                    eprintln!(
                        "integrate: the base warm of {} a previous integrator started was cut \
                         off, but {}'s main cannot be read ({error}), {tries} times: not warmed \
                         again",
                        lost.commit, lost.source
                    );
                } else {
                    self.lost = Some(Lost { tries, ..lost });
                }
            }
        }
    }

    /// Main of `want.source` is now `want.commit`: warm it at a later
    /// [`Self::poll`]. A later want of the same source replaces an earlier
    /// one not yet started (in its place); another source's is kept.
    pub(super) fn want(&mut self, want: Want) {
        match self
            .wanted
            .iter_mut()
            .find(|wanted| wanted.source == want.source)
        {
            Some(wanted) => *wanted = want,
            None => self.wanted.push(want),
        }
    }

    /// Once per pass, on the main thread: say how a warm that ended went,
    /// abandon one its own source's main has moved past, and, with none
    /// running, start the first wanted one whose mirror has the commit
    /// (`mirror` answers, refreshing it when it may; None: not this pass).
    /// A want of another source than the running warm's waits for it.
    pub(super) fn poll(&mut self, mirror: &dyn Fn(&Want) -> Option<PathBuf>) {
        self.reap();
        let (done, running) = (&self.done, self.running.as_ref());
        self.wanted
            .retain(|want| !done.contains(want) && running.is_none_or(|warm| warm.want != *want));
        if let Some(warm) = &self.running {
            let source = warm.want.source.clone();
            if !self.wanted.iter().any(|want| want.source == source) {
                return;
            }
            self.abandon(&format!("{source}'s main moved on"));
        }
        let Some((at, repo)) = self
            .wanted
            .iter()
            .enumerate()
            .find_map(|(at, want)| mirror(want).map(|repo| (at, repo)))
        else {
            return;
        };
        let want = self.wanted.remove(at);
        match self.start(&want, &repo) {
            Ok(warm) => {
                println!(
                    "integrate: base warm of {} on {}: started (pid {}), log {}",
                    want.commit,
                    want.source,
                    warm.child.id(),
                    warm.log.display()
                );
                self.running = Some(warm);
            }
            Err(error) => {
                eprintln!(
                    "integrate: base warm of {} on {}: not started: {error}",
                    want.commit, want.source
                );
            }
        }
    }

    /// The warm that ended, said once.
    fn reap(&mut self) {
        let Some(warm) = self.running.as_mut() else {
            return;
        };
        match warm.child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                println!(
                    "integrate: base warm of {}: {status} after {} s (log {})",
                    warm.want.commit,
                    warm.started.elapsed().as_secs(),
                    warm.log.display()
                );
                if let Some(warm) = self.running.take() {
                    self.done.retain(|done| done.source != warm.want.source);
                    self.done.push(warm.want);
                }
                let _ = std::fs::remove_file(self.dir.join(PID_FILE));
            }
            Err(error) => {
                eprintln!(
                    "integrate: base warm of {}: cannot wait for it: {error}",
                    warm.want.commit
                );
                self.abandon("cannot wait for it");
            }
        }
    }

    /// Stop the warm in flight: TERM to its group, KILL after the grace.
    pub(super) fn abandon(&mut self, why: &str) {
        let Some(mut warm) = self.running.take() else {
            return;
        };
        let group = warm.child.id();
        signal_group(group, "TERM");
        let deadline = Instant::now() + ABANDON_GRACE;
        // Waited for without reaping it: an exited leader stays a zombie
        // until `wait`, and while it does its pid, the group's id, cannot be
        // given to another process, so the KILL below reaches this group's
        // leftovers (the gate's own children) and nothing else.
        while !exited_unreaped(group) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        signal_group(group, "KILL");
        let _ = warm.child.wait();
        let _ = std::fs::remove_file(self.dir.join(PID_FILE));
        println!(
            "integrate: base warm of {} abandoned after {} s: {why}; the results it \
             finished stay in the base cache",
            warm.want.commit,
            warm.started.elapsed().as_secs()
        );
    }

    fn start(&self, want: &Want, repo: &Path) -> Result<Warm, String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("{}: {error}", self.dir.display()))?;
        clean_logs(&self.dir, LOG_DAYS);
        let log = self.dir.join(format!("{}.log", want.commit));
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|error| format!("{}: {error}", log.display()))?;
        let err = out
            .try_clone()
            .map_err(|error| format!("{}: {error}", log.display()))?;
        let mut command = if setpriv_available() {
            let mut command = Command::new("setpriv");
            command.args(["--pdeathsig", "TERM", "sh"]);
            command
        } else {
            Command::new("sh")
        };
        command
            .arg(&self.gate)
            .arg("--base-only")
            .arg(repo)
            .arg(&want.commit)
            .arg(&want.scenarios)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0);
        let child = command
            .spawn()
            .map_err(|error| format!("cannot run {}: {error}", self.gate.display()))?;
        let started = Instant::now();
        let pid = child.id();
        match start_ticks(pid) {
            Some(ticks) => {
                let record = record(pid, ticks, want);
                if let Err(error) = std::fs::write(self.dir.join(PID_FILE), record) {
                    eprintln!(
                        "integrate: base warm of {}: {}: {error}; a crash of this integrator \
                         leaves it to its parent-death signal",
                        want.commit,
                        self.dir.join(PID_FILE).display()
                    );
                }
            }
            None => eprintln!(
                "integrate: base warm of {}: cannot read its start time; no pid file",
                want.commit
            ),
        }
        Ok(Warm {
            want: want.clone(),
            child,
            started,
            log,
        })
    }

    /// At startup: stop the warm a predecessor left, when [`PID_FILE`]
    /// names one that is still that warm (see the module's docs), and
    /// remove the file in any case. A warm it names with its source, gone
    /// or stopped here, is kept to be warmed again ([`Self::want_lost`]).
    pub(super) fn take_over(&mut self) -> Taken {
        let path = self.dir.join(PID_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Taken::Nothing;
        };
        let _ = std::fs::remove_file(&path);
        let Some(left) = Left::parse(&text) else {
            eprintln!(
                "integrate: {}: not a warm's pid file; nothing stopped",
                path.display()
            );
            return Taken::NotAWarm;
        };
        let stopped = match left.still_running(&self.gate) {
            Ok(()) => {
                signal_group(left.pid, "TERM");
                // As abandon does: KILL after the grace, but only while the
                // pid is still that warm (it is not this process's child, so
                // nothing keeps its pid from being reused once it is gone).
                let deadline = Instant::now() + ABANDON_GRACE;
                let mut alive = true;
                while Instant::now() < deadline {
                    if left.still_running(&self.gate).is_err() {
                        alive = false;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if alive && left.still_running(&self.gate).is_ok() {
                    signal_group(left.pid, "KILL");
                }
                println!(
                    "integrate: stopped the base warm of {} (pid {}) a previous integrator \
                     started{}",
                    left.commit,
                    left.pid,
                    if alive {
                        " (killed: it outlived TERM)"
                    } else {
                        ""
                    }
                );
                true
            }
            Err(why) => {
                println!(
                    "integrate: the base warm of {} (pid {}) a previous integrator started is \
                     gone ({why}); nothing stopped",
                    left.commit, left.pid
                );
                false
            }
        };
        let (Some(source), Some(scenarios)) = (left.source, left.scenarios) else {
            println!(
                "integrate: the base warm of {} a previous integrator started was cut off, but \
                 its pid file names no source (an older integrator's): nothing is warmed again",
                left.commit
            );
            return Taken::NoSource {
                commit: left.commit,
            };
        };
        println!(
            "integrate: the base warm of {} on {source} a previous integrator started was cut \
             off: its source's main is warmed again at the first passes",
            left.commit
        );
        let lost = Lost {
            source,
            commit: left.commit,
            scenarios,
            tries: 0,
        };
        self.lost = Some(lost.clone());
        Taken::Again { lost, stopped }
    }

    /// The warm in flight, for tests.
    #[cfg(test)]
    pub(super) fn running_pid(&self) -> Option<u32> {
        self.running.as_ref().map(|warm| warm.child.id())
    }
}

/// The pid file's text for the warm of `want`, `pid` started at `ticks`.
/// A source or scenarios a line cannot hold is left out: that warm is not
/// warmed again after a restart.
fn record(pid: u32, ticks: u64, want: &Want) -> String {
    let record = format!("pid {pid}\nstart {ticks}\ncommit {}\n", want.commit);
    let scenarios = want.scenarios.to_str().unwrap_or_default();
    let one_line = |text: &str| !text.is_empty() && !text.contains('\n') && text.trim() == text;
    if one_line(&want.source) && one_line(scenarios) {
        return format!("{record}source {}\nscenarios {scenarios}\n", want.source);
    }
    record
}

/// What a pid file says.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Left {
    pid: u32,
    start: u64,
    commit: String,
    source: Option<String>,
    scenarios: Option<PathBuf>,
}

impl Left {
    pub(super) fn parse(text: &str) -> Option<Self> {
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
                .map(str::trim)
        };
        let pid = field("pid")?.parse().ok().filter(|pid| *pid > 1)?;
        let start = field("start")?.parse().ok()?;
        let commit = field("commit")?;
        let source = field("source").filter(|source| !source.is_empty());
        let scenarios = field("scenarios").filter(|scenarios| !scenarios.is_empty());
        let (source, scenarios) = match (source, scenarios) {
            (Some(source), Some(scenarios)) => {
                (Some(source.to_string()), Some(PathBuf::from(scenarios)))
            }
            _ => (None, None),
        };
        (!commit.is_empty() && commit.chars().all(|c| c.is_ascii_hexdigit())).then(|| Self {
            pid,
            start,
            commit: commit.to_string(),
            source,
            scenarios,
        })
    }

    /// Ok when `pid` is still the warm the file names: this user's, started
    /// at the recorded time, running `sh <gate> --base-only <repo>
    /// <commit> …`. The reason otherwise.
    pub(super) fn still_running(&self, gate: &Path) -> Result<(), String> {
        let proc_dir = PathBuf::from(format!("/proc/{}", self.pid));
        let owner = std::fs::metadata(&proc_dir)
            .map_err(|_| "no such process".to_string())?
            .uid();
        let me = std::fs::metadata("/proc/self")
            .map_err(|error| error.to_string())?
            .uid();
        if owner != me {
            return Err(format!("pid {} is another user's", self.pid));
        }
        if start_ticks(self.pid) != Some(self.start) {
            return Err(format!("pid {} is another process now", self.pid));
        }
        let cmdline = std::fs::read(proc_dir.join("cmdline")).map_err(|error| error.to_string())?;
        let words: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
        let named = |word: &&[u8]| -> bool {
            std::str::from_utf8(word).is_ok_and(|word| {
                Path::new(word).file_name() == gate.file_name() && word.ends_with("gate.sh")
            })
        };
        let Some(at) = words.iter().position(named) else {
            return Err(format!("pid {} runs no gate.sh", self.pid));
        };
        let rest = &words[at + 1..];
        if rest.first() != Some(&b"--base-only".as_slice()) {
            return Err(format!("pid {} is a gate, not a warm", self.pid));
        }
        if rest.get(2) != Some(&self.commit.as_bytes()) {
            return Err(format!("pid {} warms another commit", self.pid));
        }
        Ok(())
    }
}

/// The process's start time in clock ticks since boot: field 22 of
/// `/proc/<pid>/stat`, read after the command name, which may hold spaces
/// and parentheses.
pub(super) fn start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat_start_ticks(&stat)
}

pub(super) fn stat_start_ticks(stat: &str) -> Option<u64> {
    let after = &stat[stat.rfind(')')? + 1..];
    // Fields from 3 (the state) on; the start time is the 22nd.
    after.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// Whether our child `pid` has exited and waits to be reaped (a zombie),
/// read from `/proc/<pid>/stat` without reaping it. Unreadable counts as
/// exited: a child not yet reaped always has the file.
pub(super) fn exited_unreaped(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    stat.rfind(')')
        .and_then(|end| stat[end + 1..].split_whitespace().next())
        .is_none_or(|state| state == "Z" || state == "X")
}

pub(super) fn signal_group(group: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{signal}"), "--", &format!("-{group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub(super) fn setpriv_available() -> bool {
    Command::new("setpriv")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Remove warm logs older than `days`.
fn clean_logs(dir: &Path, days: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let limit = Duration::from_secs(days * 24 * 3600);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "log") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::{Left, Lost, PID_FILE, Taken, Want, Warms, stat_start_ticks};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(case: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "spawn-warm-test-{case}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("scratch");
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fake gate.sh: writes its arguments, then sleeps `secs`.
    fn fake_gate(dir: &Path, secs: u32) -> PathBuf {
        let gate = dir.join("gate.sh");
        std::fs::write(
            &gate,
            // Written whole, then named: a test that sees the file reads
            // all of it.
            format!(
                "printf '%s\\n' \"$@\" >\"$WARM_ARGS.$3.tmp\"\n\
                 mv \"$WARM_ARGS.$3.tmp\" \"$WARM_ARGS.$3\"\nsleep {secs}\n"
            ),
        )
        .expect("gate");
        gate
    }

    fn warms(scratch: &Scratch, secs: u32) -> Warms {
        let gate = fake_gate(&scratch.0, secs);
        let env = vec![
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            (
                "WARM_ARGS".to_string(),
                scratch.0.join("args").display().to_string(),
            ),
        ];
        Warms::new(gate, env, scratch.0.join("warm"))
    }

    fn want(commit: &str) -> Want {
        Want {
            source: "/src/spawn".to_string(),
            commit: commit.to_string(),
            scenarios: PathBuf::from("/live/rsi/scenarios"),
        }
    }

    fn until(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z "))
    }

    /// A landing's commit is warmed in the background with `--base-only`
    /// on the mirror, the commit and the scenarios; the pass does not wait.
    #[test]
    fn a_wanted_commit_is_warmed_in_the_background_with_base_only() {
        let scratch = Scratch::new("start");
        let mut warms = warms(&scratch, 30);
        warms.want(want("abc123"));
        let begun = Instant::now();
        warms.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert!(begun.elapsed() < Duration::from_secs(5), "poll never waits");
        let pid = warms.running_pid().expect("a warm runs");
        let args = scratch.0.join("args.abc123");
        until("the gate's arguments", || args.exists());
        let said = std::fs::read_to_string(&args).expect("args");
        assert_eq!(
            said.lines().collect::<Vec<_>>(),
            [
                "--base-only",
                "/mirror.git",
                "abc123",
                "/live/rsi/scenarios"
            ]
        );
        let file = std::fs::read_to_string(scratch.0.join("warm").join(PID_FILE)).expect("pid");
        assert!(file.contains(&format!("pid {pid}\n")), "{file}");
        assert!(file.contains("commit abc123\n"), "{file}");
        assert!(file.contains("source /src/spawn\n"), "{file}");
        assert!(file.contains("scenarios /live/rsi/scenarios\n"), "{file}");
        warms.abandon("test over");
        assert!(!alive(pid));
    }

    /// Main moved again: the warm of the old commit is abandoned (its
    /// group gone) and the new commit warmed; a commit already warming is
    /// not started twice.
    #[test]
    fn a_warm_main_moved_past_is_abandoned_for_the_new_commit() {
        let scratch = Scratch::new("supersede");
        let mut warms = warms(&scratch, 30);
        warms.want(want("aaa111"));
        warms.poll(&|_| Some(PathBuf::from("/mirror.git")));
        let first = warms.running_pid().expect("first");
        warms.want(want("aaa111"));
        warms.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert_eq!(warms.running_pid(), Some(first), "the same commit: kept");
        warms.want(want("bbb222"));
        warms.poll(&|_| Some(PathBuf::from("/mirror.git")));
        let second = warms.running_pid().expect("second");
        assert_ne!(first, second);
        assert!(!alive(first), "the abandoned warm's group is gone");
        warms.abandon("test over");
    }

    /// cc-astra A33: a landing on another source never cuts the running
    /// warm off. Its warm is kept (a second landing there replaces it) and
    /// starts once the running one is gone; the running warm's own source
    /// moving still supersedes it, and then the wants start in the order
    /// they came.
    #[test]
    fn another_sources_landing_waits_for_the_running_warm_and_is_kept() {
        let scratch = Scratch::new("two-sources");
        let mut warms = warms(&scratch, 30);
        let mirror = |_: &Want| Some(PathBuf::from("/mirror.git"));
        let other = |commit: &str| Want {
            source: "/src/other".to_string(),
            ..want(commit)
        };
        warms.want(want("aaa111"));
        warms.poll(&mirror);
        let first = warms.running_pid().expect("spawn's warm");
        warms.want(other("bbb222"));
        warms.poll(&mirror);
        assert_eq!(warms.running_pid(), Some(first), "not cut off");
        assert!(alive(first));
        warms.want(other("ccc333"));
        warms.poll(&mirror);
        assert_eq!(warms.running_pid(), Some(first), "still not cut off");
        // Spawn's own main moves: its warm is superseded, and the other
        // source's, wanted first, starts, with its latest commit.
        warms.want(want("ddd444"));
        warms.poll(&mirror);
        let second = warms.running_pid().expect("the other source's warm");
        assert_ne!(second, first);
        assert!(!alive(first), "the superseded warm's group is gone");
        until("the other source's warm", || {
            scratch.0.join("args.ccc333").exists()
        });
        assert!(
            !scratch.0.join("args.bbb222").exists(),
            "a replaced want never runs"
        );
        // Spawn's new commit waits for it, kept.
        warms.poll(&mirror);
        assert_eq!(warms.running_pid(), Some(second));
        warms.abandon("test over");
        warms.poll(&mirror);
        let third = warms.running_pid().expect("spawn's new commit, kept");
        until("spawn's new commit's warm", || {
            scratch.0.join("args.ddd444").exists()
        });
        warms.abandon("test over");
        assert!(!alive(third));
    }

    /// A mirror that has not the commit yet (an owner's landing, a gate on
    /// the source holding the mirror) defers the warm to a later pass.
    #[test]
    fn a_warm_waits_for_a_mirror_with_its_commit() {
        let scratch = Scratch::new("defer");
        let mut warms = warms(&scratch, 30);
        warms.want(want("ccc333"));
        warms.poll(&|_| None);
        assert_eq!(warms.running_pid(), None);
        warms.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert!(
            warms.running_pid().is_some(),
            "started once the mirror has it"
        );
        warms.abandon("test over");
    }

    /// A warm that ended is said, its pid file removed, and its commit not
    /// warmed again.
    #[test]
    fn a_finished_warm_is_not_started_again() {
        let scratch = Scratch::new("done");
        let mut warms = warms(&scratch, 0);
        warms.want(want("ddd444"));
        warms.poll(&|_| Some(PathBuf::from("/mirror.git")));
        let pid = warms.running_pid().expect("started");
        until("the warm to end", || !alive(pid));
        warms.want(want("ddd444"));
        warms.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert_eq!(warms.running_pid(), None, "warmed once");
        assert!(!scratch.0.join("warm").join(PID_FILE).exists());
    }

    /// A gate that cannot be started is said; nothing else happens.
    #[test]
    fn a_warm_that_cannot_start_changes_nothing() {
        let scratch = Scratch::new("nostart");
        std::fs::write(scratch.0.join("warm"), "a file where the directory goes").expect("file");
        let mut warms = warms(&scratch, 0);
        warms.want(want("eee555"));
        warms.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert_eq!(warms.running_pid(), None);
    }

    /// The takeover stops a predecessor's warm only when the pid file still
    /// names it: same user, same start time, `gate.sh --base-only` of the
    /// same commit. A pid used again, or another program, is left alone.
    #[test]
    fn a_predecessors_warm_is_stopped_only_when_the_pid_file_still_names_it() {
        let scratch = Scratch::new("takeover");
        let mut old = warms(&scratch, 30);
        old.want(want("fff666"));
        old.poll(&|_| Some(PathBuf::from("/mirror.git")));
        let pid = old.running_pid().expect("the predecessor's warm");
        // Until the child has exec'd gate.sh it runs no gate.sh, and every
        // take_over below would find it "gone" for that reason alone.
        until("the warm to start its gate", || {
            scratch.0.join("args.fff666").exists()
        });
        let record = std::fs::read_to_string(scratch.0.join("warm").join(PID_FILE)).expect("pid");
        // Another commit in the file: not this warm, nothing stopped.
        std::fs::write(
            scratch.0.join("warm").join(PID_FILE),
            record.replace("fff666", "0000aa"),
        )
        .expect("edit");
        let mut new = warms(&scratch, 30);
        new.take_over();
        assert!(alive(pid), "a file naming another commit stops nothing");
        // Another start time: a pid used again.
        let start = Left::parse(&record).expect("parsed").start;
        std::fs::write(
            scratch.0.join("warm").join(PID_FILE),
            record.replace(&format!("start {start}"), &format!("start {}", start + 1)),
        )
        .expect("edit");
        new.take_over();
        assert!(alive(pid), "another start time stops nothing");
        // The file as written: stopped.
        std::fs::write(scratch.0.join("warm").join(PID_FILE), &record).expect("restore");
        new.take_over();
        until("the predecessor's warm to stop", || !alive(pid));
        assert!(!scratch.0.join("warm").join(PID_FILE).exists());
        old.abandon("test over");
    }

    /// A predecessor's warm that ignores TERM is killed after the grace,
    /// as an abandoned one is.
    #[test]
    fn a_predecessors_warm_that_ignores_term_is_killed_after_the_grace() {
        let scratch = Scratch::new("takeover-kill");
        let mut old = warms(&scratch, 30);
        std::fs::write(
            scratch.0.join("gate.sh"),
            "trap '' TERM\nprintf '%s\\n' \"$@\" >\"$WARM_ARGS.$3\"\nsleep 30\n",
        )
        .expect("gate");
        old.want(want("abc777"));
        old.poll(&|_| Some(PathBuf::from("/mirror.git")));
        let pid = old.running_pid().expect("the predecessor's warm");
        until("the warm to start its gate", || {
            scratch.0.join("args.abc777").exists()
        });
        let started = Instant::now();
        warms(&scratch, 30).take_over();
        until("the predecessor's warm to die", || !alive(pid));
        assert!(
            started.elapsed() >= super::ABANDON_GRACE,
            "TERM alone does not stop it: the KILL came after the grace"
        );
        old.abandon("test over");
    }

    #[test]
    fn a_pid_file_says_pid_start_and_commit_or_is_refused() {
        assert_eq!(
            Left::parse(
                "pid 4242\nstart 98765\ncommit abc123\nsource /src/my spawn\n\
                 scenarios /live/rsi/scenarios\n"
            ),
            Some(Left {
                pid: 4242,
                start: 98765,
                commit: "abc123".to_string(),
                source: Some("/src/my spawn".to_string()),
                scenarios: Some(PathBuf::from("/live/rsi/scenarios")),
            })
        );
        // An older integrator's file: no source, no scenarios.
        assert_eq!(
            Left::parse("pid 4242\nstart 98765\ncommit abc123\n"),
            Some(Left {
                pid: 4242,
                start: 98765,
                commit: "abc123".to_string(),
                source: None,
                scenarios: None,
            })
        );
        // A source without scenarios cannot be warmed as it was: neither.
        assert_eq!(
            Left::parse("pid 4242\nstart 98765\ncommit abc123\nsource /src/spawn\n")
                .and_then(|left| left.source),
            None
        );
        for wrong in [
            "",
            "pid 1\nstart 5\ncommit abc\n",
            "pid x\nstart 5\ncommit abc\n",
            "pid 7\ncommit abc\n",
            "pid 7\nstart 5\ncommit not-hex\n",
        ] {
            assert_eq!(Left::parse(wrong), None, "{wrong:?}");
        }
        // What a warm writes is what a take-over reads.
        let written = super::record(77, 88, &want("abc123"));
        assert_eq!(
            Left::parse(&written),
            Some(Left {
                pid: 77,
                start: 88,
                commit: "abc123".to_string(),
                source: Some("/src/spawn".to_string()),
                scenarios: Some(PathBuf::from("/live/rsi/scenarios")),
            })
        );
    }

    /// A pid that names no process any more: one that ran and was reaped.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().expect("true");
        let pid = child.id();
        child.wait().expect("reaped");
        pid
    }

    fn write_pid_file(scratch: &Scratch, text: &str) {
        std::fs::create_dir_all(scratch.0.join("warm")).expect("warm dir");
        std::fs::write(scratch.0.join("warm").join(PID_FILE), text).expect("pid file");
    }

    fn lost(commit: &str) -> Lost {
        Lost {
            source: "/src/spawn".to_string(),
            commit: commit.to_string(),
            scenarios: PathBuf::from("/live/rsi/scenarios"),
            tries: 0,
        }
    }

    fn warmed_args(scratch: &Scratch, commit: &str) -> Vec<String> {
        let args = scratch.0.join(format!("args.{commit}"));
        until("the gate's arguments", || args.exists());
        std::fs::read_to_string(&args)
            .expect("args")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A warm its integrator's restart cut off (gone: its parent-death
    /// signal ended it) is warmed again by the next integrator, for its
    /// source's main as it is, with its scenarios.
    #[test]
    fn a_gone_warm_with_a_source_is_warmed_again() {
        let scratch = Scratch::new("again-gone");
        write_pid_file(
            &scratch,
            &format!(
                "pid {}\nstart 1\ncommit aaa111\nsource /src/spawn\nscenarios \
                 /live/rsi/scenarios\n",
                dead_pid()
            ),
        );
        let mut new = warms(&scratch, 30);
        assert_eq!(
            new.take_over(),
            Taken::Again {
                lost: lost("aaa111"),
                stopped: false
            }
        );
        assert!(!scratch.0.join("warm").join(PID_FILE).exists());
        assert_eq!(new.lost_source(), Some("/src/spawn"));
        new.want_lost(true, &|source| {
            assert_eq!(source, "/src/spawn");
            Ok("aaa111".to_string())
        });
        assert_eq!(new.lost_source(), None, "wanted once");
        new.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert!(new.running_pid().is_some());
        assert_eq!(
            warmed_args(&scratch, "aaa111"),
            [
                "--base-only",
                "/mirror.git",
                "aaa111",
                "/live/rsi/scenarios"
            ]
        );
        new.abandon("test over");
    }

    /// A warm still running when the next integrator starts (no setpriv,
    /// or its parent-death signal not yet delivered) is stopped by the
    /// take-over, then warmed again.
    #[test]
    fn a_running_warm_with_a_source_is_stopped_then_warmed_again() {
        let scratch = Scratch::new("again-running");
        let mut old = warms(&scratch, 30);
        old.want(want("bbb222"));
        old.poll(&|_| Some(PathBuf::from("/mirror.git")));
        let pid = old.running_pid().expect("the predecessor's warm");
        until("the warm to start its gate", || {
            scratch.0.join("args.bbb222").exists()
        });
        std::fs::remove_file(scratch.0.join("args.bbb222")).expect("args");
        let mut new = warms(&scratch, 30);
        assert_eq!(
            new.take_over(),
            Taken::Again {
                lost: lost("bbb222"),
                stopped: true
            }
        );
        until("the predecessor's warm to stop", || !alive(pid));
        new.want_lost(true, &|_| Ok("bbb222".to_string()));
        new.poll(&|_| Some(PathBuf::from("/mirror.git")));
        let again = new.running_pid().expect("warmed again");
        assert_ne!(again, pid);
        assert_eq!(warmed_args(&scratch, "bbb222")[2], "bbb222");
        new.abandon("test over");
        old.abandon("test over");
    }

    /// Main moved between the two integrators by a move neither saw: the
    /// new main is warmed, never the commit the lost warm had.
    #[test]
    fn a_lost_warm_whose_main_moved_warms_the_new_main() {
        let scratch = Scratch::new("again-moved");
        write_pid_file(
            &scratch,
            &format!(
                "pid {}\nstart 1\ncommit ccc333\nsource /src/spawn\nscenarios \
                 /live/rsi/scenarios\n",
                dead_pid()
            ),
        );
        let mut new = warms(&scratch, 30);
        assert!(matches!(new.take_over(), Taken::Again { .. }));
        new.want_lost(true, &|_| Ok("ddd444".to_string()));
        new.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert_eq!(warmed_args(&scratch, "ddd444")[2], "ddd444");
        new.abandon("test over");
        assert!(
            !scratch.0.join("args.ccc333").exists(),
            "the old main is never warmed"
        );
        new.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert_eq!(new.running_pid(), None, "nothing more is wanted");
    }

    /// A source whose missions are all halted is warmed for no gate: the
    /// lost warm is dropped, and its main is not even read.
    #[test]
    fn a_lost_warm_on_a_halted_source_is_not_warmed_again() {
        let scratch = Scratch::new("again-halted");
        write_pid_file(
            &scratch,
            &format!(
                "pid {}\nstart 1\ncommit eee555\nsource /src/spawn\nscenarios /s\n",
                dead_pid()
            ),
        );
        let mut new = warms(&scratch, 30);
        assert!(matches!(new.take_over(), Taken::Again { .. }));
        new.want_lost(false, &|_| panic!("main is not read for a halted source"));
        assert_eq!(new.lost_source(), None);
        new.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert_eq!(new.running_pid(), None);
    }

    /// A main that cannot be read is tried again at the next passes, then
    /// given up; a warm this integrator wants first is not replaced.
    #[test]
    fn a_lost_warm_waits_for_its_main_and_for_the_warm_wanted_first() {
        let scratch = Scratch::new("again-wait");
        write_pid_file(
            &scratch,
            &format!(
                "pid {}\nstart 1\ncommit fff666\nsource /src/spawn\nscenarios /s\n",
                dead_pid()
            ),
        );
        let mut new = warms(&scratch, 30);
        assert!(matches!(new.take_over(), Taken::Again { .. }));
        new.want(want("abc999"));
        new.want_lost(true, &|_| panic!("not while another warm is wanted"));
        assert_eq!(new.lost_source(), Some("/src/spawn"));
        new.poll(&|_| None);
        for _ in 1..super::LOST_TRIES {
            new.wanted.clear();
            new.want_lost(true, &|_| Err("unreachable".to_string()));
            assert_eq!(new.lost_source(), Some("/src/spawn"), "tried again");
        }
        new.want_lost(true, &|_| Err("unreachable".to_string()));
        assert_eq!(new.lost_source(), None, "given up");
    }

    /// An older integrator's pid file names no source: whatever it warmed
    /// is not warmed again. A file that is not a warm's is as before.
    #[test]
    fn a_pid_file_without_a_source_or_not_a_warms_warms_nothing_again() {
        let scratch = Scratch::new("again-none");
        write_pid_file(
            &scratch,
            &format!("pid {}\nstart 1\ncommit abc123\n", dead_pid()),
        );
        let mut new = warms(&scratch, 30);
        assert_eq!(
            new.take_over(),
            Taken::NoSource {
                commit: "abc123".to_string()
            }
        );
        assert_eq!(new.lost_source(), None);
        assert!(!scratch.0.join("warm").join(PID_FILE).exists());
        write_pid_file(&scratch, "something else\n");
        assert_eq!(new.take_over(), Taken::NotAWarm);
        assert_eq!(new.lost_source(), None);
        assert!(!scratch.0.join("warm").join(PID_FILE).exists());
        assert_eq!(new.take_over(), Taken::Nothing);
        new.want_lost(true, &|_| panic!("nothing is lost"));
        new.poll(&|_| Some(PathBuf::from("/mirror.git")));
        assert_eq!(new.running_pid(), None);
    }

    /// The start time is field 22 of /proc/<pid>/stat, read after a command
    /// name that may hold spaces and parentheses.
    #[test]
    fn the_start_time_is_read_past_a_command_name_with_parentheses() {
        let stat = "4242 (sh (x) y) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 \
                    123456 2000000 300 18446744073709551615";
        assert_eq!(stat_start_ticks(stat), Some(123_456));
        assert_eq!(stat_start_ticks("garbage"), None);
    }
}

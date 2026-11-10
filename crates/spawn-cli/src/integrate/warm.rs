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
//! One warm at a time. A warm of a commit main has since moved past is
//! abandoned (its process group gets TERM, then KILL after
//! [`ABANDON_GRACE`]). The entries it finished stay valid for the commit
//! they name, and the new commit gets its own warm. A commit warmed once is
//! not warmed again by the same integrator.
//!
//! It cannot outlive the integrator unaccounted for:
//! - the gate runs under `setpriv --pdeathsig TERM`, started from the
//!   integrator's main thread (the parent-death signal follows the parent
//!   *thread*). An integrator that dies by any means TERMs its warm, whose
//!   trap stops its children and removes its scratch;
//! - it stays in the integrator's process tree and cgroup (its own process
//!   group, never a new session), so stopping the integrator's unit stops
//!   it too;
//! - its pid, start time and commit are in [`PID_FILE`]. A starting
//!   integrator ([`Warms::take_over`]) stops the warm that file names only
//!   when the process is this user's, started at the recorded time (a pid
//!   used again never matches), and runs `gate.sh --base-only` of the
//!   recorded commit. Anything else signals nothing.

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
    wanted: Option<Want>,
    running: Option<Warm>,
    done: Option<Want>,
}

impl Warms {
    pub(super) fn new(gate: PathBuf, env: Vec<(String, String)>, dir: PathBuf) -> Self {
        Self {
            gate,
            env,
            dir,
            wanted: None,
            running: None,
            done: None,
        }
    }

    /// Main of `want.source` is now `want.commit`: warm it at the next
    /// [`Self::poll`]. A later want replaces an earlier one not yet started.
    pub(super) fn want(&mut self, want: Want) {
        self.wanted = Some(want);
    }

    /// Once per pass, on the main thread: say how a warm that ended went,
    /// abandon one main has moved past, and start the wanted one when its
    /// mirror has the commit (`mirror` answers, refreshing it when it may;
    /// None: not this pass).
    pub(super) fn poll(&mut self, mirror: &dyn Fn(&Want) -> Option<PathBuf>) {
        self.reap();
        let Some(want) = self.wanted.clone() else {
            return;
        };
        if self.done.as_ref() == Some(&want)
            || self.running.as_ref().is_some_and(|warm| warm.want == want)
        {
            self.wanted = None;
            return;
        }
        if self.running.is_some() {
            self.abandon("main moved on");
        }
        let Some(repo) = mirror(&want) else {
            return;
        };
        self.wanted = None;
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
                    self.done = Some(warm.want);
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
                let record = format!("pid {pid}\nstart {ticks}\ncommit {}\n", want.commit);
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
    /// remove the file in any case.
    pub(super) fn take_over(&self) {
        let path = self.dir.join(PID_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let _ = std::fs::remove_file(&path);
        let Some(left) = Left::parse(&text) else {
            eprintln!(
                "integrate: {}: not a warm's pid file; nothing stopped",
                path.display()
            );
            return;
        };
        match left.still_running(&self.gate) {
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
            }
            Err(why) => println!(
                "integrate: the base warm of {} (pid {}) a previous integrator started is \
                 gone ({why}); nothing stopped",
                left.commit, left.pid
            ),
        }
    }

    /// The warm in flight, for tests.
    #[cfg(test)]
    pub(super) fn running_pid(&self) -> Option<u32> {
        self.running.as_ref().map(|warm| warm.child.id())
    }
}

/// What a pid file says.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Left {
    pid: u32,
    start: u64,
    commit: String,
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
        (!commit.is_empty() && commit.chars().all(|c| c.is_ascii_hexdigit())).then(|| Self {
            pid,
            start,
            commit: commit.to_string(),
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
    use super::{Left, PID_FILE, Want, Warms, stat_start_ticks};
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
            format!("printf '%s\\n' \"$@\" >\"$WARM_ARGS.$3\"\nsleep {secs}\n"),
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
        let new = warms(&scratch, 30);
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
            Left::parse("pid 4242\nstart 98765\ncommit abc123\n"),
            Some(Left {
                pid: 4242,
                start: 98765,
                commit: "abc123".to_string()
            })
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

//! What a long-lived spawn process does to itself at start.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Makes this process not dumpable (`prctl(PR_SET_DUMPABLE, 0)`), so that
/// another process of the same user can no longer read its
/// `/proc/<pid>/environ` (or `mem`, or attach to it): the kernel then treats
/// it like a process of root. The loop, the integrator and the server hold
/// the owner's database URL and tokens in their environment, and the
/// engines they start run as the same user.
///
/// A child it execs is dumpable again (the kernel resets it at `execve` for
/// an ordinary binary): an engine, a gate or git is unaffected, and its own
/// environment is what it was given.
///
/// It also stops core dumps of this process, and makes `/proc/self` owned by
/// root: read the process's uid with [`current_uid`], not from there.
///
/// # Errors
///
/// When the kernel refuses (not on Linux, it is not available).
pub fn undumpable() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
            .map_err(|error| format!("prctl(PR_SET_DUMPABLE, 0): {error}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("not dumpable is a Linux setting".to_string())
    }
}

/// The real uid of this process.
#[must_use]
pub fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// A process told apart from any later one of the same pid: its pid and its
/// start time (`/proc/<pid>/stat`'s 22nd field, clock ticks since boot).
/// Recorded where a process must be found again by another process, or after
/// a restart: a tmux pane's leader (P6), a gate's runner (gate v2 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessId {
    /// Numeric OS process identifier (`/proc/<pid>`).
    pub pid: u32,
    /// 0 when it was not known (the process was gone when it was recorded).
    pub start: u64,
}

impl ProcessId {
    /// `pid` as it is now, its start time read now (0 when it is gone).
    #[must_use]
    pub fn of(pid: u32) -> Self {
        Self {
            pid,
            start: start_time(pid).unwrap_or(0),
        }
    }

    /// As [`ProcessId::record`] writes it: `<pid> <start>`. `None` for
    /// anything else, or pid 0 or 1.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let pid: u32 = words.next()?.parse().ok()?;
        let start: u64 = words.next()?.parse().ok()?;
        (pid > 1 && words.next().is_none()).then_some(Self { pid, start })
    }

    /// The text [`ProcessId::parse`] reads.
    #[must_use]
    pub fn record(&self) -> String {
        format!("{} {}", self.pid, self.start)
    }

    /// Whether this very process still runs: its pid is there, with the
    /// recorded start time, and is no zombie. Never for an unknown start.
    #[must_use]
    pub fn alive(&self) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)) else {
            return false;
        };
        self.start != 0
            && start_time_of(&stat) == Some(self.start)
            && state_of(&stat).is_some_and(|state| state != 'Z')
    }

    /// Whether what is left of the session (or process group) this process
    /// led is still its own: the kernel gives no new process a pid a
    /// session or group still uses. So: no process has the pid (its
    /// members, if any, are this one's), or the process with it is this one
    /// (same start time). A pid given anew, or a start that was not
    /// recorded while a process has the pid now, names nothing.
    #[must_use]
    pub(crate) fn leads_its_session(&self) -> bool {
        if self.pid <= 1 {
            return false;
        }
        match start_time(self.pid) {
            None => true,
            Some(now) => self.start != 0 && now == self.start,
        }
    }
}

/// The process's start time, or `None` when there is no such process.
#[must_use]
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    start_time_of(&stat)
}

/// A `/proc/<pid>/stat` line's start time: the 20th field after the
/// command's closing parenthesis (a command can hold spaces and
/// parentheses; state is the first).
#[must_use]
pub fn start_time_of(stat: &str) -> Option<u64> {
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// A `/proc/<pid>/stat` line's state (`R`, `S`, `Z`, ...).
fn state_of(stat: &str) -> Option<char> {
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .next()?
        .chars()
        .next()
}

/// Whether `path` is a directory (not a symlink) of this user that no one
/// else may enter or write: mode & 0o077 == 0.
#[allow(clippy::verbose_bit_mask)] // `& 0o077 == 0` reads as the mode it means
#[must_use]
pub fn private_to_us(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == current_uid() && meta.mode() & 0o077 == 0)
}

/// Write `text` to `directory/name` whole or not at all (a temporary name,
/// then a rename), the file 0600 and the directory made 0700 when missing.
///
/// # Errors
///
/// What the filesystem says.
pub(crate) fn write_private(
    directory: &std::path::Path,
    name: &str,
    text: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let path = directory.join(name);
    let temporary = directory.join(format!(".{name}.{}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .and_then(|()| std::fs::rename(&temporary, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

#[cfg(all(test, target_os = "linux"))]
mod process_id_tests {
    use super::ProcessId;

    #[test]
    fn a_process_id_tells_this_process_from_a_later_one_of_its_pid() {
        let me = ProcessId::of(std::process::id());
        assert_ne!(me.start, 0);
        assert!(me.alive() && me.leads_its_session());
        let later = ProcessId {
            start: me.start + 1,
            ..me
        };
        assert!(
            !later.alive() && !later.leads_its_session(),
            "a pid given anew"
        );
        let unknown = ProcessId { start: 0, ..me };
        assert!(
            !unknown.alive() && !unknown.leads_its_session(),
            "a start not recorded"
        );

        let mut child = std::process::Command::new("true").spawn().expect("true");
        let gone = ProcessId::of(child.id());
        child.wait().expect("reaped");
        assert!(!gone.alive(), "exited");
        assert!(gone.leads_its_session(), "no process has the pid");

        assert_eq!(ProcessId::parse(&me.record()), Some(me));
        for bad in ["", "x 1", "1 5", "0 5", "7", "7 5 9"] {
            assert_eq!(ProcessId::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_start_time_is_read_past_a_command_with_parentheses() {
        let stat = "42 (a) (b c) S 1 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(super::start_time_of(stat), Some(987_654));
        assert_eq!(super::state_of(stat), Some('S'));
        assert_eq!(super::start_time_of("42 (x) S 1"), None);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{current_uid, undumpable};
    use std::io::{BufRead, BufReader, ErrorKind};
    use std::process::{Child, Command, Stdio};

    /// Set in the child [`child_helper`] runs in.
    const CHILD: &str = "SPAWN_UNDUMPABLE_CHILD";

    /// Not a test of its own: the body of the child process the tests below
    /// start (this test binary, run again for this one test). It makes
    /// itself not dumpable, reads the socket table, starts `sleep` with an
    /// environment of one variable, says what it did on stdout and waits
    /// for its parent to kill it.
    #[test]
    fn child_helper() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        undumpable().expect("not dumpable");
        let hz = rustix::param::clock_ticks_per_second();
        let tcp = std::fs::read_to_string("/proc/net/tcp").map_or_else(
            |error| format!("tcp error {error}"),
            |table| format!("tcp read {}", table.lines().count()),
        );
        let mut sleep = Command::new("/bin/sleep")
            .arg("30")
            .env_clear()
            .env("SPAWN_PROBE", "1")
            .spawn()
            .expect("sleep");
        // On a line of its own: libtest's "test ... " has no newline yet.
        println!();
        println!("hz {hz}");
        println!("{tcp}");
        println!("exec {}", sleep.id());
        println!("ready {}", std::process::id());
        // The parent kills it first.
        let _ = sleep.wait();
    }

    /// The helper started, and the lines it said once ready.
    fn helper() -> (Child, Vec<String>) {
        let mut child = Command::new(std::env::current_exe().expect("this test binary"))
            .args(["--exact", "process::tests::child_helper", "--nocapture"])
            .args(["--test-threads", "1"])
            .env(CHILD, "1")
            .env("SPAWN_SECRET_PROBE", "hidden")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the helper starts");
        let mut lines = Vec::new();
        for line in BufReader::new(child.stdout.take().expect("stdout")).lines() {
            let line = line.expect("a line");
            let ready = line.starts_with("ready ");
            lines.push(line);
            if ready {
                break;
            }
        }
        (child, lines)
    }

    fn said(lines: &[String], word: &str) -> String {
        lines
            .iter()
            .find_map(|line| line.strip_prefix(&format!("{word} ")))
            .unwrap_or_else(|| panic!("no {word} in {lines:?}"))
            .to_string()
    }

    fn stop_helper(mut child: Child, lines: &[String]) {
        let sleep = said(lines, "exec");
        let _ = Command::new("kill").arg(&sleep).status();
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn an_undumpable_process_hides_its_environment_and_its_children_do_not() {
        // As root the checks prove nothing: root reads it anyway.
        if current_uid() == 0 {
            return;
        }
        let (child, lines) = helper();
        let pid = said(&lines, "ready");
        let sleep = said(&lines, "exec");
        // Only whether it was readable, never what it holds: a failure
        // message must not print an environment (this one's has tokens).
        let hidden = std::fs::read(format!("/proc/{pid}/environ")).map(|bytes| bytes.len());
        let execd = std::fs::read(format!("/proc/{sleep}/environ"));
        let ours =
            std::fs::read(format!("/proc/{}/environ", std::process::id())).map(|bytes| bytes.len());
        stop_helper(child, &lines);

        let error = hidden.expect_err("the helper's environment is unreadable (bytes read)");
        assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{error}");
        // The program it started is dumpable again from its exec, with the
        // environment it was given.
        // The sleep's environment is the one variable it was given.
        assert!(
            execd
                .as_deref()
                .is_ok_and(|bytes| bytes == b"SPAWN_PROBE=1\0"),
            "the program's environment, readable and its own: {}",
            execd
                .as_ref()
                .map_or_else(ToString::to_string, |bytes| format!(
                    "{} bytes",
                    bytes.len()
                ))
        );
        // A process that did not call it: readable, as before.
        assert!(ours.is_ok(), "{ours:?}");
        // It still reads the socket table (the server tells the owner's
        // connections by it).
        assert!(said(&lines, "tcp").starts_with("read "), "{lines:?}");
    }

    #[test]
    fn undumpable_process_reads_clock_ticks_without_panicking() {
        let (child, lines) = helper();
        stop_helper(child, &lines);
        let hz: u64 = said(&lines, "hz")
            .parse()
            .expect("hz must be a positive integer");
        assert!(hz > 0, "expected positive clock ticks, got {hz}");
    }

    #[test]
    fn the_uid_is_this_processs_own() {
        use std::os::unix::fs::MetadataExt;
        // This test process is dumpable: /proc/self is its own.
        let proc_self = std::fs::metadata("/proc/self").expect("/proc").uid();
        assert_eq!(current_uid(), proc_self);
    }
}

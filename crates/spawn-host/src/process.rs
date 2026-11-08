//! What a long-lived spawn process does to itself at start.

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

    #[test]
    fn an_undumpable_process_hides_its_environment_and_its_children_do_not() {
        // As root the checks prove nothing: root reads it anyway.
        if current_uid() == 0 {
            return;
        }
        let (mut child, lines) = helper();
        let pid = said(&lines, "ready");
        let sleep = said(&lines, "exec");
        // Only whether it was readable, never what it holds: a failure
        // message must not print an environment (this one's has tokens).
        let hidden = std::fs::read(format!("/proc/{pid}/environ")).map(|bytes| bytes.len());
        let execd = std::fs::read(format!("/proc/{sleep}/environ"));
        let ours =
            std::fs::read(format!("/proc/{}/environ", std::process::id())).map(|bytes| bytes.len());
        let _ = Command::new("kill").arg(&sleep).status();
        let _ = child.kill();
        let _ = child.wait();

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
    fn the_uid_is_this_processs_own() {
        use std::os::unix::fs::MetadataExt;
        // This test process is dumpable: /proc/self is its own.
        let proc_self = std::fs::metadata("/proc/self").expect("/proc").uid();
        assert_eq!(current_uid(), proc_self);
    }
}

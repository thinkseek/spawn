//! tmux adapter.
//!
//! tmux gives each execution a named session, a scrollback and a way to read
//! the last lines of the pane. `send-keys -l` types bytes literally, so a
//! prompt reaches the engine unchanged.
//!
//! tmux runs on the same machine as Spawn, and the engine starts in that
//! terminal. The host keeps no state, so it works the same in a new
//! process.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{Launch, READ_LIMIT, Runner, TerminalError, TerminalHost};

/// Variables kept out of the tmux server's environment, besides every
/// `SPAWN_*` one (see [`scrub`]).
const SCRUBBED: [&str; 2] = ["PGPASSWORD", "TYPESAFE_API_KEY"];

/// Whether a variable of the controller's stays out of tmux: every `SPAWN_*`
/// setting (database logins, the serve token, the live fleet's tmux socket,
/// repository and workspace root) and the secrets in [`SCRUBBED`]. A pane
/// gets only what its launch passes explicitly; an agent that runs the
/// black-box or e2e scripts in its pane must not reach the live fleet.
fn scrub(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with("SPAWN_") || SCRUBBED.contains(&name))
}

/// Size of a new terminal. Claude needs width for a prompt box and height
/// for the tail window.
const WIDTH: &str = "200";
const HEIGHT: &str = "50";

/// How long [`TerminalHost::terminate`] waits for a killed terminal's
/// processes to exit after `kill-session` hangs their terminal up, and again
/// after it sends the survivors SIGKILL.
const HANGUP_WAIT: Duration = Duration::from_secs(5);
const KILL_WAIT: Duration = Duration::from_secs(2);

/// How often that wait looks.
const GONE_POLL: Duration = Duration::from_millis(20);

/// tmux host.
pub struct TmuxHost {
    program: String,
    socket: Option<String>,
    /// [`HANGUP_WAIT`] and [`KILL_WAIT`], shorter in a test.
    waits: (Duration, Duration),
    /// Where the leaders directory goes instead of `$XDG_RUNTIME_DIR`
    /// ([`TmuxHost::with_leaders_under`]): a test's own.
    leaders_base: Option<std::path::PathBuf>,
}

/// One finished tmux call.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl TmuxHost {
    /// Host whose tmux shares this machine with Spawn, on tmux's default
    /// server.
    pub fn local(program: &str) -> Self {
        Self::on_socket(program, None)
    }

    /// Host on the tmux server of `socket` (`tmux -L`), or the default one.
    pub fn on_socket(program: &str, socket: Option<String>) -> Self {
        Self {
            program: program.to_string(),
            socket,
            waits: (HANGUP_WAIT, KILL_WAIT),
            leaders_base: None,
        }
    }

    /// The same host, waiting `hangup` for a killed terminal's processes to
    /// exit and `kill` after SIGKILL ([`TerminalHost::terminate`]).
    #[must_use]
    pub fn with_terminate_waits(mut self, hangup: Duration, kill: Duration) -> Self {
        self.waits = (hangup, kill);
        self
    }

    /// The same host, recording its terminals' pane leaders under `base`
    /// instead of `$XDG_RUNTIME_DIR` (a test's private directory; the same
    /// checks apply).
    #[must_use]
    pub fn with_leaders_under(mut self, base: std::path::PathBuf) -> Self {
        self.leaders_base = Some(base);
        self
    }

    /// The pids of the terminal's panes: each the leader of the session its
    /// runner and everything the runner started run in (tmux starts a pane
    /// with `setsid`). Empty when the terminal is gone.
    fn pane_pids(&self, terminal: &TerminalName) -> Result<Vec<u32>, TerminalError> {
        let run = self.run(&[
            "list-panes".to_string(),
            "-s".to_string(),
            "-t".to_string(),
            // The session form, `=name:`: exact. `=name` is a window target
            // here, and a session that is gone falls back to a prefix or a
            // window name, so terminating a gone spawn-exec_1 read
            // spawn-exec_12's pane and killed its runner (rev-f on ca758b4b).
            Self::pane(terminal),
            "-F".to_string(),
            "#{pane_pid}".to_string(),
        ])?;
        match run.code {
            Some(0) => Ok(run
                .stdout
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect()),
            // A terminal already gone has nothing to wait for.
            Some(1) if gone(&run.stderr) => Ok(Vec::new()),
            other => Err(TerminalError(format!(
                "list-panes returned {other:?}: {}",
                run.stderr.trim()
            ))),
        }
    }

    /// Wait until no process is left in the sessions `leaders` lead: within
    /// the hang-up wait, then SIGKILL to what is left and the kill wait. An
    /// error names what outlived both.
    fn wait_gone(&self, terminal: &TerminalName, leaders: &[u32]) -> Result<(), TerminalError> {
        let (hangup, kill) = self.waits;
        if until_gone(leaders, hangup).is_empty() {
            return Ok(());
        }
        for pid in session_members(leaders) {
            if let Some(pid) = rustix::process::Pid::from_raw(pid.cast_signed()) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
        let left = until_gone(leaders, kill);
        if left.is_empty() {
            return Ok(());
        }
        Err(TerminalError(format!(
            "{} process(es) of {terminal}'s runner outlived kill-session and SIGKILL: {left:?}",
            left.len()
        )))
    }

    /// Where this host records its terminals' pane leaders: a private
    /// directory per tmux this host runs (its program and socket), so a
    /// host on another server (a test's, the live fleet's) never reads the
    /// record of a terminal of the same name there. Stable across a
    /// restart and a new binary: the name is an FNV-1a hash of the words.
    ///
    /// Only under `$XDG_RUNTIME_DIR`, and only while it and both levels
    /// below it are directories of this user that no one else may enter
    /// ([`private_to_us`]): a record names a session that terminate
    /// SIGKILLs, so a directory another user could write (a shared /tmp,
    /// one pre-made 0777 or a symlink to theirs) must never be read or
    /// written (rev-b on b279f33c). None without it: the pane alone, as
    /// before P6. `create` makes the two levels (0700) when missing.
    fn leaders_directory(&self, create: bool) -> Option<std::path::PathBuf> {
        let base = self
            .leaders_base
            .clone()
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from))?;
        self.leaders_directory_in(&base, create)
    }

    fn leaders_directory_in(
        &self,
        base: &std::path::Path,
        create: bool,
    ) -> Option<std::path::PathBuf> {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for word in self.words(&[]) {
            for byte in word.bytes().chain([0]) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        let user = base.join(format!(
            "spawn-leaders-{}",
            rustix::process::getuid().as_raw()
        ));
        let directory = user.join(format!("{hash:016x}"));
        if !private_to_us(base) {
            return None;
        }
        for level in [&user, &directory] {
            if create {
                use std::os::unix::fs::DirBuilderExt;
                let _ = std::fs::DirBuilder::new().mode(0o700).create(level);
            }
            if !private_to_us(level) {
                return None;
            }
        }
        Some(directory)
    }

    /// Record `leader`, the pane's pid, with its start time, for
    /// [`Self::recorded_leader`]. Best effort: a terminal whose leader is
    /// not recorded is terminated as before, by its pane alone.
    fn record_leader(&self, terminal: &TerminalName, leader: u32) {
        if let Some(directory) = self.leaders_directory(true) {
            record_leader_in(&directory, terminal, leader);
        }
    }

    /// The pane leader recorded at the terminal's start, when it still
    /// names the terminal's session. A session is told by its leader's pid
    /// (`session_members`), and the kernel gives no new process that pid
    /// while any process is in the session, the leader included. So:
    /// - no process has the pid: whatever is left of the session is the
    ///   terminal's (or nothing is);
    /// - the process with the pid started when the leader did: it is the
    ///   leader;
    /// - it started at another time (or the record does not say when the
    ///   leader started): the pid was given anew, so nothing of the
    ///   terminal's session is left, and the record names nothing.
    fn recorded_leader(&self, terminal: &TerminalName) -> Option<u32> {
        let text =
            std::fs::read_to_string(self.leaders_directory(false)?.join(terminal.as_str())).ok()?;
        let mut words = text.split_whitespace();
        let leader: u32 = words.next()?.parse().ok()?;
        let started: u64 = words.next()?.parse().ok()?;
        if leader <= 1 {
            return None;
        }
        match start_time(leader) {
            None => Some(leader),
            Some(now) if started != 0 && now == started => Some(leader),
            Some(_) => None,
        }
    }

    /// Remove the terminal's record, once its processes are gone.
    fn forget_leader(&self, terminal: &TerminalName) {
        if let Some(directory) = self.leaders_directory(false) {
            let _ = std::fs::remove_file(directory.join(terminal.as_str()));
        }
    }

    fn words(&self, arguments: &[String]) -> Vec<String> {
        let mut words = vec![self.program.clone()];
        if let Some(socket) = self.socket.as_ref() {
            words.push("-L".to_string());
            words.push(socket.clone());
        }
        words.extend(arguments.iter().cloned());
        words
    }

    fn run(&self, arguments: &[String]) -> Result<Run, TerminalError> {
        run_words(&self.words(arguments))
    }

    /// [`Self::run`], killed and failed when it has not exited within
    /// `limit`: a tmux server that stopped answering (stopped, swapping, a
    /// pane that never yields) must not hold the caller's tick.
    fn run_within(&self, arguments: &[String], limit: Duration) -> Result<Run, TerminalError> {
        run_words_within(&self.words(arguments), limit)
    }

    /// Session target with exact-name matching.
    fn target(terminal: &TerminalName) -> String {
        format!("={}", terminal.as_str())
    }

    /// Pane target with exact-name matching.
    ///
    /// `send-keys` and `capture-pane` take a pane target, and tmux reads a
    /// bare `=name` there as a pane specification. `=name:` names the
    /// active pane of the current window and does not depend on
    /// `pane-base-index`.
    fn pane(terminal: &TerminalName) -> String {
        format!("={}:", terminal.as_str())
    }

    /// The paste buffer that holds a terminal's staged text.
    fn buffer(terminal: &TerminalName) -> String {
        format!("spawn-{}", terminal.as_str())
    }
}

/// The processes still in the sessions `leaders` lead once none is left or
/// `limit` passes.
fn until_gone(leaders: &[u32], limit: Duration) -> Vec<u32> {
    let deadline = Instant::now() + limit;
    loop {
        let left = session_members(leaders);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(GONE_POLL);
    }
}

/// The live processes (a zombie writes nothing) whose session is one of
/// `leaders`, by `/proc/<pid>/stat`. A process that left the session
/// (`setsid`) is not among them.
fn session_members(leaders: &[u32]) -> Vec<u32> {
    if leaders.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut members: Vec<u32> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| session_of(&stat))
                .is_some_and(|(state, session)| state != 'Z' && leaders.contains(&session))
        })
        .collect();
    members.sort_unstable();
    members
}

/// A `/proc/<pid>/stat` line's state and session: the fields after the
/// command's closing parenthesis (a command can hold spaces and parentheses)
/// are state, ppid, pgrp, session.
fn session_of(stat: &str) -> Option<(char, u32)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let session = fields.nth(2)?.parse().ok()?;
    Some((state, session))
}

/// [`TmuxHost::record_leader`] into `directory`, already checked.
fn record_leader_in(directory: &std::path::Path, terminal: &TerminalName, leader: u32) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // Unknown (0) when the leader is already gone: then the record
    // stands for its session's members only (`recorded_leader`).
    let started = start_time(leader).unwrap_or(0);
    let path = directory.join(terminal.as_str());
    let temporary = directory.join(format!(".{}.{}", terminal.as_str(), std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| writeln!(file, "{leader} {started}"))
        .and_then(|()| std::fs::rename(&temporary, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Whether `path` is a directory (not a symlink) of this user that no one
/// else may enter or write: mode & 0o077 == 0.
#[allow(clippy::verbose_bit_mask)] // `& 0o077 == 0` reads as the mode it means
fn private_to_us(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.is_dir()
            && meta.uid() == rustix::process::getuid().as_raw()
            && meta.mode() & 0o077 == 0
    })
}

/// The process's start time (`/proc/<pid>/stat`'s 22nd field, in clock
/// ticks since boot), or `None` when there is no such process.
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    start_time_of(&stat)
}

/// A `/proc/<pid>/stat` line's start time: the 20th field after the
/// command's closing parenthesis (state is the first).
fn start_time_of(stat: &str) -> Option<u64> {
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// How long a `capture-pane` may take.
const CAPTURE_LIMIT: Duration = Duration::from_secs(5);

/// The fields [`runner_of`] reads, in its order.
///
/// The separator is a printable `|`, not a tab: tmux replaces a tab with `_`
/// when its locale is not UTF-8 (`env -i tmux ... -F '#{a}\t#{b}'`), which
/// would make every field one and every runner unreadable. A `|` inside the
/// last field, a pane's start command, is read whole.
const PANE_STATE: &str =
    "#{pane_dead}|#{pane_dead_status}|#{pane_current_command}|#{pane_start_command}";

/// The command that runs `words` (the program, then its arguments), with
/// the controller's settings kept out of its environment.
fn command_of(words: &[String]) -> Command {
    let mut command = Command::new(&words[0]);
    command.args(&words[1..]);
    // The first command starts the tmux server, whose global environment
    // every pane inherits and can read with `tmux show-environment -g`.
    for (name, _) in std::env::vars_os() {
        if scrub(&name) {
            command.env_remove(name);
        }
    }
    command
}

/// Run `words`: the program, then its arguments.
fn run_words(words: &[String]) -> Result<Run, TerminalError> {
    let failed = |error: std::io::Error| TerminalError(format!("run {}: {error}", words[0]));
    let output = command_of(words).output().map_err(failed)?;
    Ok(Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

/// [`run_words`], killed and failed when it has not exited within `limit`.
/// Its output is read while it runs, so a large one cannot fill a pipe and
/// stall it.
fn run_words_within(words: &[String], limit: Duration) -> Result<Run, TerminalError> {
    use std::io::Read;
    use std::process::Stdio;
    let failed = |error: std::io::Error| TerminalError(format!("run {}: {error}", words[0]));
    let mut child = command_of(words)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(failed)?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(failed)? {
            break status;
        }
        if started.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TerminalError(format!(
                "{} {} did not answer within {} s",
                words[0],
                words.get(1).map_or("", String::as_str),
                limit.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(Run {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
    })
}

/// Write `environment` as `NAME='value'` lines to a new file only this uid
/// can read (created exclusively, mode 600, in `XDG_RUNTIME_DIR` when there
/// is one, else the temporary directory), for a pane's shell to source.
fn write_environment(
    terminal: &TerminalName,
    environment: &[(String, String)],
) -> Result<std::path::PathBuf, TerminalError> {
    let mut text = String::new();
    for (key, value) in environment {
        let name_ok = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            return Err(TerminalError(format!(
                "{key} is not an environment variable name"
            )));
        }
        text.push_str(key);
        text.push('=');
        text.push_str(&spawn_core::shell::shell_quote(value));
        text.push('\n');
    }
    write_private(terminal, LAUNCH, "env", "the environment", &text)
}

/// The kind of [`write_private`] file a launch's environment goes through
/// ([`write_environment`]).
const LAUNCH: &str = "launch";

/// What the start command of a pane this host launched as `terminal` holds,
/// and nothing else's does: the name of its environment file
/// (`spawn-launch-<terminal>-<pid>-<nanos>.env`, [`write_private`]). The
/// `-` after the name keeps `spawn-exec_1` from matching `spawn-exec_10`.
fn launch_marker(terminal: &TerminalName) -> String {
    format!("spawn-{LAUNCH}-{}-", terminal.as_str())
}

/// What a terminal hosts, from one `list-panes` line of
/// `#{pane_dead}\t#{pane_dead_status}\t#{pane_current_command}\t#{pane_start_command}`
/// ([`TerminalHost::runner`]): the runner this host launched there, alive,
/// or why not.
fn runner_of(terminal: &TerminalName, line: &str) -> Runner {
    let mut fields = line.splitn(4, '|');
    let dead = fields.next().unwrap_or_default();
    let status = fields.next().unwrap_or_default();
    let current = fields.next().unwrap_or_default();
    let started = fields.next().unwrap_or_default();
    if dead == "1" {
        let status = if status.is_empty() { "none" } else { status };
        return Runner::Untracked(format!(
            "the pane of {terminal} is dead (exit status {status}): its runner is gone"
        ));
    }
    if started.contains(&launch_marker(terminal)) {
        return Runner::Running;
    }
    let started = if started.is_empty() {
        "no command (a shell, as a session restored after a restart is)".to_string()
    } else {
        let shown: String = started.chars().take(160).collect();
        format!("`{shown}`")
    };
    Runner::Untracked(format!(
        "{terminal} does not host the runner Spawn started there: its pane was started with \
         {started} and runs `{current}`"
    ))
}

/// `text` in a new file only this uid reads (mode 600, created exclusively)
/// in `XDG_RUNTIME_DIR` (else the temporary directory): what must reach tmux
/// but never a command line, which every local user reads in /proc and tmux
/// caps in size. The caller removes it.
fn write_private(
    terminal: &TerminalName,
    kind: &str,
    extension: &str,
    what: &str,
    text: &str,
) -> Result<std::path::PathBuf, TerminalError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let path = directory.join(format!(
        "spawn-{kind}-{}-{}-{nanos}.{extension}",
        terminal.as_str(),
        std::process::id()
    ));
    let failed =
        |error: std::io::Error| TerminalError(format!("write {what} of {terminal}: {error}"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(failed)?;
    if let Err(error) = file.write_all(text.as_bytes()) {
        let _ = std::fs::remove_file(&path);
        return Err(failed(error));
    }
    Ok(path)
}

/// How many `new-session` calls one [`TerminalHost::start`] makes when
/// each reaches a server that shuts down ([`server_left`]). The client sees
/// its connection drop only once the old server's socket is closed for
/// good, so the next call finds no server and starts one: one shutdown
/// costs one call. The rest cover another server coming and going
/// meanwhile, without looping on a tmux that always fails this way.
const START_ATTEMPTS: usize = 3;

impl TmuxHost {
    /// One `new-session` for `launch`, its environment file removed unless
    /// a pane was started to remove it.
    fn new_session(&self, launch: &Launch) -> Result<Run, TerminalError> {
        let mut arguments = vec![
            "new-session".to_string(),
            "-d".to_string(),
            // The pane's pid, the leader of the session the runner runs in,
            // recorded for terminate (`record_leader`).
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_pid}".to_string(),
            "-s".to_string(),
            launch.terminal.as_str().to_string(),
            "-x".to_string(),
            WIDTH.to_string(),
            "-y".to_string(),
            HEIGHT.to_string(),
            "-c".to_string(),
            launch.cwd.clone(),
        ];
        // The environment (a database login among it) never goes on a
        // command line, which every local user reads in /proc: it goes
        // through a file only this uid reads, which the pane's shell sources
        // and removes before it runs the program. Every launch has that file,
        // empty or not: its name, on the pane's start command, is how
        // `runner` tells this launch from anything else of the same name.
        let file = write_environment(&launch.terminal, &launch.environment)?;
        arguments.extend(
            [
                "--",
                "/bin/sh",
                "-c",
                "set -a; . \"$0\"; rm -f -- \"$0\"; set +a; exec \"$@\"",
            ]
            .map(str::to_string),
        );
        arguments.push(file.to_string_lossy().into_owned());
        arguments.push(launch.program.clone());
        arguments.extend(launch.arguments.iter().cloned());

        let run = self.run(&arguments);
        if !matches!(&run, Ok(run) if run.code == Some(0)) {
            // No pane ran to remove it.
            let _ = std::fs::remove_file(&file);
        }
        run
    }
}

impl TerminalHost for TmuxHost {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        let mut attempts = 1;
        let run = loop {
            let run = self.new_session(launch)?;
            // The server this call reached was shutting down after its last
            // session ended (another terminal was just killed or exited)
            // and dropped the call unread: nothing was started, and a
            // server that goes takes its sessions with it, so a new call
            // cannot start the terminal twice.
            if run.code != Some(0) && server_left(&run.stderr) && attempts < START_ATTEMPTS {
                attempts += 1;
                continue;
            }
            break run;
        };
        if run.code == Some(0) {
            if let Some(leader) = run
                .stdout
                .lines()
                .next()
                .and_then(|line| line.trim().parse().ok())
            {
                self.record_leader(&launch.terminal, leader);
            }
            return Ok(());
        }
        let stderr = run.stderr.trim();
        if stderr.contains("duplicate session") {
            return Err(TerminalError(format!("{} already exists", launch.terminal)));
        }
        Err(TerminalError(format!(
            "new-session for {} failed: {stderr}",
            launch.terminal
        )))
    }

    fn stage_text(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        // A named buffer, which no key reaches until it is pasted. Loaded
        // from a private file, not given on the command line: tmux refuses
        // a command over its message size (16 KiB), and a long prompt (an
        // objective's brief) is longer.
        let path = write_private(terminal, "stage", "txt", "the text to stage", text)?;
        let load = vec![
            "load-buffer".to_string(),
            "-b".to_string(),
            Self::buffer(terminal),
            path.to_string_lossy().into_owned(),
        ];
        let run = self.run(&load);
        let _ = std::fs::remove_file(&path);
        let run = run?;
        if run.code != Some(0) {
            return Err(TerminalError(format!(
                "load-buffer failed: {}",
                run.stderr.trim()
            )));
        }
        Ok(())
    }

    fn submit_staged(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        let target = Self::pane(terminal);
        // One bracketed paste, then one Enter. Typing a line and pressing
        // Enter per line would submit a multi-paragraph prompt in pieces.
        let paste = vec![
            "paste-buffer".to_string(),
            "-p".to_string(),
            "-d".to_string(),
            "-b".to_string(),
            Self::buffer(terminal),
            "-t".to_string(),
            target.clone(),
        ];
        let run = self.run(&paste)?;
        if run.code != Some(0) {
            return Err(TerminalError(format!(
                "paste-buffer failed: {}",
                run.stderr.trim()
            )));
        }
        let enter = vec![
            "send-keys".to_string(),
            "-t".to_string(),
            target,
            "Enter".to_string(),
        ];
        let run = self.run(&enter)?;
        if run.code != Some(0) {
            return Err(TerminalError(format!(
                "send-keys Enter failed: {}",
                run.stderr.trim()
            )));
        }
        Ok(())
    }

    fn send_keys(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError> {
        let target = Self::pane(terminal);
        // One key per call. A dialog reads the first key and redraws; keys
        // sent in the same burst can arrive before it listens again.
        for key in keys {
            let send = vec![
                "send-keys".to_string(),
                "-t".to_string(),
                target.clone(),
                (*key).to_string(),
            ];
            let run = self.run(&send)?;
            if run.code != Some(0) {
                return Err(TerminalError(format!(
                    "send-keys {key} failed: {}",
                    run.stderr.trim()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Ok(())
    }

    fn is_alive(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        let run = self.run(&[
            "has-session".to_string(),
            "-t".to_string(),
            Self::target(terminal),
        ])?;
        match run.code {
            Some(0) => Ok(true),
            Some(1) if gone(&run.stderr) => Ok(false),
            // Anything else (a socket we may not open, a failing tmux)
            // says nothing about the terminal: ending its execution
            // on it would unmount a workspace an agent may still be using.
            other => Err(TerminalError(format!(
                "has-session returned {other:?}: {}",
                run.stderr.trim()
            ))),
        }
    }

    /// Whether the server that holds every terminal is there. `list-sessions`
    /// only asks: unlike `new-session` it never starts a server, so a
    /// check after a host restart cannot make one appear and hide the fact
    /// that every terminal died with it.
    fn server_alive(&self) -> bool {
        let Ok(run) = self.run(&["list-sessions".to_string()]) else {
            return true;
        };
        match run.code {
            // Exit 1 says no server answers, unless tmux said something
            // else (a socket we may not open, a failing tmux): that says
            // nothing about the terminals.
            Some(1) if gone(&run.stderr) => false,
            _ => true,
        }
    }

    /// `list-panes`, not `display-message`, which answers a target that is
    /// not there with empty fields and success.
    fn runner(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<Runner, TerminalError> {
        // One tmux call, as `has-session` was.
        let run = self.run(&[
            "list-panes".to_string(),
            "-t".to_string(),
            Self::pane(terminal),
            "-F".to_string(),
            PANE_STATE.to_string(),
        ])?;
        match run.code {
            Some(0) => {}
            Some(1) if gone(&run.stderr) => return Ok(Runner::Gone),
            // Like `has-session`: a failing tmux says nothing about the
            // terminal.
            other => {
                return Err(TerminalError(format!(
                    "list-panes returned {other:?}: {}",
                    run.stderr.trim()
                )));
            }
        }
        // A session with no pane left is being torn down.
        Ok(run
            .stdout
            .lines()
            .next()
            .map_or(Runner::Gone, |line| runner_of(terminal, line)))
    }

    /// One `list-panes -a` for the whole fleet, not one per terminal: what a
    /// Bridge pass costs when every execution is asked whether its runner
    /// still runs. The sessions of other programs on the socket are read too
    /// and ignored.
    fn runners(
        &self,
        terminals: &[(TerminalName, WorkspaceId)],
    ) -> Result<Vec<(TerminalName, Runner)>, TerminalError> {
        let run = self.run(&[
            "list-panes".to_string(),
            "-a".to_string(),
            "-F".to_string(),
            format!("#{{session_name}}|{PANE_STATE}"),
        ])?;
        match run.code {
            Some(0) => {}
            // No server, no session: every terminal is gone.
            Some(1) if gone(&run.stderr) => {
                return Ok(terminals
                    .iter()
                    .map(|(terminal, _)| (terminal.clone(), Runner::Gone))
                    .collect());
            }
            other => {
                return Err(TerminalError(format!(
                    "list-panes -a returned {other:?}: {}",
                    run.stderr.trim()
                )));
            }
        }
        let mut states: HashMap<&str, &str> = HashMap::new();
        for line in run.stdout.lines() {
            let (session, state) = line.split_once('|').unwrap_or((line, ""));
            // A session with more than one pane is the first one's.
            states.entry(session).or_insert(state);
        }
        Ok(terminals
            .iter()
            .map(|(terminal, _)| {
                let state = states
                    .get(terminal.as_str())
                    .map_or(Runner::Gone, |state| runner_of(terminal, state));
                (terminal.clone(), state)
            })
            .collect())
    }

    fn capture_tail(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError> {
        let run = self.run_within(
            &[
                "capture-pane".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                Self::pane(terminal),
                "-S".to_string(),
                format!("-{lines}"),
            ],
            CAPTURE_LIMIT,
        )?;
        if run.code == Some(0) {
            return Ok(run.stdout);
        }
        Err(TerminalError(format!(
            "capture-pane failed: {}",
            run.stderr.trim()
        )))
    }

    /// `kill-session`, then the wait until every process of the terminal's
    /// runner has exited: kill-session returns once the session is gone,
    /// while the runner and the engine it started are still exiting on the
    /// hang-up, and can write to the workspace until they have. The caller
    /// (the Bridge's end) takes its final checkpoint only after this returns
    /// (rev-e, 028's lost line).
    fn terminate(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        // Read before the kill: the session's processes are told by it.
        // With the leader recorded at start: a session that vanished on its
        // own (tmux died, or the session ended outside kill-session) has no
        // pane to read, while its runner and engine may still be exiting,
        // or ignore the hang-up altogether (backlog P6).
        let mut leaders = self.pane_pids(terminal)?;
        if let Some(recorded) = self.recorded_leader(terminal)
            && !leaders.contains(&recorded)
        {
            leaders.push(recorded);
        }
        let run = self.run(&[
            "kill-session".to_string(),
            "-t".to_string(),
            Self::target(terminal),
        ])?;
        // A terminal that is already gone is the desired end state.
        if run.code != Some(0) && !gone(&run.stderr) {
            return Err(TerminalError(format!(
                "kill-session failed: {}",
                run.stderr.trim()
            )));
        }
        self.wait_gone(terminal, &leaders)?;
        self.forget_leader(terminal);
        Ok(())
    }

    /// The kill waits until the session's processes are gone (seconds:
    /// idle median 1.26 s, max 2.4 s at load 20; a bounded wait of up to
    /// 7 s): the Bridge runs it off its loop.
    fn terminate_waits(&self) -> bool {
        true
    }

    fn read_from(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        offset: u64,
    ) -> Result<Option<Vec<u8>>, TerminalError> {
        // A file in the workspace (the engine's exit status) is the agent's:
        // opened the safe way, from the workspace through held directories.
        let top = workspace.path().trim_end_matches('/');
        match path
            .strip_prefix(top)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            Some(relative) if !top.is_empty() => {
                read_in_workspace(std::path::Path::new(top), relative, offset)
            }
            _ => read_local(path, offset),
        }
    }

    fn write_new(
        &self,
        _workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError> {
        write_local(path, bytes)
    }
}

/// Create `path` holding `bytes` unless something is there: whether it was
/// written. The bytes go to a temporary file beside it first, made durable,
/// and a hard link puts it in place, which never replaces an existing file:
/// the file appears whole or not at all.
fn write_local(path: &str, bytes: &[u8]) -> Result<bool, TerminalError> {
    use std::io::Write;
    let target = std::path::Path::new(path);
    if target.symlink_metadata().is_ok() {
        return Ok(false);
    }
    let failed =
        |what: &str, error: std::io::Error| TerminalError(format!("{what} {path}: {error}"));
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(TerminalError(format!("{path} names no file")));
    };
    std::fs::create_dir_all(parent).map_err(|error| failed("create the directory of", error))?;
    let temporary = parent.join(format!(
        ".{}.spawn-write-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temporary);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        });
    let linked = written.and_then(|()| std::fs::hard_link(&temporary, target));
    let _ = std::fs::remove_file(&temporary);
    match linked {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(failed("write", error)),
    }
}

/// Whether tmux's error says the terminal does not exist: no such session,
/// no server on the socket (or one shutting down), or no socket at all. A socket that may not be
/// opened is not among them.
fn gone(stderr: &str) -> bool {
    stderr.contains("can't find session")
        || stderr.contains("no server running")
        || server_left(stderr)
        || (stderr.contains("error connecting") && stderr.contains("No such file or directory"))
}

/// Whether tmux's error says the call reached a server that went before it
/// read the call: one shutting down after its last session ended. Its
/// socket is still open (the server is exiting, or a pane it is forking
/// holds a copy until it execs), so the client connects, and the server
/// never accepts it.
fn server_left(stderr: &str) -> bool {
    stderr.contains("server exited unexpectedly")
}

/// At most [`READ_LIMIT`] bytes from `offset` of the file `relative` in
/// the workspace at `top`, through [`crate::agent_files::open_regular`];
/// `None` when it does not exist.
fn read_in_workspace(
    top: &std::path::Path,
    relative: &str,
    offset: u64,
) -> Result<Option<Vec<u8>>, TerminalError> {
    use std::io::{Read, Seek, SeekFrom};
    let shown = || format!("{}/{relative}", top.display());
    let opened = crate::agent_files::open_regular(top, &crate::tree_path::TreePath::from(relative))
        .map_err(|error| TerminalError(format!("open {}: {error}", shown())))?;
    let Some(crate::agent_files::Opened { file, .. }) = opened else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    let mut file = file;
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| (&mut file).take(READ_LIMIT as u64).read_to_end(&mut bytes))
        .map_err(|error| TerminalError(format!("read {}: {error}", shown())))?;
    Ok(Some(bytes))
}

/// At most [`READ_LIMIT`] bytes of a local file from `offset`; `None` when
/// it does not exist. The agent controls the path: a symlink is not followed
/// and a FIFO does not block the open (`O_NOFOLLOW | O_NONBLOCK`), and
/// anything but a regular file is refused before a byte is read.
fn read_local(path: &str, offset: u64) -> Result<Option<Vec<u8>>, TerminalError> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(TerminalError(format!("open {path}: {error}"))),
    };
    let regular = file
        .metadata()
        .map_err(|error| TerminalError(format!("stat {path}: {error}")))?
        .is_file();
    if !regular {
        return Err(TerminalError(format!("{path} is not a regular file")));
    }
    let mut bytes = Vec::new();
    let mut file = file;
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| (&mut file).take(READ_LIMIT as u64).read_to_end(&mut bytes))
        .map_err(|error| TerminalError(format!("read {path}: {error}")))?;
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::{READ_LIMIT, read_in_workspace, read_local, runner_of};
    use spawn_core::id::TerminalName;
    use spawn_core::port::Runner;
    use std::time::{Duration, Instant};

    #[test]
    fn a_pane_hosts_its_runner_only_when_this_host_launched_it_and_it_lives() {
        let terminal = TerminalName::new("spawn-exec_1").expect("valid terminal");
        let launched = "0||spawn|/bin/sh -c \"set -a; . \\\"$0\\\"; exec \\\"$@\\\"\" \
                        /run/user/1000/spawn-launch-spawn-exec_1-42-7.env /bin/spawn run -- claude";
        assert_eq!(runner_of(&terminal, launched), Runner::Running);
        // The same launch of another terminal is not this one's runner.
        let other = launched.replace("spawn-exec_1-", "spawn-exec_10-");
        assert!(matches!(runner_of(&terminal, &other), Runner::Untracked(_)));
        // A start command that holds the separator is read whole: it is the
        // last field, and the fields before it are unchanged.
        let piped = launched.replace("/bin/spawn run", "/bin/spawn run | tee");
        assert_eq!(runner_of(&terminal, &piped), Runner::Running);
        // A shell at its prompt, as a session restored after a restart is.
        let Runner::Untracked(why) = runner_of(&terminal, "0||bash|") else {
            panic!("a restored shell hosts no runner");
        };
        assert!(
            why.contains("no command") && why.contains("`bash`"),
            "{why}"
        );
        // A dead pane (remain-on-exit) hosts nothing, whatever it ran.
        let dead = launched.replacen('0', "1", 1).replacen("||", "|137|", 1);
        let Runner::Untracked(why) = runner_of(&terminal, &dead) else {
            panic!("a dead pane hosts no runner");
        };
        assert!(why.contains("dead (exit status 137)"), "{why}");
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spawn-read-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_controllers_settings_stay_out_of_tmux() {
        use std::ffi::OsStr;
        for name in [
            "SPAWN_DATABASE_URL",
            "SPAWN_SERVE_TOKEN",
            "SPAWN_TMUX_SOCKET",
            "PGPASSWORD",
        ] {
            assert!(super::scrub(OsStr::new(name)), "{name}");
        }
        for name in ["PATH", "HOME", "https_proxy", "LANG"] {
            assert!(!super::scrub(OsStr::new(name)), "{name}");
        }
    }

    /// tmux is given no locale (see the crate's documentation): it has no
    /// translations, so what Spawn reads from it is the same on a host whose
    /// locale is Chinese. Each answer that says a terminal is gone (no
    /// socket, no server on it, no such session) is word for word the one
    /// of the C locale, and read as gone.
    #[test]
    fn tmux_says_a_terminal_is_gone_alike_in_a_translated_locale() {
        let dir = scratch("tmux-locale");
        std::fs::write(dir.join("not-a-server"), b"").unwrap();
        let answer = |locale: &str, language: Option<&str>, socket: &str, start: bool| {
            let tmux = |arguments: &[&str]| {
                let words: Vec<String> = ["tmux", "-S", socket, "-f", "/dev/null"]
                    .iter()
                    .chain(arguments)
                    .map(ToString::to_string)
                    .collect();
                let mut command = super::command_of(&words);
                command
                    .env("LC_ALL", locale)
                    .env("LANG", locale)
                    .env("LC_MESSAGES", locale);
                match language {
                    Some(language) => command.env("LANGUAGE", language),
                    None => command.env_remove("LANGUAGE"),
                };
                command.output().expect("tmux is installed")
            };
            if start {
                let started = tmux(&["new-session", "-d", "-s", "spawn-locale-live"]);
                assert!(started.status.success(), "{started:?}");
            }
            let asked = tmux(&["has-session", "-t", "=spawn-locale-none"]);
            if start {
                let _ = tmux(&["kill-server"]);
            }
            assert_eq!(asked.status.code(), Some(1), "{asked:?}");
            let stderr = String::from_utf8_lossy(&asked.stderr).to_string();
            assert!(super::gone(&stderr), "{locale}: {stderr}");
            stderr
        };
        for (socket, start) in [
            ("none/socket", false),
            ("not-a-server", false),
            // A server of its own per locale: one killed is not gone yet
            // when the next would start on its socket.
            ("live", true),
        ] {
            let at = |locale: &str| {
                let name = if start {
                    format!("{socket}-{locale}")
                } else {
                    socket.to_string()
                };
                dir.join(name).to_str().unwrap().to_string()
            };
            assert_eq!(
                answer("zh_CN.UTF-8", Some("zh_CN:zh"), &at("zh"), start),
                answer("C", None, &at("c"), start),
                "{socket}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_agent_planted_fifo_or_symlink_is_refused_without_blocking() {
        let dir = scratch("special");
        let fifo = dir.join("exit-fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let started = Instant::now();
        assert!(read_local(fifo.to_str().unwrap(), 0).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the open blocked"
        );

        let link = dir.join("exit-link");
        std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
        assert!(read_local(link.to_str().unwrap(), 0).is_err());

        assert!(
            read_local(dir.join("absent").to_str().unwrap(), 0)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_read_returns_at_most_the_limit_from_the_offset() {
        let dir = scratch("limit");
        let file = dir.join("big");
        std::fs::write(&file, vec![b'x'; READ_LIMIT + 10]).unwrap();
        let path = file.to_str().unwrap();
        assert_eq!(read_local(path, 0).unwrap().unwrap().len(), READ_LIMIT);
        assert_eq!(
            read_local(path, READ_LIMIT as u64).unwrap().unwrap().len(),
            10
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A workspace file is read from the workspace through held
    /// directories: a parent directory the agent swapped for a symlink is
    /// refused, not followed out of the workspace.
    #[test]
    fn a_workspace_file_behind_a_symlinked_directory_is_refused() {
        let dir = scratch("workspace");
        let workspace = dir.join("ws");
        let outside = dir.join("outside");
        std::fs::create_dir_all(workspace.join(".spawn")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join(".spawn/exit-1"), b"0\n").unwrap();
        std::fs::write(outside.join("exit-2"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();
        assert_eq!(
            read_in_workspace(&workspace, ".spawn/exit-1", 0).unwrap(),
            Some(b"0\n".to_vec())
        );
        assert_eq!(
            read_in_workspace(&workspace, ".spawn/exit-3", 0).unwrap(),
            None
        );
        assert!(read_in_workspace(&workspace, "link/exit-2", 0).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A record names the terminal's session only while it can: the leader
    /// itself (same start time), or a pid no process has (what is left of
    /// the session is the terminal's); never a pid given anew, nor one whose
    /// start was not recorded and that a process has now. Hosts on other
    /// tmux programs or sockets never read it, and terminate forgets it.
    #[test]
    fn a_recorded_leader_names_only_the_terminal_s_own_session() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("spawn-p6-leaders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir(&base).expect("base");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).expect("0700");
        let host = super::TmuxHost::on_socket("tmux", Some(format!("p6-{}", std::process::id())))
            .with_leaders_under(base.clone());
        let other =
            super::TmuxHost::on_socket("tmux", Some(format!("p6-other-{}", std::process::id())))
                .with_leaders_under(base.clone());
        let terminal = TerminalName::new("spawn-exec_p6").expect("valid terminal");
        let write = |text: &str| {
            let directory = host.leaders_directory(true).expect("a private directory");
            std::fs::write(directory.join(terminal.as_str()), text).expect("record");
        };
        let me = std::process::id();
        let started = super::start_time(me).expect("my start time");

        host.record_leader(&terminal, me);
        assert_eq!(
            host.recorded_leader(&terminal),
            Some(me),
            "the leader itself"
        );
        assert_eq!(
            other.recorded_leader(&terminal),
            None,
            "another host's record"
        );

        write(&format!("{me} {}\n", started + 1));
        assert_eq!(host.recorded_leader(&terminal), None, "a pid given anew");
        write(&format!("{me} 0\n"));
        assert_eq!(
            host.recorded_leader(&terminal),
            None,
            "a start not recorded"
        );

        let mut child = std::process::Command::new("true").spawn().expect("true");
        let gone = child.id();
        child.wait().expect("reaped");
        write(&format!("{gone} 12345\n"));
        assert_eq!(
            host.recorded_leader(&terminal),
            Some(gone),
            "no process has it"
        );

        for bad in ["", "x 1\n", "1 5\n", "0 5\n", "7\n"] {
            write(bad);
            assert_eq!(host.recorded_leader(&terminal), None, "{bad:?}");
        }
        host.forget_leader(&terminal);
        assert_eq!(host.recorded_leader(&terminal), None, "forgotten");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A leaders directory anyone else could write is never read or
    /// written: the base, or a level below it, open to others or a symlink
    /// (another user could plant a record naming one of our sessions, which
    /// terminate SIGKILLs; rev-b on b279f33c).
    #[test]
    fn a_leaders_directory_others_could_write_is_never_used() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &std::path::Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        };
        let scratch = std::env::temp_dir().join(format!("spawn-p6-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("scratch");
        let terminal = TerminalName::new("spawn-exec_p6").expect("valid terminal");
        let me = std::process::id();

        // The base open to others.
        let base = scratch.join("open-base");
        std::fs::create_dir(&base).expect("base");
        mode(&base, 0o777);
        let host = super::TmuxHost::local("tmux").with_leaders_under(base.clone());
        assert_eq!(host.leaders_directory(true), None);
        host.record_leader(&terminal, me);
        assert_eq!(host.recorded_leader(&terminal), None);

        // A private base, its per-user level pre-made 0777 by someone else.
        let base = scratch.join("private-base");
        std::fs::create_dir(&base).expect("base");
        mode(&base, 0o700);
        let host = super::TmuxHost::local("tmux").with_leaders_under(base.clone());
        let user = base.join(format!(
            "spawn-leaders-{}",
            rustix::process::getuid().as_raw()
        ));
        std::fs::create_dir(&user).expect("planted");
        mode(&user, 0o777);
        assert_eq!(host.leaders_directory(true), None);
        host.record_leader(&terminal, me);
        assert_eq!(host.recorded_leader(&terminal), None);

        // The per-user level a symlink to a directory of "theirs".
        std::fs::remove_dir(&user).expect("unplant");
        let theirs = scratch.join("theirs");
        std::fs::create_dir(&theirs).expect("theirs");
        mode(&theirs, 0o700);
        std::os::unix::fs::symlink(&theirs, &user).expect("symlink");
        assert_eq!(host.leaders_directory(true), None);
        host.record_leader(&terminal, me);
        assert_eq!(host.recorded_leader(&terminal), None);
        assert_eq!(
            std::fs::read_dir(&theirs).expect("theirs").count(),
            0,
            "nothing written there"
        );

        // No base at all: nothing.
        std::fs::remove_file(&user).expect("unlink");
        let host = super::TmuxHost::local("tmux").with_leaders_under(scratch.join("missing"));
        assert_eq!(host.leaders_directory(true), None);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_start_time_is_read_past_a_command_with_parentheses() {
        let stat = "42 (a) (b c) S 1 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(super::start_time_of(stat), Some(987_654));
        assert_eq!(super::start_time_of("42 (x) S 1"), None);
    }
}

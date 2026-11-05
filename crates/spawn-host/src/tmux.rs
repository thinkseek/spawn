//! tmux adapter.
//!
//! tmux gives each execution a named session, a scrollback and a way to read
//! the last lines of the pane. `send-keys -l` types bytes literally, so a
//! prompt reaches the engine unchanged.
//!
//! Two deployment shapes:
//!
//! - local: tmux runs on the same machine, and the engine starts in that
//!   terminal.
//! - in workspace: tmux runs inside the execution's container, so every tmux
//!   call is wrapped by a `docker exec` prefix. Every port call names the
//!   workspace, so the host keeps no state and works the same in a new
//!   process.

use std::process::Command;
use std::sync::Arc;

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{Launch, READ_LIMIT, TerminalError, TerminalHost};
use spawn_core::shell::shell_join;

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

/// Builds the argv prefix that runs one command inside a workspace.
pub type PrefixBuilder = dyn Fn(&WorkspaceId) -> Vec<String> + Send + Sync;

/// Size of a new terminal. Claude needs width for a prompt box and height
/// for the tail window.
const WIDTH: &str = "200";
const HEIGHT: &str = "50";

/// tmux host.
pub struct TmuxHost {
    program: String,
    socket: Option<String>,
    builder: Option<Arc<PrefixBuilder>>,
}

/// One finished tmux call.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    stdout_bytes: Vec<u8>,
}

impl TmuxHost {
    /// Host whose tmux shares this machine with Spawn.
    pub fn local(program: &str) -> Self {
        Self {
            program: program.to_string(),
            socket: None,
            builder: None,
        }
    }

    /// Host whose tmux lives inside the workspace sandbox.
    pub fn in_workspaces(
        program: &str,
        builder: Arc<PrefixBuilder>,
        socket: Option<String>,
    ) -> Self {
        Self {
            program: program.to_string(),
            socket,
            builder: Some(builder),
        }
    }

    fn prefix(&self, workspace: &WorkspaceId) -> Vec<String> {
        self.builder
            .as_ref()
            .map_or_else(Vec::new, |builder| builder(workspace))
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

    fn run(&self, prefix: &[String], arguments: &[String]) -> Result<Run, TerminalError> {
        run_words(prefix, &self.words(arguments))
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
}

/// Run `words`, inside the workspace when `prefix` is not empty.
fn run_words(prefix: &[String], words: &[String]) -> Result<Run, TerminalError> {
    run_words_with_input(prefix, words, None)
}

/// [`run_words`], with `input` on the command's standard input.
fn run_words_with_input(
    prefix: &[String],
    words: &[String],
    input: Option<&[u8]>,
) -> Result<Run, TerminalError> {
    let mut command = match prefix.split_first() {
        None => {
            let mut command = Command::new(&words[0]);
            command.args(&words[1..]);
            command
        }
        Some((program, rest)) => {
            let mut command = Command::new(program);
            command.args(rest).arg(shell_join(words));
            command
        }
    };
    // The first command starts the tmux server, whose global environment
    // every pane inherits and can read with `tmux show-environment -g`.
    for (name, _) in std::env::vars_os() {
        if scrub(&name) {
            command.env_remove(name);
        }
    }
    let failed = |error: std::io::Error| TerminalError(format!("run {}: {error}", words[0]));
    let output = match input {
        None => command.output().map_err(failed)?,
        Some(input) => {
            use std::io::Write;
            let mut child = command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(failed)?;
            let written = child
                .stdin
                .take()
                .map_or(Ok(()), |mut stdin| stdin.write_all(input));
            let output = child.wait_with_output().map_err(failed)?;
            written.map_err(failed)?;
            output
        }
    };
    Ok(Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        stdout_bytes: output.stdout,
    })
}

/// Write `environment` as `NAME='value'` lines to a new file only this uid
/// can read (created exclusively, mode 600, in `XDG_RUNTIME_DIR` when there
/// is one, else the temporary directory), for a pane's shell to source.
fn write_environment(
    terminal: &TerminalName,
    environment: &[(String, String)],
) -> Result<std::path::PathBuf, TerminalError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
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
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let path = directory.join(format!(
        "spawn-launch-{}-{}-{nanos}.env",
        terminal.as_str(),
        std::process::id()
    ));
    let failed = |error: std::io::Error| {
        TerminalError(format!("write the environment of {terminal}: {error}"))
    };
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

impl TerminalHost for TmuxHost {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        let prefix = self.prefix(&launch.workspace);

        let mut arguments = vec![
            "new-session".to_string(),
            "-d".to_string(),
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
        // command line, which every local user reads in /proc: on this
        // machine it goes through a file only this uid reads, which the
        // pane's shell sources and removes before it runs the program.
        let mut environment_file = None;
        if prefix.is_empty() && !launch.environment.is_empty() {
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
            environment_file = Some(file);
        } else {
            for (key, value) in &launch.environment {
                arguments.push("-e".to_string());
                arguments.push(format!("{key}={value}"));
            }
            arguments.push("--".to_string());
        }
        arguments.push(launch.program.clone());
        arguments.extend(launch.arguments.iter().cloned());

        let run = self.run(&prefix, &arguments);
        let run = match run {
            Ok(run) if run.code == Some(0) => return Ok(()),
            Ok(run) => run,
            Err(error) => {
                if let Some(file) = &environment_file {
                    let _ = std::fs::remove_file(file);
                }
                return Err(error);
            }
        };
        // No pane ran to remove it.
        if let Some(file) = &environment_file {
            let _ = std::fs::remove_file(file);
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

    fn submit_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        let prefix = self.prefix(workspace);
        let target = Self::pane(terminal);
        // One bracketed paste, then one Enter. Typing a line and pressing
        // Enter per line would submit a multi-paragraph prompt in pieces.
        let buffer = format!("spawn-{}", terminal.as_str());
        let set = vec![
            "set-buffer".to_string(),
            "-b".to_string(),
            buffer.clone(),
            "--".to_string(),
            text.to_string(),
        ];
        let run = self.run(&prefix, &set)?;
        if run.code != Some(0) {
            return Err(TerminalError(format!(
                "set-buffer failed: {}",
                run.stderr.trim()
            )));
        }
        let paste = vec![
            "paste-buffer".to_string(),
            "-p".to_string(),
            "-d".to_string(),
            "-b".to_string(),
            buffer,
            "-t".to_string(),
            target.clone(),
        ];
        let run = self.run(&prefix, &paste)?;
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
        let run = self.run(&prefix, &enter)?;
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
        workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError> {
        let prefix = self.prefix(workspace);
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
            let run = self.run(&prefix, &send)?;
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
        workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        let prefix = self.prefix(workspace);
        let run = self.run(
            &prefix,
            &[
                "has-session".to_string(),
                "-t".to_string(),
                Self::target(terminal),
            ],
        )?;
        match run.code {
            Some(0) => Ok(true),
            Some(1) if gone(&run.stderr) => Ok(false),
            // Anything else (a socket we may not open, a failing container
            // runtime) says nothing about the terminal: ending its execution
            // on it would unmount a workspace an agent may still be using.
            other => Err(TerminalError(format!(
                "has-session returned {other:?}: {}",
                run.stderr.trim()
            ))),
        }
    }

    fn capture_tail(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError> {
        let prefix = self.prefix(workspace);
        let run = self.run(
            &prefix,
            &[
                "capture-pane".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                Self::pane(terminal),
                "-S".to_string(),
                format!("-{lines}"),
            ],
        )?;
        if run.code == Some(0) {
            return Ok(run.stdout);
        }
        Err(TerminalError(format!(
            "capture-pane failed: {}",
            run.stderr.trim()
        )))
    }

    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        let prefix = self.prefix(workspace);
        let run = self.run(
            &prefix,
            &[
                "kill-session".to_string(),
                "-t".to_string(),
                Self::target(terminal),
            ],
        )?;
        if run.code == Some(0) {
            return Ok(());
        }
        // A terminal that is already gone is the desired end state.
        if gone(&run.stderr) {
            return Ok(());
        }
        Err(TerminalError(format!(
            "kill-session failed: {}",
            run.stderr.trim()
        )))
    }

    fn read_from(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        offset: u64,
    ) -> Result<Option<Vec<u8>>, TerminalError> {
        let prefix = self.prefix(workspace);
        if prefix.is_empty() {
            return read_local(path, offset);
        }
        // Inside the workspace's container: `tail -c +N` prints from byte N
        // (1-based); a missing file exits 3 so it is told apart from failure.
        // Anything but a regular file exits 4. The read opens the file the
        // way `read_local` does (no symlink, never blocking), so a file
        // swapped for a FIFO after the check reads as nothing rather than
        // hanging; it is bounded in size (`count`) and time (`timeout`).
        let script = format!(
            "test -e {path} || test -L {path} || exit 3; \
             test -f {path} && ! test -L {path} || exit 4; \
             dd if={path} iflag=nofollow,nonblock,skip_bytes,count_bytes \
             skip={offset} count={READ_LIMIT} bs=1M status=none",
            path = spawn_core::shell::shell_quote(path)
        );
        let words = vec!["sh".to_string(), "-c".to_string(), script];
        let bounded: Vec<String> = ["timeout", "5"]
            .into_iter()
            .map(String::from)
            .chain(prefix)
            .collect();
        let run = run_words(&bounded, &words)?;
        match run.code {
            Some(0) => Ok(Some(run.stdout_bytes)),
            Some(3) => Ok(None),
            Some(4) => Err(TerminalError(format!("{path} is not a regular file"))),
            other => Err(TerminalError(format!(
                "read {path} returned {other:?}: {}",
                run.stderr.trim()
            ))),
        }
    }

    fn write_new(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError> {
        let prefix = self.prefix(workspace);
        if prefix.is_empty() {
            return write_local(path, bytes);
        }
        // Inside the workspace's container, as `write_local` does it: a
        // file already there exits 3; the bytes come on standard input.
        let script = format!(
            "test -e {path} || test -L {path} && exit 3; \
             d=$(dirname {path}) && mkdir -p \"$d\" || exit 1; \
             t=\"$d/.spawn-write-$$\"; rm -f \"$t\"; \
             cat >\"$t\" && ln \"$t\" {path}; s=$?; rm -f \"$t\"; exit $s",
            path = spawn_core::shell::shell_quote(path)
        );
        let words = vec!["sh".to_string(), "-c".to_string(), script];
        let bounded: Vec<String> = ["timeout", "60"]
            .into_iter()
            .map(String::from)
            .chain(prefix)
            .collect();
        let run = run_words_with_input(&bounded, &words, Some(bytes))?;
        match run.code {
            Some(0) => Ok(true),
            Some(3) => Ok(false),
            other => Err(TerminalError(format!(
                "write {path} returned {other:?}: {}",
                run.stderr.trim()
            ))),
        }
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
        // Asked while the server shuts down after its last session ended.
        || stderr.contains("server exited unexpectedly")
        || (stderr.contains("error connecting") && stderr.contains("No such file or directory"))
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
    use super::{READ_LIMIT, read_local};
    use std::time::{Duration, Instant};

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
}

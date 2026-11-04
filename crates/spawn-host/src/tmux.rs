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
use spawn_core::port::{Launch, TerminalError, TerminalHost};
use spawn_core::shell::shell_join;

/// Variables kept out of the tmux server's environment.
const SCRUBBED: [&str; 4] = [
    "SPAWN_DATABASE_URL",
    "SPAWN_TEST_DATABASE_URL",
    "PGPASSWORD",
    "TYPESAFE_API_KEY",
];

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
    // every pane can read with `tmux show-environment -g`. The
    // controller's own database login must not end up there.
    for secret in SCRUBBED {
        command.env_remove(secret);
    }
    let output = command
        .output()
        .map_err(|error| TerminalError(format!("run {}: {error}", words[0])))?;
    Ok(Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        stdout_bytes: output.stdout,
    })
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
        for (key, value) in &launch.environment {
            arguments.push("-e".to_string());
            arguments.push(format!("{key}={value}"));
        }
        arguments.push("--".to_string());
        arguments.push(launch.program.clone());
        arguments.extend(launch.arguments.iter().cloned());

        let run = self.run(&prefix, &arguments)?;
        if run.code == Some(0) {
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
        let script = format!(
            "test -f {path} || exit 3; tail -c +{} {path}",
            offset.saturating_add(1),
            path = spawn_core::shell::shell_quote(path)
        );
        let words = vec!["sh".to_string(), "-c".to_string(), script];
        let run = run_words(&prefix, &words)?;
        match run.code {
            Some(0) => Ok(Some(run.stdout_bytes)),
            Some(3) => Ok(None),
            other => Err(TerminalError(format!(
                "read {path} returned {other:?}: {}",
                run.stderr.trim()
            ))),
        }
    }
}

/// Whether tmux's error says the terminal does not exist: no such session,
/// no server on the socket, or no socket at all. A socket that may not be
/// opened is not among them.
fn gone(stderr: &str) -> bool {
    stderr.contains("can't find session")
        || stderr.contains("no server running")
        || (stderr.contains("error connecting") && stderr.contains("No such file or directory"))
}

/// Bytes of a local file from `offset`; `None` when it does not exist.
fn read_local(path: &str, offset: u64) -> Result<Option<Vec<u8>>, TerminalError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(TerminalError(format!("open {path}: {error}"))),
    };
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_to_end(&mut bytes))
        .map_err(|error| TerminalError(format!("read {path}: {error}")))?;
    Ok(Some(bytes))
}

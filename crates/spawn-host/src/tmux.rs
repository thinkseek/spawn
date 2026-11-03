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
//! - in workspace: tmux runs inside the sandbox, so every tmux call is
//!   wrapped by the LayerFS `workspace exec` prefix of the terminal's
//!   workspace. Every port call names the workspace, so the host keeps no
//!   state and works the same in a new process.

use std::process::Command;
use std::sync::Arc;

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{Launch, TerminalError, TerminalHost};
use spawn_core::shell::shell_join;

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
        let words = self.words(arguments);
        let mut command = match prefix.split_first() {
            None => {
                let mut command = Command::new(&words[0]);
                command.args(&words[1..]);
                command
            }
            Some((program, rest)) => {
                let mut command = Command::new(program);
                command.args(rest).arg(shell_join(&words));
                command
            }
        };
        let output = command
            .output()
            .map_err(|error| TerminalError(format!("run {}: {error}", self.program)))?;
        Ok(Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
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
        for line in text.lines() {
            let literal = vec![
                "send-keys".to_string(),
                "-t".to_string(),
                target.clone(),
                "-l".to_string(),
                "--".to_string(),
                line.to_string(),
            ];
            let run = self.run(&prefix, &literal)?;
            if run.code != Some(0) {
                return Err(TerminalError(format!(
                    "send-keys failed: {}",
                    run.stderr.trim()
                )));
            }
            let enter = vec![
                "send-keys".to_string(),
                "-t".to_string(),
                target.clone(),
                "Enter".to_string(),
            ];
            let run = self.run(&prefix, &enter)?;
            if run.code != Some(0) {
                return Err(TerminalError(format!(
                    "send-keys Enter failed: {}",
                    run.stderr.trim()
                )));
            }
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
            Some(1) => Ok(false),
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
        let stderr = run.stderr.trim();
        // A terminal that is already gone is the desired end state.
        if stderr.contains("can't find session")
            || stderr.contains("no server running")
            || stderr.contains("error connecting")
        {
            return Ok(());
        }
        Err(TerminalError(format!("kill-session failed: {stderr}")))
    }
}

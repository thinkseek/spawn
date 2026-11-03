use spawn_process::{CommandLine, ProcessRunner, SystemRunner};
use spawn_types::identifiers::TerminalName;

use crate::{LaunchSpec, PaneCapture, TerminalError, TerminalHost};

/// Terminal host backed by the `tmux` command.
pub struct TmuxHost<R: ProcessRunner> {
    runner: R,
    executable: String,
}

impl TmuxHost<SystemRunner> {
    /// Create a host using the default `tmux` executable.
    #[must_use]
    pub fn new() -> Self {
        Self::with_executable("tmux")
    }

    /// Create a host with a specific executable path.
    #[must_use]
    pub fn with_executable(executable: impl Into<String>) -> Self {
        Self {
            runner: SystemRunner,
            executable: executable.into(),
        }
    }
}

impl Default for TmuxHost<SystemRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: ProcessRunner> TmuxHost<R> {
    /// Create a host with an injected process runner.
    pub fn with_runner(executable: impl Into<String>, runner: R) -> Self {
        Self {
            runner,
            executable: executable.into(),
        }
    }

    fn tmux(&self, arguments: Vec<String>) -> CommandLine {
        let mut command = CommandLine::new(self.executable.clone());
        for argument in arguments {
            command = command.arg(argument);
        }
        command
    }

    fn run_ok(&self, command: &CommandLine) -> Result<spawn_process::ProcessOutput, TerminalError> {
        let output = self.runner.run(command)?;
        if !output.succeeded() {
            return Err(TerminalError::Rejected(format!(
                "{}: {}",
                command.program,
                output.stderr.trim()
            )));
        }
        Ok(output)
    }

    fn shell_command(spec: &LaunchSpec) -> String {
        let mut parts = spec.command_prefix.clone();
        parts.push(spec.program.clone());
        parts.extend(spec.arguments.iter().cloned());
        let joined = parts
            .iter()
            .map(|part| shell_quote(part))
            .collect::<Vec<_>>()
            .join(" ");
        let mut exports = String::new();
        for (key, value) in &spec.environment {
            exports.push_str("export ");
            exports.push_str(key);
            exports.push('=');
            exports.push_str(&shell_quote(value));
            exports.push_str("; ");
        }
        format!(
            "cd {} && {}{}",
            shell_quote(&spec.working_directory),
            exports,
            joined
        )
    }
}

fn shell_quote(value: &str) -> String {
    if value.chars().all(|character| {
        character.is_ascii_alphanumeric()
            || matches!(character, '/' | '.' | '_' | '-' | ':' | ',' | '=')
    }) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

impl<R: ProcessRunner> TerminalHost for TmuxHost<R> {
    fn start(&self, spec: &LaunchSpec) -> Result<(), TerminalError> {
        let command = self.tmux(vec![
            "new-session".to_string(),
            "-d".to_string(),
            "-s".to_string(),
            spec.name.as_str().to_string(),
            Self::shell_command(spec),
        ]);
        self.run_ok(&command)?;
        Ok(())
    }

    fn send_keys(&self, name: &TerminalName, keys: &str, enter: bool) -> Result<(), TerminalError> {
        let mut arguments = vec![
            "send-keys".to_string(),
            "-t".to_string(),
            name.as_str().to_string(),
            keys.to_string(),
        ];
        if enter {
            arguments.push("Enter".to_string());
        }
        self.run_ok(&self.tmux(arguments))?;
        Ok(())
    }

    fn capture(&self, name: &TerminalName) -> Result<PaneCapture, TerminalError> {
        let output = self.run_ok(&self.tmux(vec![
            "capture-pane".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            name.as_str().to_string(),
        ]))?;
        Ok(PaneCapture {
            text: output.stdout,
        })
    }

    fn is_alive(&self, name: &TerminalName) -> Result<bool, TerminalError> {
        let output = self.runner.run(&self.tmux(vec![
            "has-session".to_string(),
            "-t".to_string(),
            name.as_str().to_string(),
        ]))?;
        Ok(output.succeeded())
    }

    fn terminate(&self, name: &TerminalName) -> Result<(), TerminalError> {
        self.run_ok(&self.tmux(vec![
            "kill-session".to_string(),
            "-t".to_string(),
            name.as_str().to_string(),
        ]))?;
        Ok(())
    }
}

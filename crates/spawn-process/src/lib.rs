//! Process execution port.
//!
//! Adapters for external CLIs (LayerFS, tmux) depend on this small trait so
//! tests can replace the real executable with a scripted fake.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use thiserror::Error;

/// A fully specified process invocation without shell interpolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    /// Executable path or name resolved through `PATH`.
    pub program: String,
    /// Arguments passed verbatim, without shell quoting.
    pub arguments: Vec<String>,
    /// Working directory applied when present.
    pub working_directory: Option<PathBuf>,
    /// Extra environment variables for the child process.
    pub environment: HashMap<String, String>,
}

impl CommandLine {
    /// Create a command with only a program name.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            working_directory: None,
            environment: HashMap::new(),
        }
    }

    /// Append one argument and return the builder.
    #[must_use]
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    /// Append arguments and return the builder.
    #[must_use]
    pub fn args(mut self, arguments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// Set the working directory and return the builder.
    #[must_use]
    pub fn working_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }

    /// Add one environment variable and return the builder.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }
}

/// Captured result of a finished process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    /// OS exit code. A value of `None` means termination by signal.
    pub exit_code: Option<i32>,
    /// Captured standard output as UTF-8 text.
    pub stdout: String,
    /// Captured standard error as UTF-8 text.
    pub stderr: String,
}

impl ProcessOutput {
    /// Return whether the process exited with code zero.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Process-level failures distinct from non-zero protocol exit codes.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The executable could not be spawned.
    #[error("failed to spawn {program}: {source}")]
    Spawn {
        /// Requested program.
        program: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// The process ran but its output could not be awaited or read.
    #[error("failed to wait for {program}: {source}")]
    Wait {
        /// Requested program.
        program: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
}

/// Executes a [`CommandLine`] and captures its output.
pub trait ProcessRunner: Send + Sync {
    /// Run the command to completion.
    ///
    /// Non-zero exit codes are returned inside [`ProcessOutput`]. Only
    /// spawn/wait failures become [`ProcessError`].
    fn run(&self, command: &CommandLine) -> Result<ProcessOutput, ProcessError>;
}

/// Runner backed by [`std::process::Command`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl ProcessRunner for SystemRunner {
    fn run(&self, command: &CommandLine) -> Result<ProcessOutput, ProcessError> {
        let mut child = Command::new(&command.program);
        child.args(&command.arguments);
        if let Some(directory) = &command.working_directory {
            child.current_dir(directory);
        }
        for (key, value) in &command.environment {
            child.env(key, value);
        }
        let output = child.output().map_err(|source| ProcessError::Spawn {
            program: command.program.clone(),
            source,
        })?;
        Ok(ProcessOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

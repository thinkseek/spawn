//! LayerFS CLI adapter.
//!
//! One `layerfs` call is one `@layerfs/agent` call. Exit code 0 succeeds, 2 is
//! a usage error (nothing was called), and 125 is a refusal or a failure with
//! a JSON error on standard error. Workspace identifiers travel as
//! `<sandbox>:<path>`, and the CLI rejects anything whose colon is not
//! followed by `/`.

use std::process::Command;

use serde_json::Value;
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{Commit, WorkspaceError, WorkspaceStore};

/// Exit code of a refusal or a failure.
const REFUSED: i32 = 125;
/// Exit code of a usage error.
const USAGE: i32 = 2;

/// `layerfs` in one project.
#[derive(Debug, Clone)]
pub struct LayerFsCli {
    program: String,
    project: String,
}

impl LayerFsCli {
    /// Adapter for one executable and one project.
    pub fn new(program: &str, project: &str) -> Self {
        Self {
            program: program.to_string(),
            project: project.to_string(),
        }
    }

    /// argv prefix that runs one shell command inside a workspace.
    ///
    /// LayerFS has no interactive exec, so a terminal is hosted by a tmux
    /// server that lives inside the sandbox: every tmux call goes through
    /// this prefix.
    pub fn exec_prefix(&self, workspace: &WorkspaceId) -> Vec<String> {
        vec![
            self.program.clone(),
            "--project".to_string(),
            self.project.clone(),
            "workspace".to_string(),
            "exec".to_string(),
            workspace.as_str().to_string(),
            "--stream".to_string(),
            "--".to_string(),
        ]
    }

    fn run(&self, arguments: &[String]) -> Result<String, WorkspaceError> {
        let output = Command::new(&self.program)
            .arg("--project")
            .arg(&self.project)
            .args(arguments)
            .output()
            .map_err(|error| {
                WorkspaceError::Unavailable(format!("run {}: {error}", self.program))
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        match output.status.code() {
            Some(0) => Ok(stdout),
            Some(REFUSED) => Err(refusal(&stderr)),
            Some(USAGE) => Err(WorkspaceError::Unavailable(format!(
                "usage error, nothing was called: {stderr}"
            ))),
            Some(code) => Err(WorkspaceError::Unavailable(format!(
                "exit {code}: {stderr}"
            ))),
            None => Err(WorkspaceError::Unavailable(
                "process was killed".to_string(),
            )),
        }
    }

    fn object(&self, arguments: &[String]) -> Result<Value, WorkspaceError> {
        let text = self.run(arguments)?;
        serde_json::from_str(&text)
            .map_err(|error| WorkspaceError::Unavailable(format!("answer is not JSON: {error}")))
    }
}

/// Structured refusal from the JSON error on standard error.
fn refusal(stderr: &str) -> WorkspaceError {
    let Ok(value) = serde_json::from_str::<Value>(stderr) else {
        return WorkspaceError::Unavailable(format!("refused without JSON detail: {stderr}"));
    };
    let inner = value.get("error").unwrap_or(&value);
    let code = inner
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_string();
    let message = inner
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(stderr)
        .to_string();
    WorkspaceError::Refused { code, message }
}

/// Commit identifier of a reply: `commit.id`, then `commit`, then `id`.
fn commit_id(value: &Value) -> Option<String> {
    let commit = value.get("commit");
    let text = match commit {
        Some(Value::Object(object)) => object.get("id").and_then(Value::as_str),
        Some(Value::String(text)) => Some(text.as_str()),
        _ => value.get("id").and_then(Value::as_str),
    };
    text.map(str::to_string)
}

fn is_refusal(error: &WorkspaceError, codes: &[&str]) -> bool {
    matches!(error, WorkspaceError::Refused { code, .. } if codes.contains(&code.as_str()))
}

impl WorkspaceStore for LayerFsCli {
    fn project(&self) -> &str {
        &self.project
    }

    fn ensure_branch(&self, branch: &BranchName) -> Result<(), WorkspaceError> {
        let arguments = [
            "branch".to_string(),
            "fork".to_string(),
            branch.as_str().to_string(),
        ];
        match self.run(&arguments) {
            Ok(_) => Ok(()),
            // A fork of an existing branch is the desired state. A fork whose
            // source is missing means the project has no base branch yet, and
            // LayerFS then creates the branch from the default ref.
            Err(error) if is_refusal(&error, &["NameTaken", "Exists"]) => Ok(()),
            Err(error) if is_refusal(&error, &["NoSuchBranch"]) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        let sandbox = self.object(&["sandbox".to_string(), "create".to_string()])?;
        let sandbox = sandbox
            .get("sandbox")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                WorkspaceError::Unavailable("sandbox create returned no sandbox".to_string())
            })?
            .to_string();
        let mounted = self.object(&[
            "workspace".to_string(),
            "mount".to_string(),
            sandbox.clone(),
            branch.as_str().to_string(),
        ])?;
        let workspace = mounted.get("ws").and_then(Value::as_str).ok_or_else(|| {
            WorkspaceError::Unavailable(format!("mount of {branch} returned no ws"))
        })?;
        WorkspaceId::new(workspace).map_err(|error| WorkspaceError::Unavailable(error.to_string()))
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        let arguments = [
            "workspace".to_string(),
            "commit".to_string(),
            workspace.as_str().to_string(),
            "-m".to_string(),
            message.to_string(),
        ];
        match self.run(&arguments) {
            Ok(text) => {
                let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let up_to_date = value
                    .get("upToDate")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match (up_to_date, commit_id(&value)) {
                    (true, _) | (false, None) => Ok(Commit::Unchanged),
                    (false, Some(id)) => CommitId::new(id)
                        .map(Commit::Created)
                        .map_err(|error| WorkspaceError::Unavailable(error.to_string())),
                }
            }
            Err(error) if is_refusal(&error, &["NoCommit"]) => Ok(Commit::Unchanged),
            Err(error) => Err(error),
        }
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        let arguments = [
            "workspace".to_string(),
            "unmount".to_string(),
            workspace.as_str().to_string(),
        ];
        self.run(&arguments)?;
        // The sandbox was created for this workspace. Closing it keeps the
        // machine from filling with idle containers; another live mount is a
        // reason to leave it alone.
        let close = [
            "sandbox".to_string(),
            "close".to_string(),
            workspace.sandbox().to_string(),
        ];
        match self.run(&close) {
            Ok(_) => Ok(()),
            Err(error)
                if matches!(error, WorkspaceError::Unavailable(_))
                    || is_refusal(&error, &["MountsActive", "NoSuchSandbox"]) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

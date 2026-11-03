use serde::Deserialize;
use serde_json::Value;

use spawn_process::{CommandLine, ProcessError, ProcessRunner};
use spawn_types::identifiers::{BranchName, CommitId};

use crate::port::{
    ArtifactError, ArtifactStore, BranchForkOutcome, CommitOutcome, CommitPage, CommitSummary,
    MergeOutcome, SandboxId, WorkspaceMount,
};

/// Exit code LayerFS uses for structured refusals.
const REFUSED_EXIT_CODE: i32 = 125;

#[derive(Debug, Deserialize)]
struct BranchDto {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SandboxDto {
    sandbox: u64,
}

#[derive(Debug, Deserialize)]
struct MountDto {
    sandbox: u64,
    path: String,
    image: String,
}

#[derive(Debug, Deserialize)]
struct CommitDto {
    id: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct CommitListDto {
    items: Vec<CommitDto>,
    #[serde(default)]
    has_more: bool,
}

#[derive(Debug, Deserialize)]
struct RefusalDto {
    code: String,
    message: String,
    #[serde(default)]
    detail: Option<Value>,
}

/// CLI-backed [`ArtifactStore`] implementation.
pub struct LayerFsCli<R: ProcessRunner> {
    runner: R,
    executable: String,
}

impl LayerFsCli<spawn_process::SystemRunner> {
    /// Create a client using the default `layerfs` executable name.
    #[must_use]
    pub fn new() -> Self {
        Self::with_executable("layerfs")
    }

    /// Create a client using a specific executable path.
    #[must_use]
    pub fn with_executable(executable: impl Into<String>) -> Self {
        Self {
            runner: spawn_process::SystemRunner,
            executable: executable.into(),
        }
    }
}

impl Default for LayerFsCli<spawn_process::SystemRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: ProcessRunner> LayerFsCli<R> {
    /// Create a client with an injected runner, primarily for tests.
    pub fn with_runner(executable: impl Into<String>, runner: R) -> Self {
        Self {
            runner,
            executable: executable.into(),
        }
    }

    fn base(&self) -> CommandLine {
        CommandLine::new(self.executable.clone()).arg("--json")
    }

    fn execute<T>(&self, command: &CommandLine) -> Result<T, ArtifactError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let output = self
            .runner
            .run(command)
            .map_err(|error| map_process_error(&error))?;
        if output.exit_code == Some(REFUSED_EXIT_CODE) {
            return Err(parse_refusal(&output.stdout));
        }
        if !output.succeeded() {
            return Err(ArtifactError::Unavailable(format!(
                "exit {:?}: {}",
                output.exit_code, output.stderr
            )));
        }
        serde_json::from_str::<T>(output.stdout.trim()).map_err(|error| {
            ArtifactError::InvalidOutput(format!("{error}; body={}", output.stdout.trim()))
        })
    }
}

fn map_process_error(error: &ProcessError) -> ArtifactError {
    ArtifactError::Unavailable(error.to_string())
}

fn parse_refusal(body: &str) -> ArtifactError {
    match serde_json::from_str::<RefusalDto>(body.trim()) {
        Ok(refusal) => ArtifactError::Refused {
            code: refusal.code,
            message: refusal.message,
            detail: refusal.detail,
        },
        Err(_) => ArtifactError::InvalidOutput(format!("refusal body: {body}")),
    }
}

fn branch(value: String) -> Result<BranchName, ArtifactError> {
    BranchName::new(value).map_err(|error| ArtifactError::InvalidOutput(error.to_string()))
}

fn commit(value: String) -> Result<CommitId, ArtifactError> {
    CommitId::new(value).map_err(|error| ArtifactError::InvalidOutput(error.to_string()))
}

impl<R: ProcessRunner> ArtifactStore for LayerFsCli<R> {
    fn ensure_branch(
        &self,
        project: &str,
        source: &BranchName,
        target: &BranchName,
    ) -> Result<BranchForkOutcome, ArtifactError> {
        let command = self
            .base()
            .args(["branch", "fork", project])
            .arg("--from")
            .arg(source.as_str())
            .arg("--name")
            .arg(target.as_str());
        // Success prints the created Branch record directly (no kind envelope);
        // an existing name comes back as a structured refusal on stderr.
        match self.execute::<BranchDto>(&command) {
            Ok(dto) => Ok(BranchForkOutcome::Forked(branch(dto.name)?)),
            // LayerFS reports an existing branch as a structured refusal.
            // Reusing the branch is the desired idempotent outcome.
            Err(ArtifactError::Refused { code, .. }) if code == "NameTaken" => {
                Ok(BranchForkOutcome::NameTaken(target.clone()))
            }
            Err(error) => Err(error),
        }
    }

    fn create_sandbox(&self, project: &str, image: &str) -> Result<SandboxId, ArtifactError> {
        let command = self
            .base()
            .args(["sandbox", "create", project])
            .arg("--image")
            .arg(image);
        let dto: SandboxDto = self.execute(&command)?;
        Ok(SandboxId::new(dto.sandbox))
    }

    fn mount(
        &self,
        project: &str,
        branch_name: &BranchName,
        sandbox: SandboxId,
        path: &str,
    ) -> Result<WorkspaceMount, ArtifactError> {
        let command = self
            .base()
            .args(["workspace", "mount", project])
            .arg("--branch")
            .arg(branch_name.as_str())
            .arg("--sandbox")
            .arg(sandbox.value().to_string())
            .arg("--path")
            .arg(path);
        let dto: MountDto = self.execute(&command)?;
        Ok(WorkspaceMount {
            sandbox: SandboxId::new(dto.sandbox),
            path: dto.path,
            image: dto.image,
        })
    }

    fn commit(
        &self,
        mount: &WorkspaceMount,
        message: &str,
    ) -> Result<CommitOutcome, ArtifactError> {
        let command = self
            .base()
            .args(["workspace", "commit"])
            .arg("--sandbox")
            .arg(mount.sandbox.value().to_string())
            .arg("--path")
            .arg(mount.path.clone())
            .arg("--message")
            .arg(message);
        // A dirty workspace returns the new Commit record. An unchanged workspace
        // is the structured refusal `NoCommit`, mapped to the empty outcome.
        match self.execute::<CommitDto>(&command) {
            Ok(dto) => Ok(CommitOutcome::Committed(commit(dto.id)?)),
            Err(ArtifactError::Refused { code, .. }) if code == "NoCommit" => {
                Ok(CommitOutcome::NoChanges)
            }
            Err(error) => Err(error),
        }
    }

    fn unmount(&self, mount: &WorkspaceMount, discard: bool) -> Result<(), ArtifactError> {
        let mut command = self
            .base()
            .args(["workspace", "unmount"])
            .arg("--sandbox")
            .arg(mount.sandbox.value().to_string())
            .arg("--path")
            .arg(mount.path.clone());
        if discard {
            command = command.arg("--discard");
        }
        let _envelope: Value = self.execute(&command)?;
        Ok(())
    }

    fn list_commits(
        &self,
        project: &str,
        branch_name: &BranchName,
        limit: u32,
        offset: u32,
    ) -> Result<CommitPage, ArtifactError> {
        let command = self
            .base()
            .args(["branch", "list_commits", project])
            .arg("--branch")
            .arg(branch_name.as_str())
            .arg("--limit")
            .arg(limit.to_string())
            .arg("--offset")
            .arg(offset.to_string());
        let dto: CommitListDto = self.execute(&command)?;
        let items = dto
            .items
            .into_iter()
            .map(|item| {
                Ok(CommitSummary {
                    commit: commit(item.id)?,
                    message: item.message,
                    created_at: item.created_at,
                })
            })
            .collect::<Result<Vec<_>, ArtifactError>>()?;
        Ok(CommitPage {
            items,
            has_more: dto.has_more,
        })
    }

    fn merge(
        &self,
        project: &str,
        source: &BranchName,
        target: &BranchName,
    ) -> Result<MergeOutcome, ArtifactError> {
        let command = self
            .base()
            .args(["branch", "merge", project])
            .arg("--from")
            .arg(source.as_str())
            .arg("--into")
            .arg(target.as_str());
        // Success returns the merge Commit record. A content clash is the
        // structured refusal `Conflict`, whose detail lists the conflict paths.
        match self.execute::<CommitDto>(&command) {
            Ok(dto) => Ok(MergeOutcome::Merged(commit(dto.id)?)),
            Err(ArtifactError::Refused {
                code,
                detail: Some(detail),
                ..
            }) if code == "Conflict" => {
                let paths = detail
                    .get("conflicts")
                    .and_then(Value::as_array)
                    .map(|conflicts| {
                        conflicts
                            .iter()
                            .filter_map(|value| value.get("path").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(MergeOutcome::Conflict { paths })
            }
            Err(error) => Err(error),
        }
    }
}

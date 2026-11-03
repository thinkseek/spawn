use thiserror::Error;

use spawn_types::identifiers::{BranchName, CommitId, MountId};

/// Numeric LayerFS sandbox identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SandboxId(pub u64);

impl SandboxId {
    /// Construct from a raw number.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric identifier.
    #[must_use]
    pub fn value(self) -> u64 {
        self.0
    }
}

/// A mounted workspace inside one sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMount {
    /// Owning sandbox.
    pub sandbox: SandboxId,
    /// Absolute in-sandbox mount path.
    pub path: String,
    /// Image the sandbox runs.
    pub image: String,
}

impl WorkspaceMount {
    /// Build the stable mount identifier used in events and terminal specs.
    #[must_use]
    pub fn mount_id(&self) -> MountId {
        MountId::new(format!("{}:{}", self.sandbox.value(), self.path))
            .expect("mount id components are validated labels")
    }
}

/// Result of a branch fork attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchForkOutcome {
    /// A new branch was created.
    Forked(BranchName),
    /// The branch already existed and is safe to reuse.
    NameTaken(BranchName),
}

/// Result of a workspace commit attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// LayerFS created a new commit.
    Committed(CommitId),
    /// The workspace had no changes and no commit was created.
    NoChanges,
}

/// One commit entry in branch history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitSummary {
    /// Commit identifier.
    pub commit: CommitId,
    /// Commit message.
    pub message: String,
    /// ISO timestamp reported by LayerFS.
    pub created_at: String,
}

/// One page of commit history.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitPage {
    /// Commit entries in storage order.
    pub items: Vec<CommitSummary>,
    /// Whether another page exists.
    pub has_more: bool,
}

/// Result of a branch merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Merge completed with a new merge commit.
    Merged(CommitId),
    /// Merge stopped and reported structured conflicts.
    Conflict {
        /// Relative conflict paths returned by LayerFS.
        paths: Vec<String>,
    },
}

/// Failures declared by the artifact port.
#[derive(Debug, Error)]
pub enum ArtifactError {
    /// LayerFS refused an operation with a structured error body.
    #[error("layerfs refused operation {code}: {message}")]
    Refused {
        /// Stable error code.
        code: String,
        /// Human-readable message.
        message: String,
        /// Structured refusal detail, for example conflict paths.
        detail: Option<serde_json::Value>,
    },
    /// Output could not be parsed as the documented JSON contract.
    #[error("invalid layerfs output: {0}")]
    InvalidOutput(String),
    /// The process could not be executed.
    #[error("layerfs unavailable: {0}")]
    Unavailable(String),
}

/// Branch and workspace operations required by Agent Bridge.
pub trait ArtifactStore: Send + Sync {
    /// Fork a branch when absent; treat `NameTaken` as success.
    fn ensure_branch(
        &self,
        project: &str,
        source: &BranchName,
        target: &BranchName,
    ) -> Result<BranchForkOutcome, ArtifactError>;

    /// Create a reusable sandbox for one project.
    fn create_sandbox(&self, project: &str, image: &str) -> Result<SandboxId, ArtifactError>;

    /// Mount a branch into a sandbox.
    fn mount(
        &self,
        project: &str,
        branch: &BranchName,
        sandbox: SandboxId,
        path: &str,
    ) -> Result<WorkspaceMount, ArtifactError>;

    /// Commit a mounted workspace when it is dirty.
    fn commit(&self, mount: &WorkspaceMount, message: &str)
    -> Result<CommitOutcome, ArtifactError>;

    /// Unmount a workspace, optionally discarding changes.
    fn unmount(&self, mount: &WorkspaceMount, discard: bool) -> Result<(), ArtifactError>;

    /// Read one page of branch history.
    fn list_commits(
        &self,
        project: &str,
        branch: &BranchName,
        limit: u32,
        offset: u32,
    ) -> Result<CommitPage, ArtifactError>;

    /// Merge one branch into another.
    fn merge(
        &self,
        project: &str,
        source: &BranchName,
        target: &BranchName,
    ) -> Result<MergeOutcome, ArtifactError>;
}

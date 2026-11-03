//! Agent Bridge orchestration.
//!
//! The bridge wires storage, workspace and terminal ports. It owns no native
//! process logic: engine payloads are normalized in `spawn-engine`, files are
//! persisted in `spawn-layerfs`, and terminals run in `spawn-terminal`.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
mod directory;
mod policy;

use std::sync::{Arc, Mutex};

use spawn_engine::NormalizedEvent;
use spawn_layerfs::{ArtifactError, ArtifactStore, CommitOutcome, SandboxId, WorkspaceMount};
use spawn_store::StoreError;
use spawn_terminal::{LaunchSpec, TerminalError, TerminalHost};
use spawn_types::clock::{Clock, Timestamp};
use spawn_types::engine::Engine;
use spawn_types::event::{
    AppendOutcome, ArtifactRef, CheckpointReason, EventKind, EventSource, GlobalPosition, NewEvent,
    SessionSeq,
};
use spawn_types::identifiers::{
    AgentId, BranchName, CommitId, ExecutionId, SessionId, TerminalName,
};
use thiserror::Error;

pub use directory::{RecoveredBinding, SessionBinding, SessionDirectory};
pub use policy::CheckpointPolicy;

/// Errors surfaced across bridge orchestration.
#[derive(Debug, Error)]
pub enum BridgeError {
    /// Storage port failure.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Workspace port failure.
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    /// Terminal port failure.
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    /// A required binding was missing from the session directory.
    #[error("missing session binding: {0}")]
    MissingBinding(String),
}

/// Parameters needed to start one execution.
#[derive(Debug, Clone)]
pub struct StartExecutionRequest {
    /// Engine to launch.
    pub engine: Engine,
    /// LayerFS project.
    pub project: String,
    /// Working branch.
    pub branch: BranchName,
    /// Container image for the sandbox.
    pub image: String,
    /// In-sandbox mount path.
    pub mount_path: String,
    /// Initial prompt, possibly empty.
    pub prompt: String,
    /// Spawn CLI command used by engine hooks.
    pub hook_forwarder: String,
    /// Optional container exec prefix for the terminal.
    pub command_prefix: Vec<String>,
}

/// Aggregated result of starting an execution.
#[derive(Debug, Clone)]
pub struct StartedExecution {
    /// New execution identifier.
    pub execution_id: ExecutionId,
    /// Mounted workspace.
    pub mount: WorkspaceMount,
    /// Terminal session name.
    pub terminal_name: TerminalName,
}

/// Agent bridge with injected ports.
pub struct AgentBridge {
    log: Arc<dyn spawn_store::EventLog>,
    artifacts: Arc<dyn ArtifactStore>,
    terminals: Arc<dyn TerminalHost>,
    clock: Arc<dyn Clock>,
    directory: Mutex<SessionDirectory>,
}

impl AgentBridge {
    /// Construct a bridge with its ports.
    #[must_use]
    pub fn new(
        log: Arc<dyn spawn_store::EventLog>,
        artifacts: Arc<dyn ArtifactStore>,
        terminals: Arc<dyn TerminalHost>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            log,
            artifacts,
            terminals,
            clock,
            directory: Mutex::new(SessionDirectory::default()),
        }
    }

    /// Ensure the branch for a session exists. Idempotent across restarts.
    ///
    /// # Errors
    /// Propagates workspace and storage errors.
    pub fn open_session(
        &self,
        agent_id: &AgentId,
        session_id: &SessionId,
        project: &str,
        source: &BranchName,
        target: &BranchName,
    ) -> Result<(), BridgeError> {
        self.artifacts.ensure_branch(project, source, target)?;
        let now = self.clock.now();
        self.log.append(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            execution_id: None,
            source: EventSource::Bridge,
            source_event_id: Some(format!("bridge:open:{}", session_id.as_str())),
            kind: EventKind::RelationLinked {
                parent_agent_id: agent_id.clone(),
                parent_session_id: session_id.clone(),
            },
            artifact: None,
            occurred_at: Some(now),
        })?;
        Ok(())
    }

    /// Create the workspace and terminal for one new execution.
    ///
    /// # Errors
    /// Propagates port failures and identifier validation errors.
    pub fn start_execution(
        &self,
        agent_id: &AgentId,
        session_id: &SessionId,
        execution_id: ExecutionId,
        request: &StartExecutionRequest,
    ) -> Result<StartedExecution, BridgeError> {
        let sandbox: SandboxId = self
            .artifacts
            .create_sandbox(&request.project, &request.image)?;
        let mount = self.artifacts.mount(
            &request.project,
            &request.branch,
            sandbox,
            &request.mount_path,
        )?;
        let launch = spawn_engine::build_launch(
            request.engine,
            &request.mount_path,
            &request.prompt,
            None,
            &request.hook_forwarder,
        )
        .expect("mount path validated by caller");
        let terminal_name =
            TerminalName::new(format!("spawn-{}", execution_id.as_str())).map_err(|error| {
                BridgeError::MissingBinding(format!("terminal name invalid: {error}"))
            })?;
        let spec = LaunchSpec {
            name: terminal_name.clone(),
            program: launch.program,
            arguments: launch.arguments,
            working_directory: request.mount_path.clone(),
            command_prefix: request.command_prefix.clone(),
            environment: launch.environment,
        };
        self.terminals.start(&spec)?;

        let now = self.clock.now();
        self.log.append(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            execution_id: Some(execution_id.clone()),
            source: EventSource::Bridge,
            source_event_id: Some(format!("bridge:start:{}", execution_id.as_str())),
            kind: EventKind::ExecutionStarted {
                engine: request.engine,
                terminal_name: terminal_name.as_str().to_string(),
                mount_id: mount.mount_id().as_str().to_string(),
            },
            artifact: None,
            occurred_at: Some(now),
        })?;

        self.directory
            .lock()
            .expect("directory lock is not poisoned")
            .register(SessionBinding {
                agent_id: agent_id.clone(),
                session_id: session_id.clone(),
                execution_id: execution_id.clone(),
                engine: request.engine,
                project: request.project.clone(),
                branch: request.branch.clone(),
                mount: mount.clone(),
                terminal_name: terminal_name.clone(),
                native_session_id: None,
            });

        Ok(StartedExecution {
            execution_id,
            mount,
            terminal_name,
        })
    }

    /// Append a normalized native event and checkpoint when policy requires it.
    ///
    /// # Errors
    /// Propagates storage and workspace errors.
    pub fn ingest(
        &self,
        agent_id: &AgentId,
        session_id: &SessionId,
        execution_id: &ExecutionId,
        source: EventSource,
        event: NormalizedEvent,
    ) -> Result<AppendOutcome, BridgeError> {
        let now = self.clock.now();
        let outcome = self.log.append(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            execution_id: Some(execution_id.clone()),
            source,
            source_event_id: event.source_event_id,
            kind: event.kind.clone(),
            artifact: None,
            occurred_at: Some(now),
        })?;

        if let Some(reason) = CheckpointPolicy::reason_for(&event.kind)
            && let Some(binding) = self.binding(session_id)
        {
            self.checkpoint(&binding, reason)?;
        }
        Ok(outcome)
    }

    /// Persist pending workspace changes and record one commit event.
    ///
    /// The commit id is reused as the event dedup key, so retrying after a
    /// failed append cannot double-record the same commit.
    ///
    /// # Errors
    /// Propagates workspace and storage errors.
    pub fn checkpoint(
        &self,
        binding: &SessionBinding,
        reason: CheckpointReason,
    ) -> Result<Option<(CommitId, AppendOutcome)>, BridgeError> {
        let outcome = self
            .artifacts
            .commit(&binding.mount, checkpoint_message(reason))?;
        match outcome {
            CommitOutcome::NoChanges => Ok(None),
            CommitOutcome::Committed(commit) => {
                let now = self.clock.now();
                let append = self.log.append(NewEvent {
                    agent_id: binding.agent_id(),
                    session_id: binding.session_id.clone(),
                    execution_id: Some(binding.execution_id.clone()),
                    source: EventSource::Bridge,
                    source_event_id: Some(commit.as_str().to_string()),
                    kind: EventKind::ArtifactCommitted { reason },
                    artifact: Some(ArtifactRef {
                        project: binding.project.clone(),
                        branch: binding.branch.clone(),
                        commit: commit.clone(),
                    }),
                    occurred_at: Some(now),
                })?;
                Ok(Some((commit, append)))
            }
        }
    }

    /// Rebuild the latest binding of a session from its event stream.
    ///
    /// # Errors
    /// Propagates storage failures.
    pub fn recover(&self, session_id: &SessionId) -> Result<Option<RecoveredBinding>, BridgeError> {
        let records = self.log.read_session(session_id, SessionSeq(0), 1024)?;
        Ok(directory::fold_recovery(session_id, &records))
    }

    fn binding(&self, session_id: &SessionId) -> Option<SessionBinding> {
        self.directory
            .lock()
            .expect("directory lock is not poisoned")
            .get(session_id)
            .cloned()
    }
}

fn checkpoint_message(reason: CheckpointReason) -> &'static str {
    match reason {
        CheckpointReason::ToolCompleted => "spawn checkpoint: tool completed",
        CheckpointReason::TurnFinished => "spawn checkpoint: turn finished",
        CheckpointReason::PeriodicSweep => "spawn checkpoint: periodic sweep",
        CheckpointReason::ExecutionShutdown => "spawn checkpoint: execution shutdown",
    }
}

/// Read-only access to the global stream used by projector and observer wiring.
pub fn read_global(
    log: &dyn spawn_store::EventLog,
    since: GlobalPosition,
    limit: usize,
) -> Result<Vec<spawn_types::event::EventRecord>, StoreError> {
    log.read_global(since, limit)
}

/// Test/helper constructor for a deterministic timestamp.
#[must_use]
pub fn timestamp_from_clock(clock: &dyn Clock) -> Timestamp {
    clock.now()
}

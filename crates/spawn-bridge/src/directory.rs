use std::collections::HashMap;

use spawn_layerfs::WorkspaceMount;
use spawn_types::engine::Engine;
use spawn_types::event::EventRecord;
use spawn_types::identifiers::{
    AgentId, BranchName, ExecutionId, MountId, SessionId, TerminalName,
};

/// Live binding of one session to its execution resources.
#[derive(Debug, Clone)]
pub struct SessionBinding {
    /// Owning agent.
    pub agent_id: AgentId,
    /// Logical session.
    pub session_id: SessionId,
    /// Active execution.
    pub execution_id: ExecutionId,
    /// Engine behind the execution.
    pub engine: Engine,
    /// LayerFS project.
    pub project: String,
    /// Working branch.
    pub branch: BranchName,
    /// Mounted workspace.
    pub mount: WorkspaceMount,
    /// Terminal session name.
    pub terminal_name: TerminalName,
    /// Last known native session id when resume is available.
    pub native_session_id: Option<String>,
}

impl SessionBinding {
    /// Borrow the owning agent id.
    #[must_use]
    pub fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }
}

/// In-memory directory of live bindings.
#[derive(Debug, Default)]
pub struct SessionDirectory {
    bindings: HashMap<SessionId, SessionBinding>,
}

impl SessionDirectory {
    /// Insert or replace one binding.
    pub fn register(&mut self, binding: SessionBinding) {
        self.bindings.insert(binding.session_id.clone(), binding);
    }

    /// Borrow a binding by session.
    #[must_use]
    pub fn get(&self, session_id: &SessionId) -> Option<&SessionBinding> {
        self.bindings.get(session_id)
    }

    /// Remove a binding on shutdown.
    pub fn remove(&mut self, session_id: &SessionId) -> Option<SessionBinding> {
        self.bindings.remove(session_id)
    }
}

/// Minimal recovered binding reconstructed from durable events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredBinding {
    /// Last seen execution.
    pub execution_id: ExecutionId,
    /// Engine used by the execution.
    pub engine: Engine,
    /// Terminal name recorded at execution start.
    pub terminal_name: TerminalName,
    /// Mount id recorded at execution start.
    pub mount_id: MountId,
    /// Native session id when an engine start event recorded one.
    pub native_session_id: Option<String>,
}

/// Fold a session event stream into the latest recoverable binding.
#[must_use]
pub fn fold_recovery(session_id: &SessionId, records: &[EventRecord]) -> Option<RecoveredBinding> {
    let mut recovered: Option<RecoveredBinding> = None;
    let mut native_session_id: Option<String> = None;
    for record in records {
        match &record.event.kind {
            spawn_types::event::EventKind::NativeSessionStarted {
                engine: _,
                native_session_id: native,
                ..
            } => native_session_id = Some(native.clone()),
            spawn_types::event::EventKind::ExecutionStarted {
                engine,
                terminal_name,
                mount_id,
            } => {
                let execution_id = record.event.execution_id.clone()?;
                recovered = Some(RecoveredBinding {
                    execution_id,
                    engine: *engine,
                    terminal_name: TerminalName::new(terminal_name.clone()).ok()?,
                    mount_id: MountId::new(mount_id.clone()).ok()?,
                    native_session_id: native_session_id.clone(),
                });
            }
            _ => {}
        }
    }
    let _ = session_id;
    recovered
}

//! What the model judge needs beyond the log: the terminal tail and the task.
//!
//! The task is read from storage. The terminal tail is a compute peek at
//! tmux; the Observer stores the copy it used as evidence, so the judgement
//! stays replayable from storage alone.

use std::sync::Arc;

use spawn_core::id::{SessionId, WorkspaceId};
use spawn_core::observe::ObservationContext;
use spawn_core::port::TerminalHost;
use spawn_postgres::PostgresStore;

/// Lines of terminal captured for the judge.
const TAIL_LINES: usize = 60;

/// Context backed by PostgreSQL and the terminal host.
pub(crate) struct HostContext {
    store: PostgresStore,
    terminals: Arc<dyn TerminalHost>,
}

impl HostContext {
    pub(crate) fn new(store: PostgresStore, terminals: Arc<dyn TerminalHost>) -> Self {
        Self { store, terminals }
    }
}

impl ObservationContext for HostContext {
    fn screen_tail(&self, session: &SessionId) -> Option<String> {
        let (terminal, workspace) = self.store.latest_terminal(session).ok()??;
        let workspace = WorkspaceId::new(workspace).ok()?;
        let text = self
            .terminals
            .capture_tail(&terminal, &workspace, TAIL_LINES)
            .ok()?;
        let trimmed = text.trim_end();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    fn task(&self, session: &SessionId) -> Option<String> {
        self.store.latest_task(session).ok()?
    }

    fn perpetual(&self, session: &SessionId) -> bool {
        session.as_str() == crate::supervise::SUPERVISOR_SESSION
    }
}

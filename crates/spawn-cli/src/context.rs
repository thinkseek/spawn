//! What the model judge needs beyond the log: the terminal tail, the task,
//! and the model's latest reply.
//!
//! The task is read from storage. The terminal tail is a compute peek at
//! tmux; the Observer stores the copy it used as evidence, so the judgement
//! stays replayable from storage alone. The tail leaves the host through
//! here (the judge's evidence, the attention view's read back), so it is
//! masked here first: as it is only for a session whose newest start is
//! positively Claude Code's, through its engine's mask otherwise, a failed
//! lookup included (decision C2, N4; `spawn_core::driver::excerpt_for`). The reply is read from the engine's
//! transcript, from where the last read ended.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use spawn_core::id::{SessionId, WorkspaceId};
use spawn_core::observe::ObservationContext;
use spawn_core::observe::reply::Reply;
use spawn_core::port::TerminalHost;
use spawn_postgres::PostgresStore;

/// Lines of terminal captured for the judge.
const TAIL_LINES: usize = 60;

/// Reads of a transcript per call, at most: a long transcript is caught up
/// over several calls.
const READS_PER_CALL: usize = 8;

/// Context backed by PostgreSQL and the terminal host.
pub(crate) struct HostContext {
    store: PostgresStore,
    terminals: Arc<dyn TerminalHost>,
    /// The reply followed in each transcript, by its path.
    replies: Mutex<HashMap<String, Reply>>,
}

impl HostContext {
    pub(crate) fn new(store: PostgresStore, terminals: Arc<dyn TerminalHost>) -> Self {
        Self {
            store,
            terminals,
            replies: Mutex::new(HashMap::new()),
        }
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
        let engine = self.store.latest_engine(session).ok().flatten();
        let text = spawn_core::driver::excerpt_for(engine.as_deref(), &text);
        let trimmed = text.trim_end();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    fn task(&self, session: &SessionId) -> Option<String> {
        self.store.latest_task(session).ok()?
    }

    fn reply(&self, workspace: &WorkspaceId, transcript: &str) -> Option<String> {
        let mut replies = self.replies.lock().unwrap_or_else(PoisonError::into_inner);
        let reply = replies.entry(transcript.to_string()).or_default();
        for _ in 0..READS_PER_CALL {
            let Ok(Some(bytes)) = self
                .terminals
                .read_from(workspace, transcript, reply.offset())
            else {
                break;
            };
            if bytes.is_empty() || !reply.feed(&bytes) {
                break;
            }
        }
        reply.text().map(str::to_string)
    }

    fn perpetual(&self, session: &SessionId) -> bool {
        session.as_str() == crate::supervise::SUPERVISOR_SESSION
    }
}

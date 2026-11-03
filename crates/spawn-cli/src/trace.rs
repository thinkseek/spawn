//! Port timing for `SPAWN_TRACE=1`.
//!
//! Each wrapper forwards to the real port and prints one line per call to
//! standard error: `spawn-trace <port>.<method> <microseconds>`.
// ponytail: stderr lines, no tracing crate; add `tracing` when a daemon needs spans.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use spawn_core::command::{Claimed, Outcome, Submission};
use spawn_core::event::{Appended, EventRecord, GlobalPosition, NewEvent, SessionSeq};
use spawn_core::id::{BranchName, CommandId, Principal, SessionId, TerminalName, WorkspaceId};
use spawn_core::observe::{JudgeError, JudgeInput, Judgement, StateJudge};
use spawn_core::port::{
    CommandQueue, Commit, EventLog, Launch, StoreError, TerminalError, TerminalHost,
    WorkspaceError, WorkspaceStore,
};

/// Whether `SPAWN_TRACE` is set to a non-empty value.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("SPAWN_TRACE").is_ok_and(|value| !value.is_empty()))
}

/// Run `f` and, when tracing, print its duration under `name`.
pub fn time<T>(name: &str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let start = Instant::now();
    let value = f();
    print(name, start.elapsed());
    value
}

fn print(name: &str, elapsed: Duration) {
    eprintln!("spawn-trace {name} {}", elapsed.as_micros());
}

/// Timed port.
pub struct Timed<T: ?Sized>(pub Arc<T>);

impl<T: EventLog + ?Sized> EventLog for Timed<T> {
    fn append(&self, event: NewEvent) -> Result<Appended, StoreError> {
        time("log.append", || self.0.append(event))
    }

    fn read_session(
        &self,
        session: &SessionId,
        after: SessionSeq,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        time("log.read_session", || {
            self.0.read_session(session, after, limit)
        })
    }

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        time("log.read_global", || self.0.read_global(after, limit))
    }
}

impl<T: CommandQueue + ?Sized> CommandQueue for Timed<T> {
    fn submit(
        &self,
        principal: &Principal,
        submission: &Submission,
    ) -> Result<CommandId, StoreError> {
        time("queue.submit", || self.0.submit(principal, submission))
    }

    fn claim(&self, worker: &str, ttl: Duration) -> Result<Option<Claimed>, StoreError> {
        time("queue.claim", || self.0.claim(worker, ttl))
    }

    fn finish(&self, claimed: &Claimed, outcome: &Outcome) -> Result<(), StoreError> {
        time("queue.finish", || self.0.finish(claimed, outcome))
    }
}

impl<T: WorkspaceStore + ?Sized> WorkspaceStore for Timed<T> {
    fn project(&self) -> &str {
        self.0.project()
    }

    fn ensure_branch(&self, branch: &BranchName) -> Result<(), WorkspaceError> {
        time("git.ensure_branch", || self.0.ensure_branch(branch))
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        time("git.mount", || self.0.mount(branch))
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        time("git.commit", || self.0.commit(workspace, message))
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        time("git.unmount", || self.0.unmount(workspace))
    }
}

impl<T: TerminalHost + ?Sized> TerminalHost for Timed<T> {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        time("tmux.start", || self.0.start(launch))
    }

    fn submit_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        time("tmux.submit_text", || {
            self.0.submit_text(terminal, workspace, text)
        })
    }

    fn is_alive(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        time("tmux.is_alive", || self.0.is_alive(terminal, workspace))
    }

    fn capture_tail(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError> {
        time("tmux.capture_tail", || {
            self.0.capture_tail(terminal, workspace, lines)
        })
    }

    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        time("tmux.terminate", || self.0.terminate(terminal, workspace))
    }
}

impl<T: StateJudge + ?Sized> StateJudge for Timed<T> {
    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        time("judge.judge", || self.0.judge(input))
    }
}

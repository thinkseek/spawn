//! The facts the Observer keeps for one session, folded from the log.

use std::collections::{BTreeMap, HashSet};

use super::SCREEN_STALE;
use super::judge::{JudgeInput, TurnFact};
use crate::event::{CommandStep, EndCause, EventKind, EventRecord, JobKind, SessionSeq, Source};
use crate::id::{AgentId, ExecutionId, SessionId, WorkspaceId};
use crate::screen::ScreenState;
use crate::time::Timestamp;

/// Facts the Observer keeps for one session.
#[derive(Debug, Clone, Default)]
pub(super) struct Snapshot {
    engine: Option<String>,
    /// The newest execution started.
    execution: Option<ExecutionId>,
    /// Its workspace.
    workspace: Option<WorkspaceId>,
    /// The session's transcript, as its engine reported it.
    transcript: Option<String>,
    pub(super) execution_active: bool,
    /// The current execution's end, whose final checkpoint keeps failing:
    /// its workspace stays mounted until someone acts.
    stalled_end: Option<EndCause>,
    last_end: Option<EndCause>,
    open_tools: HashSet<String>,
    /// The background jobs the current execution started and that have not
    /// ended, by id, with their kind and when they started. A turn's end
    /// leaves them open; the execution's end or the next start closes them.
    open_jobs: BTreeMap<String, (JobKind, Timestamp)>,
    last_tool: Option<String>,
    permission_pending: bool,
    screen: Option<ScreenState>,
    screen_at: Option<Timestamp>,
    turn: Option<TurnFact>,
    /// The latest failure (a tool's or a turn's) that nothing clean has
    /// superseded since: a tool that completed, a turn that stopped, a
    /// message, a typed input or a new execution clears it. An early failed
    /// command must not follow a session through hundreds of clean tools
    /// to a clean end, where the judge read it as the turn's (live
    /// dogfooding, mission #7: false WAITING alerts on finished tasks).
    last_error: Option<String>,
    exit_code: Option<i32>,
    pub(super) evidence_seq: SessionSeq,
    last_fact_at: Option<Timestamp>,
    /// When the engine last moved: its latest transcript fact, or the
    /// execution's start. Screen readings and commands do not move it.
    progress_at: Option<Timestamp>,
    /// The engine's latest move, as a position: what the model judge's
    /// fingerprint counts as new evidence beyond the facts shown.
    pub(super) progress_seq: SessionSeq,
}

impl Snapshot {
    /// When the facts were last added to, and the engine last moved.
    fn note_times(&mut self, record: &EventRecord) {
        self.evidence_seq = record.session_seq;
        self.last_fact_at = Some(record.recorded_at);
        if record.event.source == Source::Claude
            || matches!(record.event.kind, EventKind::ExecutionStarted { .. })
        {
            self.progress_at = Some(record.recorded_at);
            self.progress_seq = record.session_seq;
        }
    }

    /// A record of the session that adds nothing to its facts (an
    /// observation claimed by another source): judgements made after it
    /// still cover it.
    pub(super) fn note_seq(&mut self, record: &EventRecord) {
        self.evidence_seq = self.evidence_seq.max(record.session_seq);
    }

    fn tool_started(&mut self, tool_name: &str, tool_use_id: &str) {
        self.last_tool = Some(tool_name.to_string());
        self.open_tools.insert(tool_use_id.to_string());
    }

    /// A tool call ended: one that failed is the latest error, one that
    /// completed supersedes any error before it.
    fn tool_ended(&mut self, tool_use_id: &str, error: Option<&String>) {
        self.open_tools.remove(tool_use_id);
        self.last_error = error.cloned();
    }

    /// Apply one event.
    pub(super) fn apply(&mut self, record: &EventRecord) {
        self.note_times(record);
        // A hook event is newer than any screen reading and means the engine
        // moved on, so an older screen state or permission prompt is stale.
        if matches!(
            record.event.kind,
            EventKind::ToolStarted { .. }
                | EventKind::ToolCompleted { .. }
                | EventKind::ToolFailed { .. }
                | EventKind::TurnStopped { .. }
                | EventKind::TurnFailed { .. }
                | EventKind::MessageReceived
        ) {
            self.screen = None;
            self.permission_pending = false;
        }
        match &record.event.kind {
            EventKind::ExecutionStarted {
                engine, workspace, ..
            } => self.started(engine, workspace, record.event.execution_id.as_ref()),
            // A late end report of an older execution says nothing about
            // the current one.
            EventKind::ExecutionEnded { .. }
                if record.event.execution_id.is_some()
                    && self.execution.is_some()
                    && record.event.execution_id != self.execution => {}
            EventKind::ExecutionEnded {
                cause, exit_code, ..
            } => {
                self.execution_active = false;
                self.stalled_end = None;
                self.last_end = Some(*cause);
                self.exit_code = *exit_code;
                self.open_tools.clear();
                self.open_jobs.clear();
                // The pane is gone with the execution; nothing on it is a
                // current fact any more.
                self.screen = None;
                self.permission_pending = false;
            }
            EventKind::ExecutionEndStalled { cause, .. }
                if record.event.execution_id.is_none()
                    || record.event.execution_id == self.execution =>
            {
                self.stalled_end = Some(*cause);
                // The Bridge killed the terminal: nothing on it is current.
                self.screen = None;
                self.permission_pending = false;
            }
            EventKind::NativeSessionStarted {
                engine,
                transcript_path,
                ..
            } => {
                self.engine = Some(engine.clone());
                if let Some(path) = transcript_path {
                    self.transcript = Some(path.clone());
                }
            }
            EventKind::ToolStarted {
                tool_name,
                tool_use_id,
                ..
            } => self.tool_started(tool_name, tool_use_id),
            EventKind::ToolCompleted { tool_use_id, .. } => self.tool_ended(tool_use_id, None),
            EventKind::ToolFailed { tool_use_id, error } => {
                self.tool_ended(tool_use_id, Some(error));
            }
            EventKind::TurnStopped { .. } => {
                self.turn = Some(TurnFact::Stopped);
                self.last_error = None;
                self.open_tools.clear();
            }
            EventKind::JobStarted { .. } | EventKind::JobEnded { .. } => self.job(record),
            EventKind::TurnFailed { error, .. } => {
                self.turn = Some(TurnFact::Failed);
                self.last_error = Some(error.clone());
                self.open_tools.clear();
            }
            EventKind::NotificationReceived { notification_type } => {
                self.permission_pending = notification_type.contains("permission");
            }
            EventKind::ScreenObserved { state, .. } => {
                self.screen = Some(*state);
                self.screen_at = Some(record.recorded_at);
                if *state == ScreenState::Permission {
                    self.permission_pending = true;
                }
                if *state == ScreenState::Ready {
                    self.permission_pending = false;
                }
            }
            EventKind::MessageReceived => {
                self.turn = None;
                self.last_error = None;
            }
            // The Bridge reads the pane as ready right before it types
            // input, and records the typing (`typed`, once per command). A
            // typing that comes after the latest ready reading delivered
            // that input: the ready screen is consumed and a new turn
            // begins, even though the engine's own hook for it may land a
            // moment later. Not the command's `command.finished`: a retried
            // command whose text was typed before the turn ended finishes
            // after the turn's end and a ready screen, and starts nothing.
            EventKind::CommandStepped {
                step: CommandStep::Typed,
                ..
            } if self.screen == Some(ScreenState::Ready) => {
                self.screen = None;
                self.turn = None;
                self.last_error = None;
            }
            _ => {}
        }
    }

    /// A background job started or ended. A start recorded for an earlier
    /// execution opens nothing: its jobs died with it. An Agent resumed
    /// under its id starts again after its end: open again, as of the new
    /// start.
    fn job(&mut self, record: &EventRecord) {
        match &record.event.kind {
            EventKind::JobStarted { job_id, kind }
                if record.event.execution_id.is_none()
                    || record.event.execution_id == self.execution =>
            {
                self.open_jobs
                    .insert(job_id.clone(), (*kind, record.recorded_at));
            }
            EventKind::JobEnded { job_id, .. } => {
                self.open_jobs.remove(job_id);
            }
            _ => {}
        }
    }

    /// A new execution: what the previous one showed is gone.
    fn started(&mut self, engine: &str, workspace: &WorkspaceId, execution: Option<&ExecutionId>) {
        self.engine = Some(engine.to_string());
        self.workspace = Some(workspace.clone());
        self.execution = execution.cloned();
        self.execution_active = true;
        self.stalled_end = None;
        self.last_end = None;
        self.exit_code = None;
        self.open_tools.clear();
        self.open_jobs.clear();
        self.permission_pending = false;
        self.screen = None;
        self.turn = None;
        self.last_error = None;
    }

    /// The running execution's workspace and the session's transcript:
    /// where the model's latest reply is read.
    pub(super) fn transcript(&self) -> Option<(&WorkspaceId, &str)> {
        if !self.execution_active {
            return None;
        }
        Some((self.workspace.as_ref()?, self.transcript.as_deref()?))
    }

    /// The judge input these facts make at `now`, before any context is
    /// attached.
    pub(super) fn input(
        &self,
        agent_id: &AgentId,
        session_id: &SessionId,
        now: Timestamp,
    ) -> JudgeInput {
        let stale = self
            .screen_at
            .is_none_or(|at| now.since(at) >= SCREEN_STALE);
        let screen = match self.screen {
            Some(ScreenState::Busy) if stale => None,
            other => other,
        };
        let since = |at: Timestamp| u64::try_from(now.since(at).as_millis()).unwrap_or(u64::MAX);
        JudgeInput {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            engine: self.engine.clone(),
            execution_active: self.execution_active,
            stalled_end: self.stalled_end,
            last_end: self.last_end,
            exit_code: self.exit_code,
            open_tools: self.open_tools.len(),
            open_jobs: self.open_jobs.len(),
            oldest_job_ms: self.open_jobs.values().map(|(_, at)| since(*at)).max(),
            last_tool: self.last_tool.clone(),
            permission_pending: self.permission_pending,
            screen,
            turn: self.turn,
            last_error: self.last_error.clone(),
            evidence_seq: self.evidence_seq,
            ambiguity: None,
            quiet_ms: self.last_fact_at.map(since),
            progress_ms: self.progress_at.map(since),
            task: None,
            screen_tail: None,
            reply: None,
            perpetual: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Snapshot;
    use crate::event::{
        CommandStep, EventKind, EventRecord, GlobalPosition, ModelProfile, NewEvent, SessionSeq,
        Source,
    };
    use crate::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
    use crate::screen::ScreenState;
    use crate::time::Timestamp;

    /// Folds `kinds` into a fresh snapshot, in order, and returns the
    /// `last_error` its judge input carries.
    fn last_error(kinds: Vec<(Source, EventKind)>) -> Option<String> {
        let mut snapshot = Snapshot::default();
        for (n, (source, kind)) in (1_u64..).zip(kinds) {
            snapshot.apply(&EventRecord {
                position: GlobalPosition(n),
                session_seq: SessionSeq(n),
                recorded_at: Timestamp(i64::try_from(n).expect("small") * 1_000),
                event: NewEvent {
                    agent_id: AgentId::new("agent_w").expect("valid agent"),
                    session_id: SessionId::new("session_w").expect("valid session"),
                    execution_id: Some(ExecutionId::new("exec_1").expect("valid execution")),
                    source,
                    source_event_id: None,
                    kind,
                },
            });
        }
        snapshot
            .input(
                &AgentId::new("agent_w").expect("valid agent"),
                &SessionId::new("session_w").expect("valid session"),
                Timestamp(1_000_000),
            )
            .last_error
    }

    fn started() -> (Source, EventKind) {
        (
            Source::Bridge,
            EventKind::ExecutionStarted {
                engine: "claude".to_string(),
                workspace: WorkspaceId::new("w:/workspaces/w").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        )
    }

    fn tool(id: &str) -> (Source, EventKind) {
        (
            Source::Claude,
            EventKind::ToolStarted {
                tool_name: "Bash".to_string(),
                tool_use_id: id.to_string(),
                summary: None,
                summary_whole: false,
            },
        )
    }

    fn tool_failed(id: &str) -> (Source, EventKind) {
        (
            Source::Claude,
            EventKind::ToolFailed {
                tool_use_id: id.to_string(),
                error: "exit status 101".to_string(),
            },
        )
    }

    fn tool_completed(id: &str) -> (Source, EventKind) {
        (
            Source::Claude,
            EventKind::ToolCompleted {
                tool_use_id: id.to_string(),
                duration_ms: None,
            },
        )
    }

    fn turn_stopped() -> (Source, EventKind) {
        (
            Source::Claude,
            EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
        )
    }

    fn turn_failed() -> (Source, EventKind) {
        (
            Source::Claude,
            EventKind::TurnFailed {
                error: "API Error: 529 overloaded".to_string(),
                status: Some(529),
                error_kind: Some("overloaded".to_string()),
            },
        )
    }

    /// A failure nothing clean has followed is the session's latest error.
    #[test]
    fn a_failure_nothing_clean_followed_stays_the_latest_error() {
        assert_eq!(
            last_error(vec![started(), tool("t1"), tool_failed("t1")]).as_deref(),
            Some("exit status 101")
        );
        assert_eq!(
            last_error(vec![
                started(),
                tool("t1"),
                tool_completed("t1"),
                turn_failed()
            ])
            .as_deref(),
            Some("API Error: 529 overloaded")
        );
        // A clean tool before the failure does not hide it.
        assert_eq!(
            last_error(vec![
                started(),
                tool("t1"),
                tool_completed("t1"),
                tool("t2"),
                tool_failed("t2"),
            ])
            .as_deref(),
            Some("exit status 101")
        );
    }

    /// Mission #7: an early failed test script followed by hundreds of
    /// clean tools and a clean end of turn kept its error to the end, and
    /// the judge read the finished task as waiting. A tool that completes
    /// after the failure supersedes it.
    #[test]
    fn a_tool_completed_after_a_failed_one_clears_the_error() {
        let mut kinds = vec![started(), tool("t1"), tool_failed("t1")];
        for n in 2..300 {
            let id = format!("t{n}");
            kinds.push(tool(&id));
            kinds.push(tool_completed(&id));
        }
        assert_eq!(last_error(kinds), None);
    }

    /// A turn that stopped cleanly after a failed tool ended clean.
    #[test]
    fn a_turn_stopped_after_a_failed_tool_clears_the_error() {
        assert_eq!(
            last_error(vec![
                started(),
                tool("t1"),
                tool_failed("t1"),
                turn_stopped()
            ]),
            None
        );
    }

    /// A new execution starts with no error: the previous one's failed
    /// turn is not its.
    #[test]
    fn a_new_execution_clears_a_failed_turns_error() {
        assert_eq!(last_error(vec![started(), turn_failed(), started()]), None);
    }

    /// A message, or input typed at a ready prompt, starts a new turn: the
    /// failure of the turn before is not its.
    #[test]
    fn a_message_or_typed_input_clears_a_failed_turns_error() {
        assert_eq!(
            last_error(vec![
                started(),
                turn_failed(),
                (Source::Claude, EventKind::MessageReceived)
            ]),
            None
        );
        let ready = (
            Source::Bridge,
            EventKind::ScreenObserved {
                state: ScreenState::Ready,
                evidence: "prompt".to_string(),
            },
        );
        let typed = (
            Source::Bridge,
            EventKind::CommandStepped {
                command_id: CommandId(2),
                step: CommandStep::Typed,
                workspace: None,
            },
        );
        assert_eq!(
            last_error(vec![started(), turn_failed(), ready, typed.clone()]),
            None
        );
        // Typed into a pane not read as ready: no new turn, the error stays.
        assert_eq!(
            last_error(vec![started(), turn_failed(), typed]).as_deref(),
            Some("API Error: 529 overloaded")
        );
    }
}

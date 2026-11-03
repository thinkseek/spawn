//! Periodic semantic observation of agent sessions.
//!
//! The observer folds execution facts into a snapshot, asks a [`StateJudge`]
//! for semantic state, and appends observations only when the conclusion
//! changes. Its own events never trigger another judgement.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
mod heuristic;
mod snapshot;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use spawn_store::{EventLog, StoreError};
use spawn_types::clock::{Clock, Timestamp};
use spawn_types::event::{EventKind, EventRecord, EventSource, NewEvent, SessionSeq};
use spawn_types::identifiers::{AgentId, ExecutionId, SessionId};
use spawn_types::observation::{Confidence, Observation, SemanticState, WaitReason};

pub use heuristic::HeuristicJudge;
pub use snapshot::SessionSnapshot;

/// Decision produced by a judge implementation.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgement {
    /// Semantic state.
    pub state: SemanticState,
    /// Optional structured reason.
    pub reason: Option<WaitReason>,
    /// Calibrated confidence.
    pub confidence: Confidence,
}

/// Port for pluggable decision models.
pub trait StateJudge: Send + Sync {
    /// Judge one session snapshot.
    fn judge(&self, snapshot: &SessionSnapshot, now: Timestamp) -> Result<Judgement, String>;

    /// Stable judge name stored on each observation.
    fn name(&self) -> &str;
}

/// Per-session observer bookkeeping.
#[derive(Debug, Clone, Default)]
struct SessionProgress {
    cursor: u64,
    last: Option<(SemanticState, Option<WaitReason>)>,
}

/// Drives periodic observation over an [`EventLog`].
pub struct Observer {
    log: Arc<dyn EventLog>,
    judge: Arc<dyn StateJudge>,
    clock: Arc<dyn Clock>,
    heartbeat_timeout: Duration,
    progress: HashMap<SessionId, SessionProgress>,
}

impl Observer {
    /// Construct an observer with its ports and freshness policy.
    #[must_use]
    pub fn new(
        log: Arc<dyn EventLog>,
        judge: Arc<dyn StateJudge>,
        clock: Arc<dyn Clock>,
        heartbeat_timeout: Duration,
    ) -> Self {
        Self {
            log,
            judge,
            clock,
            heartbeat_timeout,
            progress: HashMap::new(),
        }
    }

    /// Run one evaluation pass and return every newly stored observation.
    ///
    /// # Errors
    /// Propagates storage failures. Judge failures degrade to `Unknown`.
    pub fn tick(&mut self) -> Result<Vec<Observation>, StoreError> {
        let now = self.clock.now();
        let cursors = self.log.session_cursors()?;
        let mut emitted = Vec::new();
        for cursor in cursors {
            if let Some(observation) = self.evaluate_session(&cursor.session_id, now)? {
                emitted.push(observation);
            }
        }
        Ok(emitted)
    }

    fn evaluate_session(
        &mut self,
        session_id: &SessionId,
        now: Timestamp,
    ) -> Result<Option<Observation>, StoreError> {
        let progress = self.progress.entry(session_id.clone()).or_default();
        let mut records: Vec<EventRecord> = Vec::new();
        let mut batch = self
            .log
            .read_session(session_id, SessionSeq(progress.cursor), 256)?;
        while !batch.is_empty() {
            let last = batch.last().expect("batch is non-empty").session_seq.0;
            records.extend(batch);
            batch = self.log.read_session(session_id, SessionSeq(last), 256)?;
        }
        progress.cursor = records
            .last()
            .map_or(progress.cursor, |record| record.session_seq.0);

        // Observer events are facts about judgements, not new agent evidence.
        let evidence: Vec<&EventRecord> = records
            .iter()
            .filter(|record| record.event.source != EventSource::Observer)
            .collect();
        let Some(latest_evidence) = evidence.last().copied() else {
            return Ok(None);
        };

        let mut snapshot = SessionSnapshot::fold(evidence.iter().copied());
        let based_on_seq = latest_evidence.session_seq;
        snapshot.based_on_seq = based_on_seq;

        let stale = snapshot.execution_active
            && now
                .unix_millis()
                .saturating_sub(snapshot.last_activity_at.unix_millis())
                >= i64::try_from(self.heartbeat_timeout.as_millis()).unwrap_or(i64::MAX);
        let judgement = if stale {
            Judgement {
                state: SemanticState::Unknown,
                reason: Some(WaitReason::ObservationExpired),
                confidence: Confidence::new(0.0).expect("zero is valid"),
            }
        } else {
            self.judge
                .judge(&snapshot, now)
                .unwrap_or_else(|_| Judgement {
                    state: SemanticState::Unknown,
                    reason: Some(WaitReason::JudgeUnavailable),
                    confidence: Confidence::new(0.0).expect("zero is valid"),
                })
        };

        let signature = (judgement.state, judgement.reason.clone());
        if progress.last.as_ref() == Some(&signature) {
            return Ok(None);
        }
        progress.last = Some(signature);

        let observation = Observation {
            state: judgement.state,
            reason: judgement.reason,
            based_on_seq,
            confidence: judgement.confidence,
            judge_name: self.judge.name().to_string(),
        };
        let first = records
            .first()
            .expect("at least one evidence record exists");
        let new_event = NewEvent {
            agent_id: first.event.agent_id.clone(),
            session_id: session_id.clone(),
            execution_id: latest_evidence.event.execution_id.clone(),
            source: EventSource::Observer,
            source_event_id: Some(format!(
                "observer:{}:{}",
                session_id.as_str(),
                based_on_seq.0
            )),
            kind: EventKind::ObservationRecorded {
                observation: observation.clone(),
            },
            artifact: None,
            occurred_at: Some(now),
        };
        self.log.append(new_event)?;
        Ok(Some(observation))
    }
}

/// Helper used by CLI wiring to build typed ids from stored strings.
///
/// # Errors
/// Rejects labels that violate identifier validation.
pub fn parse_session_id(raw: &str) -> Result<SessionId, spawn_types::SpawnError> {
    SessionId::from_prefixed(raw).map_err(spawn_types::SpawnError::Validation)
}

/// Helper for execution ids used in recovery wiring.
///
/// # Errors
/// Rejects labels that violate identifier validation.
pub fn parse_execution_id(raw: &str) -> Result<ExecutionId, spawn_types::SpawnError> {
    ExecutionId::from_prefixed(raw).map_err(spawn_types::SpawnError::Validation)
}

/// Helper for agent ids used in recovery wiring.
///
/// # Errors
/// Rejects labels that violate identifier validation.
pub fn parse_agent_id(raw: &str) -> Result<AgentId, spawn_types::SpawnError> {
    AgentId::from_prefixed(raw).map_err(spawn_types::SpawnError::Validation)
}

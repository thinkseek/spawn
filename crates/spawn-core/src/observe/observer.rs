//! The Observer loop: read the log, keep per-session facts, classify
//! changed or due sessions and append an observation when the judgement
//! changes.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::classify::{RulesVerdict, exited_failing, rules, wait_reason};
use super::evidence::{ObservationContext, attach_context, fingerprint_of, human_question};
use super::interpret::{attention_reason, fold_completed, narrow};
use super::judge::{JudgeInput, StateJudge};
use super::snapshot::Snapshot;
use super::{RECHECK_EVERY, RETRY_BASE, RETRY_MAX};
use crate::event::{EventKind, EventRecord, GlobalPosition, NewEvent, SessionSeq, Source};
use crate::id::{AgentId, SessionId};
use crate::observation::{Ambiguity, Confidence, Observation, SemanticState, WaitReason};
use crate::port::{EventLog, StoreError};
use crate::time::{Clock, SystemClock, Timestamp};

/// Events read from the log per batch.
const BATCH: usize = 200;

/// What the Observer last wrote for a session. A new observation is written
/// only when this changes.
type WrittenKey = (SemanticState, Option<WaitReason>, bool, bool);

fn key_of(observation: &Observation) -> WrittenKey {
    (
        observation.state,
        observation.reason,
        observation.attention,
        observation.ambiguity.is_some(),
    )
}

/// Pause before asking an unavailable judge again after `failures` failures.
fn retry_after(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(8);
    RETRY_BASE.saturating_mul(1 << doublings).min(RETRY_MAX)
}

/// Milliseconds elapsed since `started`, saturating.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Per-session observation state.
#[derive(Debug)]
struct Tracked {
    agent_id: AgentId,
    snapshot: Snapshot,
    last: Option<WrittenKey>,
    judged_seq: SessionSeq,
    judged_at: Option<Timestamp>,
    /// Evidence of the last model answer; an identical one is not asked again.
    asked: Option<String>,
    /// Consecutive judge failures; the next tick retries after a backoff.
    failures: u32,
}

impl Tracked {
    fn new(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            snapshot: Snapshot::default(),
            last: None,
            judged_seq: SessionSeq(0),
            judged_at: None,
            asked: None,
            failures: 0,
        }
    }

    /// Whether the session must be classified at `now`: it has new facts, or
    /// a recheck or a judge retry is due.
    fn is_due(&self, now: Timestamp) -> bool {
        if self.snapshot.evidence_seq > self.judged_seq {
            return true;
        }
        let elapsed = |pause: Duration| self.judged_at.is_none_or(|at| now.since(at) >= pause);
        // While the judge fails, its backoff replaces the recheck.
        if self.failures > 0 {
            elapsed(retry_after(self.failures))
        } else {
            self.snapshot.execution_active && elapsed(RECHECK_EVERY)
        }
    }

    /// Restore what the Observer last wrote and asked from one of its own
    /// rows, so a new process neither writes the same observation again nor
    /// asks the model about the same evidence.
    fn restore(&mut self, observation: &Observation, recorded_at: Timestamp) {
        // `judged_seq` is not restored: a new process checks every session
        // once. A rules case costs nothing and writes only if the state
        // changed; a model case is skipped by its fingerprint. A row from an
        // older build that the current rules disagree with is thus corrected
        // instead of trusted forever.
        self.last = Some(key_of(observation));
        self.judged_at = Some(recorded_at);
        // The live process also reads back its own rows, so this records that
        // the judge failed, not how often.
        if observation.reason == Some(WaitReason::JudgeUnavailable) {
            self.failures = self.failures.max(1);
        } else {
            self.failures = 0;
        }
        if observation.fingerprint.is_some() {
            self.asked.clone_from(&observation.fingerprint);
        }
    }

    /// Fold one log record into the session.
    fn apply(&mut self, record: &EventRecord) {
        // Only the Observer's rows count as observations; anything else
        // claiming to be one is ignored.
        if record.event.source == Source::Observer {
            if let EventKind::ObservationRecorded { observation } = &record.event.kind {
                self.restore(observation, record.recorded_at);
            }
        } else if !matches!(record.event.kind, EventKind::ObservationRecorded { .. }) {
            self.snapshot.apply(record);
        }
    }
}

/// The outcome of classifying one session, before it is written.
struct Classified {
    observation: Observation,
    /// Fingerprint of the last model answer after this classification.
    asked: Option<String>,
    /// Consecutive judge failures after this classification.
    failures: u32,
}

/// State observer: rules first, the model judge on ambiguity.
pub struct Observer {
    log: Arc<dyn EventLog>,
    judge: Arc<dyn StateJudge>,
    context: Option<Arc<dyn ObservationContext>>,
    clock: Arc<dyn Clock>,
    cursor: GlobalPosition,
    sessions: HashMap<SessionId, Tracked>,
}

impl Observer {
    /// Observer over one log, with `judge` answering ambiguous cases.
    pub fn new(log: Arc<dyn EventLog>, judge: Arc<dyn StateJudge>) -> Self {
        Self {
            log,
            judge,
            context: None,
            clock: Arc::new(SystemClock),
            cursor: GlobalPosition(0),
            sessions: HashMap::new(),
        }
    }

    /// Give the model judge the terminal tail and the task.
    #[must_use]
    pub fn with_context(mut self, context: Arc<dyn ObservationContext>) -> Self {
        self.context = Some(context);
        self
    }

    /// Use another clock, for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Sessions the Observer tracks.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Rebuild snapshots from the whole log.
    pub fn recover(&mut self) -> Result<(), StoreError> {
        self.cursor = GlobalPosition(0);
        self.sessions.clear();
        self.catch_up()
    }

    /// Read new events and classify changed or due sessions.
    ///
    /// Returns the number of observations written.
    pub fn tick(&mut self) -> Result<usize, StoreError> {
        self.catch_up()?;
        let now = self.clock.now();
        let pending: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, tracked)| tracked.is_due(now))
            .map(|(session, _)| session.clone())
            .collect();
        let mut written = 0;
        for session in pending {
            written += self.observe(&session, now)?;
        }
        Ok(written)
    }

    fn catch_up(&mut self) -> Result<(), StoreError> {
        loop {
            let batch = self.log.read_global(self.cursor, BATCH)?;
            if batch.is_empty() {
                return Ok(());
            }
            // The control stream is not an agent's session.
            for record in batch
                .iter()
                .filter(|record| !record.event.kind.is_control())
            {
                self.sessions
                    .entry(record.event.session_id.clone())
                    .or_insert_with(|| Tracked::new(record.event.agent_id.clone()))
                    .apply(record);
            }
            if let Some(last) = batch.last() {
                self.cursor = last.position;
            }
            if batch.len() < BATCH {
                return Ok(());
            }
        }
    }

    fn observe(&mut self, session: &SessionId, now: Timestamp) -> Result<usize, StoreError> {
        let Some(tracked) = self.sessions.get(session) else {
            return Ok(0);
        };
        let input = tracked.snapshot.input(&tracked.agent_id, session, now);
        let agent = tracked.agent_id.clone();
        let seq = tracked.snapshot.evidence_seq;

        let classified = match rules(&input) {
            RulesVerdict::Certain { state } => Self::certain(state, &input, seq),
            RulesVerdict::Ambiguous { ambiguity, guess } => {
                let asked = self.ambiguous(tracked, session, input, ambiguity, guess, seq);
                let Some(classified) = asked else {
                    // Nothing new to show the model; keep the last answer.
                    if let Some(tracked) = self.sessions.get_mut(session) {
                        tracked.judged_seq = seq;
                        tracked.judged_at = Some(now);
                    }
                    return Ok(0);
                };
                classified
            }
        };
        let Classified {
            observation,
            asked,
            failures,
        } = classified;

        let key = key_of(&observation);
        // Every model answer is kept, with its evidence and fingerprint: it is
        // the record of what the model saw and said, and what a restarted
        // Observer uses to avoid asking again.
        let answered = observation.fingerprint.is_some();
        let changed = answered
            || self
                .sessions
                .get(session)
                .is_none_or(|tracked| tracked.last != Some(key));
        let written = if changed {
            self.append_observation(&agent, session, observation)?;
            1
        } else {
            0
        };
        if let Some(tracked) = self.sessions.get_mut(session) {
            tracked.last = Some(key);
            tracked.judged_seq = seq;
            tracked.judged_at = Some(now);
            tracked.asked = asked;
            tracked.failures = failures;
        }
        Ok(written)
    }

    /// A state the rules settle.
    fn certain(state: SemanticState, input: &JudgeInput, seq: SessionSeq) -> Classified {
        let mut observation = Observation::basic(
            state,
            wait_reason(state, input),
            seq,
            Confidence::new(1.0).unwrap_or(Confidence::ZERO),
            "rules",
        );
        observation.attention = state == SemanticState::Blocked;
        if observation.attention {
            observation.attention_reason = Some("permission".to_string());
        }
        Classified {
            observation,
            asked: None,
            failures: 0,
        }
    }

    /// An ambiguous case: attach the context and ask the model, unless it
    /// was already asked about exactly this evidence (then `None`).
    fn ambiguous(
        &self,
        tracked: &Tracked,
        session: &SessionId,
        mut input: JudgeInput,
        ambiguity: Ambiguity,
        guess: SemanticState,
        seq: SessionSeq,
    ) -> Option<Classified> {
        input.ambiguity = Some(ambiguity);
        attach_context(&mut input, self.context.as_deref(), session);
        let fingerprint = fingerprint_of(&input);
        if tracked.failures == 0 && tracked.asked.as_deref() == Some(fingerprint.as_str()) {
            return None;
        }
        let (mut observation, failed) = self.ask_model(&input, ambiguity, guess, seq);
        if let Some(question) = input.screen_tail.as_deref().and_then(human_question) {
            observation.human_question = Some(question);
            observation.attention = true;
            observation.attention_reason = Some("human_question".to_string());
        }
        Some(if failed {
            Classified {
                observation,
                asked: tracked.asked.clone(),
                failures: tracked.failures.saturating_add(1),
            }
        } else {
            observation.fingerprint = Some(fingerprint.clone());
            Classified {
                observation,
                asked: Some(fingerprint),
                failures: 0,
            }
        })
    }

    /// Ask the model judge about an ambiguous case and narrow its answer.
    ///
    /// Returns the observation and whether the next tick must retry.
    fn ask_model(
        &self,
        input: &JudgeInput,
        ambiguity: Ambiguity,
        guess: SemanticState,
        seq: SessionSeq,
    ) -> (Observation, bool) {
        let started = Instant::now();
        let answer = self.judge.judge(input);
        let latency = elapsed_ms(started);
        let model_observation = |state, reason, confidence| {
            let mut observation =
                Observation::basic(state, reason, seq, confidence, self.judge.name());
            observation.ambiguity = Some(ambiguity);
            observation.rules_state = Some(guess);
            observation.latency_ms = Some(latency);
            observation.evidence = serde_json::to_value(input).ok();
            observation
        };
        let Ok(raw) = answer else {
            let mut observation =
                model_observation(guess, Some(WaitReason::JudgeUnavailable), Confidence::ZERO);
            observation.attention = true;
            observation.attention_reason = Some("judge_unavailable".to_string());
            return (observation, true);
        };
        // The raw answer is stored; the folded one is what counts.
        let judgement = if input.perpetual {
            fold_completed(&raw)
        } else {
            raw
        };
        let (state, confidence) = narrow(ambiguity, guess, &judgement);
        let mut observation = model_observation(state, wait_reason(state, input), confidence);
        observation.probabilities = raw.probabilities.map(|values| {
            SemanticState::ALL
                .iter()
                .zip(values)
                .map(|(state, value)| (state.key().to_string(), value))
                .collect::<BTreeMap<_, _>>()
        });
        observation.needs_human = judgement.needs_human;
        observation.stuck = judgement.stuck;
        // A process that exited with a failing status gets a look whatever
        // the model says about it.
        let failing_exit = ambiguity == Ambiguity::ExitedOnItsOwn && exited_failing(input);
        let reason = attention_reason(Some(ambiguity), state, confidence, &judgement)
            .or(failing_exit.then_some("failing_exit"));
        observation.attention = reason.is_some();
        observation.attention_reason = reason.map(str::to_string);
        (observation, false)
    }

    fn append_observation(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        observation: Observation,
    ) -> Result<(), StoreError> {
        self.log.append(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session.clone(),
            execution_id: None,
            source: Source::Observer,
            source_event_id: None,
            kind: EventKind::ObservationRecorded { observation },
        })?;
        Ok(())
    }
}

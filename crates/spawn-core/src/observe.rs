//! Observer: turns stored facts into semantic state, in two tiers.
//!
//! 1. [`rules`] decide every state the facts settle. No model is called.
//! 2. Only a case the rules mark [`Ambiguity`] goes to the configured
//!    [`StateJudge`] (Jev in production). Its answer is narrowed to the states
//!    the facts still allow, so a model cannot contradict a fact.
//!
//! The Observer never acts. It appends an `observation.recorded` event when
//! the judgement changes, with who decided, why the rules could not, and the
//! evidence the model saw. A judge failure keeps the rules' guess, marks it
//! `JudgeUnavailable` and asks for attention; it never reports "dead".

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use thiserror::Error;

use crate::event::{
    EndCause, Engine, EventKind, EventRecord, GlobalPosition, NewEvent, SessionSeq, Source,
};
use crate::id::{AgentId, SessionId};
use crate::observation::{Ambiguity, Confidence, Observation, SemanticState, WaitReason};
use crate::port::{EventLog, StoreError};
use crate::screen::ScreenState;
use crate::time::{Clock, SystemClock, Timestamp};

/// A live execution with no open tool and no fact for this long is ambiguous.
pub const QUIET_AFTER: Duration = Duration::from_mins(2);

/// A live session is classified again this often when no fact arrives, so a
/// silent hang is noticed.
pub const RECHECK_EVERY: Duration = Duration::from_secs(30);

/// Below this confidence a model judgement asks for attention.
pub const CONFIDENT: f64 = 0.6;

/// At or above this probability `needs_human` or `stuck` asks for attention.
pub const LIKELY: f64 = 0.5;

/// Characters of terminal tail sent to the model and kept as evidence.
const SCREEN_CHARS: usize = 2500;

/// Characters of the task sent to the model and kept as evidence.
const TASK_CHARS: usize = 1000;

/// Judge failure.
#[derive(Debug, Error)]
pub enum JudgeError {
    /// The judge could not produce a judgement.
    #[error("judge unavailable: {0}")]
    Unavailable(String),
}

/// Facts one judgement uses. The shape is the judge's input contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JudgeInput {
    /// Owning agent.
    pub agent_id: String,
    /// Judged session.
    pub session_id: String,
    /// Engine of the latest execution.
    pub engine: Option<Engine>,
    /// Whether an execution is running.
    pub execution_active: bool,
    /// How the latest execution ended.
    pub last_end: Option<EndCause>,
    /// Number of tool calls still open.
    pub open_tools: usize,
    /// Tool that started last.
    pub last_tool: Option<String>,
    /// Whether a permission dialog is open.
    pub permission_pending: bool,
    /// Latest state read from the engine pane.
    pub screen: Option<ScreenState>,
    /// State of the latest turn.
    pub turn: Option<TurnFact>,
    /// Latest engine error, truncated.
    pub last_error: Option<String>,
    /// Evidence sequence of the snapshot.
    pub evidence_seq: SessionSeq,
    /// Why the rules could not decide; set only for the model judge.
    pub ambiguity: Option<Ambiguity>,
    /// Milliseconds since the session's latest fact.
    pub quiet_ms: Option<u64>,
    /// The agent's task, as it was typed into the agent.
    pub task: Option<String>,
    /// Tail of the agent's terminal, as a person would see it.
    pub screen_tail: Option<String>,
}

/// How the latest turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFact {
    /// The model finished a turn.
    Stopped,
    /// The turn failed.
    Failed,
}

/// One judgement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Judgement {
    /// Judged state.
    pub state: SemanticState,
    /// Judge confidence.
    pub confidence: Confidence,
    /// Distribution over [`SemanticState::ALL`], in that order, when the
    /// judge gives one.
    pub probabilities: Option<[f64; 5]>,
    /// Probability that a human must answer or decide.
    pub needs_human: Option<f64>,
    /// Probability that the agent loops or makes no progress.
    pub stuck: Option<f64>,
}

impl Judgement {
    /// A judgement with a state and a confidence only.
    pub fn of(state: SemanticState, confidence: Confidence) -> Self {
        Self {
            state,
            confidence,
            probabilities: None,
            needs_human: None,
            stuck: None,
        }
    }
}

/// Component that classifies a snapshot.
pub trait StateJudge: Send + Sync {
    /// Judge name stored in the observation.
    fn name(&self) -> &'static str;

    /// Classify one snapshot.
    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError>;
}

/// Deterministic stand-in for the model judge.
///
/// It answers ambiguous cases when no Jev endpoint is configured, and keeps
/// the Observer testable without a model. It is not the rules tier: see
/// [`rules`].
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicJudge;

impl StateJudge for HeuristicJudge {
    fn name(&self) -> &'static str {
        "heuristic"
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        let confidence = |value: f64| Confidence::new(value).unwrap_or(Confidence::ZERO);
        if input.permission_pending || input.screen == Some(ScreenState::Permission) {
            return Ok(Judgement::of(SemanticState::Blocked, confidence(0.9)));
        }
        if input.execution_active {
            let state = if input.open_tools > 0 || input.screen == Some(ScreenState::Busy) {
                SemanticState::Working
            } else if input.turn == Some(TurnFact::Stopped)
                || input.screen == Some(ScreenState::Ready)
            {
                SemanticState::Waiting
            } else {
                SemanticState::Working
            };
            return Ok(Judgement::of(state, confidence(0.8)));
        }
        match input.last_end {
            Some(EndCause::Exited | EndCause::Stopped) => {
                Ok(Judgement::of(SemanticState::Completed, confidence(0.6)))
            }
            Some(EndCause::Lost) => Ok(Judgement::of(SemanticState::Unknown, confidence(0.3))),
            None => Ok(Judgement::of(SemanticState::Waiting, confidence(0.5))),
        }
    }
}

/// What the rules tier concluded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RulesVerdict {
    /// The facts settle the state.
    Certain {
        /// Decided state.
        state: SemanticState,
    },
    /// The facts leave several states open.
    Ambiguous {
        /// Why.
        ambiguity: Ambiguity,
        /// The rules' best guess, used when no model can answer.
        guess: SemanticState,
    },
}

/// The rules tier: decide what the facts settle, mark the rest ambiguous.
pub fn rules(input: &JudgeInput) -> RulesVerdict {
    use RulesVerdict::{Ambiguous, Certain};
    if input.permission_pending || input.screen == Some(ScreenState::Permission) {
        return Certain {
            state: SemanticState::Blocked,
        };
    }
    if input.execution_active {
        if input.open_tools > 0 || input.screen == Some(ScreenState::Busy) {
            return Certain {
                state: SemanticState::Working,
            };
        }
        if input.turn == Some(TurnFact::Failed) || input.screen == Some(ScreenState::Error) {
            return Ambiguous {
                ambiguity: Ambiguity::EngineError,
                guess: SemanticState::Working,
            };
        }
        if input.turn == Some(TurnFact::Stopped) || input.screen == Some(ScreenState::Ready) {
            return Ambiguous {
                ambiguity: Ambiguity::TurnStoppedIdle,
                guess: SemanticState::Waiting,
            };
        }
        if input
            .quiet_ms
            .is_some_and(|quiet| Duration::from_millis(quiet) >= QUIET_AFTER)
        {
            return Ambiguous {
                ambiguity: Ambiguity::QuietLive,
                guess: SemanticState::Working,
            };
        }
        // A turn is running and the latest fact is recent.
        return Certain {
            state: SemanticState::Working,
        };
    }
    match input.last_end {
        Some(EndCause::Stopped) => Certain {
            state: SemanticState::Completed,
        },
        Some(EndCause::Exited) => Ambiguous {
            ambiguity: Ambiguity::ExitedOnItsOwn,
            guess: SemanticState::Completed,
        },
        Some(EndCause::Lost) => Ambiguous {
            ambiguity: Ambiguity::Lost,
            guess: SemanticState::Unknown,
        },
        None => Certain {
            state: SemanticState::Waiting,
        },
    }
}

/// Narrow a model answer to the states the facts allow.
///
/// With a distribution, the allowed states are renormalized and the most
/// likely one wins. Without one, a disallowed answer falls back to the rules'
/// guess with zero confidence, which asks for attention.
pub fn narrow(
    ambiguity: Ambiguity,
    guess: SemanticState,
    judgement: &Judgement,
) -> (SemanticState, Confidence) {
    let allowed = ambiguity.allowed();
    if let Some(probabilities) = judgement.probabilities {
        let weight = |state: SemanticState| {
            SemanticState::ALL
                .iter()
                .position(|candidate| *candidate == state)
                .map_or(0.0, |index| probabilities[index].max(0.0))
        };
        let total: f64 = allowed.iter().map(|state| weight(*state)).sum();
        if total > 0.0 {
            let best = allowed
                .iter()
                .copied()
                .max_by(|left, right| weight(*left).total_cmp(&weight(*right)))
                .unwrap_or(guess);
            let confidence = Confidence::new(weight(best) / total).unwrap_or(Confidence::ZERO);
            return (best, confidence);
        }
        return (guess, Confidence::ZERO);
    }
    if allowed.contains(&judgement.state) {
        (judgement.state, judgement.confidence)
    } else {
        (guess, Confidence::ZERO)
    }
}

/// Whether an observation should draw a person's or a higher tier's eye.
pub fn needs_attention(
    state: SemanticState,
    confidence: Confidence,
    needs_human: Option<f64>,
    stuck: Option<f64>,
) -> bool {
    matches!(state, SemanticState::Blocked | SemanticState::Unknown)
        || confidence.value() < CONFIDENT
        || needs_human.is_some_and(|value| value >= LIKELY)
        || stuck.is_some_and(|value| value >= LIKELY)
}

/// Context the model judge needs and the log does not hold.
pub trait ObservationContext: Send + Sync {
    /// Tail of the session's terminal, if one runs.
    fn screen_tail(&self, session: &SessionId) -> Option<String>;

    /// The latest task typed into the session.
    fn task(&self, session: &SessionId) -> Option<String>;
}

fn tail_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(limit)).collect()
}

fn head_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Facts the Observer keeps for one session.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    engine: Option<Engine>,
    execution_active: bool,
    last_end: Option<EndCause>,
    open_tools: HashSet<String>,
    last_tool: Option<String>,
    permission_pending: bool,
    screen: Option<ScreenState>,
    turn: Option<TurnFact>,
    last_error: Option<String>,
    evidence_seq: SessionSeq,
    last_fact_at: Option<Timestamp>,
}
impl Snapshot {
    /// Apply one event.
    fn apply(&mut self, record: &EventRecord) {
        self.evidence_seq = record.session_seq;
        self.last_fact_at = Some(record.recorded_at);
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
            EventKind::ExecutionStarted { engine, .. } => {
                self.engine = Some(*engine);
                self.execution_active = true;
                self.last_end = None;
                self.open_tools.clear();
                self.permission_pending = false;
                self.screen = None;
                self.turn = None;
            }
            EventKind::ExecutionEnded { cause, .. } => {
                self.execution_active = false;
                self.last_end = Some(*cause);
                self.open_tools.clear();
            }
            EventKind::NativeSessionStarted { engine, .. } => {
                self.engine = Some(*engine);
            }
            EventKind::ToolStarted {
                tool_name,
                tool_use_id,
            } => {
                self.last_tool = Some(tool_name.clone());
                self.open_tools.insert(tool_use_id.clone());
            }
            EventKind::ToolCompleted { tool_use_id, .. }
            | EventKind::ToolFailed { tool_use_id, .. } => {
                self.open_tools.remove(tool_use_id);
                if let EventKind::ToolFailed { error, .. } = &record.event.kind {
                    self.last_error = Some(error.clone());
                }
            }
            EventKind::TurnStopped { .. } => {
                self.turn = Some(TurnFact::Stopped);
                self.open_tools.clear();
            }
            EventKind::TurnFailed { error } => {
                self.turn = Some(TurnFact::Failed);
                self.last_error = Some(error.clone());
                self.open_tools.clear();
            }
            EventKind::NotificationReceived { notification_type } => {
                self.permission_pending = notification_type.contains("permission");
            }
            EventKind::ScreenObserved { state, .. } => {
                self.screen = Some(*state);
                if *state == ScreenState::Permission {
                    self.permission_pending = true;
                }
                if *state == ScreenState::Ready {
                    self.permission_pending = false;
                }
            }
            EventKind::MessageReceived => {
                self.turn = None;
            }
            _ => {}
        }
    }

    fn input(&self, agent_id: &AgentId, session_id: &SessionId) -> JudgeInput {
        JudgeInput {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            engine: self.engine,
            execution_active: self.execution_active,
            last_end: self.last_end,
            open_tools: self.open_tools.len(),
            last_tool: self.last_tool.clone(),
            permission_pending: self.permission_pending,
            screen: self.screen,
            turn: self.turn,
            last_error: self.last_error.clone(),
            evidence_seq: self.evidence_seq,
            ambiguity: None,
            quiet_ms: None,
            task: None,
            screen_tail: None,
        }
    }
}

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

/// Per-session observation state.
#[derive(Debug)]
struct Tracked {
    agent_id: AgentId,
    snapshot: Snapshot,
    last: Option<WrittenKey>,
    judged_seq: SessionSeq,
    judged_at: Option<Timestamp>,
    /// Evidence of the last model call; an identical one is not asked again.
    asked: Option<u64>,
    retry: bool,
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
            .filter(|(_, tracked)| {
                tracked.retry
                    || tracked.snapshot.evidence_seq > tracked.judged_seq
                    || (tracked.snapshot.execution_active
                        && tracked
                            .judged_at
                            .is_none_or(|at| now.since(at) >= RECHECK_EVERY))
            })
            .map(|(session, _)| session.clone())
            .collect();
        let mut written = 0;
        for session in pending {
            written += self.observe(&session, now)?;
        }
        Ok(written)
    }

    fn catch_up(&mut self) -> Result<(), StoreError> {
        const BATCH: usize = 200;
        loop {
            let batch = self.log.read_global(self.cursor, BATCH)?;
            if batch.is_empty() {
                return Ok(());
            }
            for record in &batch {
                let tracked = self
                    .sessions
                    .entry(record.event.session_id.clone())
                    .or_insert_with(|| Tracked {
                        agent_id: record.event.agent_id.clone(),
                        snapshot: Snapshot::default(),
                        last: None,
                        judged_seq: SessionSeq(0),
                        judged_at: None,
                        asked: None,
                        retry: false,
                    });
                // The Observer's own output restores what it last wrote, so a
                // new process does not write the same observation again.
                if let EventKind::ObservationRecorded { observation } = &record.event.kind {
                    tracked.last = Some(key_of(observation));
                    tracked.judged_seq = observation.based_on_seq;
                    tracked.judged_at = Some(record.recorded_at);
                    tracked.retry = observation.reason == Some(WaitReason::JudgeUnavailable);
                } else if record.event.source != Source::Observer {
                    tracked.snapshot.apply(record);
                }
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
        let mut input = tracked.snapshot.input(&tracked.agent_id, session);
        input.quiet_ms = tracked
            .snapshot
            .last_fact_at
            .map(|at| u64::try_from(now.since(at).as_millis()).unwrap_or(u64::MAX));
        let agent = tracked.agent_id.clone();
        let seq = tracked.snapshot.evidence_seq;
        let retry = tracked.retry;
        let previous_ask = tracked.asked;

        let (observation, asked, retry_next) = match rules(&input) {
            RulesVerdict::Certain { state } => {
                let mut observation = Observation::basic(
                    state,
                    reason_for(state, &input),
                    seq,
                    Confidence::new(1.0).unwrap_or(Confidence::ZERO),
                    "rules",
                );
                observation.attention = state == SemanticState::Blocked;
                (observation, None, false)
            }
            RulesVerdict::Ambiguous { ambiguity, guess } => {
                input.ambiguity = Some(ambiguity);
                if let Some(context) = &self.context {
                    input.screen_tail = context
                        .screen_tail(session)
                        .map(|text| tail_chars(text.trim_end(), SCREEN_CHARS));
                    input.task = context
                        .task(session)
                        .map(|text| head_chars(&text, TASK_CHARS));
                }
                let mut hasher = DefaultHasher::new();
                (ambiguity, seq.0, &input.screen_tail, &input.task).hash(&mut hasher);
                let fingerprint = hasher.finish();
                if !retry && previous_ask == Some(fingerprint) {
                    // Nothing new to show the model; keep the last answer.
                    if let Some(tracked) = self.sessions.get_mut(session) {
                        tracked.judged_seq = seq;
                        tracked.judged_at = Some(now);
                    }
                    return Ok(0);
                }
                let (observation, retry_next) = self.ask_model(&input, ambiguity, guess, seq);
                let asked = (!retry_next).then_some(fingerprint);
                (observation, asked, retry_next)
            }
        };

        let key = key_of(&observation);
        let changed = self
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
            tracked.retry = retry_next;
        }
        Ok(written)
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
        let latency = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let evidence = serde_json::to_value(input).ok();
        let Ok(judgement) = answer else {
            let mut observation = Observation::basic(
                guess,
                Some(WaitReason::JudgeUnavailable),
                seq,
                Confidence::ZERO,
                self.judge.name(),
            );
            observation.ambiguity = Some(ambiguity);
            observation.rules_state = Some(guess);
            observation.latency_ms = Some(latency);
            observation.attention = true;
            observation.evidence = evidence;
            return (observation, true);
        };
        let (state, confidence) = narrow(ambiguity, guess, &judgement);
        let mut observation = Observation::basic(
            state,
            reason_for(state, input),
            seq,
            confidence,
            self.judge.name(),
        );
        observation.ambiguity = Some(ambiguity);
        observation.rules_state = Some(guess);
        observation.probabilities = judgement.probabilities.map(|values| {
            SemanticState::ALL
                .iter()
                .zip(values)
                .map(|(state, value)| (state.key().to_string(), value))
                .collect::<BTreeMap<_, _>>()
        });
        observation.needs_human = judgement.needs_human;
        observation.stuck = judgement.stuck;
        observation.latency_ms = Some(latency);
        observation.attention =
            needs_attention(state, confidence, judgement.needs_human, judgement.stuck);
        observation.evidence = evidence;
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

fn reason_for(state: SemanticState, input: &JudgeInput) -> Option<WaitReason> {
    match state {
        SemanticState::Blocked if input.permission_pending => {
            Some(WaitReason::WaitingForUserPermission)
        }
        SemanticState::Waiting => Some(WaitReason::WaitingForUserInput),
        _ => None,
    }
}

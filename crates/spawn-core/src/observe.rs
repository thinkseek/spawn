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
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
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

/// A tool open this long with no other fact is ambiguous: it may hang.
pub const LONG_TOOL: Duration = Duration::from_mins(10);

/// A busy screen reading older than this no longer proves work: the screen is
/// read only when a command runs, so a hang would keep it forever.
pub const SCREEN_STALE: Duration = QUIET_AFTER;

/// First pause before asking an unavailable judge again; doubles per failure.
pub const RETRY_BASE: Duration = Duration::from_secs(30);

/// Longest pause between retries of an unavailable judge.
pub const RETRY_MAX: Duration = Duration::from_mins(10);

/// Without a distribution, below this confidence a model judgement asks for
/// attention.
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
///
/// It is stored as the evidence of every model judgement and read back for
/// replay, so every field defaults: a row written before a field existed
/// still replays.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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
    /// Exit status of the latest execution, when its process reported one.
    pub exit_code: Option<i32>,
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
    /// The session is meant never to end (the supervisor): it waits for
    /// instructions rather than completing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub perpetual: bool,
}

/// How the latest turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    // A permission dialog is a fact only while its pane exists.
    if input.execution_active
        && (input.permission_pending || input.screen == Some(ScreenState::Permission))
    {
        return Certain {
            state: SemanticState::Blocked,
        };
    }
    if input.execution_active {
        let quiet_for = |limit: Duration| {
            input
                .quiet_ms
                .is_some_and(|quiet| Duration::from_millis(quiet) >= limit)
        };
        if input.open_tools > 0 && quiet_for(LONG_TOOL) {
            return Ambiguous {
                ambiguity: Ambiguity::QuietLive,
                guess: SemanticState::Working,
            };
        }
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
        if quiet_for(QUIET_AFTER) {
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
        // A failing exit status makes "it got stuck" the safer guess.
        Some(EndCause::Exited) => Ambiguous {
            ambiguity: Ambiguity::ExitedOnItsOwn,
            guess: if input.exit_code.is_some_and(|code| code != 0) {
                SemanticState::Blocked
            } else {
                SemanticState::Completed
            },
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
            // On a tie the rules' guess wins: it is the documented fallback.
            let best = allowed
                .iter()
                .copied()
                .max_by(|left, right| {
                    weight(*left)
                        .total_cmp(&weight(*right))
                        .then_with(|| (*left == guess).cmp(&(*right == guess)))
                })
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
///
/// What matters is the chance that someone is needed, not how sure the model
/// is about which harmless state applies: WAITING 0.56 against COMPLETED 0.40
/// is an idle agent either way and must not page anyone. With a distribution,
/// the mass on BLOCKED and UNKNOWN among the states the facts allow decides;
/// without one, a low confidence does. When most of the model's mass sits on
/// states the facts rule out, the model and the facts disagree, which is
/// itself worth a look.
pub fn needs_attention(
    ambiguity: Option<Ambiguity>,
    state: SemanticState,
    confidence: Confidence,
    judgement: &Judgement,
) -> bool {
    attention_reason(ambiguity, state, confidence, judgement).is_some()
}

/// Why [`needs_attention`] holds, in one word, or `None`.
pub fn attention_reason(
    ambiguity: Option<Ambiguity>,
    state: SemanticState,
    confidence: Confidence,
    judgement: &Judgement,
) -> Option<&'static str> {
    match state {
        SemanticState::Blocked => return Some("blocked"),
        SemanticState::Unknown => return Some("unknown"),
        _ => {}
    }
    if judgement.needs_human.is_some_and(|value| value >= LIKELY) {
        return Some("needs_human");
    }
    if judgement.stuck.is_some_and(|value| value >= LIKELY) {
        return Some("stuck");
    }
    let Some(probabilities) = judgement.probabilities else {
        return (confidence.value() < CONFIDENT).then_some("low_confidence");
    };
    let allowed = ambiguity.map_or(&SemanticState::ALL[..], Ambiguity::allowed);
    let weight = |candidate: &SemanticState| {
        SemanticState::ALL
            .iter()
            .position(|known| known == candidate)
            .map_or(0.0, |index| probabilities[index].max(0.0))
    };
    let total: f64 = allowed.iter().map(weight).sum();
    let everything: f64 = SemanticState::ALL.iter().map(weight).sum();
    if total <= 0.0 || total < everything * LIKELY {
        return Some("disagreement");
    }
    let risky: f64 = allowed
        .iter()
        .filter(|candidate| matches!(candidate, SemanticState::Blocked | SemanticState::Unknown))
        .map(weight)
        .sum();
    (risky / total >= LIKELY).then_some("risky")
}

/// For a session that never ends (the supervisor), COMPLETED only means it
/// waits for its next instruction: fold that mass into WAITING.
fn fold_completed(judgement: &Judgement) -> Judgement {
    let index = |wanted: SemanticState| {
        SemanticState::ALL
            .iter()
            .position(|state| *state == wanted)
            .unwrap_or(0)
    };
    let mut folded = *judgement;
    if let Some(mut probabilities) = folded.probabilities {
        let (waiting, completed) = (index(SemanticState::Waiting), index(SemanticState::Completed));
        probabilities[waiting] += probabilities[completed];
        probabilities[completed] = 0.0;
        folded.probabilities = Some(probabilities);
    }
    if folded.state == SemanticState::Completed {
        folded.state = SemanticState::Waiting;
    }
    folded
}

/// Context the model judge needs and the log does not hold.
pub trait ObservationContext: Send + Sync {
    /// Tail of the session's terminal, if one runs.
    fn screen_tail(&self, session: &SessionId) -> Option<String>;

    /// The latest task typed into the session.
    fn task(&self, session: &SessionId) -> Option<String>;

    /// Whether the session is meant never to end (the supervisor). Its
    /// COMPLETED is read as WAITING.
    fn perpetual(&self, _session: &SessionId) -> bool {
        false
    }
}

/// Whether the screen ends with a question for a person: a line that starts
/// with `HUMAN:` (after an optional `claude:` speaker tag), below the last
/// line the user typed. An answered question no longer counts.
pub fn asks_human(screen_tail: &str) -> bool {
    human_question(screen_tail).is_some()
}

/// The last unanswered `HUMAN:` question on the screen, without the tag.
pub fn human_question(screen_tail: &str) -> Option<String> {
    let lines: Vec<&str> = screen_tail.lines().collect();
    let start = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("you:"))
        .map_or(0, |index| index + 1);
    lines[start..].iter().rev().find_map(|line| {
        let text = line.trim_start();
        let text = text.strip_prefix("claude:").map_or(text, str::trim_start);
        text.strip_prefix("HUMAN:")
            .map(|question| head_chars(question.trim(), 300))
    })
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
    screen_at: Option<Timestamp>,
    turn: Option<TurnFact>,
    last_error: Option<String>,
    exit_code: Option<i32>,
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
                self.exit_code = None;
                self.open_tools.clear();
                self.permission_pending = false;
                self.screen = None;
                self.turn = None;
            }
            EventKind::ExecutionEnded { cause, exit_code } => {
                self.execution_active = false;
                self.last_end = Some(*cause);
                self.exit_code = *exit_code;
                self.open_tools.clear();
                // The pane is gone with the execution; nothing on it is a
                // current fact any more.
                self.screen = None;
                self.permission_pending = false;
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
            }
            // The Bridge reads the pane as ready only right before it types
            // input. A command that then succeeds delivered that input: the
            // ready screen is consumed and a new turn begins, even though the
            // engine's own hook for it may land a moment later.
            EventKind::CommandFinished {
                succeeded: true, ..
            } if self.screen == Some(ScreenState::Ready) => {
                self.screen = None;
                self.turn = None;
            }
            _ => {}
        }
    }

    fn input(&self, agent_id: &AgentId, session_id: &SessionId, now: Timestamp) -> JudgeInput {
        let stale = self
            .screen_at
            .is_none_or(|at| now.since(at) >= SCREEN_STALE);
        let screen = match self.screen {
            Some(ScreenState::Busy) if stale => None,
            other => other,
        };
        JudgeInput {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            engine: self.engine,
            execution_active: self.execution_active,
            last_end: self.last_end,
            exit_code: self.exit_code,
            open_tools: self.open_tools.len(),
            last_tool: self.last_tool.clone(),
            permission_pending: self.permission_pending,
            screen,
            turn: self.turn,
            last_error: self.last_error.clone(),
            evidence_seq: self.evidence_seq,
            ambiguity: None,
            quiet_ms: None,
            task: None,
            screen_tail: None,
            perpetual: false,
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

#[cfg(test)]
mod tests {
    use super::asks_human;

    #[test]
    fn a_question_for_a_person_is_seen_until_answered() {
        assert!(asks_human(
            "you: deploy it\nclaude: Done with the build.\nclaude: HUMAN: which region?\n$ "
        ));
        assert!(asks_human(
            "claude: The key is missing.\nHUMAN: please add it\n"
        ));
        assert!(!asks_human(
            "claude: HUMAN: which region?\nyou: eu-west\nclaude: deploying\n"
        ));
        assert!(!asks_human(
            "claude: a human should look at the HUMAN: tag docs\n"
        ));
    }
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

/// Pause before asking an unavailable judge again after `failures` failures.
fn retry_after(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(8);
    RETRY_BASE.saturating_mul(1 << doublings).min(RETRY_MAX)
}

/// Stable digest of what the model is shown (FNV-1a over its JSON), so a
/// stored fingerprint still matches after a restart or a rebuild.
fn fingerprint_of(input: &JudgeInput) -> String {
    let shown = serde_json::json!([
        input.ambiguity,
        input.evidence_seq.0,
        input.exit_code,
        input.screen_tail,
        input.task,
        input.perpetual,
    ]);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in shown.to_string().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
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
                if tracked.snapshot.evidence_seq > tracked.judged_seq {
                    return true;
                }
                let elapsed =
                    |pause: Duration| tracked.judged_at.is_none_or(|at| now.since(at) >= pause);
                // While the judge fails, its backoff replaces the recheck.
                if tracked.failures > 0 {
                    elapsed(retry_after(tracked.failures))
                } else {
                    tracked.snapshot.execution_active && elapsed(RECHECK_EVERY)
                }
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
                        failures: 0,
                    });
                // The Observer's own output restores what it last wrote and
                // what it last asked, so a new process neither writes the same
                // observation again nor asks the model about the same evidence.
                // Only the Observer's rows count; anything else claiming to be
                // an observation is ignored.
                if record.event.source == Source::Observer {
                    if let EventKind::ObservationRecorded { observation } = &record.event.kind {
                        // `judged_seq` is not restored: a new process checks
                        // every session once. A rules case costs nothing and
                        // writes only if the state changed; a model case is
                        // skipped by its fingerprint. A row from an older
                        // build that the current rules disagree with is thus
                        // corrected instead of trusted forever.
                        tracked.last = Some(key_of(observation));
                        tracked.judged_at = Some(record.recorded_at);
                        // The live process also reads back its own rows, so
                        // this records that the judge failed, not how often.
                        if observation.reason == Some(WaitReason::JudgeUnavailable) {
                            tracked.failures = tracked.failures.max(1);
                        } else {
                            tracked.failures = 0;
                        }
                        if observation.fingerprint.is_some() {
                            tracked.asked.clone_from(&observation.fingerprint);
                        }
                    }
                } else if !matches!(record.event.kind, EventKind::ObservationRecorded { .. }) {
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
        let mut input = tracked.snapshot.input(&tracked.agent_id, session, now);
        input.quiet_ms = tracked
            .snapshot
            .last_fact_at
            .map(|at| u64::try_from(now.since(at).as_millis()).unwrap_or(u64::MAX));
        let agent = tracked.agent_id.clone();
        let seq = tracked.snapshot.evidence_seq;
        let failures = tracked.failures;
        let previous_ask = tracked.asked.clone();

        let (observation, asked, failures_next) = match rules(&input) {
            RulesVerdict::Certain { state } => {
                let mut observation = Observation::basic(
                    state,
                    reason_for(state, &input),
                    seq,
                    Confidence::new(1.0).unwrap_or(Confidence::ZERO),
                    "rules",
                );
                observation.attention = state == SemanticState::Blocked;
                if observation.attention {
                    observation.attention_reason = Some("permission".to_string());
                }
                (observation, None, 0)
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
                input.perpetual = self
                    .context
                    .as_ref()
                    .is_some_and(|context| context.perpetual(session));
                let fingerprint = fingerprint_of(&input);
                if failures == 0 && previous_ask.as_deref() == Some(fingerprint.as_str()) {
                    // Nothing new to show the model; keep the last answer.
                    if let Some(tracked) = self.sessions.get_mut(session) {
                        tracked.judged_seq = seq;
                        tracked.judged_at = Some(now);
                    }
                    return Ok(0);
                }
                let (mut observation, failed) =
                    self.ask_model(&input, ambiguity, guess, seq, input.perpetual);
                if let Some(question) = input.screen_tail.as_deref().and_then(human_question) {
                    observation.human_question = Some(question);
                    observation.attention = true;
                    observation.attention_reason = Some("human_question".to_string());
                }
                if failed {
                    (observation, previous_ask, failures.saturating_add(1))
                } else {
                    observation.fingerprint = Some(fingerprint.clone());
                    (observation, Some(fingerprint), 0)
                }
            }
        };

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
            tracked.failures = failures_next;
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
        perpetual: bool,
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
            observation.attention_reason = Some("judge_unavailable".to_string());
            observation.evidence = evidence;
            return (observation, true);
        };
        // The raw answer is stored; the folded one is what counts.
        let raw = judgement;
        let judgement = if perpetual {
            fold_completed(&raw)
        } else {
            raw
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
        observation.probabilities = raw.probabilities.map(|values| {
            SemanticState::ALL
                .iter()
                .zip(values)
                .map(|(state, value)| (state.key().to_string(), value))
                .collect::<BTreeMap<_, _>>()
        });
        observation.needs_human = judgement.needs_human;
        observation.stuck = judgement.stuck;
        observation.latency_ms = Some(latency);
        // A process that exited with a failing status gets a look whatever
        // the model says about it.
        let failing_exit =
            ambiguity == Ambiguity::ExitedOnItsOwn && input.exit_code.is_some_and(|code| code != 0);
        let reason = attention_reason(Some(ambiguity), state, confidence, &judgement)
            .or(failing_exit.then_some("failing_exit"));
        observation.attention = reason.is_some();
        observation.attention_reason = reason.map(str::to_string);
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

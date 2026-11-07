//! The Observer loop: read the log, keep per-session facts, classify
//! changed or due sessions and append an observation so that a session's
//! latest judgement covers its latest fact. Also what the model is shown
//! beyond the log (the [`ObservationContext`]) and how a repeated question is
//! recognised.
//!
//! The coverage rule: an observation is written at once when the judgement
//! changes (state, reason, attention, ambiguity) or a model answers. When a
//! later fact of the session (any record not the Observer's: a screen
//! reading, a Bridge step, a checkpoint, a command) leaves the verdict as it
//! was, the session is judged again and, once its facts have settled (no
//! newer fact for [`SETTLE`]), that verdict is written with `based_on_seq` at
//! the latest fact. It is a judgement of those facts, not a copy stamped
//! forward: the rules run again, and a model answer stands only for the same
//! evidence (its fingerprint), so the model is not asked twice about it. A
//! session with no new fact gets nothing new; a burst of facts with an
//! unchanged verdict gets one observation once it settles. The dispatcher
//! refuses a judgement older than the latest fact, so without this an idle
//! worker whose last fact changed nothing would never be acted on.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::classify::{
    RulesVerdict, attention_reason, exited_failing, fold_completed, narrow, rules, wait_reason,
};
use super::judge::{JudgeInput, StateJudge};
use super::snapshot::Snapshot;
use super::{RECHECK_EVERY, RETRY_BASE, RETRY_MAX, SETTLE};
use crate::event::{EventKind, EventRecord, GlobalPosition, NewEvent, SessionSeq, Source};
use crate::id::{AgentId, SessionId, WorkspaceId};
use crate::observation::{Ambiguity, Confidence, Observation, SemanticState, WaitReason};
use crate::port::{EventLog, StoreError};
use crate::time::{Clock, SystemClock, Timestamp};

/// Events read from the log per batch.
const BATCH: usize = 200;

/// Characters of terminal tail sent to the model and kept as evidence.
const SCREEN_CHARS: usize = 2500;

/// Characters of the task sent to the model and kept as evidence.
const TASK_CHARS: usize = 1000;

/// Characters of a `HUMAN:` question kept on the observation.
const QUESTION_CHARS: usize = 300;

/// What the Observer last wrote for a session. A new observation is written
/// at once when this changes; with it unchanged, only to cover newer facts
/// once they settle.
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
    /// The latest fact the Observer's latest observation is based on.
    covered: SessionSeq,
    /// When the latest fact was recorded, by the log's clock.
    fact_at: Option<Timestamp>,
    /// When this process read the latest fact, by its own clock: a guard
    /// against a log clock ahead of it.
    fact_seen: Option<Timestamp>,
    /// Evidence of the last model answer; an identical one is not asked again.
    asked: Option<String>,
    /// The last model answer, which stands for newer facts with the same
    /// evidence.
    answer: Option<Observation>,
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
            covered: SessionSeq(0),
            fact_at: None,
            fact_seen: None,
            asked: None,
            answer: None,
            failures: 0,
        }
    }

    /// Whether the latest observation is older than the latest fact.
    fn lagging(&self) -> bool {
        self.snapshot.evidence_seq > self.covered
    }

    /// Whether no new fact has come for [`SETTLE`]: by the log's clock (so a
    /// one-shot `spawn tick` sees old facts as settled), or by this
    /// process's (so a log clock ahead of it does not hold coverage back).
    fn settled(&self, now: Timestamp) -> bool {
        let quiet = |at: Option<Timestamp>| at.is_some_and(|at| now.since(at) >= SETTLE);
        quiet(self.fact_at) || quiet(self.fact_seen)
    }

    /// Whether the session must be classified at `now`: it has new facts,
    /// its settled facts are not covered yet, or a recheck or a judge retry
    /// is due.
    fn is_due(&self, now: Timestamp) -> bool {
        if self.snapshot.evidence_seq > self.judged_seq {
            return true;
        }
        if self.lagging() && self.settled(now) {
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

    /// The earliest time at `now` (or before it) this session is due, by the
    /// rules [`Self::is_due`] applies: a time this process waits for, never
    /// a reason to read the log. `None` while nothing about the session
    /// waits on the clock.
    fn due_at(&self, now: Timestamp) -> Option<Timestamp> {
        if self.snapshot.evidence_seq > self.judged_seq {
            return Some(now);
        }
        let mut due: Option<Timestamp> = None;
        let mut soonest = |at: Timestamp| {
            due = Some(due.map_or(at, |known: Timestamp| known.min(at)));
        };
        if self.lagging() {
            // The facts settle when either clock says `SETTLE` has passed
            // since the last one, so the earlier of the two is the wait.
            let mut settle: Option<Timestamp> = None;
            for at in [self.fact_at, self.fact_seen].into_iter().flatten() {
                let at = at.plus(SETTLE);
                settle = Some(settle.map_or(at, |known: Timestamp| known.min(at)));
            }
            if let Some(at) = settle {
                soonest(at);
            }
        }
        match self.judged_at {
            Some(at) if self.failures > 0 => soonest(at.plus(retry_after(self.failures))),
            Some(at) if self.snapshot.execution_active => soonest(at.plus(RECHECK_EVERY)),
            Some(_) => {}
            None => soonest(now),
        }
        due
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
        self.covered = self.covered.max(observation.based_on_seq);
        // The live process also reads back its own rows, so this records that
        // the judge failed, not how often.
        if observation.reason == Some(WaitReason::JudgeUnavailable) {
            self.failures = self.failures.max(1);
        } else {
            self.failures = 0;
        }
        if observation.fingerprint.is_some() {
            self.asked.clone_from(&observation.fingerprint);
            self.answer = Some(observation.clone());
        }
    }

    /// Fold one log record, read at `seen`, into the session.
    fn apply(&mut self, record: &EventRecord, seen: Timestamp) {
        // Only the Observer's rows count as observations; anything else
        // claiming to be one is ignored as a judgement, but is still a fact
        // of the session that the next judgement covers.
        if record.event.source == Source::Observer {
            if let EventKind::ObservationRecorded { observation } = &record.event.kind {
                self.restore(observation, record.recorded_at);
            }
            return;
        }
        if matches!(record.event.kind, EventKind::ObservationRecorded { .. }) {
            self.snapshot.note_seq(record);
        } else {
            self.snapshot.apply(record);
        }
        self.fact_at = Some(record.recorded_at);
        self.fact_seen = Some(seen);
    }
}

/// The outcome of classifying one session, before it is written.
struct Classified {
    observation: Observation,
    /// Fingerprint of the last model answer after this classification.
    asked: Option<String>,
    /// Whether a model answered now: every answer is written.
    answered: bool,
    /// Consecutive judge failures after this classification.
    failures: u32,
}

/// What an observation names as its judge when no model judge is
/// configured: the rules' guess stands, as when the judge is down.
pub const NO_JUDGE: &str = "none";

/// State observer: rules first, the model judge on ambiguity.
pub struct Observer {
    log: Arc<dyn EventLog>,
    /// The model judge; without one an ambiguous case keeps the rules'
    /// guess and asks for attention.
    judge: Option<Arc<dyn StateJudge>>,
    context: Option<Arc<dyn ObservationContext>>,
    clock: Arc<dyn Clock>,
    cursor: GlobalPosition,
    sessions: HashMap<SessionId, Tracked>,
}

impl Observer {
    /// Observer over one log that judges by the rules alone: a case they
    /// cannot settle is recorded as ambiguous, with their guess, and asks
    /// for attention ([`WaitReason::JudgeUnavailable`]).
    pub fn new(log: Arc<dyn EventLog>) -> Self {
        Self {
            log,
            judge: None,
            context: None,
            clock: Arc::new(SystemClock),
            cursor: GlobalPosition(0),
            sessions: HashMap::new(),
        }
    }

    /// Let `judge` answer the cases the rules cannot settle.
    #[must_use]
    pub fn with_judge(mut self, judge: Arc<dyn StateJudge>) -> Self {
        self.judge = Some(judge);
        self
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

    /// When the loop must run [`Self::tick`] again even if the log has not
    /// moved: the earliest time a session's own schedule waits for (its
    /// facts settling, a recheck of a running execution, a judge's backoff),
    /// on this clock. `None`: nothing waits on the clock.
    pub fn next_due_ms(&self) -> Option<i64> {
        let now = self.clock.now();
        self.sessions
            .values()
            .filter_map(|tracked| tracked.due_at(now))
            .map(|at| at.0)
            .min()
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
            let seen = self.clock.now();
            // The control stream is not an agent's session.
            for record in batch
                .iter()
                .filter(|record| !record.event.kind.is_control())
            {
                self.sessions
                    .entry(record.event.session_id.clone())
                    .or_insert_with(|| Tracked::new(record.event.agent_id.clone()))
                    .apply(record, seen);
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
                self.ambiguous(tracked, session, input, ambiguity, guess, seq)
            }
        };
        let Classified {
            observation,
            asked,
            answered,
            failures,
        } = classified;

        let key = key_of(&observation);
        // Every model answer is kept, with its evidence and fingerprint: it is
        // the record of what the model saw and said, and what a restarted
        // Observer uses to avoid asking again. A verdict that did not change
        // is written once the facts settle, if the latest observation is
        // older than them (the coverage rule).
        let (changed, covers) = self.sessions.get(session).map_or((true, false), |tracked| {
            (
                tracked.last != Some(key),
                tracked.lagging() && tracked.settled(now),
            )
        });
        let write = answered || changed || covers;
        let answer = (write && observation.fingerprint.is_some()).then(|| observation.clone());
        if write {
            self.append_observation(&agent, session, observation)?;
        }
        if let Some(tracked) = self.sessions.get_mut(session) {
            tracked.last = Some(key);
            tracked.judged_seq = seq;
            tracked.judged_at = Some(now);
            tracked.asked = asked;
            tracked.failures = failures;
            if write {
                tracked.covered = tracked.covered.max(seq);
            }
            if answer.is_some() {
                tracked.answer = answer;
            }
        }
        Ok(usize::from(write))
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
            let reason = if input.stalled_end.is_some() {
                "end_stalled"
            } else {
                "permission"
            };
            observation.attention_reason = Some(reason.to_string());
        }
        Classified {
            observation,
            asked: None,
            answered: false,
            failures: 0,
        }
    }

    /// An ambiguous case: attach the context and ask the model, unless it
    /// was already asked about exactly this evidence. Then its answer stands
    /// for the facts through `seq`, as it was given: same state, reason,
    /// attention and evidence, with no new call.
    fn ambiguous(
        &self,
        tracked: &Tracked,
        session: &SessionId,
        mut input: JudgeInput,
        ambiguity: Ambiguity,
        guess: SemanticState,
        seq: SessionSeq,
    ) -> Classified {
        input.ambiguity = Some(ambiguity);
        attach_context(
            &mut input,
            self.context.as_deref(),
            session,
            tracked.snapshot.transcript(),
        );
        let fingerprint = fingerprint_of(&input, tracked.snapshot.progress_seq);
        if tracked.failures == 0 && tracked.asked.as_deref() == Some(fingerprint.as_str()) {
            let standing = tracked
                .answer
                .as_ref()
                .filter(|answer| answer.fingerprint.as_deref() == Some(fingerprint.as_str()));
            if let Some(answer) = standing {
                let mut observation = answer.clone();
                observation.based_on_seq = seq;
                // No model was called for this one.
                observation.latency_ms = None;
                return Classified {
                    observation,
                    asked: tracked.asked.clone(),
                    answered: false,
                    failures: 0,
                };
            }
        }
        let (mut observation, failed) = self.ask_model(&input, ambiguity, guess, seq);
        if let Some(question) = input.reply.as_deref().and_then(human_question) {
            observation.human_question = Some(question);
            observation.attention = true;
            observation.attention_reason = Some("human_question".to_string());
        }
        if failed {
            Classified {
                observation,
                asked: tracked.asked.clone(),
                answered: false,
                failures: tracked.failures.saturating_add(1),
            }
        } else {
            observation.fingerprint = Some(fingerprint.clone());
            Classified {
                observation,
                asked: Some(fingerprint),
                answered: true,
                failures: 0,
            }
        }
    }

    /// Ask the model judge about an ambiguous case and narrow its answer.
    /// With no judge configured the case goes the way of a judge that is
    /// down: the rules' guess, `JudgeUnavailable`, attention.
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
        let answer = self
            .judge
            .as_ref()
            .and_then(|judge| judge.judge(input).ok());
        let latency = elapsed_ms(started);
        let name = self.judge.as_ref().map_or(NO_JUDGE, |judge| judge.name());
        let model_observation = |state, reason, confidence| {
            let mut observation = Observation::basic(state, reason, seq, confidence, name);
            observation.ambiguity = Some(ambiguity);
            observation.rules_state = Some(guess);
            observation.latency_ms = Some(latency);
            observation.evidence = serde_json::to_value(input).ok();
            observation
        };
        let Some(raw) = answer else {
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

/// Context the model judge needs and the log does not hold.
pub trait ObservationContext: Send + Sync {
    /// Tail of the session's terminal, if one runs.
    fn screen_tail(&self, session: &SessionId) -> Option<String>;

    /// The latest task typed into the session.
    fn task(&self, session: &SessionId) -> Option<String>;

    /// The model's latest reply: the text it wrote since the last prompt,
    /// read from its transcript at `transcript` in `workspace`
    /// ([`super::reply::Reply`]), not from the screen, where a tool's output
    /// looks the same.
    fn reply(&self, _workspace: &WorkspaceId, _transcript: &str) -> Option<String> {
        None
    }

    /// Whether the session is meant never to end (the supervisor). Its
    /// COMPLETED is read as WAITING.
    fn perpetual(&self, _session: &SessionId) -> bool {
        false
    }
}

/// Fill in what the context knows about `session`: the terminal tail and the
/// task, truncated, and whether the session never ends. Without a context
/// they stay empty.
fn attach_context(
    input: &mut JudgeInput,
    context: Option<&dyn ObservationContext>,
    session: &SessionId,
    transcript: Option<(&WorkspaceId, &str)>,
) {
    let Some(context) = context else {
        input.perpetual = false;
        return;
    };
    input.screen_tail = context
        .screen_tail(session)
        .map(|text| tail_chars(text.trim_end(), SCREEN_CHARS));
    input.task = context
        .task(session)
        .map(|text| head_chars(&text, TASK_CHARS));
    input.reply = transcript.and_then(|(workspace, path)| context.reply(workspace, path));
    input.perpetual = context.perpetual(session);
}

/// The question for a person in the model's latest reply, without the tag:
/// its last line that starts with `HUMAN:`. The reply is what the model
/// wrote since the last prompt, so a question someone answered is no longer
/// in it.
fn human_question(reply: &str) -> Option<String> {
    reply.lines().rev().find_map(|line| {
        line.trim_start()
            .strip_prefix("HUMAN:")
            .map(|question| head_chars(question.trim(), QUESTION_CHARS))
    })
}

/// Stable digest of what the model is shown (FNV-1a over its JSON), so a
/// stored fingerprint still matches after a restart. Digits on the screen are
/// left out: a ticking timer or token count is not a new situation, and must
/// not ask the model again. Nor is a fact that changes none of what is shown
/// (a screen reading that confirms the last one, a checkpoint): the facts
/// count by `progress`, the engine's latest move, and by what they say.
fn fingerprint_of(input: &JudgeInput, progress: SessionSeq) -> String {
    let screen = input.screen_tail.as_deref().map(|tail| {
        tail.chars()
            .filter(|c| !c.is_ascii_digit())
            .collect::<String>()
    });
    let shown = serde_json::json!([
        input.ambiguity,
        progress.0,
        input.exit_code,
        screen,
        input.task,
        input.perpetual,
        input.screen,
        input.turn,
        input.open_tools,
        input.permission_pending,
        input.execution_active,
        input.stalled_end,
        input.last_end,
    ]);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in shown.to_string().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The last `limit` characters of `text`.
fn tail_chars(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(limit)).collect()
}

/// The first `limit` characters of `text`.
fn head_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::human_question;

    fn asks_human(screen: &str) -> bool {
        human_question(screen).is_some()
    }

    #[test]
    fn a_ticking_timer_is_not_a_new_situation() {
        let mut input = crate::observe::JudgeInput {
            screen_tail: Some("✽ Working… (12s · 1.2k tokens)".to_string()),
            ..crate::observe::JudgeInput::default()
        };
        let first = super::fingerprint_of(&input, crate::event::SessionSeq(1));
        input.screen_tail = Some("✽ Working… (47s · 3.9k tokens)".to_string());
        assert_eq!(
            super::fingerprint_of(&input, crate::event::SessionSeq(1)),
            first
        );
        input.screen_tail = Some("HUMAN: which region?".to_string());
        assert_ne!(
            super::fingerprint_of(&input, crate::event::SessionSeq(1)),
            first
        );
    }

    #[test]
    fn a_question_for_a_person_is_a_line_of_the_models_reply() {
        assert!(asks_human("Done with the build.\nHUMAN: which region?"));
        assert!(asks_human("The key is missing.\n  HUMAN: please add it"));
        assert!(!asks_human("a human should look at the HUMAN: tag docs"));
    }
}

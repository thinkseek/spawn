//! The Observer's two tiers: rules decide what the facts settle, the model
//! judge answers only ambiguous cases, and its answer never contradicts a fact.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spawn_core::event::EndCause;
use spawn_core::event::{ENGINE, EventKind, GlobalPosition, ModelProfile, NewEvent, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
use spawn_core::memory::MemoryStore;
use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState, WaitReason};
use spawn_core::observe::{
    JudgeError, JudgeInput, Judgement, ObservationContext, Observer, StateJudge, narrow,
};
use spawn_core::port::EventLog;
use spawn_core::screen::ScreenState;
use spawn_core::time::{ManualClock, Timestamp};

/// A model judge that answers from a script and counts its calls.
struct Scripted {
    calls: AtomicUsize,
    answer: Mutex<Result<Judgement, String>>,
    seen: Mutex<Option<JudgeInput>>,
}

impl Scripted {
    fn new(answer: Result<Judgement, String>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            answer: Mutex::new(answer),
            seen: Mutex::new(None),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl StateJudge for Scripted {
    fn name(&self) -> &'static str {
        "jev"
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.seen.lock().expect("lock") = Some(input.clone());
        self.answer
            .lock()
            .expect("lock")
            .clone()
            .map_err(JudgeError::Unavailable)
    }
}

/// Terminal and task the way the CLI's context would supply them.
struct Context;

impl ObservationContext for Context {
    fn screen_tail(&self, _session: &SessionId) -> Option<String> {
        Some("claude: Should I delete the old parser? Please confirm.\n$ ".to_string())
    }

    fn task(&self, _session: &SessionId) -> Option<String> {
        Some("Speed up ingest".to_string())
    }
}

struct Fixture {
    clock: Arc<ManualClock>,
    store: Arc<MemoryStore>,
    observer: Observer,
}

fn fixture(judge: Arc<Scripted>) -> Fixture {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock.clone()));
    let log: Arc<dyn EventLog> = store.clone();
    let observer = Observer::new(log)
        .with_judge(judge)
        .with_clock(clock.clone())
        .with_context(Arc::new(Context));
    Fixture {
        clock,
        store,
        observer,
    }
}

fn agent() -> AgentId {
    AgentId::new("agent_one").expect("valid agent")
}

fn session() -> SessionId {
    SessionId::new("session_one").expect("valid session")
}

fn append(store: &MemoryStore, source: Source, kind: EventKind) {
    store
        .append(NewEvent {
            agent_id: agent(),
            session_id: session(),
            execution_id: Some(ExecutionId::for_command(CommandId(1))),
            source,
            source_event_id: None,
            kind,
        })
        .expect("append succeeds");
}

fn started(store: &MemoryStore) {
    append(
        store,
        Source::Bridge,
        EventKind::ExecutionStarted {
            engine: ENGINE.to_string(),
            workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
            command_id: CommandId(1),
            fork: None,
            model: ModelProfile::Default,
            transcript_from: None,
        },
    );
}

/// The engine named its native session and transcript.
fn native(store: &MemoryStore) {
    append(
        store,
        Source::Bridge,
        EventKind::NativeSessionStarted {
            engine: ENGINE.to_string(),
            native_session_id: "n1".to_string(),
            transcript_path: Some("/root/.claude/projects/w/n1.jsonl".to_string()),
            engine_version: None,
        },
    );
}

fn turn_stopped(store: &MemoryStore) {
    append(
        store,
        Source::Claude,
        EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
    );
}

fn observations(store: &MemoryStore) -> Vec<Observation> {
    store
        .read_global(GlobalPosition(0), 500)
        .expect("read succeeds")
        .into_iter()
        .filter_map(|record| match record.event.kind {
            EventKind::ObservationRecorded { observation } => Some(observation),
            _ => None,
        })
        .collect()
}

fn confidence(value: f64) -> Confidence {
    Confidence::new(value).expect("valid confidence")
}

/// Jev's answer to an agent that asked a question: mostly BLOCKED.
fn blocked_answer() -> Judgement {
    Judgement {
        state: SemanticState::Blocked,
        confidence: confidence(0.82),
        probabilities: Some([0.0, 0.18, 0.82, 0.0, 0.0]),
        needs_human: Some(0.96),
        stuck: Some(0.22),
    }
}

#[test]
fn rules_decide_settled_facts_without_a_model_call() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    append(
        &fixture.store,
        Source::Claude,
        EventKind::ToolStarted {
            tool_name: "Bash".to_string(),
            tool_use_id: "t1".to_string(),
        },
    );

    assert_eq!(fixture.observer.tick().expect("tick"), 1);
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Working);
    assert_eq!(last.judge, "rules");
    assert_eq!(last.ambiguity, None);
    assert!(!last.attention);
    assert_eq!(judge.calls(), 0, "a settled fact needs no model");
}

#[test]
fn a_stalled_end_asks_for_attention_until_the_execution_ends() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    append(
        &fixture.store,
        Source::Bridge,
        EventKind::ExecutionEndStalled {
            cause: EndCause::Exited,
            exit_code: Some(0),
            failures: 20,
            error: "commit unavailable: No space left on device".to_string(),
        },
    );
    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Blocked);
    assert_eq!(last.judge, "rules");
    assert!(last.attention);
    assert_eq!(last.attention_reason.as_deref(), Some("end_stalled"));
    assert_eq!(judge.calls(), 0, "a fact, not a question for the model");

    append(
        &fixture.store,
        Source::Bridge,
        EventKind::ExecutionEnded {
            cause: EndCause::Exited,
            exit_code: Some(0),
        },
    );
    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_ne!(last.attention_reason.as_deref(), Some("end_stalled"));
}

#[test]
fn an_idle_prompt_goes_to_the_model_with_screen_and_task() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    turn_stopped(&fixture.store);

    assert_eq!(fixture.observer.tick().expect("tick"), 1);
    assert_eq!(judge.calls(), 1);
    let seen = judge.seen.lock().expect("lock").clone().expect("input");
    assert_eq!(seen.ambiguity, Some(Ambiguity::TurnStoppedIdle));
    assert!(
        seen.screen_tail
            .as_deref()
            .is_some_and(|tail| tail.contains("Please confirm"))
    );
    assert_eq!(seen.task.as_deref(), Some("Speed up ingest"));

    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Blocked);
    assert_eq!(last.judge, "jev");
    assert_eq!(last.rules_state, Some(SemanticState::Waiting));
    assert_eq!(last.needs_human, Some(0.96));
    assert!(last.attention, "a blocked agent asks for attention");
    assert!(last.evidence.is_some(), "the evidence is kept for audit");
}

#[test]
fn the_model_cannot_contradict_the_facts() {
    // WORKING is impossible at an idle prompt with no open tool.
    let judge = Scripted::new(Ok(Judgement {
        state: SemanticState::Working,
        confidence: confidence(0.7),
        probabilities: Some([0.7, 0.1, 0.2, 0.0, 0.0]),
        needs_human: Some(0.1),
        stuck: Some(0.0),
    }));
    let mut fixture = fixture(judge);
    started(&fixture.store);
    turn_stopped(&fixture.store);

    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Blocked, "best allowed state");
    assert!(
        (last.confidence.value() - 0.2 / 0.3).abs() < 1e-9,
        "renormalized"
    );
    assert_eq!(
        last.probabilities.and_then(|p| p.get("WORKING").copied()),
        Some(0.7),
        "the raw answer stays visible"
    );
}

#[test]
fn the_same_evidence_is_not_asked_twice() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    turn_stopped(&fixture.store);

    fixture.observer.tick().expect("tick");
    fixture.clock.advance(Duration::from_secs(31));
    fixture.observer.tick().expect("recheck");
    assert_eq!(judge.calls(), 1, "nothing new to show the model");
    assert_eq!(observations(&fixture.store).len(), 1);
}

#[test]
fn without_the_model_the_rules_guess_stands_and_asks_for_attention() {
    let judge = Scripted::new(Err("connection refused".to_string()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    turn_stopped(&fixture.store);

    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Waiting, "the rules' guess");
    assert_eq!(last.reason, Some(WaitReason::JudgeUnavailable));
    assert!(last.attention);

    fixture.observer.tick().expect("too soon");
    assert_eq!(judge.calls(), 1, "an unavailable judge is not hammered");
    fixture.clock.advance(Duration::from_secs(31));
    fixture.observer.tick().expect("retry");
    assert_eq!(judge.calls(), 2, "an unavailable judge is asked again");
    fixture.clock.advance(Duration::from_secs(31));
    fixture.observer.tick().expect("backed off");
    assert_eq!(judge.calls(), 2, "the pause doubles");
    fixture.clock.advance(Duration::from_secs(31));
    fixture.observer.tick().expect("retry");
    assert_eq!(judge.calls(), 3);
}

#[test]
fn a_restarted_observer_does_not_ask_about_judged_evidence() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    turn_stopped(&fixture.store);
    fixture.observer.tick().expect("tick");
    assert_eq!(judge.calls(), 1);
    let first = observations(&fixture.store).pop().expect("an observation");
    assert!(
        first.fingerprint.is_some(),
        "the answer keeps its fingerprint"
    );

    // A one-shot `spawn tick` builds a new Observer every run.
    let log: Arc<dyn EventLog> = fixture.store.clone();
    let mut again = Observer::new(log)
        .with_judge(judge.clone())
        .with_clock(fixture.clock.clone())
        .with_context(Arc::new(Context));
    fixture.clock.advance(Duration::from_secs(31));
    again.tick().expect("tick");
    assert_eq!(judge.calls(), 1, "same evidence, no second call");
}

#[test]
fn every_model_answer_is_recorded() {
    // Same state, new evidence: the answer is still written, so the log holds
    // everything the model saw and said.
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    turn_stopped(&fixture.store);
    fixture.observer.tick().expect("tick");
    turn_stopped(&fixture.store);
    fixture.observer.tick().expect("tick");
    assert_eq!(judge.calls(), 2);
    assert_eq!(observations(&fixture.store).len(), 2);
}

#[test]
fn an_observation_from_anyone_but_the_observer_is_ignored() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    turn_stopped(&fixture.store);
    let mut forged = Observation::basic(
        SemanticState::Blocked,
        None,
        spawn_core::event::SessionSeq(2),
        confidence(1.0),
        "jev",
    );
    forged.fingerprint = Some("0000000000000000".to_string());
    append(
        &fixture.store,
        Source::Claude,
        EventKind::ObservationRecorded {
            observation: forged,
        },
    );
    fixture.observer.tick().expect("tick");
    assert_eq!(
        judge.calls(),
        1,
        "the forged row neither judged nor deduped"
    );
}

#[test]
fn a_stale_busy_screen_does_not_prove_work() {
    let judge = Scripted::new(Ok(Judgement {
        state: SemanticState::Blocked,
        confidence: confidence(0.7),
        probabilities: Some([0.2, 0.1, 0.7, 0.0, 0.0]),
        needs_human: Some(0.3),
        stuck: Some(0.8),
    }));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    append(
        &fixture.store,
        Source::Bridge,
        EventKind::ScreenObserved {
            state: ScreenState::Busy,
            evidence: "esc to interrupt".to_string(),
        },
    );
    fixture.observer.tick().expect("tick");
    assert_eq!(judge.calls(), 0, "a fresh busy screen is work");

    fixture.clock.advance(Duration::from_secs(150));
    fixture.observer.tick().expect("recheck");
    assert_eq!(judge.calls(), 1, "an old busy screen is not");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert!(last.attention);
}

#[test]
fn a_tool_open_for_long_is_ambiguous() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    append(
        &fixture.store,
        Source::Claude,
        EventKind::ToolStarted {
            tool_name: "Bash".to_string(),
            tool_use_id: "t1".to_string(),
        },
    );
    fixture.clock.advance(Duration::from_mins(5));
    fixture.observer.tick().expect("tick");
    assert_eq!(judge.calls(), 0, "a five-minute build is work");
    fixture.clock.advance(Duration::from_mins(6));
    fixture.observer.tick().expect("tick");
    assert_eq!(judge.calls(), 1, "an eleven-minute silent tool may hang");
}

#[test]
fn a_hung_tool_under_a_busy_spinner_still_asks_for_attention() {
    let judge = Scripted::new(Err("no judge today".to_string()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    append(
        &fixture.store,
        Source::Claude,
        EventKind::ToolStarted {
            tool_name: "Bash".to_string(),
            tool_use_id: "t1".to_string(),
        },
    );
    // The pane keeps showing the spinner, so the Bridge records the busy
    // screen again every 30 s; the transcript says nothing more.
    let busy = |store: &MemoryStore| {
        append(
            store,
            Source::Bridge,
            EventKind::ScreenObserved {
                state: ScreenState::Busy,
                evidence: "esc to interrupt".to_string(),
            },
        );
    };
    for _ in 0..19 {
        busy(&fixture.store);
        fixture.observer.tick().expect("tick");
        fixture.clock.advance(Duration::from_secs(30));
    }
    assert_eq!(judge.calls(), 0, "nine and a half minutes: a long build");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Working);
    assert!(!last.attention);

    for _ in 0..3 {
        busy(&fixture.store);
        fixture.observer.tick().expect("tick");
        fixture.clock.advance(Duration::from_secs(30));
    }
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(
        last.ambiguity,
        Some(Ambiguity::QuietLive),
        "eleven minutes of one tool and no progress may hang, busy screen or not"
    );
    assert!(last.attention, "{last:?}");
    assert!(judge.calls() >= 1, "the case went to the judge");

    // The tool finishes: progress again, and work again.
    append(
        &fixture.store,
        Source::Claude,
        EventKind::ToolCompleted {
            tool_use_id: "t1".to_string(),
            duration_ms: None,
        },
    );
    busy(&fixture.store);
    fixture.clock.advance(Duration::from_secs(1));
    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Working);
    assert_eq!(last.ambiguity, None);
}

#[test]
fn a_failing_exit_asks_for_attention_whatever_the_model_says() {
    let judge = Scripted::new(Ok(Judgement {
        state: SemanticState::Completed,
        confidence: confidence(0.95),
        probabilities: Some([0.0, 0.0, 0.05, 0.95, 0.0]),
        needs_human: Some(0.05),
        stuck: Some(0.05),
    }));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    append(
        &fixture.store,
        Source::Bridge,
        EventKind::ExecutionEnded {
            cause: EndCause::Exited,
            exit_code: Some(1),
        },
    );
    fixture.observer.tick().expect("tick");
    let seen = judge.seen.lock().expect("lock").clone().expect("input");
    assert_eq!(seen.exit_code, Some(1), "the model sees the status");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.rules_state, Some(SemanticState::Blocked));
    assert!(last.attention);
}

#[test]
fn a_model_that_disagrees_with_the_facts_asks_for_attention() {
    // 0.8 on WORKING, which an idle prompt rules out.
    let judge = Scripted::new(Ok(Judgement {
        state: SemanticState::Working,
        confidence: confidence(0.8),
        probabilities: Some([0.8, 0.15, 0.0, 0.05, 0.0]),
        needs_human: Some(0.1),
        stuck: Some(0.1),
    }));
    let mut fixture = fixture(judge);
    started(&fixture.store);
    turn_stopped(&fixture.store);
    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Waiting);
    assert!(last.attention, "most of the mass was on a ruled-out state");
}

#[test]
fn a_silent_live_session_becomes_ambiguous() {
    let judge = Scripted::new(Ok(Judgement {
        state: SemanticState::Working,
        confidence: confidence(0.9),
        probabilities: Some([0.9, 0.05, 0.05, 0.0, 0.0]),
        needs_human: Some(0.02),
        stuck: Some(0.1),
    }));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);

    fixture.observer.tick().expect("tick");
    assert_eq!(judge.calls(), 0, "a fresh turn is working by rule");

    fixture.clock.advance(Duration::from_secs(150));
    fixture.observer.tick().expect("recheck");
    assert_eq!(judge.calls(), 1);
    let seen = judge.seen.lock().expect("lock").clone().expect("input");
    assert_eq!(seen.ambiguity, Some(Ambiguity::QuietLive));
}

#[test]
fn an_idle_agent_split_between_harmless_states_pages_no_one() {
    // Live answer from jev-1.14.0 for an agent that finished and sits idle.
    let judge = Scripted::new(Ok(Judgement {
        state: SemanticState::Waiting,
        confidence: confidence(0.56),
        probabilities: Some([0.01, 0.56, 0.01, 0.40, 0.02]),
        needs_human: Some(0.2),
        stuck: Some(0.13),
    }));
    let mut fixture = fixture(judge);
    started(&fixture.store);
    turn_stopped(&fixture.store);

    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Waiting);
    assert!(!last.attention, "WAITING or COMPLETED, nobody is needed");
}

#[test]
fn a_permission_screen_dies_with_its_execution() {
    // Review finding: a permission screen recorded before a stop kept the
    // session BLOCKED by rule after the pane was gone.
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge);
    started(&fixture.store);
    append(
        &fixture.store,
        Source::Bridge,
        EventKind::ScreenObserved {
            state: ScreenState::Permission,
            evidence: "Do you want to proceed?".to_string(),
        },
    );
    append(
        &fixture.store,
        Source::Bridge,
        EventKind::ExecutionEnded {
            cause: EndCause::Stopped,
            exit_code: None,
        },
    );

    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Completed);
    assert_eq!(last.judge, "rules");
    assert!(!last.attention);
}

#[test]
fn a_tie_goes_to_the_rules_guess() {
    let tie = Judgement {
        state: SemanticState::Blocked,
        confidence: confidence(0.5),
        probabilities: Some([0.0, 0.0, 0.5, 0.5, 0.0]),
        needs_human: None,
        stuck: None,
    };
    let (state, _) = narrow(Ambiguity::ExitedOnItsOwn, SemanticState::Completed, &tie);
    assert_eq!(state, SemanticState::Completed);
}

#[test]
fn a_restarted_observer_corrects_a_stale_row() {
    // A row an older build wrote: WAITING for a session the Bridge stopped.
    let judge = Scripted::new(Ok(blocked_answer()));
    let fixture = fixture(judge.clone());
    started(&fixture.store);
    append(
        &fixture.store,
        Source::Bridge,
        EventKind::ExecutionEnded {
            cause: EndCause::Stopped,
            exit_code: None,
        },
    );
    append(
        &fixture.store,
        Source::Observer,
        EventKind::ObservationRecorded {
            observation: Observation::basic(
                SemanticState::Waiting,
                None,
                spawn_core::event::SessionSeq(2),
                confidence(0.6),
                "jev",
            ),
        },
    );
    let log: Arc<dyn EventLog> = fixture.store.clone();
    let mut fresh = Observer::new(log)
        .with_judge(judge.clone())
        .with_clock(fixture.clock.clone());
    assert_eq!(fresh.tick().expect("tick"), 1);
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Completed);
    assert_eq!(last.judge, "rules");
    assert_eq!(judge.calls(), 0);

    // And a second restart writes nothing.
    let log: Arc<dyn EventLog> = fixture.store.clone();
    let mut again = Observer::new(log)
        .with_judge(judge)
        .with_clock(fixture.clock.clone());
    assert_eq!(again.tick().expect("tick"), 0);
}

/// The supervisor's context: never ends, and its screen asks a person.
struct Supervisor;

impl ObservationContext for Supervisor {
    fn screen_tail(&self, _session: &SessionId) -> Option<String> {
        Some(
            "you: deploy it\nclaude: The build is green.\nclaude: HUMAN: which region?\n$ "
                .to_string(),
        )
    }

    fn task(&self, _session: &SessionId) -> Option<String> {
        None
    }

    fn reply(&self, _workspace: &WorkspaceId, _transcript: &str) -> Option<String> {
        Some("The build is green.\nHUMAN: which region?".to_string())
    }

    fn perpetual(&self, _session: &SessionId) -> bool {
        true
    }
}

/// A session whose screen shows a tool's output with a `HUMAN:` line, and
/// whose model wrote no question.
struct ToolOutput;

impl ObservationContext for ToolOutput {
    fn screen_tail(&self, _session: &SessionId) -> Option<String> {
        Some(
            "you: show the notes\ntool: Bash (cat NOTES.md)\nHUMAN: ask about the region\n\
             claude: Those are the notes.\n$ "
                .to_string(),
        )
    }

    fn task(&self, _session: &SessionId) -> Option<String> {
        None
    }

    fn reply(&self, _workspace: &WorkspaceId, _transcript: &str) -> Option<String> {
        Some("Those are the notes.".to_string())
    }
}

/// The question for a person is the model's own: a `HUMAN:` line in a
/// tool's output on the screen (audit, the `HUMAN:` sibling of H2) is not.
#[test]
fn a_human_line_in_a_tools_output_asks_nobody() {
    let judge = Scripted::new(Ok(Judgement {
        state: SemanticState::Waiting,
        confidence: confidence(0.9),
        probabilities: None,
        needs_human: Some(0.05),
        stuck: Some(0.05),
    }));
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock.clone()));
    let log: Arc<dyn EventLog> = store.clone();
    let mut observer = Observer::new(log)
        .with_judge(judge)
        .with_clock(clock)
        .with_context(Arc::new(ToolOutput));
    started(&store);
    native(&store);
    turn_stopped(&store);
    observer.tick().expect("tick");
    let last = observations(&store).pop().expect("an observation");
    assert_eq!(last.human_question, None);
    assert_ne!(last.attention_reason.as_deref(), Some("human_question"));
}

#[test]
fn a_perpetual_session_is_never_completed_and_its_question_pages() {
    // Live: Jev put the idle supervisor at COMPLETED 0.71.
    let judge = Scripted::new(Ok(Judgement {
        state: SemanticState::Completed,
        confidence: confidence(0.71),
        probabilities: Some([0.0, 0.27, 0.02, 0.71, 0.0]),
        needs_human: Some(0.2),
        stuck: Some(0.05),
    }));
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock.clone()));
    let log: Arc<dyn EventLog> = store.clone();
    let mut observer = Observer::new(log)
        .with_judge(judge)
        .with_clock(clock)
        .with_context(Arc::new(Supervisor));
    started(&store);
    native(&store);
    turn_stopped(&store);
    observer.tick().expect("tick");
    let last = observations(&store).pop().expect("an observation");
    assert_eq!(
        last.state,
        SemanticState::Waiting,
        "COMPLETED folds into WAITING"
    );
    assert!(last.confidence.value() > 0.95, "0.98 of the allowed mass");
    assert_eq!(
        last.probabilities.and_then(|p| p.get("COMPLETED").copied()),
        Some(0.71),
        "the raw answer is kept"
    );
    assert_eq!(last.human_question.as_deref(), Some("which region?"));
    assert!(last.attention);
    assert_eq!(last.attention_reason.as_deref(), Some("human_question"));
}

#[test]
fn attention_says_why() {
    let judge = Scripted::new(Err("connection refused".to_string()));
    let mut fixture = fixture(judge);
    started(&fixture.store);
    turn_stopped(&fixture.store);
    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.attention_reason.as_deref(), Some("judge_unavailable"));

    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = self::fixture(judge);
    started(&fixture.store);
    turn_stopped(&fixture.store);
    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.attention_reason.as_deref(), Some("blocked"));
}

#[test]
fn input_typed_into_a_ready_pane_is_work_not_an_idle_prompt() {
    // Live: the ready screen, then the typed prompt's command.finished, with
    // the engine's message.received hook still in flight. Jev was asked about
    // an idle prompt that had just received its input.
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    ready(&fixture.store);
    typed(&fixture.store, 1);
    finished(&fixture.store, 1);
    fixture.observer.tick().expect("tick");
    assert_eq!(judge.calls(), 0);
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Working);
    assert_eq!(last.judge, "rules");
}

fn ready(store: &MemoryStore) {
    append(
        store,
        Source::Bridge,
        EventKind::ScreenObserved {
            state: ScreenState::Ready,
            evidence: ">".to_string(),
        },
    );
}

fn typed(store: &MemoryStore, command: u64) {
    append(
        store,
        Source::Bridge,
        EventKind::CommandStepped {
            command_id: CommandId(command),
            step: spawn_core::event::CommandStep::Typed,
            workspace: None,
        },
    );
}

fn finished(store: &MemoryStore, command: u64) {
    append(
        store,
        Source::Gateway,
        EventKind::CommandFinished {
            command_id: CommandId(command),
            succeeded: true,
            error: None,
        },
    );
}

/// The input was typed and its turn ended before the command's finish was
/// recorded (the first `finish` failed; the retried command succeeds at
/// once, its text already typed). The late `command.finished` comes after
/// the turn's end and a ready screen: it starts no turn, and the idle
/// worker is still seen idle (causality audit #12).
#[test]
fn a_late_finish_of_an_input_typed_before_the_turn_ended_starts_no_turn() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    ready(&fixture.store);
    typed(&fixture.store, 1);
    turn_stopped(&fixture.store);
    ready(&fixture.store);
    finished(&fixture.store, 1);
    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_ne!(
        last.state,
        SemanticState::Working,
        "an idle worker read as working is never served: {last:?}"
    );
}

#[test]
fn the_control_stream_is_not_an_agent_to_observe() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    fixture
        .store
        .append(NewEvent {
            agent_id: AgentId::new("agent_control").expect("valid agent"),
            session_id: SessionId::new("session_control").expect("valid session"),
            execution_id: None,
            source: Source::Control,
            source_event_id: None,
            kind: EventKind::ControlRecorded {
                record: serde_json::json!({"type": "mission.added", "id": 1, "text": "x"}),
            },
        })
        .expect("append succeeds");
    fixture.clock.advance(Duration::from_mins(10));

    assert_eq!(fixture.observer.tick().expect("tick"), 0);
    assert!(observations(&fixture.store).is_empty());
    assert_eq!(judge.calls(), 0);
}

#[test]
fn a_late_end_of_an_older_execution_does_not_end_the_current_one() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge);
    started(&fixture.store);
    let in_execution = |command: u64, source: Source, kind: EventKind| {
        fixture
            .store
            .append(NewEvent {
                agent_id: agent(),
                session_id: session(),
                execution_id: Some(ExecutionId::for_command(CommandId(command))),
                source,
                source_event_id: None,
                kind,
            })
            .expect("append succeeds");
    };
    // Execution 1 is ended as lost; its runner's exit report reaches the
    // log only after execution 2 started.
    in_execution(
        1,
        Source::Bridge,
        EventKind::ExecutionEnded {
            cause: EndCause::Lost,
            exit_code: None,
        },
    );
    in_execution(
        2,
        Source::Bridge,
        EventKind::ExecutionStarted {
            engine: ENGINE.to_string(),
            workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
            command_id: CommandId(2),
            fork: None,
            model: ModelProfile::Default,
            transcript_from: None,
        },
    );
    in_execution(
        1,
        Source::Claude,
        EventKind::ExecutionEnded {
            cause: EndCause::Exited,
            exit_code: Some(0),
        },
    );
    fixture.observer.tick().expect("tick");
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_ne!(
        last.state,
        SemanticState::Completed,
        "execution 2 is still running"
    );
}

// --- Coverage: the latest judgement covers the latest fact ---------------------

/// An Observer that judges by the rules alone, on the fixture's log and clock.
fn rules_only(fixture: &Fixture) -> Observer {
    let log: Arc<dyn EventLog> = fixture.store.clone();
    Observer::new(log).with_clock(fixture.clock.clone())
}

/// The session's latest fact: its latest record that is not the Observer's.
fn latest_fact(store: &MemoryStore) -> u64 {
    store
        .read_global(GlobalPosition(0), 500)
        .expect("read succeeds")
        .into_iter()
        .filter(|record| record.event.source != Source::Observer)
        .map(|record| record.session_seq.0)
        .max()
        .unwrap_or(0)
}

fn tool_started(store: &MemoryStore, id: &str) {
    append(
        store,
        Source::Claude,
        EventKind::ToolStarted {
            tool_name: "Bash".to_string(),
            tool_use_id: id.to_string(),
        },
    );
}

fn tool_completed(store: &MemoryStore, id: &str) {
    append(
        store,
        Source::Claude,
        EventKind::ToolCompleted {
            tool_use_id: id.to_string(),
            duration_ms: None,
        },
    );
}

fn busy(store: &MemoryStore) {
    append(
        store,
        Source::Bridge,
        EventKind::ScreenObserved {
            state: ScreenState::Busy,
            evidence: "✽ Working…".to_string(),
        },
    );
}

#[test]
fn an_unchanged_verdict_is_written_again_once_the_facts_settle() {
    let fixture = fixture(Scripted::new(Ok(blocked_answer())));
    let mut observer = rules_only(&fixture);
    started(&fixture.store);
    tool_started(&fixture.store, "t1");
    assert_eq!(observer.tick().expect("tick"), 1);

    // The screen confirms the work: the verdict stays WORKING.
    busy(&fixture.store);
    let reading = latest_fact(&fixture.store);
    assert_eq!(observer.tick().expect("tick"), 0, "not settled yet");
    fixture.clock.advance(Duration::from_secs(1));
    assert_eq!(observer.tick().expect("tick"), 0, "still not settled");
    fixture.clock.advance(Duration::from_secs(2));
    assert_eq!(observer.tick().expect("settled"), 1);
    let all = observations(&fixture.store);
    assert_eq!(all.len(), 2);
    let last = all.last().expect("an observation");
    assert_eq!(last.state, SemanticState::Working);
    assert_eq!(last.based_on_seq.0, reading, "covers the screen reading");
    assert_eq!(last.judge, "rules");
}

#[test]
fn a_session_without_new_facts_gets_no_new_observation() {
    let fixture = fixture(Scripted::new(Ok(blocked_answer())));
    let mut observer = rules_only(&fixture);
    started(&fixture.store);
    tool_started(&fixture.store, "t1");
    busy(&fixture.store);
    observer.tick().expect("tick");
    fixture.clock.advance(Duration::from_secs(3));
    observer.tick().expect("settled");
    let before = observations(&fixture.store).len();
    for _ in 0..5 {
        fixture.clock.advance(Duration::from_secs(31));
        assert_eq!(observer.tick().expect("recheck"), 0);
    }
    assert_eq!(observations(&fixture.store).len(), before);
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.based_on_seq.0, latest_fact(&fixture.store));
}

#[test]
fn a_burst_of_facts_with_an_unchanged_verdict_is_covered_once() {
    let fixture = fixture(Scripted::new(Ok(blocked_answer())));
    let mut observer = rules_only(&fixture);
    started(&fixture.store);
    tool_started(&fixture.store, "t0");
    assert_eq!(observer.tick().expect("tick"), 1);
    let mut written = 0;
    for n in 1..=20 {
        // A tool stays open across the burst: the verdict stays WORKING.
        tool_started(&fixture.store, &format!("t{n}"));
        fixture.clock.advance(Duration::from_millis(200));
        written += observer.tick().expect("tick");
        tool_completed(&fixture.store, &format!("t{n}"));
        fixture.clock.advance(Duration::from_millis(200));
        written += observer.tick().expect("tick");
    }
    assert_eq!(written, 0, "no observation per fact while the facts come");
    fixture.clock.advance(Duration::from_secs(3));
    assert_eq!(observer.tick().expect("settled"), 1, "one once it settles");
    fixture.clock.advance(Duration::from_secs(31));
    assert_eq!(observer.tick().expect("recheck"), 0);
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Working);
    assert_eq!(last.based_on_seq.0, latest_fact(&fixture.store));
}

#[test]
fn a_model_answer_covers_newer_facts_with_the_same_evidence_without_a_call() {
    let judge = Scripted::new(Ok(blocked_answer()));
    let mut fixture = fixture(judge.clone());
    started(&fixture.store);
    turn_stopped(&fixture.store);
    ready(&fixture.store);
    fixture.observer.tick().expect("tick");
    assert_eq!(judge.calls(), 1);
    let answer = observations(&fixture.store).pop().expect("an answer");

    // A second, equal reading of the prompt: nothing new to show the model.
    ready(&fixture.store);
    let reading = latest_fact(&fixture.store);
    assert_eq!(fixture.observer.tick().expect("tick"), 0, "not settled yet");
    fixture.clock.advance(Duration::from_secs(3));
    assert_eq!(fixture.observer.tick().expect("settled"), 1);
    assert_eq!(judge.calls(), 1, "the model is not asked again");
    let covering = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(covering.based_on_seq.0, reading);
    assert_eq!(covering.state, answer.state);
    assert_eq!(covering.judge, answer.judge);
    assert_eq!(covering.ambiguity, answer.ambiguity);
    assert_eq!(covering.attention, answer.attention);
    assert_eq!(covering.fingerprint, answer.fingerprint);
    assert_eq!(covering.evidence, answer.evidence);
    assert_eq!(covering.latency_ms, None, "no model call was made for it");

    fixture.clock.advance(Duration::from_secs(31));
    assert_eq!(fixture.observer.tick().expect("recheck"), 0);
    assert_eq!(judge.calls(), 1);
}

#[test]
fn a_restarted_observer_covers_a_lagging_session_once() {
    let fixture = fixture(Scripted::new(Ok(blocked_answer())));
    let mut first = rules_only(&fixture);
    started(&fixture.store);
    tool_started(&fixture.store, "t1");
    assert_eq!(first.tick().expect("tick"), 1);
    // The Observer stops; a fact that changes nothing lands meanwhile.
    drop(first);
    busy(&fixture.store);
    let reading = latest_fact(&fixture.store);
    fixture.clock.advance(Duration::from_secs(3));

    let mut restarted = rules_only(&fixture);
    assert_eq!(restarted.tick().expect("tick"), 1);
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.based_on_seq.0, reading);
    assert_eq!(last.state, SemanticState::Working);
    fixture.clock.advance(Duration::from_secs(31));
    assert_eq!(restarted.tick().expect("recheck"), 0);

    let mut again = rules_only(&fixture);
    assert_eq!(again.tick().expect("tick"), 0, "covered: nothing to write");
}

/// An input that waits for a busy session's turn records `waiting`. The
/// Observer takes it as it takes any command step: it changes no verdict,
/// and the judgement written once the facts settle covers it, so the
/// verdict never looks stale behind it.
#[test]
fn a_waiting_input_is_covered_like_any_command_step_and_changes_no_verdict() {
    let fixture = fixture(Scripted::new(Ok(blocked_answer())));
    let mut observer = rules_only(&fixture);
    started(&fixture.store);
    tool_started(&fixture.store, "t1");
    busy(&fixture.store);
    assert_eq!(observer.tick().expect("tick"), 1);
    fixture.clock.advance(Duration::from_secs(3));
    observer.tick().expect("settled");

    append(
        &fixture.store,
        Source::Bridge,
        EventKind::CommandStepped {
            command_id: CommandId(2),
            step: spawn_core::event::CommandStep::Waiting,
            workspace: None,
        },
    );
    let waiting = latest_fact(&fixture.store);
    fixture.clock.advance(Duration::from_secs(3));
    assert_eq!(observer.tick().expect("settled"), 1);
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Working);
    assert_eq!(last.based_on_seq.0, waiting, "covers the waiting step");
    fixture.clock.advance(Duration::from_secs(31));
    assert_eq!(observer.tick().expect("recheck"), 0);
}

//! The Observer's two tiers: rules decide what the facts settle, the model
//! judge answers only ambiguous cases, and its answer never contradicts a fact.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spawn_core::event::EndCause;
use spawn_core::event::{Engine, EventKind, GlobalPosition, NewEvent, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, TerminalName, WorkspaceId};
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
    let observer = Observer::new(log, judge)
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
            engine: Engine::Claude,
            terminal: TerminalName::new("spawn-exec_1").expect("valid"),
            workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
            command_id: CommandId(1),
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
    let mut again = Observer::new(log, judge.clone())
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
    let mut fresh = Observer::new(log, judge.clone()).with_clock(fixture.clock.clone());
    assert_eq!(fresh.tick().expect("tick"), 1);
    let last = observations(&fixture.store).pop().expect("an observation");
    assert_eq!(last.state, SemanticState::Completed);
    assert_eq!(last.judge, "rules");
    assert_eq!(judge.calls(), 0);

    // And a second restart writes nothing.
    let log: Arc<dyn EventLog> = fixture.store.clone();
    let mut again = Observer::new(log, judge).with_clock(fixture.clock.clone());
    assert_eq!(again.tick().expect("tick"), 0);
}

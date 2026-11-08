//! A worker waiting on its own background job is working, not idle
//! (docs/fleet-architecture.md §2, "Waiting on its own job"): the Bridge
//! records the jobs an execution starts and ends from its transcript, and
//! the rules settle a stopped turn with one still open as WORKING, with no
//! model call. A turn that stops with none open is judged as before.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{
    ENGINE, EventKind, GlobalPosition, JobEnd, JobKind, ModelProfile, NewEvent, Source,
};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState, WaitReason};
use spawn_core::observe::{JudgeError, JudgeInput, Judgement, Observer, StateJudge};
use spawn_core::port::{CommandQueue, EventLog};
use spawn_core::time::{ManualClock, Timestamp};

const READY: &str = "❯ \n  model line";

/// A model judge that counts its calls and keeps what it saw.
struct Counting {
    calls: AtomicUsize,
    seen: Mutex<Option<JudgeInput>>,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(None),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl StateJudge for Counting {
    fn name(&self) -> &'static str {
        "jev"
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.seen.lock().expect("lock") = Some(input.clone());
        Ok(Judgement {
            state: SemanticState::Waiting,
            confidence: Confidence::new(0.9).expect("valid"),
            probabilities: Some([0.0, 0.9, 0.05, 0.05, 0.0]),
            needs_human: Some(0.05),
            stuck: Some(0.05),
        })
    }
}

fn agent() -> AgentId {
    AgentId::new("agent_worker_jobs").expect("valid agent")
}

fn session() -> SessionId {
    SessionId::new("session_worker_jobs").expect("valid session")
}

struct Fixture {
    clock: Arc<ManualClock>,
    store: Arc<MemoryStore>,
    judge: Arc<Counting>,
    observer: Observer,
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock.clone()));
    let judge = Counting::new();
    let log: Arc<dyn EventLog> = store.clone();
    let observer = Observer::new(log)
        .with_judge(judge.clone())
        .with_clock(clock.clone());
    Fixture {
        clock,
        store,
        judge,
        observer,
    }
}

fn observations(store: &MemoryStore) -> Vec<Observation> {
    store
        .read_global(GlobalPosition(0), 1000)
        .expect("read")
        .into_iter()
        .filter_map(|record| match record.event.kind {
            EventKind::ObservationRecorded { observation } => Some(observation),
            _ => None,
        })
        .collect()
}

fn latest(store: &MemoryStore) -> Observation {
    observations(store).pop().expect("an observation")
}

/// The judgement of the facts so far, once they settled.
fn judge(fixture: &mut Fixture) -> Observation {
    fixture.observer.tick().expect("tick");
    fixture.clock.advance(Duration::from_secs(3));
    fixture.observer.tick().expect("settled");
    latest(&fixture.store)
}

fn append(store: &MemoryStore, execution: u64, key: Option<&str>, kind: EventKind) {
    store
        .append(NewEvent {
            agent_id: agent(),
            session_id: session(),
            execution_id: Some(ExecutionId::for_command(CommandId(execution))),
            source: if matches!(
                kind,
                EventKind::ExecutionStarted { .. } | EventKind::ExecutionEnded { .. }
            ) {
                Source::Bridge
            } else {
                Source::Claude
            },
            source_event_id: key.map(str::to_string),
            kind,
        })
        .expect("append");
}

fn execution_started(store: &MemoryStore, execution: u64) {
    append(
        store,
        execution,
        None,
        EventKind::ExecutionStarted {
            engine: ENGINE.to_string(),
            workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
            command_id: CommandId(execution),
            fork: None,
            model: ModelProfile::Default,
            transcript_from: None,
        },
    );
}

fn turn_stopped(store: &MemoryStore, execution: u64) {
    append(
        store,
        execution,
        None,
        EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
    );
}

fn job(id: &str, kind: JobKind) -> EventKind {
    EventKind::JobStarted {
        job_id: id.to_string(),
        kind,
    }
}

fn job_ended(id: &str, how: JobEnd) -> EventKind {
    EventKind::JobEnded {
        job_id: id.to_string(),
        how,
    }
}

fn waits_on_its_job(observation: &Observation) {
    assert_eq!(observation.state, SemanticState::Working, "{observation:?}");
    assert_eq!(
        observation.reason,
        Some(WaitReason::WaitingForBackgroundJob)
    );
    assert_eq!(observation.judge, "rules");
    assert_eq!(observation.ambiguity, None, "the rules decided");
    assert!(!observation.attention);
    assert!(observation.waits_on_own_job());
}

#[test]
fn a_stopped_turn_with_a_job_open_is_working_on_it_without_a_model_call() {
    let mut fixture = fixture();
    execution_started(&fixture.store, 1);
    append(&fixture.store, 1, None, job("b8w03og7k", JobKind::Bash));
    turn_stopped(&fixture.store, 1);
    waits_on_its_job(&judge(&mut fixture));
    assert_eq!(fixture.judge.calls(), 0, "no Jev call");

    // Hours later, still the rules' (the bound is the control layer's).
    for _ in 0..20 {
        fixture.clock.advance(Duration::from_mins(10));
        fixture.observer.tick().expect("recheck");
    }
    waits_on_its_job(&latest(&fixture.store));
    assert_eq!(fixture.judge.calls(), 0);

    // It ends: the next stopped turn is judged as before, by Jev.
    append(
        &fixture.store,
        1,
        None,
        job_ended("b8w03og7k", JobEnd::Completed),
    );
    append(&fixture.store, 1, None, EventKind::MessageReceived);
    turn_stopped(&fixture.store, 1);
    let after = judge(&mut fixture);
    assert_eq!(after.ambiguity, Some(Ambiguity::TurnStoppedIdle));
    assert_eq!(after.judge, "jev");
    assert_eq!(fixture.judge.calls(), 1);
}

#[test]
fn a_monitor_whose_events_start_short_turns_keeps_the_worker_working() {
    let mut fixture = fixture();
    execution_started(&fixture.store, 1);
    append(&fixture.store, 1, None, job("bv41gl0hm", JobKind::Monitor));
    append(&fixture.store, 1, None, job("b6yqlx30i", JobKind::Bash));
    turn_stopped(&fixture.store, 1);
    waits_on_its_job(&judge(&mut fixture));
    // Each event wakes the agent for a short turn; the jobs stay open.
    for _ in 0..3 {
        append(&fixture.store, 1, None, EventKind::MessageReceived);
        turn_stopped(&fixture.store, 1);
        waits_on_its_job(&judge(&mut fixture));
    }
    // The bash ends, the Monitor expires: no job is open any more.
    append(
        &fixture.store,
        1,
        None,
        job_ended("b6yqlx30i", JobEnd::Completed),
    );
    append(&fixture.store, 1, None, EventKind::MessageReceived);
    turn_stopped(&fixture.store, 1);
    waits_on_its_job(&judge(&mut fixture));
    append(
        &fixture.store,
        1,
        None,
        job_ended("bv41gl0hm", JobEnd::Expired),
    );
    append(&fixture.store, 1, None, EventKind::MessageReceived);
    turn_stopped(&fixture.store, 1);
    assert_eq!(
        judge(&mut fixture).ambiguity,
        Some(Ambiguity::TurnStoppedIdle)
    );
    assert_eq!(fixture.judge.calls(), 1, "only once no job was open");
}

#[test]
fn a_job_is_open_only_within_its_execution() {
    let mut fixture = fixture();
    execution_started(&fixture.store, 1);
    append(&fixture.store, 1, None, job("b8w03og7k", JobKind::Bash));
    turn_stopped(&fixture.store, 1);
    waits_on_its_job(&judge(&mut fixture));
    // The engine was lost and resumed: the earlier process's jobs died
    // with it, whether or not the transcript ever says so.
    append(
        &fixture.store,
        1,
        None,
        EventKind::ExecutionEnded {
            cause: spawn_core::event::EndCause::Lost,
            exit_code: None,
            signal: None,
            evidence: None,
        },
    );
    execution_started(&fixture.store, 2);
    turn_stopped(&fixture.store, 2);
    let resumed = judge(&mut fixture);
    assert_eq!(resumed.ambiguity, Some(Ambiguity::TurnStoppedIdle));
    // A late record of the earlier execution opens nothing in this one.
    append(&fixture.store, 1, None, job("bq1n7c2x4", JobKind::Bash));
    fixture.clock.advance(Duration::from_secs(31));
    let late = judge(&mut fixture);
    assert_ne!(late.reason, Some(WaitReason::WaitingForBackgroundJob));
}

/// What the Bridge appends for one transcript line of execution 1.
fn read_line(store: &MemoryStore, line: &str) {
    for event in spawn_core::ingest::claude_line(line) {
        append(store, 1, event.source_event_id.as_deref(), event.kind);
    }
}

/// The lines of one background Agent, as Claude Code 2.1.317/318 writes
/// them (cut to the fields read): launched, paused with work of its own,
/// finished, resumed by a message, finished again.
const AGENT_LAUNCH: &str = r#"{"type":"user","isSidechain":false,"uuid":"e0246752","version":"2.1.317","toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a2adacaabc9f9b048","description":"Build storage phase 2a","canContinueAgent":true},"message":{"role":"user","content":[{"tool_use_id":"toolu_018hgmBWFMc3fomfMC4sgXBo","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully.\nagentId: a2adacaabc9f9b048 (internal ID)"}]}]}}"#;
const AGENT_PAUSED: &str = r#"{"type":"attachment","isSidechain":false,"uuid":"t1","version":"2.1.318","attachment":{"type":"queued_command","commandMode":"task-notification","prompt":"<task-notification>\n<task-id>a2adacaabc9f9b048</task-id>\n<tool-use-id>toolu_018hgmBWFMc3fomfMC4sgXBo</tool-use-id>\n<status>completed</status>\n<summary>Agent \"Build storage phase 2a\" is waiting on its own background work</summary>\n<note>This agent stopped with background work of its own still running. It may resume on its own when that work completes or reports, and the same task-id notifies again if it does; the result below may be interim.</note>\n<result>The check runs in the background.</result>\n<usage><tool_uses>40</tool_uses></usage>\n</task-notification>"}}"#;
const AGENT_FINISHED: &str = "<task-notification>\\n<task-id>a2adacaabc9f9b048</task-id>\\n<tool-use-id>toolu_018hgmBWFMc3fomfMC4sgXBo</tool-use-id>\\n<status>completed</status>\\n<summary>Agent \\\"Build storage phase 2a\\\" finished</summary>\\n<result>Built and committed.</result>\\n<usage><tool_uses>122</tool_uses></usage>\\n</task-notification>";
const AGENT_RESUMED: &str = r#"{"type":"user","isSidechain":false,"uuid":"8c491cc0","version":"2.1.317","toolUseResult":{"success":true,"message":"Resuming agent a2adaca","resumedAgentId":"a2adacaabc9f9b048","revivedAgentId":"a2adacaabc9f9b048"},"message":{"role":"user","content":[{"tool_use_id":"toolu_01C8SfVsFL53ceFvQyhH19yS","type":"tool_result","content":[{"type":"text","text":"{\"success\":true,\"message\":\"Resuming agent a2adaca\",\"resumedAgentId\":\"a2adacaabc9f9b048\"}"}]}]}}"#;
const AGENT_FINISHED_AGAIN: &str = r#"{"type":"attachment","isSidechain":false,"uuid":"t3","version":"2.1.318","attachment":{"type":"queued_command","commandMode":"task-notification","prompt":"<task-notification>\n<task-id>a2adacaabc9f9b048</task-id>\n<tool-use-id>toolu_01C8SfVsFL53ceFvQyhH19yS</tool-use-id>\n<status>completed</status>\n<summary>Agent \"Build storage phase 2a\" finished</summary>\n<result>The review's points are fixed.</result>\n<usage><tool_uses>39</tool_uses></usage>\n</task-notification>"}}"#;

#[test]
fn a_background_agent_is_open_for_each_run_until_that_run_ends() {
    let mut fixture = fixture();
    let store = fixture.store.clone();
    execution_started(&store, 1);
    read_line(&store, AGENT_LAUNCH);
    turn_stopped(&store, 1);
    waits_on_its_job(&judge(&mut fixture));
    // It paused with work of its own: still running.
    read_line(&store, AGENT_PAUSED);
    read_line(
        &store,
        r#"{"type":"user","isSidechain":false,"uuid":"m1","message":{"role":"user","content":"the agent paused; wait for it"}}"#,
    );
    turn_stopped(&store, 1);
    waits_on_its_job(&judge(&mut fixture));
    assert_eq!(fixture.judge.calls(), 0);
    // It finished: given mid-turn, then as the prompt of a turn, one end.
    let queued = format!(
        r#"{{"type":"attachment","isSidechain":false,"uuid":"t2","attachment":{{"type":"queued_command","prompt":"{AGENT_FINISHED}"}}}}"#
    );
    let prompt = format!(
        r#"{{"type":"user","isSidechain":false,"uuid":"m2","message":{{"role":"user","content":"{AGENT_FINISHED}"}}}}"#
    );
    read_line(&store, &queued);
    read_line(&store, &prompt);
    turn_stopped(&store, 1);
    assert_eq!(
        judge(&mut fixture).ambiguity,
        Some(Ambiguity::TurnStoppedIdle),
        "no job open: judged as before"
    );
    // A message resumes it: open again, until this run's own end.
    read_line(&store, AGENT_RESUMED);
    turn_stopped(&store, 1);
    waits_on_its_job(&judge(&mut fixture));
    read_line(&store, AGENT_FINISHED_AGAIN);
    read_line(
        &store,
        r#"{"type":"user","isSidechain":false,"uuid":"m3","message":{"role":"user","content":"report"}}"#,
    );
    turn_stopped(&store, 1);
    assert_eq!(
        judge(&mut fixture).ambiguity,
        Some(Ambiguity::TurnStoppedIdle)
    );
    let ends = store
        .read_global(GlobalPosition(0), 1000)
        .expect("read")
        .into_iter()
        .filter(|record| record.event.kind.name() == "job.ended")
        .count();
    assert_eq!(ends, 2, "one end per run, not per copy");
}

#[test]
fn a_failed_turn_with_a_job_open_is_still_the_judges_and_it_sees_the_job() {
    let mut fixture = fixture();
    execution_started(&fixture.store, 1);
    append(&fixture.store, 1, None, job("b8w03og7k", JobKind::Bash));
    append(
        &fixture.store,
        1,
        None,
        EventKind::TurnFailed {
            error: "API Error: 529 overloaded".to_string(),
            status: Some(529),
            error_kind: Some("overloaded".to_string()),
        },
    );
    let observation = judge(&mut fixture);
    assert_eq!(observation.ambiguity, Some(Ambiguity::EngineError));
    let seen = fixture
        .judge
        .seen
        .lock()
        .expect("lock")
        .clone()
        .expect("asked");
    assert_eq!(seen.open_jobs, 1);
    assert!(seen.oldest_job_ms.is_some());
}

fn submit(store: &MemoryStore, key: &str, action: CommandAction) {
    store
        .submit(
            &Principal::new("tester").expect("valid principal"),
            &Submission {
                agent_id: agent(),
                session_id: session(),
                idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                action,
            },
        )
        .expect("submit");
}

/// The transcript path the start named.
fn transcript(store: &MemoryStore) -> String {
    store
        .read_global(GlobalPosition(0), 100)
        .expect("read")
        .into_iter()
        .find_map(|record| match record.event.kind {
            EventKind::NativeSessionStarted {
                transcript_path, ..
            } => transcript_path,
            _ => None,
        })
        .expect("the start names the transcript")
}

/// Where the resumed engine writes: the native session's transcript in the
/// latest execution's workspace.
fn resumed_transcript(store: &MemoryStore) -> String {
    let records = store.read_global(GlobalPosition(0), 1000).expect("read");
    let native = records
        .iter()
        .find_map(|record| match &record.event.kind {
            EventKind::NativeSessionStarted {
                native_session_id, ..
            } => Some(native_session_id.clone()),
            _ => None,
        })
        .expect("a native session");
    let workspace = records
        .iter()
        .rev()
        .find_map(|record| match &record.event.kind {
            EventKind::ExecutionStarted { workspace, .. } => Some(workspace.clone()),
            _ => None,
        })
        .expect("an execution");
    spawn_core::bridge::transcript_path(
        &BridgeConfig::new("tester").claude_projects,
        &workspace,
        &native,
    )
}

/// The job records in the log, with the execution each belongs to.
fn job_records(store: &MemoryStore) -> Vec<(String, EventKind)> {
    store
        .read_global(GlobalPosition(0), 1000)
        .expect("read")
        .into_iter()
        .filter(|record| {
            matches!(
                record.event.kind,
                EventKind::JobStarted { .. } | EventKind::JobEnded { .. }
            )
        })
        .map(|record| {
            assert_eq!(record.event.source, Source::Claude);
            (
                record
                    .event
                    .execution_id
                    .map(|execution| execution.to_string())
                    .unwrap_or_default(),
                record.event.kind,
            )
        })
        .collect()
}

/// A gate in a background shell, a Monitor on its log, and the turn's end.
const GATE_TURN: &str = concat!(
    r#"{"type":"assistant","uuid":"a1","isSidechain":false,"message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"sh gate.sh","run_in_background":true}}]}}"#,
    "\n",
    r#"{"type":"user","uuid":"u1","isSidechain":false,"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"Command running in background with ID: b8w03og7k. Output is being written to: /tmp/b8w03og7k.output.","is_error":false}]},"toolUseResult":{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"b8w03og7k"}}"#,
    "\n",
    r#"{"type":"assistant","uuid":"a2","isSidechain":false,"message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Monitor","input":{"command":"tail -f gate.log","timeout_ms":1800000}}]}}"#,
    "\n",
    r#"{"type":"user","uuid":"u2","isSidechain":false,"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"Monitor started (task bv41gl0hm, expires in 30m unless the source ends first)."}]},"toolUseResult":{"taskId":"bv41gl0hm","timeoutMs":1800000,"persistent":false}}"#,
    "\n",
    r#"{"type":"system","subtype":"turn_duration","uuid":"s1","isSidechain":false,"durationMs":5}"#,
    "\n",
);

/// The gate ends: queued, then the prompt of a turn of its own.
const GATE_DONE: &str = concat!(
    r#"{"type":"queue-operation","operation":"enqueue","sessionId":"n","content":"<task-notification>\n<task-id>b8w03og7k</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n<summary>Background command \"sh gate.sh\" completed (exit code 0)</summary>\n</task-notification>"}"#,
    "\n",
    r#"{"type":"user","uuid":"u3","isSidechain":false,"message":{"content":"<task-notification>\n<task-id>b8w03og7k</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n<summary>Background command \"sh gate.sh\" completed (exit code 0)</summary>\n</task-notification>"},"origin":{"kind":"task-notification"}}"#,
    "\n",
    r#"{"type":"system","subtype":"turn_duration","uuid":"s2","isSidechain":false,"durationMs":5}"#,
    "\n",
);

/// After a resume: the new process reports the Monitor stopped.
const RESUMED_TURN: &str = concat!(
    r#"{"type":"user","uuid":"u4","isSidechain":false,"message":{"content":"<task-notification>\n<task-id>bv41gl0hm</task-id>\n<task-id>__orphan_summary__:shell</task-id>\n<status>stopped</status>\n<summary>Background shell command didn't finish before the previous session ended</summary>\n</task-notification>"},"origin":{"kind":"task-notification"}}"#,
    "\n",
    r#"{"type":"system","subtype":"turn_duration","uuid":"s3","isSidechain":false,"durationMs":5}"#,
    "\n",
);

#[test]
fn the_bridge_records_the_jobs_of_an_execution_from_its_transcript() {
    let mut fixture = fixture();
    let store = fixture.store.clone();
    for command in 1..=3 {
        store.set_pane(
            &TerminalName::for_execution(&ExecutionId::for_command(CommandId(command))),
            READY,
        );
    }
    // The archive puts the transcript back where the resumed engine reads it.
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    )
    .with_transcript_archive(store.clone());
    submit(
        &store,
        "key_start",
        CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "run the gate".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
            charter: None,
        },
    );
    bridge.tick().expect("start");
    let path = transcript(&store);
    store.append_file(&path, GATE_TURN);
    bridge.tick().expect("read the turn");
    bridge.tick().expect("a second read records nothing twice");
    let first = ExecutionId::for_command(CommandId(1)).to_string();
    assert_eq!(
        job_records(&store),
        vec![
            (first.clone(), job("b8w03og7k", JobKind::Bash)),
            (first.clone(), job("bv41gl0hm", JobKind::Monitor)),
        ]
    );
    waits_on_its_job(&judge(&mut fixture));

    store.append_file(&path, GATE_DONE);
    bridge.tick().expect("read the notification");
    assert_eq!(
        job_records(&store).last(),
        Some(&(first.clone(), job_ended("b8w03og7k", JobEnd::Completed)))
    );
    waits_on_its_job(&judge(&mut fixture));

    // Stopped and resumed with the Monitor still open: its execution's end
    // closed it. The new process reports it stopped; that changes nothing.
    submit(&store, "key_stop", CommandAction::Stop);
    bridge.tick().expect("stop");
    submit(
        &store,
        "key_resume",
        CommandAction::Resume {
            prompt: Some("go on".to_string()),
            model: None,
        },
    );
    bridge.tick().expect("resume");
    let path = resumed_transcript(&store);
    store.append_file(&path, RESUMED_TURN);
    bridge.tick().expect("read the resumed turn");
    let resumed = store
        .read_global(GlobalPosition(0), 1000)
        .expect("read")
        .into_iter()
        .rev()
        .find_map(|record| match record.event.kind {
            EventKind::ExecutionStarted { .. } => record.event.execution_id,
            _ => None,
        })
        .expect("the resume started an execution");
    assert_ne!(resumed.to_string(), first);
    assert_eq!(
        job_records(&store).last(),
        Some(&(resumed.to_string(), job_ended("bv41gl0hm", JobEnd::Stopped)))
    );
    let after = judge(&mut fixture);
    assert_eq!(after.ambiguity, Some(Ambiguity::TurnStoppedIdle));
}

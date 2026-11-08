//! The one-resume rule against the log itself (task 132): the dispatcher's
//! facts read from a real store (`SPAWN_TEST_DATABASE_URL`, a schema of
//! each case's own), replayed from recorded events or written by a real
//! Bridge over an in-memory terminal host, then decided.

use std::sync::Arc;
use std::time::Duration;

use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::command::{Claimed, CommandAction, Outcome, Submission};
use spawn_core::event::{CommandStep, ENGINE, EndCause, EventKind, ModelProfile, NewEvent, Source};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, EventLog};
use spawn_core::screen::ScreenState;
use spawn_core::time::SystemClock;
use spawn_postgres::PostgresStore;

use super::{DISPATCHER, Facts, Plan, Why, decide};

/// A pane that takes input: an empty input box above the status line.
const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

/// What the Bridge records when a start's ready wait fails (`type_prompt`)
/// and the start is abandoned (`abandon`), as task 130 found them.
const NEVER_READY: &str = "pane never became ready, last state Unknown";

/// A schema of its own in the test database, dropped afterwards.
struct Db {
    store: Arc<PostgresStore>,
    url: String,
    schema: String,
}

impl Db {
    fn new(case: &str) -> Self {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let schema = format!("spawn_test_{}_{nanos:x}_{case}", std::process::id());
        let store = PostgresStore::connect_in_schema(&url, &schema).expect("connect and migrate");
        Self {
            store: Arc::new(store),
            url,
            schema,
        }
    }

    /// Move every record so far `ms` into the past: what ran until now ran
    /// that much longer.
    fn age(&self, ms: i64) {
        let mut client = postgres::Client::connect(&self.url, postgres::NoTls).expect("connect");
        client
            .batch_execute(&format!("set search_path to {}", self.schema))
            .expect("schema");
        client
            .execute(
                "update spawn_events set recorded_at_ms = recorded_at_ms - $1",
                &[&ms],
            )
            .expect("age the log");
    }

    /// The dispatcher's facts as it reads them, and its plan without a
    /// supervisor (escalations stay pending, in the attention queue).
    fn plan(&self) -> (Vec<Facts>, Plan) {
        let rows: Vec<Facts> =
            serde_json::from_value(self.store.dispatch_facts().expect("facts")).expect("rows");
        let now = super::now_ms();
        let plan = decide(&rows, false, now, "spawn");
        (rows, plan)
    }

    fn submit(
        &self,
        principal: &str,
        agent: &AgentId,
        session: &SessionId,
        key: &str,
        action: CommandAction,
    ) -> CommandId {
        self.store
            .submit(
                &Principal::new(principal).expect("principal"),
                &Submission {
                    agent_id: agent.clone(),
                    session_id: session.clone(),
                    idempotency_key: IdempotencyKey::new(key).expect("key"),
                    action,
                },
            )
            .expect("submit")
    }

    /// Submit every order of `plan` as the dispatcher does; the resumes'
    /// keys.
    fn send(&self, plan: Plan) -> Vec<String> {
        let principal = Principal::new(DISPATCHER).expect("principal");
        plan.orders
            .into_iter()
            .map(|order| {
                assert!(
                    matches!(order.action, CommandAction::Resume { .. }),
                    "only resumes here: {order:?}"
                );
                let key = order.key.clone();
                super::submit(&self.store, &principal, order).expect("submit the order");
                key
            })
            .collect()
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Ok(mut client) = postgres::Client::connect(&self.url, postgres::NoTls) {
            let _ = client.batch_execute(&format!("drop schema if exists {} cascade", self.schema));
        }
    }
}

/// Records of one worker session, written as the Bridge writes them.
struct Replay<'a> {
    db: &'a Db,
    agent: AgentId,
    session: SessionId,
    workspace: WorkspaceId,
}

impl Replay<'_> {
    fn record(&self, execution: Option<&ExecutionId>, key: String, kind: EventKind) {
        self.db
            .store
            .append(NewEvent {
                agent_id: self.agent.clone(),
                session_id: self.session.clone(),
                execution_id: execution.cloned(),
                source: Source::Bridge,
                source_event_id: Some(key),
                kind,
            })
            .expect("append");
    }

    /// The Bridge claims the next command, mounts its workspace and starts
    /// its execution.
    fn mount_and_start(&self) -> (Claimed, ExecutionId) {
        let claimed = self
            .db
            .store
            .claim("bridge", Duration::from_mins(1))
            .expect("claim")
            .expect("a command");
        let command = claimed.command_id;
        let execution = ExecutionId::for_command(command);
        self.record(
            None,
            format!("{}:mounted", command.0),
            EventKind::CommandStepped {
                command_id: command,
                step: CommandStep::Mounted,
                workspace: Some(self.workspace.clone()),
            },
        );
        self.record(
            Some(&execution),
            format!("{execution}:started"),
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: self.workspace.clone(),
                command_id: command,
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
            },
        );
        (claimed, execution)
    }

    /// The start's ready wait saw the pane ready.
    fn ready(&self, execution: &ExecutionId) {
        self.record(
            Some(execution),
            format!("{execution}:screen:ready"),
            EventKind::ScreenObserved {
                state: ScreenState::Ready,
                evidence: "❯".to_string(),
            },
        );
    }

    fn ended_lost(&self, execution: &ExecutionId, evidence: &str) {
        self.record(
            Some(execution),
            format!("{execution}:ended"),
            EventKind::ExecutionEnded {
                cause: EndCause::Lost,
                exit_code: None,
                signal: None,
                evidence: Some(evidence.to_string()),
            },
        );
    }

    fn finish(&self, claimed: &Claimed, outcome: &Outcome) {
        self.db.store.finish(claimed, outcome).expect("finish");
    }
}

fn start(prompt: &str) -> CommandAction {
    CommandAction::Start {
        engine: ENGINE.to_string(),
        prompt: prompt.to_string(),
        parent_session_id: None,
        from: None,
        model: None,
        charter: None,
    }
}

/// The only worker row of `rows`.
fn only(rows: &[Facts]) -> &Facts {
    assert_eq!(rows.len(), 1, "{rows:?}");
    &rows[0]
}

/// Task 132 (3): session_worker_objective_129 after the host move,
/// replayed from the records the Bridge wrote. Its task's execution ran
/// for half an hour and was lost; the dispatcher resumed it; the resume
/// was accepted, mounted its workspace and started an execution, whose
/// pane never became ready: the execution ended lost ("its start failed:
/// pane never became ready …") and the command failed. No resume was ever
/// sent under the new execution's key and the young-death count saw
/// nothing, so it was resumed again every 20 s (commands 920-929). The
/// log's own record of that resume escalates it, with the evidence.
#[test]
fn the_129_sequence_replayed_from_events_escalates() {
    let db = Db::new("resume_129");
    let replay = Replay {
        db: &db,
        agent: AgentId::new("agent_worker_objective_129").expect("agent"),
        session: SessionId::new("session_worker_objective_129").expect("session"),
        workspace: WorkspaceId::new("1:/ws/agent_worker_objective_129").expect("workspace"),
    };
    // Its task: started, ready, typed; half an hour later the host moved.
    db.submit(
        "spawn_supervisor",
        &replay.agent,
        &replay.session,
        "task-129",
        start("objective 129"),
    );
    let (claimed, first) = replay.mount_and_start();
    replay.ready(&first);
    replay.finish(&claimed, &Outcome::Succeeded);
    db.age(1_800_000);
    replay.ended_lost(&first, "the engine's terminal is gone");

    let (_, plan) = db.plan();
    assert!(plan.pending.is_empty(), "{:?}", plan.pending);
    let sent = db.send(plan);
    assert_eq!(sent, [format!("dispatch-resume-{first}")], "resumed once");

    // Accepted (by the submission), mounted, started, ended lost.
    let (claimed, second) = replay.mount_and_start();
    replay.ended_lost(&second, &format!("its start failed: {NEVER_READY}"));
    replay.finish(&claimed, &Outcome::Failed(NEVER_READY.to_string()));

    let (rows, plan) = db.plan();
    let facts = only(&rows);
    assert_eq!(facts.resumed_young, 0, "the young-death count sees nothing");
    assert!(
        facts.sent(&format!("dispatch-resume-{second}")).is_none(),
        "and no resume was sent under the new execution's key"
    );
    let failed = facts.failed_resume.as_ref().expect("the failed resume");
    assert_eq!(failed.execution_id, second.as_str());
    assert_eq!(failed.command_id, i64::try_from(claimed.command_id.0).ok());
    assert!(
        plan.orders.is_empty(),
        "not resumed again: {:?}",
        plan.orders
    );
    assert_eq!(plan.pending.len(), 1, "{:?}", plan.pending);
    let item = &plan.pending[0];
    assert_eq!(item.why, Why::ResumeFailed);
    assert_eq!(item.tag(), format!("{} resume-{second}", replay.session));
    assert!(
        item.line
            .contains(&format!("its start failed: {NEVER_READY}")),
        "with the evidence: {}",
        item.line
    );
}

/// A Bridge over the test store and an in-memory terminal host whose
/// panes stay empty (never ready) unless a test draws one.
struct World {
    db: Db,
    host: Arc<MemoryStore>,
    bridge: Bridge,
    agent: AgentId,
    session: SessionId,
}

impl World {
    fn new(case: &str) -> Self {
        let db = Db::new(case);
        let host = Arc::new(MemoryStore::new("resume", Arc::new(SystemClock)));
        let mut config = BridgeConfig::new("tester");
        config.ready_attempts = 2;
        config.ready_interval = Duration::from_millis(1);
        let mut bridge = Bridge::new(
            db.store.clone(),
            db.store.clone(),
            host.clone(),
            host.clone(),
            config,
        );
        bridge.recover().expect("the Bridge recovers");
        Self {
            db,
            host,
            bridge,
            agent: AgentId::new("agent_worker_resume").expect("agent"),
            session: SessionId::new("session_worker_resume").expect("session"),
        }
    }

    fn tick(&mut self) {
        self.bridge.tick().expect("the Bridge ticks");
    }

    fn terminal(command: CommandId) -> TerminalName {
        TerminalName::for_execution(&ExecutionId::for_command(command))
    }

    /// The worker's task ran for half an hour (its pane ready, its prompt
    /// typed), then the host moved: its terminal came back at a shell
    /// prompt, and the Bridge ended the execution as lost.
    fn lost_after_half_an_hour(&mut self) -> ExecutionId {
        let (agent, session) = (self.agent.clone(), self.session.clone());
        let task = self.db.submit(
            "spawn_supervisor",
            &agent,
            &session,
            "task",
            start("the task"),
        );
        self.host.set_pane(&Self::terminal(task), READY);
        self.tick();
        self.db.age(1_800_000);
        self.host
            .untrack(&Self::terminal(task), "a shell prompt after the host move");
        self.tick();
        let (rows, _) = self.db.plan();
        let facts = only(&rows);
        assert!(!facts.live, "{facts:?}");
        assert_eq!(facts.end_cause.as_deref(), Some("lost"));
        ExecutionId::for_command(task)
    }
}

/// Task 132 (1), through a real Bridge: every resume it runs is abandoned
/// at its ready wait (its pane never becomes ready). The dispatcher
/// resumes once and then escalates, with the Bridge's own evidence; on
/// every later pass it sends nothing, however often it decides.
#[test]
fn a_resume_the_bridge_abandons_at_its_ready_wait_is_escalated_not_resumed_again() {
    let mut world = World::new("resume_unready");
    let first = world.lost_after_half_an_hour();
    let mut resumes = Vec::new();
    for _ in 0..5 {
        let (_, plan) = world.db.plan();
        resumes.extend(world.db.send(plan));
        world.tick();
    }
    assert_eq!(
        resumes,
        [format!("dispatch-resume-{first}")],
        "resumed once"
    );
    let (rows, plan) = world.db.plan();
    let facts = only(&rows);
    let failed = facts.failed_resume.as_ref().expect("the failed resume");
    assert!(plan.orders.is_empty(), "{:?}", plan.orders);
    assert_eq!(plan.pending.len(), 1, "{:?}", plan.pending);
    let item = &plan.pending[0];
    assert_eq!(item.why, Why::ResumeFailed);
    assert_eq!(
        item.tag(),
        format!("{} resume-{}", world.session, failed.execution_id)
    );
    assert!(item.line.contains(NEVER_READY), "{}", item.line);
}

/// Task 132 (2), through a real Bridge: a resume that succeeded (its pane
/// became ready and took its prompt) and whose engine was lost again
/// within the minute is no failed resume; the young-death rule escalates
/// it, as before.
#[test]
fn a_resume_that_succeeded_and_died_young_follows_the_young_death_rule_through_the_bridge() {
    let mut world = World::new("resume_young");
    let first = world.lost_after_half_an_hour();
    let (_, plan) = world.db.plan();
    let sent = world.db.send(plan);
    assert_eq!(sent, [format!("dispatch-resume-{first}")]);
    let (rows, _) = world.db.plan();
    let resume = only(&rows)
        .sent(&sent[0])
        .and_then(|sent| sent.id)
        .expect("the resume's command");
    let terminal = World::terminal(CommandId(u64::try_from(resume).expect("an id")));
    world.host.set_pane(&terminal, READY);
    world.tick();
    let (rows, _) = world.db.plan();
    assert!(only(&rows).live, "the resume succeeded");
    world.host.untrack(&terminal, "lost again at once");
    world.tick();
    let (rows, plan) = world.db.plan();
    let facts = only(&rows);
    assert_eq!(facts.resumed_young, 1);
    assert_eq!(facts.failed_resume, None, "its engine became ready");
    assert!(plan.orders.is_empty(), "{:?}", plan.orders);
    assert_eq!(plan.pending.len(), 1, "{:?}", plan.pending);
    assert_eq!(plan.pending[0].why, Why::ResumeFailed);
    assert_eq!(
        plan.pending[0].line,
        "engine was lost again within a minute of its automatic resume"
    );
}

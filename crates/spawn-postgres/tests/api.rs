//! Public API tests for spawn-postgres.
//!
//! A real database is required. `SPAWN_TEST_DATABASE_URL` must point at a
//! disposable server, for example
//! `postgres://postgres:PASSWORD@127.0.0.1:15432/spawn`. The test panics when the
//! variable is missing: silently skipping would let a broken adapter pass.
//!
//! Every case runs in a schema of its own, so a shared database cannot leak
//! events or commands between cases or between runs.

use std::sync::Arc;
use std::time::Duration;

use postgres::{Client, NoTls};
use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::command::{Claimed, CommandAction, Outcome, Submission};
use spawn_core::contract;
use spawn_core::event::{
    ArtifactRef, CheckpointReason, ENGINE, EndCause, EventKind, GlobalPosition, ModelProfile,
    NewEvent, SessionSeq, Source,
};
use spawn_core::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, IdempotencyKey, Principal, SessionId,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::observation::{Confidence, Observation, SemanticState};
use spawn_core::observe::Observer;
use spawn_core::port::{CommandQueue, EventLog};
use spawn_core::time::SystemClock;
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, url};

/// Process-wide connection and cluster-snapshot budget for `tests/api.rs`.
/// Keeps peak active PG connections strictly under `max_connections=30` (and
/// its 27 non-superuser slots) under `--test-threads=12`, and isolates tests
/// that hold cluster-wide shared-catalog DDL XIDs (`CREATE/DROP DATABASE|ROLE`)
/// or deliberate fail-closed watermark probes so they cannot mask sibling tests'
/// one-shot `read_global` assertions.
const MAX_CONCURRENT_API_CASES: usize = 4;

struct ApiBudgetState {
    active_cases: usize,
    exclusive: bool,
    waiting_exclusive: usize,
}

static API_BUDGET: (std::sync::Mutex<ApiBudgetState>, std::sync::Condvar) = (
    std::sync::Mutex::new(ApiBudgetState {
        active_cases: 0,
        exclusive: false,
        waiting_exclusive: 0,
    }),
    std::sync::Condvar::new(),
);

#[derive(Clone, Copy)]
struct ThreadBudget {
    depth: usize,
    exclusive: bool,
}

std::thread_local! {
    static THREAD_BUDGET: std::cell::Cell<ThreadBudget> =
        const { std::cell::Cell::new(ThreadBudget { depth: 0, exclusive: false }) };
}

struct ApiBudgetGuard {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl ApiBudgetGuard {
    fn shared() -> Self {
        let current = THREAD_BUDGET.get();
        if current.depth > 0 {
            THREAD_BUDGET.set(ThreadBudget {
                depth: current.depth + 1,
                exclusive: current.exclusive,
            });
            return Self {
                _not_send: std::marker::PhantomData,
            };
        }
        let (lock, cvar) = &API_BUDGET;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.exclusive
            || state.waiting_exclusive > 0
            || state.active_cases >= MAX_CONCURRENT_API_CASES
        {
            state = cvar
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.active_cases += 1;
        drop(state);
        THREAD_BUDGET.set(ThreadBudget {
            depth: 1,
            exclusive: false,
        });
        Self {
            _not_send: std::marker::PhantomData,
        }
    }

    fn exclusive() -> Self {
        assert_eq!(
            THREAD_BUDGET.get().depth,
            0,
            "exclusive API budget guard cannot nest inside another API budget guard"
        );
        let (lock, cvar) = &API_BUDGET;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.waiting_exclusive += 1;
        while state.exclusive || state.active_cases > 0 {
            state = cvar
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.waiting_exclusive -= 1;
        state.exclusive = true;
        drop(state);
        THREAD_BUDGET.set(ThreadBudget {
            depth: 1,
            exclusive: true,
        });
        Self {
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Drop for ApiBudgetGuard {
    fn drop(&mut self) {
        let current = THREAD_BUDGET.get();
        let remaining = current.depth.saturating_sub(1);
        THREAD_BUDGET.set(ThreadBudget {
            depth: remaining,
            exclusive: if remaining == 0 {
                false
            } else {
                current.exclusive
            },
        });
        if remaining == 0 {
            let (lock, cvar) = &API_BUDGET;
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if current.exclusive {
                state.exclusive = false;
            } else {
                state.active_cases = state.active_cases.saturating_sub(1);
            }
            drop(state);
            cvar.notify_all();
        }
    }
}

struct ApiSchema {
    inner: common::Schema,
    // Field drop order runs `common::Schema::drop` (`DROP SCHEMA`) before releasing `_budget`.
    _budget: ApiBudgetGuard,
}

impl ApiSchema {
    fn name(&self) -> &str {
        self.inner.name()
    }
}

impl std::ops::Deref for ApiSchema {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::fmt::Display for ApiSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

fn store(case: &str) -> (PostgresStore, ApiSchema) {
    let budget = ApiBudgetGuard::shared();
    let (store, schema) = common::store(case);
    (
        store,
        ApiSchema {
            inner: schema,
            _budget: budget,
        },
    )
}

fn agent() -> AgentId {
    AgentId::new("agent_one").expect("valid agent")
}

fn session() -> SessionId {
    SessionId::new("session_one").expect("valid session")
}

fn message(kind: EventKind) -> NewEvent {
    NewEvent {
        agent_id: agent(),
        session_id: session(),
        execution_id: None,
        source: Source::Observer,
        source_event_id: None,
        kind,
    }
}

fn start(submission_session: SessionId, key: &str) -> Submission {
    Submission {
        agent_id: agent(),
        session_id: submission_session,
        idempotency_key: IdempotencyKey::new(key).expect("valid key"),
        action: CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "hello".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
            charter: None,
            model_label: None,
        },
    }
}

#[test]
fn postgres_passes_the_shared_contract() {
    let (store, _schema) = store("contract");
    contract::event_log("pg_evt", &store);
    contract::one_execution("pg_one", &store);
    contract::command_queue("pg_cmd", &store, &store);
}

#[test]
fn append_is_idempotent_per_source_key() {
    let (store, _schema) = store("append");
    let mut keyed = message(EventKind::MessageReceived);
    keyed.source_event_id = Some("hook:prompt_1".to_string());
    let kept = store.append(keyed.clone()).expect("append succeeds");
    assert!(kept.inserted);
    let repeated = store.append(keyed).expect("repeat append succeeds");
    assert!(!repeated.inserted, "the same source key is stored once");
    assert_eq!(
        repeated.record.position, kept.record.position,
        "a repeated key keeps the stored position"
    );
}

#[test]
fn session_sequences_are_dense_and_positions_increase() {
    let (store, _schema) = store("seq");
    for expected in 1..=3 {
        let appended = store
            .append(message(EventKind::TurnStopped {
                input_tokens: Some(10),
                output_tokens: Some(20),
            }))
            .expect("append succeeds");
        assert_eq!(appended.record.session_seq, SessionSeq(expected));
    }
    let read = store
        .read_global(GlobalPosition(0), 10)
        .expect("read succeeds");
    assert_eq!(read.len(), 3);
    assert!(
        read.windows(2)
            .all(|pair| pair[0].position < pair[1].position)
    );
}

#[test]
fn submit_is_idempotent_and_accepts_once() {
    let (store, _schema) = store("submit");
    let principal = Principal::new("tester").expect("valid principal");
    let submission = start(session(), "key_one");
    let first = store
        .submit(&principal, &submission)
        .expect("submit succeeds");
    let second = store
        .submit(&principal, &submission)
        .expect("submit succeeds");
    assert_eq!(first, second, "the same key maps to the same command");
    let read = store
        .read_global(GlobalPosition(0), 10)
        .expect("read succeeds");
    assert_eq!(
        read.iter()
            .filter(|record| record.event.kind.name() == "command.accepted")
            .count(),
        1,
        "command.accepted is written once"
    );
}

#[test]
fn committed_duplicates_do_not_wait_for_the_append_lock() {
    let (store, schema) = store("duplicate_lock");
    let mut keyed = message(EventKind::MessageReceived);
    keyed.source_event_id = Some("hook:prompt_1".to_string());
    let kept = store.append(keyed).expect("append succeeds");
    let principal = Principal::new("tester").expect("valid principal");
    let command = store
        .submit(&principal, &start(session(), "key_duplicate"))
        .expect("submit succeeds");

    let mut holder = admin(&schema);
    let mut holding = holder.transaction().expect("begin");
    holding
        .execute(
            "select pg_advisory_xact_lock(1399957401, hashtext('session_one'))",
            &[],
        )
        .expect("hold session_one L3 append lock");

    let worker_schema = schema.to_string();
    let worker = std::thread::spawn(move || -> Result<_, postgres::Error> {
        let mut client = admin(&worker_schema);
        // A regression waits here and fails instead of hanging the test.
        client.execute("set lock_timeout = '1s'", &[])?;
        let event = client.query_one(
            "select out_id, out_seq, out_inserted from spawn_append(
                 'agent_one', 'session_one', null, 'observer', 'hook:prompt_1',
                 '{\"name\":\"message.received\",\"detail\":{}}'::jsonb)",
            &[],
        )?;
        let repeated = client.query_one(
            "select out_id from spawn_submit(
                 'tester', 'key_duplicate', 'agent_one', 'session_one',
                 '{\"type\":\"stop\"}'::jsonb)",
            &[],
        )?;
        Ok((
            event.get::<_, i64>("out_id"),
            event.get::<_, i64>("out_seq"),
            event.get::<_, bool>("out_inserted"),
            repeated.get::<_, i64>("out_id"),
        ))
    });
    let result = worker.join();
    holding.commit().expect("release the append lock");
    let (event_id, session_seq, inserted, command_id) = result
        .expect("the duplicate reader does not panic")
        .expect("committed duplicates do not wait for the lock");
    assert_eq!(
        u64::try_from(event_id).expect("event_id fits in u64"),
        kept.record.position.0
    );
    assert_eq!(
        u64::try_from(session_seq).expect("session_seq fits in u64"),
        kept.record.session_seq.0
    );
    assert!(!inserted);
    assert_eq!(
        u64::try_from(command_id).expect("command_id fits in u64"),
        command.0
    );
}

#[test]
fn one_session_runs_one_command_at_a_time() {
    let (store, _schema) = store("serial");
    let principal = Principal::new("tester").expect("valid principal");
    let older = store
        .submit(&principal, &start(session(), "key_a"))
        .expect("submit succeeds");
    let younger = store
        .submit(&principal, &start(session(), "key_b"))
        .expect("submit succeeds");
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("one command is ready");
    assert_eq!(claimed.command_id, older, "the older command goes first");
    assert!(
        store
            .claim("w1", Duration::from_mins(1))
            .expect("claim succeeds")
            .is_none(),
        "the younger command waits"
    );
    store
        .finish(&claimed, &Outcome::Succeeded)
        .expect("finish succeeds");
    let next = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("the younger command is ready");
    assert_eq!(next.command_id, younger);
}

/// `spawn_claim` leases a command only when nothing earlier (by id) in its
/// session is unfinished, which holds only if commands commit in id order.
/// A submission that waits for the append lock while another commits must
/// not hold an id below the one that committed first: then both would be
/// leased at once.
#[test]
fn two_commands_of_a_session_are_never_leased_at_once_when_their_submissions_race() {
    let (_store, schema) = store("claim_order");
    let submit = |key: &str| {
        format!(
            "select out_id from spawn_submit('human', '{key}', 'agent_one', 'session_one',
                                             '{{\"type\":\"stop\"}}')"
        )
    };
    let claim = |client: &mut Client| -> Option<i64> {
        client
            .query_opt("select out_id from spawn_claim('w1', 60000)", &[])
            .expect("claim")
            .map(|row| row.get(0))
    };

    // Another writer holds the append lock.
    let mut holder = admin(&schema);
    let mut holding = holder.transaction().expect("begin");
    holding
        .execute(
            "select pg_advisory_xact_lock(1399957401, hashtext('session_one'))",
            &[],
        )
        .expect("hold session_one L3 append lock");

    // A submission starts and waits for it, in a transaction it keeps open.
    let (submitted_tx, submitted) = std::sync::mpsc::channel::<(i32, Option<i64>)>();
    let (commit_tx, commit) = std::sync::mpsc::channel::<()>();
    let waiter_schema = schema.to_string();
    let waiter_submit = submit("key_waiter");
    let waiter = std::thread::spawn(move || {
        let mut client = admin(&waiter_schema);
        let pid: i32 = client
            .query_one("select pg_backend_pid()", &[])
            .expect("pid")
            .get(0);
        submitted_tx.send((pid, None)).expect("send the pid");
        let mut transaction = client.transaction().expect("begin");
        let id: i64 = transaction
            .query_one(&waiter_submit, &[])
            .expect("the waiting submission")
            .get(0);
        submitted_tx.send((pid, Some(id))).expect("send the id");
        commit.recv().expect("told to commit");
        transaction.commit().expect("commit the waiting submission");
        id
    });
    let (pid, _) = submitted.recv().expect("the waiter's pid");
    let mut watcher = admin(&schema);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let waiting: bool = watcher
            .query_one(
                "select exists (select 1 from pg_locks
                                 where pid = $1 and locktype = 'advisory' and not granted)",
                &[&pid],
            )
            .expect("read the locks")
            .get(0);
        if waiting {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the submission waits for the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The holder submits a command of the same session and commits first.
    let first: i64 = holding
        .query_one(&submit("key_holder"), &[])
        .expect("the holder's submission")
        .get(0);
    holding.commit().expect("commit the holder");
    let (_, second) = submitted.recv().expect("the waiting submission returns");
    let second = second.expect("its id");
    assert!(
        first < second,
        "the command that committed first has the lower id ({first}, {second})"
    );

    // While the second is uncommitted, the first is leased; once it is
    // committed, it waits for the first to finish.
    assert_eq!(claim(&mut watcher), Some(first));
    commit_tx.send(()).expect("commit the waiter");
    assert_eq!(waiter.join().expect("the waiter"), second);
    assert_eq!(
        claim(&mut watcher),
        None,
        "one command of the session is leased at a time"
    );
}

#[test]
fn an_expired_lease_is_taken_again_with_a_new_attempt() {
    let (store, schema) = store("lease");
    let principal = Principal::new("tester").expect("valid principal");
    let id = store
        .submit(&principal, &start(session(), "key_lease"))
        .expect("submit succeeds");
    let first = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("a command is ready");
    assert_eq!(first.attempt, 1);
    assert!(
        store
            .claim("w2", Duration::from_mins(1))
            .expect("claim succeeds")
            .is_none(),
        "a live lease is not stolen"
    );

    admin(&schema)
        .execute(
            "update spawn_commands set lease_until_ms = spawn_now_ms() - 1 where id = $1",
            &[&id.0.cast_signed()],
        )
        .expect("expire the lease");

    let second = store
        .claim("w2", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("the expired lease is taken again");
    assert_eq!(second.attempt, 2, "an expired lease increments the attempt");
    assert!(
        store.finish(&first, &Outcome::Succeeded).is_err(),
        "the old attempt cannot finish the command"
    );
    store
        .finish(&second, &Outcome::Succeeded)
        .expect("the current attempt finishes");
}

#[test]
fn worker_ownership_is_not_reentrant_on_one_store() {
    let (store, _schema) = store("worker_ownership");
    let (ownership, recovered) = store
        .recover_worker("spawn-worker")
        .expect("the first worker owns its name");
    assert_eq!(recovered, 0);
    let error = store
        .recover_worker("spawn-worker")
        .err()
        .expect("PostgreSQL session locks are re-entrant, but workers are not");
    assert!(error.to_string().contains("still owned"), "{error}");
    drop(ownership);
    let (_replacement, _) = store
        .recover_worker("spawn-worker")
        .expect("dropping the owner releases the name on the same store");
}

/// A one-shot `spawn tick` (`recover_worker_leases` off) beside a live holder
/// that leased without the ownership protocol, as black-box `040-queue` does
/// with `spawn_claim` in psql: the lease stays live, and the tick holds the
/// worker's name neither in its process nor in the server.
#[test]
fn a_bridge_without_worker_recovery_takes_no_ownership_and_keeps_live_leases() {
    let (store, schema) = store("tick_no_recovery");
    let store = Arc::new(store);
    let principal = Principal::new("tester").expect("valid principal");
    let id = store
        .submit(&principal, &start(session(), "key_tick"))
        .expect("submit succeeds");
    let held = store
        .claim("spawn-worker", Duration::from_mins(2))
        .expect("claim succeeds")
        .expect("the live holder leases the start");
    let host = Arc::new(MemoryStore::new("tick", Arc::new(SystemClock)));
    let mut config = BridgeConfig::new("spawn-worker");
    config.recover_worker_leases = false;
    let mut tick = Bridge::new(store.clone(), store.clone(), host.clone(), host, config);
    tick.recover().expect("a tick recovers its view");
    tick.recover().expect("and again, still without the worker");

    let row = admin(&schema)
        .query_one(
            "select lease_attempt, lease_until_ms > spawn_now_ms() as live
               from spawn_commands where id = $1",
            &[&id.0.cast_signed()],
        )
        .expect("the command row");
    assert_eq!(row.get::<_, i32>("lease_attempt"), 1);
    assert!(row.get::<_, bool>("live"), "the live lease is not expired");
    assert!(
        store
            .claim("spawn-worker", Duration::from_mins(2))
            .expect("claim succeeds")
            .is_none(),
        "nothing is claimable while the holder's lease lives"
    );
    store
        .finish(&held, &Outcome::Succeeded)
        .expect("the holder's attempt is still current");

    // With the tick still alive, a resident loop takes the worker on another
    // server session (no advisory lock held) and on the tick's own store (no
    // local claim).
    let loop_store = PostgresStore::connect_in_schema(&url(), schema.name()).expect("loop store");
    let (loop_owner, recovered) = loop_store
        .recover_worker("spawn-worker")
        .expect("the tick holds no server-side ownership");
    assert_eq!(recovered, 0);
    drop(loop_owner);
    let (_same_store, _) = store
        .recover_worker("spawn-worker")
        .expect("the tick holds no ownership in its own store");
    drop(tick);
}

#[cfg(unix)]
struct KillOnDrop(std::process::Child);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Child half of `sigkill_recovers_a_claim_without_waiting_for_its_ttl`.
/// Normal test runs have no helper environment and do nothing.
#[test]
#[cfg(unix)]
fn sigkill_claim_helper() {
    if std::env::var("SPAWN_SIGKILL_CLAIM_HELPER").as_deref() != Ok("1") {
        return;
    }
    let schema = std::env::var("SPAWN_SIGKILL_CLAIM_SCHEMA").expect("helper schema");
    let store = Arc::new(PostgresStore::connect_in_schema(&url(), &schema).expect("helper store"));
    let host = Arc::new(MemoryStore::new("crash-helper", Arc::new(SystemClock)));
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        host.clone(),
        host,
        BridgeConfig::new("spawn-worker"),
    );
    bridge.recover().expect("helper owns the worker");
    let claimed = store
        .claim("spawn-worker", Duration::from_mins(2))
        .expect("helper claim succeeds")
        .expect("the submitted start is claimed");
    assert_eq!(claimed.attempt, 1);
    // The parent waits for that row, then sends this process SIGKILL. No
    // destructor, release, finish, or cleanup runs on this path.
    loop {
        std::thread::sleep(Duration::from_mins(1));
    }
}

#[cfg(unix)]
fn new_postgres_bridge(schema: &str, host_name: &str) -> (Arc<PostgresStore>, Bridge) {
    let store = Arc::new(PostgresStore::connect_in_schema(&url(), schema).expect("bridge store"));
    let host = Arc::new(MemoryStore::new(host_name, Arc::new(SystemClock)));
    let bridge = Bridge::new(
        store.clone(),
        store.clone(),
        host.clone(),
        host,
        BridgeConfig::new("spawn-worker"),
    );
    (store, bridge)
}

#[cfg(unix)]
fn spawn_sigkill_helper_and_wait_for_claim(
    schema: &str,
    client: &mut Client,
    command_id: CommandId,
) -> KillOnDrop {
    use std::process::{Command, Stdio};

    let mut child = KillOnDrop(
        Command::new(std::env::current_exe().expect("test executable"))
            .args(["sigkill_claim_helper", "--exact", "--nocapture"])
            .env("SPAWN_SIGKILL_CLAIM_HELPER", "1")
            .env("SPAWN_SIGKILL_CLAIM_SCHEMA", schema)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the isolated loop child"),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .query_opt(
                "select lease_attempt from spawn_commands
                  where id = $1 and lease_worker = 'spawn-worker'
                    and lease_until_ms > spawn_now_ms()",
                &[&command_id.0.cast_signed()],
            )
            .expect("read the child claim")
            .is_some()
        {
            return child;
        }
        if let Some(status) = child.0.try_wait().expect("inspect the child") {
            panic!("the claim helper exited before its claim: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the child claimed in time"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[cfg(unix)]
fn sigkill_recovers_a_claim_without_waiting_for_its_ttl() {
    use std::os::unix::process::ExitStatusExt;

    let (store, schema) = store("sigkill_recovery");
    let store = Arc::new(store);
    let principal = Principal::new("tester").expect("valid principal");
    let submission = start(session(), "key_sigkill");
    let id = store
        .submit(&principal, &submission)
        .expect("submit succeeds");
    let abandoned = Claimed {
        command_id: id,
        attempt: 1,
        agent_id: submission.agent_id.clone(),
        session_id: submission.session_id.clone(),
        principal: principal.as_str().to_string(),
        created_at_ms: 0,
        action: serde_json::to_value(&submission.action).expect("action json"),
    };
    let mut client = admin(&schema);
    let mut child = spawn_sigkill_helper_and_wait_for_claim(schema.name(), &mut client, id);

    // A genuinely live predecessor must be refused, not have its lease taken.
    let (_live_store, mut blocked) = new_postgres_bridge(schema.name(), "live-peer");
    let error = blocked
        .recover()
        .expect_err("the live child's ownership refuses a replacement");
    assert!(error.to_string().contains("still owned"), "{error}");
    assert_eq!(
        client
            .query_one(
                "select lease_attempt from spawn_commands where id = $1",
                &[&id.0.cast_signed()],
            )
            .expect("the live lease remains")
            .get::<_, i32>(0),
        1
    );
    drop(blocked);

    child.0.kill().expect("SIGKILL only the isolated helper");
    let status = child.0.wait().expect("wait for the helper");
    assert_eq!(status.signal(), Some(9), "the helper died by SIGKILL");
    let recovery_started = std::time::Instant::now();

    // The server can take a moment to observe the dead socket. Retry only the
    // ownership acquisition; no command is touched while the old lock lives.
    let (recovered_store, mut replacement) = new_postgres_bridge(schema.name(), "recovered");
    loop {
        match replacement.recover() {
            Ok(()) => break,
            Err(error)
                if error.to_string().contains("still owned")
                    && recovery_started.elapsed() < Duration::from_secs(10) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("replacement recovery failed: {error}"),
        }
    }
    let reclaimed = recovered_store
        .claim("spawn-worker", Duration::from_mins(2))
        .expect("replacement claim succeeds")
        .expect("SIGKILL released ownership and recovery released the lease");
    let recovery = recovery_started.elapsed();
    eprintln!("SIGKILL_RECOVERY_MS={}", recovery.as_millis());
    assert!(
        recovery < Duration::from_secs(5),
        "recovery stays far below 120 s"
    );
    assert_eq!(reclaimed.command_id, id);
    assert_eq!(reclaimed.attempt, 2);
    assert!(
        recovered_store
            .finish(&abandoned, &Outcome::Succeeded)
            .is_err(),
        "the killed process's attempt remains fenced"
    );
    recovered_store
        .finish(&reclaimed, &Outcome::Succeeded)
        .expect("the replacement finishes once");
    assert_eq!(
        client
            .query_one("select count(*) from spawn_events", &[])
            .expect("count events")
            .get::<_, i64>(0),
        2,
        "accepted and finished are both retained, with no duplicate events"
    );
}

#[test]
fn views_answer_from_the_event_log() {
    let (store, schema) = store("views");
    let observation = Observation::basic(
        SemanticState::Working,
        None,
        SessionSeq(1),
        Confidence::new(0.75).expect("valid confidence"),
        "heuristic",
    );
    store
        .append(message(EventKind::ObservationRecorded { observation }))
        .expect("append succeeds");
    store
        .append(NewEvent {
            source: Source::Claude,
            ..message(EventKind::TurnStopped {
                input_tokens: Some(10),
                output_tokens: Some(20),
            })
        })
        .expect("append succeeds");

    let row = admin(&schema)
        .query_one(
            "select state, judge, session_seq, based_on_seq from agent_state",
            &[],
        )
        .expect("agent_state answers");
    let state: Option<String> = row.get(0);
    assert_eq!(state.as_deref(), Some("WORKING"));
    let judge: Option<String> = row.get(1);
    assert_eq!(judge.as_deref(), Some("heuristic"));
    let seq: i64 = row.get(2);
    assert_eq!(seq, 1, "agent_state keeps the observation row");
    let based_on: Option<i64> = row.get(3);
    assert_eq!(based_on, Some(1));

    let event_id: i64 = row_event_id(&schema);
    let state = store
        .state(&session())
        .expect("state reads")
        .expect("one state");
    assert_eq!(state.event_id, GlobalPosition(event_id.unsigned_abs()));
    assert_eq!(state.observation.state, SemanticState::Working);
    assert_eq!(store.states().expect("states read"), vec![state]);

    let timeline = store
        .timeline(&session(), SessionSeq(0), 10)
        .expect("timeline reads");
    let names: Vec<&str> = timeline.iter().map(|e| e.event_name.as_str()).collect();
    assert_eq!(
        names,
        vec!["turn.stopped"],
        "the timeline has no judgements"
    );
    assert!(
        store
            .timeline(&session(), SessionSeq(2), 10)
            .expect("timeline reads")
            .is_empty(),
        "the cursor is exclusive"
    );
    let judgements = admin(&schema)
        .query(
            "select event_name from agent_timeline where source = 'observer'",
            &[],
        )
        .expect("agent_timeline answers");
    assert!(judgements.is_empty());
    let judged: i64 = admin(&schema)
        .query_one("select count(*) from agent_judgements", &[])
        .expect("agent_judgements answers")
        .get(0);
    assert_eq!(judged, 1, "the judgement view holds the observation");

    let fresh = store
        .freshness(&session())
        .expect("freshness reads")
        .expect("one row");
    assert_eq!(fresh.last_session_seq, SessionSeq(2));
    assert_eq!(fresh.event_count, 2);
    assert!(
        store
            .freshness(&SessionId::new("session_none").expect("valid session"))
            .expect("freshness reads")
            .is_none(),
        "a session without events has no row"
    );
}

fn row_event_id(schema: &str) -> i64 {
    admin(schema)
        .query_one(
            "select id from spawn_events where event_name = 'observation.recorded'",
            &[],
        )
        .expect("observation event")
        .get(0)
}

fn lifecycle(execution: &ExecutionId, source: Source, key: &str, kind: EventKind) -> NewEvent {
    NewEvent {
        agent_id: agent(),
        session_id: session(),
        execution_id: Some(execution.clone()),
        source,
        source_event_id: Some(key.to_string()),
        kind,
    }
}

#[test]
fn an_execution_ends_on_the_bridge_event_not_the_runner_report() {
    let (store, schema) = store("executions");
    let execution = ExecutionId::new("exec_1").expect("valid execution");
    let ended = |cause| EventKind::ExecutionEnded {
        cause,
        exit_code: Some(0),
        signal: None,
        evidence: None,
    };
    let row = |label: &str| {
        let row = admin(&schema)
            .query_one(
                "select engine, ended_at_ms, cause, exit_reported from agent_executions",
                &[],
            )
            .expect("one execution row");
        let engine: Option<String> = row.get(0);
        assert_eq!(engine.as_deref(), Some("claude"), "{label}");
        let ended_at: Option<i64> = row.get(1);
        let cause: Option<String> = row.get(2);
        let reported: bool = row.get(3);
        (ended_at.is_some(), cause, reported)
    };

    store
        .append(lifecycle(
            &execution,
            Source::Bridge,
            "exec_1:started",
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("9:/workspaces/9").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        ))
        .expect("append succeeds");
    assert_eq!(row("started"), (false, None, false));

    store
        .append(lifecycle(
            &execution,
            Source::Claude,
            "exec_1:exited",
            ended(EndCause::Exited),
        ))
        .expect("append succeeds");
    assert_eq!(
        row("reported"),
        (false, None, true),
        "a runner report alone does not end the execution"
    );

    store
        .append(lifecycle(
            &execution,
            Source::Bridge,
            "exec_1:ended",
            ended(EndCause::Exited),
        ))
        .expect("append succeeds");
    assert_eq!(row("ended"), (true, Some("exited".to_string()), true));

    let executions = store.executions(&session()).expect("executions read");
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].command_id, Some(CommandId(1)));
    let duration = executions[0]
        .duration_ms
        .expect("an ended execution has a duration");
    assert!(duration >= 0);
}

#[test]
fn two_connections_racing_to_start_one_session_record_one_execution() {
    let (first, schema) = store("one_execution");
    let second = spawn_postgres::PostgresStore::connect_in_schema(&url(), &schema)
        .expect("a second Bridge's connection");
    let started = |name: &str| {
        lifecycle(
            &ExecutionId::new(name).expect("valid execution"),
            Source::Bridge,
            &format!("{name}:started"),
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("9:/workspaces/9").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        )
    };
    let outcomes: Vec<Result<_, _>> = std::thread::scope(|scope| {
        let a = scope.spawn(|| first.append(started("exec_a")));
        let b = scope.spawn(|| second.append(started("exec_b")));
        vec![a.join().expect("thread a"), b.join().expect("thread b")]
    });
    let taken = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(taken, 1, "{outcomes:?}");
    assert!(
        outcomes.iter().any(|outcome| matches!(
            outcome,
            Err(spawn_core::port::StoreError::Rejected(why)) if why.contains("already runs")
        )),
        "{outcomes:?}"
    );
    let live: i64 = admin(&schema)
        .query_one(
            "select count(*) from agent_executions where ended_at_ms is null",
            &[],
        )
        .expect("executions read")
        .get(0);
    assert_eq!(live, 1);

    // The lost-execution path: the Bridge's end frees the session, and the
    // next start (a resume) is taken on either connection.
    let winner = if outcomes[0].is_ok() {
        "exec_a"
    } else {
        "exec_b"
    };
    first
        .append(lifecycle(
            &ExecutionId::new(winner).expect("valid execution"),
            Source::Bridge,
            &format!("{winner}:ended"),
            EventKind::ExecutionEnded {
                cause: EndCause::Lost,
                exit_code: None,
                signal: None,
                evidence: None,
            },
        ))
        .expect("the end of a lost execution");
    second
        .append(started("exec_c"))
        .expect("a start after the end");
}

fn tool_event(execution: &ExecutionId, key: &str, kind: EventKind) -> NewEvent {
    lifecycle(execution, Source::Claude, key, kind)
}

/// A tool call's summary goes through the real store as the event's detail,
/// which the session API and the trace read; the store redacts it again
/// (event_log's redact_event), so a summary written with a credential in it
/// is stored without; an event without a summary reads as before.
#[test]
fn a_tool_calls_summary_is_stored_redacted_and_read_back() {
    let (store, _schema) = store("tool_summary");
    let execution = ExecutionId::new("exec_1").expect("valid execution");
    store
        .append(tool_event(
            &execution,
            "a:start",
            EventKind::ToolStarted {
                tool_name: "Bash".to_string(),
                tool_use_id: "a".to_string(),
                summary: Some("PGPASSWORD=s3cret psql -h db".to_string()),
                // A writer's claim the store's redaction makes untrue.
                summary_whole: true,
            },
        ))
        .expect("append succeeds");
    store
        .append(tool_event(
            &execution,
            "b:start",
            EventKind::ToolStarted {
                tool_name: "ScheduleWakeup".to_string(),
                tool_use_id: "b".to_string(),
                summary: None,
                summary_whole: false,
            },
        ))
        .expect("append succeeds");
    store
        .append(tool_event(
            &execution,
            "c:start",
            EventKind::ToolStarted {
                tool_name: "Bash".to_string(),
                tool_use_id: "c".to_string(),
                summary: Some("git status".to_string()),
                summary_whole: true,
            },
        ))
        .expect("append succeeds");
    let timeline = store
        .timeline(&session(), SessionSeq(0), 100)
        .expect("timeline read");
    let started: Vec<&serde_json::Value> = timeline
        .iter()
        .filter(|entry| entry.event_name == "tool.started")
        .filter_map(|entry| entry.detail.as_ref())
        .collect();
    assert_eq!(started.len(), 3, "{timeline:?}");
    assert_eq!(started[0]["tool_name"], "Bash");
    assert_eq!(started[0]["summary"], "PGPASSWORD=[redacted] psql -h db");
    assert!(
        started[0].get("summary_whole").is_none(),
        "a redacted summary is not whole: {}",
        started[0]
    );
    assert!(
        started[1].get("summary").is_none() && started[1].get("summary_whole").is_none(),
        "no summary, no keys: {}",
        started[1]
    );
    assert_eq!(started[2]["summary"], "git status");
    assert_eq!(started[2]["summary_whole"], true, "{}", started[2]);
}

#[test]
fn open_tools_are_started_minus_finished_in_live_executions() {
    let (store, _schema) = store("open_tools");
    let execution = ExecutionId::new("exec_1").expect("valid execution");
    let started = |id: &str| EventKind::ToolStarted {
        tool_name: "Bash".to_string(),
        tool_use_id: id.to_string(),
        summary: None,
        summary_whole: false,
    };
    store
        .append(tool_event(&execution, "a:start", started("a")))
        .expect("append succeeds");
    store
        .append(tool_event(&execution, "b:start", started("b")))
        .expect("append succeeds");
    store
        .append(tool_event(
            &execution,
            "a:done",
            EventKind::ToolCompleted {
                tool_use_id: "a".to_string(),
                duration_ms: Some(5),
            },
        ))
        .expect("append succeeds");
    let open = store.open_tools(&session()).expect("open tools read");
    let ids: Vec<&str> = open.iter().map(|tool| tool.tool_use_id.as_str()).collect();
    assert_eq!(ids, vec!["b"]);

    store
        .append(lifecycle(
            &execution,
            Source::Bridge,
            "exec_1:ended",
            EventKind::ExecutionEnded {
                cause: EndCause::Exited,
                exit_code: Some(0),
                signal: None,
                evidence: None,
            },
        ))
        .expect("append succeeds");
    assert!(
        store
            .open_tools(&session())
            .expect("open tools read")
            .is_empty(),
        "an ended execution has no open tools"
    );
}

#[test]
fn lineage_follows_links_to_every_ancestor() {
    let (store, schema) = store("lineage");
    for (child, parent) in [("session_b", "session_a"), ("session_c", "session_b")] {
        store
            .append(NewEvent {
                agent_id: AgentId::new(child.replace("session_", "agent_")).expect("valid agent"),
                session_id: SessionId::new(child).expect("valid session"),
                ..message(EventKind::SessionLinked {
                    parent_session_id: SessionId::new(parent).expect("valid session"),
                })
            })
            .expect("append succeeds");
    }
    let rows = admin(&schema)
        .query(
            "select ancestor_session_id, depth from agent_lineage \
             where session_id = 'session_c' order by depth",
            &[],
        )
        .expect("agent_lineage answers");
    let chain: Vec<(String, i32)> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
    assert_eq!(
        chain,
        vec![("session_b".to_string(), 1), ("session_a".to_string(), 2)]
    );
}

#[test]
fn a_task_lists_its_command_and_execution_events() {
    let (store, schema) = store("task_events");
    let principal = Principal::new("tester").expect("valid principal");
    let command = store
        .submit(&principal, &start(session(), "key_1"))
        .expect("submit succeeds");
    let claimed = store
        .claim("worker", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("one command");
    let execution = ExecutionId::new("exec_1").expect("valid execution");
    store
        .append(lifecycle(
            &execution,
            Source::Bridge,
            "exec_1:started",
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("9:/workspaces/9").expect("valid workspace"),
                command_id: command,
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        ))
        .expect("append succeeds");
    store
        .append(tool_event(&execution, "turn", EventKind::MessageReceived))
        .expect("append succeeds");
    store
        .append(message(EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        }))
        .expect("an event outside the execution");
    store
        .finish(&claimed, &Outcome::Succeeded)
        .expect("finish succeeds");

    let id = i64::try_from(command.0).expect("small id");
    let names: Vec<String> = admin(&schema)
        .query(
            "select event_name from agent_task_events where command_id = $1 order by session_seq",
            &[&id],
        )
        .expect("agent_task_events answers")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        names,
        vec![
            "command.accepted",
            "execution.started",
            "message.received",
            "command.finished"
        ]
    );
    let task = admin(&schema)
        .query_one(
            "select action, state, succeeded, lease_attempt from agent_tasks where command_id = $1",
            &[&id],
        )
        .expect("agent_tasks answers");
    let action: Option<String> = task.get(0);
    let state: String = task.get(1);
    let succeeded: Option<bool> = task.get(2);
    let attempt: i32 = task.get(3);
    assert_eq!(
        (action.as_deref(), state.as_str(), succeeded, attempt),
        (Some("start"), "finished", Some(true), 1)
    );
}

#[test]
fn artifact_and_relation_views_read_their_events() {
    let (store, schema) = store("artifacts");
    store
        .append(message(EventKind::ArtifactCommitted {
            files: Vec::new(),
            reason: CheckpointReason::TurnStopped,
            artifact: ArtifactRef {
                project: "shop".to_string(),
                branch: BranchName::new("b1").expect("valid branch"),
                commit: CommitId::new("c1").expect("valid commit"),
            },
        }))
        .expect("append succeeds");
    store
        .append(message(EventKind::SessionLinked {
            parent_session_id: SessionId::new("session_parent").expect("valid session"),
        }))
        .expect("append succeeds");

    let artifact = admin(&schema)
        .query_one(
            "select project, branch, commit_id, reason from agent_artifacts",
            &[],
        )
        .expect("one artifact row");
    let columns: Vec<Option<String>> = (0..4).map(|index| artifact.get(index)).collect();
    let expected = ["shop", "b1", "c1", "turn_stopped"].map(|text| Some(text.to_string()));
    assert_eq!(columns, expected);

    let parent: Option<String> = admin(&schema)
        .query_one("select parent_session_id from agent_relations", &[])
        .expect("one relation row")
        .get(0);
    assert_eq!(parent.as_deref(), Some("session_parent"));
}

#[test]
fn finish_reports_missing_stale_done_and_already() {
    let (store, schema) = store("finish");
    let principal = Principal::new("tester").expect("valid principal");
    let id = store
        .submit(&principal, &start(session(), "key_finish"))
        .expect("submit succeeds");
    let claimed = store
        .claim("worker", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("one command is ready");
    let command = i64::try_from(id.0).expect("small id");
    let attempt = i32::try_from(claimed.attempt).expect("small attempt");
    let mut client = admin(&schema);
    let mut finish = |command: i64, attempt: i32| -> String {
        client
            .query_one(
                "select spawn_finish($1, $2, true, null)",
                &[&command, &attempt],
            )
            .expect("spawn_finish answers")
            .get(0)
    };
    assert_eq!(finish(command + 1000, attempt), "missing");
    assert_eq!(finish(command, attempt + 1), "stale");
    assert_eq!(finish(command, attempt), "done");
    assert_eq!(finish(command, attempt), "already");
}

#[test]
fn the_gateway_view_rejects_a_principal_without_a_grant() {
    let (store, schema) = store("grant");
    // A plain insert through `agent_commands` runs the trigger as the
    // database session user, which has no grant.
    let error = admin(&schema)
        .execute(
            "insert into agent_commands (agent_id, session_id, idempotency_key, action) \
             values ('agent_one', 'session_one', 'key_1', '{\"type\":\"start\"}'::jsonb)",
            &[],
        )
        .expect_err("an ungranted insert is refused");
    let code = error
        .code()
        .expect("a database error carries a code")
        .code()
        .to_string();
    assert_eq!(code, "42501", "the trigger raises insufficient privilege");
    assert!(
        store
            .submit(
                &Principal::new("tester").expect("valid principal"),
                &start(session(), "key_ok"),
            )
            .is_ok(),
        "the sanctioned path still works"
    );
}

#[test]
fn a_granted_principal_inserts_and_reads_only_its_agents() {
    let (store, schema) = store("granted");
    let mut client = admin(&schema);
    store
        .append(message(EventKind::MessageReceived))
        .expect("append succeeds");
    store
        .append(NewEvent {
            agent_id: AgentId::new("agent_two").expect("valid agent"),
            session_id: SessionId::new("session_two").expect("valid session"),
            ..message(EventKind::MessageReceived)
        })
        .expect("append succeeds");
    let readable = |client: &mut Client| -> Vec<String> {
        client
            .query(
                "select session_id from spawn_read_freshness() order by session_id",
                &[],
            )
            .expect("spawn_read_freshness answers")
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    assert_eq!(
        readable(&mut client),
        vec!["session_one", "session_two"],
        "the owner reads every agent"
    );

    // Another login reads only the agents its principal holds a grant for:
    // this database's reader login acts as `spawn_reader`.
    let mut reader = common::as_login(&schema, "spawn_reader");
    assert!(readable(&mut reader).is_empty(), "no grant reads nothing");
    client
        .execute(
            "insert into spawn_grants (principal, agent_id) values ('spawn_reader', 'agent_one')",
            &[],
        )
        .expect("grant");
    assert_eq!(readable(&mut reader), vec!["session_one"]);
    let timeline: i64 = reader
        .query_one(
            "select count(*) from spawn_read_timeline('session_two')",
            &[],
        )
        .expect("spawn_read_timeline answers")
        .get(0);
    assert_eq!(timeline, 0, "an ungranted agent's session reads empty");
    drop(reader);
    client
        .execute(
            "insert into spawn_grants (principal, agent_id) values (session_user, 'agent_one')",
            &[],
        )
        .expect("grant");

    let row = client
        .query_one(
            "insert into agent_commands (agent_id, session_id, idempotency_key, action) \
             values ('agent_one', 'session_one', 'key_1', '{\"type\":\"stop\"}'::jsonb) \
             returning id",
            &[],
        )
        .expect("a granted insert is accepted");
    let id: i64 = row.get(0);
    let action: Option<String> = client
        .query_one(
            "select action from agent_tasks where command_id = $1",
            &[&id],
        )
        .expect("the command is queued")
        .get(0);
    assert_eq!(action.as_deref(), Some("stop"));
}

#[test]
fn a_global_read_returns_session_records_in_order() {
    let (store, _schema) = store("global");
    for kind in [
        EventKind::MessageReceived,
        EventKind::TurnStopped {
            input_tokens: Some(10),
            output_tokens: Some(20),
        },
    ] {
        store.append(message(kind)).expect("append succeeds");
    }
    let records = store
        .read_global(GlobalPosition(0), 10)
        .expect("read succeeds");
    assert_eq!(records.len(), 2);
    assert!(
        records
            .windows(2)
            .all(|pair| pair[0].position < pair[1].position)
    );
}

#[test]
fn a_source_this_build_does_not_know_stops_no_reader() {
    let (store, schema) = store("unknown_source");
    let store = Arc::new(store);
    let mut client = admin(&schema);
    store
        .append(message(EventKind::MessageReceived))
        .expect("append succeeds");
    // A writer since removed: a run of its records longer than any reader's
    // batch, one of them with a kind this build does decode.
    client
        .batch_execute(
            "do $$ begin
               for n in 1..450 loop
                 perform spawn_append('agent_one', 'session_one', null, 'layerfs',
                   'layerfs:' || n,
                   jsonb_build_object('name', 'layerfs.mounted', 'detail',
                                      jsonb_build_object('n', n)));
               end loop;
               perform spawn_append('agent_one', 'session_one', null, 'codex', 'codex:1',
                 '{\"name\": \"tool.started\", \"detail\": {\"tool_name\": \"x\", \"tool_use_id\": \"t1\"}}');
             end $$",
        )
        .expect("append records of unknown sources");
    // The first record of another session comes after the run.
    store
        .append(NewEvent {
            agent_id: AgentId::new("agent_two").expect("valid agent"),
            session_id: SessionId::new("session_two").expect("valid session"),
            ..message(EventKind::MessageReceived)
        })
        .expect("append succeeds");

    let names = |records: &[spawn_core::event::EventRecord]| -> Vec<String> {
        records
            .iter()
            .map(|record| record.event.session_id.to_string())
            .collect()
    };
    let all = store
        .read_global(GlobalPosition(0), 200)
        .expect("a global read skips what it cannot read");
    assert_eq!(names(&all), vec!["session_one", "session_two"]);
    let past = store
        .read_global(all[0].position, 1)
        .expect("a read that starts at the run");
    assert_eq!(
        names(&past),
        vec!["session_two"],
        "the read goes past the run"
    );

    let timeline = store
        .timeline(&session(), SessionSeq(0), 1000)
        .expect("the timeline reads");
    // The timeline leaves the Observer's records out and shows the rest as
    // stored.
    assert_eq!(timeline.len(), 451, "the timeline shows every record");
    assert_eq!(timeline[0].source, "layerfs");
    assert_eq!(timeline[450].source, "codex");
    let tail = store.tail_json(&session(), 5).expect("tail reads");
    assert_eq!(tail["events"].as_array().map(Vec::len), Some(5));
    client
        .batch_execute("select spawn_control_refold()")
        .expect("the control stream refolds");

    let host = Arc::new(MemoryStore::new("unknown_source", Arc::new(SystemClock)));
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        host.clone(),
        host,
        BridgeConfig::new("tester"),
    );
    bridge.recover().expect("the Bridge recovers");
    assert_eq!(bridge.session_count(), 2, "the Bridge reads past the run");
    bridge.tick().expect("the Bridge ticks");
    let mut observer = Observer::new(store.clone());
    observer.recover().expect("the Observer recovers");
    assert_eq!(
        observer.session_count(),
        2,
        "the Observer reads past the run"
    );
    observer.tick().expect("the Observer ticks");
}

#[test]
fn migrating_twice_changes_nothing() {
    let (store, _schema) = store("migrate");
    store.migrate().expect("a second migration succeeds");
}

fn observed(seq: u64) -> NewEvent {
    message(EventKind::ObservationRecorded {
        observation: Observation::basic(
            SemanticState::Working,
            None,
            SessionSeq(seq),
            Confidence::new(0.5).expect("valid confidence"),
            "heuristic",
        ),
    })
}

/// The projection row as text, next to the same row folded from the log.
const PROJECTION_VS_FOLD: &str = "
    select p::text, (
        select row(f.session_id, (select agent_id from spawn_events
                                  where session_id = p.session_id order by session_seq limit 1),
                   f.last_session_seq, f.last_event_id,
                   f.last_event_at_ms, f.event_count, o.observation, o.session_seq,
                   o.id, o.recorded_at_ms)::text
        from (select session_id, max(session_seq) as last_session_seq, max(id) as last_event_id,
                     max(recorded_at_ms) as last_event_at_ms, count(*) as event_count
              from spawn_events where session_id = p.session_id group by session_id) f
        left join lateral (
            select kind -> 'detail' -> 'observation' as observation, session_seq, id, recorded_at_ms
            from spawn_events
            where session_id = f.session_id and event_name = 'observation.recorded'
            order by session_seq desc limit 1) o on true)
    from spawn_session_state p order by p.session_id";

fn projection_matches_fold(client: &mut Client) {
    let rows = client.query(PROJECTION_VS_FOLD, &[]).expect("compare");
    assert!(!rows.is_empty(), "the projection has rows");
    for row in rows {
        let projected: String = row.get(0);
        let folded: String = row.get(1);
        assert_eq!(
            projected, folded,
            "the projection equals the fold of the log"
        );
    }
}

#[test]
fn the_projection_follows_the_log_and_rebuilds_from_it() {
    let (store, schema) = store("projection");
    let mut client = admin(&schema);
    store.append(observed(1)).expect("append succeeds");
    store
        .append(message(EventKind::MessageReceived))
        .expect("append succeeds");
    store.append(observed(2)).expect("append succeeds");
    store
        .append(message(EventKind::MessageReceived))
        .expect("append succeeds");
    projection_matches_fold(&mut client);
    let (seq, based): (i64, i64) = {
        let row = client
            .query_one("select session_seq, based_on_seq from agent_state", &[])
            .expect("agent_state answers");
        (row.get(0), row.get(1))
    };
    assert_eq!((seq, based), (3, 2), "the latest observation wins");

    // The guard: a row out of step with the log is folded again.
    client
        .execute(
            "update spawn_session_state set last_session_seq = 100, event_count = 0",
            &[],
        )
        .expect("move ahead");
    store
        .append(message(EventKind::MessageReceived))
        .expect("append succeeds");
    projection_matches_fold(&mut client);

    // Another agent's event is refused: the session stays its owner's.
    assert!(
        store
            .append(NewEvent {
                agent_id: AgentId::new("agent_0").expect("valid agent"),
                ..message(EventKind::MessageReceived)
            })
            .is_err(),
        "a session has one agent"
    );
    let owner: String = client
        .query_one("select agent_id from spawn_session_state", &[])
        .expect("projection row")
        .get(0);
    assert_eq!(
        owner, "agent_one",
        "the first event's agent owns the session"
    );

    client
        .execute("delete from spawn_session_state", &[])
        .expect("drop the projection rows");
    store.migrate().expect("migration backfills");
    projection_matches_fold(&mut client);
}

#[test]
fn status_joins_the_judgement_and_the_latest_execution() {
    let (store, schema) = store("status");
    let execution = ExecutionId::new("exec_status").expect("valid execution");
    store.append(observed(1)).expect("append succeeds");
    store
        .append(lifecycle(
            &execution,
            Source::Bridge,
            "exec_status:started",
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        ))
        .expect("append succeeds");
    let row = admin(&schema)
        .query_one(
            "select state, execution_id, live, last_session_seq from agent_status",
            &[],
        )
        .expect("agent_status answers");
    assert_eq!(row.get::<_, Option<String>>(0).as_deref(), Some("WORKING"));
    assert_eq!(
        row.get::<_, Option<String>>(1).as_deref(),
        Some("exec_status")
    );
    assert!(row.get::<_, bool>(2), "a started execution is live");
    assert_eq!(row.get::<_, i64>(3), 2);
    // The record names no terminal: the pane is the execution's own.
    let (terminal, workspace) = store
        .latest_terminal(&session())
        .expect("query succeeds")
        .expect("an execution");
    assert_eq!(terminal.as_str(), "spawn-exec_status");
    assert_eq!(workspace, "1:/workspaces/1");
}

#[test]
fn the_gateway_refuses_a_session_another_agent_owns() {
    let (store, schema) = store("session_owner");
    let mut client = admin(&schema);
    store
        .append(message(EventKind::MessageReceived))
        .expect("session_one belongs to agent_one");
    client
        .execute(
            "insert into spawn_grants (principal, agent_id) values (session_user, 'agent_worker_*')",
            &[],
        )
        .expect("grant the workers");
    let error = client
        .execute(
            "insert into agent_commands (agent_id, session_id, idempotency_key, action) \
             values ('agent_worker_x', 'session_one', 'k1', '{\"type\":\"stop\"}'::jsonb)",
            &[],
        )
        .expect_err("a worker grant does not reach agent_one's session");
    assert_eq!(
        error.code(),
        Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    );
    client
        .execute(
            "insert into agent_commands (agent_id, session_id, idempotency_key, action) \
             values ('agent_worker_x', 'session_worker_x', 'k2', '{\"type\":\"stop\"}'::jsonb)",
            &[],
        )
        .expect("a new session is the worker's own");
}

#[test]
fn a_gateway_submission_returns_its_own_command() {
    let (store, schema) = store("gateway_id");
    let mut client = admin(&schema);
    client
        .execute(
            "insert into spawn_grants (principal, agent_id) values (session_user, 'agent_one')",
            &[],
        )
        .expect("grant");
    let mine = store
        .submit_via_gateway(&start(session(), "mine"))
        .expect("submit");
    let principal = Principal::new("spawn").expect("valid principal");
    let newer = store
        .submit(&principal, &start(session(), "newer"))
        .expect("a newer command in the same session");
    let again = store
        .submit_via_gateway(&start(session(), "mine"))
        .expect("the same key again");
    assert_ne!(mine, newer);
    assert_eq!(again, mine, "the retry names the command it made");
}

#[test]
fn views_are_closed_to_other_roles() {
    let _budget = ApiBudgetGuard::exclusive();
    let (_store, schema) = store("revoke");
    let mut client = admin(&schema);
    // Roles are the server's, not the schema's: named by the schema, which
    // is this run's alone (the pid alone is the same in every gate's PID
    // namespace, and two gates on one server would share the role).
    let role = format!("{schema}_reader");
    let password = common::password();
    client
        .batch_execute(&format!(
            "drop role if exists {role}; create role {role} login password '{password}'; \
             grant usage on schema {schema} to {role}"
        ))
        .expect("a plain role");
    let mut plain = common::connect_as(&schema, &role, &password);
    for view in [
        "agent_state",
        "agent_status",
        "agent_timeline",
        "agent_tasks",
    ] {
        let error = plain
            .query(&format!("select * from {view}"), &[])
            .expect_err("a plain role cannot read a view");
        assert_eq!(
            error.code(),
            Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{view}"
        );
    }
    let rows = plain
        .query("select * from spawn_read_status()", &[])
        .expect("the read function answers");
    assert!(rows.is_empty(), "no grant reads nothing");
    drop(plain);
    client
        .batch_execute(&format!(
            "revoke usage on schema {schema} from {role}; drop role {role}"
        ))
        .expect("clean up the role");
}

#[test]
fn a_login_without_grants_reads_no_command_ids() {
    let _budget = ApiBudgetGuard::exclusive();
    let (store, schema) = store("public_ids");
    store
        .submit(
            &Principal::new("human").expect("valid principal"),
            &start(session(), "someone_elses"),
        )
        .expect("a command");
    let mut client = admin(&schema);
    let role = common::schema_name("plain").replace("spawn_test_", "spawn_plain_");
    let password = common::password();
    client
        .batch_execute(&format!(
            "create role {role} login password '{password}'; \
             grant usage on schema {schema} to {role}"
        ))
        .expect("a login with no grants");
    let mut owner = client;
    let mut client = common::connect_as(&schema, &role, &password);
    for query in [
        "select id from agent_commands",
        "select count(*) from agent_commands",
        "select max(id) from spawn_commands",
        "select last_value from spawn_commands_id_seq",
    ] {
        let error = client.query(query, &[]).expect_err(query);
        assert_eq!(
            error.code(),
            Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{query}"
        );
    }
    let tasks = client
        .query("select command_id from spawn_read_tasks()", &[])
        .expect("the reader answers");
    assert!(tasks.is_empty(), "no grant lists no command");
    let error = client
        .query(
            "select spawn_gateway_submit('agent_one', 'session_one', 'k', '{\"type\":\"stop\"}')",
            &[],
        )
        .expect_err("no grant submits nothing");
    assert_eq!(
        error.code(),
        Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    );
    drop(client);
    owner
        .batch_execute(&format!(
            "revoke usage on schema {schema} from {role}; drop role {role}"
        ))
        .expect("clean up the role");
}

/// What every role may do in the schema, from the access lists themselves
/// (a function without one is executable by PUBLIC): the gateway's insert
/// and submit, the grant-filtered readers, and pure helpers.
#[test]
fn public_holds_only_the_gateway_the_readers_and_pure_helpers() {
    let (_store, schema) = store("public_acl");
    let granted: Vec<String> = admin(&schema)
        .query(
            "select format('%s %s', o.name, a.privilege_type)
               from (select c.relname::text as name,
                            coalesce(c.relacl, acldefault(case c.relkind when 'S' then 's'::\"char\"
                                                          else 'r'::\"char\" end, c.relowner)) as acl
                       from pg_class c
                      where c.relnamespace = $1::text::regnamespace and c.relkind in ('r', 'v', 'S')
                     union all
                     select p.proname::text, coalesce(p.proacl, acldefault('f'::\"char\", p.proowner))
                       from pg_proc p
                      where p.pronamespace = $1::text::regnamespace) o
              cross join lateral aclexplode(o.acl) a
              where a.grantee = 0
              order by 1",
            &[&&*schema],
        )
        .expect("read the access lists")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        granted,
        [
            "agent_commands INSERT",
            "spawn_experience_int EXECUTE",
            "spawn_gateway_insert EXECUTE",
            "spawn_gateway_submit EXECUTE",
            "spawn_grant_matches EXECUTE",
            "spawn_login_role EXECUTE",
            "spawn_mission_caller EXECUTE",
            "spawn_now_ms EXECUTE",
            "spawn_principal EXECUTE",
            "spawn_read_freshness EXECUTE",
            "spawn_read_judgements EXECUTE",
            "spawn_read_state EXECUTE",
            "spawn_read_status EXECUTE",
            "spawn_read_tasks EXECUTE",
            "spawn_read_timeline EXECUTE",
            "spawn_source_allowed EXECUTE",
        ]
    );
}

/// Column names and types of every read view. A change here breaks SQL
/// readers; update the snapshot on purpose.
#[test]
fn view_columns_match_the_snapshot() {
    let (_store, schema) = store("columns");
    let rows = admin(&schema)
        .query(
            "select table_name, string_agg(column_name || ' ' || data_type, ', ' order by ordinal_position)
               from information_schema.columns
              where table_schema = $1 and table_name like 'agent\\_%'
              group by table_name order by table_name",
            &[&&*schema],
        )
        .expect("information_schema answers");
    let actual: Vec<String> = rows
        .iter()
        .map(|row| format!("{}: {}", row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect();
    let expected = [
        "agent_artifacts: session_id text, agent_id text, execution_id text, project text, branch text, commit_id text, reason text, session_seq bigint, recorded_at_ms bigint",
        "agent_commands: id bigint, agent_id text, session_id text, idempotency_key text, action jsonb, created_at_ms bigint",
        "agent_executions: session_id text, agent_id text, execution_id text, engine text, terminal text, workspace text, started_at_ms bigint, cause text, exit_code integer, ended_at_ms bigint, exit_reported boolean, duration_ms bigint, command_id bigint, signal integer, evidence text, launch jsonb",
        "agent_freshness: session_id text, agent_id text, last_session_seq bigint, last_event_id bigint, last_event_at_ms bigint, event_count bigint",
        "agent_judgements: session_id text, session_seq bigint, event_id bigint, agent_id text, state text, reason text, based_on_seq bigint, observation jsonb, recorded_at_ms bigint",
        "agent_lineage: session_id text, agent_id text, ancestor_session_id text, depth integer",
        "agent_open_jobs: session_id text, agent_id text, execution_id text, event_id bigint, job_id text, job_kind text, session_seq bigint, started_at_ms bigint",
        "agent_open_tools: session_id text, agent_id text, execution_id text, tool_use_id text, tool_name text, session_seq bigint, started_at_ms bigint",
        "agent_relations: session_id text, agent_id text, parent_session_id text, recorded_at_ms bigint",
        "agent_state: session_id text, agent_id text, state text, reason text, confidence double precision, based_on_seq bigint, judge text, session_seq bigint, recorded_at_ms bigint, event_id bigint, observation jsonb",
        "agent_status: session_id text, agent_id text, state text, reason text, observation_seq bigint, last_session_seq bigint, last_event_at_ms bigint, execution_id text, engine text, started_at_ms bigint, ended_at_ms bigint, exit_reported boolean, live boolean, launch jsonb",
        "agent_task_events: command_id bigint, session_id text, agent_id text, session_seq bigint, event_id bigint, execution_id text, event_name text, recorded_at_ms bigint",
        "agent_tasks: command_id bigint, agent_id text, session_id text, action text, state text, lease_worker text, lease_attempt integer, succeeded boolean, error text, created_at_ms bigint, finished_at_ms bigint, prompt text, principal text",
        "agent_timeline: session_id text, session_seq bigint, id bigint, agent_id text, execution_id text, source text, event_name text, detail jsonb, recorded_at_ms bigint",
    ];
    assert_eq!(actual, expected);
}

#[test]
fn a_delivered_task_starts_a_new_episode_and_a_nudge_does_not() {
    let (store, _schema) = store("episode");
    let worker = AgentId::new("agent_worker_a").expect("valid agent");
    let session = SessionId::new("session_worker_a").expect("valid session");
    store
        .append(NewEvent {
            agent_id: worker.clone(),
            session_id: session.clone(),
            ..message(EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            })
        })
        .expect("append succeeds");
    let submit = |principal: &str, key: &str| {
        let submission = Submission {
            agent_id: worker.clone(),
            idempotency_key: IdempotencyKey::new(key).expect("valid key"),
            action: CommandAction::Input {
                text: key.to_string(),
                tags: Vec::new(),
            },
            ..start(session.clone(), key)
        };
        let id = store
            .submit(
                &Principal::new(principal).expect("valid principal"),
                &submission,
            )
            .expect("submit succeeds");
        i64::try_from(id.0).expect("small id")
    };
    let episode = || {
        let facts = store.dispatch_facts().expect("facts");
        let row = &facts.as_array().expect("rows")[0];
        (
            row["episode"].as_i64(),
            row["episode_action"].as_str().map(str::to_string),
        )
    };
    let human = submit("human", "human-1");
    submit("spawn-dispatch", "dispatch-nudge-session_worker_a-1");
    assert_eq!(episode(), (Some(human), Some("input".to_string())));
    let delivered = submit("spawn-dispatch", "deliver-4-0");
    assert_eq!(episode().0, Some(delivered), "a new task is a new episode");
}

#[test]
fn dispatch_facts_name_the_latest_delivery_however_old() {
    let (store, schema) = store("facts_delivery");
    let worker = AgentId::new("agent_worker_a").expect("valid agent");
    let session = SessionId::new("session_worker_a").expect("valid session");
    store
        .append(NewEvent {
            agent_id: worker.clone(),
            session_id: session.clone(),
            ..message(EventKind::MessageReceived)
        })
        .expect("append succeeds");
    let submit = |principal: &str, key: &str| {
        let submission = Submission {
            agent_id: worker.clone(),
            idempotency_key: IdempotencyKey::new(key).expect("valid key"),
            action: CommandAction::Input {
                text: key.to_string(),
                tags: Vec::new(),
            },
            ..start(session.clone(), key)
        };
        store
            .submit(
                &Principal::new(principal).expect("valid principal"),
                &submission,
            )
            .expect("submit succeeds")
    };
    let row = || store.dispatch_facts().expect("facts")[0].clone();
    assert!(row()["last_delivery"].is_null(), "nothing delivered yet");
    submit("spawn-dispatch", "deliver-4-0");
    let latest = submit("spawn-dispatch", "deliver-7-1");
    submit("spawn-dispatch", "dispatch-nudge-session_worker_a-1");
    submit("human", "deliver-9-0");
    // A failed command carries its error (what a retried start looks at).
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("a command waits");
    store
        .finish(
            &claimed,
            &Outcome::Failed("the source did not answer".to_string()),
        )
        .expect("finish succeeds");
    let failed = row()["dispatched"]
        .as_array()
        .expect("dispatched")
        .iter()
        .find(|sent| sent["succeeded"] == serde_json::json!(false))
        .cloned()
        .expect("the failed command is among the dispatched");
    assert_eq!(failed["error"].as_str(), Some("the source did not answer"));
    // Two days ago: out of the window `dispatched` keeps for other commands.
    admin(&schema)
        .execute(
            "update spawn_commands set created_at_ms = created_at_ms - 2 * 86400000",
            &[],
        )
        .expect("age the commands");
    let facts = row();
    let kept: Vec<&str> = facts["dispatched"]
        .as_array()
        .expect("dispatched")
        .iter()
        .filter_map(|sent| sent["key"].as_str())
        .collect();
    assert_eq!(
        kept,
        vec!["deliver-4-0", "deliver-7-1"],
        "only the deliveries stay, to be confirmed however old"
    );
    assert!(
        facts["dispatched"][0]["id"].is_i64(),
        "a delivery names its command"
    );
    assert_eq!(
        facts["last_delivery"].as_str(),
        Some("deliver-7-1"),
        "the dispatcher's latest delivery (command {}), not a nudge or another principal's",
        latest.0
    );
}

/// A cancel notice stays among the dispatcher's facts however old: a failed
/// one goes again under the next number (`cancel-notice-<task>-<n>`), and a
/// notice older than the day the facts keep for other commands, read as
/// never sent, made the retry reuse a key already used. A submit under an
/// existing key makes no new command, so nothing was sent and the resident
/// stayed blocked. A cancel's stop (`cancel-stop-<task>-<execution>`) and
/// the dispatcher's retries of the Stops into that execution
/// (`stop-<execution>-retry-<n>`) stay too: a resident paused or slow to
/// recover past a day was stopped for its task, and is not stopped again.
#[test]
fn a_dispatchers_cancel_notices_stay_among_its_facts_however_old() {
    let (store, schema) = store("facts_cancel_notice");
    let worker = AgentId::new("agent_resident_core").expect("valid agent");
    let session = SessionId::new("session_resident_core").expect("valid session");
    store
        .append(NewEvent {
            agent_id: worker.clone(),
            session_id: session.clone(),
            ..message(EventKind::MessageReceived)
        })
        .expect("append succeeds");
    let submit = |key: &str| {
        store
            .submit(
                &Principal::new("spawn-dispatch").expect("valid principal"),
                &Submission {
                    agent_id: worker.clone(),
                    idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                    action: CommandAction::Input {
                        text: key.to_string(),
                        tags: Vec::new(),
                    },
                    ..start(session.clone(), key)
                },
            )
            .expect("submit succeeds")
    };
    let notice = submit("cancel-notice-4-0");
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("a command waits");
    store
        .finish(&claimed, &Outcome::Failed("pane not ready".to_string()))
        .expect("finish succeeds");
    submit("cancel-stop-4-exec_1");
    submit("stop-exec_1-retry-1");
    submit("stop-exec_2-retry-1");
    submit("stop-exec_1");
    admin(&schema)
        .execute(
            "update spawn_commands set created_at_ms = created_at_ms - 2 * 86400000",
            &[],
        )
        .expect("age the commands");
    let facts = store.dispatch_facts().expect("facts")[0].clone();
    let kept: Vec<(&str, Option<bool>)> = facts["dispatched"]
        .as_array()
        .expect("dispatched")
        .iter()
        .filter_map(|sent| Some((sent["key"].as_str()?, sent["succeeded"].as_bool())))
        .collect();
    assert_eq!(
        kept,
        vec![
            ("cancel-notice-4-0", Some(false)),
            ("cancel-stop-4-exec_1", None),
            ("stop-exec_1-retry-1", None),
        ],
        "the failed notice, the cancel's stop and the retry of a Stop into its execution \
         stay, two days old; another execution's retry and another stop do not"
    );
    assert_eq!(
        submit("cancel-notice-4-0"),
        notice,
        "a key already used makes no new command"
    );
}

/// A delivery refused before anything was typed (a pane not ready) is not
/// the worker's last delivery, nor an episode: it told the worker nothing.
/// One that may have typed is, and so is one not finished yet.
#[test]
fn a_delivery_that_never_reached_the_worker_is_not_its_last() {
    let (store, _schema) = store("facts_unreached");
    let worker = AgentId::new("agent_resident_core").expect("valid agent");
    let session = SessionId::new("session_resident_core").expect("valid session");
    store
        .append(NewEvent {
            agent_id: worker.clone(),
            session_id: session.clone(),
            ..message(EventKind::MessageReceived)
        })
        .expect("append succeeds");
    // Submit `key` as the dispatcher and finish it with `outcome` (none:
    // left open).
    let deliver = |key: &str, outcome: Option<Outcome>| {
        let submission = Submission {
            agent_id: worker.clone(),
            idempotency_key: IdempotencyKey::new(key).expect("valid key"),
            action: CommandAction::Input {
                text: key.to_string(),
                tags: Vec::new(),
            },
            ..start(session.clone(), key)
        };
        let id = store
            .submit(
                &Principal::new("spawn-dispatch").expect("valid principal"),
                &submission,
            )
            .expect("submit succeeds");
        if let Some(outcome) = outcome {
            let claimed = store
                .claim("w1", Duration::from_mins(1))
                .expect("claim succeeds")
                .expect("the command waits");
            assert_eq!(claimed.command_id, id, "{key} is the one waiting");
            store.finish(&claimed, &outcome).expect("finish succeeds");
        }
        i64::try_from(id.0).expect("small id")
    };
    let row = || store.dispatch_facts().expect("facts")[0].clone();
    let reached = deliver("deliver-24-0", Some(Outcome::Succeeded));
    deliver(
        "deliver-25-0",
        Some(Outcome::Failed(
            "pane state Busy does not accept input".to_string(),
        )),
    );
    let facts = row();
    assert_eq!(
        facts["last_delivery"].as_str(),
        Some("deliver-24-0"),
        "the refused delivery of task 25 is not the worker's"
    );
    assert_eq!(facts["episode"].as_i64(), Some(reached), "nor an episode");
    assert!(
        facts["dispatched"]
            .as_array()
            .expect("dispatched")
            .iter()
            .any(|sent| sent["key"] == "deliver-25-0" && sent["succeeded"] == false),
        "the refusal stays among the attempts, for the retry"
    );
    deliver(
        "deliver-25-0-retry-1",
        Some(Outcome::Failed(format!(
            "the text {}",
            spawn_core::bridge::MAY_HAVE_TYPED
        ))),
    );
    assert_eq!(
        row()["last_delivery"].as_str(),
        Some("deliver-25-0-retry-1"),
        "one that may have typed may have reached the worker"
    );
    let open = deliver("deliver-26-0", None);
    let facts = row();
    assert_eq!(
        facts["last_delivery"].as_str(),
        Some("deliver-26-0"),
        "in flight"
    );
    assert_eq!(facts["episode"].as_i64(), Some(open));
}

/// A worker's session with a command helper: `send(principal, key,
/// action, outcome)` submits and, given an outcome, claims and finishes it.
struct Worker<'a> {
    store: &'a spawn_postgres::PostgresStore,
    agent: AgentId,
    session: SessionId,
}

impl Worker<'_> {
    fn send(
        &self,
        principal: &str,
        key: &str,
        action: CommandAction,
        outcome: Option<Outcome>,
    ) -> i64 {
        let submission = Submission {
            agent_id: self.agent.clone(),
            session_id: self.session.clone(),
            idempotency_key: IdempotencyKey::new(key).expect("valid key"),
            action,
        };
        let id = self
            .store
            .submit(
                &Principal::new(principal).expect("valid principal"),
                &submission,
            )
            .expect("submit succeeds");
        if let Some(outcome) = outcome {
            let claimed = self
                .store
                .claim("w1", Duration::from_mins(1))
                .expect("claim succeeds")
                .expect("the command waits");
            assert_eq!(claimed.command_id, id, "{key} is the one waiting");
            self.store
                .finish(&claimed, &outcome)
                .expect("finish succeeds");
        }
        i64::try_from(id.0).expect("small id")
    }

    fn input(&self, principal: &str, key: &str, outcome: Option<Outcome>) -> i64 {
        let action = CommandAction::Input {
            text: key.to_string(),
            tags: Vec::new(),
        };
        self.send(principal, key, action, outcome)
    }

    fn event(&self, execution: &str, key: &str, kind: EventKind) {
        self.store
            .append(NewEvent {
                agent_id: self.agent.clone(),
                session_id: self.session.clone(),
                execution_id: Some(ExecutionId::new(execution).expect("valid execution")),
                source: Source::Bridge,
                source_event_id: Some(key.to_string()),
                kind,
            })
            .expect("append succeeds");
    }

    fn started(&self, execution: &str, command: i64) {
        self.event(
            execution,
            &format!("{execution}:started"),
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                command_id: CommandId(u64::try_from(command).expect("positive")),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        );
    }

    fn ended(&self, execution: &str) {
        self.event(
            execution,
            &format!("{execution}:ended"),
            EventKind::ExecutionEnded {
                cause: EndCause::Stopped,
                exit_code: None,
                signal: None,
                evidence: None,
            },
        );
    }
}

/// Audit #2 and #3: every Stop and Discard sent during the live execution,
/// by anyone, is a fact with its outcome, read from the command queue (so
/// the dispatcher's pursuit of a failed one survives any restart); one from
/// before the execution started, or of an execution that ended, is not.
#[test]
fn dispatch_facts_list_the_stops_of_the_live_execution() {
    let (store, _schema) = store("facts_stops");
    let worker = Worker {
        store: &store,
        agent: AgentId::new("agent_worker_a").expect("valid agent"),
        session: SessionId::new("session_worker_a").expect("valid session"),
    };
    let row = || store.dispatch_facts().expect("facts")[0].clone();
    worker.send(
        "human",
        "old-stop",
        CommandAction::Stop,
        Some(Outcome::Succeeded),
    );
    let start = worker.send(
        "spawn-dispatch",
        "deliver-4-0",
        start(worker.session.clone(), "deliver-4-0").action,
        Some(Outcome::Succeeded),
    );
    worker.started("exec_1", start);
    assert_eq!(row()["stops"], serde_json::json!([]));
    let switch = worker.send(
        "spawn-dispatch",
        "switch-model-4-0",
        CommandAction::Stop,
        Some(Outcome::Failed("kill-session refused".to_string())),
    );
    worker.input("human", "hello", Some(Outcome::Succeeded));
    let discard = worker.send(
        "spawn-integrate",
        "integrate-abort-4",
        CommandAction::Discard,
        None,
    );
    let stops = row()["stops"].clone();
    let listed: Vec<(i64, &str, &str, &str)> = stops
        .as_array()
        .expect("stops")
        .iter()
        .map(|stop| {
            (
                stop["id"].as_i64().expect("id"),
                stop["key"].as_str().expect("key"),
                stop["principal"].as_str().expect("principal"),
                stop["action"].as_str().expect("action"),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (switch, "switch-model-4-0", "spawn-dispatch", "stop"),
            (discard, "integrate-abort-4", "spawn-integrate", "discard"),
        ]
    );
    assert_eq!(stops[0]["succeeded"], false);
    assert_eq!(stops[0]["error"], "kill-session refused");
    assert!(stops[0]["finished_at_ms"].is_i64());
    assert!(stops[1]["succeeded"].is_null(), "in flight");
    worker.ended("exec_1");
    assert_eq!(row()["stops"], serde_json::json!([]), "nothing is live");
}

/// Audit #16: what a decision rests on is kept however old: the nudges of
/// the current episode and the resume of the current execution. Out of the
/// day's window they read as "not sent", and the dispatcher would plan them
/// again (deduplicated to nothing) instead of escalating.
#[test]
fn dispatch_facts_keep_the_current_episodes_nudges_however_old() {
    let (store, schema) = store("facts_window");
    let worker = Worker {
        store: &store,
        agent: AgentId::new("agent_worker_a").expect("valid agent"),
        session: SessionId::new("session_worker_a").expect("valid session"),
    };
    let start = worker.send(
        "human",
        "start",
        start(worker.session.clone(), "start").action,
        Some(Outcome::Succeeded),
    );
    worker.started("exec_1", start);
    let episode = worker.input("human", "go on", Some(Outcome::Succeeded));
    for key in [
        format!("dispatch-nudge-session_worker_a-{episode}"),
        format!("dispatch-nudge-session_worker_a-{episode}-retry-1"),
        format!("dispatch-nudge-session_worker_a-{episode}0"),
        "dispatch-nudge-session_worker_a-1".to_string(),
        "dispatch-resume-exec_1".to_string(),
        "dispatch-resume-exec_0".to_string(),
    ] {
        worker.input("spawn-dispatch", &key, Some(Outcome::Succeeded));
    }
    admin(&schema)
        .execute(
            "update spawn_commands set created_at_ms = created_at_ms - 2 * 86400000",
            &[],
        )
        .expect("age the commands");
    let facts = store.dispatch_facts().expect("facts")[0].clone();
    assert_eq!(facts["episode"].as_i64(), Some(episode));
    let kept: Vec<&str> = facts["dispatched"]
        .as_array()
        .expect("dispatched")
        .iter()
        .filter_map(|sent| sent["key"].as_str())
        .collect();
    assert_eq!(
        kept,
        [
            format!("dispatch-nudge-session_worker_a-{episode}").as_str(),
            &format!("dispatch-nudge-session_worker_a-{episode}-retry-1"),
            "dispatch-resume-exec_1",
        ]
    );
}

/// Audit #16: a closed ephemeral task stays among the delivery rows for as
/// long as its worker runs, not only for a day: until then its worker is
/// stopped (and its stop pursued), and its session stays managed.
#[test]
fn a_closed_ephemeral_task_stays_a_delivery_row_while_its_worker_runs() {
    let (store, schema) = store("rows_live");
    let mission = store
        .mission_add("One", &serde_json::json!([]), "human")
        .expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_cancel(task).expect("cancel");
    let worker = Worker {
        store: &store,
        agent: AgentId::new(format!("agent_worker_w_{task}")).expect("valid agent"),
        session: SessionId::new(format!("session_worker_w_{task}")).expect("valid session"),
    };
    let start = worker.send(
        "spawn-dispatch",
        &format!("deliver-{task}-0"),
        start(worker.session.clone(), "s").action,
        Some(Outcome::Succeeded),
    );
    worker.started("exec_1", start);
    admin(&schema)
        .execute(
            "update mission_tasks set updated_at_ms = updated_at_ms - 2 * 86400000",
            &[],
        )
        .expect("age the task");
    let rows = || -> Vec<i64> {
        store
            .delivery_rows()
            .expect("rows")
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|row| row["id"].as_i64())
            .collect()
    };
    assert_eq!(rows(), [task], "its worker still runs");
    worker.ended("exec_1");
    assert!(rows().is_empty(), "stopped, and closed for more than a day");
}

/// Audit #9 and #16: a human command is news once it finished, with its
/// outcome, and stays news (as an approval does) until a digest that
/// carried it reached the supervisor, however long that takes.
#[test]
fn mission_news_reports_human_commands_with_their_outcome_until_told() {
    let (store, schema) = store("news_told");
    let worker = Worker {
        store: &store,
        agent: AgentId::new("agent_worker_a").expect("valid agent"),
        session: SessionId::new("session_worker_a").expect("valid session"),
    };
    let supervisor = Worker {
        store: &store,
        agent: AgentId::new("agent_supervisor").expect("valid agent"),
        session: SessionId::new("session_supervisor").expect("valid session"),
    };
    let human = || store.mission_news().expect("news")["human"].clone();
    let refused = worker.input("human", "use eu", None);
    assert_eq!(human(), serde_json::json!([]), "not finished yet");
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim")
        .expect("waits");
    store
        .finish(
            &claimed,
            &Outcome::Failed("pane state Busy does not accept input".to_string()),
        )
        .expect("finish");
    let said = worker.input("human", "use us", Some(Outcome::Succeeded));
    store.resident_propose("core", "Owns it").expect("propose");
    store.resident_approve("core").expect("approve");
    let mut admin = admin(&schema);
    admin
        .batch_execute(
            "update spawn_commands set created_at_ms = created_at_ms - 2 * 86400000;
             update mission_residents set updated_at_ms = updated_at_ms - 2 * 86400000",
        )
        .expect("age everything");
    let news = human();
    assert_eq!(news[0]["command_id"].as_i64(), Some(refused));
    assert_eq!(news[0]["succeeded"], false);
    assert_eq!(news[0]["action"], "input");
    assert_eq!(news[0]["error"], "pane state Busy does not accept input");
    assert_eq!(news[1]["command_id"].as_i64(), Some(said));
    assert_eq!(news[1]["succeeded"], true);
    assert_eq!(
        store.mission_news().expect("news")["approved"],
        serde_json::json!([{"name": "core"}]),
        "an approval two days old not told yet"
    );
    // A digest that carried the refusal and the approval reached the
    // supervisor; one that failed told nothing.
    let digest = |key: &str, tags: Vec<String>, outcome: Outcome| {
        supervisor.send(
            "spawn-dispatch",
            key,
            CommandAction::Input {
                text: "digest".to_string(),
                tags,
            },
            Some(outcome),
        );
    };
    digest(
        "digest-1",
        vec![format!("session_worker_a human-{said}")],
        Outcome::Failed("pane state Busy does not accept input".to_string()),
    );
    digest(
        "digest-2",
        vec![
            format!("session_worker_a human-{refused}"),
            "session_resident_core approved-core".to_string(),
        ],
        Outcome::Succeeded,
    );
    let news = human();
    assert_eq!(news.as_array().map(Vec::len), Some(1), "{news}");
    assert_eq!(news[0]["command_id"].as_i64(), Some(said));
    assert_eq!(
        store.mission_news().expect("news")["approved"],
        serde_json::json!([])
    );
}

/// The attention view (`GET /api/attention`) reads when each told
/// escalation first reached the supervisor: the earliest successful digest
/// that filed its tag (a failed one told nothing), and nothing for a tag no
/// digest carried.
#[test]
fn the_attention_view_reads_when_each_tag_was_first_told() {
    let (store, schema) = store("told_at");
    let supervisor = Worker {
        store: &store,
        agent: AgentId::new("agent_supervisor").expect("valid agent"),
        session: SessionId::new("session_supervisor").expect("valid session"),
    };
    let digest = |key: &str, tags: &[&str], outcome: Outcome| {
        supervisor.send(
            "spawn-dispatch",
            key,
            CommandAction::Input {
                text: "digest".to_string(),
                tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
            },
            Some(outcome),
        )
    };
    digest(
        "digest-1",
        &["session_worker_a 7"],
        Outcome::Failed("pane state Busy does not accept input".to_string()),
    );
    let landed = digest(
        "digest-2",
        &["session_worker_a 7", "host disk-3"],
        Outcome::Succeeded,
    );
    let again = digest("digest-3", &["host disk-3"], Outcome::Succeeded);
    let mut admin = admin(&schema);
    admin
        .execute(
            "update spawn_commands set finished_at_ms = 1000 where id = $1",
            &[&landed],
        )
        .expect("date the first digest");
    admin
        .execute(
            "update spawn_commands set finished_at_ms = 2000 where id = $1",
            &[&again],
        )
        .expect("date the second digest");
    let tags = [
        "session_worker_a 7".to_string(),
        "host disk-3".to_string(),
        "missions mission-9".to_string(),
    ];
    assert_eq!(
        store.told_at("session_supervisor", &tags).expect("told at"),
        serde_json::json!({"session_worker_a 7": 1000, "host disk-3": 1000})
    );
    assert_eq!(
        store.told_at("session_worker_a", &tags).expect("told at"),
        serde_json::json!({}),
        "another session's digests"
    );
}

/// GitHub #16 item 9: the dispatcher's facts say where the live execution's
/// latest turn event is in the session (`turn_seq`), so a reader can tell a
/// judgement of the turn that ended from one of an earlier turn. A turn is
/// judged, a person answers, the answer's turn ends: the latest judgement
/// is then based before `turn_seq` until the Observer judges again.
#[test]
fn the_facts_place_the_latest_turn_so_an_earlier_turns_judgement_is_told_apart() {
    let (store, _schema) = store("turn_seq");
    let execution = ExecutionId::new("exec_1").expect("valid execution");
    // The supervisor's session: the facts read it and the workers'.
    let of_supervisor = |event: NewEvent| NewEvent {
        agent_id: AgentId::new("agent_supervisor").expect("valid agent"),
        session_id: SessionId::new("session_supervisor").expect("valid session"),
        ..event
    };
    let turn = |key: &str, kind: EventKind| {
        store
            .append(of_supervisor(lifecycle(
                &execution,
                Source::Claude,
                key,
                kind,
            )))
            .expect("append")
            .record
            .session_seq
            .0
    };
    let stopped = || EventKind::TurnStopped {
        input_tokens: None,
        output_tokens: None,
    };
    let row = || store.dispatch_facts().expect("facts")[0].clone();
    let based_on = |row: &serde_json::Value| row["observation"]["based_on_seq"].clone();

    store
        .append(of_supervisor(lifecycle(
            &execution,
            Source::Bridge,
            "exec_1:started",
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        )))
        .expect("an execution starts");
    assert!(row()["turn_seq"].is_null(), "no turn yet");
    turn("message-1", EventKind::MessageReceived);
    let asked = turn("turn-1", stopped());
    // The Observer judges the turn that ended.
    store
        .append(of_supervisor(observed(asked)))
        .expect("judged");
    let facts = row();
    assert_eq!(facts["turn_ended"], true);
    assert_eq!(facts["turn_seq"], asked);
    assert_eq!(
        based_on(&facts),
        asked,
        "the judgement is of this turn: {facts}"
    );

    // A person answers: a new turn runs, then ends, not judged yet.
    let answer = turn("message-2", EventKind::MessageReceived);
    let facts = row();
    assert_eq!(facts["turn_ended"], false);
    assert_eq!(facts["turn_seq"], answer);
    let ended = turn("turn-2", stopped());
    let facts = row();
    assert_eq!(facts["turn_ended"], true);
    assert_eq!(facts["turn_seq"], ended);
    assert!(
        based_on(&facts).as_u64() < Some(ended),
        "the latest judgement is the earlier turn's: {facts}"
    );
    store
        .append(of_supervisor(observed(ended)))
        .expect("judged again");
    let facts = row();
    assert_eq!(based_on(&facts), facts["turn_seq"]);
}

/// The attention view reads where the supervisor's engine writes its
/// transcript (the latest `session.native_started` that names one) and when
/// its latest turn ended, whether a turn started since or not.
#[test]
fn the_attention_view_reads_the_supervisors_transcript_and_latest_turn() {
    let (store, schema) = store("latest_turn");
    let supervisor = Worker {
        store: &store,
        agent: AgentId::new("agent_supervisor").expect("valid agent"),
        session: SessionId::new("session_supervisor").expect("valid session"),
    };
    let none = store
        .latest_turn("session_supervisor")
        .expect("latest turn");
    assert_eq!(
        none,
        serde_json::json!({"transcript": null, "workspace": null, "turn_ended_at_ms": null})
    );
    let start = supervisor.input("human", "start-1", None);
    supervisor.started("exec_1", start);
    let native = |key: &str, path: Option<&str>| {
        supervisor.event(
            "exec_1",
            key,
            EventKind::NativeSessionStarted {
                engine: ENGINE.to_string(),
                native_session_id: "native-1".to_string(),
                transcript_path: path.map(str::to_string),
                engine_version: None,
            },
        );
    };
    native(
        "native:1",
        Some("/home/owner/.claude/projects/p/native-1.jsonl"),
    );
    native("native:2", None);
    supervisor.event(
        "exec_1",
        "turn:1",
        EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
    );
    supervisor.event("exec_1", "message:2", EventKind::MessageReceived);
    let turn = store
        .latest_turn("session_supervisor")
        .expect("latest turn");
    let stopped: i64 = admin(&schema)
        .query_one(
            "select recorded_at_ms from spawn_events
              where session_id = 'session_supervisor' and event_name = 'turn.stopped'",
            &[],
        )
        .expect("the turn's end")
        .get(0);
    assert_eq!(
        turn,
        serde_json::json!({
            "transcript": "/home/owner/.claude/projects/p/native-1.jsonl",
            "workspace": "1:/workspaces/1",
            "turn_ended_at_ms": stopped,
        })
    );
}

/// Which charter the supervisor's running native session holds is read
/// from records: the start of the execution that first named the native
/// session (a resume names it again, a handoff names a new one) with the
/// charter hash that start recorded, and the dispatcher's charter inputs
/// since that start, with their outcomes. Other sessions carry none.
#[test]
#[allow(clippy::too_many_lines)]
fn dispatch_facts_name_the_charter_the_supervisors_native_session_holds() {
    let (store, _schema) = store("facts_charter");
    let supervisor = Worker {
        store: &store,
        agent: AgentId::new("agent_supervisor").expect("valid agent"),
        session: SessionId::new("session_supervisor").expect("valid session"),
    };
    let worker = Worker {
        store: &store,
        agent: AgentId::new("agent_worker_a").expect("valid agent"),
        session: SessionId::new("session_worker_a").expect("valid session"),
    };
    let start = |key: &str, charter: Option<&str>| {
        supervisor.send(
            "spawn-liveness",
            key,
            CommandAction::Start {
                engine: ENGINE.to_string(),
                prompt: "charter".to_string(),
                parent_session_id: None,
                from: None,
                model: None,
                charter: charter.map(str::to_string),
                model_label: None,
            },
            Some(Outcome::Succeeded),
        )
    };
    let native = |execution: &str, key: &str, id: &str| {
        supervisor.event(
            execution,
            key,
            EventKind::NativeSessionStarted {
                engine: ENGINE.to_string(),
                native_session_id: id.to_string(),
                transcript_path: None,
                engine_version: None,
            },
        );
    };
    let charter_input = |key: &str, hash: &str, outcome: Outcome| {
        supervisor.send(
            "spawn-dispatch",
            key,
            CommandAction::Input {
                text: "Spawn dispatcher: your charter changed".to_string(),
                tags: vec![format!("charter {hash}")],
            },
            Some(outcome),
        )
    };
    let charter = || {
        let facts = store.dispatch_facts().expect("facts");
        let rows = facts.as_array().expect("rows");
        let row = |session: &str| {
            rows.iter()
                .find(|row| row["session_id"] == session)
                .cloned()
                .expect("a row of the session")
        };
        assert!(row("session_worker_a")["charter"].is_null(), "a worker's");
        row("session_supervisor")["charter"].clone()
    };
    worker.event("exec_w", "w:message", EventKind::MessageReceived);

    // The first native session, given charter `aa`.
    let first = start("supervisor-start-first", Some("aa"));
    supervisor.started("exec_1", first);
    native("exec_1", "exec_1:native", "native-1");
    assert_eq!(
        charter(),
        serde_json::json!({"start": first, "given": "aa", "inputs": []})
    );

    // A digest (no charter tag), a person's input with a charter-like
    // tag, a charter input refused before typing, then one typed.
    supervisor.send(
        "spawn-dispatch",
        "dispatch-digest-1",
        CommandAction::Input {
            text: "Spawn dispatcher: 1 update".to_string(),
            tags: vec!["session_worker_a 7".to_string()],
        },
        Some(Outcome::Succeeded),
    );
    supervisor.send(
        "human",
        "human-1",
        CommandAction::Input {
            text: "hello".to_string(),
            tags: vec!["charter zz".to_string()],
        },
        Some(Outcome::Succeeded),
    );
    let refused = charter_input(
        "dispatch-charter-bb-over-1",
        "bb",
        Outcome::Failed("the pane does not accept input".to_string()),
    );
    let typed = charter_input(
        "dispatch-charter-bb-over-1-retry-1",
        "bb",
        Outcome::Succeeded,
    );
    // A resume of the same native session (its version record names it
    // again): the same start began it.
    let resume = supervisor.send(
        "spawn-liveness",
        "supervisor-resume-1",
        CommandAction::Resume {
            prompt: None,
            model: None,
        },
        Some(Outcome::Succeeded),
    );
    supervisor.ended("exec_1");
    supervisor.started("exec_2", resume);
    native("exec_2", "exec_2:native-version", "native-1");
    let read = charter();
    assert_eq!(read["start"], first, "{read}");
    assert_eq!(read["given"], "aa");
    let inputs = read["inputs"].as_array().expect("inputs");
    assert_eq!(
        inputs
            .iter()
            .map(|input| (input["id"].as_i64(), input["hash"].as_str()))
            .collect::<Vec<_>>(),
        vec![(Some(refused), Some("bb")), (Some(typed), Some("bb"))],
        "the dispatcher's charter inputs only: {read}"
    );
    assert_eq!(inputs[0]["succeeded"], false);
    assert_eq!(inputs[0]["error"], "the pane does not accept input");
    assert_eq!(inputs[0]["key"], "dispatch-charter-bb-over-1");
    assert_eq!(inputs[1]["succeeded"], true);
    assert!(inputs[1]["finished_at_ms"].is_i64());

    // A handoff: a new native session, given `bb` by its start; the inputs
    // typed into the one before are not its own.
    supervisor.ended("exec_2");
    let handoff = start("supervisor-start-handoff", Some("bb"));
    supervisor.started("exec_3", handoff);
    native("exec_3", "exec_3:native", "native-3");
    assert_eq!(
        charter(),
        serde_json::json!({"start": handoff, "given": "bb", "inputs": []})
    );

    // A start that recorded no charter (a person's, or one from before).
    supervisor.ended("exec_3");
    let plain = start("human-start", None);
    supervisor.started("exec_4", plain);
    native("exec_4", "exec_4:native", "native-4");
    assert_eq!(
        charter(),
        serde_json::json!({"start": plain, "given": null, "inputs": []})
    );
}

#[test]
fn dispatch_facts_carry_the_model_profile_of_the_latest_execution() {
    let (store, _schema) = store("facts_model");
    let worker = AgentId::new("agent_worker_docs_1").expect("valid agent");
    let session = SessionId::new("session_worker_docs_1").expect("valid session");
    let model = || {
        let facts = store.dispatch_facts().expect("facts");
        facts.as_array().expect("rows")[0]["model"]
            .as_str()
            .map(str::to_string)
    };
    store
        .append(NewEvent {
            agent_id: worker.clone(),
            session_id: session.clone(),
            ..message(EventKind::MessageReceived)
        })
        .expect("append succeeds");
    assert_eq!(model().as_deref(), Some("default"), "never ran");
    for (number, profile) in [(1, ModelProfile::Default), (2, ModelProfile::Cheap)] {
        let execution = ExecutionId::new(format!("exec_{number}")).expect("valid execution");
        store
            .append(NewEvent {
                agent_id: worker.clone(),
                session_id: session.clone(),
                ..lifecycle(
                    &execution,
                    Source::Bridge,
                    &format!("exec_{number}:started"),
                    EventKind::ExecutionStarted {
                        engine: ENGINE.to_string(),
                        workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                        command_id: CommandId(number),
                        fork: None,
                        model: profile,
                        transcript_from: None,
                        launch: None,
                    },
                )
            })
            .expect("append succeeds");
        assert_eq!(model().as_deref(), Some(profile.as_str()));
        // One execution at a time: this one ends before the next starts.
        store
            .append(NewEvent {
                agent_id: worker.clone(),
                session_id: session.clone(),
                ..lifecycle(
                    &execution,
                    Source::Bridge,
                    &format!("exec_{number}:ended"),
                    EventKind::ExecutionEnded {
                        cause: EndCause::Stopped,
                        exit_code: None,
                        signal: None,
                        evidence: None,
                    },
                )
            })
            .expect("append succeeds");
    }
}

#[test]
fn the_restricted_logins_have_derived_passwords_and_write_no_events() {
    let (_store, schema) = store("logins");
    for base in common::LOGIN_BASES {
        let role = common::login(&schema, base);
        assert_ne!(role, base, "the login is this database's own");
        let fixed = format!(
            "postgres://{role}:{role}@{}",
            common::url().rsplit_once('@').expect("a host").1
        );
        assert!(
            postgres::Client::connect(&fixed, postgres::NoTls).is_err(),
            "{role} has no fixed password"
        );
        let url = spawn_postgres::role_url(&common::schema_url(&schema), base)
            .expect("a password in the test URL");
        assert!(
            url.contains(&format!("://{role}:")),
            "{base} logs in as {role}"
        );
        let mut client =
            postgres::Client::connect(&url, postgres::NoTls).expect("the derived password logs in");
        client
            .batch_execute(&format!("set search_path to {schema}"))
            .expect("schema");
        let error = client
            .query(
                "select * from spawn_append('agent_x', 'session_x', null, 'claude', null, \
                 '{\"name\":\"tool.started\",\"detail\":{\"tool_name\":\"Bash\",\"tool_use_id\":\"t\"}}'::jsonb)",
                &[],
            )
            .expect_err("a restricted login writes no events");
        assert_eq!(
            error.code().map(|code| code.code().to_string()).as_deref(),
            Some("42501")
        );
    }
}

/// The same server spelled another way: the authority's host and port
/// moved to the `host` and `port` parameters. Any host works: a socket
/// directory, 127.0.0.1, `localhost`, `[::1]` (the brackets dropped), a
/// name.
fn respelled(url: &str) -> String {
    let (head, tail) = url.rsplit_once('@').expect("a host");
    let (authority, path) = tail.split_once('/').expect("a database");
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && (!host.starts_with('[') || host.ends_with(']')) =>
        {
            (host, format!("&port={port}"))
        }
        _ => (authority, String::new()),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{head}@/{path}{separator}host={host}{port}")
}

#[test]
fn respelling_moves_any_host_to_the_parameters() {
    for (url, expected) in [
        (
            "postgres://u:p@%2Frun%2Fpg/db",
            "postgres://u:p@/db?host=%2Frun%2Fpg",
        ),
        (
            "postgres://u:p@127.0.0.1:15434/db",
            "postgres://u:p@/db?host=127.0.0.1&port=15434",
        ),
        (
            "postgres://u:p@localhost/db?sslmode=disable",
            "postgres://u:p@/db?sslmode=disable&host=localhost",
        ),
        (
            "postgres://u:p@[::1]:5432/db",
            "postgres://u:p@/db?host=::1&port=5432",
        ),
    ] {
        assert_eq!(respelled(url), expected);
        assert_ne!(respelled(url), url);
    }
}

#[test]
fn a_cluster_is_known_by_its_identity_not_its_spelling() {
    let _budget = ApiBudgetGuard::shared();
    let url = common::url();
    let identity = spawn_postgres::cluster_identity(&url).expect("identity");
    assert!(!identity.is_empty());
    let other = respelled(&url);
    assert_ne!(other, url);
    assert_eq!(
        spawn_postgres::cluster_identity(&other).expect("identity through another spelling"),
        identity
    );
    assert!(
        spawn_postgres::cluster_identity("postgres://nobody:x@%2Fnonexistent%2Fdir/db").is_err(),
        "an unreachable server has no identity"
    );
}

/// The rule that drops a test database's logins (roles belong to the whole
/// server, so dropping the database leaves them), shared with every
/// scenario's cleanup: scripts/blackbox/drop-logins.sql.
const DROP_LOGINS: &str = include_str!("../../../scripts/blackbox/drop-logins.sql");

/// Run [`DROP_LOGINS`] on a connection to the server's `postgres`
/// database: the logins of `database` (after it was dropped), or, with
/// `database` empty, a sweep of the logins of every database whose name
/// starts with `prefix` and no longer exists. Its result: `dropped <n> of
/// <m>` or `skipped: <why>`.
fn drop_logins(maintenance: &mut Client, database: &str, prefix: &str) -> String {
    maintenance
        .execute(
            "select set_config('spawn_logins.database', $1, false),
                    set_config('spawn_logins.prefix', $2, false)",
            &[&database, &prefix],
        )
        .and_then(|_| maintenance.batch_execute(DROP_LOGINS))
        .and_then(|()| {
            maintenance.query_one(
                "select coalesce(current_setting('spawn_logins.result', true), '')",
                &[],
            )
        })
        .map_or_else(|error| format!("failed: {error}"), |row| row.get(0))
}

/// Databases of one case, each dropped with the logins its migration made
/// when the guard goes: after the case, a failed assertion and a panicked
/// thread alike.
struct Databases {
    /// The server's `postgres` database, where the drops run.
    maintenance: String,
    names: Vec<String>,
    /// Other roles the case made, dropped with what they hold.
    roles: Vec<String>,
}

impl Drop for Databases {
    fn drop(&mut self) {
        let Ok(mut maintenance) = Client::connect(&self.maintenance, NoTls) else {
            return;
        };
        for name in &self.names {
            let _ =
                maintenance.batch_execute(&format!("drop database if exists {name} with (force)"));
            let _ = drop_logins(&mut maintenance, name, "");
            // drop-logins.sql knows the first three logins; the node
            // gateway's (spawn-node v1) is dropped here by its name, as
            // spawn_login_role gives it (the long-name hash included).
            let _ = maintenance
                .query_one(GATEWAY_LOGIN_OF, &[&"spawn_node_gateway", name])
                .map(|row| row.get::<_, String>(0))
                .and_then(|role| maintenance.batch_execute(&format!("drop role if exists {role}")));
        }
        for role in &self.roles {
            // Apart: a login that is not a superuser may not `drop owned`
            // of a role it only administers; what the role held went with
            // the databases above.
            let _ = maintenance.batch_execute(&format!("drop owned by {role}"));
            let _ = maintenance.batch_execute(&format!("drop role if exists {role}"));
        }
    }
}

/// `spawn_login_role($1)` for the database `$2`, computed where the
/// function is not (the server's `postgres` database).
const GATEWAY_LOGIN_OF: &str =
    "select case when $2 ~ '^[a-z0-9_]+$' and length($1) + 1 + length($2) <= 63
                 then $1 || '_' || $2
                 else left($1 || '_' || regexp_replace(lower($2), '[^a-z0-9_]+', '_', 'g'), 54)
                      || '_' || left(md5($2), 8) end";

/// A sweep of the logins of every database under `prefix` that no longer
/// exists.
fn sweep(maintenance: &mut Client, prefix: &str) -> String {
    drop_logins(maintenance, "", prefix)
}

#[test]
fn a_sweep_drops_only_the_logins_of_gone_databases_under_its_prefix() {
    let _budget = ApiBudgetGuard::exclusive();
    let base = url();
    let (server, _) = base.rsplit_once('/').expect("a database URL");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let prefix = format!("spawn_sweep_{}_{nanos:x}_", std::process::id());
    let mut databases = Databases {
        maintenance: format!("{server}/postgres"),
        names: Vec::new(),
        roles: Vec::new(),
    };
    let mut maintenance =
        Client::connect(&databases.maintenance, NoTls).expect("maintenance connection");
    // Kept: an existing database; gone: a dropped one, and one whose name is
    // long enough for spawn_login_role to hash all three logins' (60 of the
    // 63 characters a name may have); outside: another prefix. Migrated
    // (the logins as init-db names them), or, where the name is the plain
    // one, given its logins as a migration leaves them: a role that may
    // connect to its database (the passwords' derivation is slow).
    let kept = format!("{prefix}kept");
    let gone = format!("{prefix}gone");
    let long = format!("{prefix}{}", "l".repeat(60 - prefix.len()));
    let outside = format!("spawn_sweeq_{}_{nanos:x}_gone", std::process::id());
    for name in [&kept, &gone, &long, &outside] {
        maintenance
            .batch_execute(&format!("create database {name}"))
            .expect("create a database");
        databases.names.push(name.clone());
    }
    for name in [&kept, &long] {
        spawn_postgres::PostgresStore::connect(&format!("{server}/{name}"))
            .and_then(|store| store.migrate())
            .expect("migrate");
    }
    for name in [&gone, &outside] {
        for base in ["spawn_supervisor", "spawn_reader", "spawn_human"] {
            maintenance
                .batch_execute(&format!(
                    "create role {base}_{name} nologin; grant connect on database {name} to {base}_{name}"
                ))
                .expect("a login of the database");
        }
    }
    for name in [&gone, &long, &outside] {
        maintenance
            .batch_execute(&format!("drop database {name} with (force)"))
            .expect("drop a database, not its logins");
    }
    // A login-shaped role of no database that still holds a privilege: on
    // a database this test made (a login that is not a superuser grants
    // nothing on `postgres`, which it does not own).
    let holding = format!("spawn_reader_{prefix}holding");
    databases.roles.push(holding.clone());
    maintenance
        .batch_execute(&format!(
            "create role {holding} nologin; grant connect on database {kept} to {holding}"
        ))
        .expect("a role that holds a privilege");
    let logins = |client: &mut Client, pattern: &str| -> Vec<String> {
        client
            .query(
                "select rolname::text from pg_roles
                  where rolname ~ ('^spawn_(supervisor|reader|human)_' || $1)
                  order by rolname collate \"C\"",
                &[&pattern],
            )
            .expect("the logins")
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    let hashed: i64 = maintenance
        .query_one(
            "select count(*) from pg_roles
              where rolname ~ ('^spawn_(supervisor|reader|human)_' || $2 || '.*_' || left(md5($1), 8) || '$')
                and length(rolname) = 63",
            &[&long, &prefix],
        )
        .expect("the hashed logins")
        .get(0);
    assert_eq!(hashed, 3, "the long name's logins are hashed");
    assert_eq!(logins(&mut maintenance, &format!("{prefix}.*")).len(), 10);

    // One database's logins, while it still exists: none go.
    assert_eq!(drop_logins(&mut maintenance, &kept, ""), "dropped 0 of 4");
    assert_eq!(sweep(&mut maintenance, &prefix), "dropped 7 of 12");
    let left = logins(&mut maintenance, &format!("{prefix}.*"));
    assert_eq!(
        left,
        [
            format!("spawn_human_{kept}"),
            holding.clone(),
            format!("spawn_reader_{kept}"),
            format!("spawn_supervisor_{kept}"),
        ]
    );
    assert_eq!(
        logins(&mut maintenance, &outside).len(),
        3,
        "another prefix's logins stay"
    );
    assert_eq!(
        drop_logins(&mut maintenance, &outside, ""),
        "dropped 3 of 3"
    );
}

/// libpq's variables for the test server, from its URL, as a scenario hands
/// the server to psql (scripts/pg-url.sh pg_url_export): never a URL on a
/// command line.
fn libpq_env(command: &mut std::process::Command) {
    use postgres::config::Host;
    let config: postgres::Config = url().parse().expect("a database URL");
    for name in [
        "PGHOST",
        "PGHOSTADDR",
        "PGPORT",
        "PGUSER",
        "PGPASSWORD",
        "PGDATABASE",
        "PGSERVICE",
        "PGOPTIONS",
        "SPAWN_PSQL",
    ] {
        command.env_remove(name);
    }
    match config.get_hosts().first() {
        Some(Host::Tcp(host)) => {
            command.env("PGHOST", host);
        }
        Some(Host::Unix(directory)) => {
            command.env("PGHOST", directory);
        }
        None => {}
    }
    if let Some(port) = config.get_ports().first() {
        command.env("PGPORT", port.to_string());
    }
    if let Some(user) = config.get_user() {
        command.env("PGUSER", user);
    }
    if let Some(password) = config.get_password() {
        command.env("PGPASSWORD", String::from_utf8_lossy(password).as_ref());
    }
}

/// `sh scripts/blackbox/scenario-db.sh sweep <prefix>` on the test server,
/// with SPAWN_SCENARIO_DB_SWEEP_MINUTES `minutes` (None: unset, its
/// default): whether it exited 0, and what it said on stdout and stderr.
fn scenario_db_sweep(prefix: &str, minutes: Option<&str>) -> (bool, String) {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/blackbox/scenario-db.sh");
    let mut command = std::process::Command::new("sh");
    command.arg(&script).args(["sweep", prefix]);
    libpq_env(&mut command);
    match minutes {
        Some(minutes) => command.env("SPAWN_SCENARIO_DB_SWEEP_MINUTES", minutes),
        None => command.env_remove("SPAWN_SCENARIO_DB_SWEEP_MINUTES"),
    };
    let output = command.output().expect("run scenario-db.sh");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// Wait (30 s at most) until `connected` sessions are open on `database`.
fn wait_for_sessions(maintenance: &mut Client, database: &str, connected: bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let open: bool = maintenance
            .query_one(
                "select exists (select 1 from pg_stat_activity where datname = $1)",
                &[&database],
            )
            .expect("the sessions")
            .get(0);
        if open == connected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sessions open on {database} after 30 s: {open}, waiting for {connected}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A psql session on `database`, killed with SIGKILL (std's kill) once the
/// server has it: a run killed with kill -9, whose cleanup never runs.
/// Returns once the server has ended the session.
fn kill_a_session_on(maintenance: &mut Client, database: &str) {
    let mut session = std::process::Command::new("psql");
    session
        .args(["-X", "-q", "-d", database])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    libpq_env(&mut session);
    let mut session = session.spawn().expect("a psql session (psql on PATH)");
    wait_for_sessions(maintenance, database, true);
    session.kill().expect("kill -9 the session");
    let _ = session.wait();
    wait_for_sessions(maintenance, database, false);
}

fn database_exists(client: &mut Client, name: &str) -> bool {
    client
        .query_one(
            "select exists (select 1 from pg_database where datname = $1)",
            &[&name],
        )
        .expect("the databases")
        .get(0)
}

/// How many of `name`'s three logins the server has.
fn logins_of(client: &mut Client, name: &str) -> i64 {
    client
        .query_one(
            "select count(*) from pg_roles
              where rolname in ('spawn_supervisor_' || $1, 'spawn_reader_' || $1,
                                'spawn_human_' || $1)",
            &[&name],
        )
        .expect("the logins")
        .get(0)
}

/// The start-of-run sweep (scripts/blackbox/scenario-db.sh) drops a
/// scenario's leftover databases, but only once they are older than
/// SPAWN_SCENARIO_DB_SWEEP_MINUTES: "nothing is connected" alone also holds
/// for a live run between two connections, which a parallel copy's sweep
/// took (a live 007 lost its database and failed: review of task 154).
#[test]
fn a_sweep_drops_a_leftover_database_only_once_it_is_older_than_its_setting() {
    let _budget = ApiBudgetGuard::exclusive();
    let base = url();
    let (server, _) = base.rsplit_once('/').expect("a database URL");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let prefix = format!("spawn_dbsweep_{}_{nanos:x}_", std::process::id());
    let mut databases = Databases {
        maintenance: format!("{server}/postgres"),
        names: Vec::new(),
        roles: Vec::new(),
    };
    let mut maintenance =
        Client::connect(&databases.maintenance, NoTls).expect("maintenance connection");
    // Killed: a run killed with kill -9 before its cleanup (its database,
    // its logins as a migration leaves them, its session gone with it).
    // Quiet: a live run between two connections. Busy: a live run with a
    // session open.
    let killed = format!("{prefix}killed");
    let quiet = format!("{prefix}quiet");
    let busy = format!("{prefix}busy");
    for name in [&killed, &quiet, &busy] {
        maintenance
            .batch_execute(&format!("create database {name}"))
            .expect("create a database");
        databases.names.push(name.clone());
    }
    for login in ["spawn_supervisor", "spawn_reader", "spawn_human"] {
        maintenance
            .batch_execute(&format!(
                "create role {login}_{killed} nologin; grant connect on database {killed} to {login}_{killed}"
            ))
            .expect("a login of the database");
    }
    kill_a_session_on(&mut maintenance, &killed);
    let _busy_session =
        Client::connect(&format!("{server}/{busy}"), NoTls).expect("a session on busy");

    // Seconds old: the default (180 minutes) and any setting above their age
    // keep every one, connected or not.
    for minutes in [None, Some("180"), Some("5")] {
        let (ran, said) = scenario_db_sweep(&prefix, minutes);
        assert!(ran, "the sweep with {minutes:?}: {said}");
        for name in [&killed, &quiet, &busy] {
            assert!(
                database_exists(&mut maintenance, name),
                "{name} after a sweep with {minutes:?} minutes: {said}"
            );
        }
        assert_eq!(
            logins_of(&mut maintenance, &killed),
            3,
            "a database that is still there keeps its logins ({minutes:?}): {said}"
        );
    }
    // A setting that is not a whole number of minutes drops no database.
    for minutes in ["ten", "-1", "1.5"] {
        let (ran, said) = scenario_db_sweep(&prefix, Some(minutes));
        assert!(ran, "the sweep with {minutes}: {said}");
        assert!(
            said.contains("no database is swept"),
            "the sweep with {minutes}: {said}"
        );
        for name in [&killed, &quiet, &busy] {
            assert!(
                database_exists(&mut maintenance, name),
                "{name} after a sweep with {minutes}: {said}"
            );
        }
    }
    // 0: every leftover nothing is connected to goes, then the killed run's
    // logins; the busy one stays.
    let (ran, said) = scenario_db_sweep(&prefix, Some("0"));
    assert!(ran, "the sweep with 0: {said}");
    assert!(
        !database_exists(&mut maintenance, &killed),
        "killed: {said}"
    );
    assert!(!database_exists(&mut maintenance, &quiet), "quiet: {said}");
    assert!(database_exists(&mut maintenance, &busy), "busy: {said}");
    assert_eq!(logins_of(&mut maintenance, &killed), 0, "{said}");
    assert!(
        said.contains(&format!("sweep {prefix}: dropped 3 of 3")),
        "{said}"
    );
}

#[test]
fn migrations_racing_on_one_server_all_succeed() {
    let _budget = ApiBudgetGuard::exclusive();
    // Roles and grants are shared by the whole server: parallel migrations of
    // separate databases used to fail with "tuple concurrently updated".
    let base = url();
    let (server, _) = base.rsplit_once('/').expect("a database URL");
    let maintenance = format!("{server}/postgres");
    // What a run of this case killed before its guard dropped them left.
    let swept = drop_logins(
        &mut Client::connect(&maintenance, NoTls).expect("maintenance connection"),
        "",
        "spawn_race_",
    );
    assert!(
        swept.starts_with("dropped ") || swept.starts_with("skipped: "),
        "the sweep of earlier runs' logins: {swept}"
    );
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let mut databases = Databases {
        maintenance,
        names: Vec::new(),
        roles: Vec::new(),
    };
    let mut admin = Client::connect(&base, NoTls).expect("admin connection");
    for index in 0..4 {
        let name = format!("spawn_race_{}_{nanos:x}_{index}", std::process::id());
        admin
            .batch_execute(&format!("create database {name}"))
            .expect("create a database");
        databases.names.push(name);
    }
    let workers: Vec<_> = databases
        .names
        .iter()
        .map(|name| {
            let url = format!("{server}/{name}");
            std::thread::spawn(move || {
                spawn_postgres::PostgresStore::connect(&url)
                    .and_then(|store| store.migrate())
                    .map_err(|error| error.to_string())
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().expect("a migration thread"))
        .collect();
    for result in results {
        result.expect("every racing migration succeeds");
    }
    // The guard drops the databases and their logins.
    let names = databases.names.clone();
    drop(databases);
    let left: i64 = admin
        .query_one(
            "select count(*) from pg_roles r, unnest($1::text[]) d, unnest($2::text[]) b
              where r.rolname = b || '_' || d",
            &[&names, &common::LOGIN_BASES.as_slice()],
        )
        .expect("the logins left")
        .get(0);
    assert_eq!(
        left, 0,
        "the racing databases' logins are dropped with them"
    );
}

/// M15: a revival whose engine dies right after it launched counts as a
/// failure though its command succeeded, so the backoff and the handoff
/// after three failed resumes engage; a revival that stayed up past the
/// grace minute starts the count over.
#[test]
fn m15_a_revival_that_dies_at_once_counts_as_a_failure() {
    let (store, schema) = store("revival_storm");
    let reviver = "spawn-supervise";
    let run = |number: u64, command: CommandId| {
        let execution = ExecutionId::new(format!("exec_{number}")).expect("valid execution");
        store
            .append(lifecycle(
                &execution,
                Source::Bridge,
                &format!("exec_{number}:started"),
                EventKind::ExecutionStarted {
                    engine: ENGINE.to_string(),
                    workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                    command_id: command,
                    fork: None,
                    model: ModelProfile::Default,
                    transcript_from: None,
                    launch: None,
                },
            ))
            .expect("started");
        store
            .append(lifecycle(
                &execution,
                Source::Bridge,
                &format!("exec_{number}:ended"),
                EventKind::ExecutionEnded {
                    cause: EndCause::Lost,
                    exit_code: None,
                    signal: None,
                    evidence: None,
                },
            ))
            .expect("ended");
    };
    let revive = |key: &str| {
        let command = store
            .submit(
                &Principal::new(reviver).expect("valid principal"),
                &Submission {
                    action: CommandAction::Resume {
                        prompt: None,
                        model: None,
                    },
                    ..start(session(), key)
                },
            )
            .expect("submit");
        let claimed = store
            .claim("bridge", Duration::from_mins(1))
            .expect("claim")
            .expect("the resume");
        store
            .finish(&claimed, &Outcome::Succeeded)
            .expect("the engine launched");
        command
    };
    let first = store
        .submit(
            &Principal::new("human").expect("valid"),
            &start(session(), "first"),
        )
        .expect("submit");
    let claimed = store
        .claim("bridge", Duration::from_mins(1))
        .expect("claim")
        .expect("start");
    store.finish(&claimed, &Outcome::Succeeded).expect("finish");
    run(1, first);
    // Three revivals, each lost the moment it launched.
    for attempt in 2..=4 {
        let command = revive(&format!("revive-{attempt}"));
        run(attempt, command);
    }
    let facts = store.liveness_facts(&session(), reviver).expect("facts");
    assert_eq!(
        facts.failures, 3,
        "three revivals died at once; counted {}",
        facts.failures
    );
    assert!(facts.last_failure_ms.is_some(), "the backoff has a start");
    // A fourth revival whose engine ran two minutes before it was lost.
    let command = revive("revive-5");
    run(5, command);
    admin(&schema)
        .execute(
            "update spawn_events set recorded_at_ms = recorded_at_ms - 120000
              where execution_id is distinct from 'exec_5' or event_name <> 'execution.ended'",
            &[],
        )
        .expect("age all but the last end");
    let facts = store.liveness_facts(&session(), reviver).expect("facts");
    assert_eq!(
        facts.failures, 0,
        "an engine that stayed up resets the count"
    );
}

/// Objective 032: a revival that made progress before a fault took it is no
/// failed revival, though it ended lost within the grace minute — its
/// command succeeded and its engine worked. The liveness rule counts none
/// of them, so it revives at once instead of backing off; revivals that
/// never got to work are still failures.
/// An execution's start on the default profile, from `command`.
fn started_by(command: CommandId) -> EventKind {
    EventKind::ExecutionStarted {
        engine: ENGINE.to_string(),
        workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
        command_id: command,
        fork: None,
        model: ModelProfile::Default,
        transcript_from: None,
        launch: None,
    }
}

#[test]
fn a_revival_that_worked_before_it_was_lost_is_not_a_failure() {
    let (store, _schema) = store("revival_progress");
    let reviver = "spawn-supervise";
    let run = |number: u64, command: CommandId, worked: bool| {
        let execution = ExecutionId::new(format!("exec_{number}")).expect("valid execution");
        store
            .append(lifecycle(
                &execution,
                Source::Bridge,
                &format!("exec_{number}:started"),
                started_by(command),
            ))
            .expect("started");
        if worked {
            store
                .append(lifecycle(
                    &execution,
                    Source::Bridge,
                    &format!("exec_{number}:tool"),
                    EventKind::ToolCompleted {
                        tool_use_id: format!("toolu_{number}"),
                        duration_ms: Some(5),
                    },
                ))
                .expect("progress");
        }
        store
            .append(lifecycle(
                &execution,
                Source::Bridge,
                &format!("exec_{number}:ended"),
                EventKind::ExecutionEnded {
                    cause: EndCause::Lost,
                    exit_code: None,
                    signal: None,
                    evidence: None,
                },
            ))
            .expect("ended");
    };
    let revive = |key: &str| {
        let command = store
            .submit(
                &Principal::new(reviver).expect("valid principal"),
                &Submission {
                    action: CommandAction::Resume {
                        prompt: None,
                        model: None,
                    },
                    ..start(session(), key)
                },
            )
            .expect("submit");
        let claimed = store
            .claim("bridge", Duration::from_mins(1))
            .expect("claim")
            .expect("the resume");
        store
            .finish(&claimed, &Outcome::Succeeded)
            .expect("the engine launched");
        command
    };
    let first = store
        .submit(
            &Principal::new("human").expect("valid"),
            &start(session(), "first"),
        )
        .expect("submit");
    let claimed = store
        .claim("bridge", Duration::from_mins(1))
        .expect("claim")
        .expect("start");
    store.finish(&claimed, &Outcome::Succeeded).expect("finish");
    run(1, first, false);
    // Three revivals that each worked a unit, then were lost: none failed.
    for attempt in 2..=4 {
        let command = revive(&format!("revive-{attempt}"));
        run(attempt, command, true);
    }
    let facts = store.liveness_facts(&session(), reviver).expect("facts");
    assert_eq!(
        facts.failures, 0,
        "a revival that worked is no failed revival; counted {}",
        facts.failures
    );
    // Revivals after it that never got to work are failures.
    for attempt in 5..=6 {
        let command = revive(&format!("revive-{attempt}"));
        run(attempt, command, false);
    }
    let facts = store.liveness_facts(&session(), reviver).expect("facts");
    assert_eq!(
        facts.failures, 2,
        "revivals that never worked are failures; counted {}",
        facts.failures
    );
}

/// Objective 032: a database restart breaks the store's connection. The
/// pass that meets it says so (`Unreachable`: the loop paces it and never
/// ends on it), and the loop's next pass opens a store of its own, which
/// reads the log again — the loop does not keep a dead connection.
#[test]
fn a_restarted_databases_broken_connection_is_unreachable_and_a_fresh_store_reads() {
    let (_, schema) = store("restart_reopens");
    let name = format!("spawn_test_restart_{}", std::process::id());
    let url = url();
    let separator = if url.contains('?') { '&' } else { '?' };
    let named = format!("{url}{separator}application_name={name}");
    let store = PostgresStore::connect_in_schema(&named, &schema).expect("connect");
    assert!(
        store.watermark().is_ok(),
        "the connection works before the restart"
    );
    // The restart, as the database's own end of the connection: this
    // store's backend is terminated, and only it (the name is this test's).
    let ended = admin(&schema)
        .execute(
            "select pg_terminate_backend(pid) from pg_stat_activity
              where application_name = $1 and pid <> pg_backend_pid()",
            &[&name],
        )
        .expect("terminate the store's backend");
    assert_eq!(ended, 1, "one backend of this test's");
    let error = store
        .watermark()
        .expect_err("a closed connection says the database is unreachable");
    assert!(
        spawn_core::port::is_unreachable(&error.to_string()),
        "the loop paces an unreachable pass instead of ending: {error}"
    );
    // The loop's next pass: a store of its own, on a live connection.
    let fresh = PostgresStore::connect_in_schema(&named, &schema).expect("reconnect");
    assert!(
        fresh.watermark().is_ok(),
        "a fresh store reads the log again after the restart"
    );
}

/// The dispatcher's facts count a resume whose engine was lost again within
/// the grace minute (`resumed_young`), until an execution stays up.
#[test]
fn dispatch_facts_count_resumes_lost_again_at_once() {
    let (store, schema) = store("facts_resumed_young");
    let worker = AgentId::new("agent_worker_a").expect("valid agent");
    let session = SessionId::new("session_worker_a").expect("valid session");
    let run = |number: u64, command: CommandId| {
        let execution = ExecutionId::new(format!("exec_{number}")).expect("valid execution");
        for (key, kind) in [
            (
                "started",
                EventKind::ExecutionStarted {
                    engine: ENGINE.to_string(),
                    workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                    command_id: command,
                    fork: None,
                    model: ModelProfile::Default,
                    transcript_from: None,
                    launch: None,
                },
            ),
            (
                "ended",
                EventKind::ExecutionEnded {
                    cause: EndCause::Lost,
                    exit_code: None,
                    signal: None,
                    evidence: None,
                },
            ),
        ] {
            store
                .append(NewEvent {
                    agent_id: worker.clone(),
                    session_id: session.clone(),
                    ..lifecycle(
                        &execution,
                        Source::Bridge,
                        &format!("exec_{number}:{key}"),
                        kind,
                    )
                })
                .expect("append");
        }
    };
    let send = |principal: &str, key: &str| {
        let command = store
            .submit(
                &Principal::new(principal).expect("valid principal"),
                &Submission {
                    agent_id: worker.clone(),
                    action: CommandAction::Resume {
                        prompt: None,
                        model: None,
                    },
                    ..start(session.clone(), key)
                },
            )
            .expect("submit");
        let claimed = store
            .claim("bridge", Duration::from_mins(1))
            .expect("claim")
            .expect("the command");
        store
            .finish(&claimed, &Outcome::Succeeded)
            .expect("the engine launched");
        command
    };
    let row = || store.dispatch_facts().expect("facts")[0].clone();
    run(1, send("human", "first"));
    assert_eq!(
        row()["resumed_young"],
        serde_json::json!(0),
        "no resume yet"
    );
    run(2, send("spawn-dispatch", "dispatch-resume-exec_1"));
    assert_eq!(
        row()["resumed_young"],
        serde_json::json!(1),
        "the resumed engine was lost at once"
    );
    // A later execution that ran two minutes before it was lost.
    run(3, send("human", "again"));
    admin(&schema)
        .execute(
            "update spawn_events set recorded_at_ms = recorded_at_ms - 120000
              where execution_id is distinct from 'exec_3' or event_name <> 'execution.ended'",
            &[],
        )
        .expect("age all but the last end");
    assert_eq!(
        row()["resumed_young"],
        serde_json::json!(0),
        "an engine that stayed up starts the count over"
    );
}

/// The dispatcher's facts name its latest resume whose execution ended lost
/// before its engine became ready (`failed_resume`), whatever the command's
/// outcome and however long the execution took to end, with the evidence;
/// a resume whose engine became ready (by its ready wait's record or by a
/// later reading) is not one, nor is anyone else's resume; an execution
/// that stayed up starts over (task 132).
#[test]
#[allow(clippy::too_many_lines)]
fn dispatch_facts_name_the_resume_lost_before_its_engine_became_ready() {
    /// How an execution's engine became ready, if it did.
    #[derive(Clone, Copy)]
    enum Ready {
        Never,
        /// The ready wait's own record, before the prompt was typed.
        Waited,
        /// A later reading of the Bridge's (a resume with no prompt).
        Read,
    }
    let (store, schema) = store("facts_failed_resume");
    let worker = AgentId::new("agent_worker_a").expect("valid agent");
    let session = SessionId::new("session_worker_a").expect("valid session");
    let append = |execution: &ExecutionId, key: Option<String>, kind: EventKind| {
        store
            .append(NewEvent {
                agent_id: worker.clone(),
                session_id: session.clone(),
                execution_id: Some(execution.clone()),
                source: Source::Bridge,
                source_event_id: key,
                kind,
            })
            .expect("append");
    };
    let ready = || EventKind::ScreenObserved {
        state: spawn_core::screen::ScreenState::Ready,
        evidence: "❯".to_string(),
    };
    let begin = |number: u64, command: CommandId, became: Ready| {
        let execution = ExecutionId::new(format!("exec_{number}")).expect("valid execution");
        append(
            &execution,
            Some(format!("exec_{number}:started")),
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                command_id: command,
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        );
        match became {
            Ready::Never => {}
            Ready::Waited => append(
                &execution,
                Some(format!("exec_{number}:screen:ready")),
                ready(),
            ),
            Ready::Read => append(&execution, None, ready()),
        }
    };
    let lose = |number: u64, evidence: &str| {
        let execution = ExecutionId::new(format!("exec_{number}")).expect("valid execution");
        append(
            &execution,
            Some(format!("exec_{number}:ended")),
            EventKind::ExecutionEnded {
                cause: EndCause::Lost,
                exit_code: None,
                signal: None,
                evidence: Some(evidence.to_string()),
            },
        );
    };
    let age = |ms: i64| {
        admin(&schema)
            .execute(
                "update spawn_events set recorded_at_ms = recorded_at_ms - $1",
                &[&ms],
            )
            .expect("age the log");
    };
    let send = |principal: &str, key: &str, outcome: Outcome| {
        let command = store
            .submit(
                &Principal::new(principal).expect("valid principal"),
                &Submission {
                    agent_id: worker.clone(),
                    action: CommandAction::Resume {
                        prompt: Some("go on".to_string()),
                        model: None,
                    },
                    ..start(session.clone(), key)
                },
            )
            .expect("submit");
        let claimed = store
            .claim("bridge", Duration::from_mins(1))
            .expect("claim")
            .expect("the command");
        (command, claimed, outcome)
    };
    let finish = |(_, claimed, outcome): &(CommandId, spawn_core::command::Claimed, Outcome)| {
        store.finish(claimed, outcome).expect("finish");
    };
    let row = || store.dispatch_facts().expect("facts")[0].clone();
    let never_ready = || Outcome::Failed("pane never became ready, last state Unknown".to_string());
    let abandoned = "its start failed: pane never became ready, last state Unknown";

    // A task that ran half an hour, then was lost.
    let first = send("human", "first", Outcome::Succeeded);
    begin(1, first.0, Ready::Waited);
    finish(&first);
    age(1_800_000);
    lose(1, "the engine's terminal is gone");
    assert_eq!(
        row()["failed_resume"],
        serde_json::Value::Null,
        "no resume yet"
    );

    // The dispatcher's resume: started, its pane never ready, abandoned.
    let resume = send("spawn-dispatch", "dispatch-resume-exec_1", never_ready());
    begin(2, resume.0, Ready::Never);
    lose(2, abandoned);
    finish(&resume);
    let failed = row()["failed_resume"].clone();
    assert_eq!(failed["execution_id"], "exec_2", "{failed}");
    assert_eq!(failed["command_id"], serde_json::json!(resume.0.0));
    assert_eq!(failed["evidence"], abandoned);
    assert_eq!(
        failed["error"],
        "pane never became ready, last state Unknown"
    );
    assert_eq!(
        row()["resumed_young"],
        serde_json::json!(0),
        "its command failed"
    );

    // A person's resume ends the same way: the dispatcher's is still the one.
    let again = send("human", "again", never_ready());
    begin(3, again.0, Ready::Never);
    lose(3, abandoned);
    finish(&again);
    assert_eq!(row()["failed_resume"]["execution_id"], "exec_2");

    // One whose engine became ready and stayed up two minutes starts over.
    let fixed = send("human", "fixed", Outcome::Succeeded);
    begin(4, fixed.0, Ready::Waited);
    finish(&fixed);
    age(120_000);
    lose(4, "the engine's terminal is gone");
    assert_eq!(
        row()["failed_resume"],
        serde_json::Value::Null,
        "an engine that stayed up starts over"
    );

    // A resume whose engine became ready, read by the Bridge's follow, and
    // was lost at once: the young-death count's, not a failed resume.
    let resume = send(
        "spawn-dispatch",
        "dispatch-resume-exec_4",
        Outcome::Succeeded,
    );
    begin(5, resume.0, Ready::Read);
    finish(&resume);
    lose(5, "the engine's terminal is gone");
    assert_eq!(
        row()["failed_resume"],
        serde_json::Value::Null,
        "it became ready"
    );
    assert_eq!(row()["resumed_young"], serde_json::json!(1));

    // A resume lost before ready whose end took two minutes still counts.
    let slow = send("spawn-dispatch", "dispatch-resume-exec_5", never_ready());
    begin(6, slow.0, Ready::Never);
    age(120_000);
    lose(6, abandoned);
    finish(&slow);
    assert_eq!(row()["failed_resume"]["execution_id"], "exec_6");
}

#[test]
fn the_reader_reads_the_whole_log_from_a_time_without_screen_text() {
    let (store, schema) = store("log_after");
    for kind in [
        EventKind::MessageReceived,
        EventKind::ScreenObserved {
            state: spawn_core::screen::ScreenState::Ready,
            evidence: "pane text".to_string(),
        },
        EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
        EventKind::MessageReceived,
    ] {
        store.append(message(kind)).expect("append succeeds");
    }
    // Ten seconds apart, first to last: positions 1..=4.
    admin(&schema)
        .batch_execute("update spawn_events set recorded_at_ms = 1000000 + id * 10000")
        .expect("spread the record times");
    let url = spawn_postgres::role_url(&common::schema_url(&schema), "spawn_reader")
        .expect("the reader's URL");
    let reader = spawn_postgres::PostgresStore::connect(&url).expect("the reader connects");

    let entries = reader
        .log_after(GlobalPosition(0), 10)
        .expect("the log reads");
    let names: Vec<&str> = entries.iter().map(|e| e.event_name.as_str()).collect();
    assert_eq!(
        names,
        [
            "message.received",
            "screen.observed",
            "turn.stopped",
            "message.received"
        ]
    );
    assert_eq!(
        entries[1].detail,
        Some(serde_json::json!({"state": "ready"})),
        "the pane's text stays out"
    );
    assert_eq!(entries[0].session_id, session().as_str());
    let after_two = reader
        .log_after(GlobalPosition(2), 1)
        .expect("the log reads");
    assert_eq!(after_two.len(), 1);
    assert_eq!(after_two[0].position, GlobalPosition(3));

    let before = |ms: i64| {
        reader
            .position_before(spawn_core::time::Timestamp(ms))
            .expect("a position")
    };
    assert_eq!(before(0), GlobalPosition(0), "everything is that recent");
    assert_eq!(before(1_010_000), GlobalPosition(0));
    assert_eq!(before(1_010_001), GlobalPosition(1));
    assert_eq!(before(1_030_000), GlobalPosition(2));
    assert_eq!(before(1_035_000), GlobalPosition(3));
    assert_eq!(before(9_000_000), GlobalPosition(4), "nothing that recent");
}

#[derive(Clone)]
enum StressOp {
    Submit {
        agent: AgentId,
        session: SessionId,
        key: IdempotencyKey,
    },
    Append {
        agent: AgentId,
        session: SessionId,
    },
}

struct StressReport {
    total_ops: usize,
    total_events: usize,
    elapsed_ms: f64,
    ops_per_sec: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}

fn percentile_ms(sorted_us: &[u64], numerator: usize, denominator: usize) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_us.len() - 1) * numerator) / denominator;
    f64::from(u32::try_from(sorted_us[idx]).unwrap_or(u32::MAX)) / 1000.0
}

fn build_stress_queue(
    case: &str,
    sessions: usize,
    appends_per_session: usize,
) -> std::collections::VecDeque<StressOp> {
    let mut queue = std::collections::VecDeque::with_capacity(sessions * (1 + appends_per_session));
    for round in 0..=appends_per_session {
        for s in 0..sessions {
            let agent = AgentId::new(format!("agent_{case}_{s}")).expect("valid agent");
            let session = SessionId::new(format!("session_{case}_{s}")).expect("valid session");
            if round == 0 {
                let key = IdempotencyKey::new(format!("k_{case}_{s}_0")).expect("valid key");
                queue.push_back(StressOp::Submit {
                    agent,
                    session,
                    key,
                });
            } else {
                queue.push_back(StressOp::Append { agent, session });
            }
        }
    }
    queue
}

fn record_consumed_batch(
    batch: Vec<spawn_core::event::EventRecord>,
    seen_ids: &mut std::collections::BTreeSet<u64>,
    per_session: &mut std::collections::BTreeMap<String, Vec<u64>>,
    duplicates: &mut usize,
) {
    for record in batch {
        if !seen_ids.insert(record.position.0) {
            *duplicates += 1;
        }
        per_session
            .entry(record.event.session_id.as_str().to_string())
            .or_default()
            .push(record.session_seq.0);
    }
}

type ReaderOutcome = (
    usize,
    usize,
    std::collections::BTreeMap<String, Vec<u64>>,
    usize,
);

fn spawn_stress_reader(
    reader: PostgresStore,
    done: Arc<std::sync::atomic::AtomicBool>,
    barrier: Arc<std::sync::Barrier>,
    read_batch: usize,
    expected_events: usize,
) -> std::thread::JoinHandle<ReaderOutcome> {
    std::thread::spawn(move || {
        let mut cursor = GlobalPosition(0);
        let mut seen_ids = std::collections::BTreeSet::new();
        let mut duplicates = 0usize;
        let mut per_session = std::collections::BTreeMap::new();
        let mut read_errors = 0usize;

        barrier.wait();
        loop {
            let drained_hint = done.load(std::sync::atomic::Ordering::Acquire);
            if let Ok(batch) = reader.read_global(cursor, read_batch) {
                if let Some(last) = batch.last() {
                    cursor = last.position;
                }
                let count = batch.len();
                record_consumed_batch(batch, &mut seen_ids, &mut per_session, &mut duplicates);
                if drained_hint && count < read_batch && seen_ids.len() >= expected_events {
                    let extra = reader.read_global(cursor, read_batch).expect("final read");
                    record_consumed_batch(extra, &mut seen_ids, &mut per_session, &mut duplicates);
                    break;
                }
                if count == 0 {
                    std::thread::yield_now();
                }
            } else {
                read_errors += 1;
                if drained_hint {
                    break;
                }
            }
        }
        (seen_ids.len(), duplicates, per_session, read_errors)
    })
}

fn spawn_stress_writer(
    client: PostgresStore,
    work: Arc<std::sync::Mutex<std::collections::VecDeque<StressOp>>>,
    barrier: Arc<std::sync::Barrier>,
    principal: Principal,
) -> std::thread::JoinHandle<(Vec<u64>, Vec<String>)> {
    std::thread::spawn(move || {
        let mut latencies_us = Vec::new();
        let mut errors = Vec::new();
        barrier.wait();
        loop {
            let next = work.lock().expect("work lock").pop_front();
            let Some(op) = next else {
                break;
            };
            let t0 = std::time::Instant::now();
            let res = match op {
                StressOp::Submit {
                    agent,
                    session,
                    key,
                } => client
                    .submit(
                        &principal,
                        &Submission {
                            agent_id: agent,
                            session_id: session,
                            idempotency_key: key,
                            action: CommandAction::Stop,
                        },
                    )
                    .map(|_| ()),
                StressOp::Append { agent, session } => client
                    .append(NewEvent {
                        agent_id: agent,
                        session_id: session,
                        execution_id: None,
                        source: Source::Observer,
                        source_event_id: None,
                        kind: EventKind::MessageReceived,
                    })
                    .map(|_| ()),
            };
            let elapsed_us = u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);
            match res {
                Ok(()) => latencies_us.push(elapsed_us),
                Err(err) => errors.push(err.to_string()),
            }
        }
        (latencies_us, errors)
    })
}

static STRESS_WORKLOAD_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_concurrent_session_workload(
    case: &str,
    sessions: usize,
    appends_per_session: usize,
    writer_pool_size: usize,
    read_batch: usize,
) -> StressReport {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Barrier, Mutex};
    use std::time::Instant;

    let _stress_gate = STRESS_WORKLOAD_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _budget = ApiBudgetGuard::exclusive();

    let (setup_store, schema) = store(case);
    drop(setup_store);
    let schema_conn = common::schema_url(&schema);

    let queue = build_stress_queue(case, sessions, appends_per_session);
    let total_ops = queue.len();
    let expected_events = sessions * (1 + appends_per_session);
    let work = Arc::new(Mutex::new(queue));

    // Pre-open bounded writer pool + 1 continuous reader (= writer_pool_size + 1 <= max_connections).
    let writers: Vec<_> = (0..writer_pool_size)
        .map(|_| PostgresStore::connect(&schema_conn).expect("writer connect"))
        .collect();
    let reader = PostgresStore::connect(&schema_conn).expect("reader connect");

    let writers_done = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(writer_pool_size + 2));
    let reader_handle = spawn_stress_reader(
        reader,
        Arc::clone(&writers_done),
        Arc::clone(&barrier),
        read_batch,
        expected_events,
    );

    let principal = Principal::new("tester").expect("valid principal");
    let writer_handles: Vec<_> = writers
        .into_iter()
        .map(|client| {
            spawn_stress_writer(
                client,
                Arc::clone(&work),
                Arc::clone(&barrier),
                principal.clone(),
            )
        })
        .collect();

    barrier.wait();
    let wall_start = Instant::now();
    let mut all_latencies_us = Vec::with_capacity(total_ops);
    let mut write_errors = Vec::new();
    for handle in writer_handles {
        let (mut lats, mut errs) = handle.join().expect("writer thread");
        all_latencies_us.append(&mut lats);
        write_errors.append(&mut errs);
    }
    writers_done.store(true, Ordering::Release);
    let (consumed, duplicates, per_session, read_errors) =
        reader_handle.join().expect("reader thread");
    let total_elapsed = wall_start.elapsed();

    assert!(
        write_errors.is_empty(),
        "{case}: expected 0 write/deadlock/serialization errors, got: {:?}",
        &write_errors[..write_errors.len().min(5)]
    );
    assert_eq!(read_errors, 0, "{case}: expected 0 read errors");
    assert_eq!(duplicates, 0, "{case}: expected 0 duplicate events");
    assert_eq!(
        consumed, expected_events,
        "{case}: expected {expected_events} consumed events, got {consumed}"
    );
    assert_eq!(
        per_session.len(),
        sessions,
        "{case}: expected all {sessions} sessions"
    );
    let want_seqs: Vec<u64> = (1..=u64::try_from(1 + appends_per_session).expect("u64")).collect();
    for (sess, seqs) in &per_session {
        assert_eq!(
            seqs, &want_seqs,
            "{case}: session {sess} lost or reordered events: {seqs:?}"
        );
    }

    all_latencies_us.sort_unstable();
    let elapsed_ms = total_elapsed.as_secs_f64() * 1000.0;
    let ops_per_sec =
        f64::from(u32::try_from(total_ops).unwrap_or(u32::MAX)) / total_elapsed.as_secs_f64();
    let report = StressReport {
        total_ops,
        total_events: expected_events,
        elapsed_ms,
        ops_per_sec,
        p50_ms: percentile_ms(&all_latencies_us, 50, 100),
        p95_ms: percentile_ms(&all_latencies_us, 95, 100),
        p99_ms: percentile_ms(&all_latencies_us, 99, 100),
    };
    eprintln!(
        "STRESS_RESULT {case}: sessions={sessions} ops={} events={} elapsed_ms={:.2} tps={:.1} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3}",
        report.total_ops,
        report.total_events,
        report.elapsed_ms,
        report.ops_per_sec,
        report.p50_ms,
        report.p95_ms,
        report.p99_ms,
    );
    report
}

#[test]
fn stress_100_concurrent_sessions_mixed_submit_append_and_read_global() {
    let report = run_concurrent_session_workload("stress100", 100, 5, 6, 64);
    assert_eq!(report.total_ops, 600);
    assert_eq!(report.total_events, 600);
}

#[test]
fn stress_1000_session_burst_writes_and_concurrent_consumption() {
    let report = run_concurrent_session_workload("stress1000", 1000, 2, 6, 128);
    assert_eq!(report.total_ops, 3000);
    assert_eq!(report.total_events, 3000);
}

#[test]
fn different_sessions_and_missions_append_concurrently_without_blocking() {
    let (store, schema) = store("sharded_locks");
    let m1 = store
        .mission_add("M1", &serde_json::json!([]), "human")
        .expect("m1");
    let m2 = store
        .mission_add("M2", &serde_json::json!([]), "human")
        .expect("m2");

    let mut holder = admin(&schema);
    let mut hold = holder.transaction().expect("begin");
    hold.execute(
        "select pg_advisory_xact_lock(1399957401, hashtext('session_one')),
                pg_advisory_xact_lock(1399957402, spawn_mission_lock_key($1))",
        &[&m1],
    )
    .expect("hold session_one L3 and mission m1 L2 locks");

    let worker_schema = schema.to_string();
    let handle = std::thread::spawn(move || -> Result<(), postgres::Error> {
        let mut other = admin(&worker_schema);
        other.execute("set lock_timeout = '1s'", &[])?;
        other.query_one(
            "select out_id from spawn_submit('human', 'k_s2', 'agent_two', 'session_two', '{\"type\":\"stop\"}')",
            &[],
        )?;
        other.query_one(
            "select out_id from spawn_append('agent_two', 'session_two', null, 'observer', 'obs:1', '{\"name\":\"message.received\"}'::jsonb)",
            &[],
        )?;
        other.query_one(
            "select spawn_task_assign($1, 'ephemeral', 'worker', 'parallel mission task')",
            &[&m2],
        )?;
        Ok(())
    });
    let outcome = handle.join().expect("worker thread joins");
    hold.commit().expect("release holder locks");
    outcome.expect("unrelated session and mission must not wait behind L3/L2 holds");
}

#[test]
fn xid8_watermark_recovers_out_of_order_lower_id_commit_without_gaps_or_duplicates() {
    let (store, schema) = store("xid8_ooo_lower_id");
    let reader_url =
        spawn_postgres::role_url(&common::schema_url(&schema), "spawn_reader").expect("reader url");
    let reader = PostgresStore::connect(&reader_url).expect("reader connect");

    let anchor = store
        .append(NewEvent {
            agent_id: AgentId::new("agent_anchor").expect("agent"),
            session_id: SessionId::new("session_anchor").expect("session"),
            execution_id: None,
            source: Source::Observer,
            source_event_id: None,
            kind: EventKind::MessageReceived,
        })
        .expect("anchor append");
    let mut cursor = anchor.record.position;

    // T1 reserves lower event id via nextval BEFORE acquiring an XID (so XID is not yet assigned).
    let mut t1_conn = admin(&schema);
    let mut t1 = t1_conn.transaction().expect("begin t1");
    let id_t1: i64 = t1
        .query_one(
            "select nextval(pg_get_serial_sequence('spawn_events', 'id'))",
            &[],
        )
        .expect("reserve lower id")
        .get(0);
    let xid_before: Option<String> = t1
        .query_one("select pg_current_xact_id_if_assigned()::text", &[])
        .expect("check unassigned xid")
        .get(0);
    assert!(xid_before.is_none(), "nextval must not assign an XID yet");

    // T2 takes higher id (id_t1 + 1) and earlier commit_xid, and commits first.
    let mut t2_conn = admin(&schema);
    let mut t2 = t2_conn.transaction().expect("begin t2");
    let id_t2: i64 = t2
        .query_one(
            "select out_id from spawn_append(
                 'agent_b', 'session_b', null, 'observer', 'ooo_b_1',
                 '{\"name\":\"message.received\"}'::jsonb)",
            &[],
        )
        .expect("t2 append")
        .get(0);
    t2.commit().expect("t2 commit");
    assert!(id_t1 < id_t2, "expected T1 id {id_t1} < T2 id {id_t2}");

    // Global reader reads once after T2 commits -> MUST get id_t2 cleanly.
    let g1 = store.read_global(cursor, 100).expect("read_global 1");
    let l1 = reader.log_after(cursor, 100).expect("log_after 1");
    let pos_t2 = GlobalPosition(u64::try_from(id_t2).expect("u64"));
    assert_eq!(g1.iter().map(|r| r.position).collect::<Vec<_>>(), [pos_t2]);
    assert_eq!(l1.iter().map(|e| e.position).collect::<Vec<_>>(), [pos_t2]);
    cursor = pos_t2;

    // Now T1 assigns commit_xid > T2's commit_xid, inserts the lower id_t1 row, and commits.
    t1.execute(
        "select pg_advisory_xact_lock(1399957401, hashtext('session_a'))",
        &[],
    )
    .expect("t1 session lock");
    t1.execute(
        "insert into spawn_agent_sessions (agent_id, session_id)
         values ('agent_a', 'session_a') on conflict do nothing",
        &[],
    )
    .expect("t1 agent session");
    t1.execute(
        "insert into spawn_events
             (id, agent_id, session_id, execution_id, source, source_event_id,
              kind, session_seq, recorded_at_ms, commit_xid)
         values
             ($1, 'agent_a', 'session_a', null, 'observer', 'ooo_a_1',
              '{\"name\":\"message.received\"}'::jsonb, 1,
              spawn_now_ms(), pg_current_xact_id())",
        &[&id_t1],
    )
    .expect("t1 insert lower id");
    t1.execute("select spawn_session_fold('session_a')", &[])
        .expect("t1 fold");
    t1.commit().expect("t1 commit");

    // Global reader reads again after T1 commits -> MUST recover lower id_t1 with zero misses.
    let pos_t1 = GlobalPosition(u64::try_from(id_t1).expect("u64"));
    let g2 = store.read_global(cursor, 100).expect("read_global 2");
    let l2 = reader.log_after(cursor, 100).expect("log_after 2");
    assert_eq!(g2.iter().map(|r| r.position).collect::<Vec<_>>(), [pos_t1]);
    assert_eq!(l2.iter().map(|e| e.position).collect::<Vec<_>>(), [pos_t1]);
    cursor = pos_t1;

    // Subsequent read must return empty -> zero duplicates.
    assert!(
        store
            .read_global(cursor, 100)
            .expect("read_global 3")
            .is_empty()
    );
    assert!(
        reader
            .log_after(cursor, 100)
            .expect("log_after 3")
            .is_empty()
    );
}

#[test]
fn xid8_watermark_holds_back_higher_xid_while_lower_xid_is_uncommitted() {
    let (store, schema) = store("xid8_xmin_barrier");
    let reader_url =
        spawn_postgres::role_url(&common::schema_url(&schema), "spawn_reader").expect("reader url");
    let reader = PostgresStore::connect(&reader_url).expect("reader connect");

    let anchor = store
        .append(NewEvent {
            agent_id: AgentId::new("agent_base").expect("agent"),
            session_id: SessionId::new("session_base").expect("session"),
            execution_id: None,
            source: Source::Observer,
            source_event_id: None,
            kind: EventKind::MessageReceived,
        })
        .expect("anchor append");
    let cursor = anchor.record.position;

    // T1 takes lower xid + lower id and pauses BEFORE commit.
    let mut t1_conn = admin(&schema);
    let mut t1 = t1_conn.transaction().expect("begin t1");
    let id_1: i64 = t1
        .query_one(
            "select out_id from spawn_append(
                 'agent_x1', 'session_x1', null, 'observer', 'k_x1',
                 '{\"name\":\"message.received\"}'::jsonb)",
            &[],
        )
        .expect("t1 append")
        .get(0);

    // T2 takes higher xid + higher id and commits immediately while T1 stays open.
    let mut t2_conn = admin(&schema);
    let mut t2 = t2_conn.transaction().expect("begin t2");
    let id_2: i64 = t2
        .query_one(
            "select out_id from spawn_append(
                 'agent_x2', 'session_x2', null, 'observer', 'k_x2',
                 '{\"name\":\"message.received\"}'::jsonb)",
            &[],
        )
        .expect("t2 append")
        .get(0);
    t2.commit().expect("t2 commit");
    assert!(id_1 < id_2);

    // Watermark must NOT advance past uncommitted T1!
    assert!(
        store
            .read_global(cursor, 100)
            .expect("read_global")
            .is_empty()
    );
    assert!(reader.log_after(cursor, 100).expect("log_after").is_empty());
    assert_eq!(
        reader
            .position_before(spawn_core::time::Timestamp(i64::MAX))
            .expect("last position"),
        cursor
    );

    t1.commit().expect("t1 commit");

    let p1 = GlobalPosition(u64::try_from(id_1).expect("u64"));
    let p2 = GlobalPosition(u64::try_from(id_2).expect("u64"));
    let g = store
        .read_global(cursor, 100)
        .expect("read_global after commit");
    let l = reader
        .log_after(cursor, 100)
        .expect("log_after after commit");
    assert_eq!(g.iter().map(|r| r.position).collect::<Vec<_>>(), [p1, p2]);
    assert_eq!(l.iter().map(|e| e.position).collect::<Vec<_>>(), [p1, p2]);
    assert!(store.read_global(p2, 100).expect("tail empty").is_empty());
}

fn assert_early_xid_watermark_held_and_recovered<A, F>(
    store: &PostgresStore,
    reader: &PostgresStore,
    schema: &str,
    cursor0: GlobalPosition,
    key_suffix: &str,
    acquire_xa: A,
    finish_ta: F,
) where
    A: FnOnce(&mut postgres::Transaction<'_>),
    F: FnOnce(&mut postgres::Transaction<'_>) -> i64,
{
    let mut ca = admin(schema);
    let mut ta = ca.transaction().expect("begin ta");
    acquire_xa(&mut ta);
    let xa: Option<String> = ta
        .query_one("select pg_current_xact_id_if_assigned()::text", &[])
        .expect("xa")
        .get(0);
    assert!(xa.is_some(), "T_a must hold an assigned xid");

    // T_b appends in another session with x_b > x_a and commits while T_a stays open.
    let mut cb = admin(schema);
    let mut tb = cb.transaction().expect("begin tb");
    let key_b = format!("k_{key_suffix}_b");
    let id_b: i64 = tb
        .query_one(
            "select out_id from spawn_append(
                 'agent_b', 'session_b', null, 'observer', $1,
                 '{\"name\":\"message.received\"}'::jsonb)",
            &[&key_b],
        )
        .expect("tb append")
        .get(0);
    tb.commit().expect("tb commit");

    // Readers MUST hold back id_b while x_a is in flight (fail-closed watermark).
    assert!(
        store
            .read_global(cursor0, 100)
            .expect("rg empty")
            .is_empty()
    );
    assert!(reader.log_after(cursor0, 100).expect("la empty").is_empty());
    assert_eq!(
        reader
            .position_before(spawn_core::time::Timestamp(i64::MAX))
            .expect("last position held back"),
        cursor0
    );

    let id_a = finish_ta(&mut ta);
    ta.commit().expect("ta commit");
    assert!(id_a > id_b);

    let pos_a = GlobalPosition(u64::try_from(id_a).expect("u64"));
    let pos_b = GlobalPosition(u64::try_from(id_b).expect("u64"));
    let g = store.read_global(cursor0, 100).expect("rg after commit");
    let l = reader.log_after(cursor0, 100).expect("la after commit");
    assert_eq!(
        g.iter().map(|r| r.position).collect::<Vec<_>>(),
        [pos_a, pos_b]
    );
    assert_eq!(
        l.iter().map(|e| e.position).collect::<Vec<_>>(),
        [pos_a, pos_b]
    );
    assert!(store.read_global(pos_b, 100).expect("rg tail").is_empty());
    assert!(reader.log_after(pos_b, 100).expect("la tail").is_empty());
}

fn setup_anchor_and_reader(
    case: &str,
) -> (PostgresStore, PostgresStore, ApiSchema, GlobalPosition) {
    let (store, schema) = store(case);
    let reader_url =
        spawn_postgres::role_url(&common::schema_url(&schema), "spawn_reader").expect("reader url");
    let reader = PostgresStore::connect(&reader_url).expect("reader connect");
    let anchor = store
        .append(NewEvent {
            agent_id: AgentId::new("agent_anchor").expect("agent"),
            session_id: SessionId::new("session_anchor").expect("session"),
            execution_id: None,
            source: Source::Observer,
            source_event_id: None,
            kind: EventKind::MessageReceived,
        })
        .expect("anchor append");
    let cursor0 = anchor.record.position;
    (store, reader, schema, cursor0)
}

#[test]
fn xid8_watermark_holds_back_commit_during_for_update_row_lock_on_spawn_events() {
    let (store, reader, schema, cursor0) = setup_anchor_and_reader("xid8_for_update_events");
    let cursor0_i64 = i64::try_from(cursor0.0).expect("cursor0 fits i64");

    assert_early_xid_watermark_held_and_recovered(
        &store,
        &reader,
        &schema,
        cursor0,
        "for_upd_events",
        |ta| {
            let locked: i64 = ta
                .query_one(
                    "select id from spawn_events where id = $1 for update",
                    &[&cursor0_i64],
                )
                .expect("for update")
                .get(0);
            assert_eq!(locked, cursor0_i64);
        },
        |ta| {
            ta.query_one(
                "select out_id from spawn_append(
                     'agent_a', 'session_a', null, 'observer', 'k_for_upd_a',
                     '{\"name\":\"message.received\"}'::jsonb)",
                &[],
            )
            .expect("ta append")
            .get(0)
        },
    );
}

#[test]
fn xid8_watermark_holds_back_commit_during_spawn_commands_for_update_and_spawn_finish() {
    let (store, schema) = store("xid8_for_update_commands");
    let reader_url =
        spawn_postgres::role_url(&common::schema_url(&schema), "spawn_reader").expect("reader url");
    let reader = PostgresStore::connect(&reader_url).expect("reader connect");

    let mut setup = admin(&schema);
    let cmd_a: i64 = setup
        .query_one(
            "select out_id from spawn_submit(
                 'human', 'k_cmd_for_upd_a', 'agent_a', 'session_a',
                 '{\"type\":\"stop\"}'::jsonb)",
            &[],
        )
        .expect("submit cmd_a")
        .get(0);
    let accepted_key = format!("cmd_{cmd_a}:accepted");
    let accepted_id: i64 = setup
        .query_one(
            "select id from spawn_events where source_event_id = $1",
            &[&accepted_key],
        )
        .expect("accepted event id")
        .get(0);
    let cursor0 = GlobalPosition(u64::try_from(accepted_id).expect("u64"));
    drop(setup);

    assert_early_xid_watermark_held_and_recovered(
        &store,
        &reader,
        &schema,
        cursor0,
        "for_upd_commands",
        |ta| {
            let locked: i64 = ta
                .query_one(
                    "select id from spawn_commands where id = $1 for update",
                    &[&cmd_a],
                )
                .expect("commands for update")
                .get(0);
            assert_eq!(locked, cmd_a);
        },
        |ta| {
            let finished: String = ta
                .query_one("select spawn_finish($1, 0, true, null)", &[&cmd_a])
                .expect("spawn_finish")
                .get(0);
            assert_eq!(finished, "done");
            let finished_key = format!("cmd_{cmd_a}:finished");
            ta.query_one(
                "select id from spawn_events where source_event_id = $1",
                &[&finished_key],
            )
            .expect("finished event id")
            .get(0)
        },
    );
}

#[test]
fn xid8_watermark_holds_back_commit_during_bare_pg_current_xact_id_without_relation_locks() {
    let _budget = ApiBudgetGuard::exclusive();
    let (store, reader, schema, cursor0) = setup_anchor_and_reader("xid8_bare_xact_id");

    assert_early_xid_watermark_held_and_recovered(
        &store,
        &reader,
        &schema,
        cursor0,
        "bare_xact_id",
        |ta| {
            let _: String = ta
                .query_one("select pg_current_xact_id()::text", &[])
                .expect("assign bare xid")
                .get(0);
            let rel_locks: i64 = ta
                .query_one(
                    "select count(*)
                       from pg_locks
                      where pid = pg_backend_pid()
                        and locktype = 'relation'
                        and mode <> 'AccessShareLock'",
                    &[],
                )
                .expect("count relation locks")
                .get(0);
            assert_eq!(
                rel_locks, 0,
                "bare pg_current_xact_id must hold zero non-AccessShare relation locks"
            );
        },
        |ta| {
            ta.query_one(
                "select out_id from spawn_append(
                     'agent_a', 'session_a', null, 'observer', 'k_bare_xact_a',
                     '{\"name\":\"message.received\"}'::jsonb)",
                &[],
            )
            .expect("ta append")
            .get(0)
        },
    );
}

#[test]
fn xid8_watermark_holds_back_commit_during_create_temp_table_and_insert_before_append() {
    let _budget = ApiBudgetGuard::exclusive();
    let (store, reader, schema, cursor0) = setup_anchor_and_reader("xid8_temp_table");

    assert_early_xid_watermark_held_and_recovered(
        &store,
        &reader,
        &schema,
        cursor0,
        "temp_table",
        |ta| {
            ta.batch_execute(
                "create temp table tt (id int) on commit drop;
                 insert into tt values (1);",
            )
            .expect("create temp table and insert");
        },
        |ta| {
            ta.query_one(
                "select out_id from spawn_append(
                     'agent_a', 'session_a', null, 'observer', 'k_temp_table_a',
                     '{\"name\":\"message.received\"}'::jsonb)",
                &[],
            )
            .expect("ta append")
            .get(0)
        },
    );
}

#[test]
fn xid8_watermark_holds_back_commit_during_create_table_in_schema_before_append() {
    let _budget = ApiBudgetGuard::exclusive();
    let (store, reader, schema, cursor0) = setup_anchor_and_reader("xid8_new_table");

    assert_early_xid_watermark_held_and_recovered(
        &store,
        &reader,
        &schema,
        cursor0,
        "new_table",
        |ta| {
            ta.batch_execute(
                "create table if not exists review_new_feature (id bigint primary key, note text);",
            )
            .expect("create uncommitted table in schema");
        },
        |ta| {
            ta.query_one(
                "select out_id from spawn_append(
                     'agent_a', 'session_a', null, 'observer', 'k_new_table_a',
                     '{\"name\":\"message.received\"}'::jsonb)",
                &[],
            )
            .expect("ta append")
            .get(0)
        },
    );
}

#[test]
fn init_db_migration_lock_excludes_concurrent_append_without_blocking_other_schemas() {
    let (store_a, reader_a, schema_a, cursor_a) = setup_anchor_and_reader("l0_mig_a");
    let (store_b, reader_b, schema_b, cursor_b) = setup_anchor_and_reader("l0_mig_b");

    let mut mig_conn = admin(&schema_a);
    let mut mig_tx = mig_conn.transaction().expect("mig tx");
    mig_tx
        .batch_execute(
            "select pg_advisory_xact_lock(
                 1399957400,
                 (
                     ((quote_ident(current_schema())::regnamespace)::oid::bigint
                      - case when (quote_ident(current_schema())::regnamespace)::oid::bigint >= 2147483648
                             then 4294967296 else 0 end)::integer
                 )
             );
             create table if not exists l0_probe_table (id int);",
        )
        .expect("acquire L0 exclusive and assign xid in schema_a");

    let appended_b = store_b
        .append(NewEvent {
            agent_id: AgentId::new("agent_b").expect("agent"),
            session_id: SessionId::new("session_b").expect("session"),
            execution_id: None,
            source: Source::Observer,
            source_event_id: Some("k_l0_other_schema".to_string()),
            kind: EventKind::MessageReceived,
        })
        .expect("schema_b append must not block on schema_a L0");
    let pos_b = appended_b.record.position;
    let g_b = store_b.read_global(cursor_b, 100).expect("rg schema_b");
    let l_b = reader_b.log_after(cursor_b, 100).expect("la schema_b");
    assert_eq!(g_b.iter().map(|r| r.position).collect::<Vec<_>>(), [pos_b]);
    assert_eq!(l_b.iter().map(|e| e.position).collect::<Vec<_>>(), [pos_b]);
    drop(reader_b);
    drop(store_b);
    drop(schema_b);

    let schema_a_name = schema_a.to_string();
    let schema_a_for_thread = schema_a_name.clone();
    let appender = std::thread::spawn(move || -> i64 {
        let mut conn = admin(&schema_a_for_thread);
        conn.query_one(
            "select out_id from spawn_append(
                 'agent_a', 'session_a', null, 'observer', 'k_l0_blocked_a',
                 '{\"name\":\"message.received\"}'::jsonb)",
            &[],
        )
        .expect("schema_a append after L0 release")
        .get(0)
    });

    let mut watcher = admin(&schema_a_name);
    let started = std::time::Instant::now();
    loop {
        let waiting: bool = watcher
            .query_one(
                "select exists (
                     select 1 from pg_locks l
                      where l.locktype = 'advisory'
                        and l.database = (select oid from pg_database where datname = current_database())
                        and l.classid = 1399957400::oid
                        and l.objsubid = 2
                        and l.objid = quote_ident($1)::regnamespace
                        and l.mode = 'ShareLock'
                        and not l.granted
                 )",
                &[&schema_a_name],
            )
            .expect("poll L0 waiter")
            .get(0);
        if waiting {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "concurrent spawn_append never waited on L0 exclusive lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !appender.is_finished(),
        "concurrent spawn_append must remain blocked until L0 migration commits"
    );

    mig_tx.commit().expect("commit L0 migration");
    let id_a = appender.join().expect("join blocked appender");
    let pos_a = GlobalPosition(u64::try_from(id_a).expect("u64"));
    let g_a = store_a.read_global(cursor_a, 100).expect("rg schema_a");
    let l_a = reader_a.log_after(cursor_a, 100).expect("la schema_a");
    assert_eq!(g_a.iter().map(|r| r.position).collect::<Vec<_>>(), [pos_a]);
    assert_eq!(l_a.iter().map(|e| e.position).collect::<Vec<_>>(), [pos_a]);
}

fn collect_rs_paths(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_paths(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn normalize_sql_whitespace(raw: &str) -> String {
    let mut no_comments = String::with_capacity(raw.len());
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("--") {
            no_comments.push_str(line);
            no_comments.push('\n');
        }
    }
    no_comments
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

struct ParsedSchemaSql {
    top_level_normalized: String,
    first_create_table_offset: usize,
    function_bodies: Vec<String>,
    do_blocks: Vec<(usize, String)>,
}

fn find_dollar_tag(bytes: &[u8], pos: usize) -> Option<&str> {
    if bytes.get(pos) != Some(&b'$') {
        return None;
    }
    let mut end = pos + 1;
    while end < bytes.len() {
        let b = bytes[end];
        if b == b'$' {
            return std::str::from_utf8(&bytes[pos..=end]).ok();
        }
        if !(b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        end += 1;
    }
    None
}

fn parse_schema_sql_blocks(schema_sql: &str) -> ParsedSchemaSql {
    let bytes = schema_sql.as_bytes();
    let mut top_level_raw = String::with_capacity(schema_sql.len());
    let mut function_bodies = Vec::new();
    let mut do_blocks = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'\'' {
            top_level_raw.push('\'');
            i += 1;
            while i < bytes.len() {
                let ch = schema_sql[i..].chars().next().expect("char");
                top_level_raw.push(ch);
                i += ch.len_utf8();
                if ch == '\'' {
                    if bytes.get(i) == Some(&b'\'') {
                        top_level_raw.push('\'');
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            continue;
        }
        if let Some(tag) = find_dollar_tag(bytes, i) {
            let block_start = i;
            let body_start = i + tag.len();
            let rest = &schema_sql[body_start..];
            let end_rel = rest.find(tag).expect("matching closing dollar quote tag");
            let body = &rest[..end_rel];
            let stmt_prefix = top_level_raw
                .rsplit(';')
                .next()
                .unwrap_or("")
                .trim_start()
                .to_ascii_lowercase();
            let norm_body = normalize_sql_whitespace(body);
            if stmt_prefix.starts_with("create function")
                || stmt_prefix.starts_with("create or replace function")
            {
                function_bodies.push(norm_body);
            } else {
                do_blocks.push((block_start, norm_body));
            }
            top_level_raw.push(' ');
            i = body_start + end_rel + tag.len();
            continue;
        }
        let ch = schema_sql[i..].chars().next().expect("char");
        top_level_raw.push(ch);
        i += ch.len_utf8();
    }

    let lower_raw = schema_sql.to_ascii_lowercase();
    let first_create_table_offset = lower_raw
        .find("\ncreate table ")
        .expect("expected top-level create table in schema.sql");
    ParsedSchemaSql {
        top_level_normalized: normalize_sql_whitespace(&top_level_raw),
        first_create_table_offset,
        function_bodies,
        do_blocks,
    }
}

fn strip_comments_and_normalize_rust(source: &str) -> String {
    let mut without_blocks = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find("/*") {
        without_blocks.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("*/") else {
            rest = "";
            break;
        };
        rest = &after_start[end + 2..];
    }
    without_blocks.push_str(rest);

    without_blocks
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

#[test]
fn watermark_writers_never_use_temp_tables_or_early_pg_current_xact_id() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_dirs = [
        manifest_dir.join("src"),
        manifest_dir.join("../spawn-cli/src"),
    ];
    for dir in &src_dirs {
        assert!(dir.is_dir(), "expected source dir {}", dir.display());
        let mut files = Vec::new();
        collect_rs_paths(dir, &mut files);
        files.sort();
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read rs file");
            let normalized = strip_comments_and_normalize_rust(&text);
            for forbidden in ["temp table", "temporary table", "pg_current_xact_id"] {
                assert!(
                    !normalized.contains(forbidden),
                    "{} must not contain `{forbidden}` in production SQL/code",
                    path.display()
                );
            }
        }
    }

    let schema_sql =
        std::fs::read_to_string(manifest_dir.join("sql/schema.sql")).expect("read schema.sql");
    let parsed = parse_schema_sql_blocks(&schema_sql);
    assert!(
        parsed.function_bodies.len() >= 50,
        "expected >= 50 SQL functions in schema.sql, got {}",
        parsed.function_bodies.len()
    );
    for (idx, body) in parsed.function_bodies.iter().enumerate() {
        for forbidden in ["temp table", "temporary table"] {
            assert!(
                !body.contains(forbidden),
                "schema.sql function #{idx} must never use `{forbidden}` at runtime"
            );
        }
    }

    let (first_do_offset, first_do_body) = parsed
        .do_blocks
        .first()
        .expect("top schema/migration lock DO block");
    assert!(
        first_do_body.contains("pg_advisory_xact_lock(1399957400, v_ns_key)")
            && *first_do_offset < parsed.first_create_table_offset,
        "schema.sql must acquire exclusive L0 lock (1399957400) before its first CREATE TABLE"
    );

    for forbidden in ["temp table", "temporary table"] {
        assert!(
            !parsed.top_level_normalized.contains(forbidden),
            "schema.sql top-level DDL outside DO blocks must not contain `{forbidden}`"
        );
    }

    let mut legacy_temp_blocks = 0;
    for (offset, body) in &parsed.do_blocks {
        if body.contains("temp table") || body.contains("temporary table") {
            assert!(
                *offset > *first_do_offset
                    && body.contains("create temporary table spawn_legacy_rows")
                    && body.contains("pg_advisory_xact_lock(1399957402, 0)"),
                "unexpected temporary table in schema.sql DO block at byte {offset}"
            );
            legacy_temp_blocks += 1;
        }
    }
    assert_eq!(
        legacy_temp_blocks, 1,
        "expected exactly one legacy backfill DO block using spawn_legacy_rows under L0_exclusive"
    );

    for (idx, line) in schema_sql.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("pg_current_xact_id") {
            assert!(
                lower.contains("commit_xid")
                    || lower.contains("column_default is distinct from 'pg_current_xact_id()'")
                    || lower.contains("v_seq, v_at, pg_current_xact_id()"),
                "schema.sql:{} must only invoke pg_current_xact_id() on spawn_events commit_xid assignment: {trimmed}",
                idx + 1
            );
        }
    }
}

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
use spawn_core::command::{CommandAction, Outcome, Submission};
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

mod common;
use common::{admin, store, url};

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
        .execute("select pg_advisory_xact_lock(1399957335)", &[])
        .expect("hold the append lock");

    // A submission starts and waits for it, in a transaction it keeps open.
    let (submitted_tx, submitted) = std::sync::mpsc::channel::<(i32, Option<i64>)>();
    let (commit_tx, commit) = std::sync::mpsc::channel::<()>();
    let waiter_schema = schema.clone();
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

#[test]
fn open_tools_are_started_minus_finished_in_live_executions() {
    let (store, _schema) = store("open_tools");
    let execution = ExecutionId::new("exec_1").expect("valid execution");
    let started = |id: &str| EventKind::ToolStarted {
        tool_name: "Bash".to_string(),
        tool_use_id: id.to_string(),
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
    let reader = common::login(&schema, "spawn_reader");
    let as_reader = |client: &mut Client| {
        client
            .batch_execute(&format!("set session authorization {reader}"))
            .expect("become the reader");
    };
    as_reader(&mut client);
    assert!(readable(&mut client).is_empty(), "no grant reads nothing");
    client
        .batch_execute("reset session authorization")
        .expect("back to the owner");
    client
        .execute(
            "insert into spawn_grants (principal, agent_id) values ('spawn_reader', 'agent_one')",
            &[],
        )
        .expect("grant");
    as_reader(&mut client);
    assert_eq!(readable(&mut client), vec!["session_one"]);
    let timeline: i64 = client
        .query_one(
            "select count(*) from spawn_read_timeline('session_two')",
            &[],
        )
        .expect("spawn_read_timeline answers")
        .get(0);
    assert_eq!(timeline, 0, "an ungranted agent's session reads empty");
    client
        .batch_execute("reset session authorization")
        .expect("back to the owner");
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
    let (_store, schema) = store("revoke");
    let mut client = admin(&schema);
    // Roles are the server's, not the schema's: named by the schema, which
    // is this run's alone (the pid alone is the same in every gate's PID
    // namespace, and two gates on one server would share the role).
    let role = format!("{schema}_reader");
    client
        .batch_execute(&format!(
            "drop role if exists {role}; create role {role}; \
             grant usage on schema {schema} to {role}; set role {role}"
        ))
        .expect("become a plain role");
    for view in [
        "agent_state",
        "agent_status",
        "agent_timeline",
        "agent_tasks",
    ] {
        let error = client
            .query(&format!("select * from {view}"), &[])
            .expect_err("a plain role cannot read a view");
        assert_eq!(
            error.code(),
            Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{view}"
        );
    }
    let rows = client
        .query("select * from spawn_read_status()", &[])
        .expect("the read function answers");
    assert!(rows.is_empty(), "no grant reads nothing");
    client
        .batch_execute(&format!(
            "reset role; drop owned by {role}; drop role {role}"
        ))
        .expect("clean up the role");
}

#[test]
fn a_login_without_grants_reads_no_command_ids() {
    let (store, schema) = store("public_ids");
    store
        .submit(
            &Principal::new("human").expect("valid principal"),
            &start(session(), "someone_elses"),
        )
        .expect("a command");
    let mut client = admin(&schema);
    let role = common::schema_name("plain").replace("spawn_test_", "spawn_plain_");
    client
        .batch_execute(&format!(
            "create role {role}; grant usage on schema {schema} to {role}; \
             set session authorization {role}"
        ))
        .expect("become a login with no grants");
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
    client
        .batch_execute(&format!(
            "reset session authorization; drop owned by {role}; drop role {role}"
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
            &[&schema],
        )
        .expect("read the access lists")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        granted,
        [
            "agent_commands INSERT",
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
            &[&schema],
        )
        .expect("information_schema answers");
    let actual: Vec<String> = rows
        .iter()
        .map(|row| format!("{}: {}", row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect();
    let expected = [
        "agent_artifacts: session_id text, agent_id text, execution_id text, project text, branch text, commit_id text, reason text, session_seq bigint, recorded_at_ms bigint",
        "agent_commands: id bigint, agent_id text, session_id text, idempotency_key text, action jsonb, created_at_ms bigint",
        "agent_executions: session_id text, agent_id text, execution_id text, engine text, terminal text, workspace text, started_at_ms bigint, cause text, exit_code integer, ended_at_ms bigint, exit_reported boolean, duration_ms bigint, command_id bigint",
        "agent_freshness: session_id text, agent_id text, last_session_seq bigint, last_event_id bigint, last_event_at_ms bigint, event_count bigint",
        "agent_judgements: session_id text, session_seq bigint, event_id bigint, agent_id text, state text, reason text, based_on_seq bigint, observation jsonb, recorded_at_ms bigint",
        "agent_lineage: session_id text, agent_id text, ancestor_session_id text, depth integer",
        "agent_open_jobs: session_id text, agent_id text, execution_id text, event_id bigint, job_id text, job_kind text, session_seq bigint, started_at_ms bigint",
        "agent_open_tools: session_id text, agent_id text, execution_id text, tool_use_id text, tool_name text, session_seq bigint, started_at_ms bigint",
        "agent_relations: session_id text, agent_id text, parent_session_id text, recorded_at_ms bigint",
        "agent_state: session_id text, agent_id text, state text, reason text, confidence double precision, based_on_seq bigint, judge text, session_seq bigint, recorded_at_ms bigint, event_id bigint, observation jsonb",
        "agent_status: session_id text, agent_id text, state text, reason text, observation_seq bigint, last_session_seq bigint, last_event_at_ms bigint, execution_id text, engine text, started_at_ms bigint, ended_at_ms bigint, exit_reported boolean, live boolean",
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
    for base in ["spawn_supervisor", "spawn_reader", "spawn_human"] {
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

#[test]
fn migrations_racing_on_one_server_all_succeed() {
    // Roles and grants are shared by the whole server: parallel migrations of
    // separate databases used to fail with "tuple concurrently updated".
    let base = url();
    let (server, _) = base.rsplit_once('/').expect("a database URL");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let names: Vec<String> = (0..4)
        .map(|index| format!("spawn_race_{}_{nanos:x}_{index}", std::process::id()))
        .collect();
    let mut admin = Client::connect(&base, NoTls).expect("admin connection");
    for name in &names {
        admin
            .batch_execute(&format!("create database {name}"))
            .expect("create a database");
    }
    let workers: Vec<_> = names
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
    for name in &names {
        let _ = admin.batch_execute(&format!("drop database if exists {name} with (force)"));
    }
    for result in results {
        result.expect("every racing migration succeeds");
    }
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
                },
            ),
            (
                "ended",
                EventKind::ExecutionEnded {
                    cause: EndCause::Lost,
                    exit_code: None,
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

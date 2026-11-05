//! Public API tests for spawn-postgres.
//!
//! A real database is required. `SPAWN_TEST_DATABASE_URL` must point at a
//! disposable server, for example
//! `postgres://postgres:PASSWORD@127.0.0.1:15432/spawn`. The test panics when the
//! variable is missing: silently skipping would let a broken adapter pass.
//!
//! Every case runs in a schema of its own, so a shared database cannot leak
//! events or commands between cases or between runs.

use std::time::Duration;

use postgres::Client;
use spawn_core::command::{CommandAction, Outcome, Submission};
use spawn_core::contract;
use spawn_core::event::{
    ArtifactRef, CheckpointReason, EndCause, Engine, EventKind, GlobalPosition, ModelProfile,
    NewEvent, SessionSeq, Source,
};
use spawn_core::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, IdempotencyKey, Principal, SessionId,
    TerminalName, WorkspaceId,
};
use spawn_core::observation::{Confidence, Observation, SemanticState};
use spawn_core::port::{CommandQueue, EventLog};

mod common;
use common::{admin, store};

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
            engine: Engine::Claude,
            prompt: "hello".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
        },
    }
}

#[test]
fn postgres_passes_the_shared_contract() {
    let (store, _schema) = store("contract");
    contract::event_log("pg_evt", &store);
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
                engine: Engine::Claude,
                terminal: TerminalName::new("spawn-exec_1").expect("valid terminal"),
                workspace: WorkspaceId::new("9:/workspaces/9").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
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
                engine: Engine::Claude,
                terminal: TerminalName::new("spawn-exec_1").expect("valid terminal"),
                workspace: WorkspaceId::new("9:/workspaces/9").expect("valid workspace"),
                command_id: command,
                fork: None,
                model: ModelProfile::Default,
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

    // Another login reads only the agents it holds a grant for.
    client
        .batch_execute(
            "grant execute on function spawn_read_freshness(), spawn_read_timeline(text) \
             to spawn_reader",
        )
        .expect("the reader may call the readers");
    let as_reader = |client: &mut Client| {
        client
            .batch_execute("set session authorization spawn_reader")
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

    // Another agent's event does not move the session out of its owner.
    store
        .append(NewEvent {
            agent_id: AgentId::new("agent_0").expect("valid agent"),
            ..message(EventKind::MessageReceived)
        })
        .expect("append succeeds");
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
                engine: Engine::Claude,
                terminal: TerminalName::new("spawn-exec_status").expect("valid terminal"),
                workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
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
    let role = format!("spawn_reader_{}", std::process::id());
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
                        engine: Engine::Claude,
                        terminal: TerminalName::for_execution(&execution),
                        workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                        command_id: CommandId(number),
                        fork: None,
                        model: profile,
                    },
                )
            })
            .expect("append succeeds");
        assert_eq!(model().as_deref(), Some(profile.as_str()));
    }
}

#[test]
fn the_restricted_logins_have_derived_passwords_and_write_no_events() {
    let (_store, schema) = store("logins");
    for role in ["spawn_supervisor", "spawn_reader"] {
        let fixed = format!(
            "postgres://{role}:{role}@{}",
            common::url().rsplit_once('@').expect("a host").1
        );
        assert!(
            postgres::Client::connect(&fixed, postgres::NoTls).is_err(),
            "{role}'s old fixed password is refused"
        );
        let mut client = postgres::Client::connect(
            &spawn_postgres::role_url(&common::url(), role).expect("a password in the test URL"),
            postgres::NoTls,
        )
        .expect("the derived password logs in");
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

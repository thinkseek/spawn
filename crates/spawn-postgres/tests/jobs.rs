//! Background jobs in the dispatcher's facts and on the board: a
//! `job.started` of the live execution with no later `job.started` or
//! `job.ended` of its id is open, with its age on the database's clock; its
//! `job.ended`, or its execution's end, closes it, a job of an earlier
//! execution is never open in a later one, and an Agent resumed under its id
//! is open again from its new start.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use serde_json::Value;
use spawn_core::event::{
    ENGINE, EndCause, EventKind, JobEnd, JobKind, ModelProfile, NewEvent, Source,
};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
use spawn_core::port::EventLog;
use spawn_postgres::PostgresStore;

mod common;
use common::store;

const SESSION: &str = "session_worker_jobs";
const AGENT: &str = "agent_worker_jobs";

fn append(store: &PostgresStore, execution: u64, source: Source, key: &str, kind: EventKind) {
    store
        .append(NewEvent {
            agent_id: AgentId::new(AGENT).expect("valid agent"),
            session_id: SessionId::new(SESSION).expect("valid session"),
            execution_id: Some(ExecutionId::for_command(CommandId(execution))),
            source,
            source_event_id: Some(key.to_string()),
            kind,
        })
        .expect("append");
}

fn started(store: &PostgresStore, execution: u64) {
    append(
        store,
        execution,
        Source::Bridge,
        &format!("start-{execution}"),
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

fn job(store: &PostgresStore, execution: u64, id: &str, kind: JobKind) {
    job_by(store, execution, id, kind, "toolu_1");
}

/// A start of `id` by the tool call `by` (an Agent resumed by a message is
/// started again, by that call).
fn job_by(store: &PostgresStore, execution: u64, id: &str, kind: JobKind, by: &str) {
    append(
        store,
        execution,
        Source::Claude,
        &format!("job:{id}:started:{by}"),
        EventKind::JobStarted {
            job_id: id.to_string(),
            kind,
        },
    );
}

fn job_ended(store: &PostgresStore, execution: u64, id: &str) {
    job_ended_by(store, execution, id, "ended:0f1e2d3c4b5a6978");
}

/// An end of `id` keyed by `what` ended it (a notification's digest).
fn job_ended_by(store: &PostgresStore, execution: u64, id: &str, what: &str) {
    append(
        store,
        execution,
        Source::Claude,
        &format!("job:{id}:{what}"),
        EventKind::JobEnded {
            job_id: id.to_string(),
            how: JobEnd::Completed,
        },
    );
}

/// The worker's row of `dispatch_facts`.
fn facts(store: &PostgresStore) -> Value {
    store
        .dispatch_facts()
        .expect("facts")
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["session_id"] == SESSION)
        .cloned()
        .expect("the worker's row")
}

/// The ids of the open jobs in the dispatcher's facts and on the board,
/// which must agree.
fn open(store: &PostgresStore) -> Vec<String> {
    let ids = |jobs: &Value| -> Vec<String> {
        jobs.as_array()
            .expect("a list")
            .iter()
            .map(|job| job["id"].as_str().expect("an id").to_string())
            .collect()
    };
    let dispatched = ids(&facts(store)["jobs"]);
    let board = store.fleet_board().expect("board");
    let row = board
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["session_id"] == SESSION)
        .expect("the worker on the board");
    assert_eq!(ids(&row["activity"]["jobs"]), dispatched, "{row:#}");
    dispatched
}

#[test]
fn an_executions_open_jobs_are_facts_until_they_or_it_end() {
    let (store, _schema) = store("open_jobs");
    started(&store, 1);
    job(&store, 1, "b8w03og7k", JobKind::Bash);
    job(&store, 1, "bv41gl0hm", JobKind::Monitor);
    append(
        &store,
        1,
        Source::Claude,
        "turn-1",
        EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
    );
    assert_eq!(open(&store), ["b8w03og7k", "bv41gl0hm"], "oldest first");
    let first = &facts(&store)["jobs"][0];
    assert_eq!(first["kind"], "bash");
    assert!(first["event_id"].as_i64().is_some(), "{first}");
    let age = first["age_ms"].as_i64().expect("an age");
    assert!(age >= 0, "on the database's clock: {first}");
    assert!(first["started_at_ms"].as_i64().is_some());

    job_ended(&store, 1, "b8w03og7k");
    assert_eq!(open(&store), ["bv41gl0hm"]);

    // The execution ends with the Monitor open: closed by that record.
    append(
        &store,
        1,
        Source::Bridge,
        "end-1",
        EventKind::ExecutionEnded {
            cause: EndCause::Stopped,
            exit_code: None,
            signal: None,
            evidence: None,
        },
    );
    assert!(open(&store).is_empty());

    // Resumed: the earlier execution's job is not open in the new one, and
    // the new one's are.
    started(&store, 2);
    assert!(open(&store).is_empty());
    job(&store, 2, "bq1n7c2x4", JobKind::Bash);
    assert_eq!(open(&store), ["bq1n7c2x4"]);
    // An end the engine names for a job no record of this session started
    // (a resume's notice about an earlier engine process) closes nothing.
    job_ended(&store, 2, "bqikdrfgk");
    assert_eq!(open(&store), ["bq1n7c2x4"]);

    // A background Agent: open from its launch to its end, then open again
    // from the start a message's resume records, as of that start.
    job_by(
        &store,
        2,
        "a2adacaabc9f9b048",
        JobKind::Agent,
        "toolu_launch",
    );
    assert_eq!(open(&store), ["bq1n7c2x4", "a2adacaabc9f9b048"]);
    let first = facts(&store)["jobs"][1]["event_id"]
        .as_i64()
        .expect("an id");
    assert_eq!(facts(&store)["jobs"][1]["kind"], "agent");
    job_ended_by(&store, 2, "a2adacaabc9f9b048", "ended:1111111111111111");
    assert_eq!(open(&store), ["bq1n7c2x4"]);
    job_by(
        &store,
        2,
        "a2adacaabc9f9b048",
        JobKind::Agent,
        "toolu_resume",
    );
    assert_eq!(open(&store), ["bq1n7c2x4", "a2adacaabc9f9b048"]);
    let resumed = facts(&store)["jobs"][1]["event_id"]
        .as_i64()
        .expect("an id");
    assert!(resumed > first, "the row is the latest start");
    job_ended_by(&store, 2, "a2adacaabc9f9b048", "ended:2222222222222222");
    assert_eq!(open(&store), ["bq1n7c2x4"]);
}

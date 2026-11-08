//! The recovery's records (objective 028, docs/fleet-architecture.md §3.1,
//! "the recovery"): `spawn_host_record` accepts an interruption and a clear
//! with their fields, from the owner only, and refuses a cause or a path it
//! does not know — a record no reader could act on is never written.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, store};

/// The control records of `kind` in `client`'s schema, oldest first.
fn records(client: &mut Client, kind: &str) -> Vec<Value> {
    client
        .query(
            "select kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control' and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = $1
              order by session_seq",
            &[&kind],
        )
        .expect("read the control records")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// What a refused record's database error says: the message the function
/// raised, not the client's one-line summary of it.
fn refused(error: &postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |error| error.message().to_string())
}

/// An interruption of `execution`, as the recovery writes it.
fn interruption(execution: &str, cause: &str, path: &str, due: &Value) -> Value {
    json!({
        "type": "execution.interrupted",
        "agent_id": "agent_worker_recovery",
        "session_id": "session_worker_recovery",
        "execution_id": execution,
        "cause": cause,
        "evidence": {"exit_code": 0},
        "path": path,
        "attempt": 1,
        "due_ms": due.clone(),
        "fact_event_id": 7,
    })
}

#[test]
fn the_record_function_takes_an_interruption_and_a_clear_from_the_owner_only() {
    let (_store, schema) = store("recovery_records");
    let mut client = admin(&schema);
    // An interruption through the owner's function, with its fields.
    for (cause, path, due) in [
        ("crashed", "resume", json!(1_000)),
        ("context_exhausted", "handoff", json!(1_000)),
        ("model_unavailable", "wait", json!(2_000)),
        ("database_unavailable", "wait", json!(1_000)),
        ("disk_full", "wait", json!(1_000)),
        ("stopped", "final", Value::Null),
    ] {
        let id: i64 = client
            .query_one(
                "select spawn_host_record($1)",
                &[&interruption("exec_1", cause, path, &due)],
            )
            .expect("the owner records an interruption")
            .get(0);
        assert!(id > 0);
    }
    let written = records(&mut client, "execution.interrupted");
    assert_eq!(written.len(), 6, "{written:?}");
    assert_eq!(written[0]["cause"], "crashed");
    assert_eq!(written[0]["path"], "resume");
    assert_eq!(
        written[5]["due_ms"],
        Value::Null,
        "a final path is due never"
    );

    // The clear of a wait, with its fields.
    let cleared = json!({
        "type": "execution.cleared",
        "execution_id": "exec_1",
        "session_id": "session_worker_recovery",
        "cause": "model_unavailable",
        "evidence": {"answer_event_id": 71},
        "waited_ms": 4_000,
    });
    client
        .query_one("select spawn_host_record($1)", &[&cleared])
        .expect("the owner records a clear");
    assert_eq!(records(&mut client, "execution.cleared").len(), 1);

    // A restricted login writes none of it: these records are the owner's.
    // Logged in as the login, not `set role`, which needs a superuser.
    let mut restricted = common::as_login(&schema, "spawn_supervisor");
    let refused = restricted.query_one(
        "select spawn_host_record($1)",
        &[&interruption("exec_1", "crashed", "resume", &json!(1_000))],
    );
    assert!(refused.is_err(), "a restricted login records no host facts");
    assert_eq!(
        records(&mut client, "execution.interrupted").len(),
        6,
        "the refusal wrote nothing"
    );
}

#[test]
fn the_record_function_refuses_a_cause_or_a_path_it_does_not_know() {
    let (_store, schema) = store("recovery_refused");
    let mut client = admin(&schema);
    // A cause outside the vocabulary: the recovery could not act on it.
    let error = client
        .query_one(
            "select spawn_host_record($1)",
            &[&interruption(
                "exec_1",
                "went_wrong",
                "resume",
                &json!(1_000),
            )],
        )
        .expect_err("an unknown cause is refused");
    assert!(
        refused(&error).contains("not an interruption cause"),
        "{error}"
    );
    // So is a path.
    let error = client
        .query_one(
            "select spawn_host_record($1)",
            &[&interruption(
                "exec_1",
                "crashed",
                "retry-forever",
                &json!(1_000),
            )],
        )
        .expect_err("an unknown path is refused");
    assert!(refused(&error).contains("not a recovery path"), "{error}");
    // A revival with no due time, and a final one that claims one.
    assert!(
        client
            .query_one(
                "select spawn_host_record($1)",
                &[&interruption("exec_1", "crashed", "resume", &Value::Null)],
            )
            .is_err(),
        "a revival is due at due_ms"
    );
    assert!(
        client
            .query_one(
                "select spawn_host_record($1)",
                &[&interruption("exec_1", "stopped", "final", &json!(1_000))],
            )
            .is_err(),
        "a final path is due never"
    );
    // A clear names its execution, cause and waited_ms.
    let mut clear = json!({
        "type": "execution.cleared",
        "execution_id": "exec_1",
        "session_id": "session_worker_recovery",
        "cause": "disk_full",
        "evidence": {},
        "waited_ms": 1,
    });
    clear["waited_ms"] = Value::Null;
    assert!(
        client
            .query_one("select spawn_host_record($1)", &[&clear])
            .is_err(),
        "a clear names waited_ms"
    );
    assert!(
        records(&mut client, "execution.interrupted").is_empty(),
        "nothing refused was written"
    );
    assert!(records(&mut client, "execution.cleared").is_empty());
}

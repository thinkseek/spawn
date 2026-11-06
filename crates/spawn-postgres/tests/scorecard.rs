//! The scorecard's facts (`scorecard_facts`, `database_now_ms`) and the
//! records that make its measures exact: `task.cancelled` names who
//! cancelled (docs/design.md §15).
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, schema_url, store};
use spawn_postgres::{PostgresStore, role_url};

/// One control record at `at`, written straight into the log as the stream
/// writes it (the next `session_seq`).
fn record(client: &mut Client, at: i64, record: &Value) {
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             values ('agent_control', 'session_control', null, 'control', null,
                     jsonb_build_object('name', 'control.recorded',
                                        'detail', jsonb_build_object('record', $1::jsonb)),
                     (select coalesce(max(session_seq), 0) + 1 from spawn_events
                       where session_id = 'session_control'),
                     $2)",
            &[record, &at],
        )
        .expect("write a control record");
}

/// One command row.
#[allow(clippy::too_many_arguments)]
fn command(
    client: &mut Client,
    principal: &str,
    key: &str,
    action: &Value,
    succeeded: Option<bool>,
    error: Option<&str>,
    created: i64,
) {
    let state = if succeeded.is_some() {
        "finished"
    } else {
        "pending"
    };
    let finished = succeeded.map(|_| created + 1000);
    client
        .execute(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, error, created_at_ms, finished_at_ms)
             values ($1, $2, 'agent_resident_r1', 'session_resident_r1', $3, $4, $5, $6, $7, $8)",
            &[
                &principal, &key, action, &state, &succeeded, &error, &created, &finished,
            ],
        )
        .expect("write a command");
}

fn by_type<'a>(facts: &'a Value, kind: &str) -> Vec<&'a Value> {
    facts["control"]
        .as_array()
        .expect("control records")
        .iter()
        .filter(|record| record["type"] == kind)
        .collect()
}

/// A gated mission's task through assignment, three gate runs (one log with
/// several stage lines, one with `gate_ms`, one with no total), a person's
/// cancellation, a halt and resume, a row from before the stream, and a
/// landing at 8000.
fn control_history(client: &mut Client) {
    let log = "| == candidate: build\n| stage gate passed 120ms build\n\
               | stage gate rejected 300000ms total\n| more\n  | stage gate accepted 600000ms total  \n\
               | stage gate accepted 999ms total and more\n! the integrator's note";
    for (at, value) in [
        (
            1000,
            json!({"type": "mission.added", "id": 1, "text": "m", "origin": "human", "sources": []}),
        ),
        (
            1000,
            json!({"type": "mission.configured", "id": 1, "gate": "/g", "accept": "gate",
                      "budget": null, "objectives": null}),
        ),
        (
            1000,
            json!({"type": "mission.configured", "id": 2, "gate": null, "accept": "review",
                      "budget": null, "objectives": null}),
        ),
        (
            2000,
            json!({"type": "task.assigned", "id": 4, "mission_id": 1, "kind": "resident",
                      "agent_id": "agent_resident_r1", "session_id": "session_resident_r1",
                      "brief": "Objective 11", "created_by": "spawn-integrate", "objective": 11,
                      "model": "default"}),
        ),
        (
            3000,
            json!({"type": "task.reported", "id": 4, "report": "a long report", "commit": null}),
        ),
        (
            4000,
            json!({"type": "task.gated", "id": 4, "verdict": "accepted", "log": log}),
        ),
        (
            5000,
            json!({"type": "task.gated", "id": 4, "verdict": "accepted", "gate_ms": 1234,
                      "log": "| stage gate accepted 5ms total"}),
        ),
        (
            6000,
            json!({"type": "task.gated", "id": 4, "verdict": "conflict",
                      "log": "! the gate gave no verdict"}),
        ),
        (
            7000,
            json!({"type": "task.cancelled", "id": 4, "by": "human"}),
        ),
        (
            7000,
            json!({"type": "mission.halted", "id": 1, "reason": "r"}),
        ),
        (7000, json!({"type": "mission.resumed", "id": 1})),
        (
            7500,
            json!({"type": "legacy.imported", "table": "mission_tasks",
                      "row": {"id": 9, "mission_id": 1, "agent_id": "a9", "session_id": "s9",
                              "brief": "Objective 11", "state": "landed", "created_by": "human",
                              "created_at_ms": 100, "updated_at_ms": 200, "delivered_at_ms": 150,
                              "gate_log": "big", "report": "big"}}),
        ),
        (
            8000,
            json!({"type": "task.landed", "id": 4, "from": "a", "to": "b"}),
        ),
    ] {
        record(client, at, &value);
    }
}

#[test]
fn the_facts_project_each_record_to_what_the_measures_read() {
    let (store, schema) = store("scorecard_control");
    let mut client = admin(&schema);
    control_history(&mut client);
    let facts = store.scorecard_facts(8000).expect("the facts");
    let types: Vec<&str> = facts["control"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| record["type"].as_str().expect("a type"))
        .collect();
    assert_eq!(
        types,
        [
            "mission.added",
            "mission.configured",
            "mission.configured",
            "task.assigned",
            "task.gated",
            "task.gated",
            "task.gated",
            "task.cancelled",
            "mission.resumed",
            "legacy.imported",
        ],
        "records of no measure, and those at or after the end, are left out"
    );
    let configured = by_type(&facts, "mission.configured");
    assert_eq!(configured[0]["gate"], "/g");
    assert_eq!(configured[1].get("gate"), None, "no gate");
    assert_eq!(configured[1]["accept"], "review");
    let assigned = by_type(&facts, "task.assigned")[0];
    assert_eq!(assigned["seq"], 4);
    assert_eq!(assigned["at"], 2000);
    assert_eq!(assigned["mission_id"], 1);
    assert_eq!(assigned["objective"], 11);
    assert_eq!(assigned["created_by"], "spawn-integrate");
    assert_eq!(assigned["session_id"], "session_resident_r1");
    let md5: String = client
        .query_one("select md5('Objective 11')", &[])
        .expect("md5")
        .get(0);
    assert_eq!(assigned["brief"], md5.as_str(), "a brief is its md5");
    let gated = by_type(&facts, "task.gated");
    assert_eq!(
        gated[0]["stage_ms"], 600_000,
        "the last whole `stage gate ... total` line"
    );
    assert_eq!(
        gated[0].get("log"),
        None,
        "the log itself stays in the database"
    );
    assert_eq!(gated[1]["gate_ms"], 1234);
    assert_eq!(gated[1]["stage_ms"], 5);
    assert_eq!(gated[2].get("stage_ms"), None, "no stage line: no gate ran");
    assert_eq!(gated[2]["verdict"], "conflict");
    assert_eq!(by_type(&facts, "task.cancelled")[0]["by"], "human");
    let legacy = by_type(&facts, "legacy.imported")[0];
    assert_eq!(legacy["table"], "mission_tasks");
    assert_eq!(legacy["id"], 9);
    assert_eq!(legacy["mission_id"], 1);
    assert_eq!(legacy["state"], "landed");
    assert_eq!(legacy["created_at_ms"], 100);
    assert_eq!(legacy["updated_at_ms"], 200);
    assert_eq!(legacy["delivered_at_ms"], 150);
    assert_eq!(legacy["brief"], md5.as_str());
    assert_eq!(legacy.get("report"), None);
    // The end is excluded; a later read up to after it sees the landing.
    assert_eq!(
        by_type(&store.scorecard_facts(8001).expect("facts"), "task.landed").len(),
        1
    );
}

/// Commands of every kind a measure reads, and some it does not, created at
/// 100 to 1000.
fn command_history(client: &mut Client) {
    let input = json!({"type": "input", "text": "x"});
    let start = json!({"type": "start", "engine": "claude", "prompt": "Task #6"});
    let digest = json!({"type": "input", "text": "stalled",
                        "tags": ["session_resident_r1 stall-9-activity-1",
                                 "session_worker_i_12 report-12-0"]});
    let plain_digest = json!({"type": "input", "text": "reports", "tags": ["s report-12-0"]});
    command(client, "human", "board-1", &input, Some(true), None, 100);
    command(
        client,
        "spawn",
        "cli-stop-1",
        &json!({"type": "stop"}),
        Some(false),
        Some("no execution"),
        200,
    );
    command(
        client,
        "spawn-dispatch",
        "deliver-6-0",
        &start,
        Some(false),
        Some("command too long"),
        300,
    );
    command(
        client,
        "spawn-dispatch",
        "deliver-6-0-retry-1",
        &start,
        Some(true),
        None,
        400,
    );
    command(
        client,
        "spawn-dispatch",
        "dispatch-nudge-1",
        &input,
        Some(true),
        None,
        500,
    );
    command(
        client,
        "spawn-dispatch",
        "dispatch-resume-1",
        &input,
        Some(false),
        Some("lost"),
        600,
    );
    command(
        client,
        "spawn-dispatch",
        "digest-a",
        &digest,
        Some(true),
        None,
        700,
    );
    command(
        client,
        "spawn-dispatch",
        "digest-b",
        &plain_digest,
        Some(true),
        None,
        800,
    );
    command(
        client,
        "spawn_supervisor",
        "s-1",
        &input,
        Some(true),
        None,
        900,
    );
    command(
        client,
        "spawn-dispatch",
        "deliver-7-0",
        &start,
        None,
        None,
        950,
    );
    command(client, "human", "board-2", &input, Some(true), None, 1000);
}

#[test]
fn the_facts_hold_the_commands_a_measure_reads() {
    let (store, schema) = store("scorecard_commands");
    let mut client = admin(&schema);
    command_history(&mut client);
    let facts = store.scorecard_facts(1000).expect("the facts");
    let commands = facts["commands"].as_array().expect("commands");
    let keys: Vec<&str> = commands
        .iter()
        .map(|command| command["key"].as_str().expect("a key"))
        .collect();
    assert_eq!(
        keys,
        [
            "board-1",
            "cli-stop-1",
            "deliver-6-0",
            "deliver-6-0-retry-1",
            "dispatch-resume-1",
            "digest-a",
            "deliver-7-0",
        ]
    );
    let find = |key: &str| {
        commands
            .iter()
            .find(|command| command["key"] == key)
            .expect("the command")
    };
    assert_eq!(find("board-1")["principal"], "human");
    assert_eq!(find("board-1")["action"], "input");
    assert_eq!(find("board-1")["session_id"], "session_resident_r1");
    assert_eq!(find("board-1")["created_at_ms"], 100);
    assert_eq!(find("board-1")["finished_at_ms"], 1100);
    assert_eq!(find("board-1")["succeeded"], true);
    assert_eq!(find("cli-stop-1")["succeeded"], false);
    assert_eq!(
        find("cli-stop-1").get("error"),
        None,
        "only a failed delivery's error is read"
    );
    assert_eq!(find("deliver-6-0")["error"], "command too long");
    assert_eq!(find("deliver-6-0-retry-1").get("error"), None);
    assert_eq!(find("deliver-7-0").get("succeeded"), None, "still pending");
    assert_eq!(
        find("digest-a")["stalls"],
        json!(["session_resident_r1 stall-9-activity-1"]),
        "only the stall tags"
    );
}

#[test]
fn the_reader_login_reads_the_facts_and_the_databases_clock() {
    let (store, schema) = store("scorecard_reader");
    store
        .mission_add("Ship it", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let url = role_url(&schema_url(&schema), "spawn_reader").expect("the reader's URL");
    let reader = PostgresStore::connect(&url).expect("the reader connects");
    let now = reader.database_now_ms().expect("the clock");
    let host = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970")
            .as_millis(),
    )
    .expect("fits");
    assert!(
        (now - host).abs() < 600_000,
        "the database's clock is near this host's"
    );
    let facts = reader
        .scorecard_facts(now + 1)
        .expect("the reader reads the facts");
    assert_eq!(by_type(&facts, "mission.added").len(), 1);
}

#[test]
fn a_cancellation_names_who_cancelled() {
    let (store, schema) = store("scorecard_cancel_by");
    let mission = store
        .mission_add("Ship it", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let by_human = store
        .task_assign(mission, "ephemeral", "a", "first", "default")
        .expect("assign");
    store.task_cancel(by_human).expect("the owner cancels");
    let by_supervisor = store
        .task_assign(mission, "ephemeral", "b", "second", "default")
        .expect("assign");
    let supervisor = PostgresStore::connect(
        &role_url(&schema_url(&schema), "spawn_supervisor").expect("the supervisor's URL"),
    )
    .expect("the supervisor connects");
    supervisor
        .task_cancel(by_supervisor)
        .expect("the supervisor cancels");
    let closed_by_close = store
        .task_assign(mission, "ephemeral", "c", "third", "default")
        .expect("assign");
    store
        .mission_close(mission, "dropped", "enough")
        .expect("the owner drops the mission");

    // The integrator's: a verdict that sends back work of a closed mission.
    let gated = store
        .mission_add("Gated", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(gated, Some("/gate"), "gate", None, None)
        .expect("configure");
    let by_integrator = store
        .task_assign(gated, "ephemeral", "d", "fourth", "default")
        .expect("assign");
    store.task_delivered(by_integrator, 1).expect("delivered");
    assert!(
        store
            .task_reported(by_integrator, "done")
            .expect("reported")
    );
    store
        .mission_close(gated, "done", "done")
        .expect("the owner closes the mission");
    store
        .integrator_record(
            &json!({"type": "task.gated", "id": by_integrator, "verdict": "rejected",
                                   "revision": 0, "base": "b", "candidate": "c", "reason": "r",
                                   "note": "n", "log": ""}),
        )
        .expect("the integrator sends it back");

    let mut client = admin(&schema);
    let by = |client: &mut Client, task: i64| -> Vec<Option<String>> {
        client
            .query(
                "select kind -> 'detail' -> 'record' ->> 'by' from spawn_events
                  where session_id = 'session_control' and event_name = 'control.recorded'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
                    and kind -> 'detail' -> 'record' ->> 'id' = $1
                  order by session_seq",
                &[&task.to_string()],
            )
            .expect("read the cancellations")
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    assert_eq!(by(&mut client, by_human), [Some("human".to_string())]);
    assert_eq!(
        by(&mut client, by_supervisor),
        [Some("spawn_supervisor".to_string())]
    );
    assert_eq!(
        by(&mut client, closed_by_close),
        [Some("human".to_string())]
    );
    assert_eq!(
        by(&mut client, by_integrator),
        [Some("spawn-integrate".to_string())]
    );
    // The fold reads none of it: a replay gives the same tables.
    let before: Value = client
        .query_one(
            "select jsonb_agg(to_jsonb(t) order by id) from mission_tasks t",
            &[],
        )
        .expect("tasks")
        .get(0);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    let after: Value = client
        .query_one(
            "select jsonb_agg(to_jsonb(t) order by id) from mission_tasks t",
            &[],
        )
        .expect("tasks")
        .get(0);
    assert_eq!(before, after);
}

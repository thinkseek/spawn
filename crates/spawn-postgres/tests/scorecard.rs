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
    let finished = succeeded.map(|_| created + 50);
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
            // A report is kept as its time (objective 081).
            "task.reported",
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
    assert_eq!(find("board-1")["finished_at_ms"], 150);
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

/// GitHub #16 item 3: a fixed window's facts are those recorded before its
/// end. A command created inside the window that finishes after it reads as
/// unfinished there, whenever the window is asked for.
#[test]
fn a_command_finishing_after_the_window_leaves_its_facts_unchanged() {
    let (store, schema) = store("scorecard_fixed_window");
    let mut client = admin(&schema);
    let input = json!({"type": "input", "text": "go"});
    let start = json!({"type": "start", "engine": "claude", "prompt": "Task #6"});
    command(&mut client, "human", "board-1", &input, None, None, 100);
    command(
        &mut client,
        "spawn_supervisor",
        "s-1",
        &input,
        None,
        None,
        200,
    );
    command(
        &mut client,
        "spawn-dispatch",
        "deliver-6-0",
        &start,
        None,
        None,
        300,
    );
    let before = store.scorecard_facts(1000).expect("the facts");
    assert_eq!(
        before["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .map(|command| command["key"].as_str().expect("a key"))
            .collect::<Vec<_>>(),
        ["board-1", "deliver-6-0"]
    );
    // All three finish at 1500, after the window's end.
    client
        .execute(
            "update spawn_commands
                set state = 'finished', finished_at_ms = 1500,
                    succeeded = (idempotency_key = 'board-1'),
                    error = case when idempotency_key <> 'board-1' then 'lost' end",
            &[],
        )
        .expect("finish the commands");
    let after = store.scorecard_facts(1000).expect("the facts");
    assert_eq!(
        serde_json::to_string(&after).expect("json"),
        serde_json::to_string(&before).expect("json"),
        "the same window gives the same bytes"
    );
    // A window that ends after the results holds them.
    let later = store.scorecard_facts(1501).expect("the facts");
    let find = |key: &str| {
        later["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .find(|command| command["key"] == key)
            .expect("the command")
    };
    assert_eq!(find("board-1")["succeeded"], true);
    assert_eq!(find("board-1")["finished_at_ms"], 1500);
    assert_eq!(find("s-1")["succeeded"], false);
    assert_eq!(find("deliver-6-0")["error"], "lost");
    // The end is exclusive: a result at the end itself is after the window.
    assert_eq!(
        serde_json::to_string(&store.scorecard_facts(1500).expect("the facts")).expect("json"),
        serde_json::to_string(&before).expect("json")
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

/// Objective 061: the facts keep each owner judging (`source.moved`,
/// `source.push_refused`) with its gate's wall clock (`gate_ms`) and the
/// mission whose gate judged it (`mission`); one recorded before them names
/// neither, and the missions a landing moved are not its `mission`.
#[test]
fn the_facts_keep_the_owners_judgings_with_their_gate_time_and_mission() {
    let (store, schema) = store("scorecard_owner_judgings");
    let mut client = admin(&schema);
    for (at, value) in [
        (
            1000,
            json!({"type": "source.moved", "source": "/s", "from": "a", "to": "b", "request": 7,
                   "verdict": "accepted", "missions": [1, 2], "mission": 1, "gate_ms": 300_000}),
        ),
        (
            2000,
            json!({"type": "source.push_refused", "request": 8, "verdict": "rejected",
                   "reason": "r", "mission": 2, "gate_ms": 450_000}),
        ),
        (
            3000,
            json!({"type": "source.moved", "source": "/s", "from": "b", "to": "c", "request": 9,
                   "verdict": "accepted", "missions": [1, 2]}),
        ),
    ] {
        record(&mut client, at, &value);
    }
    let facts = store.scorecard_facts(4000).expect("the facts");
    let moved = by_type(&facts, "source.moved");
    assert_eq!(moved.len(), 2, "{facts}");
    assert_eq!(moved[0]["gate_ms"], 300_000);
    assert_eq!(moved[0]["mission"], 1);
    assert_eq!(
        moved[1].get("gate_ms"),
        None,
        "a landing from before names no gate time"
    );
    assert_eq!(moved[1].get("mission"), None);
    let refused = by_type(&facts, "source.push_refused");
    assert_eq!(refused[0]["gate_ms"], 450_000);
    assert_eq!(refused[0]["mission"], 2);
    assert_eq!(refused[0]["at"], 2000);
}

/// Objective 081: the scorecard's facts carry each `task.reported` (its time,
/// not its text) and each `deploy.finished` with its `ok`, a failed deploy
/// too, so the scorecard can tell how long a landing waited to go live.
#[test]
fn the_facts_carry_reports_and_deploys() {
    let (store, schema) = store("scorecard_report_to_live");
    let mut client = admin(&schema);
    for (at, value) in [
        (
            1000,
            json!({"type": "task.reported", "id": 4, "commit": "c", "report": "a long report"}),
        ),
        (
            2000,
            json!({"type": "deploy.finished", "ok": false, "head": "h", "by": "landing",
                   "reason": "the build failed"}),
        ),
        (
            3000,
            json!({"type": "deploy.finished", "ok": true, "head": "h", "by": "owner",
                   "reason": ""}),
        ),
        (
            5000,
            json!({"type": "deploy.finished", "ok": true, "head": "h", "by": "owner",
                   "reason": ""}),
        ),
    ] {
        record(&mut client, at, &value);
    }
    let facts = store.scorecard_facts(4000).expect("the facts");
    let reported = by_type(&facts, "task.reported");
    assert_eq!(reported.len(), 1, "{facts}");
    assert_eq!(reported[0]["id"], 4);
    assert_eq!(reported[0]["at"], 1000);
    assert_eq!(
        reported[0].get("report"),
        None,
        "a report's text is not read"
    );
    let deploys = by_type(&facts, "deploy.finished");
    assert_eq!(
        deploys.len(),
        2,
        "a deploy after `until` is not read: {facts}"
    );
    assert_eq!(deploys[0]["ok"], false);
    assert_eq!(deploys[0]["at"], 2000);
    assert_eq!(deploys[1]["ok"], true);
    assert_eq!(deploys[1]["at"], 3000);
}

/// The live record's start (objective 095's (2), the Core lines hand fix):
/// the first `deploy.finished` of any outcome, a failed one too; none
/// without one, and other records are not it.
#[test]
fn the_live_record_starts_at_the_first_deploy_of_any_outcome() {
    let (store, schema) = store("scorecard_first_deploy");
    let mut client = admin(&schema);
    assert_eq!(store.first_deploy_finished_ms().expect("read"), None);
    record(
        &mut client,
        1000,
        &json!({"type": "task.reported", "id": 4, "commit": "c"}),
    );
    assert_eq!(
        store.first_deploy_finished_ms().expect("read"),
        None,
        "a report is no deploy"
    );
    for (at, ok) in [(5000, true), (2000, false), (3000, true)] {
        record(
            &mut client,
            at,
            &json!({"type": "deploy.finished", "ok": ok, "head": "h", "by": "landing",
                   "reason": ""}),
        );
    }
    assert_eq!(
        store.first_deploy_finished_ms().expect("read"),
        Some(2000),
        "the earliest, a failed deploy too"
    );
}

/// Objective 095: the facts name where the live record begins, the first
/// `deploy.finished` of any outcome (a failed one too), read over the whole
/// log before their end: one recorded long before a 7-day window is found,
/// none after the end counts, and with no deploy record it is null.
#[test]
fn the_facts_find_the_live_records_start_before_the_window() {
    const DAY: i64 = 86_400_000;
    let (store, schema) = store("scorecard_live_from");
    let mut client = admin(&schema);
    let now = 100 * DAY;
    let empty = store.scorecard_facts(now).expect("the facts");
    assert_eq!(empty["live_from_ms"], Value::Null, "{empty}");
    for (at, value) in [
        // Eight days before `now`: before a 7-day window's start.
        (
            now - 8 * DAY,
            json!({"type": "deploy.finished", "ok": false, "head": "h", "by": "landing",
                   "reason": "the build failed"}),
        ),
        (
            now - 2 * DAY,
            json!({"type": "deploy.finished", "ok": true, "head": "h", "by": "landing",
                   "reason": ""}),
        ),
        (
            now - DAY,
            json!({"type": "task.reported", "id": 4, "commit": "c"}),
        ),
    ] {
        record(&mut client, at, &value);
    }
    let facts = store.scorecard_facts(now).expect("the facts");
    assert_eq!(facts["live_from_ms"], now - 8 * DAY, "{facts}");
    // The facts of an earlier end hold only what was recorded before it.
    let earlier = store.scorecard_facts(now - 5 * DAY).expect("the facts");
    assert_eq!(earlier["live_from_ms"], now - 8 * DAY);
    let before = store.scorecard_facts(now - 8 * DAY).expect("the facts");
    assert_eq!(before["live_from_ms"], Value::Null, "{before}");
}

/// FACTS as it was before the lead's cut of 2026-11-13 (7c4e8ab6), word for
/// word: the oracle the facts are compared with.
const OLD_FACTS: &str = r"
with h as materialized (
  select u.id, i.intent, i.at_ms
    from spawn_human_intents i
   cross join lateral unnest(i.commands) as u(id)
   where i.at_ms < $1)
select jsonb_build_object(
  'control', (
    select coalesce(jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
             'seq', e.session_seq,
             'at', e.recorded_at_ms,
             'type', x.r ->> 'type',
             'id', coalesce(x.r -> 'id', x.r -> 'row' -> 'id'),
             'mission_id', coalesce(x.r -> 'mission_id', x.r -> 'row' -> 'mission_id'),
             'agent_id', coalesce(x.r -> 'agent_id', x.r -> 'row' -> 'agent_id'),
             'session_id', coalesce(x.r -> 'session_id', x.r -> 'row' -> 'session_id'),
             'created_by', coalesce(x.r -> 'created_by', x.r -> 'row' -> 'created_by'),
             'objective', coalesce(x.r -> 'objective', x.r -> 'row' -> 'objective'),
             'brief', to_jsonb(md5(coalesce(x.r ->> 'brief', x.r -> 'row' ->> 'brief'))),
             'verdict', x.r -> 'verdict',
             'accepted', x.r -> 'accepted',
             'ok', x.r -> 'ok',
             'by', x.r -> 'by',
             'gate', coalesce(x.r -> 'gate', x.r -> 'row' -> 'gate'),
             'accept', coalesce(x.r -> 'accept', x.r -> 'row' -> 'accept'),
             'gate_ms', case when jsonb_typeof(x.r -> 'gate_ms') = 'number'
                             then x.r -> 'gate_ms' end,
             -- An owner judging's mission: the one whose gate judged it.
             'mission', case when jsonb_typeof(x.r -> 'mission') = 'number'
                             then x.r -> 'mission' end,
             'stage_ms', (select to_jsonb((s.m[1])::bigint)
                            from regexp_matches(x.r ->> 'log',
                                   '^[ \t]*\| stage gate [^ \t\n]+ ([0-9]{1,15})ms total[ \t\r]*$',
                                   'gn') with ordinality as s(m, i)
                           order by s.i desc limit 1),
             'table', x.r -> 'table',
             'state', x.r -> 'row' -> 'state',
             'created_at_ms', x.r -> 'row' -> 'created_at_ms',
             'updated_at_ms', x.r -> 'row' -> 'updated_at_ms',
             'delivered_at_ms', x.r -> 'row' -> 'delivered_at_ms'))
             order by e.session_seq), '[]'::jsonb)
      from spawn_events e
     -- `offset 0` keeps the record a value of its own: inlined, each of the
     -- thirty `x.r` above would take it out of the event's `kind` (a gate's
     -- log, a brief) again, which took most of the statement's time.
     cross join lateral (select e.kind -> 'detail' -> 'record' as r offset 0) x
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and e.recorded_at_ms < $1
       and x.r ->> 'type' in ('mission.added', 'mission.configured', 'mission.resumed',
                              'task.assigned', 'task.delivered', 'task.gated', 'task.landed',
                              'task.reviewed', 'task.cancelled', 'task.aborted',
                              'task.over_budget', 'legacy.imported',
                              'source.moved', 'source.push_refused',
                              'task.reported', 'deploy.finished')),
  'commands', (
    select coalesce(jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
             'id', c.id,
             'principal', c.principal,
             'key', c.idempotency_key,
             'session_id', c.session_id,
             'action', c.action ->> 'type',
             'created_at_ms', c.created_at_ms,
             'finished_at_ms', r.finished_at_ms,
             'succeeded', r.succeeded,
             'error', case when r.succeeded is false and c.principal = 'spawn-dispatch'
                                and c.idempotency_key like 'deliver-%' then c.error end,
             'stalls', t.stalls,
             'intent', h.intent,
             'intent_at_ms', h.at_ms))
             order by c.id), '[]'::jsonb)
      from spawn_commands c
     -- A command's result is a fact of the moment it finished: one that
     -- finished at or after `$1` is unfinished in these facts, so the same
     -- `$1` answers the same rows whenever it is asked.
     cross join lateral (
       select case when c.finished_at_ms < $1 then c.finished_at_ms end as finished_at_ms,
              case when c.finished_at_ms < $1 then c.succeeded end as succeeded) r
     cross join lateral (
       select case when c.principal = 'spawn-dispatch' and c.action ->> 'type' = 'input'
                        and jsonb_typeof(c.action -> 'tags') = 'array'
                   then (select jsonb_agg(g.tag order by g.n)
                           from jsonb_array_elements_text(c.action -> 'tags')
                                with ordinality as g(tag, n)
                          where g.tag ~ '(^| )stall-[0-9]+-[a-z_]+-[0-9]+$') end as stalls) t
     left join h on h.id = c.id
     where c.created_at_ms < $1
       and (c.principal in ('human', 'spawn')
            or r.succeeded is false
            or (c.principal = 'spawn-dispatch'
                and (c.idempotency_key like 'deliver-%' or t.stalls is not null)))),
  -- From when the owner's judgings are counted (objective 076): the
  -- earliest owner judging that carries its gate's wall clock, whatever
  -- `$1` is; null when none does.
  'owner_runs_from_ms', (
    select min(e.recorded_at_ms)
      from spawn_events e
     cross join lateral (select e.kind -> 'detail' -> 'record' as r offset 0) x
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and x.r ->> 'type' in ('source.moved', 'source.push_refused')
       and jsonb_typeof(x.r -> 'gate_ms') = 'number'),
  -- Where the live record begins (objective 095): the first
  -- `deploy.finished` of any outcome, over the whole log before `$1`
  -- (whatever the window); null when there is none.
  'live_from_ms', (
    select min(e.recorded_at_ms)
      from spawn_events e
     cross join lateral (select e.kind -> 'detail' -> 'record' as r offset 0) x
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and e.recorded_at_ms < $1
       and x.r ->> 'type' = 'deploy.finished'))";

/// The lead's cut (the control facts sorted as small rows, not with their
/// records; `live_from_ms` and `owner_runs_from_ms` taken from them, not
/// read twice more) gives the old statement's answer exactly: on a history
/// of every kind the facts read (gate logs with a stage line, owner
/// judgings with and without their gate time, deploys, commands), at ends
/// before everything, between the owner judgings (the earliest one after
/// the end: read over the whole log, as before), between the deploys and
/// after everything.
#[test]
fn the_facts_are_the_old_statements_answer() {
    let (store, schema) = store("scorecard_facts_oracle");
    let mut client = admin(&schema);
    // Before everything: a report (not a deploy) and a landing that names
    // no gate time (not an owner judging with one): neither is the start
    // of either.
    record(
        &mut client,
        100,
        &json!({"type": "task.reported", "id": 4, "commit": "c"}),
    );
    record(
        &mut client,
        200,
        &json!({"type": "source.moved", "source": "/s", "from": "x", "to": "a", "request": 6,
               "verdict": "accepted", "missions": [1]}),
    );
    control_history(&mut client);
    command_history(&mut client);
    for (at, value) in [
        (
            2500,
            json!({"type": "source.moved", "source": "/s", "from": "a", "to": "b", "request": 7,
                   "verdict": "accepted", "missions": [1], "mission": 1, "gate_ms": 300_000}),
        ),
        (
            2600,
            json!({"type": "source.push_refused", "request": 8, "verdict": "rejected",
                   "reason": "r", "mission": 2, "gate_ms": 450_000}),
        ),
        (
            2700,
            json!({"type": "source.moved", "source": "/s", "from": "b", "to": "c", "request": 9,
                   "verdict": "accepted", "missions": [1]}),
        ),
        (
            2800,
            json!({"type": "deploy.finished", "ok": false, "head": "h", "by": "landing",
                   "reason": "the build failed"}),
        ),
        (
            9000,
            json!({"type": "deploy.finished", "ok": true, "head": "h", "by": "owner",
                   "reason": ""}),
        ),
    ] {
        record(&mut client, at, &value);
    }
    for until in [
        0, 150, 250, 1000, 2000, 2550, 2650, 2750, 5000, 9001, 20_000,
    ] {
        let old: Value = client
            .query_one(OLD_FACTS, &[&until])
            .expect("the old facts")
            .get(0);
        let new = store.scorecard_facts(until).expect("the facts");
        assert_eq!(new, old, "until {until}");
    }
    let facts = store.scorecard_facts(2000).expect("the facts");
    assert_eq!(facts["owner_runs_from_ms"], 2500, "after the end: {facts}");
    assert_eq!(
        store.scorecard_facts(20_000).expect("facts")["live_from_ms"],
        2800
    );
}

/// A control record at `at` with the seq `seq` (a gap left before it, or a
/// late commit into one).
fn record_seq(client: &mut Client, at: i64, seq: i64, record: &Value) {
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             values ('agent_control', 'session_control', null, 'control', null,
                     jsonb_build_object('name', 'control.recorded',
                                        'detail', jsonb_build_object('record', $1::jsonb)),
                     $3, $2)",
            &[record, &at, &seq],
        )
        .expect("write a control record");
}

fn oracle(client: &mut Client, until: i64) -> Value {
    client
        .query_one(OLD_FACTS, &[&until])
        .expect("the old facts")
        .get(0)
}

/// A step of a random history: a control record of a kind the facts read,
/// a command of a kind they keep (or not), a pending command finishing (the
/// oldest, or one at random: far below the newest), or a record or command
/// stamped ahead of `now` (a database clock stepped back after it, so later
/// numbers carry earlier times).
fn random_step(client: &mut Client, state: &mut u64, now: i64, task: &mut i64) {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let pick = (*state >> 33) % 12;
    // Ahead by 20 or 21: on a later step's end (`now` or `now + 1`) exactly.
    let ahead = now + 20 + i64::from((*state >> 20) % 2 == 0);
    let log = "| == candidate\n| stage gate rejected 300000ms total\n| more";
    match pick {
        0 => {
            *task += 1;
            record(
                client,
                now,
                &json!({"type": "task.assigned", "id": *task, "mission_id": 1, "kind": "ephemeral",
                        "agent_id": format!("agent_w{task}"), "session_id": format!("session_w{task}"),
                        "brief": "b", "created_by": "spawn-dispatch", "objective": *task}),
            );
        }
        1 => record(
            client,
            now,
            &json!({"type": "task.gated", "id": *task, "verdict": "rejected",
                                          "revision": 0, "log": log}),
        ),
        2 => record(client, now, &json!({"type": "task.landed", "id": *task})),
        3 => record(
            client,
            now,
            &json!({"type": "deploy.finished", "ok": now % 2 == 0,
                                          "head": "h", "by": "landing", "reason": ""}),
        ),
        4 => record(
            client,
            now,
            &json!({"type": "source.moved", "source": "/s", "from": "a",
                                          "to": "b", "request": now, "verdict": "accepted",
                                          "missions": [1], "mission": 1, "gate_ms": 1000}),
        ),
        5 => {
            let principal = if now % 2 == 0 { "human" } else { "spawn" };
            command(
                client,
                principal,
                &format!("k{now}"),
                &json!({"type": "input", "text": "x"}),
                None,
                None,
                now,
            );
        }
        6 => command(
            client,
            "spawn-dispatch",
            &format!("deliver-{task}-0-{now}"),
            &json!({"type": "input", "text": "x"}),
            None,
            None,
            now,
        ),
        7 => command(
            client,
            "spawn-dispatch",
            &format!("other-{now}"),
            &json!({"type": "start", "engine": "claude", "prompt": "p"}),
            None,
            None,
            now,
        ),
        8 => record(
            client,
            ahead,
            &json!({"type": "task.landed", "id": *task}),
        ),
        9 => command(
            client,
            "human",
            &format!("ahead-{now}"),
            &json!({"type": "input", "text": "x"}),
            None,
            None,
            ahead,
        ),
        10 => {
            // A pending command at random finishes now, failed: most are far
            // below the newest id.
            let offset = i64::try_from((*state >> 40) % 64).expect("small");
            client
                .execute(
                    "update spawn_commands set state = 'finished', succeeded = false,
                            finished_at_ms = $1
                      where id = (select id from spawn_commands where state = 'pending'
                                   order by id offset ($2 % greatest(1, (select count(*)
                                     from spawn_commands where state = 'pending'))) limit 1)",
                    &[&now, &offset],
                )
                .expect("a command finishes");
        }
        _ => {
            // The oldest pending command finishes now: failed every other
            // time, which makes a dispatcher's "other" command a fact.
            client
                .execute(
                    "update spawn_commands set state = 'finished', succeeded = $1,
                            finished_at_ms = $2
                      where id = (select min(id) from spawn_commands where state = 'pending')",
                    &[&(now % 3 != 0), &now],
                )
                .expect("a command finishes");
        }
    }
}

/// The kept facts (design-facts-incremental.md): after each step of a
/// random history (records, commands made, commands finishing, either
/// stamped ahead), a reading at a later end (or the same end again) reads
/// only what is new and gives exactly the whole statement's answer, the
/// oracle's; now and then a reading at an earlier end, also the oracle's.
/// The overlap is 0 and 3 (rev-e's F1), so most rows are below the window
/// and only its time and finish arms find what changed. Three seeds each.
#[test]
fn the_kept_facts_are_the_whole_facts_after_every_step() {
    for (seed, overlap) in [1_u64, 7, 42].into_iter().flat_map(|s| [(s, 0), (s, 3)]) {
        let (store, schema) = store(&format!("scorecard_kept_{seed}_{overlap}"));
        let mut client = admin(&schema);
        let (mut state, mut task) = (seed, 0);
        record(
            &mut client,
            10,
            &json!({"type": "mission.added", "id": 1, "text": "m",
                                         "origin": "human", "sources": []}),
        );
        for step in 0..220_i64 {
            let now = 100 + step * 10;
            random_step(&mut client, &mut state, now, &mut task);
            let until = match step % 11 {
                5 => now - 35,
                _ if step % 7 == 3 => now,
                _ => now + 1,
            };
            assert_eq!(
                store
                    .scorecard_facts_with_overlap(until, overlap)
                    .expect("the facts"),
                oracle(&mut client, until),
                "seed {seed} overlap {overlap} step {step}"
            );
        }
    }
}

/// A record that commits after a kept reading with a seq below the kept
/// one's (a late commit), recorded before that reading's end, is in the
/// next reading while it is within the overlap; an earlier end reads in
/// full and leaves the kept facts as they were; two stores' facts are kept
/// apart.
#[test]
fn the_kept_facts_take_a_late_commit_an_earlier_end_and_keep_stores_apart() {
    let (other, other_schema) = store("scorecard_kept_other");
    let (kept, schema) = store("scorecard_kept_late");
    let mut client = admin(&schema);
    let mut other_client = admin(&other_schema);
    for (seq, at) in [(1, 100), (2, 200), (10, 300)] {
        record_seq(
            &mut client,
            at,
            seq,
            &json!({"type": "deploy.finished", "ok": true,
                                                  "head": "h", "by": "landing", "reason": ""}),
        );
    }
    record(
        &mut other_client,
        150,
        &json!({"type": "task.landed", "id": 9}),
    );
    assert_eq!(
        kept.scorecard_facts(400).expect("facts"),
        oracle(&mut client, 400)
    );
    assert_eq!(
        other.scorecard_facts(400).expect("facts"),
        oracle(&mut other_client, 400)
    );
    // Seq 5, recorded at 250 (before the kept end), committed only now.
    record_seq(
        &mut client,
        250,
        5,
        &json!({"type": "task.landed", "id": 4}),
    );
    let late = kept.scorecard_facts(500).expect("facts");
    assert_eq!(late, oracle(&mut client, 500));
    assert_eq!(
        by_type(&late, "task.landed").len(),
        1,
        "the late commit: {late}"
    );
    // An earlier end: in full, and the kept facts untouched by it.
    assert_eq!(
        kept.scorecard_facts(250).expect("facts"),
        oracle(&mut client, 250)
    );
    record(&mut client, 600, &json!({"type": "task.landed", "id": 6}));
    assert_eq!(
        kept.scorecard_facts(700).expect("facts"),
        oracle(&mut client, 700)
    );
    assert_eq!(
        other.scorecard_facts(700).expect("facts"),
        oracle(&mut other_client, 700)
    );
    // A command more than the overlap below the last one finishes failed:
    // only now is it a fact, found by its finish, not by its id.
    for n in 0..300_i64 {
        command(
            &mut client,
            "spawn-dispatch",
            &format!("other-{n}"),
            &json!({"type": "start", "engine": "claude", "prompt": "p"}),
            None,
            None,
            800 + n,
        );
    }
    // A person's command after them: a fact with the highest id, so the
    // kept reading's id window starts above every one of the 300.
    command(
        &mut client,
        "human",
        "after-the-300",
        &json!({"type": "input", "text": "x"}),
        None,
        None,
        1150,
    );
    assert_eq!(
        kept.scorecard_facts(1200).expect("facts"),
        oracle(&mut client, 1200)
    );
    client
        .execute(
            "update spawn_commands set state = 'finished', succeeded = false, finished_at_ms = 1300
              where idempotency_key = 'other-0'",
            &[],
        )
        .expect("the oldest fails");
    let finished = kept.scorecard_facts(1400).expect("facts");
    assert_eq!(finished, oracle(&mut client, 1400));
    assert_eq!(
        finished["commands"].as_array().map(Vec::len),
        Some(2),
        "the person's and the failed one: {finished}"
    );
}

/// A person's command written straight into the commands table with the id
/// `id` (a gap's id, as a commit later than its number would leave it).
fn command_with_id(client: &mut Client, id: i64, key: &str, created: i64) {
    client
        .execute(
            "insert into spawn_commands (id, principal, idempotency_key, agent_id, session_id,
                                         action, state, created_at_ms)
             values ($1, 'human', $2, 'agent_resident_r1', 'session_resident_r1',
                     '{\"type\": \"input\", \"text\": \"x\"}'::jsonb, 'pending', $3)",
            &[&id, &key, &created],
        )
        .expect("write a command with its id");
}

/// A row no window finds (a command committed below the kept id, recorded
/// before the kept end, unfinished) is missing from kept readings for at
/// most an hour of ends: the first reading an hour after the whole one
/// reads them all again. The clock is the readings' ends, passed in.
#[test]
fn kept_facts_are_read_whole_again_after_an_hour() {
    let (store, schema) = store("scorecard_kept_hour");
    let mut client = admin(&schema);
    record(&mut client, 10, &json!({"type": "task.landed", "id": 1}));
    for n in 0..6_i64 {
        command(
            &mut client,
            "human",
            &format!("p{n}"),
            &json!({"type": "input", "text": "x"}),
            None,
            None,
            500 + n,
        );
    }
    let first: i64 = client
        .query_one("select min(id) from spawn_commands", &[])
        .expect("the first id")
        .get(0);
    client
        .execute("delete from spawn_commands where id = $1", &[&first])
        .expect("leave a gap");
    let start = 1_000_i64;
    assert_eq!(
        store
            .scorecard_facts_with_overlap(start, 0)
            .expect("facts"),
        oracle(&mut client, start)
    );
    command_with_id(&mut client, first, "late", 600);
    let within = start + 3_600_000 - 1;
    let missed = store
        .scorecard_facts_with_overlap(within, 0)
        .expect("facts");
    assert_eq!(
        missed["commands"].as_array().map(Vec::len),
        Some(5),
        "the late command is not seen within the hour: {missed}"
    );
    let after = start + 3_600_000;
    let whole = store
        .scorecard_facts_with_overlap(after, 0)
        .expect("facts");
    assert_eq!(whole, oracle(&mut client, after));
    assert_eq!(whole["commands"].as_array().map(Vec::len), Some(6));
}

/// A store restored in place to an earlier point (the same URL, database
/// and schema): when the last kept control record is gone, or its seq now
/// holds another record, the next reading drops the kept facts and reads
/// them whole, so no fact of the lost records stays (rev-e's F2). Overlap
/// 0, so a window alone would keep them.
#[test]
fn a_store_restored_in_place_leaves_no_kept_fact_behind() {
    for replaced in [false, true] {
        let (store, schema) = store(&format!("scorecard_kept_restored_{replaced}"));
        let mut client = admin(&schema);
        for n in 1..=5_i64 {
            record(&mut client, n * 100, &json!({"type": "task.landed", "id": n}));
        }
        assert_eq!(
            store.scorecard_facts_with_overlap(1_000, 0).expect("facts"),
            oracle(&mut client, 1_000)
        );
        // The restore: seqs 4 and 5 lost; seq 4 written again as another
        // record, and (replaced) seq 5 too, at seq 5's old time.
        client
            .execute(
                "delete from spawn_events where session_id = 'session_control'
                   and session_seq >= 4",
                &[],
            )
            .expect("restore to an earlier point");
        record_seq(&mut client, 450, 4, &json!({"type": "task.landed", "id": 44}));
        if replaced {
            record_seq(&mut client, 500, 5, &json!({"type": "task.landed", "id": 55}));
        }
        let read = store.scorecard_facts_with_overlap(1_100, 0).expect("facts");
        assert_eq!(read, oracle(&mut client, 1_100), "replaced {replaced}");
        assert!(
            by_type(&read, "task.landed")
                .iter()
                .all(|fact| fact["id"] != 4 && fact["id"] != 5),
            "no lost record's fact stays: {read}"
        );
    }
}

/// A kept reading's statement reads what is new, not the log: the buffers
/// of a steady reading (the last 256 seqs again, nothing else new) over a
/// control log four times as long are within 10% of the shorter one's,
/// under a generic plan. Buffers, not rows (the interventions lesson: an
/// index walked on a column that does not lead shows in no row count).
#[test]
fn a_kept_reading_costs_what_is_new_not_the_log() {
    let mut buffers = Vec::new();
    for records in [2_000_i64, 8_000] {
        let (_store, schema) = store(&format!("scorecard_kept_growth_{records}"));
        let mut client = admin(&schema);
        client
            .execute(
                "insert into spawn_events (agent_id, session_id, execution_id, source,
                                           source_event_id, kind, session_seq, recorded_at_ms)
                 select 'agent_control', 'session_control', null, 'control', null,
                        jsonb_build_object('name', 'control.recorded', 'detail',
                          jsonb_build_object('record', jsonb_build_object(
                            'type', 'task.gated', 'id', n, 'verdict', 'rejected',
                            'log', repeat('| a line of a gate log\n', 40)))),
                        n, n * 10
                   from generate_series(1, $1::bigint) n",
                &[&records],
            )
            .expect("a control log");
        client
            .batch_execute("analyze spawn_events")
            .expect("analyze");
        client
            .batch_execute("set plan_cache_mode = force_generic_plan")
            .expect("generic plans");
        let end = records * 10;
        let statement = format!(
            "explain (analyze, buffers, format json) {}",
            PostgresStore::scorecard_facts_since_statement()
        );
        let plan: Value = client
            .query_one(
                &statement,
                &[&(end + 1), &(end - 2_000), &(records - 256), &0_i64],
            )
            .expect("the plan")
            .get(0);
        let root = &plan[0]["Plan"];
        buffers.push(
            root["Shared Hit Blocks"].as_i64().unwrap_or(0)
                + root["Shared Read Blocks"].as_i64().unwrap_or(0),
        );
    }
    assert!(
        buffers[1] <= buffers[0] + buffers[0] / 10,
        "4 times the log read {} buffers, the shorter {}",
        buffers[1],
        buffers[0]
    );
}

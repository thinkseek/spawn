//! The experience views (`spawn_decisions`, `spawn_interventions`,
//! `spawn_outcomes`, `spawn_escalations`; objective 043, docs/fleet-
//! architecture.md §4), the window they answer (`experience_window`) and
//! `/api/fleet`'s ladder, which reads the same view.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, as_login, store};

const H: i64 = 3_600_000;

/// One control record `hours` after 0, written straight into the log.
fn record(client: &mut Client, hours: i64, record: &Value) {
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
            &[record, &(hours * H)],
        )
        .expect("write a control record");
}

fn command(client: &mut Client, principal: &str, key: &str, session: &str, action: &Value, hours: i64) {
    client
        .execute(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, created_at_ms, finished_at_ms)
             values ($1, $2, 'agent_x', $3, $4, 'finished', true, $5::bigint, $5::bigint + 1000)",
            &[&principal, &key, &session, action, &(hours * H)],
        )
        .expect("write a command");
}

fn judgement(client: &mut Client, hours: i64, observation: &Value) {
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             values ('agent_w', 'session_w', null, 'judge', null,
                     jsonb_build_object('name', 'observation.recorded',
                                        'detail', jsonb_build_object('observation', $1::jsonb)),
                     (select coalesce(max(session_seq), 0) + 1 from spawn_events
                       where session_id = 'session_w'),
                     $2)",
            &[observation, &(hours * H)],
        )
        .expect("write a judgement");
}

fn assign(client: &mut Client, hours: i64, id: i64, mission: i64, by: &str, objective: Option<i64>, brief: &str) {
    record(
        client,
        hours,
        &json!({"type": "task.assigned", "id": id, "mission_id": mission, "kind": "ephemeral",
                "agent_id": format!("agent_worker_w_{id}"), "session_id": format!("session_worker_w_{id}"),
                "brief": brief, "created_by": by, "objective": objective, "model": "default"}),
    );
}

/// `<value>:<n>` of `column` in `view` over `[since, until)` hours, by value.
fn counts(client: &mut Client, view: &str, column: &str, since: i64, until: i64) -> String {
    client
        .query_one(
            &format!(
                "select coalesce(string_agg(v || ':' || n, ' ' order by v), '') from (
                   select {column}::text as v, count(*) as n from {view}
                    where at_ms >= $1 and at_ms < $2 group by 1) t"
            ),
            &[&(since * H), &(until * H)],
        )
        .expect("count a view")
        .get(0)
}

/// Mission 1 without a gate, mission 2 gated and accepted by its gate,
/// mission 3 closed: every kind of intervention and outcome, and the
/// records that are none.
fn history(client: &mut Client) {
    record(client, 0, &json!({"type": "mission.added", "id": 1, "text": "open", "origin": "human", "sources": []}));
    record(client, 0, &json!({"type": "mission.added", "id": 2, "text": "gated", "origin": "human", "sources": []}));
    record(client, 0, &json!({"type": "mission.configured", "id": 2, "gate": "scripts/gate.sh", "accept": "gate"}));
    record(client, 0, &json!({"type": "mission.added", "id": 3, "text": "short", "origin": "human", "sources": []}));
    // Mission 1: a person's task (an intervention: review accepts here),
    // accepted; a task cancelled by a person then the same objective again
    // (cancel and reassign), landed, and a landing after that changes
    // nothing; a task the integrator cancelled, then its brief again (no
    // intervention); one over budget.
    assign(client, 1, 1, 1, "human", None, "one");
    record(client, 2, &json!({"type": "task.reviewed", "id": 1, "accepted": true}));
    assign(client, 3, 2, 1, "spawn_supervisor", Some(9), "two");
    record(client, 4, &json!({"type": "task.cancelled", "id": 2, "by": "human"}));
    record(client, 4, &json!({"type": "task.landed", "id": 2, "from": "a", "to": "b"}));
    assign(client, 5, 3, 1, "spawn_supervisor", Some(9), "two, again");
    record(client, 6, &json!({"type": "task.landed", "id": 3, "from": "a", "to": "b"}));
    record(client, 7, &json!({"type": "task.landed", "id": 3, "from": "b", "to": "c"}));
    assign(client, 8, 4, 1, "spawn_supervisor", None, "four");
    record(client, 9, &json!({"type": "task.cancelled", "id": 4, "by": "spawn-integrate"}));
    assign(client, 10, 5, 1, "spawn_supervisor", None, "four");
    record(client, 11, &json!({"type": "task.over_budget", "id": 5, "reason": "big"}));
    // Mission 2: a person's task on a gated mission (no intervention), its
    // review accepted (not its close: the gate lands it), then aborted.
    assign(client, 12, 6, 2, "human", None, "six");
    record(client, 13, &json!({"type": "task.reviewed", "id": 6, "accepted": true}));
    record(client, 14, &json!({"type": "task.aborted", "id": 6, "reason": "host"}));
    record(client, 15, &json!({"type": "mission.resumed", "id": 2}));
    // Mission 3: open 0-20, a task 16-18.
    assign(client, 16, 7, 3, "spawn_supervisor", None, "seven");
    record(client, 18, &json!({"type": "task.landed", "id": 7, "from": "a", "to": "b"}));
    record(client, 20, &json!({"type": "mission.closed", "id": 3, "state": "done", "summary": "ok"}));

    judgement(client, 1, &json!({"state": "WORKING", "judge": "rules"}));
    judgement(client, 21, &json!({"state": "WORKING", "judge": "jev", "latency_ms": 400, "rules_state": "WAITING"}));
    judgement(client, 22, &json!({"state": "WORKING", "judge": "jev", "latency_ms": 200}));
    judgement(client, 23, &json!({"state": "WORKING", "judge": "rules", "reason": "JUDGE_UNAVAILABLE"}));

    let tags = |tags: &[&str]| json!({"type": "input", "text": "items", "tags": tags});
    command(client, "spawn-dispatch", "digest-1", "session_supervisor", &tags(&["session_w a-1", "charter 0abc"]), 2);
    command(client, "spawn-dispatch", "digest-2", "session_supervisor", &tags(&["session_w a-1", "session_w b-1"]), 21);
    // Tags into a worker are no escalation.
    command(client, "spawn-dispatch", "nudge-1", "session_w", &tags(&["session_w c-1"]), 21);
    command(client, "spawn_supervisor", "sup-1", "session_w", &json!({"type": "input", "text": "go"}), 22);
    command(client, "human", "board-1", "session_w", &json!({"type": "input", "text": "note"}), 3);
    command(client, "spawn", "cli-1", "session_w", &json!({"type": "input", "text": "note"}), 22);
    command(client, "spawn-liveness", "live-1", "session_supervisor", &json!({"type": "resume"}), 23);
    command(client, "owner_gate", "gate-1", "session_w", &json!({"type": "input", "text": "landed"}), 23);
}

#[test]
fn the_views_derive_decisions_interventions_outcomes_and_escalations_from_the_log() {
    let (_store, schema) = store("experience_views");
    let mut client = admin(&schema);
    history(&mut client);

    assert_eq!(
        counts(&mut client, "spawn_decisions", "tier", 0, 24),
        "dispatcher:3 human:2 liveness:1 model:2 owner_gate:1 rules:2 supervisor:1"
    );
    assert_eq!(
        counts(&mut client, "spawn_decisions", "tier", 20, 24),
        "dispatcher:2 human:1 liveness:1 model:2 owner_gate:1 rules:1 supervisor:1"
    );
    assert_eq!(
        counts(&mut client, "spawn_interventions", "kind", 0, 24),
        "cancel_reassign:1 command:2 owner_task:1 resume:1",
        "the integrator's cancellation and the gated mission's person's task are none"
    );
    assert_eq!(
        counts(&mut client, "spawn_interventions", "kind", 4, 6),
        "cancel_reassign:1",
        "counted at the assignment that does the work again"
    );
    assert_eq!(
        counts(&mut client, "spawn_outcomes", "outcome", 0, 24),
        "aborted:1 accepted:1 cancelled:2 landed:2 over_budget:1",
        "one close per task: no second landing, no gated review"
    );
    let landed: String = client
        .query_one(
            "select string_agg(task_id || '@' || (at_ms / 3600000), ' ' order by task_id)
               from spawn_outcomes where outcome = 'landed'",
            &[],
        )
        .expect("landings")
        .get(0);
    assert_eq!(landed, "3@6 7@18");
    assert_eq!(counts(&mut client, "spawn_escalations", "'all'", 0, 24), "all:2");
    assert_eq!(
        counts(&mut client, "spawn_escalations", "tag", 20, 24),
        "session_w b-1:1",
        "a tag counts once, at the first digest that carried it"
    );
}

#[test]
fn the_window_counts_the_views_and_each_missions_open_span() {
    let (store, schema) = store("experience_window");
    let mut client = admin(&schema);
    history(&mut client);
    client
        .batch_execute(&format!(
            "insert into spawn_events (agent_id, session_id, execution_id, source, kind, session_seq, recorded_at_ms)
             values ('agent_w', 'session_w', 'exec_a', 'bridge',
                     '{{\"name\":\"execution.started\",\"detail\":{{\"engine\":\"claude\",\"workspace\":\"/w\"}}}}', 100, {}),
                    ('agent_w', 'session_w', 'exec_a', 'bridge',
                     '{{\"name\":\"execution.ended\",\"detail\":{{\"cause\":\"exited\",\"exit_code\":0}}}}', 101, {}),
                    ('agent_w', 'session_w', 'exec_b', 'bridge',
                     '{{\"name\":\"execution.started\",\"detail\":{{\"engine\":\"claude\",\"workspace\":\"/w\"}}}}', 102, {})",
            10 * H,
            22 * H,
            23 * H
        ))
        .expect("executions");

    let window = store.experience_window(12 * H, 24 * H).expect("the window reads");
    assert_eq!(window["agent_ms"], json!(11 * H), "12-22, and 23 to the window's end");
    assert_eq!(window["ladder"]["human"], 1);
    assert_eq!(window["ladder"]["owner_gate"], 1);
    assert_eq!(window["interventions"], json!({"command": 1, "resume": 1}));
    assert_eq!(window["outcomes"], json!({"aborted": 1, "landed": 1}));
    assert_eq!(window["escalations"], 1);
    assert_eq!(window["judgements"]["total"], 3);
    assert_eq!(window["judgements"]["by_judge"], json!({"jev": 2, "rules": 1}));
    assert_eq!(window["judgements"]["judge_unavailable"], 1);
    assert_eq!(window["judgements"]["disagreements"], 1);
    assert_eq!(window["judgements"]["model_latency_p50_ms"].as_f64(), Some(300.0));
    let missions: Vec<(i64, Option<i64>, Value)> = window["missions"]
        .as_array()
        .expect("missions")
        .iter()
        .map(|m| (m["mission"].as_i64().expect("id"), m["closed_at_ms"].as_i64(), m["tasks"].clone()))
        .collect();
    assert_eq!(
        missions,
        vec![
            (1, None, json!([[H, 2 * H], [3 * H, 4 * H], [5 * H, 6 * H], [8 * H, 9 * H], [10 * H, 11 * H]])),
            (2, None, json!([[12 * H, 14 * H]])),
            (3, Some(20 * H), json!([[16 * H, 18 * H]])),
        ]
    );

    // As of an earlier end: a close at or after it is not one yet.
    let before = store.experience_window(0, 17 * H).expect("the window reads");
    assert_eq!(before["missions"][2]["tasks"], json!([[16 * H, null]]));
    assert_eq!(before["missions"][2]["closed_at_ms"], Value::Null);
}

#[test]
fn the_reader_and_the_supervisor_read_the_views() {
    let (_store, schema) = store("experience_grants");
    let mut client = admin(&schema);
    history(&mut client);
    for login in ["spawn_reader", "spawn_supervisor"] {
        let mut reader = as_login(&schema, login);
        for view in ["spawn_decisions", "spawn_interventions", "spawn_outcomes", "spawn_escalations"] {
            let n: i64 = reader
                .query_one(&format!("select count(*) from {view}"), &[])
                .unwrap_or_else(|error| panic!("{login} reads {view}: {error}"))
                .get(0);
            assert!(n > 0, "{login} sees the rows of {view}");
        }
    }
}

#[test]
fn the_fleets_ladder_reads_the_decisions_view() {
    let (store, schema) = store("experience_health");
    let mut client = admin(&schema);
    let now: i64 = client.query_one("select spawn_now_ms()", &[]).expect("now").get(0);
    let hours_ago = |h: i64| (now - h * H) / H;
    // Times relative to now, rounded to the hour below: within the day.
    command(&mut client, "human", "board-1", "session_w", &json!({"type": "input", "text": "a"}), hours_ago(2));
    command(&mut client, "spawn", "cli-1", "session_w", &json!({"type": "input", "text": "b"}), hours_ago(3));
    command(&mut client, "owner_gate", "gate-1", "session_w", &json!({"type": "input", "text": "c"}), hours_ago(4));
    command(&mut client, "human", "board-0", "session_w", &json!({"type": "input", "text": "old"}), hours_ago(30));
    let health = store.fleet_health().expect("the health reads");
    assert_eq!(health["ladder_24h"]["human"], 2, "the board's and the CLI's, in the day");
    assert_eq!(health["ladder_24h"]["owner_gate"], 1, "another principal as its own tier");
    assert_eq!(health["ladder_24h"]["rules"], 0);
    assert_eq!(health["human_per_agent_hour"], Value::Null, "no agent hours");
}

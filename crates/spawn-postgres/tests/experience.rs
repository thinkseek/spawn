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

fn command(
    client: &mut Client,
    principal: &str,
    key: &str,
    session: &str,
    action: &Value,
    hours: i64,
) {
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

fn assign(
    client: &mut Client,
    hours: i64,
    id: i64,
    mission: i64,
    by: &str,
    objective: Option<i64>,
    brief: &str,
) {
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
    record(
        client,
        0,
        &json!({"type": "mission.added", "id": 1, "text": "open", "origin": "human", "sources": []}),
    );
    record(
        client,
        0,
        &json!({"type": "mission.added", "id": 2, "text": "gated", "origin": "human", "sources": []}),
    );
    record(
        client,
        0,
        &json!({"type": "mission.configured", "id": 2, "gate": "scripts/gate.sh", "accept": "gate"}),
    );
    record(
        client,
        0,
        &json!({"type": "mission.added", "id": 3, "text": "short", "origin": "human", "sources": []}),
    );
    // Mission 1: a person's task (an intervention: review accepts here),
    // accepted; a task cancelled by a person then the same objective again
    // (cancel and reassign), landed, and a landing after that changes
    // nothing; a task the integrator cancelled, then its brief again (no
    // intervention); one over budget.
    assign(client, 1, 1, 1, "human", None, "one");
    record(
        client,
        2,
        &json!({"type": "task.reviewed", "id": 1, "accepted": true}),
    );
    assign(client, 3, 2, 1, "spawn_supervisor", Some(9), "two");
    record(
        client,
        4,
        &json!({"type": "task.cancelled", "id": 2, "by": "human"}),
    );
    record(
        client,
        4,
        &json!({"type": "task.landed", "id": 2, "from": "a", "to": "b"}),
    );
    assign(client, 5, 3, 1, "spawn_supervisor", Some(9), "two, again");
    record(
        client,
        6,
        &json!({"type": "task.landed", "id": 3, "from": "a", "to": "b"}),
    );
    record(
        client,
        7,
        &json!({"type": "task.landed", "id": 3, "from": "b", "to": "c"}),
    );
    assign(client, 8, 4, 1, "spawn_supervisor", None, "four");
    record(
        client,
        9,
        &json!({"type": "task.cancelled", "id": 4, "by": "spawn-integrate"}),
    );
    assign(client, 10, 5, 1, "spawn_supervisor", None, "four");
    record(
        client,
        11,
        &json!({"type": "task.over_budget", "id": 5, "reason": "big"}),
    );
    // Mission 2: a person's task on a gated mission (no intervention), its
    // review accepted (not its close: the gate lands it), then aborted.
    assign(client, 12, 6, 2, "human", None, "six");
    record(
        client,
        13,
        &json!({"type": "task.reviewed", "id": 6, "accepted": true}),
    );
    record(
        client,
        14,
        &json!({"type": "task.aborted", "id": 6, "reason": "host"}),
    );
    record(client, 15, &json!({"type": "mission.resumed", "id": 2}));
    // Mission 3: open 0-20, a task 16-18.
    assign(client, 16, 7, 3, "spawn_supervisor", None, "seven");
    record(
        client,
        18,
        &json!({"type": "task.landed", "id": 7, "from": "a", "to": "b"}),
    );
    record(
        client,
        20,
        &json!({"type": "mission.closed", "id": 3, "state": "done", "summary": "ok"}),
    );

    decisions(client);
}

/// Judgements, digests into the supervisor (one tag twice, a charter's tag)
/// and commands of every principal.
fn decisions(client: &mut Client) {
    judgement(client, 1, &json!({"state": "WORKING", "judge": "rules"}));
    judgement(
        client,
        21,
        &json!({"state": "WORKING", "judge": "jev", "latency_ms": 400, "rules_state": "WAITING"}),
    );
    judgement(
        client,
        22,
        &json!({"state": "WORKING", "judge": "jev", "latency_ms": 200}),
    );
    judgement(
        client,
        23,
        &json!({"state": "WORKING", "judge": "rules", "reason": "JUDGE_UNAVAILABLE"}),
    );

    let tags = |tags: &[&str]| json!({"type": "input", "text": "items", "tags": tags});
    command(
        client,
        "spawn-dispatch",
        "digest-1",
        "session_supervisor",
        &tags(&["session_w a-1", "charter 0abc"]),
        2,
    );
    command(
        client,
        "spawn-dispatch",
        "digest-2",
        "session_supervisor",
        &tags(&["session_w a-1", "session_w b-1"]),
        21,
    );
    // Tags into a worker are no escalation.
    command(
        client,
        "spawn-dispatch",
        "nudge-1",
        "session_w",
        &tags(&["session_w c-1"]),
        21,
    );
    command(
        client,
        "spawn_supervisor",
        "sup-1",
        "session_w",
        &json!({"type": "input", "text": "go"}),
        22,
    );
    command(
        client,
        "human",
        "board-1",
        "session_w",
        &json!({"type": "input", "text": "note"}),
        3,
    );
    command(
        client,
        "spawn",
        "cli-1",
        "session_w",
        &json!({"type": "input", "text": "note"}),
        22,
    );
    command(
        client,
        "spawn-liveness",
        "live-1",
        "session_supervisor",
        &json!({"type": "resume"}),
        23,
    );
    command(
        client,
        "owner_gate",
        "gate-1",
        "session_w",
        &json!({"type": "input", "text": "landed"}),
        23,
    );
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
    assert_eq!(
        counts(&mut client, "spawn_escalations", "'all'", 0, 24),
        "all:2"
    );
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

    let window = store
        .experience_window(12 * H, 24 * H)
        .expect("the window reads");
    assert_eq!(
        window["agent_ms"],
        json!(11 * H),
        "12-22, and 23 to the window's end"
    );
    assert_eq!(window["ladder"]["human"], 1);
    assert_eq!(window["ladder"]["owner_gate"], 1);
    assert_eq!(window["interventions"], json!({"command": 1, "resume": 1}));
    assert_eq!(window["outcomes"], json!({"aborted": 1, "landed": 1}));
    assert_eq!(window["escalations"], 1);
    assert_eq!(window["judgements"]["total"], 3);
    assert_eq!(
        window["judgements"]["by_judge"],
        json!({"jev": 2, "rules": 1})
    );
    assert_eq!(window["judgements"]["judge_unavailable"], 1);
    assert_eq!(window["judgements"]["disagreements"], 1);
    assert_eq!(
        window["judgements"]["model_latency_p50_ms"].as_f64(),
        Some(300.0)
    );
    let missions: Vec<(i64, Option<i64>, Value)> = window["missions"]
        .as_array()
        .expect("missions")
        .iter()
        .map(|m| {
            (
                m["mission"].as_i64().expect("id"),
                m["closed_at_ms"].as_i64(),
                m["tasks"].clone(),
            )
        })
        .collect();
    assert_eq!(
        missions,
        vec![
            (
                1,
                None,
                json!([
                    [H, 2 * H],
                    [3 * H, 4 * H],
                    [5 * H, 6 * H],
                    [8 * H, 9 * H],
                    [10 * H, 11 * H]
                ])
            ),
            (2, None, json!([[12 * H, 14 * H]])),
            (3, Some(20 * H), json!([[16 * H, 18 * H]])),
        ]
    );

    // As of an earlier end: a close at or after it is not one yet.
    let before = store
        .experience_window(0, 17 * H)
        .expect("the window reads");
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
        for view in [
            "spawn_decisions",
            "spawn_interventions",
            "spawn_outcomes",
            "spawn_escalations",
        ] {
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
    let now: i64 = client
        .query_one("select spawn_now_ms()", &[])
        .expect("now")
        .get(0);
    let hours_ago = |h: i64| (now - h * H) / H;
    // Times relative to now, rounded to the hour below: within the day.
    command(
        &mut client,
        "human",
        "board-1",
        "session_w",
        &json!({"type": "input", "text": "a"}),
        hours_ago(2),
    );
    command(
        &mut client,
        "spawn",
        "cli-1",
        "session_w",
        &json!({"type": "input", "text": "b"}),
        hours_ago(3),
    );
    command(
        &mut client,
        "owner_gate",
        "gate-1",
        "session_w",
        &json!({"type": "input", "text": "c"}),
        hours_ago(4),
    );
    command(
        &mut client,
        "human",
        "board-0",
        "session_w",
        &json!({"type": "input", "text": "old"}),
        hours_ago(30),
    );
    let health = store.fleet_health().expect("the health reads");
    assert_eq!(
        health["ladder_24h"]["human"], 2,
        "the board's and the CLI's, in the day"
    );
    assert_eq!(
        health["ladder_24h"]["owner_gate"], 1,
        "another principal as its own tier"
    );
    assert_eq!(health["ladder_24h"]["rules"], 0);
    assert_eq!(
        health["human_per_agent_hour"],
        Value::Null,
        "no agent hours"
    );
}

/// Objective 049: the interventions window lists every kind with its line,
/// newest first: a resume, a person's task, a cancel and reassign, and each
/// person's intent.
#[test]
fn the_interventions_window_lists_each_kind_with_its_line() {
    let (store, schema) = store("experience_interventions");
    let mut client = admin(&schema);
    history(&mut client);
    let read = store
        .interventions_window(0, 24 * H)
        .expect("the window reads");
    let lines: Vec<String> = read["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["kind"] != "command")
        .map(|item| {
            format!(
                "{} {}",
                item["kind"].as_str().unwrap_or_default(),
                item["line"].as_str().unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(lines.len(), 3, "{read}");
    assert!(
        lines.contains(&"owner_task assigned task #1".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"cancel_reassign cancelled task #2, assigned task #3".to_string()),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("resume resumed mission #")),
        "{lines:?}"
    );
    let times: Vec<i64> = read["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| item["at_ms"].as_i64().unwrap_or_default())
        .collect();
    assert!(
        times.windows(2).all(|pair| pair[0] >= pair[1]),
        "newest first: {times:?}"
    );
    for item in read["items"].as_array().into_iter().flatten() {
        if item["kind"] == "command" {
            assert!(
                item["outcome"].is_string() && item["attempts"].is_i64(),
                "{item}"
            );
        } else {
            assert_eq!(item["commands"], json!([]), "{item}");
            assert_eq!(item["outcome"], Value::Null, "{item}");
        }
    }
}

/// Every value of `key` anywhere in a JSON plan, as text.
fn plan_values(plan: &Value, key: &str, out: &mut Vec<Value>) {
    match plan {
        Value::Object(map) => {
            for (name, value) in map {
                if name == key {
                    out.push(value.clone());
                }
                plan_values(value, key, out);
            }
        }
        Value::Array(list) => list.iter().for_each(|value| plan_values(value, key, out)),
        _ => {}
    }
}

/// The rows the plan read from spawn_events: each scan's rows and the rows
/// its filter removed, times its loops.
fn events_read(plan: &Value) -> i64 {
    let mut total = 0;
    let mut all = vec![plan.clone()];
    while let Some(node) = all.pop() {
        if node["Relation Name"] == "spawn_events" {
            let loops = node["Actual Loops"].as_i64().unwrap_or(1);
            let rows = node["Actual Rows"].as_i64().unwrap_or(0)
                + node["Rows Removed by Filter"].as_i64().unwrap_or(0)
                + node["Rows Removed by Index Recheck"].as_i64().unwrap_or(0);
            total += rows * loops;
        }
        match node {
            Value::Object(map) => all.extend(map.into_iter().map(|(_, value)| value)),
            Value::Array(list) => all.extend(list),
            _ => {}
        }
    }
    total
}

/// A week of 300 tasks after `mission.added`: each assigned, judged eight
/// times and closed, every tenth cancelled by a person and assigned again
/// (same objective) as task 1000 + its id, then landed; every twentieth
/// assigned by a person. About 3000 control records, as the live log.
fn live_size_week(client: &mut Client) {
    client
        .batch_execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             select 'agent_control', 'session_control', null, 'control', null,
                    jsonb_build_object('name', 'control.recorded',
                                       'detail', jsonb_build_object('record', r.record)),
                    1 + row_number() over (order by r.ord), 3600000 + r.ord * 180000
               from (
                 select t * 12 + 0 as ord,
                        jsonb_build_object('type', 'task.assigned', 'id', t, 'mission_id', 1,
                                           'kind', 'ephemeral', 'agent_id', 'agent_worker_w_' || t,
                                           'session_id', 'session_worker_w_' || t,
                                           'brief', 'brief ' || t, 'objective', t,
                                           'created_by',
                                           case when t % 20 = 0 then 'human' else 'spawn-dispatch' end,
                                           'model', 'default') as record
                   from generate_series(1, 300) t
                 union all
                 select t * 12 + g, jsonb_build_object('type', 'task.gated', 'id', t,
                                                      'verdict', 'rejected', 'revision', g)
                   from generate_series(1, 300) t, generate_series(1, 8) g
                 union all
                 select t * 12 + 9,
                        case when t % 10 = 0
                             then jsonb_build_object('type', 'task.cancelled', 'id', t, 'by', 'human')
                             else jsonb_build_object('type', 'task.landed', 'id', t) end
                   from generate_series(1, 300) t
                 union all
                 select t * 12 + 10,
                        jsonb_build_object('type', 'task.assigned', 'id', 1000 + t, 'mission_id', 1,
                                           'kind', 'ephemeral', 'agent_id', 'agent_worker_w_' || (1000 + t),
                                           'session_id', 'session_worker_w_' || (1000 + t),
                                           'brief', 'brief ' || t, 'objective', t,
                                           'created_by', 'spawn-dispatch', 'model', 'default')
                   from generate_series(10, 300, 10) t
                 union all
                 select t * 12 + 11, jsonb_build_object('type', 'task.landed', 'id', 1000 + t)
                   from generate_series(10, 300, 10) t
               ) r;
             analyze spawn_events;",
        )
        .expect("a live-size control stream");
}

/// Home's interventions figure at the live log's size (2026-11-11: objective
/// 049's /api/interventions passed its 10 s statement timeout on every read,
/// 12 s on a copy of the live log, 2736 control records): a week of 330
/// tasks, each assigned, judged and closed, every tenth cancelled by a person
/// and assigned again, every twentieth assigned by a person. The read and
/// the scorecard's window count the same interventions; the experience
/// views look a task's or a mission's records up by index
/// (spawn_events_control_record_int_id, _row_id), not by decoding the whole
/// control stream once per assignment; and the read stays within 25 passes
/// over the stream (the cancel-and-reassign lateral over
/// spawn_experience_assignments took 34 here).
#[test]
fn the_interventions_read_looks_records_up_by_index_at_live_size() {
    let (store, schema) = store("experience_live_size");
    let mut client = admin(&schema);
    record(
        &mut client,
        0,
        &json!({"type": "mission.added", "id": 1, "text": "live size", "origin": "human",
                "sources": []}),
    );
    live_size_week(&mut client);
    let (since, until) = (0, 8 * 24 * H);

    let read = store
        .interventions_window(since, until)
        .expect("the interventions read");
    let mut by_kind = std::collections::BTreeMap::<String, i64>::new();
    for item in read["items"].as_array().into_iter().flatten() {
        *by_kind
            .entry(item["kind"].as_str().unwrap_or_default().to_string())
            .or_default() += 1;
    }
    assert_eq!(by_kind.get("cancel_reassign"), Some(&30), "{by_kind:?}");
    assert_eq!(by_kind.get("owner_task"), Some(&15), "{by_kind:?}");
    let window = store
        .experience_window(since, until)
        .expect("the scorecard's window");
    let counted: std::collections::BTreeMap<String, i64> = window["interventions"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(kind, n)| (kind.clone(), n.as_i64().unwrap_or_default()))
        .collect();
    assert_eq!(by_kind, counted, "the figure agrees with the scorecard");

    let plan: Value = client
        .query_one(
            "explain (analyze, format json)
             select count(*) from spawn_interventions where at_ms >= $1 and at_ms < $2",
            &[&since, &until],
        )
        .expect("the plan")
        .get(0);
    let mut indexes = Vec::new();
    plan_values(&plan, "Index Name", &mut indexes);
    for index in [
        "spawn_events_control_record_int_id",
        "spawn_events_control_record_row_id",
    ] {
        assert!(
            indexes.contains(&json!(index)),
            "{index} unused: {indexes:?}"
        );
    }
    // Each lookup by index reads a task's or a mission's records, not the
    // stream: 56 448 event rows on this log (18 passes over its ~3060
    // control records), against 103 848 (34) with the cancel-and-reassign
    // lateral over spawn_experience_assignments, which folded every
    // assignment again for each cancelled task (12 s on the live log).
    let read_rows = events_read(&plan);
    let stream: i64 = client
        .query_one(
            "select count(*) from spawn_events where session_id = 'session_control'",
            &[],
        )
        .expect("the stream")
        .get(0);
    assert!(
        read_rows <= 25 * stream,
        "{read_rows} event rows read for a stream of {stream}"
    );
}

#[test]
fn a_missions_row_says_owner_work_when_it_has_no_objective_backlog() {
    let (store, schema) = store("experience_owner_work");
    let mut client = admin(&schema);
    let control = |client: &mut Client, record: Value| {
        client
            .query_one("select spawn_control_record($1)", &[&record])
            .expect("record through spawn_control_record");
    };
    for id in 1..=4 {
        control(
            &mut client,
            json!({"type": "mission.added", "id": id, "text": format!("m{id}"), "origin": "human", "sources": []}),
        );
    }
    let backlog = json!({"backlog": "/ctl/backlog", "resident": "res", "workers": 1});
    for id in [1, 3] {
        control(
            &mut client,
            json!({"type": "mission.configured", "id": id, "accept": "gate", "gate": "/ctl/scenarios",
                   "objectives": backlog}),
        );
    }
    control(
        &mut client,
        json!({"type": "mission.configured", "id": 2, "accept": "review"}),
    );
    // Mission 3's backlog taken away again: its last settings rule.
    control(
        &mut client,
        json!({"type": "mission.configured", "id": 3, "accept": "gate", "gate": "/ctl/scenarios"}),
    );
    let now: i64 = client
        .query_one("select spawn_now_ms()", &[])
        .expect("now")
        .get(0);
    let window = store
        .experience_window(now - 24 * H, now + H)
        .expect("the window reads");
    let rows: Vec<(i64, bool)> = window["missions"]
        .as_array()
        .expect("missions")
        .iter()
        .map(|m| {
            (
                m["mission"].as_i64().expect("id"),
                m["owner_work"].as_bool().expect("owner_work"),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![(1, false), (2, true), (3, true), (4, true)],
        "a backlog mission is not owner work; one configured without a backlog, \
         with its backlog removed, or never configured at all is (objective 077)"
    );
}

/// Objective 077: a mission that was only ever added (its `mission.added`,
/// no `mission.configured`, no legacy row) has no objective backlog, so its
/// work comes from the owner: its row is owner work, whatever its text
/// says, and the card never flags it idle. Beside it a backlog mission with
/// no task all along stays not owner work, so the card flags it idle.
/// Seeded like live's missions 4, 7 and 8, through the real store.
#[test]
fn a_mission_only_ever_added_is_owner_work_and_not_idle_beside_an_idle_backlog_one() {
    let (store, schema) = store("experience_owner_work_added");
    let mut client = admin(&schema);
    let control = |client: &mut Client, record: Value| {
        client
            .query_one("select spawn_control_record($1)", &[&record])
            .expect("record through spawn_control_record");
    };
    let texts = [
        (4, "Improve Spawn from the backlog"),
        (7, "Owner work on the deploy"),
        (8, "Keep the docs in step with the code"),
    ];
    for (id, text) in texts {
        control(
            &mut client,
            json!({"type": "mission.added", "id": id, "text": text, "origin": "human", "sources": []}),
        );
    }
    control(
        &mut client,
        json!({"type": "mission.configured", "id": 4, "accept": "gate", "gate": "/ctl/scenarios",
               "objectives": {"backlog": "/ctl/backlog", "resident": "res", "workers": 1}}),
    );
    let now: i64 = client
        .query_one("select spawn_now_ms()", &[])
        .expect("now")
        .get(0);
    let window = store
        .experience_window(now - 24 * H, now + H)
        .expect("the window reads");
    // The card's rule (spawn-cli scorecard::fleet `idle`): a mission is idle
    // when it is not owner work and sat without a task more than half its
    // span; none of these has a task, so each is idle unless owner work.
    let rows: Vec<(i64, bool, bool)> = window["missions"]
        .as_array()
        .expect("missions")
        .iter()
        .map(|m| {
            let owner_work = m["owner_work"].as_bool().expect("owner_work");
            let tasks = m["tasks"].as_array().expect("tasks");
            (
                m["mission"].as_i64().expect("id"),
                owner_work,
                !owner_work && tasks.is_empty(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![(4, false, true), (7, true, false), (8, true, false)],
        "[mission, owner_work, idle]: a never-configured mission is owner work \
         and not idle, whatever its text; the backlog mission is idle"
    );
}

/// S22 (rev-g's note on 077): the idle hours card's `owner_work` reads a
/// mission's last configuration record before the window's end, and that
/// is the mission's configuration as its fold has it, because every
/// configure records the whole configuration: `spawn mission configure`
/// merges its change into the mission's row (`spawn_mission_reconfigure`,
/// objective 027) and records `mission.configured` with every setting. So
/// a later configure that names only `accept`, a budget or a worker count
/// keeps the backlog, and only an explicit null clears it. Each configured
/// mission's `owner_work` is checked against the fold
/// (`mission_missions.objectives` not an object). Seeded like live: missions
/// 6, 7 and 8 have only their `mission.added` and are owner work (077,
/// pinned by its own test), so only the configured ones are compared.
#[test]
fn a_partial_reconfigure_keeps_a_missions_owner_work_as_its_fold_has_it() {
    let (store, schema) = store("experience_owner_work_fold");
    let mut client = admin(&schema);
    let missions: Vec<i64> = (1..=8)
        .map(|n| {
            store
                .mission_add(&format!("m{n}"), &json!(["/tmp/source.git"]), "human")
                .expect("add")
        })
        .collect();
    assert_eq!(missions, (1..=8).collect::<Vec<i64>>(), "ids as live's");
    let backlog = json!({"backlog": "/ctl/backlog", "resident": "res", "workers": 1});
    for id in [1, 2, 3, 5] {
        store
            .mission_configure(id, Some("/ctl/scenarios"), "gate", None, Some(&backlog))
            .expect("a backlog mission");
    }
    store
        .mission_configure(4, Some("/ctl/scenarios"), "gate", None, None)
        .expect("a gated mission with no backlog");
    for (id, change) in [
        (1, json!({"accept": "review"})),
        (2, json!({"budget": {"minutes": 90}})),
        (3, json!({"objectives": null})),
        (4, json!({"budget": {"minutes": 90}})),
        (5, json!({"objectives": {"workers": 4}})),
    ] {
        store
            .mission_reconfigure(id, &change)
            .unwrap_or_else(|error| panic!("mission {id}: {change}: {error}"));
    }
    let now: i64 = client
        .query_one("select spawn_now_ms()", &[])
        .expect("now")
        .get(0);
    let window = store
        .experience_window(now - 24 * H, now + H)
        .expect("the window reads");
    let card: Vec<(i64, bool)> = window["missions"]
        .as_array()
        .expect("missions")
        .iter()
        .map(|m| {
            (
                m["mission"].as_i64().expect("id"),
                m["owner_work"].as_bool().expect("owner_work"),
            )
        })
        .filter(|(id, _)| *id <= 5)
        .collect();
    let fold: Vec<(i64, bool)> = client
        .query(
            "select id, coalesce(jsonb_typeof(objectives), 'null') <> 'object'
               from mission_missions where id <= 5 order by id",
            &[],
        )
        .expect("the fold")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(card, fold, "the card says what the missions' fold says");
    assert_eq!(
        card,
        vec![(1, false), (2, false), (3, true), (4, true), (5, false)],
        "a configure that names only accept, a budget or the workers keeps the backlog; \
         an explicit null clears it; a mission configured with none is owner work"
    );
}

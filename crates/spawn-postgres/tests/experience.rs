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

/// Each scan of spawn_events in a plan: its index (or a sequential scan),
/// its rows with what its filter removed, and its loops; for a message.
fn event_scans(plan: &Value) -> Vec<String> {
    let mut scans = Vec::new();
    let mut all = vec![plan.clone()];
    while let Some(node) = all.pop() {
        if node["Relation Name"] == "spawn_events" {
            scans.push(format!(
                "{} {} rows+{} removed x{}",
                node["Index Name"].as_str().unwrap_or("seq"),
                node["Actual Rows"],
                node["Rows Removed by Filter"].as_i64().unwrap_or(0),
                node["Actual Loops"]
            ));
        }
        match node {
            Value::Object(map) => all.extend(map.into_iter().map(|(_, value)| value)),
            Value::Array(list) => all.extend(list),
            _ => {}
        }
    }
    scans
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

/// One control record at `at_ms`, written straight into the log.
fn record_ms(client: &mut Client, at_ms: i64, record: &Value) {
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
            &[record, &at_ms],
        )
        .expect("write a control record");
}

/// The rows of `from` (a relation with `$1`, `$2` its window), every
/// column, in one order, as text.
fn intervention_rows(client: &mut Client, from: &str, since: i64, until: i64) -> String {
    client
        .query_one(
            &format!(
                "select coalesce(string_agg(row, ' ' order by row), '') from (
                   select concat_ws(',', v.at_ms, v.kind, coalesce(v.mission_id::text, '-'),
                                    coalesce(v.task_id::text, '-'),
                                    coalesce(v.command_id::text, '-'),
                                    coalesce(v.cancelled_task_id::text, '-')) as row
                     from {from} v) t"
            ),
            &[&since, &until],
        )
        .expect("the interventions")
        .get(0)
}

/// spawn_interventions as it was before it read from the window outward
/// (d61e85eb), word for word: the oracle the view's answers are compared
/// with.
const ORACLE: &str = r"
create view spawn_interventions_oracle as
select i.at_ms, 'command'::text as kind, null::bigint as mission_id,
       null::bigint as task_id, i.intent as command_id, null::bigint as cancelled_task_id
from spawn_human_intents i
union all
select r.at_ms, 'resume', spawn_experience_int(r.record -> 'id'), null, null, null
from spawn_experience_records r
where r.type = 'mission.resumed' and spawn_experience_int(r.record -> 'id') is not null
union all
select a.assigned_at_ms, 'owner_task', a.mission_id, a.task_id, null, null
from spawn_experience_assignments a
where a.type = 'task.assigned' and a.created_by = 'human'
  and coalesce((select s.review from spawn_experience_settings s
                 where s.mission_id = a.mission_id and s.seq < a.seq
                 order by s.seq desc limit 1), true)
union all
select n.assigned_at_ms, 'cancel_reassign', t.mission_id, n.task_id, null, t.task_id
from spawn_task_spans t
join spawn_experience_records x on x.seq = t.closed_seq
cross join lateral (
    select r.at_ms as assigned_at_ms,
           spawn_experience_int(coalesce(r.record -> 'id', r.record -> 'row' -> 'id')) as task_id
      from spawn_experience_records r
     where r.type = 'task.assigned' and r.seq > t.closed_seq
       and spawn_experience_int(coalesce(r.record -> 'mission_id', r.record -> 'row' -> 'mission_id'))
           = t.mission_id
       and spawn_experience_int(coalesce(r.record -> 'id', r.record -> 'row' -> 'id')) is not null
       and case when t.objective is not null
                then spawn_experience_int(coalesce(r.record -> 'objective', r.record -> 'row' -> 'objective'))
                     = t.objective
                else t.brief is not null
                     and coalesce(r.record ->> 'brief', r.record -> 'row' ->> 'brief') = t.brief end
     order by r.seq
     limit 1
) n
where t.outcome = 'cancelled'
  and (jsonb_typeof(x.record -> 'by') is distinct from 'string' or x.record ->> 'by' = 'human');";

/// The window `[$1, $2)` of the view, as a reader asks it.
const WINDOWED: &str = "(select * from spawn_interventions where at_ms >= $1 and at_ms < $2)";

/// The oracle's rows of `[since, until)` against the view's.
fn same_as_the_view(client: &mut Client, since: i64, until: i64, what: &str) {
    let oracle = intervention_rows(
        client,
        "(select * from spawn_interventions_oracle where at_ms >= $1 and at_ms < $2)",
        since,
        until,
    );
    let read = intervention_rows(client, WINDOWED, since, until);
    assert_eq!(read, oracle, "{what}: [{since}, {until})");
}

/// The oracle beside the view, in the case's schema.
fn with_oracle(client: &mut Client) {
    client.batch_execute(ORACLE).expect("the oracle view");
}

/// A small deterministic generator (no crate for it): the same stream for
/// the same seed.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[usize::try_from(self.below(from.len() as u64)).unwrap_or(0)]
    }
}

/// A record that closes task `id`, one of every way (or none).
fn random_closing(random: &mut Lcg, id: i64) -> Value {
    match random.below(8) {
        0 => json!({"type": "task.cancelled", "id": id, "by": "human"}),
        1 => json!({"type": "task.cancelled", "id": id, "by": "spawn-integrate"}),
        2 => json!({"type": "task.cancelled", "id": id, "by": 7}),
        3 => json!({"type": "task.cancelled", "id": id}),
        4 => json!({"type": "task.landed", "id": id, "from": "a", "to": "b"}),
        5 => json!({"type": "task.aborted", "id": id, "reason": "x"}),
        6 => json!({"type": "task.over_budget", "id": id, "reason": "x"}),
        _ => json!({"type": "task.reviewed", "id": id,
                    "accepted": random.pick(&[json!(true), json!(false), json!("t")])}),
    }
}

/// A person's command at `at`, taken or failed.
fn random_command(client: &mut Client, random: &mut Lcg, key: &str, at: i64) {
    let principal = *random.pick(&["human", "spawn"]);
    let text = *random.pick(&["a", "b"]);
    command(
        client,
        principal,
        key,
        "session_x",
        &json!({"type": "input", "text": text}),
        0,
    );
    client
        .execute(
            "update spawn_commands set created_at_ms = $1::bigint, finished_at_ms = $1::bigint + 1000,
                    succeeded = $2 where idempotency_key = $3",
            &[&at, &(random.below(2) == 0), &key],
        )
        .expect("time the command");
}

/// A random control stream over two days: three missions configured and
/// reconfigured (gated or not, accepted by review or not), tasks assigned
/// and assigned again under the same id, of an objective (a number, a
/// number as text, none) or only a brief, by a person or not, closed every
/// way a span closes or not at all (cancelled by a person, by another, by a
/// `by` that is not text; landed, aborted, over budget, a review accepted
/// or not), legacy task and mission rows in every state, resumptions and a
/// person's commands. Returns the last record's time.
fn random_stream(client: &mut Client, seed: u64) -> i64 {
    let mut random = Lcg(seed);
    let minute = 60_000;
    let mut at = H;
    for mission in 1..=3 {
        record_ms(
            client,
            at,
            &json!({"type": "mission.added", "id": mission, "text": "m", "origin": "human",
                    "sources": []}),
        );
    }
    let mut tasks: Vec<i64> = Vec::new();
    let objectives = [json!(null), json!(1), json!(2), json!("2"), json!(3)];
    let briefs = [json!("a"), json!("b"), json!(null)];
    for step in 0..140 {
        at += i64::try_from(random.below(40)).unwrap_or(0) * minute + minute;
        let mission = i64::try_from(random.below(3)).unwrap_or(0) + 1;
        match random.below(12) {
            0..=3 => {
                let id = if tasks.is_empty() || random.below(4) > 0 {
                    let id = 100 + step;
                    tasks.push(id);
                    id
                } else {
                    *random.pick(&tasks)
                };
                let mut assigned = json!({
                    "type": "task.assigned", "id": id, "mission_id": mission,
                    "kind": "ephemeral", "agent_id": format!("agent_worker_w_{id}"),
                    "session_id": format!("session_worker_w_{id}"),
                    "created_by": random.pick(&["human", "spawn-dispatch", "spawn_supervisor"]),
                    "model": "default",
                });
                let objective = random.pick(&objectives).clone();
                if !objective.is_null() || random.below(2) == 0 {
                    assigned["objective"] = objective;
                }
                let brief = random.pick(&briefs).clone();
                if !brief.is_null() {
                    assigned["brief"] = brief;
                }
                record_ms(client, at, &assigned);
            }
            4..=7 if !tasks.is_empty() => {
                let id = *random.pick(&tasks);
                let closing = random_closing(&mut random, id);
                record_ms(client, at, &closing);
            }
            8 => {
                let gate = if random.below(2) == 0 {
                    json!(null)
                } else {
                    json!("scripts/gate.sh")
                };
                record_ms(
                    client,
                    at,
                    &json!({"type": "mission.configured", "id": mission, "gate": gate,
                            "accept": random.pick(&["review", "gate"])}),
                );
            }
            9 => record_ms(
                client,
                at,
                &json!({"type": "mission.resumed", "id": mission}),
            ),
            10 => {
                let id = 100 + step;
                tasks.push(id);
                record_ms(
                    client,
                    at,
                    &json!({"type": "legacy.imported", "table": "mission_tasks",
                            "row": {"id": id, "mission_id": mission,
                                    "state": random.pick(&["open", "active", "cancelled",
                                                           "landed", "accepted"]),
                                    "objective": random.pick(&objectives),
                                    "brief": random.pick(&briefs),
                                    "created_by": random.pick(&["human", "spawn-dispatch"]),
                                    "created_at_ms": at - minute, "updated_at_ms": at}}),
                );
            }
            _ => {
                random_command(client, &mut random, &format!("k{seed}-{step}"), at);
            }
        }
    }
    at
}

/// The view gives the oracle's rows (the view before, d61e85eb) for every
/// window: on the history every other test reads, and on random streams
/// with every kind of record a span or a reassignment depends on, over
/// windows that cut anywhere, both edges included and excluded, an empty
/// window and the whole log.
#[test]
fn the_window_read_gives_the_views_rows_of_every_window() {
    let (_store, schema) = store("interventions_window_read");
    let mut client = admin(&schema);
    with_oracle(&mut client);
    history(&mut client);
    same_as_the_view(&mut client, 0, i64::MAX, "history, all of it");
    same_as_the_view(&mut client, 5 * H, 5 * H, "history, an empty window");
    for since in 0..=21 {
        for until in since + 1..=22 {
            same_as_the_view(&mut client, since * H, until * H, "history");
        }
    }
    // On the gated mission 2: a review accepted does not close a task
    // (its gate does), so a person's cancellation after it does, and the
    // same objective again is its reassignment (at 25 h).
    assign(&mut client, 22, 8, 2, "spawn_supervisor", Some(4), "eight");
    record(
        &mut client,
        23,
        &json!({"type": "task.reviewed", "id": 8, "accepted": true}),
    );
    record(
        &mut client,
        24,
        &json!({"type": "task.cancelled", "id": 8, "by": "human"}),
    );
    assign(
        &mut client,
        25,
        9,
        2,
        "spawn_supervisor",
        Some(4),
        "eight, again",
    );
    assert!(
        intervention_rows(&mut client, WINDOWED, 25 * H, 26 * H).contains("cancel_reassign,2,9"),
        "a cancellation after a review on a gated mission is reassigned"
    );
    same_as_the_view(&mut client, 22 * H, 26 * H, "history, gated");
    same_as_the_view(&mut client, 0, i64::MAX, "history, all of it, gated");
    // The edges: a reassignment at exactly 5 h is in [5 h, …) and not in
    // […, 5 h).
    same_as_the_view(&mut client, 5 * H, 5 * H + 1, "history, at an edge");
    same_as_the_view(&mut client, 5 * H - 1, 5 * H, "history, before an edge");
    for seed in 1..=8 {
        let (_store, schema) = store(&format!("interventions_random_{seed}"));
        let mut client = admin(&schema);
        with_oracle(&mut client);
        let last = random_stream(&mut client, seed);
        same_as_the_view(&mut client, 0, i64::MAX, &format!("seed {seed}, all of it"));
        let all = intervention_rows(&mut client, WINDOWED, 0, last + 1);
        assert!(
            all.contains("cancel_reassign") && all.contains("owner_task"),
            "seed {seed} has the kinds that matter: {all}"
        );
        let mut cuts = vec![0, last + 1];
        let mut random = Lcg(seed * 7919);
        for _ in 0..10 {
            cuts.push(i64::try_from(random.below(u64::try_from(last).unwrap_or(1))).unwrap_or(0));
        }
        cuts.sort_unstable();
        for (i, since) in cuts.iter().enumerate() {
            for until in &cuts[i + 1..] {
                same_as_the_view(&mut client, *since, *until, &format!("seed {seed}"));
            }
        }
    }
}

/// `weeks` of the live-size week's stream ([`live_size_week`]'s records,
/// each week's task ids and objectives its own).
fn live_size_weeks(client: &mut Client, weeks: i64) {
    live_size_stream(client, weeks, false);
}

/// [`live_size_weeks`], and with `old_work` twenty pieces of work assigned
/// and cancelled by a person in the first hours, then assigned again around
/// day 7 (objectives 800001-800020: tasks 500000 + k, then 600000 + k,
/// landed): reassignments of old work.
fn live_size_stream(client: &mut Client, weeks: i64, old_work: bool) {
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             select 'agent_control', 'session_control', null, 'control', null,
                    jsonb_build_object('name', 'control.recorded',
                                       'detail', jsonb_build_object('record', r.record)),
                    1 + row_number() over (order by r.ord),
                    (3600000 + r.ord * 180000)::bigint
               from (
                 select t * 12 + 0 as ord,
                        jsonb_build_object('type', 'task.assigned', 'id', t, 'mission_id', 1,
                                           'kind', 'ephemeral', 'agent_id', 'agent_worker_w_' || t,
                                           'session_id', 'session_worker_w_' || t,
                                           'brief', 'brief ' || t, 'objective', t,
                                           'created_by',
                                           case when t % 20 = 0 then 'human' else 'spawn-dispatch' end,
                                           'model', 'default') as record
                   from generate_series(1, 300 * $1::bigint) t
                 union all
                 select t * 12 + g, jsonb_build_object('type', 'task.gated', 'id', t,
                                                      'verdict', 'rejected', 'revision', g)
                   from generate_series(1, 300 * $1::bigint) t, generate_series(1, 8) g
                 union all
                 select t * 12 + 9,
                        case when t % 10 = 0
                             then jsonb_build_object('type', 'task.cancelled', 'id', t, 'by', 'human')
                             else jsonb_build_object('type', 'task.landed', 'id', t) end
                   from generate_series(1, 300 * $1::bigint) t
                 union all
                 select t * 12 + 10,
                        jsonb_build_object('type', 'task.assigned', 'id', 100000 + t, 'mission_id', 1,
                                           'kind', 'ephemeral',
                                           'agent_id', 'agent_worker_w_' || (100000 + t),
                                           'session_id', 'session_worker_w_' || (100000 + t),
                                           'brief', 'brief ' || t, 'objective', t,
                                           'created_by', 'spawn-dispatch', 'model', 'default')
                   from generate_series(10, 300 * $1::bigint, 10) t
                 union all
                 select t * 12 + 11, jsonb_build_object('type', 'task.landed', 'id', 100000 + t)
                   from generate_series(10, 300 * $1::bigint, 10) t
                 union all
                 select k * 12 + w.at,
                        case w.what
                          when 'assigned' then
                            jsonb_build_object('type', 'task.assigned', 'id', w.task + k,
                                               'mission_id', 1, 'kind', 'ephemeral',
                                               'agent_id', 'agent_worker_w_' || (w.task + k),
                                               'session_id', 'session_worker_w_' || (w.task + k),
                                               'brief', 'old brief ' || k, 'objective', 800000 + k,
                                               'created_by', 'spawn-dispatch', 'model', 'default')
                          when 'cancelled' then
                            jsonb_build_object('type', 'task.cancelled', 'id', w.task + k,
                                               'by', 'human')
                          else jsonb_build_object('type', 'task.landed', 'id', w.task + k) end
                   from generate_series(1, 20) k,
                        (values (0.25, 'assigned', 500000), (0.5, 'cancelled', 500000),
                                (3400.25, 'assigned', 600000), (3400.5, 'landed', 600000))
                          as w(at, what, task)
                  where $2
               ) r",
            &[&weeks, &old_work],
        )
        .expect("a live-size control stream");
    client
        .batch_execute("analyze spawn_events")
        .expect("analyze");
}

/// The rows the read takes from spawn_events for the last day of a
/// stream of `weeks` weeks, its rows, and the indexes it used.
fn last_day_read(weeks: i64) -> (i64, String, String, Vec<Value>, Vec<String>) {
    let (_store, schema) = store(&format!("interventions_weeks_{weeks}"));
    let mut client = admin(&schema);
    record(
        &mut client,
        0,
        &json!({"type": "mission.added", "id": 1, "text": "live size", "origin": "human",
                "sources": []}),
    );
    with_oracle(&mut client);
    live_size_weeks(&mut client, weeks);
    let until: i64 = client
        .query_one(
            "select max(recorded_at_ms) + 1 from spawn_events where session_id = 'session_control'",
            &[],
        )
        .expect("the stream's end")
        .get(0);
    let since = until - 24 * H;
    // A generic plan, as a reader's prepared statement gets after its
    // fifth run: the window is two parameters, not constants.
    client
        .batch_execute("set plan_cache_mode = force_generic_plan")
        .expect("generic plans");
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
    // fleet_health's statement, whole (its interventions count, objective
    // 103, windows the view by its own `now`).
    let health: Value = client
        .query_one(
            &format!(
                "explain (analyze, format json) {}",
                spawn_postgres::testing::FLEET_HEALTH
            ),
            &[],
        )
        .expect("the plan")
        .get(0);
    let mut scans = event_scans(&plan);
    scans.push(format!("health: {}", events_read(&health)));
    scans.extend(event_scans(&health));
    let read = intervention_rows(&mut client, WINDOWED, since, until);
    let view = intervention_rows(
        &mut client,
        "(select * from spawn_interventions_oracle where at_ms >= $1 and at_ms < $2)",
        since,
        until,
    );
    (
        events_read(&plan).max(events_read(&health)),
        read,
        view,
        indexes,
        scans,
    )
}

/// The view's window grows with the window, not the log (the lead,
/// 2026-11-13: the view took 1.1 s on the live log, the 24 h bound applied
/// after the whole history was worked out). The last day of one week and of
/// four weeks of the live-size stream, under a generic plan: the oracle's
/// rows, every kind there, by the assignment index, and four times the
/// history reads no more than 10% more event rows than one week (a bound on
/// rows read, never a time).
#[test]
fn the_window_read_grows_with_the_window_not_the_log() {
    let (one, one_read, one_view, indexes, one_scans) = last_day_read(1);
    assert_eq!(one_read, one_view);
    assert!(one_read.contains("cancel_reassign"), "{one_read}");
    assert!(one_read.contains("owner_task"), "{one_read}");
    assert!(one > 0, "the plan reads spawn_events");
    assert!(
        indexes.contains(&json!("spawn_events_control_assignment_objective")),
        "{indexes:?}"
    );
    let (four, four_read, four_view, _, four_scans) = last_day_read(4);
    assert_eq!(four_read, four_view);
    assert_eq!(
        four_read.matches("cancel_reassign").count(),
        one_read.matches("cancel_reassign").count(),
        "a day of the same stream"
    );
    assert!(
        four <= one + one / 10,
        "the last day of 4 weeks read {four} event rows, of 1 week {one}:\n{four_scans:#?}\n{one_scans:#?}"
    );
}

/// The newest record of a type (host.rs's NEWEST_PASSED, copied word for
/// word) is found by spawn_events_control_record_type under a generic plan,
/// the type a parameter, without reading the stream.
#[test]
fn the_newest_record_of_a_type_is_read_by_its_index_under_a_generic_plan() {
    const NEWEST_PASSED: &str = r"
select e.recorded_at_ms
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = $1
   and e.kind -> 'detail' -> 'record' -> 'ok' = 'true'::jsonb
 order by e.id desc
 limit 1";
    let (store, schema) = store("interventions_type_index");
    let mut client = admin(&schema);
    record(
        &mut client,
        0,
        &json!({"type": "mission.added", "id": 1, "text": "live size", "origin": "human",
                "sources": []}),
    );
    live_size_weeks(&mut client, 1);
    record(
        &mut client,
        1,
        &json!({"type": "backup.drilled.transcripts", "ok": true}),
    );
    client
        .batch_execute("set plan_cache_mode = force_generic_plan")
        .expect("generic plans");
    let plan: Value = client
        .query_one(
            &format!("explain (analyze, format json) {NEWEST_PASSED}"),
            &[&"backup.drilled.missions"],
        )
        .expect("the plan")
        .get(0);
    let mut indexes = Vec::new();
    plan_values(&plan, "Index Name", &mut indexes);
    assert!(
        indexes.contains(&json!("spawn_events_control_record_type")),
        "{indexes:?}"
    );
    assert!(events_read(&plan) <= 1, "{} rows read", events_read(&plan));
    assert_eq!(
        store
            .newest_passed_record("backup.drilled.transcripts")
            .expect("the read"),
        Some(H)
    );
}

/// The buffers a plan's root read (shared hits and reads).
fn plan_buffers(plan: &Value) -> i64 {
    let root = &plan[0]["Plan"];
    root["Shared Hit Blocks"].as_i64().unwrap_or(0)
        + root["Shared Read Blocks"].as_i64().unwrap_or(0)
}

/// What one window read of [`windows_read`] gives: its buffers, its rows,
/// the oracle's rows, its spawn_events scans (for a message).
type WindowRead = (i64, String, String, Vec<String>);

/// The windows `[from, to)` (in days) of a stream of `weeks` weeks with old
/// work reassigned in it, under a generic plan.
fn windows_read(weeks: i64, windows: &[(i64, i64)]) -> Vec<WindowRead> {
    let (_store, schema) = store(&format!("interventions_follow_{weeks}"));
    let mut client = admin(&schema);
    record(
        &mut client,
        0,
        &json!({"type": "mission.added", "id": 1, "text": "live size", "origin": "human",
                "sources": []}),
    );
    with_oracle(&mut client);
    live_size_stream(&mut client, weeks, true);
    client
        .batch_execute("set plan_cache_mode = force_generic_plan")
        .expect("generic plans");
    windows
        .iter()
        .map(|&(from, to)| {
            let (since, until) = (from * 24 * H, to * 24 * H);
            let plan: Value = client
                .query_one(
                    "explain (analyze, buffers, format json)
                     select count(*) from spawn_interventions where at_ms >= $1 and at_ms < $2",
                    &[&since, &until],
                )
                .expect("the plan")
                .get(0);
            let read = intervention_rows(&mut client, WINDOWED, since, until);
            let view = intervention_rows(
                &mut client,
                "(select * from spawn_interventions_oracle where at_ms >= $1 and at_ms < $2)",
                since,
                until,
            );
            (plan_buffers(&plan), read, view, event_scans(&plan))
        })
        .collect()
}

/// A window costs what is in it, whatever history follows it (the lead,
/// 2026-11-13: after the window-first view a 7-day window still took 1.4 s
/// on the live log, as each cancelled attempt's next assignment, `x`,
/// walked every later assignment). The same 7-day and 24 h windows, old
/// work reassigned in both, of a stream of 2 weeks and of 4: the oracle's
/// rows, the reassignments of old work among them, and the 4 weeks' read
/// takes no more than 10% more buffers than the 2 weeks'.
///
/// Buffers, not rows: `x` returned no row and its filter removed none, as
/// its index (_brief) was walked on its third column (`session_seq`) with
/// only `mission_id is not null` on the first. A walk on a column that does
/// not lead shows in the buffers alone; the growth test before counted rows,
/// on the log's last day (nothing after it to walk), and passed.
#[test]
fn a_window_costs_the_same_whatever_history_follows_it() {
    let windows = [(7, 14), (7, 8)];
    let two = windows_read(2, &windows);
    let four = windows_read(4, &windows);
    for ((window, two), four) in windows.iter().zip(&two).zip(&four) {
        let (two_buffers, two_read, two_view, two_scans) = two;
        let (four_buffers, four_read, four_view, four_scans) = four;
        assert_eq!(two_read, two_view, "{window:?}");
        assert_eq!(four_read, four_view, "{window:?}");
        assert_eq!(two_read, four_read, "{window:?}: the same window");
        assert!(
            two_read.matches("cancel_reassign").count() >= 20,
            "{window:?}: {two_read}"
        );
        assert!(
            *four_buffers <= two_buffers + two_buffers / 10,
            "{window:?}: 4 weeks read {four_buffers} buffers, 2 weeks {two_buffers}:\n{four_scans:#?}\n{two_scans:#?}"
        );
    }
}

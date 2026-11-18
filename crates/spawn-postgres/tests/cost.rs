//! What model calls cost (objective 041): the cost step folds the Bridge's
//! `model.usage` events into one row per message (`spawn_cost_record`),
//! once whatever repeats it, with its largest usage at its earliest time,
//! for the task its agent held then, priced when it is recorded; the window
//! and the daily budgets are read from those rows.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use serde_json::{Value, json};
use spawn_core::event::{ENGINE, EndCause, EventKind, ModelProfile, NewEvent, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
use spawn_core::port::EventLog;
use spawn_postgres::{
    ArchiveCounted, ArchiveFile, ArchivedMessage, PostgresStore, SessionFile, SessionMessage,
};

mod common;
use common::{admin, store};

const DAY_MS: i64 = 86_400_000;

struct Worker {
    agent: String,
    session: String,
}

fn append(store: &PostgresStore, worker: &Worker, execution: u64, key: &str, kind: EventKind) {
    store
        .append(NewEvent {
            agent_id: AgentId::new(&worker.agent).expect("valid agent"),
            session_id: SessionId::new(&worker.session).expect("valid session"),
            execution_id: Some(ExecutionId::for_command(CommandId(execution))),
            source: Source::Claude,
            source_event_id: Some(key.to_string()),
            kind,
        })
        .expect("append");
}

fn started(store: &PostgresStore, worker: &Worker, execution: u64) {
    append(
        store,
        worker,
        execution,
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

/// One transcript line's usage, keyed as the Bridge keys it.
fn usage(
    store: &PostgresStore,
    worker: &Worker,
    execution: u64,
    (id, model, at_ms): (&str, &str, i64),
    counts: [u64; 4],
) {
    let [input, output, cache_write, cache_read] = counts;
    append(
        store,
        worker,
        execution,
        &format!("usage:{id}:{input}:{output}:{cache_write}:{cache_read}"),
        EventKind::ModelUsage {
            message_id: id.to_string(),
            model: model.to_string(),
            at_ms,
            input_tokens: input,
            output_tokens: output,
            cache_write_tokens: cache_write,
            cache_read_tokens: cache_read,
        },
    );
}

fn prices(input: f64) -> Value {
    json!({"model-x": {"input": input, "output": 15, "cache_write": 3.75, "cache_read": 0.3}})
}

fn usd(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

fn near(actual: &Value, expected: f64, what: &str) {
    assert!(
        (usd(actual) - expected).abs() < 1e-9,
        "{what}: {actual}, expected {expected}"
    );
}

fn entry<'a>(answer: &'a Value, list: &str, key: &str, id: &Value) -> &'a Value {
    answer[list]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| &row[key] == id))
        .unwrap_or_else(|| panic!("no {key} {id} in {list}: {answer}"))
}

/// A mission with one ephemeral worker's task: the mission, the task and
/// the worker's names (`spawn_task_assign`).
fn one_task(store: &PostgresStore) -> (i64, i64, Worker) {
    let mission = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "w1", "brief", "default")
        .expect("assign");
    let worker = Worker {
        agent: format!("agent_worker_w1_{task}"),
        session: format!("session_worker_w1_{task}"),
    };
    (mission, task, worker)
}

#[test]
fn each_message_counts_once_for_its_task_priced_when_recorded() {
    let (store, _schema) = store("cost_record");
    let (mission, task, worker) = one_task(&store);
    let now = store.database_now_ms().expect("now");
    started(&store, &worker, 1);
    let lines: [(&str, &str, i64, [u64; 4]); 5] = [
        // Two lines of one message (the same usage: one event), and a later
        // line whose usage grew: the message counts its largest usage, once.
        ("msg_1", "model-x", now, [1000, 2000, 10_000, 50_000]),
        ("msg_1", "model-x", now, [1000, 2000, 10_000, 50_000]),
        ("msg_1", "model-x", now + 5, [1000, 4000, 10_000, 50_000]),
        // Timed three days ago, before the task was assigned: the task's.
        ("msg_2", "model-x", now - 3 * DAY_MS, [1_000_000, 0, 0, 0]),
        // A model with no price: its tokens, no cost.
        ("msg_3", "model-z", now, [5000, 500, 0, 0]),
    ];
    for (id, model, at, counts) in lines {
        usage(&store, &worker, 1, (id, model, at), counts);
    }
    assert_eq!(store.cost_record(&prices(3.0)).expect("record"), 3);
    // Read again, and again at other prices: nothing changes.
    assert_eq!(store.cost_record(&prices(3.0)).expect("record"), 0);
    assert_eq!(store.cost_record(&prices(6.0)).expect("record"), 0);

    let week = store
        .cost_window(now - 7 * DAY_MS, now + 1000)
        .expect("window");
    // msg_1: 1000*3 + 4000*15 + 10000*3.75 + 50000*0.3 = $0.1155; msg_2 $3.
    near(&week["total"]["usd"], 3.1155, "the week's total");
    assert_eq!(
        week["total"]["tokens"],
        json!({"input": 1_006_000, "output": 4500, "cache_write": 10_000, "cache_read": 50_000})
    );
    assert_eq!(week["total"]["unpriced_tokens"], 5500);
    let by_task = entry(&week, "tasks", "task", &json!(task));
    near(
        &by_task["usd"],
        3.1155,
        "the task, the early message included",
    );
    assert_eq!(by_task["mission"], mission);
    let by_mission = entry(&week, "missions", "mission", &json!(mission));
    near(&by_mission["usd"], 3.1155, "the mission");
    let by_agent = entry(&week, "agents", "agent", &json!(worker.agent));
    near(&by_agent["usd"], 3.1155, "the agent");
    let z = entry(&week, "models", "model", &json!("model-z"));
    assert_eq!(z["usd"], Value::Null, "an unpriced model has no cost: {z}");
    let execution = entry(&week, "executions", "execution", &json!("exec_1"));
    assert_eq!(execution["task"], task);
    assert_eq!(execution["unpriced_tokens"], 5500);

    let day = store.cost_window(now - DAY_MS, now + 1000).expect("window");
    near(
        &day["total"]["usd"],
        0.1155,
        "the day leaves out the early message",
    );

    // New spending is priced at the prices of its recording.
    usage(
        &store,
        &worker,
        1,
        ("msg_4", "model-x", now + 10),
        [100_000, 0, 0, 0],
    );
    assert_eq!(store.cost_record(&prices(6.0)).expect("record"), 1);
    let day = store.cost_window(now - DAY_MS, now + 1000).expect("window");
    near(&day["total"]["usd"], 0.7155, "msg_4 at $6 a million");
}

#[test]
fn each_model_row_says_how_many_of_its_tokens_have_no_price() {
    let (store, _schema) = store("cost_models_unpriced");
    let (_mission, _task, worker) = one_task(&store);
    let now = store.database_now_ms().expect("now");
    started(&store, &worker, 1);
    usage(
        &store,
        &worker,
        1,
        ("msg_x", "model-x", now),
        [100_000, 10_000, 0, 0],
    );
    usage(
        &store,
        &worker,
        1,
        ("msg_z1", "model-z", now),
        [200_000, 20_000, 1, 2],
    );
    usage(
        &store,
        &worker,
        1,
        ("msg_z2", "model-z", now),
        [50_000, 5000, 0, 0],
    );
    assert_eq!(store.cost_record(&prices(3.0)).expect("record"), 3);

    let day = store.cost_window(now - DAY_MS, now + 1000).expect("window");
    let x = entry(&day, "models", "model", &json!("model-x"));
    near(&x["usd"], 0.45, "model-x");
    assert_eq!(x["unpriced_tokens"], 0, "a priced model: {x}");
    let z = entry(&day, "models", "model", &json!("model-z"));
    assert_eq!(z["usd"], Value::Null, "{z}");
    assert_eq!(z["unpriced_tokens"], 275_003, "all four counts: {z}");
    assert_eq!(day["total"]["unpriced_tokens"], 275_003);
    // An execution's models carry theirs too.
    let execution = entry(&day, "executions", "execution", &json!("exec_1"));
    let by_model = entry(execution, "models", "model", &json!("model-z"));
    assert_eq!(by_model["unpriced_tokens"], 275_003, "{execution}");
    let by_model = entry(execution, "models", "model", &json!("model-x"));
    assert_eq!(by_model["unpriced_tokens"], 0, "{execution}");
}

/// P30: each execution's models, grouped once and joined (no longer a
/// subquery per execution): the whole `executions` list over several
/// executions and models, priced and unpriced, pinned as the window gave it
/// before the change.
#[test]
fn every_executions_models_are_its_own_in_the_window() {
    let (store, _schema) = store("cost_execution_models");
    let (_mission, task, worker) = one_task(&store);
    let now = store.database_now_ms().expect("now");
    let lines: [(u64, &str, &str, [u64; 4]); 6] = [
        (1, "m1", "model-x", [100_000, 10_000, 0, 0]),
        (1, "m2", "model-x", [0, 20_000, 0, 0]),
        (1, "m3", "model-y", [1_000_000, 0, 0, 0]),
        (1, "m4", "model-z", [1000, 100, 0, 0]),
        (2, "m5", "model-x", [0, 0, 0, 1_000_000]),
        (3, "m6", "model-z", [10, 0, 0, 0]),
    ];
    // One session runs one execution at a time: each ends before the next.
    for execution in 1..=3 {
        started(&store, &worker, execution);
        for (_, id, model, counts) in lines.iter().filter(|line| line.0 == execution) {
            usage(&store, &worker, execution, (id, model, now), *counts);
        }
        store
            .append(NewEvent {
                agent_id: AgentId::new(&worker.agent).expect("valid agent"),
                session_id: SessionId::new(&worker.session).expect("valid session"),
                execution_id: Some(ExecutionId::for_command(CommandId(execution))),
                source: Source::Bridge,
                source_event_id: Some(format!("end-{execution}")),
                kind: EventKind::ExecutionEnded {
                    cause: EndCause::Stopped,
                    exit_code: None,
                    signal: None,
                    evidence: None,
                },
            })
            .expect("end");
    }
    let prices = json!({
        "model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3},
        "model-y": {"input": 1, "output": 5, "cache_write": 1.25, "cache_read": 0.1},
    });
    assert_eq!(store.cost_record(&prices).expect("record"), 6);
    let day = store.cost_window(now - DAY_MS, now + 1000).expect("window");
    let tokens = |input: u64, output: u64, cache_write: u64, cache_read: u64| json!({"input": input, "output": output, "cache_write": cache_write, "cache_read": cache_read});
    let model = |name: &str, usd: Value, tokens: Value, unpriced: u64| json!({"model": name, "usd": usd, "tokens": tokens, "unpriced_tokens": unpriced});
    let execution = |id: &str, usd: Value, tokens: Value, unpriced: u64, models: Vec<Value>| {
        json!({"execution": id, "agent": worker.agent, "task": task, "usd": usd,
               "tokens": tokens, "unpriced_tokens": unpriced, "models": models})
    };
    assert_eq!(
        day["executions"],
        json!([
            execution(
                "exec_1",
                json!(1.75),
                tokens(1_101_000, 30_100, 0, 0),
                1100,
                vec![
                    model("model-y", json!(1.0), tokens(1_000_000, 0, 0, 0), 0),
                    model("model-x", json!(0.75), tokens(100_000, 30_000, 0, 0), 0),
                    model("model-z", Value::Null, tokens(1000, 100, 0, 0), 1100),
                ],
            ),
            execution(
                "exec_2",
                json!(0.3),
                tokens(0, 0, 0, 1_000_000),
                0,
                vec![model("model-x", json!(0.3), tokens(0, 0, 0, 1_000_000), 0)],
            ),
            execution(
                "exec_3",
                json!(0),
                tokens(10, 0, 0, 0),
                10,
                vec![model("model-z", Value::Null, tokens(10, 0, 0, 0), 10)],
            ),
        ]),
        "{day}"
    );
}

#[test]
fn a_daily_budget_is_an_amount_above_zero_read_with_the_days_spend() {
    let (store, _schema) = store("cost_budget");
    let (mission, _task, worker) = one_task(&store);
    let now = store.database_now_ms().expect("now");
    started(&store, &worker, 1);
    usage(
        &store,
        &worker,
        1,
        ("msg_1", "model-x", now),
        [100_000, 10_000, 0, 0],
    );
    store.cost_record(&prices(3.0)).expect("record");
    let budget = |usd: Value| {
        store.mission_configure(
            mission,
            None,
            "review",
            Some(&json!({"daily_usd": usd})),
            None,
        )
    };
    assert!(budget(json!(0)).is_err(), "a budget of $0 is refused");
    assert!(budget(json!("1")).is_err(), "a budget is a number");
    budget(json!(0.5)).expect("configure");
    let budgets = store.cost_budgets(now - 1000, now + 1000).expect("budgets");
    assert_eq!(budgets[0]["mission"], mission);
    near(&budgets[0]["budget_usd"], 0.5, "the budget");
    near(&budgets[0]["spent_usd"], 0.45, "the day's spend");
    // Before the day: nothing spent.
    let earlier = store.cost_budgets(now - 2000, now - 1000).expect("budgets");
    near(&earlier[0]["spent_usd"], 0.0, "no spend before the message");
}

fn archived(agent: &str, id: &str, model: &str, at_ms: i64, input: i64) -> ArchivedMessage {
    ArchivedMessage {
        agent_id: agent.to_string(),
        message_id: id.to_string(),
        model: model.to_string(),
        at_ms,
        input,
        output: 0,
        cache_write: 0,
        cache_read: 0,
    }
}

/// Objective 048: an archived message counts for its agent's execution and
/// task at its time (one timed before the task, the task held when the
/// execution started), once; one the record holds keeps what it holds; one
/// of an agent with no execution is not counted. The answer says since when
/// the record counts.
#[test]
fn an_archived_message_counts_once_for_the_task_its_agent_held() {
    let (store, _schema) = store("cost_archive");
    let (mission, task, worker) = one_task(&store);
    let now = store.database_now_ms().expect("now");
    assert_eq!(
        store.cost_window(now - DAY_MS, now + 1000).expect("window")["counted_since_ms"],
        Value::Null,
        "nothing counted"
    );
    started(&store, &worker, 1);
    usage(
        &store,
        &worker,
        1,
        ("msg_live", "model-x", now),
        [100, 0, 0, 0],
    );
    assert_eq!(store.cost_record(&prices(3.0)).expect("record"), 1);

    let messages = [
        // Eleven days ago, before the task and the execution: the agent's
        // first execution, the task held when it started.
        archived(
            &worker.agent,
            "msg_old",
            "model-x",
            now - 11 * DAY_MS,
            1_000_000,
        ),
        archived(&worker.agent, "msg_new", "model-x", now + 1, 2_000_000),
        // Held already: it keeps what it holds.
        archived(&worker.agent, "msg_live", "model-x", now - DAY_MS, 9_999),
        archived("agent_nobody_9", "msg_stray", "model-x", now, 5),
    ];
    let counted = store
        .cost_archive_record(&prices(3.0), &messages)
        .expect("archive");
    assert_eq!(
        counted,
        ArchiveCounted {
            counted: 2,
            unplaced: 1
        }
    );
    assert_eq!(
        store
            .cost_archive_record(&prices(6.0), &messages)
            .expect("again"),
        ArchiveCounted {
            counted: 0,
            unplaced: 1
        },
        "again adds nothing"
    );

    let month = store
        .cost_window(now - 30 * DAY_MS, now + 1000)
        .expect("window");
    near(&month["total"]["usd"], 9.0003, "$3 + $6 + the live $0.0003");
    near(
        &entry(&month, "tasks", "task", &json!(task))["usd"],
        9.0003,
        "every message the task's",
    );
    near(
        &entry(&month, "missions", "mission", &json!(mission))["usd"],
        9.0003,
        "and its mission's",
    );
    let execution = entry(&month, "executions", "execution", &json!("exec_1"));
    assert_eq!(execution["task"], task);
    assert_eq!(month["counted_since_ms"], now - 11 * DAY_MS);
    assert_eq!(
        store.cost_window(now - DAY_MS, now + 1000).expect("window")["counted_since_ms"],
        now - 11 * DAY_MS,
        "whatever the window"
    );
    let live = entry(&month, "agents", "agent", &json!(worker.agent));
    assert_eq!(live["tokens"]["input"], 3_000_100, "msg_live as it was");
}

/// The marks of an agent's archive files are replaced whole; the reader
/// may not record an archived message.
#[test]
fn an_agents_archive_marks_are_replaced_whole_and_owner_only() {
    let (store, schema) = store("cost_archive_marks");
    let agent = "agent_worker_w1_1";
    // The marks of an agent's files, replaced whole.
    let file = |name: &str, length: i64| ArchiveFile {
        file: name.to_string(),
        length,
        modified_ms: 7,
    };
    store
        .cost_archive_mark(agent, &[file("a.jsonl", 1), file("a.1.jsonl", 2)])
        .expect("mark");
    store
        .cost_archive_mark(agent, &[file("a.jsonl", 3)])
        .expect("mark");
    assert_eq!(
        store.cost_archive_files(agent).expect("marks"),
        vec![file("a.jsonl", 3)]
    );
    assert!(
        store
            .cost_archive_files("agent_other")
            .expect("marks")
            .is_empty()
    );

    let mut reader = common::as_login(&schema, "spawn_reader");
    let refused = reader
        .query_one(
            "select spawn_cost_archive_record('{}'::jsonb, '[]'::jsonb)",
            &[],
        )
        .expect_err("the reader may not record");
    assert!(
        refused.code() == Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
        "{refused}"
    );
}

/// One control record at `at_ms`, written straight into the log (a record
/// the store has no call for: a task assigned again, a second landing).
fn control(client: &mut postgres::Client, at_ms: i64, record: &Value) {
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

/// A task of `mission` for worker `name` whose execution `execution` spent
/// `input` tokens of model-x at `at_ms`: the task and its worker.
fn spending_task(
    store: &PostgresStore,
    mission: i64,
    (name, execution): (&str, u64),
    (input, at_ms): (u64, i64),
) -> (i64, Worker) {
    let task = store
        .task_assign(mission, "ephemeral", name, "brief", "default")
        .expect("assign");
    let worker = Worker {
        agent: format!("agent_worker_{name}_{task}"),
        session: format!("session_worker_{name}_{task}"),
    };
    started(store, &worker, execution);
    let id = format!("msg_{execution}");
    usage(
        store,
        &worker,
        execution,
        (&id, "model-x", at_ms),
        [input, 0, 0, 0],
    );
    (task, worker)
}

/// `task` delivered by `command` and reported, then reviewed.
fn reviewed(store: &PostgresStore, task: i64, command: i64, accept: bool) {
    store.task_delivered(task, command).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    store
        .task_review(task, accept, "note", None)
        .expect("review");
}

#[test]
fn a_window_says_what_a_landing_cost_for_the_fleet_and_each_mission() {
    let (store, schema) = store("cost_landing");
    let mut client = admin(&schema);
    let since = store.database_now_ms().expect("now");

    // Mission A, without a gate: t1 sent back once, then accepted (a
    // landing), then assigned again and accepted again (still one landing).
    let a = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let (t1, w1) = spending_task(&store, a, ("w1", 1), (100_000, since + 1));
    reviewed(&store, t1, 1, false);
    reviewed(&store, t1, 2, true);
    let now = store.database_now_ms().expect("now");
    control(
        &mut client,
        now,
        &json!({"type": "task.assigned", "id": t1, "mission_id": a, "kind": "ephemeral",
                "agent_id": w1.agent, "session_id": w1.session, "brief": "brief",
                "created_by": "human", "model": "default"}),
    );
    control(
        &mut client,
        now,
        &json!({"type": "task.reviewed", "id": t1, "accepted": true}),
    );
    // t2 of mission A landed just before the window: not counted.
    let t2 = store
        .task_assign(a, "ephemeral", "w2", "brief", "default")
        .expect("assign");
    control(
        &mut client,
        since - 1,
        &json!({"type": "task.landed", "id": t2, "from": "a", "to": "b"}),
    );
    // t3 of mission A is cancelled: it costs, it does not land.
    let (t3, _) = spending_task(&store, a, ("w3", 3), (20_000, since + 1));
    store.task_cancel(t3).expect("cancel");

    // Mission B, gated: an accepting review is not a landing (its gate
    // lands it), so it has dollars and no landing.
    let b = store
        .mission_add("Gated", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(b, Some("/controller/rsi/scenarios"), "review", None, None)
        .expect("configure");
    let (t4, _) = spending_task(&store, b, ("w4", 4), (50_000, since + 1));
    reviewed(&store, t4, 4, true);

    assert_eq!(store.cost_record(&prices(3.0)).expect("record"), 3);
    let until = store.database_now_ms().expect("now") + 1000;
    let window = store.cost_window(since, until).expect("window");
    near(&window["total"]["usd"], 0.51, "the window's dollars");
    assert_eq!(
        window["landed"],
        json!(1),
        "t1 once, not t2 nor t4: {window}"
    );
    near(&window["usd_per_landing"], 0.51, "the window's per landing");
    let by_a = entry(&window, "missions", "mission", &json!(a));
    assert_eq!(by_a["landed"], json!(1), "{by_a}");
    near(&by_a["usd_per_landing"], 0.36, "mission A: t1 and t3");
    let by_b = entry(&window, "missions", "mission", &json!(b));
    assert_eq!(by_b["landed"], json!(0), "{by_b}");
    assert_eq!(by_b["usd_per_landing"], Value::Null, "{by_b}");

    // Just before the window: t2 landed, nothing spent. After it, nothing
    // landed: null, not a figure.
    let before = store.cost_window(since - 10, since).expect("window");
    assert_eq!(
        before["landed"],
        json!(1),
        "t2's landing, just before: {before}"
    );
    near(&before["usd_per_landing"], 0.0, "no dollars in it over t2");
    let empty = store.cost_window(until, until + 1000).expect("window");
    assert_eq!(empty["landed"], json!(0));
    assert_eq!(empty["usd_per_landing"], Value::Null);
}

#[test]
fn a_summary_gives_the_windows_total_landings_and_cost_per_landing() {
    let (store, _schema) = store("cost_summary");
    let since = store.database_now_ms().expect("now");

    // Two missions, three executions, two models (one priced nowhere), an
    // execution with both models, a landing in one mission and not the
    // other, and a message after the window.
    let a = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let (t1, w1) = spending_task(&store, a, ("w1", 1), (100_000, since + 1));
    usage(
        &store,
        &w1,
        1,
        ("msg_1z", "model-z", since + 2),
        [7000, 300, 20, 5],
    );
    reviewed(&store, t1, 1, true);
    let (t2, w2) = spending_task(&store, a, ("w2", 2), (40_000, since + 3));
    usage(
        &store,
        &w2,
        2,
        ("msg_2b", "model-x", since + 4),
        [1000, 2000, 3000, 4000],
    );
    store.task_cancel(t2).expect("cancel");
    let b = store
        .mission_add("Other", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let (t3, w3) = spending_task(&store, b, ("w3", 3), (60_000, since + 5));
    reviewed(&store, t3, 3, true);
    let later = store.database_now_ms().expect("now") + 60_000;
    usage(&store, &w3, 3, ("msg_3l", "model-x", later), [9, 9, 9, 9]);
    assert_eq!(store.cost_record(&prices(3.0)).expect("record"), 6);

    let until = store.database_now_ms().expect("now") + 1000;
    for (from, to, what) in [
        (since, until, "the window"),
        (since, later + 1, "with the later message"),
        (since + 3, until, "a part of it"),
        (until, until + 1000, "an empty window"),
    ] {
        let window = store.cost_window(from, to).expect("window");
        let summary = store.cost_summary(from, to).expect("summary");
        let expected = json!({
            "total": window["total"],
            "landed": window["landed"],
            "usd_per_landing": window["usd_per_landing"],
        });
        assert_eq!(summary, expected, "{what}: the window says {window}");
    }
    let window = store.cost_summary(since, until).expect("summary");
    assert_eq!(window["landed"], json!(2), "t1 and t3: {window}");
    assert!(
        usd(&window["total"]["unpriced_tokens"]) > 0.0,
        "model-z's tokens are unpriced: {window}"
    );
}

#[test]
fn a_trend_cuts_the_window_into_steps_each_with_its_dollars_and_unpriced_tokens() {
    const HOUR_MS: i64 = 3_600_000;
    let (store, _schema) = store("cost_trend");
    let (_mission, _task, worker) = one_task(&store);
    started(&store, &worker, 1);
    let since = store.database_now_ms().expect("now") - 4 * HOUR_MS;
    // A window of 3.5 hours in steps of an hour: four steps, the last half.
    let until = since + 3 * HOUR_MS + HOUR_MS / 2;
    let lines: [(&str, &str, i64, [u64; 4]); 6] = [
        ("before", "model-x", since - 1, [1_000_000, 0, 0, 0]),
        ("first", "model-x", since, [100_000, 0, 0, 0]),
        (
            "third_a",
            "model-x",
            since + 2 * HOUR_MS + 5,
            [200_000, 0, 0, 0],
        ),
        (
            "third_z",
            "model-z",
            since + 2 * HOUR_MS + 10,
            [7000, 300, 20, 5],
        ),
        ("last", "model-x", until - 1, [10_000, 0, 0, 0]),
        ("at_end", "model-x", until, [1_000_000, 0, 0, 0]),
    ];
    for (id, model, at, counts) in lines {
        usage(&store, &worker, 1, (id, model, at), counts);
    }
    assert_eq!(store.cost_record(&prices(3.0)).expect("record"), 6);

    let trend = store.cost_trend(since, until, HOUR_MS).expect("trend");
    let steps = trend.as_array().expect("an array");
    assert_eq!(steps.len(), 4, "{trend}");
    for (step, (usd, unpriced), index) in [(0.3, 0), (0.0, 0), (0.6, 7325), (0.03, 0)]
        .into_iter()
        .zip(0_i64..)
        .zip(steps)
        .map(|((expected, index), step)| (step, expected, index))
    {
        assert_eq!(step["since_ms"], json!(since + index * HOUR_MS), "{trend}");
        near(&step["usd"], usd, &format!("step {index}'s dollars"));
        assert_eq!(
            step["unpriced_tokens"],
            json!(unpriced),
            "step {index}: {trend}"
        );
    }
    // The steps add up to the window.
    let window = store.cost_window(since, until).expect("window");
    let summed: f64 = steps.iter().map(|step| usd(&step["usd"])).sum();
    near(&window["total"]["usd"], summed, "the window's total");
    assert_eq!(window["total"]["unpriced_tokens"], json!(7325));

    // A window an exact number of steps long has no extra step.
    let exact = store
        .cost_trend(since, since + 2 * HOUR_MS, HOUR_MS)
        .expect("trend");
    assert_eq!(exact.as_array().map(Vec::len), Some(2), "{exact}");
    // An empty window, or a step under a millisecond, is no step.
    assert_eq!(
        store.cost_trend(since, since, HOUR_MS).expect("trend"),
        json!([])
    );
    assert_eq!(store.cost_trend(since, until, 0).expect("trend"), json!([]));
}

fn session_message(id: &str, model: &str, at_ms: i64, counts: [i64; 4]) -> SessionMessage {
    let [input, output, cache_write, cache_read] = counts;
    SessionMessage {
        project: "-data-tmp-ccd-rev-x".to_string(),
        session_id: "s1".to_string(),
        message_id: id.to_string(),
        model: model.to_string(),
        at_ms,
        input,
        output,
        cache_write,
        cache_read,
    }
}

#[test]
fn the_sessions_count_apart_from_the_engines_and_never_twice() {
    let (store, schema) = store("cost_sessions");
    let (_mission, _task, worker) = one_task(&store);
    let now = store.database_now_ms().expect("now");
    started(&store, &worker, 1);
    // An engine's message, in the engines' record.
    usage(
        &store,
        &worker,
        1,
        ("msg_engine", "model-x", now),
        [100_000, 0, 0, 0],
    );
    store.cost_record(&prices(3.0)).expect("record");
    let engines = || store.cost_window(now - DAY_MS, now + 1000).expect("window");
    let before = engines();

    let mark = |offset: i64| SessionFile {
        file: "-data-tmp-ccd-rev-x/s1.jsonl".to_string(),
        inode: 42,
        offset_bytes: offset,
        length: offset,
        modified_ms: 7,
    };
    let first = [
        // $3 a million input: $0.30.
        session_message("msg_s1", "model-x", now, [100_000, 0, 0, 0]),
        // A model with no price: its tokens, no cost.
        session_message("msg_s2", "model-z", now + 1, [500, 50, 0, 0]),
        // The engine's message, also in this session's file: left to it.
        session_message("msg_engine", "model-x", now, [100_000, 0, 0, 0]),
    ];
    let done = store
        .cost_session_record(&prices(3.0), &first, &[mark(100)])
        .expect("record");
    assert_eq!((done.counted, done.changed, done.engine), (2, 0, 1));
    // Read again: nothing changes.
    let again = store
        .cost_session_record(&prices(3.0), &first[..2], &[mark(100)])
        .expect("record");
    assert_eq!((again.counted, again.changed, again.engine), (0, 0, 0));
    // A later line of msg_s1 whose usage grew: the larger usage, the
    // earlier time, repriced.
    let grown = store
        .cost_session_record(
            &prices(6.0),
            &[session_message(
                "msg_s1",
                "model-x",
                now + 5,
                [200_000, 0, 0, 0],
            )],
            &[mark(250)],
        )
        .expect("record");
    assert_eq!((grown.counted, grown.changed), (0, 1));
    assert_eq!(store.cost_session_files().expect("marks"), vec![mark(250)]);

    let sessions = store
        .cost_sessions(now - DAY_MS, now + 1000)
        .expect("sessions");
    near(
        &sessions["total"]["usd"],
        1.2,
        "msg_s1 at $6 a million, 200,000 tokens",
    );
    assert_eq!(sessions["total"]["unpriced_tokens"], 550);
    assert_eq!(sessions["rows"][0]["project"], "-data-tmp-ccd-rev-x");
    assert_eq!(sessions["counted_since_ms"], now);
    // A window ending at msg_s1's time leaves it out (the end is open).
    let edge = store.cost_sessions(now - DAY_MS, now).expect("sessions");
    assert_eq!(edge["rows"], json!([]), "{edge}");
    // A window before them holds none.
    let earlier = store
        .cost_sessions(now - DAY_MS, now - 1)
        .expect("sessions");
    assert_eq!(earlier["rows"], json!([]));
    near(&earlier["total"]["usd"], 0.0, "nothing before");
    // The engines' figures are as they were.
    assert_eq!(engines()["total"], before["total"]);
    assert_eq!(engines()["missions"], before["missions"]);

    let mut reader = common::as_login(&schema, "spawn_reader");
    let refused = reader
        .query_one(
            "select spawn_cost_session_record('{}'::jsonb, '[]'::jsonb, '[]'::jsonb)",
            &[],
        )
        .expect_err("the reader may not record");
    assert!(
        refused.code() == Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
        "{refused}"
    );
    // The reader reads the sessions' part, as /api/cost does.
    let rows = reader
        .query_one("select count(*) from spawn_cost_session_messages", &[])
        .expect("the reader reads")
        .get::<_, i64>(0);
    assert_eq!(rows, 2);
}

fn project_message(
    project: &str,
    id: &str,
    model: &str,
    at_ms: i64,
    counts: [i64; 4],
) -> SessionMessage {
    SessionMessage {
        project: project.to_string(),
        ..session_message(id, model, at_ms, counts)
    }
}

/// One session message as stored, read as the reader reads it:
/// `(model, at_ms, input_tokens, usd, rates)`.
fn held_message(schema: &str, id: &str) -> (String, i64, i64, f64, Value) {
    let mut reader = common::as_login(schema, "spawn_reader");
    let row = reader
        .query_one(
            "select model, at_ms, input_tokens, usd::float8, rates
               from spawn_cost_session_messages where message_id = $1",
            &[&id],
        )
        .expect("the message is held");
    (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4))
}

/// Cost coverage 1a R2 (rev-b F1): the sessions' answer whole. Each project
/// is a row with its own dollars, tokens by kind and unpriced tokens, most
/// costly first. The total sums every kind. The window's start is closed
/// and its end open. `counted_since_ms` is the earliest message the record
/// holds, whatever the window.
#[test]
fn the_sessions_answer_names_each_project_whole() {
    let (store, _schema) = store("cost_sessions_rows");
    let now = store.database_now_ms().expect("now");
    let reviewer = "-data-tmp-ccd-rev-x";
    let owner = "-home-ckl-projects-Spawn";
    let messages = [
        // $3, $15, $3.75 and $0.30 a million: 0.3 + 0.03 + 0.01125 + 0.012.
        project_message(
            reviewer,
            "msg_r1",
            "model-x",
            now,
            [100_000, 2_000, 3_000, 40_000],
        ),
        // No price: 560 tokens, no dollars.
        project_message(reviewer, "msg_r2", "model-z", now + 1, [500, 50, 7, 3]),
        // The costlier project: $1.20.
        project_message(owner, "msg_o1", "model-x", now + 2, [400_000, 0, 0, 0]),
    ];
    let done = store
        .cost_session_record(&prices(3.0), &messages, &[])
        .expect("record");
    assert_eq!((done.counted, done.invalid), (3, 0));

    // A window starting exactly at msg_r1's time holds it.
    let answer = store.cost_sessions(now, now + 1000).expect("sessions");
    let rows = answer["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2, "{answer}");
    assert_eq!(rows[0]["project"], owner, "most costly first: {answer}");
    near(&rows[0]["usd"], 1.2, "the owner's project");
    assert_eq!(
        rows[0]["tokens"],
        json!({"input": 400_000, "output": 0, "cache_write": 0, "cache_read": 0})
    );
    assert_eq!(rows[0]["unpriced_tokens"], 0);
    assert_eq!(rows[1]["project"], reviewer);
    near(&rows[1]["usd"], 0.353_25, "the reviewer's project");
    assert_eq!(
        rows[1]["tokens"],
        json!({"input": 100_500, "output": 2_050, "cache_write": 3_007, "cache_read": 40_003})
    );
    assert_eq!(rows[1]["unpriced_tokens"], 560);
    near(&answer["total"]["usd"], 1.553_25, "the total");
    assert_eq!(
        answer["total"]["tokens"],
        json!({"input": 500_500, "output": 2_050, "cache_write": 3_007, "cache_read": 40_003})
    );
    assert_eq!(answer["total"]["unpriced_tokens"], 560);
    assert_eq!(answer["counted_since_ms"], now);

    // A window starting after the reviewer's messages: the owner's alone,
    // and the record still counts since msg_r1.
    let later = store.cost_sessions(now + 2, now + 1000).expect("sessions");
    assert_eq!(later["rows"].as_array().map(Vec::len), Some(1), "{later}");
    assert_eq!(later["rows"][0]["project"], owner);
    assert_eq!(later["counted_since_ms"], now);
    // A window before every message: no rows, and the same since.
    let before = store.cost_sessions(now - DAY_MS, now).expect("sessions");
    assert_eq!(before["rows"], json!([]));
    assert_eq!(before["counted_since_ms"], now);
}

/// Cost coverage 1a R2 (rev-b F2, rev-h F2-F3): 041's fold. One call's lines
/// of a message give its largest usage at its earliest line's time and
/// model. A held message never shrinks. A larger usage is priced under the
/// held model at the call's prices, and its rates say so. An earlier time
/// moves the time alone. A line missing its model is not counted, and a
/// re-marked file takes its new inode.
#[test]
fn a_sessions_message_folds_by_its_largest_usage_and_earliest_line() {
    let (store, schema) = store("cost_sessions_fold");
    let now = store.database_now_ms().expect("now");
    let priced = |input: f64| {
        json!({
            "model-x": {"input": input, "output": 15, "cache_write": 3.75, "cache_read": 0.3},
            "model-y": {"input": 30.0, "output": 15, "cache_write": 3.75, "cache_read": 0.3},
        })
    };
    let line = |model: &str, at_ms: i64, input: i64| {
        session_message("msg_f", model, at_ms, [input, 0, 0, 0])
    };

    // One call, two lines of one message, the smaller first: the larger
    // usage, the earlier line's time and model.
    let first = store
        .cost_session_record(
            &priced(3.0),
            &[
                line("model-x", now, 100_000),
                line("model-y", now + 5, 200_000),
            ],
            &[],
        )
        .expect("record");
    assert_eq!((first.counted, first.changed), (1, 0));
    let (model, at_ms, input, usd, rates) = held_message(&schema, "msg_f");
    assert_eq!((model.as_str(), at_ms, input), ("model-x", now, 200_000));
    assert!((usd - 0.6).abs() < 1e-9, "200,000 at model-x's $3: {usd}");
    assert_eq!(rates, priced(3.0)["model-x"]);

    // A smaller usage read again (an earlier line after a mark reset): kept.
    let smaller = store
        .cost_session_record(&priced(3.0), &[line("model-x", now + 10, 50_000)], &[])
        .expect("record");
    assert_eq!((smaller.counted, smaller.changed), (0, 0));
    assert_eq!(held_message(&schema, "msg_f").2, 200_000, "never shrinks");

    // A larger usage on a line of another model, at new prices: the held
    // model's new price, and the rates it was priced with.
    let grown = store
        .cost_session_record(&priced(6.0), &[line("model-y", now + 20, 300_000)], &[])
        .expect("record");
    assert_eq!((grown.counted, grown.changed), (0, 1));
    let (model, at_ms, input, usd, rates) = held_message(&schema, "msg_f");
    assert_eq!((model.as_str(), at_ms, input), ("model-x", now, 300_000));
    assert!((usd - 1.8).abs() < 1e-9, "300,000 at model-x's $6: {usd}");
    assert_eq!(rates, priced(6.0)["model-x"]);

    // The same usage at an earlier time: the time moves, nothing else.
    let earlier = store
        .cost_session_record(&priced(3.0), &[line("model-x", now - 5, 300_000)], &[])
        .expect("record");
    assert_eq!((earlier.counted, earlier.changed), (0, 1));
    let (_, at_ms, input, usd, rates) = held_message(&schema, "msg_f");
    assert_eq!((at_ms, input), (now - 5, 300_000));
    assert!((usd - 1.8).abs() < 1e-9, "not repriced: {usd}");
    assert_eq!(rates, priced(6.0)["model-x"]);

    // A line with no model is not counted, and says so.
    let invalid = store
        .cost_session_record(
            &priced(3.0),
            &[session_message("msg_g", "", now, [1, 0, 0, 0])],
            &[],
        )
        .expect("record");
    assert_eq!(
        (invalid.counted, invalid.changed, invalid.invalid),
        (0, 0, 1)
    );

    // A file replaced (a new inode) and read from 0 again: its mark is the new one.
    let mark = |inode: i64, offset: i64| SessionFile {
        file: "-data-tmp-ccd-rev-x/s1.jsonl".to_string(),
        inode,
        offset_bytes: offset,
        length: offset,
        modified_ms: 7,
    };
    store
        .cost_session_record(&priced(3.0), &[], &[mark(42, 100)])
        .expect("mark");
    store
        .cost_session_record(&priced(3.0), &[], &[mark(43, 10)])
        .expect("re-mark");
    assert_eq!(
        store.cost_session_files().expect("marks"),
        vec![mark(43, 10)]
    );
}

/// Cost coverage 1a R2 (rev-h F1): a message the sessions counted first,
/// because their pass read an engine's transcript before the engine's fold,
/// is the engines' once their record takes it, from the live usage or the
/// archive. It is never counted on both sides. A copy left by a race is
/// removed the next time the sessions read that message.
#[test]
fn an_engine_fold_takes_a_message_the_sessions_counted_first() {
    let (store, schema) = store("cost_sessions_engine_after");
    let (_mission, _task, worker) = one_task(&store);
    let now = store.database_now_ms().expect("now");
    started(&store, &worker, 1);
    let sessions_usd = || {
        store
            .cost_sessions(now - DAY_MS, now + 1000)
            .expect("sessions")["total"]["usd"]
            .clone()
    };

    // The sessions read it first.
    let first = store
        .cost_session_record(
            &prices(3.0),
            &[session_message(
                "msg_live",
                "model-x",
                now,
                [100_000, 0, 0, 0],
            )],
            &[],
        )
        .expect("record");
    assert_eq!(first.counted, 1);
    near(&sessions_usd(), 0.3, "counted by the sessions first");
    // Then the engine's usage event is folded: the engines' alone.
    usage(
        &store,
        &worker,
        1,
        ("msg_live", "model-x", now),
        [100_000, 0, 0, 0],
    );
    assert_eq!(store.cost_record(&prices(3.0)).expect("record"), 1);
    near(&sessions_usd(), 0.0, "no longer the sessions'");
    near(
        &store.cost_window(now - DAY_MS, now + 1000).expect("window")["total"]["usd"],
        0.3,
        "the engines' once",
    );

    // The same through the archive's count.
    store
        .cost_session_record(
            &prices(3.0),
            &[session_message(
                "msg_old",
                "model-x",
                now + 1,
                [200_000, 0, 0, 0],
            )],
            &[],
        )
        .expect("record");
    near(&sessions_usd(), 0.6, "counted by the sessions first");
    let archived_now = store
        .cost_archive_record(
            &prices(3.0),
            &[archived(
                &worker.agent,
                "msg_old",
                "model-x",
                now + 1,
                200_000,
            )],
        )
        .expect("archive");
    assert_eq!(archived_now.counted, 1);
    near(&sessions_usd(), 0.0, "the archive took it");

    // A copy a race left (written here past the functions): the next read
    // of that message leaves it to the engines.
    let mut owner = common::admin(&schema);
    owner
        .execute(
            "insert into spawn_cost_session_messages
                 (message_id, project, session_id, model, at_ms, input_tokens, output_tokens,
                  cache_write_tokens, cache_read_tokens, usd, rates, recorded_at_ms)
             values ('msg_live', 'p', 's', 'model-x', $1, 100000, 0, 0, 0, 0.3, null, $1)",
            &[&now],
        )
        .expect("a raced copy");
    near(&sessions_usd(), 0.3, "the raced copy");
    let again = store
        .cost_session_record(
            &prices(3.0),
            &[session_message(
                "msg_live",
                "model-x",
                now,
                [100_000, 0, 0, 0],
            )],
            &[],
        )
        .expect("record");
    assert_eq!((again.counted, again.engine), (0, 1));
    near(&sessions_usd(), 0.0, "the raced copy is gone");
}

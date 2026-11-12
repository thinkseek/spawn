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
use spawn_postgres::{ArchiveCounted, ArchiveFile, ArchivedMessage, PostgresStore};

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

//! What model calls cost (objective 041): the cost step folds the Bridge's
//! `model.usage` events into one row per message (`spawn_cost_record`),
//! once whatever repeats it, with its largest usage at its earliest time,
//! for the task its agent held then, priced when it is recorded; the window
//! and the daily budgets are read from those rows.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use serde_json::{Value, json};
use spawn_core::event::{ENGINE, EventKind, ModelProfile, NewEvent, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
use spawn_core::port::EventLog;
use spawn_postgres::PostgresStore;

mod common;
use common::store;

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

#[test]
fn each_message_counts_once_for_its_task_priced_when_recorded() {
    let (store, _schema) = store("cost_record");
    let mission = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "w1", "brief", "default")
        .expect("assign");
    // An ephemeral worker's names (`spawn_task_assign`).
    let worker = Worker {
        agent: format!("agent_worker_w1_{task}"),
        session: format!("session_worker_w1_{task}"),
    };
    let now = store.database_now_ms().expect("now");
    started(&store, &worker, 1);
    // Two lines of one message (the same usage: one event), and a later
    // line whose usage grew: the message counts its largest usage, once.
    usage(
        &store,
        &worker,
        1,
        ("msg_1", "model-x", now),
        [1000, 2000, 10_000, 50_000],
    );
    usage(
        &store,
        &worker,
        1,
        ("msg_1", "model-x", now),
        [1000, 2000, 10_000, 50_000],
    );
    usage(
        &store,
        &worker,
        1,
        ("msg_1", "model-x", now + 5),
        [1000, 4000, 10_000, 50_000],
    );
    // Timed three days ago, before the task was assigned: still the task's.
    usage(
        &store,
        &worker,
        1,
        ("msg_2", "model-x", now - 3 * DAY_MS),
        [1_000_000, 0, 0, 0],
    );
    // A model with no price: its tokens, no cost.
    usage(
        &store,
        &worker,
        1,
        ("msg_3", "model-z", now),
        [5000, 500, 0, 0],
    );

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
        "the task, the message before it included",
    );
    assert_eq!(by_task["mission"], mission);
    near(
        &entry(&week, "missions", "mission", &json!(mission))["usd"],
        3.1155,
        "the mission",
    );
    near(
        &entry(&week, "agents", "agent", &json!(worker.agent))["usd"],
        3.1155,
        "the agent",
    );
    let z = entry(&week, "models", "model", &json!("model-z"));
    assert_eq!(z["usd"], Value::Null, "an unpriced model has no cost: {z}");
    let execution = entry(&week, "executions", "execution", &json!("exec_1"));
    assert_eq!(execution["task"], task);
    assert_eq!(execution["unpriced_tokens"], 5500);

    let day = store.cost_window(now - DAY_MS, now + 1000).expect("window");
    near(
        &day["total"]["usd"],
        0.1155,
        "the day leaves out the message of 3 days ago",
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

    // The daily budget: a positive amount, read with the day's spend.
    let refused = store.mission_configure(
        mission,
        None,
        "review",
        Some(&json!({"daily_usd": 0})),
        None,
    );
    assert!(refused.is_err(), "a budget of $0 is refused");
    store
        .mission_configure(
            mission,
            None,
            "review",
            Some(&json!({"daily_usd": 0.5})),
            None,
        )
        .expect("configure");
    let budgets = store.cost_budgets(now - 1000, now + 1000).expect("budgets");
    let budget = &budgets[0];
    assert_eq!(budget["mission"], mission);
    near(&budget["budget_usd"], 0.5, "the budget");
    near(&budget["spent_usd"], 0.7155, "the day's spend");
}

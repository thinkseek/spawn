//! The control stream (docs/fleet-architecture.md §10.1): missions, tasks and
//! residents are events in `spawn_events`, and the `mission_*` tables are
//! their fold.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::json;

mod common;
use common::{admin, store, url};
use spawn_postgres::PostgresStore;

/// Every row of the three tables, in a stable order.
fn tables(client: &mut Client) -> serde_json::Value {
    let row = client
        .query_one(
            "select jsonb_build_object(
               'missions', (select coalesce(jsonb_agg(to_jsonb(m) order by m.id), '[]') from mission_missions m),
               'residents', (select coalesce(jsonb_agg(to_jsonb(r) order by r.name), '[]') from mission_residents r),
               'tasks', (select coalesce(jsonb_agg(to_jsonb(t) order by t.id), '[]') from mission_tasks t))",
            &[],
        )
        .expect("read the tables");
    row.get(0)
}

fn control_types(client: &mut Client) -> Vec<String> {
    client
        .query(
            "select kind -> 'detail' -> 'record' ->> 'type'
               from spawn_events
              where session_id = 'session_control' and source = 'control'
                and event_name = 'control.recorded'
              order by session_seq",
            &[],
        )
        .expect("read the control stream")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

fn refold(client: &mut Client) {
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold the control stream");
}

#[test]
fn every_mission_change_is_an_event_and_the_tables_are_its_fold() {
    let (store, schema) = store("control_fold");
    let mission = store
        .mission_add("Ship it", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .resident_propose("core", "Owns the core")
        .expect("propose");
    store.resident_approve("core").expect("approve");
    let first = store
        .task_assign(mission, "resident", "core", "first", "default")
        .expect("assign to the resident");
    let second = store
        .task_assign(mission, "ephemeral", "Probe!", "second", "cheap")
        .expect("assign to an ephemeral worker");
    store.task_delivered(first, 41).expect("delivered");
    assert!(store.task_reported(first, "done once").expect("reported"));
    store.task_review(first, false, "again").expect("send back");
    store.task_delivered(first, 42).expect("delivered again");
    assert!(store.task_reported(first, "done twice").expect("reported"));
    store.task_review(first, true, "").expect("accept");
    assert!(
        !store.task_reported(first, "late").expect("a late report"),
        "an accepted task takes no report"
    );
    store.task_cancel(second).expect("cancel");
    store.resident_retire("core").expect("retire");
    store
        .mission_close(mission, "done", "shipped")
        .expect("close");

    let mut client = admin(&schema);
    assert_eq!(
        control_types(&mut client),
        [
            "mission.added",
            "resident.proposed",
            "resident.approved",
            "task.assigned",
            "task.assigned",
            "task.delivered",
            "task.reported",
            "task.reviewed",
            "task.delivered",
            "task.reported",
            "task.reviewed",
            "task.cancelled",
            "resident.retired",
            "mission.closed",
        ]
    );

    let live = tables(&mut client);
    assert_eq!(live["tasks"][0]["state"], "accepted");
    assert_eq!(live["tasks"][0]["revisions"], 1);
    assert_eq!(live["tasks"][0]["report"], "done twice");
    assert_eq!(
        live["tasks"][1]["agent_id"],
        format!("agent_worker_probe_{second}")
    );
    assert_eq!(live["tasks"][0]["model"], "default");
    assert_eq!(live["tasks"][1]["model"], "cheap");
    assert_eq!(live["missions"][0]["state"], "done");
    assert_eq!(live["residents"][0]["state"], "retired");

    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );

    // The stream belongs to no agent.
    let projected: i64 = client
        .query_one(
            "select count(*) from spawn_session_state where session_id = 'session_control'",
            &[],
        )
        .expect("count")
        .get(0);
    assert_eq!(projected, 0);
}

#[test]
fn a_refused_change_records_nothing() {
    let (store, schema) = store("control_refused");
    let mission = store.mission_add("One", &json!([]), "human").expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    assert!(store.task_review(task, true, "").is_err(), "no report yet");
    assert!(store.resident_approve("nobody").is_err());
    assert!(
        store
            .task_assign(mission, "resident", "nobody", "brief", "default")
            .is_err()
    );
    assert!(
        store
            .task_assign(mission, "ephemeral", "w", "brief", "opus")
            .is_err(),
        "a profile is default or cheap"
    );
    let mut client = admin(&schema);
    assert_eq!(
        control_types(&mut client),
        ["mission.added", "task.assigned"]
    );
}

#[test]
fn rows_from_before_the_stream_are_imported_once() {
    let (store, schema) = store("control_import");
    let mission = store.mission_add("Old", &json!([]), "human").expect("add");
    store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    let mut client = admin(&schema);
    // A database from before the control stream: rows, and no events.
    client
        .batch_execute(
            "delete from spawn_events where session_id = 'session_control';
             update mission_tasks set state = 'reported', report = 'old report';",
        )
        .expect("simulate an old database");
    let before = tables(&mut client);

    store.migrate().expect("migrate");
    store.migrate().expect("migrate again");
    assert_eq!(
        control_types(&mut client),
        ["legacy.imported", "legacy.imported"]
    );
    assert_eq!(tables(&mut client), before);
    refold(&mut client);
    assert_eq!(tables(&mut client), before);

    // New work continues on the imported rows, and ids do not collide.
    let task: i64 = before["tasks"][0]["id"].as_i64().expect("task id");
    store
        .task_review(task, true, "")
        .expect("accept the old report");
    let next = store
        .task_assign(mission, "ephemeral", "w", "more", "default")
        .expect("assign");
    assert!(next > task);
}

#[test]
fn the_task_model_column_migrates_in_place_and_old_records_fold_as_default() {
    let (store, schema) = store("control_model");
    let mission = store.mission_add("Old", &json!([]), "human").expect("add");
    store
        .task_assign(mission, "ephemeral", "w", "brief", "cheap")
        .expect("assign");
    let mut client = admin(&schema);
    // A record written without the model, then a table from before the
    // column.
    client
        .batch_execute(&format!(
            "select spawn_control_record(jsonb_build_object(
                 'type', 'task.assigned', 'id', 900, 'mission_id', {mission},
                 'kind', 'ephemeral', 'agent_id', 'agent_worker_w_900',
                 'session_id', 'session_worker_w_900', 'brief', 'old',
                 'created_by', 'human'));
             alter table mission_tasks drop column model;"
        ))
        .expect("simulate an old record and an old table");
    store.migrate().expect("migrate");
    let models = |client: &mut Client| -> Vec<String> {
        client
            .query("select model from mission_tasks order by id", &[])
            .expect("read the models")
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    // The new column takes its default; a refold restores the log's value.
    assert_eq!(models(&mut client), ["default", "default"]);
    refold(&mut client);
    assert_eq!(models(&mut client), ["cheap", "default"]);
    // A caller from before `--model` (four arguments) still assigns.
    let row = client
        .query_one(
            "select spawn_task_assign($1, 'ephemeral', 'w', 'brief')",
            &[&mission],
        )
        .expect("assign with four arguments");
    let task: i64 = row.get(0);
    let model: String = client
        .query_one("select model from mission_tasks where id = $1", &[&task])
        .expect("read the task")
        .get(0);
    assert_eq!(model, "default");
}

#[test]
#[allow(clippy::too_many_lines)]
fn gate_verdicts_landings_and_halts_fold_into_the_mission() {
    let (store, schema) = store("control_landing");
    let mission = store
        .mission_add("Gated", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(
            mission,
            Some("/controller/rsi/scenarios"),
            "review",
            None,
            None,
        )
        .expect("configure");
    assert!(
        store
            .mission_configure(mission, Some("relative/path"), "review", None, None)
            .is_err(),
        "a gate is an absolute path"
    );
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    store.task_review(task, true, "").expect("accept");

    let gated = |verdict: &str, extra: serde_json::Value| {
        let mut record = json!({"type": "task.gated", "id": task, "verdict": verdict,
                                "base": "b1", "log": format!("log {verdict}"), "scenarios": 7});
        record
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        store.integrator_record(&record).expect("record a verdict");
    };
    let mut client = admin(&schema);
    let row = |client: &mut Client| tables(client);

    gated("rejected", json!({"note": "check-tests fails"}));
    let after = row(&mut client);
    assert_eq!(after["tasks"][0]["state"], "revise");
    assert_eq!(after["tasks"][0]["review_note"], "check-tests fails");
    assert_eq!(after["missions"][0]["gate_rejections"], 1);
    assert_eq!(
        after["missions"][0]["baseline"], "b1",
        "the first gate sets the baseline"
    );
    assert_eq!(after["missions"][0]["scenario_floor"], 7);

    store.task_delivered(task, 2).expect("delivered again");
    assert!(store.task_reported(task, "fixed").expect("reported"));
    store.task_review(task, true, "").expect("accept again");
    gated("aborted", json!({}));
    assert_eq!(row(&mut client)["missions"][0]["gate_aborts"], 1);
    assert_eq!(
        row(&mut client)["tasks"][0]["state"],
        "accepted",
        "an abort leaves the task"
    );
    gated("stale", json!({}));
    assert_eq!(row(&mut client)["tasks"][0]["state"], "accepted");
    gated(
        "accepted",
        json!({"candidate": "c1", "metric": "METRIC x candidate=1"}),
    );
    let after = row(&mut client);
    assert_eq!(after["tasks"][0]["state"], "landing");
    assert_eq!(after["tasks"][0]["candidate"], "c1");
    assert_eq!(after["missions"][0]["gate_rejections"], 0);
    assert_eq!(after["missions"][0]["gate_aborts"], 0);
    assert!(
        store.task_cancel(task).is_err(),
        "landing work is not cancelled"
    );

    store
        .integrator_record(&json!({"type": "task.landed", "id": task, "from": "b1", "to": "c1"}))
        .expect("landed");
    let after = row(&mut client);
    assert_eq!(after["tasks"][0]["state"], "landed");
    assert_eq!(after["missions"][0]["baseline"], "c1");

    let source = "/tmp/source.git";
    assert_eq!(
        store.source_moved(source, "b1", "d1").expect("moved"),
        0,
        "only a mission whose baseline is `from` moves"
    );
    assert_eq!(store.source_moved(source, "c1", "d1").expect("moved"), 1);
    assert_eq!(row(&mut client)["missions"][0]["baseline"], "d1");

    store
        .integrator_record(
            &json!({"type": "mission.halted", "id": mission, "reason": "main moved"}),
        )
        .expect("halt");
    assert_eq!(row(&mut client)["missions"][0]["halted"], "main moved");
    assert!(
        store
            .integrator_record(&json!({"type": "mission.closed", "id": mission}))
            .is_err(),
        "the integrator records only its own types"
    );
    store.mission_resume(mission).expect("resume");
    let after = row(&mut client);
    assert!(after["missions"][0]["halted"].is_null());
    assert!(after["missions"][0]["baseline"].is_null());
    assert_eq!(
        after["missions"][0]["scenario_floor"], 0,
        "a resume takes the scenarios as they are now"
    );
    assert!(
        store.mission_resume(mission).is_err(),
        "not halted any more"
    );

    let live = row(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
}

#[test]
fn objective_tasks_go_to_the_missions_resident_and_keep_their_history() {
    let (store, schema) = store("control_objectives");
    let mission = store
        .mission_add("Improve", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let objectives = json!({"backlog": "/controller/rsi/backlog", "resident": "rsi"});
    assert!(
        store
            .mission_configure(mission, None, "gate", None, Some(&objectives))
            .is_err(),
        "objectives need a gate"
    );
    store
        .mission_configure(
            mission,
            Some("/controller/rsi/scenarios"),
            "gate",
            None,
            Some(&objectives),
        )
        .expect("configure");
    assert!(
        store.objective_assign(mission, 2, None, "brief").is_err(),
        "the resident is not approved yet"
    );
    store
        .resident_propose("rsi", "Pursues objectives")
        .expect("propose");
    store.resident_approve("rsi").expect("approve");

    let task = store
        .objective_assign(
            mission,
            2,
            Some("/controller/rsi/scenarios/002.sh"),
            "brief",
        )
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    store
        .integrator_record(
            &json!({"type": "task.gated", "id": task, "verdict": "rejected",
                                   "reason": "scenario fails on the candidate", "base": "b"}),
        )
        .expect("a verdict");

    assert_eq!(
        store.objective_landings(mission).expect("landings"),
        json!([]),
        "nothing landed yet"
    );
    let history = store.objective_history(mission, 2, 6).expect("history");
    assert_eq!(history[0]["task"], task);
    assert_eq!(history[0]["verdict"], "rejected");
    assert_eq!(history[0]["reason"], "scenario fails on the candidate");

    the_rsi_page_reads_it(&store, mission, task);

    let mut client = admin(&schema);
    let live = tables(&mut client);
    assert_eq!(live["tasks"][0]["agent_id"], "agent_resident_rsi");
    assert_eq!(live["tasks"][0]["created_by"], "spawn-integrate");
    assert_eq!(
        live["tasks"][0]["scenario"],
        "/controller/rsi/scenarios/002.sh"
    );
    refold(&mut client);
    assert_eq!(tables(&mut client), live);
}

#[test]
fn a_landing_moves_the_baseline_of_every_mission_on_its_source() {
    let (store, schema) = store("control_shared_source");
    let source = json!(["/tmp/shared.git"]);
    let first = store.mission_add("First", &source, "human").expect("add");
    let second = store.mission_add("Second", &source, "human").expect("add");
    let other = store
        .mission_add("Elsewhere", &json!(["/tmp/other.git"]), "human")
        .expect("add");
    for mission in [first, second, other] {
        store
            .mission_configure(
                mission,
                Some("/controller/rsi/scenarios"),
                "gate",
                None,
                None,
            )
            .expect("configure");
    }
    let mut tasks = Vec::new();
    for mission in [first, second, other] {
        let task = store
            .task_assign(mission, "ephemeral", "w", "brief", "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        store
            .integrator_record(
                &json!({"type": "task.gated", "id": task, "verdict": "rejected",
                                       "base": "b0", "note": "no"}),
            )
            .expect("a first verdict sets the baselines");
        tasks.push(task);
    }
    store.task_delivered(tasks[0], 2).expect("delivered");
    assert!(store.task_reported(tasks[0], "done").expect("reported"));
    store
        .integrator_record(
            &json!({"type": "task.gated", "id": tasks[0], "verdict": "accepted",
                                   "base": "b0", "candidate": "c1"}),
        )
        .expect("accepted");
    store
        .integrator_record(
            &json!({"type": "task.landed", "id": tasks[0], "from": "b0", "to": "c1"}),
        )
        .expect("landed");
    let mut client = admin(&schema);
    let live = tables(&mut client);
    assert_eq!(live["missions"][0]["baseline"], "c1");
    assert_eq!(
        live["missions"][1]["baseline"], "c1",
        "same source, same main"
    );
    assert_eq!(
        live["missions"][2]["baseline"], "b0",
        "another source is untouched"
    );
    refold(&mut client);
    assert_eq!(tables(&mut client), live);
}

#[test]
fn a_verdict_does_not_bring_back_a_cancelled_task() {
    let (store, _schema) = store("control_cancelled_verdict");
    let mission = store
        .mission_add("Gated", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(
            mission,
            Some("/controller/rsi/scenarios"),
            "gate",
            None,
            None,
        )
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    // The gate is running when the supervisor cancels the task.
    store.task_cancel(task).expect("cancel");
    for record in [
        json!({"type": "task.gated", "id": task, "verdict": "accepted", "base": "b", "candidate": "c"}),
        json!({"type": "task.landed", "id": task, "from": "b", "to": "c"}),
        json!({"type": "task.aborted", "id": task, "reason": "budget"}),
    ] {
        assert!(
            store.integrator_record(&record).is_err(),
            "{record} must not touch a cancelled task"
        );
    }
}

#[test]
fn accepted_work_of_a_done_mission_still_lands_and_a_dropped_one_lets_it_go() {
    let (store, schema) = store("control_closed");
    let accepted_task = |text: &str| {
        let mission = store
            .mission_add(text, &json!(["/tmp/source.git"]), "human")
            .expect("add");
        store
            .mission_configure(mission, Some("/scenarios"), "review", None, None)
            .expect("configure");
        let task = store
            .task_assign(mission, "ephemeral", "w", "brief", "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        store.task_review(task, true, "").expect("accept");
        (mission, task)
    };
    let gated_ids = || -> Vec<i64> {
        store
            .gated_missions()
            .expect("gated")
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|row| row["id"].as_i64())
            .collect()
    };
    let (done, done_task) = accepted_task("Done");
    let (dropped, dropped_task) = accepted_task("Dropped");
    store.mission_close(done, "done", "shipped").expect("close");
    store
        .mission_close(dropped, "dropped", "gone")
        .expect("close");
    assert_eq!(
        gated_ids(),
        vec![done],
        "the integrator still lands done work"
    );

    let mut client = admin(&schema);
    let state = |client: &mut Client, task: i64| -> String {
        client
            .query_one("select state from mission_tasks where id = $1", &[&task])
            .expect("task")
            .get(0)
    };
    assert_eq!(state(&mut client, dropped_task), "cancelled");
    store
        .integrator_record(
            &json!({"type": "task.gated", "id": done_task, "verdict": "accepted",
                                   "base": "b1", "candidate": "c1", "scenarios": 1}),
        )
        .expect("gate");
    store
        .integrator_record(
            &json!({"type": "task.landed", "id": done_task, "from": "b1", "to": "c1"}),
        )
        .expect("landed");
    assert_eq!(state(&mut client, done_task), "landed");
    assert!(gated_ids().is_empty(), "nothing left to land");

    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
}

#[test]
fn one_integrator_holds_the_lock() {
    let (first, schema) = store("control_integrator_lock");
    let second = PostgresStore::connect_in_schema(&url(), &schema).expect("second connection");
    assert!(
        first.integrator_try_lock().expect("lock"),
        "the first takes it"
    );
    assert!(
        !second.integrator_try_lock().expect("lock"),
        "the second waits for the first"
    );
    drop(first);
    // The lock dies with its connection; the server notices asynchronously.
    let mut held = false;
    for _ in 0..50 {
        held = second.integrator_try_lock().expect("lock");
        if held {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(held, "a dead integrator never blocks the next one");
}

/// The RSI page's reads on the objective mission built above: the mission
/// with its resident and its tasks' gate verdicts, newest task first; one
/// task with its logs; and the board's last verdict per task.
fn the_rsi_page_reads_it(store: &PostgresStore, mission: i64, task: i64) {
    let page = store.objective_missions().expect("objective missions");
    assert_eq!(page.as_array().map(Vec::len), Some(1));
    assert_eq!(page[0]["id"], mission);
    assert_eq!(page[0]["accept"], "gate");
    assert_eq!(page[0]["resident"]["session_id"], "session_resident_rsi");
    assert_eq!(page[0]["tasks"][0]["id"], task);
    assert_eq!(page[0]["tasks"][0]["gates"][0]["verdict"], "rejected");
    assert_eq!(
        page[0]["tasks"][0]["gates"][0]["reason"],
        "scenario fails on the candidate"
    );
    assert!(
        page[0]["tasks"][0]["gates"][0]["log"].is_null(),
        "no logs in the overview"
    );
    let detail = store
        .objective_task(task)
        .expect("one task")
        .expect("it exists");
    assert_eq!(detail["mission_id"], mission);
    assert_eq!(detail["brief"], "brief");
    assert_eq!(detail["gates"][0]["verdict"], "rejected");
    assert!(store.objective_task(task + 1000).expect("read").is_none());
    let gates = store.last_gates().expect("last gates");
    assert_eq!(gates[task.to_string()]["verdict"], "rejected");
}

#[test]
fn a_tasks_stages_are_its_records_in_order_with_the_time_each_took() {
    let (store, schema) = store("control_stages");
    let mission = store
        .mission_add("Timed", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(
            mission,
            Some("/controller/rsi/scenarios"),
            "review",
            None,
            None,
        )
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    let other = store
        .task_assign(mission, "ephemeral", "w", "other", "default")
        .expect("assign another");
    assert_eq!(
        task, mission,
        "the mission's records carry the task's id and are not its stages"
    );
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    store.task_review(task, false, "again").expect("send back");
    store.task_delivered(task, 2).expect("delivered again");
    assert!(store.task_reported(task, "fixed").expect("reported again"));
    store.task_review(task, true, "").expect("accept");
    store
        .integrator_record(
            &json!({"type": "task.gated", "id": task, "verdict": "accepted",
                                   "base": "b1", "candidate": "c1"}),
        )
        .expect("the gate accepts");
    store
        .integrator_record(&json!({"type": "task.landed", "id": task, "from": "b1", "to": "c1"}))
        .expect("landed");
    store.task_cancel(other).expect("cancel the other");

    // The task's records at known times, in seconds after the assignment.
    let seconds = [0, 3385, 3881, 3900, 3960, 4560, 4570, 4748, 4748];
    let base: i64 = 1_800_000_000_000;
    let mut client = admin(&schema);
    let seqs: Vec<i64> = client
        .query(
            "select session_seq from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'id' = $1
                and kind -> 'detail' -> 'record' ->> 'type' like 'task.%'
              order by session_seq",
            &[&task.to_string()],
        )
        .expect("the task's records")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(seqs.len(), seconds.len());
    for (seq, after) in seqs.iter().zip(seconds) {
        client
            .execute(
                "update spawn_events set recorded_at_ms = $1
                  where session_id = 'session_control' and session_seq = $2",
                &[&(base + after * 1000), seq],
            )
            .expect("set the record's time");
    }

    let expected = json!([
        {"stage": "assigned", "at_ms": base},
        {"stage": "delivered", "at_ms": base + 3_385_000, "took_ms": 3_385_000},
        {"stage": "reported", "at_ms": base + 3_881_000, "took_ms": 496_000},
        {"stage": "reviewed", "verdict": "rejected", "at_ms": base + 3_900_000, "took_ms": 19_000},
        {"stage": "delivered", "at_ms": base + 3_960_000, "took_ms": 60_000},
        {"stage": "reported", "at_ms": base + 4_560_000, "took_ms": 600_000},
        {"stage": "reviewed", "verdict": "accepted", "at_ms": base + 4_570_000, "took_ms": 10_000},
        {"stage": "gated", "verdict": "accepted", "at_ms": base + 4_748_000, "took_ms": 178_000},
        {"stage": "landed", "at_ms": base + 4_748_000, "took_ms": 0},
    ]);
    assert_eq!(store.task_stages(task).expect("stages"), expected);
    let cancelled = store.task_stages(other).expect("the other's stages");
    let names: Vec<&str> = cancelled
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|stage| stage["stage"].as_str())
        .collect();
    assert_eq!(names, ["assigned", "cancelled"]);
    assert_eq!(store.task_stages(task + 1000).expect("no task"), json!([]));

    // The board carries them, and every login that reads the board may.
    let board = store.mission_board(10).expect("board");
    assert_eq!(board["missions"][0]["tasks"][0]["stages"], expected);
    for login in ["spawn_supervisor", "spawn_reader"] {
        client
            .batch_execute(&format!("set session authorization {login}"))
            .expect("become the login");
        let stages: serde_json::Value = client
            .query_one("select spawn_task_stages($1)", &[&task])
            .expect("the login reads the stages")
            .get(0);
        assert_eq!(stages, expected, "{login}");
        if login == "spawn_supervisor" {
            assert!(
                client
                    .query_one("select count(*) from spawn_events", &[])
                    .is_err(),
                "the supervisor still reads no events"
            );
        }
        client
            .batch_execute("reset session authorization")
            .expect("back to the owner");
    }
}

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
    store
        .task_review(first, false, "again", None)
        .expect("send back");
    store.task_delivered(first, 42).expect("delivered again");
    assert!(store.task_reported(first, "done twice").expect("reported"));
    store.task_review(first, true, "", None).expect("accept");
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
    assert!(
        store.task_review(task, true, "", None).is_err(),
        "no report yet"
    );
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
        .task_review(task, true, "", None)
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
    store.task_review(task, true, "", None).expect("accept");

    let gated = |verdict: &str, extra: serde_json::Value| {
        // Rejected once, so every later verdict judges revision 1.
        let revision = i32::from(verdict != "rejected");
        let mut record = json!({"type": "task.gated", "id": task, "verdict": verdict,
                                "revision": revision,
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
    store
        .task_review(task, true, "", None)
        .expect("accept again");
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

/// Objective 16: `incompatible` (the head passes the gate alone and fails
/// only merged with main) sends the task back like `conflict`: `revise`, one
/// more revision, the note kept, `gate_rejections` where it was and
/// `gate_aborts` reset. Three in a row count nothing toward the halt. The
/// objective history, the stall facts' `revise` record and the store's
/// check all know it.
#[test]
#[allow(clippy::too_many_lines)]
fn an_incompatible_verdict_sends_the_task_back_and_counts_no_rejection() {
    let (store, schema) = store("control_incompatible");
    let mission = store
        .mission_add("Improve", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let objectives = json!({"backlog": "/controller/rsi/backlog", "resident": "rsi"});
    store
        .mission_configure(
            mission,
            Some("/controller/rsi/scenarios"),
            "gate",
            None,
            Some(&objectives),
        )
        .expect("configure");
    store
        .resident_propose("rsi", "Pursues objectives")
        .expect("propose");
    store.resident_approve("rsi").expect("approve");
    let task = store
        .objective_assign(mission, 16, None, "Objective 16: semantic conflicts")
        .expect("assign");
    let mut client = admin(&schema);

    let report = |revision: i64| {
        store.task_delivered(task, revision + 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
    };
    let gated = |revision: i64, verdict: &str, note: &str| {
        store.integrator_record(&json!({
            "type": "task.gated", "id": task, "revision": revision, "verdict": verdict,
            "base": "m1", "scenarios": 3, "candidate": "c", "head": "h", "head_base": "m0",
            "head_verdict": "accepted", "main_commits": ["m1"],
            "reason": "candidate tests do not build", "log": "error[E0063]", "note": note}))
    };

    // One real rejection and one abort first: the count is 1, aborts 1.
    report(0);
    gated(0, "rejected", "wrong").expect("rejected");
    report(1);
    store
        .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 1,
                                   "verdict": "aborted", "base": "m1", "reason": "timeout"}))
        .expect("aborted");
    let after = tables(&mut client);
    assert_eq!(after["missions"][0]["gate_rejections"], 1);
    assert_eq!(after["missions"][0]["gate_aborts"], 1);

    for revision in 1..=3 {
        let note = format!("adapt to m1 ({revision})");
        gated(revision, "incompatible", &note).expect("the store accepts incompatible");
        let after = tables(&mut client);
        let row = &after["tasks"][0];
        assert_eq!(row["state"], "revise", "{after}");
        assert_eq!(row["revisions"], revision + 1, "{after}");
        assert_eq!(row["review_note"], note.as_str());
        assert_eq!(
            after["missions"][0]["gate_rejections"], 1,
            "an incompatible verdict is no rejection"
        );
        assert_eq!(after["missions"][0]["gate_aborts"], 0, "as on conflict");
        assert!(after["missions"][0]["halted"].is_null(), "{after}");

        // The dispatcher measures the step from the verdict that sent it back.
        let facts = store.stall_facts().expect("stall facts");
        let open = facts["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|row| row["id"] == task)
            .expect("the task is open");
        assert_eq!(open["state"], "revise");
        assert_eq!(open["record"]["type"], "task.gated", "{open}");
        if revision < 3 {
            report(revision + 1);
        }
    }

    let history = store.objective_history(mission, 16, 10).expect("history");
    let verdicts: Vec<&str> = history
        .as_array()
        .expect("history")
        .iter()
        .filter_map(|row| row["verdict"].as_str())
        .collect();
    assert_eq!(
        verdicts,
        vec![
            "incompatible",
            "incompatible",
            "incompatible",
            "aborted",
            "rejected"
        ]
    );
    assert!(
        store
            .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 4,
                                       "verdict": "semantic", "base": "m1"}))
            .is_err(),
        "an unknown verdict is still refused"
    );

    let live = tables(&mut client);
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
            &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "rejected",
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

/// The error text of a refused call.
fn refusal<T: std::fmt::Debug>(result: Result<T, spawn_core::port::StoreError>) -> String {
    result.expect_err("refused").to_string()
}

#[test]
fn an_objective_mission_keeps_up_to_its_workers_in_flight_each_on_its_own_agent() {
    let (store, schema) = store("control_objective_workers");
    let mission = store
        .mission_add("Improve faster", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .resident_propose("wide", "Pursues objectives")
        .expect("propose");
    store.resident_approve("wide").expect("approve");
    let objectives = json!({"backlog": "/controller/rsi/backlog", "resident": "wide",
                            "workers": 2});
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, Some(&objectives))
        .expect("configure with two workers");

    // The resident has none open: it takes the first task.
    let first = store
        .objective_assign(mission, 1, None, "brief one")
        .expect("first");
    // A second task for the same objective is refused while one is in flight.
    let doubled = refusal(store.objective_assign(mission, 1, None, "again"));
    assert!(
        doubled.contains("already has a task in flight"),
        "{doubled}"
    );
    // The resident has one open: the next goes to a per-task worker.
    let second = store
        .objective_assign(mission, 2, None, "brief two")
        .expect("second");
    // At its limit, the mission is refused a third.
    let full = refusal(store.objective_assign(mission, 3, None, "brief three"));
    assert!(full.contains("2 tasks in flight"), "{full}");

    let mut client = admin(&schema);
    let task = |client: &mut Client, id: i64| -> serde_json::Value {
        client
            .query_one(
                "select to_jsonb(t) from mission_tasks t where id = $1",
                &[&id],
            )
            .expect("a task")
            .get(0)
    };
    let one = task(&mut client, first);
    assert_eq!(one["agent_id"], "agent_resident_wide");
    assert_eq!(one["kind"], "resident");
    let two = task(&mut client, second);
    assert_eq!(two["kind"], "ephemeral");
    assert_eq!(
        two["agent_id"],
        format!("agent_worker_objective_{second}").as_str()
    );
    assert_eq!(
        two["session_id"],
        format!("session_worker_objective_{second}").as_str()
    );
    assert_eq!(two["brief"], "brief two");
    assert_eq!(two["created_by"], "spawn-integrate");
    assert_eq!(two["objective"], 2);

    // A task that ends frees its slot; the resident, free again, takes the
    // next one, and the ended objective may be assigned again.
    store.task_cancel(first).expect("cancel");
    let third = store
        .objective_assign(mission, 1, None, "brief one again")
        .expect("a free slot");
    assert_eq!(task(&mut client, third)["agent_id"], "agent_resident_wide");

    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
}

#[test]
fn a_mission_without_workers_keeps_one_task_in_flight() {
    let (store, _schema) = store("control_objective_one");
    let mission = store
        .mission_add("Improve", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .resident_propose("narrow", "Pursues objectives")
        .expect("propose");
    store.resident_approve("narrow").expect("approve");
    let objectives = json!({"backlog": "/controller/rsi/backlog", "resident": "narrow"});
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, Some(&objectives))
        .expect("configure");
    store
        .objective_assign(mission, 1, None, "brief")
        .expect("first");
    let full = refusal(store.objective_assign(mission, 2, None, "brief"));
    assert!(full.contains("its workers: 1"), "{full}");
}

#[test]
fn a_mission_is_configured_with_one_to_sixteen_workers() {
    let (store, _schema) = store("control_objective_limit");
    let mission = store
        .mission_add("Improve", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let with = |workers: serde_json::Value| json!({"backlog": "/controller/rsi/backlog", "resident": "r", "workers": workers});
    for refused in [json!(0), json!(17), json!(2.5), json!("3"), json!(-1)] {
        let error = refusal(store.mission_configure(
            mission,
            Some("/scenarios"),
            "gate",
            None,
            Some(&with(refused.clone())),
        ));
        assert!(error.contains("from 1 to 16"), "{refused}: {error}");
    }
    for accepted in [1, 16] {
        store
            .mission_configure(
                mission,
                Some("/scenarios"),
                "gate",
                None,
                Some(&with(json!(accepted))),
            )
            .expect("in range");
    }
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
                &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "rejected",
                                       "base": "b0", "note": "no"}),
            )
            .expect("a first verdict sets the baselines");
        tasks.push(task);
    }
    store.task_delivered(tasks[0], 2).expect("delivered");
    assert!(store.task_reported(tasks[0], "done").expect("reported"));
    store
        .integrator_record(
            &json!({"type": "task.gated", "revision": 1, "id": tasks[0], "verdict": "accepted",
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
        json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "accepted", "base": "b", "candidate": "c"}),
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
        store.task_review(task, true, "", None).expect("accept");
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
            &json!({"type": "task.gated", "revision": 0, "id": done_task, "verdict": "accepted",
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
fn a_halted_mission_keeps_its_baseline_when_the_owner_moves_its_source() {
    let (store, schema) = store("control_moved_halted");
    let source = "/tmp/moved.git";
    let mission = || {
        let mission = store
            .mission_add("On the source", &json!([source]), "human")
            .expect("add");
        store
            .mission_configure(mission, Some("/scenarios"), "gate", None, None)
            .expect("configure");
        let task = store
            .task_assign(mission, "ephemeral", "w", "brief", "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        store
            .integrator_record(
                &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "rejected",
                                       "base": "b0", "note": "no"}),
            )
            .expect("a first verdict sets the baseline");
        mission
    };
    let (running, halted) = (mission(), mission());
    store
        .integrator_record(&json!({"type": "mission.halted", "id": halted, "reason": "stop"}))
        .expect("halt");
    assert_eq!(store.source_moved(source, "b0", "d1").expect("moved"), 1);
    let mut client = admin(&schema);
    let baselines = |client: &mut Client| -> Vec<String> {
        client
            .query(
                "select baseline from mission_missions where id = any($1) order by id",
                &[&vec![running, halted]],
            )
            .expect("baselines")
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    assert_eq!(baselines(&mut client), ["d1", "b0"], "the halted one stays");
    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(tables(&mut client), live);

    // A record from before the record named its missions folds as it did.
    client
        .batch_execute(&format!(
            "select spawn_control_record(jsonb_build_object(
                 'type', 'source.moved', 'source', '{source}', 'from', 'b0', 'to', 'e1'))"
        ))
        .expect("an earlier record");
    assert_eq!(baselines(&mut client), ["d1", "e1"]);
}

#[test]
fn a_dropped_mission_lands_nothing_even_with_a_push_in_flight() {
    let (store, schema) = store("control_dropped_landing");
    // A gated mission whose task the gate passed: landing.
    let landing_task = || {
        let mission = store
            .mission_add("Land", &json!(["/tmp/source.git"]), "human")
            .expect("add");
        store
            .mission_configure(mission, Some("/scenarios"), "review", None, None)
            .expect("configure");
        let task = store
            .task_assign(mission, "ephemeral", "w", "brief", "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        store.task_review(task, true, "", None).expect("accept");
        store
            .integrator_record(
                &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "accepted",
                                       "base": "b1", "candidate": "c1", "scenarios": 1}),
            )
            .expect("gate");
        (mission, task)
    };
    let mut client = admin(&schema);
    let state = |client: &mut Client, task: i64| -> String {
        client
            .query_one("select state from mission_tasks where id = $1", &[&task])
            .expect("task")
            .get(0)
    };

    // Dropping the mission cancels its landing.
    let (mission, task) = landing_task();
    assert_eq!(state(&mut client, task), "landing");
    store
        .mission_close(mission, "dropped", "gone")
        .expect("drop");
    assert_eq!(state(&mut client, task), "cancelled");
    assert!(store.landing_hold(task).expect("hold").is_none());
    assert!(
        store
            .integrator_record(
                &json!({"type": "task.landed", "id": task, "from": "b1", "to": "c1"})
            )
            .is_err(),
        "a cancelled landing records no landing"
    );

    // While the integrator holds a landing for its push, a drop waits.
    let (mission, task) = landing_task();
    let hold = store.landing_hold(task).expect("hold").expect("landing");
    client
        .batch_execute("set lock_timeout = '300ms'")
        .expect("bound the wait");
    let error = client
        .execute(
            "select spawn_mission_close($1, 'dropped', 'gone')",
            &[&mission],
        )
        .expect_err("the drop waits for the push");
    assert_eq!(
        error.code(),
        Some(&postgres::error::SqlState::LOCK_NOT_AVAILABLE)
    );
    // The push landed; its record goes in before the hold is let go.
    store
        .integrator_record(&json!({"type": "task.landed", "id": task, "from": "b1", "to": "c1"}))
        .expect("landed");
    drop(hold);
    // The bound above showed the wait; this drop may still wait briefly
    // behind other tests' store locks on the shared server.
    client
        .batch_execute("set lock_timeout = 0")
        .expect("unbound the wait");
    client
        .execute(
            "select spawn_mission_close($1, 'dropped', 'gone')",
            &[&mission],
        )
        .expect("the drop goes through after the push");
    assert_eq!(
        state(&mut client, task),
        "landed",
        "landed work stays landed"
    );

    // A landing a fold from before left under a dropped mission is
    // recorded cancelled instead of landed.
    let (mission, task) = landing_task();
    store
        .mission_close(mission, "dropped", "gone")
        .expect("drop");
    client
        .execute(
            "update mission_tasks set state = 'landing' where id = $1",
            &[&task],
        )
        .expect("the earlier fold's table");
    assert!(store.landing_hold(task).expect("hold").is_none());
    assert_eq!(state(&mut client, task), "cancelled");
    assert_eq!(
        control_types(&mut client).last().map(String::as_str),
        Some("task.cancelled")
    );
}

#[test]
fn a_done_missions_landing_that_stays_stale_is_aborted_after_three_tries() {
    let (store, schema) = store("control_done_stale");
    let landing_task = |text: &str| {
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
        store.task_review(task, true, "", None).expect("accept");
        store
            .integrator_record(
                &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "accepted",
                                       "base": "b1", "candidate": "c1", "scenarios": 1}),
            )
            .expect("gate");
        (mission, task)
    };
    let mut client = admin(&schema);
    let state = |client: &mut Client, task: i64| -> String {
        client
            .query_one("select state from mission_tasks where id = $1", &[&task])
            .expect("task")
            .get(0)
    };
    let gate_stale = |task: i64| {
        store
            .integrator_record(
                &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "stale"}),
            )
            .expect("stale")
    };

    // An open mission's stale landings go back to be judged, every time:
    // its worker may still push.
    let (_open, open_task) = landing_task("Open");
    for _ in 0..4 {
        gate_stale(open_task);
        assert_eq!(state(&mut client, open_task), "accepted");
    }

    // A done mission's: twice, then the third aborts it, and the integrator
    // has nothing left to judge.
    let (done, done_task) = landing_task("Done");
    gate_stale(done_task);
    store.mission_close(done, "done", "shipped").expect("close");
    for _ in 0..2 {
        gate_stale(done_task);
        assert_eq!(
            state(&mut client, done_task),
            "accepted",
            "stale before the bound"
        );
    }
    gate_stale(done_task);
    assert_eq!(state(&mut client, done_task), "cancelled");
    assert_eq!(
        control_types(&mut client).last().map(String::as_str),
        Some("task.aborted")
    );
    let note: String = client
        .query_one(
            "select review_note from mission_tasks where id = $1",
            &[&done_task],
        )
        .expect("task")
        .get(0);
    assert_eq!(
        note,
        format!("the landing went stale 3 times after mission {done} was done")
    );
    let gated: Vec<i64> = store
        .gated_missions()
        .expect("gated")
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|row| row["id"].as_i64())
        .collect();
    assert!(!gated.contains(&done), "{gated:?}");
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

/// Move a task's control records, oldest first, to `base` plus `seconds`.
fn set_record_times(client: &mut Client, task: i64, base: i64, seconds: &[i64]) {
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
    store
        .task_review(task, false, "again", None)
        .expect("send back");
    store.task_delivered(task, 2).expect("delivered again");
    assert!(store.task_reported(task, "fixed").expect("reported again"));
    store.task_review(task, true, "", None).expect("accept");
    store
        .integrator_record(
            &json!({"type": "task.gated", "revision": 1, "id": task, "verdict": "accepted",
                                   "base": "b1", "candidate": "c1"}),
        )
        .expect("the gate accepts");
    store
        .integrator_record(&json!({"type": "task.landed", "id": task, "from": "b1", "to": "c1"}))
        .expect("landed");
    store.task_cancel(other).expect("cancel the other");

    // The task's records at known times, in seconds after the assignment.
    let base: i64 = 1_800_000_000_000;
    let mut client = admin(&schema);
    set_record_times(
        &mut client,
        task,
        base,
        &[0, 3385, 3881, 3900, 3960, 4560, 4570, 4748, 4748],
    );

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
        let role = common::login(&schema, login);
        client
            .batch_execute(&format!("set session authorization {role}"))
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

#[test]
fn host_disk_records_are_owner_only_facts_the_fold_ignores() {
    let (store, schema) = store("control_host_disk");
    let mut client = admin(&schema);
    let record = |path: &str, state: &str, free: u64| {
        json!({"type": "host.disk", "label": "workspace", "path": path,
               "state": state, "free_bytes": free, "warn_bytes": 20, "floor_bytes": 2})
    };
    assert_eq!(store.host_disk().expect("none yet"), json!([]));
    store
        .host_record(&record("/ws", "low", 10))
        .expect("record low");
    store
        .host_record(&record("/tmp", "low", 5))
        .expect("another path");
    let recovered = store
        .host_record(&record("/ws", "recovered", 30))
        .expect("record recovered");
    let latest = store.host_disk().expect("latest per path");
    let summary: Vec<(String, String, u64)> = latest
        .as_array()
        .expect("a list")
        .iter()
        .map(|record| {
            (
                record["path"].as_str().unwrap_or_default().to_string(),
                record["state"].as_str().unwrap_or_default().to_string(),
                record["free_bytes"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("/tmp".to_string(), "low".to_string(), 5),
            ("/ws".to_string(), "recovered".to_string(), 30)
        ]
    );
    assert_eq!(latest[1]["event_id"], json!(recovered));
    assert!(latest[1]["at_ms"].as_i64().is_some());

    for bad in [
        json!({"type": "task.landed", "id": 1}),
        record("/ws", "full", 1),
        json!({"type": "host.disk", "path": "/ws", "state": "low"}),
    ] {
        assert!(store.host_record(&bad).is_err(), "refused: {bad}");
    }
    // The fold has nothing to do with them, and refolding replays them.
    let before = tables(&mut client);
    refold(&mut client);
    assert_eq!(tables(&mut client), before);
    assert_eq!(control_types(&mut client), ["host.disk"; 3]);

    let supervisor = common::login(&schema, "spawn_supervisor");
    client
        .batch_execute(&format!("set session authorization {supervisor}"))
        .expect("become the supervisor");
    assert!(
        client
            .query_one("select spawn_host_record($1)", &[&record("/ws", "low", 1)])
            .is_err(),
        "the supervisor records no host facts"
    );
}

/// The id of every mission the integrator visits.
fn gated_mission_ids(store: &PostgresStore) -> Vec<i64> {
    store
        .gated_missions()
        .expect("gated")
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|row| row["id"].as_i64())
        .collect()
}

fn task_state(client: &mut Client, task: i64) -> String {
    client
        .query_one("select state from mission_tasks where id = $1", &[&task])
        .expect("task")
        .get(0)
}

/// Audit L7: a gated mission that accepts by `gate` is closed done while its
/// task is delivered. The report arrives (the dispatcher still collects it),
/// the task is `reported`, and that is the state the integrator judges in
/// this mode, but the integrator never visits the mission again: it visits
/// done missions only with `accepted` or `landing` work. Nothing else moves
/// a `reported` task of a `gate` mission, so it is never judged, landed or
/// cancelled.
#[test]
fn a_gate_missions_report_that_arrives_after_done_is_judged() {
    let (store, schema) = store("control_l7_gate");
    let mission = store
        .mission_add("Gate", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    store
        .mission_close(mission, "done", "shipped")
        .expect("close");
    let mut client = admin(&schema);
    assert_eq!(
        task_state(&mut client, task),
        "delivered",
        "close leaves it"
    );
    assert!(store.task_reported(task, "done").expect("reported"));
    assert_eq!(task_state(&mut client, task), "reported");
    assert!(
        gated_mission_ids(&store).contains(&mission),
        "the integrator never visits mission {mission} again; task {task} stays reported"
    );
}

/// Audit L7, the drop: a mission dropped while its task is delivered keeps
/// the task, which then goes on through report, review and revise, and is
/// delivered to its worker again, for a mission that no longer exists.
#[test]
fn a_dropped_missions_delivered_task_is_cancelled() {
    let (store, schema) = store("control_l7_drop");
    let mission = store
        .mission_add("Drop", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "review", None, None)
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    store
        .mission_close(mission, "dropped", "gone")
        .expect("drop");
    let mut client = admin(&schema);
    let after_drop = task_state(&mut client, task);
    // Walk on, to show where it goes: a report, a send-back, a new delivery.
    let reported = store.task_reported(task, "done").expect("report");
    let sent_back = reported && store.task_review(task, false, "again", None).is_ok();
    let redelivered = sent_back && store.task_delivered(task, 2).is_ok();
    let delivered_again = task_state(&mut client, task) == "delivered";
    assert!(
        after_drop == "cancelled",
        "after the drop the task is {after_drop}; reported {reported}, sent back {sent_back}, \
         delivered again {}",
        redelivered && delivered_again
    );
}

/// Every task state, in the order the walk below reaches them.
const TASK_STATES: [&str; 8] = [
    "assigned",
    "delivered",
    "reported",
    "revise",
    "accepted",
    "cancelled",
    "landing",
    "landed",
];

/// Walk a new task of `mission` (assigned to `worker`, a resident's name
/// for `kind` = resident) to `state`, through the checked functions.
fn task_in(store: &PostgresStore, mission: i64, kind: &str, worker: &str, state: &str) -> i64 {
    let task = store
        .task_assign(mission, kind, worker, "brief", "default")
        .expect("assign");
    if state == "assigned" {
        return task;
    }
    if state == "cancelled" {
        store.task_cancel(task).expect("cancel");
        return task;
    }
    store.task_delivered(task, 1).expect("delivered");
    if state == "delivered" {
        return task;
    }
    assert!(store.task_reported(task, "done").expect("reported"));
    match state {
        // A mission its gate alone accepts takes no review: its gate sends
        // work back, and only a mission reviewed before it was switched to
        // the gate holds accepted work (its gate is `/scenarios`).
        "revise" if store.task_review(task, false, "again", None).is_err() => {
            store
                .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                           "verdict": "rejected", "note": "again"}))
                .expect("the gate sends it back");
        }
        "accepted" if store.task_review(task, true, "", None).is_err() => {
            store
                .mission_configure(mission, Some("/scenarios"), "review", None, None)
                .expect("review for a while");
            store.task_review(task, true, "", None).expect("accept");
            store
                .mission_configure(mission, Some("/scenarios"), "gate", None, None)
                .expect("back to the gate");
        }
        "reported" | "revise" | "accepted" => {}
        "landing" | "landed" => {
            store
                .integrator_record(
                    &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "accepted",
                                           "base": "b", "candidate": format!("c{task}")}),
                )
                .expect("gate passed");
            if state == "landed" {
                store
                    .integrator_record(&json!({"type": "task.landed", "id": task,
                                               "from": "b", "to": format!("c{task}")}))
                    .expect("landed");
            }
        }
        other => panic!("no walk to {other}"),
    }
    task
}

/// The ids of the tasks the integrator is handed, by mission.
fn gated_task_ids(store: &PostgresStore, mission: i64) -> Vec<i64> {
    store
        .gated_missions()
        .expect("gated")
        .as_array()
        .expect("rows")
        .iter()
        .filter(|row| row["id"].as_i64() == Some(mission))
        .flat_map(|row| row["tasks"].as_array().cloned().unwrap_or_default())
        .filter_map(|task| task["id"].as_i64())
        .collect()
}

/// Audit L7, the sweep: a mission closed done or dropped, accepting by the
/// gate or by review, or without a gate, with one task in every state. After
/// the close every task is closed, or the integrator is handed it; what goes
/// on is exactly what the closure rule names.
#[test]
fn a_closed_missions_tasks_are_all_closed_or_handed_to_the_integrator() {
    let (store, schema) = store("control_closure_sweep");
    let mut client = admin(&schema);
    for close in ["done", "dropped"] {
        for accept in ["gate", "review", "none"] {
            let gated = accept != "none";
            let mission = store
                .mission_add(
                    &format!("{close} {accept}"),
                    &json!(["/tmp/source.git"]),
                    "human",
                )
                .expect("add");
            if gated {
                store
                    .mission_configure(mission, Some("/scenarios"), accept, None, None)
                    .expect("configure");
            }
            let tasks: Vec<(&str, i64)> = TASK_STATES
                .iter()
                .filter(|state| gated || !["landing", "landed"].contains(state))
                .map(|state| (*state, task_in(&store, mission, "ephemeral", "w", state)))
                .collect();
            store
                .mission_close(mission, close, "closed")
                .expect("close");
            let handed = gated_task_ids(&store, mission);
            for (before, task) in &tasks {
                let after = task_state(&mut client, *task);
                let goes_on = match (close, gated) {
                    ("done", true) => {
                        ["delivered", "reported", "accepted", "landing"].contains(before)
                    }
                    _ => false,
                };
                let closed = ["cancelled", "landed"].contains(&after.as_str())
                    || (after == "accepted" && !gated);
                let case = format!("{close}, {accept}: a task {before} is {after}");
                assert!(closed || handed.contains(task), "{case}, and not handed on");
                if goes_on {
                    assert_eq!(after, *before, "{case}");
                    assert!(handed.contains(task), "{case}, and not handed on");
                } else if ["cancelled", "landed"].contains(before)
                    || (*before == "accepted" && !gated)
                {
                    assert_eq!(after, *before, "{case}: a closed task stays");
                } else {
                    assert_eq!(after, "cancelled", "{case}");
                }
            }
            if close == "done" && gated {
                // A report still comes in, and is handed on.
                let delivered = tasks[1].1;
                assert!(store.task_reported(delivered, "late").expect("report"));
                assert!(gated_task_ids(&store, mission).contains(&delivered));
                // Nothing new is asked of a worker: a send-back is refused,
                // a gate rejection ends the task.
                let reported = tasks[2].1;
                assert!(store.task_review(reported, false, "again", None).is_err());
                store
                    .integrator_record(&json!({"type": "task.gated", "revision": 0, "id": reported,
                                               "verdict": "rejected", "note": "no"}))
                    .expect("rejected");
                assert_eq!(task_state(&mut client, reported), "cancelled");
                // A report may still be accepted, and lands; under a gate
                // alone, the gate accepts it, never a review.
                if accept == "review" {
                    store
                        .task_review(delivered, true, "", None)
                        .expect("accept");
                    assert_eq!(task_state(&mut client, delivered), "accepted");
                } else {
                    assert!(store.task_review(delivered, true, "", None).is_err());
                    assert_eq!(task_state(&mut client, delivered), "reported");
                }
            }
        }
    }
    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
}

/// A retired resident's work that needs it again (queued, running,
/// reported) is cancelled; work that needs nothing more from it (accepted,
/// passed the gate) still lands. Nothing sends work back to it.
#[test]
fn a_retired_residents_work_is_cancelled_unless_it_only_has_to_land() {
    let (store, schema) = store("control_closure_retired");
    let mut client = admin(&schema);
    store
        .resident_propose("core", "Owns the core")
        .expect("propose");
    store.resident_approve("core").expect("approve");
    let mission = store
        .mission_add("Core work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "review", None, None)
        .expect("configure");
    let tasks: Vec<(&str, i64)> = ["assigned", "delivered", "reported", "accepted", "landing"]
        .iter()
        .map(|state| (*state, task_in(&store, mission, "resident", "core", state)))
        .collect();
    store.resident_retire("core").expect("retire");
    let handed = gated_task_ids(&store, mission);
    for (before, task) in &tasks {
        let after = task_state(&mut client, *task);
        if ["accepted", "landing"].contains(before) {
            assert_eq!(after, *before, "a task {before} still lands");
            assert!(handed.contains(task), "a task {before} is handed on");
        } else {
            assert_eq!(after, "cancelled", "a task {before} of a retired resident");
            assert!(!handed.contains(task));
        }
    }
    // A gate rejection of its accepted work ends it instead of sending it back.
    let accepted = tasks[3].1;
    store
        .integrator_record(&json!({"type": "task.gated", "revision": 0, "id": accepted,
                                   "verdict": "rejected", "note": "no"}))
        .expect("rejected");
    assert_eq!(task_state(&mut client, accepted), "cancelled");

    // A fold from before the rule left a report open (as an earlier
    // retirement did): no transition moves it, and the next migration
    // records it cancelled.
    let reported = tasks[2].1;
    let left_as = |client: &mut Client, state: &str| {
        client
            .execute(
                "update mission_tasks set state = $2 where id = $1",
                &[&reported, &state],
            )
            .expect("a task an earlier retirement left open");
    };
    left_as(&mut client, "revise");
    store.task_delivered(reported, 2).expect("deliver");
    assert_eq!(
        task_state(&mut client, reported),
        "revise",
        "not delivered again"
    );
    left_as(&mut client, "delivered");
    assert!(!store.task_reported(reported, "late").expect("report"));
    assert_eq!(
        task_state(&mut client, reported),
        "delivered",
        "no report taken"
    );
    store.migrate().expect("migrate again");
    assert_eq!(task_state(&mut client, reported), "cancelled");
    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
}

/// A done gated mission's delivered task stays with the integrator, so its
/// budget still holds: the integrator sees it with the mission's budget and
/// may abort it. A dropped mission's delivered task is cancelled instead,
/// and no budget is left to check.
#[test]
fn a_done_missions_running_task_is_still_held_to_its_budget() {
    let (store, schema) = store("control_closure_budget");
    let mut client = admin(&schema);
    let running = |close: &str| {
        let mission = store
            .mission_add(close, &json!(["/tmp/source.git"]), "human")
            .expect("add");
        store
            .mission_configure(
                mission,
                Some("/scenarios"),
                "review",
                Some(&json!({"minutes": 1})),
                None,
            )
            .expect("configure");
        let task = task_in(&store, mission, "ephemeral", "w", "delivered");
        store
            .mission_close(mission, close, "closed")
            .expect("close");
        (mission, task)
    };
    let (done, done_task) = running("done");
    let (dropped, dropped_task) = running("dropped");

    let gated = store.gated_missions().expect("gated");
    let rows = gated.as_array().expect("rows");
    let row = rows
        .iter()
        .find(|row| row["id"].as_i64() == Some(done))
        .expect("the done mission is handed on");
    assert_eq!(row["budget"], json!({"minutes": 1}));
    assert_eq!(row["tasks"][0]["id"], done_task);
    assert_eq!(row["tasks"][0]["state"], "delivered");
    assert!(row["tasks"][0]["delivered_at_ms"].is_i64());
    store
        .integrator_record(&json!({"type": "task.aborted", "id": done_task,
                                   "reason": "the task ran past its 1-minute budget"}))
        .expect("abort over budget");
    assert_eq!(task_state(&mut client, done_task), "cancelled");

    assert!(rows.iter().all(|row| row["id"].as_i64() != Some(dropped)));
    assert_eq!(task_state(&mut client, dropped_task), "cancelled");
    assert!(
        gated_mission_ids(&store).is_empty(),
        "nothing is left to land or to hold to a budget"
    );
}

/// A gate-mode mission with one task reported at revision 0.
fn gate_mode_task(store: &PostgresStore, source: &str) -> (i64, i64) {
    let mission = store
        .mission_add("Gate alone", &json!([source]), "human")
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
    assert!(store.task_reported(task, "rev0").expect("reported"));
    (mission, task)
}

/// Audit #10: a gate verdict names the revision it judged. A verdict on
/// revision 0 that arrives after the task went back and reported revision 1
/// is refused: it would land revision 0's candidate over unjudged work.
#[test]
fn a_gate_verdict_lands_only_on_the_revision_it_judged() {
    let (store, schema) = store("control_gate_revision");
    let (_, task) = gate_mode_task(&store, "/tmp/source.git");
    store
        .integrator_record(
            &json!({"type": "task.gated", "id": task, "verdict": "rejected",
                                   "revision": 0, "base": "b0", "note": "redo"}),
        )
        .expect("a verdict on revision 0");
    store.task_delivered(task, 2).expect("delivered again");
    assert!(store.task_reported(task, "rev1").expect("reported again"));
    let mut client = admin(&schema);
    let before = tables(&mut client);
    assert!(
        store
            .integrator_record(
                &json!({"type": "task.gated", "id": task, "verdict": "accepted",
                                       "revision": 0, "base": "b0", "candidate": "c0"})
            )
            .is_err(),
        "a verdict on revision 0 does not land revision 1"
    );
    assert!(
        store
            .integrator_record(
                &json!({"type": "task.gated", "id": task, "verdict": "accepted",
                                       "base": "b0", "candidate": "c0"})
            )
            .is_err(),
        "a verdict that names no revision is refused"
    );
    assert_eq!(tables(&mut client), before, "nothing moved");
    assert_eq!(task_state(&mut client, task), "reported");

    // The fold refuses a mismatch too: a record in the log that names
    // another revision changes nothing, live or refolded.
    client
        .execute(
            "select spawn_control_record($1)",
            &[
                &json!({"type": "task.gated", "id": task, "verdict": "accepted",
                      "revision": 0, "base": "b0", "candidate": "c0"}),
            ],
        )
        .expect("record directly");
    assert_eq!(tables(&mut client), before, "the fold skips a mismatch");
    refold(&mut client);
    assert_eq!(tables(&mut client), before);

    store
        .integrator_record(
            &json!({"type": "task.gated", "id": task, "verdict": "accepted",
                                   "revision": 1, "base": "b0", "candidate": "c1"}),
        )
        .expect("a verdict on revision 1");
    let after = tables(&mut client);
    assert_eq!(after["tasks"][0]["state"], "landing");
    assert_eq!(after["tasks"][0]["candidate"], "c1");
}

/// Audit #10: the gate alone judges a gate-mode mission's reports. The
/// supervisor is not told of them and may not review them, so no review
/// can send back the revision the gate is judging.
#[test]
fn a_gate_mode_report_is_neither_news_nor_reviewed() {
    let (store, _schema) = store("control_gate_no_review");
    let (_, gated) = gate_mode_task(&store, "/tmp/source.git");
    let reviewed = store
        .mission_add("Reviewed", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let task = store
        .task_assign(reviewed, "ephemeral", "v", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 3).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));

    let news = store.mission_news().expect("news");
    let ids: Vec<i64> = news["reports"]
        .as_array()
        .expect("reports")
        .iter()
        .map(|report| report["id"].as_i64().expect("id"))
        .collect();
    assert_eq!(ids, vec![task], "only the reviewed mission's report");
    assert!(
        store.task_review(gated, false, "again", None).is_err(),
        "a gate-mode report is not sent back by review"
    );
    assert!(
        store.task_review(gated, true, "", None).is_err(),
        "nor accepted by review"
    );
}

/// Audit #11: a landing moves another mission's baseline only from where
/// that mission's baseline is (the landing's `from`), and never a halted
/// one's. Main moved b0 -> b1 outside the gate; mission A, with no baseline
/// yet, gates on b1 and lands b1 -> c2. B, at b0, keeps b0 and halts on its
/// next pass instead of taking c2 and laundering the move.
#[test]
fn a_landing_moves_only_the_missions_at_its_from() {
    let (store, schema) = store("control_landing_from");
    let source = json!(["/tmp/shared.git"]);
    let missions: Vec<i64> = ["A", "B", "C", "D"]
        .iter()
        .map(|name| {
            let mission = store.mission_add(name, &source, "human").expect("add");
            store
                .mission_configure(
                    mission,
                    Some("/controller/rsi/scenarios"),
                    "gate",
                    None,
                    None,
                )
                .expect("configure");
            mission
        })
        .collect();
    let mut tasks = Vec::new();
    for &mission in &missions {
        let task = store
            .task_assign(mission, "ephemeral", "w", "brief", "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        tasks.push(task);
    }
    // B at b0, C at b1, D at b1 but halted; A has no baseline yet.
    for (task, base) in [(tasks[1], "b0"), (tasks[2], "b1"), (tasks[3], "b1")] {
        store
            .integrator_record(
                &json!({"type": "task.gated", "id": task, "verdict": "aborted",
                                       "revision": 0, "base": base}),
            )
            .expect("a first verdict sets the baseline");
    }
    store
        .integrator_record(&json!({"type": "mission.halted", "id": missions[3], "reason": "x"}))
        .expect("halt D");
    store
        .integrator_record(
            &json!({"type": "task.gated", "id": tasks[0], "verdict": "accepted",
                                   "revision": 0, "base": "b1", "candidate": "c2"}),
        )
        .expect("accepted");
    store
        .integrator_record(
            &json!({"type": "task.landed", "id": tasks[0], "from": "b1", "to": "c2"}),
        )
        .expect("landed");
    let mut client = admin(&schema);
    let live = tables(&mut client);
    let baseline = |index: usize| live["missions"][index]["baseline"].clone();
    assert_eq!(baseline(0), "c2", "its own mission");
    assert_eq!(baseline(1), "b0", "B was not at the landing's from");
    assert_eq!(baseline(2), "c2", "C was");
    assert_eq!(baseline(3), "b1", "D is halted");
    refold(&mut client);
    assert_eq!(tables(&mut client), live);
}

/// Audit #14: only a directory on the integrator's host receives agents'
/// branches, so a gate on a remote source would wait for a push nobody
/// makes. Configuring one is refused.
#[test]
fn a_gate_needs_a_source_that_receives_branches() {
    let (store, _schema) = store("control_remote_gate");
    for source in ["https://example.com/repo.git", "git@example.com:repo.git"] {
        let mission = store
            .mission_add("Remote", &json!([source]), "human")
            .expect("add");
        assert!(
            store
                .mission_configure(
                    mission,
                    Some("/controller/rsi/scenarios"),
                    "gate",
                    None,
                    None
                )
                .is_err(),
            "no gate on {source}"
        );
        assert!(
            store
                .mission_configure(mission, None, "review", None, None)
                .is_ok(),
            "settings without a gate are fine on {source}"
        );
    }
}

/// Audit #10, the review's half: a review names the revision of the report
/// it judged. The supervisor reads revision 0; a human sends it back; the
/// worker reports revision 1; the supervisor's accept of revision 0 is
/// refused instead of accepting revision 1 unread.
#[test]
fn a_review_lands_only_on_the_revision_it_read() {
    let (store, schema) = store("control_review_revision");
    let mission = store
        .mission_add("Reviewed", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "rev0").expect("reported"));
    store
        .task_review(task, false, "again", None)
        .expect("a human sends it back");
    store.task_delivered(task, 2).expect("delivered again");
    assert!(store.task_reported(task, "rev1").expect("reported again"));
    let mut client = admin(&schema);
    let before = tables(&mut client);
    assert!(
        store.task_review(task, true, "", Some(0)).is_err(),
        "an accept of revision 0 does not accept revision 1"
    );
    assert_eq!(tables(&mut client), before);

    // The fold skips a review of another revision, live and refolded.
    client
        .execute(
            "select spawn_control_record($1)",
            &[
                &json!({"type": "task.reviewed", "id": task, "accepted": true, "note": "",
                      "revision": 0}),
            ],
        )
        .expect("record directly");
    assert_eq!(tables(&mut client), before);
    refold(&mut client);
    assert_eq!(tables(&mut client), before);

    store
        .task_review(task, true, "", Some(1))
        .expect("an accept of revision 1");
    assert_eq!(task_state(&mut client, task), "accepted");
    let named: Vec<Option<String>> = client
        .query(
            "select kind -> 'detail' -> 'record' ->> 'revision' from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.reviewed'
              order by session_seq",
            &[],
        )
        .expect("reviews")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        named,
        vec![
            Some("0".to_string()),
            Some("0".to_string()),
            Some("1".to_string())
        ],
        "every review names its revision (the human's, the current one)"
    );
}

/// How many `task.cancelled` records the control stream holds for `task`.
fn cancelled_records(client: &mut Client, task: i64) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events
              where session_id = 'session_control' and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'
                and kind -> 'detail' -> 'record' ->> 'id' = $1",
            &[&task.to_string()],
        )
        .expect("count the records")
        .get(0)
}

/// The last stage `spawn task stages` shows for `task`.
fn last_stage(store: &PostgresStore, task: i64) -> String {
    let stages = store.task_stages(task).expect("stages");
    stages
        .as_array()
        .and_then(|stages| stages.last())
        .and_then(|stage| stage["stage"].as_str())
        .unwrap_or("none")
        .to_string()
}

/// Each of `cancelled` is cancelled, its stages end with `cancelled`, and the
/// stream holds exactly one record of it; each of `kept` is in its state and
/// has no such record.
fn assert_cancelled_once(
    store: &PostgresStore,
    client: &mut Client,
    cancelled: &[i64],
    kept: &[(i64, &str)],
    when: &str,
) {
    for &task in cancelled {
        assert_eq!(task_state(client, task), "cancelled", "task {task} {when}");
        assert_eq!(
            last_stage(store, task),
            "cancelled",
            "task {task}'s stages {when}"
        );
        assert_eq!(
            cancelled_records(client, task),
            1,
            "task {task}'s records {when}"
        );
    }
    for &(task, expected) in kept {
        assert_eq!(task_state(client, task), expected, "task {task} {when}");
        assert_eq!(
            cancelled_records(client, task),
            0,
            "task {task}'s records {when}"
        );
    }
}

/// An assigned, a reported and an accepted task, for `worker` of `kind`.
fn three_tasks(store: &PostgresStore, mission: i64, kind: &str, worker: &str) -> [i64; 3] {
    let tasks = ["assigned", "reported", "accepted"].map(|brief| {
        store
            .task_assign(mission, kind, worker, brief, "default")
            .expect("assign")
    });
    for (&task, command) in tasks[1..].iter().zip([70, 71]) {
        store.task_delivered(task, command).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
    }
    store.task_review(tasks[2], true, "", None).expect("accept");
    tasks
}

#[test]
fn a_retirement_records_task_cancelled_for_each_task_it_cancels() {
    let (store, schema) = store("control_retire_records");
    let mission = store
        .mission_add("Core work", &json!([]), "human")
        .expect("add");
    store
        .resident_propose("core", "Owns the core")
        .expect("propose");
    store.resident_approve("core").expect("approve");
    let [assigned, reported, accepted] = three_tasks(&store, mission, "resident", "core");
    store.resident_retire("core").expect("retire");

    let mut client = admin(&schema);
    // An accepted task of a mission without a gate goes on.
    let kept = [(accepted, "accepted")];
    assert_cancelled_once(
        &store,
        &mut client,
        &[assigned, reported],
        &kept,
        "after the retirement",
    );
    let types = control_types(&mut client);
    assert_eq!(
        types[types.len() - 3..],
        ["resident.retired", "task.cancelled", "task.cancelled"],
        "the writer appends the records after the retirement"
    );

    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
    assert_eq!(
        control_types(&mut client),
        types,
        "a refold appends nothing"
    );
    store.migrate().expect("migrate again");
    assert_eq!(
        control_types(&mut client),
        types,
        "migrating again appends nothing"
    );
    assert_cancelled_once(
        &store,
        &mut client,
        &[assigned, reported],
        &kept,
        "after the replays",
    );
}

#[test]
fn a_mission_close_records_task_cancelled_for_each_task_it_cancels() {
    let (store, schema) = store("control_close_records");
    let done = store.mission_add("Done", &json!([]), "human").expect("add");
    let dropped = store
        .mission_add("Dropped", &json!([]), "human")
        .expect("add");
    let [done_assigned, done_reported, done_accepted] = three_tasks(&store, done, "ephemeral", "a");
    let [dropped_assigned, dropped_reported, dropped_accepted] =
        three_tasks(&store, dropped, "ephemeral", "b");
    store
        .mission_close(done, "done", "shipped")
        .expect("close as done");
    store
        .mission_close(dropped, "dropped", "gone")
        .expect("drop");

    let mut client = admin(&schema);
    let cancelled = [
        done_assigned,
        done_reported,
        dropped_assigned,
        dropped_reported,
    ];
    let kept = [(done_accepted, "accepted"), (dropped_accepted, "accepted")];
    assert_cancelled_once(&store, &mut client, &cancelled, &kept, "after the closes");

    let types = control_types(&mut client);
    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
    assert_eq!(
        control_types(&mut client),
        types,
        "a refold appends nothing"
    );
    store.migrate().expect("migrate again");
    assert_eq!(
        control_types(&mut client),
        types,
        "migrating again appends nothing"
    );
    assert_cancelled_once(&store, &mut client, &cancelled, &kept, "after the replays");
}

#[test]
fn a_task_cancelled_before_the_records_gets_its_record_once_on_migration() {
    let (store, schema) = store("control_cancelled_backfill");
    let mission = store.mission_add("Old", &json!([]), "human").expect("add");
    let [assigned, reported, accepted] = three_tasks(&store, mission, "ephemeral", "w");
    let budget = store
        .task_assign(mission, "ephemeral", "w", "over budget", "default")
        .expect("assign");
    let mut client = admin(&schema);
    // An abort is a task's closing record of its own.
    client
        .execute(
            "select spawn_control_record(jsonb_build_object(
                 'type', 'task.aborted', 'id', $1::bigint, 'reason', 'over budget'))",
            &[&budget],
        )
        .expect("abort");
    store
        .mission_close(mission, "dropped", "gone")
        .expect("drop");
    // A close from before the fix: the fold cancelled, no record followed.
    client
        .execute(
            "delete from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.cancelled'",
            &[],
        )
        .expect("the log as an earlier writer left it");
    assert_eq!(last_stage(&store, reported), "reported");

    store.migrate().expect("migrate");
    // The accepted task of a mission without a gate goes on, even dropped.
    assert_cancelled_once(
        &store,
        &mut client,
        &[assigned, reported],
        &[(accepted, "accepted")],
        "after migrating",
    );
    assert_eq!(
        cancelled_records(&mut client, budget),
        0,
        "an aborted task gets none"
    );
    assert_eq!(last_stage(&store, budget), "aborted");

    let types = control_types(&mut client);
    store.migrate().expect("migrate again");
    assert_eq!(
        control_types(&mut client),
        types,
        "a task that has its record gets no second"
    );
    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
    assert_eq!(control_types(&mut client), types);
}

/// A report names the checkpoint its turn ended with (`commit` of the
/// `task.reported` record); one recorded the first way names none.
#[test]
fn a_report_names_the_checkpoint_of_its_turn() {
    let (store, _schema) = store("control_report_commit");
    let mission = store
        .mission_add("Report", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    assert_eq!(
        store.reported_commit(task).expect("read"),
        None,
        "no report"
    );
    store.task_delivered(task, 1).expect("delivered");
    let commit = "c".repeat(40);
    assert!(
        store
            .task_reported_with(task, "done", Some(&commit))
            .expect("reported")
    );
    assert_eq!(store.reported_commit(task).expect("read"), Some(commit));
    store
        .task_review(task, false, "again", None)
        .expect("send back");
    store.task_delivered(task, 2).expect("delivered again");
    assert!(
        store
            .task_reported(task, "fixed")
            .expect("reported, no commit")
    );
    assert_eq!(
        store.reported_commit(task).expect("read"),
        None,
        "the latest report names no checkpoint"
    );
}

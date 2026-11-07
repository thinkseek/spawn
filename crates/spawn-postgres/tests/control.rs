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

/// Objective 018: a `no_room` the integrator records when a check before
/// the gate falls short names `check` (`disk`, `memory`, `database`),
/// `measured` and `threshold` (a disk one still `free_kb` and `tmpdir`).
/// The store accepts it, refuses one that leaves its measure out, and folds
/// it as every `no_room`: the task stays where it was, neither count
/// moves, and the mission's run of them counts on; the escalation names the
/// check.
#[test]
#[allow(clippy::too_many_lines)]
fn a_no_room_with_its_check_measure_and_threshold_folds_as_before() {
    let (store, schema) = store("control_no_room_checks");
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
    let mut client = admin(&schema);
    let no_room = |fields: serde_json::Value| {
        let mut record = json!({"type": "task.gated", "id": task, "revision": 0,
                                "verdict": "no_room", "base": "b1", "scenarios": 7,
                                "reason": "no room to judge now; not a verdict, judged again later",
                                "log": "the gate did not run", "escalate": false});
        record
            .as_object_mut()
            .expect("object")
            .extend(fields.as_object().expect("object").clone());
        store.integrator_record(&record)
    };
    let mission_row = |client: &mut Client| tables(client)["missions"][0].clone();

    no_room(json!({"check": "memory", "measured": 1500, "threshold": 2048}))
        .expect("a memory no_room names no free_kb or tmpdir");
    let after = tables(&mut client);
    assert_eq!(after["tasks"][0]["state"], "reported", "the task stays");
    assert_eq!(after["tasks"][0]["gate_log"], "the gate did not run");
    let row = &after["missions"][0];
    assert_eq!(
        (&row["gate_aborts"], &row["gate_rejections"]),
        (&json!(0), &json!(0))
    );
    assert_eq!(
        (&row["gate_room_task"], &row["gate_room_waits"]),
        (&json!(task), &json!(1))
    );
    assert!(row["gate_room_since_ms"].is_number() && row["gate_room_last_ms"].is_number());
    assert_eq!(row["gate_room_escalated"], false);

    no_room(json!({"check": "database",
                   "measured": "ERROR: could not resize shared memory segment (53100)",
                   "threshold": "a parallel query within 10 s",
                   "server": "/run/test-pg:5432/spawn_test", "escalate": true}))
    .expect("a database no_room");
    no_room(
        json!({"check": "disk", "measured": 1024, "threshold": 10_485_760,
                   "free_kb": 1024, "min_gb": 10, "tmpdir": "/tmp"}),
    )
    .expect("a disk no_room");
    no_room(json!({"free_kb": 0, "min_gb": 10, "tmpdir": "/data/tmp"}))
        .expect("the gate's own fact, as before, names no check");
    let row = mission_row(&mut client);
    assert_eq!(
        (
            &row["gate_room_task"],
            &row["gate_room_waits"],
            &row["gate_room_escalated"]
        ),
        (&json!(task), &json!(4), &json!(true)),
        "one run, counted on"
    );
    assert_eq!(
        (&row["gate_aborts"], &row["gate_rejections"]),
        (&json!(0), &json!(0))
    );

    // A no_room that leaves out what it measured is refused, and moves nothing.
    for refused in [
        json!({"check": "disk", "measured": 1024, "threshold": 10_485_760}),
        json!({"check": "memory", "threshold": 2048}),
        json!({"check": "memory", "measured": 1500}),
        json!({"check": "database", "measured": "", "threshold": "connect within 10 s"}),
        json!({"check": "cpu", "measured": 1, "threshold": 2, "free_kb": 1, "tmpdir": "/t"}),
        json!({"check": null, "measured": 1, "threshold": 2, "free_kb": 1, "tmpdir": "/t"}),
        json!({}),
    ] {
        assert!(no_room(refused.clone()).is_err(), "{refused}");
    }
    assert_eq!(mission_row(&mut client)["gate_room_waits"], 4);

    let escalations = store.gate_room_escalations().expect("escalations");
    assert_eq!(
        escalations.as_array().map(Vec::len),
        Some(1),
        "{escalations}"
    );
    assert_eq!(escalations[0]["check"], "database");
    assert_eq!(escalations[0]["threshold"], "a parallel query within 10 s");
    assert_eq!(escalations[0]["mission"], mission);

    // A gate that judges ends the run, as before.
    store
        .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                   "verdict": "aborted", "reason": "the gate failed"}))
        .expect("an abort");
    let row = mission_row(&mut client);
    assert_eq!(
        (&row["gate_room_waits"], &row["gate_aborts"]),
        (&json!(0), &json!(1))
    );
    assert!(row["gate_room_task"].is_null());

    let live = tables(&mut client);
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

/// One event of `session` at `at`, written straight into the log.
fn session_event(
    client: &mut Client,
    session: &str,
    at: i64,
    name: &str,
    detail: &serde_json::Value,
) -> i64 {
    client
        .query_one(
            "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
             values ('agent_budget', $1, 'test',
                     jsonb_build_object('name', $2::text, 'detail', $3::jsonb),
                     (select coalesce(max(session_seq), 0) + 1 from spawn_events
                       where session_id = $1),
                     $4)
             returning id",
            &[&session, &name, detail, &at],
        )
        .expect("write an event")
        .get(0)
}

/// What a task's active time reads: its session's events after delivery
/// with their tool or job ids, and the fleet's silences between two events,
/// each capped.
#[test]
fn a_tasks_activity_and_the_fleets_silences_are_read_from_the_log() {
    let (store, schema) = store("control_budget_activity");
    let mut client = admin(&schema);
    let base = 1_000_000_000_000;
    let min = 60_000;
    session_event(
        &mut client,
        "s_task",
        base - min,
        "tool.started",
        &json!({"tool_use_id": "early"}),
    );
    let start = session_event(
        &mut client,
        "s_task",
        base + min,
        "tool.started",
        &json!({"tool_name": "Bash", "tool_use_id": "toolu_1"}),
    );
    // Other sessions keep the fleet alive for 40 minutes, then it stops
    // for five hours.
    for n in 1..=20 {
        session_event(
            &mut client,
            "s_other",
            base + min + n * 2 * min,
            "screen.observed",
            &json!({}),
        );
    }
    session_event(
        &mut client,
        "s_task",
        base + 45 * min,
        "job.started",
        &json!({"job_id": "j1"}),
    );
    let end = session_event(
        &mut client,
        "s_task",
        base + 345 * min,
        "tool.completed",
        &json!({"tool_use_id": "toolu_1", "duration_ms": null}),
    );

    let events = store.session_activity("s_task", base).expect("activity");
    let read: Vec<_> = events
        .iter()
        .map(|event| {
            (
                event.at_ms - base,
                event.name.as_str(),
                event.key.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        read,
        [
            (min, "tool.started", Some("toolu_1")),
            (45 * min, "job.started", Some("j1")),
            (345 * min, "tool.completed", Some("toolu_1")),
        ]
    );
    assert_eq!(events[0].id, start);
    // From the tool's start to its end: 40 minutes of 2-minute gaps, then
    // the 4-minute gap to the job, then five hours capped at 5 minutes.
    let alive = store.fleet_alive_ms(start, end, 5 * min).expect("alive");
    assert_eq!(alive, 40 * min + 4 * min + 5 * min);
    assert_eq!(
        store.fleet_alive_ms(end, end, 5 * min).expect("one event"),
        0
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

/// Objective 20: `host_fault` (the gate failed because the host did) is no
/// verdict on the work. The store accepts it only with its evidence, stamps
/// its `due_ms` from the record's own time, leaves the task where it was
/// (revisions, note) and both halt counts as they were, and folds the
/// mission's host faults in a row: + 1 on each, kept by `no_room` and
/// `stale`, ended by a real verdict, a landing and a resume. The second in
/// a row is an escalation until a digest tells it.
#[test]
#[allow(clippy::too_many_lines)]
fn a_host_fault_moves_no_count_and_its_run_is_folded() {
    let (store, schema) = store("control_host_fault");
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
    let accepted_task = |brief: &str| {
        let task = store
            .task_assign(mission, "ephemeral", "w", brief, "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        store.task_review(task, true, "", None).expect("accept");
        task
    };
    let task = accepted_task("first");
    let mut client = admin(&schema);
    let host_fault = |task: i64, revision: i32, faults: i64, backoff_ms: i64| {
        json!({"type": "task.gated", "id": task, "revision": revision, "verdict": "host_fault",
               "base": "b1", "scenarios": 7, "candidate": "c-host",
               "reason": "host fault: every failure check-tests.sh printed (1) is a connection \
                          error to the test database",
               "log": "| FAILED api::tests::one: Connection refused", "evidence": "failures",
               "failures": [{"test": "api::tests::one", "message": "Connection refused",
                             "class": "connection"}],
               "host_faults": faults, "backoff_ms": backoff_ms, "escalate": faults == 2})
    };
    let verdict = |task: i64, revision: i32, verdict: &str| {
        store
            .integrator_record(
                &json!({"type": "task.gated", "id": task, "revision": revision,
                                       "verdict": verdict, "base": "b1", "note": "real",
                                       "candidate": "c1", "reason": "r", "free_kb": 1,
                                       "min_gb": 10, "tmpdir": "/t"}),
            )
            .expect("record a verdict");
    };
    let mission_row = |client: &mut Client| tables(client)["missions"][0].clone();
    let task_row = |client: &mut Client, id: i64| {
        tables(client)["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|row| row["id"] == id)
            .cloned()
            .expect("the task")
    };
    let recorded = |client: &mut Client, event: i64| -> (serde_json::Value, i64) {
        let row = client
            .query_one(
                "select kind -> 'detail' -> 'record', recorded_at_ms from spawn_events
                  where id = $1",
                &[&event],
            )
            .expect("the record");
        (row.get(0), row.get(1))
    };

    // Only with its evidence: the failures, the crash lines when the log
    // decided it, and its backoff.
    let refused = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut record = host_fault(task, 0, 1, 300_000);
        change(&mut record);
        store.integrator_record(&record).is_err()
    };
    assert!(refused(&|record| {
        record.as_object_mut().expect("object").remove("evidence");
    }));
    assert!(refused(&|record| record["evidence"] = json!("a hunch")));
    assert!(refused(&|record| record["failures"] = json!([])));
    assert!(refused(
        &|record| record["failures"] = json!("Connection refused")
    ));
    assert!(refused(&|record| record["backoff_ms"] = json!("5 min")));
    assert!(refused(&|record| record["evidence"] = json!("server_log")));
    assert!(refused(&|record| {
        record["evidence"] = json!("server_log");
        record["crash"] = json!([]);
    }));

    // A rejection and an abort first: the runs a host fault must leave.
    verdict(task, 0, "rejected");
    store.task_delivered(task, 2).expect("delivered again");
    assert!(store.task_reported(task, "again").expect("reported"));
    store
        .task_review(task, true, "", Some(1))
        .expect("accept again");
    verdict(task, 1, "aborted");
    let before = task_row(&mut client, task);
    assert_eq!(
        (before["state"].clone(), before["revisions"].clone()),
        (json!("accepted"), json!(1))
    );

    let event = store
        .integrator_record(&host_fault(task, 1, 1, 300_000))
        .expect("the store accepts host_fault");
    let (record, at) = recorded(&mut client, event);
    assert_eq!(
        record["due_ms"],
        at + 300_000,
        "its own time plus the backoff"
    );
    let after = task_row(&mut client, task);
    assert_eq!(after["state"], "accepted", "the task stays where it was");
    assert_eq!(after["revisions"], 1);
    assert_eq!(after["review_note"], before["review_note"]);
    assert_eq!(after["review_note"], "real");
    assert_eq!(after["candidate"], before["candidate"]);
    let row = mission_row(&mut client);
    assert_eq!(
        (row["gate_rejections"].clone(), row["gate_aborts"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(row["gate_host_faults"], 1);
    assert_eq!(row["gate_host_task"], task);
    assert_eq!(row["gate_host_due_ms"], at + 300_000);
    assert!(row["halted"].is_null());
    assert_eq!(
        store
            .gate_host_fault_escalations()
            .expect("escalations")
            .as_array()
            .map(Vec::len),
        Some(0),
        "the first in a row is not escalated"
    );

    // `no_room` and `stale` keep the run; the next host fault grows it.
    verdict(task, 1, "no_room");
    verdict(task, 1, "stale");
    let row = mission_row(&mut client);
    assert_eq!(row["gate_host_faults"], 1);
    assert_eq!(row["gate_host_due_ms"], at + 300_000);
    assert_eq!(task_row(&mut client, task)["state"], "accepted");
    let second = store
        .integrator_record(&host_fault(task, 1, 2, 600_000))
        .expect("a second host fault");
    let (record, at) = recorded(&mut client, second);
    let row = mission_row(&mut client);
    assert_eq!(row["gate_host_faults"], 2);
    assert_eq!(row["gate_host_due_ms"], at + 600_000);
    assert_eq!(record["due_ms"], at + 600_000);
    assert_eq!(
        (row["gate_rejections"].clone(), row["gate_aborts"].clone()),
        (json!(1), json!(1))
    );

    // The second in a row is an escalation, until a digest tells it.
    let escalations = store.gate_host_fault_escalations().expect("escalations");
    assert_eq!(
        escalations.as_array().map(Vec::len),
        Some(1),
        "{escalations}"
    );
    assert_eq!(escalations[0]["event_id"], second);
    assert_eq!(escalations[0]["mission"], mission);
    assert_eq!(escalations[0]["due_ms"], at + 600_000);
    assert!(escalations[0].get("log").is_none());
    client
        .execute(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id,
                                         action, state, succeeded, created_at_ms)
             values ('spawn-dispatch', 'dispatch-digest-host', 'agent_supervisor',
                     'session_supervisor',
                     jsonb_build_object('type', 'input', 'text', 'digest',
                                        'tags', jsonb_build_array($1::text)),
                     'finished', true, 0)",
            &[&format!("host gate-host-fault-{second}")],
        )
        .expect("a delivered digest");
    assert_eq!(
        store
            .gate_host_fault_escalations()
            .expect("escalations")
            .as_array()
            .map(Vec::len),
        Some(0),
        "told once"
    );

    // A real verdict ends the run.
    verdict(task, 1, "rejected");
    let row = mission_row(&mut client);
    assert_eq!(row["gate_host_faults"], 0);
    assert!(row["gate_host_task"].is_null() && row["gate_host_due_ms"].is_null());
    assert_eq!(row["gate_rejections"], 2);

    // A landing ends it: another task's host fault, then a landing.
    let landing = accepted_task("second");
    verdict(landing, 0, "accepted");
    let third = accepted_task("third");
    store
        .integrator_record(&host_fault(third, 0, 1, 300_000))
        .expect("a host fault on another task");
    assert_eq!(mission_row(&mut client)["gate_host_faults"], 1);
    assert_eq!(mission_row(&mut client)["gate_host_task"], third);
    store
        .integrator_record(&json!({"type": "task.landed", "id": landing, "from": "b1",
                                   "to": "c1"}))
        .expect("landed");
    let row = mission_row(&mut client);
    assert_eq!(row["gate_host_faults"], 0, "a landing ends the run");
    assert!(row["gate_host_task"].is_null());

    // A resume ends it.
    store
        .integrator_record(&host_fault(third, 0, 1, 300_000))
        .expect("a host fault");
    store
        .integrator_record(&json!({"type": "mission.halted", "id": mission, "reason": "x"}))
        .expect("halt");
    assert_eq!(mission_row(&mut client)["gate_host_faults"], 1);
    store.mission_resume(mission).expect("resume");
    let row = mission_row(&mut client);
    assert_eq!(row["gate_host_faults"], 0, "a resume ends the run");
    assert!(row["gate_host_due_ms"].is_null());
    assert_eq!(task_row(&mut client, third)["state"], "accepted");

    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
}

/// Objective 33: a `host_fault` whose evidence is a base that disagreed with
/// itself. The store accepts it only with its facts (the scenario, both
/// exits, its counts, `alert` and `escalate` false), stamps its `due_ms` from
/// the record's own time, leaves the task where it was and both halt counts
/// as they were, and folds two runs: 020's `gate_host_faults` (which both
/// evidences share) and this objective's `gate_base_disagrees`. The third in
/// a row is the alert, told once; every other verdict, a landing and a resume
/// end the run.
#[test]
#[allow(clippy::too_many_lines)]
fn a_base_that_disagrees_with_itself_is_a_host_fault_with_its_own_count() {
    let (store, schema) = store("control_base_disagrees");
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
    let accepted_task = |brief: &str| {
        let task = store
            .task_assign(mission, "ephemeral", "w", brief, "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        store.task_review(task, true, "", None).expect("accept");
        task
    };
    let task = accepted_task("first");
    let mut client = admin(&schema);
    let disagrees = |task: i64, revision: i32, faults: i64, count: i64, alert: bool| {
        json!({"type": "task.gated", "id": task, "revision": revision, "verdict": "host_fault",
               "base": "b1", "scenarios": 7, "candidate": "c-host",
               "reason": "host fault: the base disagrees with itself: 011.sh exited 1, then 0 on \
                          the base; not a verdict, judged again later",
               "log": "| 011.sh exited 1, then 0 on the base: a harness error, not a verdict; \
                       the gate aborts",
               "evidence": "base_disagrees", "scenario": "011.sh", "exits": [1, 0],
               "setup_s": 244, "host_faults": faults, "backoff_ms": 300_000,
               "escalate": false, "base_disagrees": count, "alert": alert})
    };
    let verdict = |task: i64, revision: i32, verdict: &str| {
        store
            .integrator_record(
                &json!({"type": "task.gated", "id": task, "revision": revision,
                                       "verdict": verdict, "base": "b1", "note": "real",
                                       "candidate": "c1", "reason": "r", "free_kb": 1,
                                       "min_gb": 10, "tmpdir": "/t"}),
            )
            .expect("record a verdict");
    };
    let mission_row = |client: &mut Client| tables(client)["missions"][0].clone();
    let task_row = |client: &mut Client, id: i64| {
        tables(client)["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|row| row["id"] == id)
            .cloned()
            .expect("the task")
    };
    let recorded = |client: &mut Client, event: i64| -> (serde_json::Value, i64) {
        let row = client
            .query_one(
                "select kind -> 'detail' -> 'record', recorded_at_ms from spawn_events
                  where id = $1",
                &[&event],
            )
            .expect("the record");
        (row.get(0), row.get(1))
    };

    // Only with its facts.
    let refused = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut record = disagrees(task, 0, 1, 1, false);
        change(&mut record);
        store.integrator_record(&record).is_err()
    };
    assert!(refused(&|record| {
        record.as_object_mut().expect("object").remove("scenario");
    }));
    assert!(refused(&|record| record["scenario"] = json!("011.sh | 102.sh")));
    assert!(refused(&|record| record["scenario"] = json!("011 bash.sh")));
    assert!(refused(&|record| record["exits"] = json!([2, 0])));
    assert!(refused(&|record| record["exits"] = json!([1])));
    assert!(refused(&|record| record["exits"] = json!(1)));
    assert!(refused(&|record| record["base_disagrees"] = json!("1")));
    assert!(refused(&|record| {
        record.as_object_mut().expect("object").remove("alert");
    }));
    assert!(refused(&|record| record["alert"] = json!("yes")));
    assert!(refused(&|record| record["setup_s"] = json!("244 s")));
    assert!(refused(&|record| record["escalate"] = json!(true)));
    assert!(refused(&|record| {
        record.as_object_mut().expect("object").remove("backoff_ms");
    }));
    // The task's shape before any host fault.
    let before = task_row(&mut client, task);
    assert_eq!(
        (before["state"].clone(), before["revisions"].clone()),
        (json!("accepted"), json!(0))
    );

    // The first: its own time plus its backoff, the task where it was, both
    // counts at 1 and no escalation. A gate that printed no timing line
    // leaves `setup_s` out, and the store takes it.
    let event = store
        .integrator_record(&{
            let mut record = disagrees(task, 0, 1, 1, false);
            record.as_object_mut().expect("object").remove("setup_s");
            record
        })
        .expect("the store accepts a base that disagrees");
    let (record, at) = recorded(&mut client, event);
    assert_eq!(record["due_ms"], at + 300_000);
    let after = task_row(&mut client, task);
    assert_eq!(after["state"], "accepted", "the task stays where it was");
    assert_eq!(after["revisions"], 0);
    assert_eq!(after["review_note"], before["review_note"]);
    assert_eq!(after["candidate"], before["candidate"]);
    let row = mission_row(&mut client);
    assert_eq!(row["gate_host_faults"], 1);
    assert_eq!(row["gate_base_disagrees"], 1);
    assert_eq!(row["gate_host_due_ms"], at + 300_000);
    assert_eq!(
        (row["gate_rejections"].clone(), row["gate_aborts"].clone()),
        (json!(0), json!(0))
    );
    assert!(row["halted"].is_null());
    assert_eq!(
        store
            .gate_base_disagrees_alerts()
            .expect("alerts")
            .as_array()
            .map(Vec::len),
        Some(0),
        "the first in a row is no alert"
    );
    assert_eq!(
        store
            .gate_host_fault_escalations()
            .expect("escalations")
            .as_array()
            .map(Vec::len),
        Some(0),
        "a base disagreement never escalates"
    );

    // `no_room` and `stale` keep both runs; 020's evidence ends this one and
    // grows the shared one.
    verdict(task, 0, "no_room");
    verdict(task, 0, "stale");
    let row = mission_row(&mut client);
    assert_eq!(
        (row["gate_host_faults"].clone(), row["gate_base_disagrees"].clone()),
        (json!(1), json!(1))
    );
    store
        .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                   "verdict": "host_fault", "base": "b1",
                                   "reason": "host fault: a connection error",
                                   "evidence": "failures",
                                   "failures": [{"test": "api::tests::one",
                                                 "message": "Connection refused",
                                                 "class": "connection"}],
                                   "host_faults": 2, "backoff_ms": 600_000,
                                   "escalate": true}))
        .expect("020's host fault");
    let row = mission_row(&mut client);
    assert_eq!(row["gate_host_faults"], 2, "both evidences share the run");
    assert_eq!(
        row["gate_base_disagrees"], 0,
        "020's evidence ends the base disagreements"
    );

    // Three in a row: the third is the alert, folded as 3.
    store
        .integrator_record(&disagrees(task, 0, 3, 1, false))
        .expect("a base disagrees");
    store
        .integrator_record(&disagrees(task, 0, 4, 2, false))
        .expect("a second base disagrees");
    let third = store
        .integrator_record(&disagrees(task, 0, 5, 3, true))
        .expect("a third base disagrees");
    let row = mission_row(&mut client);
    assert_eq!(row["gate_base_disagrees"], 3);
    assert_eq!(row["gate_host_faults"], 5);
    assert!(row["halted"].is_null());
    let alerts = store.gate_base_disagrees_alerts().expect("alerts");
    assert_eq!(alerts.as_array().map(Vec::len), Some(1), "{alerts}");
    assert_eq!(alerts[0]["event_id"], third);
    assert_eq!(alerts[0]["mission"], mission);
    assert_eq!(alerts[0]["scenario"], "011.sh");
    assert_eq!(alerts[0]["base_disagrees"], 3);
    assert_eq!(alerts[0]["setup_s"], 244);
    assert!(alerts[0].get("log").is_none(), "the gate's log stays out");
    // Told once: a digest carrying its tag ends it.
    client
        .execute(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id,
                                         action, state, succeeded, created_at_ms)
             values ('spawn-dispatch', 'dispatch-digest-base', 'agent_supervisor',
                     'session_supervisor',
                     jsonb_build_object('type', 'input', 'text', 'digest',
                                        'tags', jsonb_build_array($1::text)),
                     'finished', true, 0)",
            &[&format!("host gate-base-disagrees-{third}")],
        )
        .expect("a delivered digest");
    assert_eq!(
        store
            .gate_base_disagrees_alerts()
            .expect("alerts")
            .as_array()
            .map(Vec::len),
        Some(0),
        "told once"
    );

    // A real abort counts and resets both runs; so does a real verdict.
    verdict(task, 0, "aborted");
    let row = mission_row(&mut client);
    assert_eq!(
        (
            row["gate_aborts"].clone(),
            row["gate_host_faults"].clone(),
            row["gate_base_disagrees"].clone()
        ),
        (json!(1), json!(0), json!(0))
    );

    // A landing ends both runs.
    let landing = accepted_task("second");
    store
        .integrator_record(&disagrees(task, 0, 2, 2, false))
        .expect("a base disagrees");
    assert_eq!(
        mission_row(&mut client)["gate_base_disagrees"],
        1,
        "a new run after the abort"
    );
    verdict(landing, 0, "accepted");
    store
        .integrator_record(&json!({"type": "task.landed", "id": landing, "from": "b1",
                                   "to": "c1"}))
        .expect("landed");
    let row = mission_row(&mut client);
    assert_eq!(
        (row["gate_host_faults"].clone(), row["gate_base_disagrees"].clone()),
        (json!(0), json!(0)),
        "a landing ends both runs"
    );

    // A resume ends both runs.
    store
        .integrator_record(&disagrees(task, 0, 1, 1, false))
        .expect("a base disagrees");
    store
        .integrator_record(&json!({"type": "mission.halted", "id": mission, "reason": "x"}))
        .expect("halt");
    assert_eq!(mission_row(&mut client)["gate_base_disagrees"], 1);
    store.mission_resume(mission).expect("resume");
    let row = mission_row(&mut client);
    assert_eq!(
        (row["gate_host_faults"].clone(), row["gate_base_disagrees"].clone()),
        (json!(0), json!(0)),
        "a resume ends both runs"
    );

    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
}

/// A report still a draft is not taken (crates/spawn-cli/src/deliver.rs):
/// `task.report_unfinished` records the check, once per content and
/// revision, only while the task is delivered, and folds into nothing.
#[test]
fn an_unfinished_report_is_recorded_once_per_content_and_revision_and_moves_nothing() {
    let (store, schema) = store("control_unfinished");
    let mission = store
        .mission_add("Payouts", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "draft", "brief", "default")
        .expect("assign");
    let markers = json!([{"line": 9, "kind": "placeholder", "marker": "PENDING_RESULTS",
                          "text": "**Results.** PENDING_RESULTS"}]);
    assert!(
        !store
            .task_report_unfinished(task, "d1", &markers, "report-unfinished-x", 7)
            .expect("an undelivered task"),
        "only a delivered task's report is read"
    );
    store.task_delivered(task, 41).expect("delivered");
    let mut client = admin(&schema);
    let before = tables(&mut client);
    assert!(!store.report_unfinished_recorded(task, "d1").expect("read"));
    assert!(
        store
            .task_report_unfinished(task, "d1", &markers, "report-unfinished-d1", 7)
            .expect("recorded")
    );
    assert!(store.report_unfinished_recorded(task, "d1").expect("read"));
    assert!(
        !store
            .task_report_unfinished(task, "d1", &markers, "report-unfinished-d1", 8)
            .expect("the same content"),
        "once per content"
    );
    assert!(!store.report_unfinished_recorded(task, "d2").expect("read"));
    assert!(
        store
            .task_report_unfinished(task, "d2", &markers, "report-unfinished-d2", 9)
            .expect("another content")
    );
    assert_eq!(tables(&mut client), before, "the task stays delivered");
    assert_eq!(task_state(&mut client, task), "delivered");

    // Sent back and delivered again: the same content is read anew.
    assert!(store.task_reported(task, "done").expect("reported"));
    assert!(
        !store
            .task_report_unfinished(task, "d3", &markers, "report-unfinished-d3", 10)
            .expect("a reported task"),
        "a report taken ends the reading"
    );
    store
        .task_review(task, false, "again", None)
        .expect("send back");
    store.task_delivered(task, 42).expect("delivered again");
    assert!(!store.report_unfinished_recorded(task, "d1").expect("read"));
    assert!(
        store
            .task_report_unfinished(task, "d1", &markers, "report-unfinished-d1-r1", 11)
            .expect("revision 1")
    );

    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
    let record: serde_json::Value = client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.report_unfinished'
              order by session_seq limit 1",
            &[],
        )
        .expect("the first record")
        .get(0);
    assert_eq!(
        record,
        json!({"type": "task.report_unfinished", "id": task, "revision": 0, "digest": "d1",
               "markers": markers, "note": "report-unfinished-d1", "command_id": 7})
    );
    let stages: Vec<String> = store
        .task_stages(task)
        .expect("stages")
        .as_array()
        .expect("a list")
        .iter()
        .map(|stage| stage["stage"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        stages,
        [
            "assigned",
            "delivered",
            "report_unfinished",
            "report_unfinished",
            "reported",
            "reviewed",
            "delivered",
            "report_unfinished"
        ]
    );
}

/// The settings `mission_missions` holds for a mission, and those its latest
/// `mission.configured` record carries.
fn settings(client: &mut Client, mission: i64) -> (serde_json::Value, serde_json::Value) {
    let row = client
        .query_one(
            "select jsonb_build_object('gate', gate, 'accept', accept, 'budget', budget,
                                       'objectives', objectives)
               from mission_missions where id = $1",
            &[&mission],
        )
        .expect("the mission")
        .get(0);
    let record: serde_json::Value = client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control' and source = 'control'
                and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = 'mission.configured'
                and (kind -> 'detail' -> 'record' ->> 'id')::bigint = $1
              order by session_seq desc limit 1",
            &[&mission],
        )
        .expect("its latest mission.configured")
        .get(0);
    let record = json!({"gate": record["gate"], "accept": record["accept"],
                        "budget": record["budget"], "objectives": record["objectives"]});
    (row, record)
}

/// `spawn mission configure --id 4 --tool-calls 600` (2026-11-08) replaced
/// every setting of mission 4: a change keeps what it does not name, and
/// only an explicit null clears.
#[test]
#[allow(clippy::too_many_lines)]
fn a_partial_configure_keeps_the_other_settings_and_only_a_null_clears() {
    let (store, schema) = store("control_reconfigure");
    let mut client = admin(&schema);
    let mission = store
        .mission_add("Supply", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let objectives = json!({"backlog": "/controller/backlog", "resident": "supply", "workers": 2});
    store
        .mission_configure(
            mission,
            Some("/controller/scenarios"),
            "gate",
            Some(&json!({"minutes": 90})),
            Some(&objectives),
        )
        .expect("configure");
    let full = json!({"gate": "/controller/scenarios", "accept": "gate",
                      "budget": {"minutes": 90}, "objectives": objectives});
    assert_eq!(settings(&mut client, mission), (full.clone(), full.clone()));

    let after = store
        .mission_reconfigure(mission, &json!({"budget": {"tool_calls": 600}}))
        .expect("the tool-call limit alone");
    let mut expected = full.clone();
    expected["budget"] = json!({"minutes": 90, "tool_calls": 600});
    assert_eq!(
        after,
        json!({"mission": mission, "before": full, "after": expected}),
        "it says what the mission was and is"
    );
    assert_eq!(
        settings(&mut client, mission),
        (expected.clone(), expected.clone()),
        "the record carries every setting, the fold keeps them"
    );

    store
        .mission_reconfigure(
            mission,
            &json!({"objectives": {"workers": 4}, "accept": "review"}),
        )
        .expect("workers and accept");
    expected["objectives"]["workers"] = json!(4);
    expected["accept"] = json!("review");
    assert_eq!(settings(&mut client, mission).0, expected);

    store
        .mission_reconfigure(
            mission,
            &json!({"budget": {"minutes": null}, "objectives": {"workers": null}}),
        )
        .expect("clear the minutes and the workers");
    expected["budget"] = json!({"tool_calls": 600});
    expected["objectives"] = json!({"backlog": "/controller/backlog", "resident": "supply"});
    assert_eq!(
        settings(&mut client, mission),
        (expected.clone(), expected.clone())
    );

    // A change that sets nothing, or sets what the mission already has,
    // changes and records nothing.
    let before = control_types(&mut client).len();
    let same = json!({"mission": mission, "before": expected, "after": expected});
    assert_eq!(
        store
            .mission_reconfigure(mission, &json!({}))
            .expect("nothing"),
        same
    );
    assert_eq!(
        store
            .mission_reconfigure(mission, &json!({"budget": {"tool_calls": 600}}))
            .expect("the same value"),
        same
    );
    assert_eq!(control_types(&mut client).len(), before, "nothing recorded");

    // The result is checked as a whole, and a refusal records nothing.
    assert!(
        refusal(store.mission_reconfigure(mission, &json!({"gate": null})))
            .contains("objectives need a gate"),
        "objectives keep needing a gate"
    );
    assert!(
        refusal(store.mission_reconfigure(mission, &json!({"budgets": {}})))
            .contains("no setting budgets")
    );
    assert!(
        refusal(store.mission_reconfigure(mission, &json!({"budget": 5})))
            .contains("is a JSON object")
    );
    assert!(
        refusal(store.mission_reconfigure(mission, &json!({"accept": "vote"})))
            .contains("review or gate")
    );
    assert_eq!(control_types(&mut client).len(), before, "nothing recorded");
    assert_eq!(settings(&mut client, mission).0, expected);

    store
        .mission_reconfigure(
            mission,
            &json!({"gate": null, "objectives": null, "budget": {"tool_calls": null}}),
        )
        .expect("clear the gate, the objectives and the last budget key");
    let cleared = json!({"gate": null, "accept": "review", "budget": null, "objectives": null});
    assert_eq!(settings(&mut client, mission), (cleared.clone(), cleared));

    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );
}

/// GitHub issue #9 (objective 027): clearing a gated mission's gate, or moving
/// its accept away from gate, while one of its tasks is landing would leave
/// that task where nothing lands it; it is refused, naming the task, and
/// goes through once nothing is landing.
#[test]
fn a_gate_is_not_cleared_while_a_task_of_its_mission_is_landing() {
    let (store, schema) = store("control_configure_landing");
    let mut client = admin(&schema);
    let mission = store
        .mission_add("Landing", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("configure");
    let task = task_in(&store, mission, "ephemeral", "w", "landing");
    assert_eq!(task_state(&mut client, task), "landing");

    let before = control_types(&mut client).len();
    for change in [
        json!({"gate": null}),
        json!({"accept": "review"}),
        json!({"gate": null, "accept": "review"}),
    ] {
        let refused = refusal(store.mission_reconfigure(mission, &change));
        assert!(
            refused.contains(&format!("task {task} is landing")),
            "{change}: {refused}"
        );
    }
    assert!(
        refusal(store.mission_configure(mission, None, "gate", None, None)).contains("is landing"),
        "the full configure refuses it too"
    );
    assert_eq!(control_types(&mut client).len(), before, "nothing recorded");
    // What keeps the task landable still goes through.
    store
        .mission_reconfigure(mission, &json!({"budget": {"minutes": 30}}))
        .expect("a budget while landing");

    store
        .integrator_record(&json!({"type": "task.landed", "id": task, "from": "b",
                                   "to": format!("c{task}")}))
        .expect("landed");
    let after = store
        .mission_reconfigure(mission, &json!({"gate": null, "accept": "review"}))
        .expect("nothing is landing");
    assert_eq!(after["after"]["gate"], json!(null));
    assert_eq!(after["after"]["accept"], "review");
    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(tables(&mut client), live);
}

/// A gated mission on `source` whose first verdict left `baseline`.
fn mission_at(store: &PostgresStore, source: &str, baseline: &str) -> i64 {
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
            &json!({"type": "task.gated", "revision": 0, "id": task, "verdict": "aborted",
                    "base": baseline, "reason": "a gate that died"}),
        )
        .expect("a first verdict sets the baseline");
    mission
}

/// Objective 21: the owner's landing is a record, `source.push_requested`
/// (`spawn mission push`), and its outcome another that names it, recorded
/// by the integrator: `source.moved` or `source.push_refused`, one per
/// request. The request and a refusal fold nothing; the move moves the
/// baselines of the missions on its source that were at its `from` and are
/// not halted (the store names them), as `spawn mission moved` does.
#[test]
#[allow(clippy::too_many_lines)]
fn an_owner_landing_is_a_request_and_one_outcome_and_only_the_move_folds() {
    let (store, schema) = store("control_owner_push");
    let mut client = admin(&schema);
    let (source, other) = ("/tmp/owner.git", "/tmp/owner-other.git");
    let (c1, c2, c3) = ("1".repeat(40), "2".repeat(40), "3".repeat(64));
    let owner_ref = |commit: &str| format!("refs/spawn-owner/{commit}");
    let record_of = |client: &mut Client, event: i64| -> serde_json::Value {
        client
            .query_one(
                "select kind -> 'detail' -> 'record' from spawn_events
                  where id = $1 and session_id = 'session_control'",
                &[&event],
            )
            .expect("the request's event")
            .get(0)
    };

    assert!(
        refusal(store.source_push_request(source, &c1, &owner_ref(&c1))).contains("no mission"),
        "a source with no mission takes no request"
    );
    let on_source = mission_at(&store, source, "b0");
    let halted = mission_at(&store, source, "b0");
    let elsewhere = mission_at(&store, other, "b0");
    // No verdict yet, so no baseline: the owner's move gives it its first.
    let fresh = store
        .mission_add("New on the source", &json!([source]), "human")
        .expect("add");
    store
        .integrator_record(&json!({"type": "mission.halted", "id": halted, "reason": "stop"}))
        .expect("halt");
    for (bad_source, commit, reference, why) in [
        (
            "https://example.invalid/x.git",
            c1.as_str(),
            owner_ref(&c1),
            "remote",
        ),
        (
            "relative/x.git",
            c1.as_str(),
            owner_ref(&c1),
            "not a mission source",
        ),
        (source, "HEAD", owner_ref("HEAD"), "full hash"),
        (source, "--all", owner_ref("--all"), "full hash"),
        (
            source,
            c1.as_str(),
            "refs/heads/main".to_string(),
            "refs/spawn-owner",
        ),
    ] {
        let refused = refusal(store.source_push_request(bad_source, commit, &reference));
        assert!(refused.contains(why), "{bad_source} {commit}: {refused}");
    }
    let before = tables(&mut client);

    // The request: a record that folds nothing, pending until its outcome.
    let request = store
        .source_push_request(source, &c1, &owner_ref(&c1))
        .expect("the owner's request");
    let recorded = record_of(&mut client, request);
    assert_eq!(recorded["type"], "source.push_requested");
    assert_eq!(recorded["source"], source);
    assert_eq!(recorded["commit"], c1.as_str());
    assert_eq!(recorded["ref"], owner_ref(&c1).as_str());
    assert_eq!(recorded["by"], "human");
    assert_eq!(tables(&mut client), before, "a request folds nothing");
    let pending = store.source_pushes_pending().expect("pending");
    assert_eq!(pending.as_array().map(Vec::len), Some(1), "{pending}");
    assert_eq!(pending[0]["request"], json!(request));
    assert_eq!(pending[0]["source"], source);
    assert_eq!(pending[0]["commit"], c1.as_str());
    assert_eq!(store.source_push_outcome(request).expect("outcome"), None);

    // Refused: folds nothing, and is the request's only outcome.
    for bad in [
        json!({"type": "source.push_refused", "request": request}),
        json!({"type": "source.push_refused", "reason": "conflict"}),
        json!({"type": "source.push_refused", "request": request + 1000, "reason": "conflict"}),
    ] {
        assert!(store.integrator_record(&bad).is_err(), "refused: {bad}");
    }
    let refused = store
        .integrator_record(
            &json!({"type": "source.push_refused", "request": request, "reason": "conflict"}),
        )
        .expect("refused");
    assert_eq!(tables(&mut client), before, "a refusal folds nothing");
    assert_eq!(store.source_pushes_pending().expect("pending"), json!([]));
    let outcome = store
        .source_push_outcome(request)
        .expect("outcome")
        .expect("recorded");
    assert_eq!(outcome["type"], "source.push_refused");
    assert_eq!(outcome["event_id"], json!(refused));
    assert_eq!(outcome["source"], source);
    assert!(
        refusal(
            store.integrator_record(&json!({"type": "source.moved", "source": source,
                "from": "b0", "to": "n1", "request": request, "by": "human"}))
        )
        .contains("outcome already"),
        "one outcome per request"
    );

    // Applied: the move names the request, and moves the baselines at its
    // `from` on its source, not a halted one's, not another source's.
    let second = store
        .source_push_request(source, &c2, &owner_ref(&c2))
        .expect("a second request");
    let third = store
        .source_push_request(other, &c3, &owner_ref(&c3))
        .expect("a request on the other source");
    let pending = store.source_pushes_pending().expect("pending");
    let ids: Vec<i64> = pending
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|row| row["request"].as_i64())
        .collect();
    assert_eq!(ids, [second, third], "oldest first");
    for bad in [
        json!({"type": "source.moved", "source": source, "from": "b0", "to": "n1", "by": "human"}),
        json!({"type": "source.moved", "source": source, "from": "b0", "to": "n1",
               "request": third, "by": "human"}),
        json!({"type": "source.moved", "source": source, "to": "n1", "request": second}),
        json!({"type": "source.moved", "source": source, "from": "b0", "to": "n1",
               "request": "drop table", "by": "human"}),
    ] {
        assert!(store.integrator_record(&bad).is_err(), "refused: {bad}");
    }
    let moved = store
        .integrator_record(
            &json!({"type": "source.moved", "source": source, "from": "b0",
                                   "to": "n1", "request": second, "by": "human"}),
        )
        .expect("moved");
    let recorded = record_of(&mut client, moved);
    assert_eq!(recorded["missions"], json!([on_source]), "{recorded}");
    assert_eq!(recorded["without_baseline"], json!([fresh]), "{recorded}");
    assert_eq!(recorded["request"], json!(second));
    assert_eq!(recorded["by"], "human");
    let baselines: Vec<String> = client
        .query(
            "select baseline from mission_missions where id = any($1) order by id",
            &[&vec![on_source, halted, elsewhere, fresh]],
        )
        .expect("baselines")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(baselines, ["n1", "b0", "b0", "n1"]);
    let outcome = store
        .source_push_outcome(second)
        .expect("outcome")
        .expect("recorded");
    assert_eq!(outcome["type"], "source.moved");
    assert_eq!(outcome["to"], "n1");
    let pending = store.source_pushes_pending().expect("pending");
    assert_eq!(pending.as_array().map(Vec::len), Some(1), "{pending}");
    assert_eq!(pending[0]["request"], json!(third));

    let live = tables(&mut client);
    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the log folds to the same tables"
    );
    let types = control_types(&mut client);
    for kind in [
        "source.push_requested",
        "source.push_refused",
        "source.moved",
    ] {
        assert!(types.iter().any(|seen| seen == kind), "{kind}: {types:?}");
    }

    let supervisor = common::login(&schema, "spawn_supervisor");
    client
        .batch_execute(&format!("set session authorization {supervisor}"))
        .expect("become the supervisor");
    assert!(
        client
            .query_one(
                "select spawn_source_push_request($1, $2, $3)",
                &[&source, &c1, &owner_ref(&c1)],
            )
            .is_err(),
        "the supervisor asks for no owner landing"
    );
    assert!(
        client
            .query_one("select spawn_source_push_hold()", &[])
            .is_err(),
        "the supervisor holds no landing"
    );
}

/// The owner's landing holds the landing hold's lock: no one else takes it
/// meanwhile (a task landing, a mission's drop), and it is released with
/// the hold.
#[test]
fn an_owner_landing_takes_the_landing_holds_lock() {
    let (store, schema) = store("control_owner_hold");
    let hold = store.source_push_hold().expect("hold");
    let mut client = admin(&schema);
    let taken: bool = client
        .query_one("select pg_try_advisory_lock(1399957337)", &[])
        .expect("try the lock")
        .get(0);
    assert!(!taken, "held by the owner's landing");
    drop(hold);
    // Other cases may hold it for a moment: wait, boundedly.
    client
        .batch_execute(
            "set lock_timeout = '30s'; select pg_advisory_lock(1399957337);
             select pg_advisory_unlock(1399957337);",
        )
        .expect("released with the hold");
}

/// An overrun is its own record, not a gate abort (objective 026,
/// docs/fleet-architecture.md §10.4): the fold takes the task out of flight
/// and moves no counter, and the integrator may record it only for a
/// delivered task.
#[test]
fn an_overrun_takes_its_task_out_of_flight_and_moves_no_counter() {
    let (store, schema) = store("control_over_budget");
    let mission = store
        .mission_add("Over budget", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(
            mission,
            Some("/controller/rsi/scenarios"),
            "gate",
            Some(&json!({"tool_calls": 5})),
            None,
        )
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    let counters = |client: &mut Client| -> (i32, i32) {
        let row = client
            .query_one(
                "select gate_aborts, gate_rejections from mission_missions where id = $1",
                &[&mission],
            )
            .expect("the mission");
        (row.get(0), row.get(1))
    };
    let mut client = admin(&schema);
    store
        .integrator_record(&json!({"type": "task.over_budget", "id": task,
                                   "reason": "the task made more than 5 tool calls",
                                   "tool_calls": 6, "elapsed_ms": 1_000,
                                   "budget": {"tool_calls": 5}}))
        .expect("the overrun");
    let state: String = client
        .query_one("select state from mission_tasks where id = $1", &[&task])
        .expect("the task")
        .get(0);
    assert_eq!(state, "cancelled", "the task leaves flight");
    assert_eq!(counters(&mut client), (0, 0), "no halt counts it");

    // The record names its reason, its tool calls and its budget.
    let task2 = store
        .task_assign(mission, "ephemeral", "v", "brief", "default")
        .expect("assign");
    store.task_delivered(task2, 1).expect("delivered");
    for record in [
        json!({"type": "task.over_budget", "id": task2, "reason": "",
               "tool_calls": 6, "budget": {"tool_calls": 5}}),
        json!({"type": "task.over_budget", "id": task2,
               "reason": "over", "budget": {"tool_calls": 5}}),
        json!({"type": "task.over_budget", "id": task2, "reason": "over",
               "tool_calls": 6}),
    ] {
        assert!(
            store.integrator_record(&record).is_err(),
            "a record that does not name what it measured is refused: {record}"
        );
    }
    // Only a delivered task: not one still assigned, not one reported.
    let assigned = store
        .task_assign(mission, "ephemeral", "u", "brief", "default")
        .expect("assign");
    assert!(
        store
            .integrator_record(&json!({"type": "task.over_budget", "id": assigned,
                                       "reason": "over", "tool_calls": 6,
                                       "budget": {"tool_calls": 5}}))
            .is_err(),
        "not a delivered task"
    );
    assert!(store.task_reported(task2, "done").expect("reported"));
    assert!(
        store
            .integrator_record(&json!({"type": "task.over_budget", "id": task2,
                                       "reason": "over", "tool_calls": 6,
                                       "budget": {"tool_calls": 5}}))
            .is_err(),
        "not a reported task"
    );
}

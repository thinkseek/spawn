//! Agent grading's store (docs/agent-grading.md): the grading catalog is the
//! fold of the control session's `grading.recorded` events (outside the
//! control fold), every change is checked before it is recorded, tasks
//! carry their difficulty, preset and quality rejections through the
//! control stream, a rework reroute is one statement with its review, and
//! calibration's window reads quality rejections alone from
//! `mission_tasks`.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_core::event::{EventKind, GlobalPosition, ModelProfile};
use spawn_core::grading::{
    CalibrationDecision, PresetState, SkillGrade, TaskDifficulty, TaskOutcomeSample,
    calibrate_preset,
};
use spawn_core::port::EventLog;
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, as_login, store};

fn catalog_rows(client: &mut Client) -> Value {
    client
        .query_one(
            "select coalesce(jsonb_agg(to_jsonb(g) order by g.preset_id), '[]')
               from grading_presets g",
            &[],
        )
        .expect("read the catalog")
        .get(0)
}

fn tasks(client: &mut Client) -> Value {
    client
        .query_one(
            "select coalesce(jsonb_agg(to_jsonb(t) order by t.id), '[]') from mission_tasks t",
            &[],
        )
        .expect("read the tasks")
        .get(0)
}

fn count(client: &mut Client, sql: &str) -> i64 {
    client.query_one(sql, &[]).expect(sql).get(0)
}

fn grading_events(client: &mut Client) -> i64 {
    count(
        client,
        "select count(*) from spawn_events where event_name = 'grading.recorded'",
    )
}

fn register(store: &PostgresStore, id: &str, model: &str, rank: u32) {
    store
        .grading_record(&json!({"type": "preset.registered", "preset_id": id,
                                "engine_label": "claude", "model_label": format!("label of {id}"),
                                "model_profile": model, "cost_rank": rank}))
        .expect("register");
}

fn onboard(store: &PostgresStore, id: &str, from: &str, grade: &str) {
    store
        .grading_record(&json!({"type": "preset.onboarded", "preset_id": id,
                                "from_state": from, "to_state": "active", "to_grade": grade,
                                "passed": 3, "total": 3, "failed_cases": [],
                                "suite": "evals/onboarding"}))
        .expect("onboard");
}

fn refusal(store: &PostgresStore, record: &Value) -> String {
    store
        .grading_record(record)
        .expect_err("refused")
        .to_string()
}

fn sql_refusal(error: &postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_string())
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_catalog_is_the_fold_of_its_records_and_refolds_the_same_every_time() {
    let (store, schema) = store("grading_fold");
    register(&store, "l1-cheap", "cheap", 0);
    register(&store, "l3-strong", "default", 5);
    onboard(&store, "l1-cheap", "unexamined", "l1");
    onboard(&store, "l3-strong", "unexamined", "l3");
    store
        .grading_record(&json!({"type": "preset.regraded", "preset_id": "l3-strong",
                                "decision": "demote", "from_grade": "l3", "to_grade": "l2",
                                "from_state": "active", "to_state": "demoted",
                                "first_pass": 1, "total": 5}))
        .expect("demote");
    store
        .grading_record(
            &json!({"type": "preset.grade_locked", "preset_id": "l1-cheap",
                                "from_grade": null, "to_grade": "l2",
                                "from_state": "active", "to_state": "active"}),
        )
        .expect("lock");
    store
        .grading_record(
            &json!({"type": "preset.state_changed", "preset_id": "l1-cheap",
                    "from_state": "active", "to_state": "disabled"}),
        )
        .expect("disable");

    let catalog = store.grading_catalog().expect("catalog");
    let strong = &catalog[1];
    assert_eq!(strong.preset_id, "l3-strong");
    assert_eq!(strong.state, PresetState::Demoted);
    assert_eq!(strong.onboarded_grade, Some(SkillGrade::L3));
    assert_eq!(strong.calibrated_grade, Some(SkillGrade::L2));
    assert_eq!(strong.routing_grade(), Some(SkillGrade::L2));
    assert_eq!(strong.model_profile, ModelProfile::Default);
    assert_eq!(strong.model_label, "label of l3-strong");
    let cheap = &catalog[0];
    assert_eq!(cheap.locked_grade, Some(SkillGrade::L2));
    assert_eq!(cheap.state, PresetState::Disabled);
    assert_eq!(cheap.routing_grade(), None);
    assert_eq!(cheap.model_profile, ModelProfile::Cheap);

    let mut client = admin(&schema);
    // Seven records, each one grading.recorded event of the control
    // session, as the nodes' records are, none a control.recorded.
    assert_eq!(grading_events(&mut client), 7);
    assert_eq!(
        count(
            &mut client,
            "select count(*) from spawn_events where session_id = 'session_control'
               and agent_id = 'agent_control' and source = 'control'
               and event_name = 'grading.recorded'"
        ),
        7
    );
    assert_eq!(
        count(
            &mut client,
            "select count(*) from spawn_events where event_name = 'control.recorded'"
        ),
        0
    );
    // Nothing on the fleet board: the control session belongs to no agent.
    assert_eq!(
        count(
            &mut client,
            "select count(*) from spawn_session_state where session_id = 'session_control'"
        ),
        0
    );
    // Every reader of the whole log decodes them, as control records that
    // belong to no agent (the Bridge's and the Observer's read).
    let read = store
        .read_global(GlobalPosition(0), 100)
        .expect("the log reads back whole");
    assert_eq!(read.len(), 7);
    assert!(read.iter().all(|record| matches!(
        record.event.kind,
        EventKind::GradingRecorded { .. }
    ) && record.event.kind.is_control()));

    // The log is the truth: a refold gives the same rows, a second refold
    // in a row the same again, and the control stream's refold leaves the
    // catalog alone.
    let before = catalog_rows(&mut client);
    store.grading_refold().expect("refold");
    assert_eq!(catalog_rows(&mut client), before);
    store.grading_refold().expect("refold again");
    assert_eq!(catalog_rows(&mut client), before);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("control refold");
    assert_eq!(catalog_rows(&mut client), before);
    client
        .batch_execute("delete from grading_presets")
        .expect("lose the table");
    store.grading_refold().expect("refold");
    assert_eq!(catalog_rows(&mut client), before);
    store.grading_refold().expect("refold once more");
    assert_eq!(catalog_rows(&mut client), before);
    assert_eq!(grading_events(&mut client), 7, "a refold records nothing");
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_refused_grading_change_records_nothing() {
    let (store, schema) = store("grading_refused");
    register(&store, "p", "default", 1);
    let mut client = admin(&schema);
    let before = grading_events(&mut client);
    for (record, says) in [
        (
            json!({"type": "preset.registered", "preset_id": "p", "engine_label": "e",
                   "model_label": "m", "model_profile": "default", "cost_rank": 1}),
            "already in the grading catalog",
        ),
        (
            json!({"type": "preset.registered", "preset_id": "Bad", "engine_label": "e",
                   "model_label": "m", "model_profile": "default", "cost_rank": 1}),
            "a preset id is",
        ),
        (
            json!({"type": "preset.registered", "preset_id": "q", "engine_label": "e",
                   "model_label": "m", "model_profile": "opus", "cost_rank": 1}),
            "default or cheap",
        ),
        (
            json!({"type": "preset.registered", "preset_id": "q", "engine_label": "e",
                   "model_label": "m", "model_profile": "default", "cost_rank": "1"}),
            "cost rank",
        ),
        (
            json!({"type": "preset.registered", "preset_id": "q", "engine_label": " ",
                   "model_label": "m", "model_profile": "default", "cost_rank": 1}),
            "not blank",
        ),
        (
            json!({"type": "preset.registered", "preset_id": "q", "engine": "e",
                   "model_label": "m", "model_profile": "default", "cost_rank": 1}),
            "and nothing else",
        ),
        (
            json!({"type": "preset.onboarded", "preset_id": "p", "from_state": "unexamined",
                   "to_state": "disabled", "to_grade": "l2", "passed": 1, "total": 1,
                   "failed_cases": [], "suite": "s"}),
            "leaves the preset active",
        ),
        (
            json!({"type": "preset.onboarded", "preset_id": "p", "from_state": "unexamined",
                   "to_state": "active", "to_grade": 2, "passed": 1, "total": 1,
                   "failed_cases": [], "suite": "s"}),
            "an onboarding grade is l1, l2, l3 or null",
        ),
        (
            json!({"type": "preset.onboarded", "preset_id": "p", "from_state": "active",
                   "to_state": "active", "to_grade": "l2", "passed": 1, "total": 1,
                   "failed_cases": [], "suite": "s"}),
            "is unexamined, not",
        ),
        (
            json!({"type": "preset.onboarded", "preset_id": "nobody",
                   "from_state": "unexamined", "to_state": "active", "to_grade": "l2",
                   "passed": 1, "total": 1, "failed_cases": [], "suite": "s"}),
            "not in the grading catalog",
        ),
        (
            json!({"type": "preset.regraded", "preset_id": "p", "decision": "demote",
                   "from_grade": "l2", "to_grade": "l1", "from_state": "unexamined",
                   "to_state": "demoted", "first_pass": 0, "total": 5}),
            "calibration grades only a routed preset",
        ),
        (
            json!({"type": "preset.grade_locked", "preset_id": "p", "from_grade": null,
                   "to_grade": "l4", "from_state": "unexamined", "to_state": "unexamined"}),
            "a locked grade is",
        ),
        (
            json!({"type": "preset.grade_locked", "preset_id": "p", "from_grade": null,
                   "to_grade": null, "from_state": "unexamined", "to_state": "unexamined"}),
            "has locked grade none already",
        ),
        (
            json!({"type": "preset.state_changed", "preset_id": "p",
                   "from_state": "unexamined", "to_state": "active"}),
            "no grade",
        ),
        (
            json!({"type": "preset.state_changed", "preset_id": "p",
                   "from_state": "unexamined", "to_state": "unexamined"}),
            "is unexamined already",
        ),
        (
            json!({"type": "preset.state_changed", "preset_id": "p", "from_state": "active",
                   "to_state": "disabled"}),
            "is unexamined, not",
        ),
        (
            json!({"type": "profile.registered", "preset_id": "p"}),
            "not a grading record",
        ),
    ] {
        let error = refusal(&store, &record);
        assert!(error.contains(says), "{record}: {error}");
    }
    assert_eq!(grading_events(&mut client), before);
    onboard(&store, "p", "unexamined", "l2");
    let before_steps = grading_events(&mut client);
    assert_eq!(before_steps, before + 1);
    for (record, says) in [
        // Not one step down.
        (
            json!({"type": "preset.regraded", "preset_id": "p", "decision": "demote",
                   "from_grade": "l2", "to_grade": "l2", "from_state": "active",
                   "to_state": "demoted", "first_pass": 0, "total": 5}),
            "not one step",
        ),
        // Above the onboarded grade.
        (
            json!({"type": "preset.regraded", "preset_id": "p", "decision": "restore",
                   "from_grade": "l2", "to_grade": "l3", "from_state": "active",
                   "to_state": "active", "first_pass": 9, "total": 10}),
            "not one step",
        ),
        // Disabled from l2: only l1 is disabled by calibration.
        (
            json!({"type": "preset.regraded", "preset_id": "p", "decision": "disable",
                   "from_grade": "l2", "to_grade": "l2", "from_state": "active",
                   "to_state": "disabled", "first_pass": 0, "total": 5}),
            "not one step",
        ),
        (
            json!({"type": "preset.regraded", "preset_id": "p", "decision": "demote",
                   "from_grade": "l3", "to_grade": "l2", "from_state": "active",
                   "to_state": "demoted", "first_pass": 0, "total": 5}),
            "is graded l2",
        ),
        // Enabling what is enabled: a no-op, refused.
        (
            json!({"type": "preset.state_changed", "preset_id": "p", "from_state": "active",
                   "to_state": "active"}),
            "is active already",
        ),
    ] {
        let error = refusal(&store, &record);
        assert!(error.contains(says), "{record}: {error}");
    }
    store
        .grading_record(&json!({"type": "preset.grade_locked", "preset_id": "p",
                                "from_grade": null, "to_grade": "l1",
                                "from_state": "active", "to_state": "active"}))
        .expect("lock");
    let error = refusal(
        &store,
        &json!({"type": "preset.regraded", "preset_id": "p", "decision": "demote",
                "from_grade": "l2", "to_grade": "l1", "from_state": "active",
                "to_state": "demoted", "first_pass": 0, "total": 5}),
    );
    assert!(error.contains("locked grade"), "{error}");
    assert_eq!(grading_events(&mut client), before_steps + 1);

    // A person's disable stands: onboarding again waits for an enable.
    store
        .grading_record(&json!({"type": "preset.state_changed", "preset_id": "p",
                                "from_state": "active", "to_state": "disabled"}))
        .expect("disable");
    let error = refusal(
        &store,
        &json!({"type": "preset.onboarded", "preset_id": "p", "from_state": "disabled",
                "to_state": "active", "to_grade": "l3", "passed": 3, "total": 3,
                "failed_cases": [], "suite": "s"}),
    );
    assert!(error.contains("enable it"), "{error}");
    let error = refusal(
        &store,
        &json!({"type": "preset.state_changed", "preset_id": "p", "from_state": "disabled",
                "to_state": "disabled"}),
    );
    assert!(error.contains("is disabled already"), "{error}");
    store
        .grading_record(&json!({"type": "preset.state_changed", "preset_id": "p",
                                "from_state": "disabled", "to_state": "active"}))
        .expect("enable");
    onboard(&store, "p", "active", "l3");
    assert_eq!(
        store.grading_catalog().expect("catalog")[0].onboarded_grade,
        Some(SkillGrade::L3)
    );

    // A preset that failed l1 (no grade at all) may sit the exam again.
    register(&store, "f", "default", 1);
    store
        .grading_record(&json!({"type": "preset.onboarded", "preset_id": "f",
                                "from_state": "unexamined", "to_state": "disabled",
                                "to_grade": null, "passed": 0, "total": 3,
                                "failed_cases": ["l1-a"], "suite": "s"}))
        .expect("failed onboarding");
    onboard(&store, "f", "disabled", "l1");
}

/// Unlocking the only grade of a routed preset leaves it unexamined, never
/// active on nothing.
#[test]
fn unlocking_the_only_grade_of_an_enabled_preset_makes_it_unexamined_again() {
    let (store, _schema) = store("grading_unlock");
    register(&store, "p", "default", 1);
    store
        .grading_record(&json!({"type": "preset.grade_locked", "preset_id": "p",
                                "from_grade": null, "to_grade": "l2",
                                "from_state": "unexamined", "to_state": "unexamined"}))
        .expect("lock");
    store
        .grading_record(&json!({"type": "preset.state_changed", "preset_id": "p",
                                "from_state": "unexamined", "to_state": "active"}))
        .expect("enable on the lock");
    assert_eq!(
        store.grading_catalog().expect("catalog")[0].routing_grade(),
        Some(SkillGrade::L2)
    );
    let error = refusal(
        &store,
        &json!({"type": "preset.grade_locked", "preset_id": "p", "from_grade": "l2",
                "to_grade": null, "from_state": "active", "to_state": "active"}),
    );
    assert!(error.contains("is unexamined, not"), "{error}");
    store
        .grading_record(&json!({"type": "preset.grade_locked", "preset_id": "p",
                                "from_grade": "l2", "to_grade": null,
                                "from_state": "active", "to_state": "unexamined"}))
        .expect("unlock");
    let p = &store.grading_catalog().expect("catalog")[0];
    assert_eq!(p.state, PresetState::Unexamined);
    assert_eq!(p.locked_grade, None);
    assert!(!p.is_routable());
}

#[test]
fn only_the_owner_records_grading_and_the_logins_read_the_catalog() {
    let (store, schema) = store("grading_logins");
    register(&store, "p", "default", 1);
    for login in ["spawn_supervisor", "spawn_reader"] {
        let mut client = as_login(&schema, login);
        let rows: i64 = client
            .query_one("select count(*) from grading_presets", &[])
            .expect("the login reads the catalog")
            .get(0);
        assert_eq!(rows, 1, "{login}");
        let error = client
            .query_one(
                "select spawn_grading_record($1)",
                &[&json!({"type": "preset.grade_locked", "preset_id": "p",
                          "from_grade": null, "to_grade": "l3",
                          "from_state": "unexamined", "to_state": "unexamined"})],
            )
            .expect_err("only the owner records grading");
        assert!(
            error
                .as_db_error()
                .is_some_and(|db| db.code().code() == "42501"),
            "{login}: {error}"
        );
    }
}

fn assigned_record(client: &mut Client, task: i64) -> Value {
    client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control' and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
                and (kind -> 'detail' -> 'record' ->> 'id')::bigint = $1",
            &[&task],
        )
        .expect("the record")
        .get(0)
}

type TaskRow = (i16, Option<String>, Option<String>, i32, String);

fn task_row(client: &mut Client, task: i64) -> TaskRow {
    let row = client
        .query_one(
            "select difficulty, preset_id, first_preset_id, quality_rejections, model
               from mission_tasks where id = $1",
            &[&task],
        )
        .expect("the task");
    (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4))
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_task_carries_its_difficulty_and_preset_through_the_control_stream() {
    let (store, schema) = store("grading_assign");
    let mission = store
        .mission_add("Graded", &json!([]), "human")
        .expect("add");
    register(&store, "cheap-l2", "cheap", 0);
    register(&store, "waiting", "default", 0);
    register(&store, "small", "default", 0);
    onboard(&store, "cheap-l2", "unexamined", "l2");
    onboard(&store, "small", "unexamined", "l1");

    // Without the catalog, the record is what it was.
    let plain = store
        .task_assign_with(mission, "ephemeral", "w", "plain", "default", false, false)
        .expect("assign");
    let routed = store
        .task_assign_routed(
            mission,
            "ephemeral",
            "w",
            "routed",
            "cheap",
            false,
            false,
            TaskDifficulty::L1,
            Some("cheap-l2"),
        )
        .expect("assign through the catalog");
    let hard = store
        .task_assign_routed(
            mission,
            "ephemeral",
            "w",
            "hard",
            "default",
            false,
            false,
            TaskDifficulty::L3,
            None,
        )
        .expect("assign at l3 without a preset");
    let mut client = admin(&schema);
    let first = assigned_record(&mut client, plain);
    assert!(first.get("difficulty").is_none() && first.get("preset_id").is_none());
    // Difficulties travel as wire names.
    let second = assigned_record(&mut client, routed);
    assert_eq!(second["difficulty"], json!("l1"));
    assert_eq!(second["preset_id"], json!("cheap-l2"));
    assert_eq!(second["model"], json!("cheap"));
    assert_eq!(
        assigned_record(&mut client, hard)["difficulty"],
        json!("l3")
    );
    assert_eq!(
        task_row(&mut client, plain),
        (2, None, None, 0, "default".to_string())
    );
    assert_eq!(
        task_row(&mut client, routed),
        (
            1,
            Some("cheap-l2".to_string()),
            Some("cheap-l2".to_string()),
            0,
            "cheap".to_string()
        )
    );
    assert_eq!(
        task_row(&mut client, hard),
        (3, None, None, 0, "default".to_string())
    );

    // Refused, recording nothing: another model than the preset's, an
    // unexamined preset or one not in the catalog, a preset graded below
    // the difficulty (the SQL enforces it, not only the CLI), a difficulty
    // out of range.
    let controls = |client: &mut Client| {
        count(
            client,
            "select count(*) from spawn_events where event_name = 'control.recorded'",
        )
    };
    let before = controls(&mut client);
    for (model, difficulty, preset, says) in [
        (
            "default",
            2,
            Some("cheap-l2"),
            "runs on the cheap model profile",
        ),
        ("default", 2, Some("waiting"), "is unexamined"),
        ("default", 2, Some("nobody"), "not in the grading catalog"),
        (
            "default",
            2,
            Some("small"),
            "is graded l1, below the l2 task",
        ),
        (
            "cheap",
            3,
            Some("cheap-l2"),
            "is graded l2, below the l3 task",
        ),
        ("default", 4, None, "difficulty is 1, 2 or 3"),
    ] {
        let error = client
            .query_one(
                "select spawn_task_assign($1, 'ephemeral', 'w', 'refused', $2, false, false,
                                          $3, $4)",
                &[&mission, &model, &difficulty, &preset],
            )
            .expect_err("refused");
        let shown = sql_refusal(&error);
        assert!(shown.contains(says), "{says}: {shown}");
    }
    assert_eq!(controls(&mut client), before);
    // The supervisor's login is held to the same rule.
    let error = as_login(&schema, "spawn_supervisor")
        .query_one(
            "select spawn_task_assign($1, 'ephemeral', 'w', 'refused', 'default', false, false,
                                      2, 'small')",
            &[&mission],
        )
        .expect_err("refused for the supervisor too");
    assert!(sql_refusal(&error).contains("below the l2 task"), "{error}");

    // A re-issue keeps its difficulty and presets; the refold restores
    // every task as it was.
    let again = store
        .task_reissue(
            routed,
            &json!([{"command": 1, "key": "k", "error": "spent"}]),
        )
        .expect("re-issue");
    assert_eq!(task_row(&mut client, again), task_row(&mut client, routed));
    let before = tasks(&mut client);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_eq!(tasks(&mut client), before);
}

#[test]
fn the_old_calls_and_old_records_fold_as_l2_without_a_preset() {
    let (store, schema) = store("grading_old");
    let mission = store.mission_add("Old", &json!([]), "human").expect("add");
    let mut client = admin(&schema);
    let task: i64 = client
        .query_one(
            "select spawn_task_assign($1, 'ephemeral', 'w', 'four arguments')",
            &[&mission],
        )
        .expect("assign with four arguments")
        .get(0);
    let seven: i64 = client
        .query_one(
            "select spawn_task_assign($1, 'ephemeral', 'w', 'seven', 'cheap', false, true)",
            &[&mission],
        )
        .expect("assign with seven arguments")
        .get(0);
    // A legacy row from before the columns, imported.
    client
        .batch_execute(&format!(
            "select spawn_control_record(jsonb_build_object(
                 'type', 'legacy.imported', 'table', 'mission_tasks',
                 'row', jsonb_build_object('id', 900, 'mission_id', {mission},
                        'kind', 'ephemeral', 'agent_id', 'agent_worker_w_900',
                        'session_id', 'session_worker_w_900', 'brief', 'legacy',
                        'state', 'assigned', 'revisions', 0, 'created_by', 'human',
                        'created_at_ms', 1, 'updated_at_ms', 1)))"
        ))
        .expect("import a legacy row");
    // A difficulty as a bare rank folds as its wire name would.
    client
        .batch_execute(&format!(
            "select spawn_control_record(jsonb_build_object(
                 'type', 'task.assigned', 'id', 901, 'mission_id', {mission},
                 'kind', 'ephemeral', 'agent_id', 'agent_worker_w_901',
                 'session_id', 'session_worker_w_901', 'brief', 'ranked',
                 'created_by', 'human', 'model', 'default', 'difficulty', 3))"
        ))
        .expect("a ranked difficulty");
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    let rows: Vec<(i64, i16, Option<String>, i32)> = client
        .query(
            "select id, difficulty, preset_id, quality_rejections from mission_tasks order by id",
            &[],
        )
        .expect("read")
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect();
    assert_eq!(
        rows,
        vec![
            (task, 2, None, 0),
            (seven, 2, None, 0),
            (900, 2, None, 0),
            (901, 3, None, 0)
        ]
    );
}

/// Assign `brief` on `preset` at `difficulty` and bring it to its report.
fn reported(
    store: &PostgresStore,
    mission: i64,
    brief: &str,
    model: &str,
    difficulty: TaskDifficulty,
    preset: &str,
    command: i64,
) -> i64 {
    let task = store
        .task_assign_routed(
            mission,
            "ephemeral",
            "w",
            brief,
            model,
            false,
            false,
            difficulty,
            Some(preset),
        )
        .expect("assign");
    store.task_delivered(task, command).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    task
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_rejection_reroutes_the_rework_in_the_same_statement() {
    let (store, schema) = store("grading_reroute");
    let mission = store
        .mission_add("Rework", &json!([]), "human")
        .expect("add");
    register(&store, "small", "cheap", 0);
    register(&store, "mid", "default", 1);
    register(&store, "strong", "default", 2);
    onboard(&store, "small", "unexamined", "l1");
    onboard(&store, "mid", "unexamined", "l2");
    onboard(&store, "strong", "unexamined", "l3");
    let task = reported(
        &store,
        mission,
        "fix it",
        "cheap",
        TaskDifficulty::L1,
        "small",
        1,
    );

    let rerouted = store
        .task_reject_rerouted(task, "again", None, "mid", 1, TaskDifficulty::L2)
        .expect("reject and reroute");
    assert_eq!(
        rerouted,
        json!({"type": "task.rerouted", "id": task, "preset_id": "mid", "model": "default",
               "from_preset_id": "small", "revision": 1, "routed_difficulty": "l2"})
    );
    let routed = store.routed_task(task).expect("read").expect("the task");
    assert_eq!(routed.state, "revise");
    assert_eq!((routed.revisions, routed.quality_rejections), (1, 1));
    assert_eq!(routed.difficulty, TaskDifficulty::L1);
    assert_eq!(routed.preset_id.as_deref(), Some("mid"));
    assert_eq!(routed.first_preset_id.as_deref(), Some("small"));
    assert_eq!(routed.model, "default");

    // A refused reroute takes the review with it: one statement.
    store.task_delivered(task, 2).expect("delivered again");
    assert!(store.task_reported(task, "done again").expect("reported"));
    let mut client = admin(&schema);
    let reviews = |client: &mut Client| {
        count(
            client,
            "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'task.reviewed'",
        )
    };
    let before = reviews(&mut client);
    for (preset, revision, difficulty, says) in [
        (
            "nobody",
            2,
            TaskDifficulty::L2,
            "not in the grading catalog",
        ),
        ("mid", 2, TaskDifficulty::L2, "runs on preset mid already"),
        ("strong", 5, TaskDifficulty::L2, "is at revision 2, not 5"),
        // Two quality rejections of an l1 task still need l2: a step to
        // l3 is not the one its rejections give.
        ("strong", 2, TaskDifficulty::L3, "is routed at l2, not l3"),
        ("small", 2, TaskDifficulty::L2, "graded below"),
    ] {
        let error = store
            .task_reject_rerouted(task, "again", None, preset, revision, difficulty)
            .expect_err("refused");
        assert!(format!("{error:?}").contains(says), "{says}: {error:?}");
    }
    assert_eq!(reviews(&mut client), before);
    assert_eq!(
        store.routed_task(task).expect("read").expect("task").state,
        "reported"
    );

    // Not routed: refused.
    let plain = store
        .task_assign_with(mission, "ephemeral", "w", "plain", "default", false, false)
        .expect("assign");
    store.task_delivered(plain, 3).expect("delivered");
    assert!(store.task_reported(plain, "done").expect("reported"));
    let error = store
        .task_reject_rerouted(plain, "again", None, "mid", 1, TaskDifficulty::L2)
        .expect_err("refused");
    assert!(
        format!("{error:?}").contains("assigned without the grading catalog"),
        "{error:?}"
    );

    // The refold keeps the reroute, and the first preset.
    let before = tasks(&mut client);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_eq!(tasks(&mut client), before);
}

/// A gate verdict on `task`, through the control stream.
fn gated(client: &mut Client, task: i64, verdict: &str, revision: i32) {
    client
        .execute(
            "select spawn_control_record($1)",
            &[
                &json!({"type": "task.gated", "id": task, "verdict": verdict,
                      "revision": revision, "note": "gate"}),
            ],
        )
        .expect("a gate verdict");
}

/// Calibration's window counts quality alone: a merge conflict or an
/// incompatible head is no rejection, and a cancel without a quality
/// rejection (a re-issue, a reassign, a person's clean cancel) says
/// nothing of the preset. Such a window does not demote a preset that
/// delivered everything it finished.
#[test]
#[allow(clippy::too_many_lines)]
fn calibrations_window_counts_quality_rejections_alone() {
    let (store, schema) = store("grading_window");
    let mission = store
        .mission_add("Window", &json!([]), "human")
        .expect("add");
    register(&store, "small", "cheap", 0);
    register(&store, "mid", "default", 1);
    onboard(&store, "small", "unexamined", "l1");
    onboard(&store, "mid", "unexamined", "l2");
    let mut client = admin(&schema);
    let mut command = 0;
    let mut next = || {
        command += 1;
        command
    };
    let small = |brief: &str, command: i64| {
        reported(
            &store,
            mission,
            brief,
            "cheap",
            TaskDifficulty::L1,
            "small",
            command,
        )
    };
    // First pass: accepted with no rejection.
    let passed = small("a", next());
    store.task_review(passed, true, "", None).expect("accept");
    // Two merge conflicts, then accepted: still first pass.
    let conflicted = small("b", next());
    gated(&mut client, conflicted, "conflict", 0);
    store.task_delivered(conflicted, next()).expect("delivered");
    assert!(store.task_reported(conflicted, "again").expect("reported"));
    gated(&mut client, conflicted, "incompatible", 1);
    store.task_delivered(conflicted, next()).expect("delivered");
    assert!(store.task_reported(conflicted, "again").expect("reported"));
    store
        .task_review(conflicted, true, "", None)
        .expect("accept");
    assert_eq!(
        task_row(&mut client, conflicted).3,
        0,
        "a conflict is no quality rejection"
    );
    // Cancelled cleanly: left out.
    let cancelled = small("c", next());
    store.task_cancel(cancelled).expect("cancel");
    // Re-issued before delivery: the old one left out, the new one open.
    let waiting = store
        .task_assign_routed(
            mission,
            "ephemeral",
            "w",
            "d",
            "cheap",
            false,
            false,
            TaskDifficulty::L1,
            Some("small"),
        )
        .expect("assign");
    let reissued = store
        .task_reissue(
            waiting,
            &json!([{"command": 1, "key": "k", "error": "spent"}]),
        )
        .expect("re-issue");
    // Sent back for quality, delivered again and reassigned: the old one
    // left out; the new one carries the rejection, and when it lands it is
    // the one that counts, not first pass.
    let moved = small("e", next());
    store
        .task_review(moved, false, "wrong", None)
        .expect("send back");
    store.task_delivered(moved, next()).expect("delivered");
    let successor = store
        .task_reassign(moved, Some("worker gone"))
        .expect("reassign")["task"]
        .as_i64()
        .expect("the new task");
    assert_eq!(task_row(&mut client, successor).2.as_deref(), Some("small"));
    assert_eq!(task_row(&mut client, successor).3, 1);
    store.task_delivered(successor, next()).expect("delivered");
    assert!(store.task_reported(successor, "done").expect("reported"));
    store
        .task_review(successor, true, "", None)
        .expect("accept");
    // Sent back by a gate's rejection, then given up on: a failure.
    let rejected = small("f", next());
    gated(&mut client, rejected, "rejected", 0);
    store.task_cancel(rejected).expect("cancel");
    // Sent back and rerouted to mid, then accepted: counts for small, not
    // first pass, and nothing for mid.
    let reworked = small("g", next());
    store
        .task_reject_rerouted(reworked, "again", None, "mid", 1, TaskDifficulty::L2)
        .expect("reroute");
    store.task_delivered(reworked, next()).expect("delivered");
    assert!(store.task_reported(reworked, "done").expect("reported"));
    store.task_review(reworked, true, "", None).expect("accept");
    // Open: not counted. Another difficulty: not counted.
    let open = small("h", next());
    let other = reported(
        &store,
        mission,
        "i",
        "default",
        TaskDifficulty::L2,
        "mid",
        next(),
    );
    store.task_review(other, true, "", None).expect("accept");

    // Distinct closing times, in the order they closed here.
    client
        .batch_execute(&format!(
            "update mission_tasks set updated_at_ms = 1000 + id
              where id in ({passed}, {conflicted}, {successor}, {rejected}, {reworked})"
        ))
        .expect("order the closings");
    let window = store
        .grading_outcomes("small", TaskDifficulty::L1, 0)
        .expect("window");
    let sample = |task_id, first_pass| TaskOutcomeSample {
        task_id,
        first_pass,
    };
    assert_eq!(
        window,
        vec![
            sample(reworked, false),
            sample(rejected, false),
            sample(successor, false),
            sample(conflicted, true),
            sample(passed, true),
        ]
    );
    for left_out in [cancelled, waiting, reissued, moved, open] {
        assert!(
            !window.iter().any(|sample| sample.task_id == left_out),
            "{left_out}"
        );
    }
    // Two of five passed first time, on three real quality failures: the
    // rule demotes. Without them, the infrastructure alone would not: the
    // three clean leavers and the two conflicts that the old window counted
    // as failures are not in it.
    let small_preset = store
        .grading_catalog()
        .expect("catalog")
        .into_iter()
        .find(|preset| preset.preset_id == "small")
        .expect("small");
    assert!(matches!(
        calibrate_preset(&small_preset, &window),
        CalibrationDecision::Demote { .. } | CalibrationDecision::Disable { .. }
    ));
    let infrastructure_only: Vec<TaskOutcomeSample> = window
        .iter()
        .copied()
        .filter(|sample| ![reworked, successor, rejected].contains(&sample.task_id))
        .collect();
    assert_eq!(
        calibrate_preset(&small_preset, &infrastructure_only),
        CalibrationDecision::Hold {
            reason: "too few outcomes".to_string()
        }
    );
    assert_eq!(
        store
            .grading_outcomes("mid", TaskDifficulty::L2, 0)
            .expect("window"),
        vec![sample(other, true)]
    );
    assert!(
        store
            .grading_outcomes("mid", TaskDifficulty::L1, 0)
            .expect("window")
            .is_empty(),
        "the rework rerouted to mid is not mid's"
    );
    // Since its window started only.
    let created: i64 = client
        .query_one(
            "select created_at_ms from mission_tasks where id = $1",
            &[&reworked],
        )
        .expect("read")
        .get(0);
    let since = store
        .grading_outcomes("small", TaskDifficulty::L1, created + 1)
        .expect("window");
    assert!(since.iter().all(|sample| sample.task_id > reworked));
    // The window's index serves its order: the newest first, by preset
    // and difficulty, with no read of the log.
    client
        .batch_execute("set enable_seqscan = off")
        .expect("no sequential scans");
    let plan: Vec<String> = client
        .query(
            "explain select k.id from mission_tasks k
              where k.first_preset_id = 'small' and k.difficulty = 1 and k.created_at_ms >= 0
              order by k.updated_at_ms desc, k.id desc limit 10",
            &[],
        )
        .expect("explain")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert!(
        plan.iter()
            .any(|line| line.contains("mission_tasks_first_preset")),
        "{plan:?}"
    );
}

/// Phase 2: a preset may name its execution profile, any name of a preset
/// id's shape but the two model profiles' own; it is folded, refolded and
/// read with the catalog, and a phase-1 registration is as it was.
#[test]
fn a_named_execution_profile_is_registered_folded_and_checked() {
    let (store, schema) = store("grading_profiles");
    let mut client = admin(&schema);
    register(&store, "plain", "default", 1);
    store
        .grading_record(&json!({"type": "preset.registered", "preset_id": "opus",
                                "engine_label": "claude", "model_label": "opus",
                                "model_profile": "default", "cost_rank": 2,
                                "execution_profile": "opus-max"}))
        .expect("register a named profile");
    store
        .grading_record(&json!({"type": "preset.registered", "preset_id": "qwen",
                                "engine_label": "local", "model_label": "qwen",
                                "model_profile": "cheap", "cost_rank": 0,
                                "execution_profile": "local-qwen"}))
        .expect("register another");
    let catalog = store.grading_catalog().expect("catalog");
    let names: Vec<(&str, &str, Option<&str>)> = catalog
        .iter()
        .map(|p| {
            (
                p.preset_id.as_str(),
                p.profile_name(),
                p.execution_profile
                    .as_ref()
                    .map(spawn_core::grading::ExecutionProfile::as_str),
            )
        })
        .collect();
    assert_eq!(
        names,
        vec![
            ("opus", "opus-max", Some("opus-max")),
            ("plain", "default", None),
            ("qwen", "local-qwen", Some("local-qwen")),
        ]
    );
    assert_eq!(catalog[2].model_profile, ModelProfile::Cheap);
    // The phase-1 shape of a preset without one.
    let plain = serde_json::to_value(&catalog[1]).expect("json");
    assert!(plain.get("execution_profile").is_none(), "{plain}");
    for (profile, says) in [
        (json!("cheap"), "execution profile"),
        (json!("default"), "execution profile"),
        (json!("Opus Max"), "execution profile"),
        (json!(""), "execution profile"),
        (json!(7), "execution profile"),
        (json!(null), "execution profile"),
    ] {
        let error = refusal(
            &store,
            &json!({"type": "preset.registered", "preset_id": "bad",
                    "engine_label": "e", "model_label": "m", "model_profile": "default",
                    "cost_rank": 1, "execution_profile": profile}),
        );
        assert!(error.contains(says), "{profile}: {error}");
    }
    // Other records take no execution profile.
    let error = refusal(
        &store,
        &json!({"type": "preset.state_changed", "preset_id": "opus",
                "from_state": "unexamined", "to_state": "disabled",
                "execution_profile": "opus-max"}),
    );
    assert!(error.contains("and nothing else"), "{error}");
    let before = catalog_rows(&mut client);
    client
        .batch_execute("select spawn_grading_refold(); select spawn_grading_refold();")
        .expect("refold");
    assert_eq!(catalog_rows(&mut client), before);
    // The table refuses a builtin name stored as one.
    let error = client
        .batch_execute("update grading_presets set execution_profile = 'cheap'")
        .expect_err("checked");
    assert!(format!("{error:?}").contains("check"), "{error:?}");
}

/// Phase 2's assignment: a named preset's execution profile and the
/// classifier's decision are recorded on `task.assigned`, a reroute names
/// the new preset's profile, a malformed classification is refused, and
/// every earlier call (nine arguments and fewer) assigns as before.
#[test]
#[allow(clippy::too_many_lines)]
fn an_assignment_records_its_classification_and_profile() {
    let (store, schema) = store("grading_classified");
    let mut client = admin(&schema);
    let mission = store
        .mission_add("Classified", &json!([]), "human")
        .expect("add");
    store
        .grading_record(&json!({"type": "preset.registered", "preset_id": "qwen",
                                "engine_label": "local", "model_label": "qwen",
                                "model_profile": "cheap", "cost_rank": 0,
                                "execution_profile": "local-qwen"}))
        .expect("register");
    onboard(&store, "qwen", "unexamined", "l1");
    store
        .grading_record(&json!({"type": "preset.registered", "preset_id": "opus",
                                "engine_label": "claude", "model_label": "opus",
                                "model_profile": "default", "cost_rank": 2,
                                "execution_profile": "opus-max"}))
        .expect("register");
    onboard(&store, "opus", "unexamined", "l3");
    let classification = json!({"difficulty": "l1", "score": -2, "rules": "brief-v1",
                                "signals": [{"rule": "low_risk", "points": -2,
                                             "evidence": ["typo"]}],
                                "rationale": "l1 (score -2: low_risk -2 [typo])"});
    let task = store
        .task_assign_classified(
            mission,
            "ephemeral",
            "w",
            "fix the typo",
            "cheap",
            false,
            false,
            TaskDifficulty::L1,
            Some("qwen"),
            Some(&classification),
        )
        .expect("assign");
    let record = assigned_record(&mut client, task);
    assert_eq!(record["classification"], classification);
    assert_eq!(record["execution_profile"], json!("local-qwen"));
    assert_eq!(record["preset_id"], json!("qwen"));
    assert_eq!(record["difficulty"], json!("l1"));
    // Malformed classifications record nothing.
    let events = count(&mut client, "select count(*) from spawn_events");
    for bad in [
        json!([]),
        json!({"difficulty": "l4", "signals": []}),
        json!({"difficulty": "l2"}),
        json!({"difficulty": "l2", "signals": {}}),
    ] {
        let error = client
            .query_one(
                "select spawn_task_assign($1, 'ephemeral', 'w', 'refused', 'default', false,
                                          false, 2, null, $2)",
                &[&mission, &bad],
            )
            .expect_err("refused");
        assert!(
            sql_refusal(&error).contains("a classification is an object"),
            "{bad}: {error:?}"
        );
    }
    assert_eq!(
        count(&mut client, "select count(*) from spawn_events"),
        events
    );
    // The nine-argument call of phase 1, positional: as before, and no
    // classification recorded.
    let nine: i64 = client
        .query_one(
            "select spawn_task_assign($1, 'ephemeral', 'w', 'nine', 'cheap', false, false, 1,
                                      'qwen')",
            &[&mission],
        )
        .expect("nine arguments")
        .get(0);
    let record = assigned_record(&mut client, nine);
    assert!(record.get("classification").is_none(), "{record}");
    assert_eq!(record["execution_profile"], json!("local-qwen"));
    // Without a preset: no profile, as before.
    let plain = store
        .task_assign_with(mission, "ephemeral", "w", "plain", "default", false, false)
        .expect("assign");
    let record = assigned_record(&mut client, plain);
    assert!(record.get("execution_profile").is_none(), "{record}");
    assert!(record.get("classification").is_none(), "{record}");

    // The reroute names the new preset's profile; the refold keeps it all.
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    let rerouted = store
        .task_reject_rerouted(task, "again", None, "opus", 1, TaskDifficulty::L2)
        .expect("reroute");
    assert_eq!(rerouted["execution_profile"], json!("opus-max"));
    assert_eq!(rerouted["model"], json!("default"));
    let before = tasks(&mut client);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_eq!(tasks(&mut client), before);

    // When each preset last had a task first assigned to it, by difficulty.
    let activity = store.grading_activity().expect("activity");
    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].preset_id, "qwen");
    assert_eq!(activity[0].difficulty, TaskDifficulty::L1);
    let newest: i64 = client
        .query_one(
            "select max(created_at_ms) from mission_tasks where first_preset_id = 'qwen'",
            &[],
        )
        .expect("read")
        .get(0);
    assert_eq!(activity[0].last_assigned_ms, newest);
}

/// A gate's quality rejection and its rework reroute are one statement:
/// both or neither, as a review's.
#[test]
fn a_gates_rejection_and_its_reroute_are_recorded_both_or_neither() {
    let (store, schema) = store("grading_gate_reroute");
    let mut client = admin(&schema);
    let mission = store
        .mission_add("Gated", &json!([]), "human")
        .expect("add");
    register(&store, "small", "cheap", 0);
    register(&store, "mid", "default", 1);
    onboard(&store, "small", "unexamined", "l1");
    onboard(&store, "mid", "unexamined", "l2");
    let task = reported(
        &store,
        mission,
        "gated",
        "cheap",
        TaskDifficulty::L1,
        "small",
        1,
    );
    let verdict = |revision: i32| {
        json!({"type": "task.gated", "id": task, "verdict": "rejected", "revision": revision,
               "base": "b", "note": "a check failed"})
    };
    let gates = |client: &mut Client| {
        count(
            client,
            "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'",
        )
    };
    // Refused reroutes take the verdict with them.
    for (preset, revision, difficulty, says) in [
        (
            "nobody",
            1,
            TaskDifficulty::L2,
            "not in the grading catalog",
        ),
        (
            "small",
            1,
            TaskDifficulty::L2,
            "runs on preset small already",
        ),
        ("mid", 4, TaskDifficulty::L2, "is at revision 1, not 4"),
        ("mid", 1, TaskDifficulty::L3, "is routed at l2, not l3"),
    ] {
        let error = store
            .integrator_record_rerouted(&verdict(0), preset, revision, difficulty)
            .expect_err("refused");
        assert!(format!("{error:?}").contains(says), "{says}: {error:?}");
        assert_eq!(gates(&mut client), 0);
        assert_eq!(
            store.routed_task(task).expect("read").expect("task").state,
            "reported"
        );
    }
    let (event, rerouted) = store
        .integrator_record_rerouted(&verdict(0), "mid", 1, TaskDifficulty::L2)
        .expect("both");
    assert!(event > 0);
    assert_eq!(
        rerouted,
        json!({"type": "task.rerouted", "id": task, "preset_id": "mid", "model": "default",
               "from_preset_id": "small", "revision": 1, "routed_difficulty": "l2"})
    );
    assert_eq!(gates(&mut client), 1);
    let routed = store.routed_task(task).expect("read").expect("task");
    assert_eq!(routed.state, "revise");
    assert_eq!((routed.revisions, routed.quality_rejections), (1, 1));
    assert_eq!(routed.preset_id.as_deref(), Some("mid"));
    assert_eq!(routed.first_preset_id.as_deref(), Some("small"));
    // The integrator's own refusal of a record still holds: a second
    // verdict on the revision it already judged reroutes nothing.
    assert!(
        store
            .integrator_record_rerouted(&verdict(0), "mid", 1, TaskDifficulty::L2)
            .is_err()
    );
    assert_eq!(gates(&mut client), 1);
}

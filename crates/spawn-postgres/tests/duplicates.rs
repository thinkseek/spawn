//! A duplicate brief in flight (objective 212, GitHub #63 part 2):
//! `spawn_task_assign` refuses the supervisor's brief that a task of the same
//! mission in flight already has (both trimmed of space, tab, CR and LF),
//! naming that task (the lowest id), the mission and its state, and records
//! nothing; with `again` it is assigned. The owner's duplicate is assigned.
//! Whenever there is such a task, the `task.assigned` record names it
//! (`duplicate_of`); a task with none records none.
//!
//! In flight: assigned, delivered, reported, revise or landing, or accepted
//! in a mission with a gate (it still waits for its gate).
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::{PostgresStore, role_url};

mod common;
use common::{admin, schema_url, store};

const BRIEF: &str = "audit the judge's verdicts against the host";

fn control(client: &mut Client, record: &Value) -> i64 {
    client
        .query_one("select spawn_control_record($1)", &[record])
        .expect("a control record")
        .get(0)
}

/// Mission `id`, with a gate when `gated`.
fn mission(client: &mut Client, id: i64, gated: bool) {
    control(
        client,
        &json!({"type": "mission.added", "id": id, "text": format!("mission {id}"),
                "origin": "human", "sources": []}),
    );
    if gated {
        control(
            client,
            &json!({"type": "mission.configured", "id": id, "gate": "/gate",
                    "accept": "gate"}),
        );
    }
}

/// The supervisor's store: its own login in the case's schema.
fn supervisor(schema: &str) -> PostgresStore {
    PostgresStore::connect(
        &role_url(&schema_url(schema), "spawn_supervisor").expect("the login's URL"),
    )
    .expect("the supervisor logs in")
}

fn assign(store: &PostgresStore, mission: i64, brief: &str) -> Result<i64, String> {
    store
        .task_assign(mission, "ephemeral", "audit", brief, "default")
        .map_err(|error| error.to_string())
}

fn tasks(client: &mut Client) -> i64 {
    client
        .query_one("select count(*) from mission_tasks", &[])
        .expect("count")
        .get(0)
}

fn assigned_records(client: &mut Client) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'",
            &[],
        )
        .expect("count")
        .get(0)
}

/// The `duplicate_of` of task `task`'s `task.assigned` record.
fn duplicate_of(client: &mut Client, task: i64) -> Option<i64> {
    let record: Value = client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
                and (kind -> 'detail' -> 'record' ->> 'id')::bigint = $1",
            &[&task],
        )
        .expect("the task's record")
        .get(0);
    record.get("duplicate_of").and_then(Value::as_i64)
}

/// The supervisor's `brief` is refused naming `task` of `mission` in
/// `state`, and nothing is recorded or made.
fn refused(
    client: &mut Client,
    supervisor: &PostgresStore,
    mission: i64,
    brief: &str,
    (task, state): (i64, &str),
) {
    let before = (tasks(client), assigned_records(client));
    let error = assign(supervisor, mission, brief).expect_err("a duplicate is refused");
    let words = format!(
        "task {task} of mission {mission} has the same brief and is {state}: a duplicate is \
         not assigned (cancel it first, or assign again on purpose with --again)"
    );
    assert!(error.contains(&words), "{error}");
    assert_eq!(
        (tasks(client), assigned_records(client)),
        before,
        "nothing recorded"
    );
}

/// Every state of a task: which are in flight (refused, naming the state),
/// which are not (assigned); the owner's duplicate and `again` assigned with
/// `duplicate_of`; another brief or another mission with none.
#[test]
fn the_supervisors_duplicate_in_flight_is_refused_and_every_assigned_one_names_it() {
    let (owner, schema) = store("duplicates_states");
    let mut client = admin(&schema);
    mission(&mut client, 1, false);
    mission(&mut client, 2, false);
    mission(&mut client, 3, true);
    let supervisor = supervisor(&schema);

    let first = assign(&supervisor, 1, BRIEF).expect("the first is assigned");
    assert_eq!(duplicate_of(&mut client, first), None);
    refused(&mut client, &supervisor, 1, BRIEF, (first, "assigned"));
    refused(
        &mut client,
        &supervisor,
        1,
        &format!("  {BRIEF}\n"),
        (first, "assigned"),
    );

    // Another mission, another brief: no duplicate.
    let other_mission = assign(&supervisor, 2, BRIEF).expect("another mission");
    assert_eq!(duplicate_of(&mut client, other_mission), None);
    let other_brief = assign(&supervisor, 1, "another brief").expect("another brief");
    assert_eq!(duplicate_of(&mut client, other_brief), None);

    // The owner's duplicate, and the supervisor's on purpose.
    let owners = assign(&owner, 1, BRIEF).expect("the owner's duplicate");
    assert_eq!(duplicate_of(&mut client, owners), Some(first));
    let again = supervisor
        .task_assign_again(1, "ephemeral", "audit", BRIEF, "default", true)
        .expect("again");
    assert_eq!(duplicate_of(&mut client, again), Some(first));

    // Every task of the brief cancelled: not in flight.
    for task in [first, owners, again] {
        control(&mut client, &json!({"type": "task.cancelled", "id": task}));
    }
    let fresh = assign(&supervisor, 1, BRIEF).expect("cancelled is not in flight");
    assert_eq!(duplicate_of(&mut client, fresh), None);

    // Delivered, reported, sent back: in flight, their state named.
    control(
        &mut client,
        &json!({"type": "task.delivered", "id": fresh, "command_id": 1}),
    );
    refused(&mut client, &supervisor, 1, BRIEF, (fresh, "delivered"));
    control(
        &mut client,
        &json!({"type": "task.reported", "id": fresh, "report": "done"}),
    );
    refused(&mut client, &supervisor, 1, BRIEF, (fresh, "reported"));
    control(
        &mut client,
        &json!({"type": "task.reviewed", "id": fresh, "accepted": false, "note": "more"}),
    );
    refused(&mut client, &supervisor, 1, BRIEF, (fresh, "revise"));

    // Accepted in a mission with no gate: done.
    control(
        &mut client,
        &json!({"type": "task.reviewed", "id": fresh, "accepted": true}),
    );
    let after_accepted = assign(&supervisor, 1, BRIEF).expect("accepted, ungated: done");
    assert_eq!(duplicate_of(&mut client, after_accepted), None);

    // Accepted in a gated mission: in flight; landing too; landed: done.
    let gated = assign(&supervisor, 3, BRIEF).expect("the gated mission's first");
    control(
        &mut client,
        &json!({"type": "task.delivered", "id": gated, "command_id": 2}),
    );
    control(
        &mut client,
        &json!({"type": "task.reported", "id": gated, "report": "done"}),
    );
    control(
        &mut client,
        &json!({"type": "task.reviewed", "id": gated, "accepted": true}),
    );
    refused(&mut client, &supervisor, 3, BRIEF, (gated, "accepted"));
    control(
        &mut client,
        &json!({"type": "task.gated", "id": gated, "verdict": "accepted",
                "candidate": "c".repeat(40), "base": "b".repeat(40)}),
    );
    refused(&mut client, &supervisor, 3, BRIEF, (gated, "landing"));
    control(
        &mut client,
        &json!({"type": "task.landed", "id": gated, "from": "a".repeat(40),
                "to": "b".repeat(40)}),
    );
    let after_landed = assign(&supervisor, 3, BRIEF).expect("landed: done");
    assert_eq!(duplicate_of(&mut client, after_landed), None);
}

/// Both briefs are trimmed of space, tab, CR and LF; with two tasks of the
/// brief in flight, the refusal and the owner's next duplicate name the
/// lowest id.
#[test]
fn both_briefs_are_trimmed_and_the_lowest_task_in_flight_is_named() {
    let (owner, schema) = store("duplicates_trimmed");
    let mut client = admin(&schema);
    mission(&mut client, 1, false);
    let supervisor = supervisor(&schema);

    let stored = assign(&supervisor, 1, &format!("\t {BRIEF}\r\n")).expect("assigned");
    refused(&mut client, &supervisor, 1, BRIEF, (stored, "assigned"));
    refused(
        &mut client,
        &supervisor,
        1,
        &format!("\t{BRIEF}\r"),
        (stored, "assigned"),
    );
    // Inside the brief nothing is trimmed: one that only looks alike is
    // another brief.
    let alike = assign(&supervisor, 1, &BRIEF.replace(' ', "  ")).expect("only alike");
    assert_eq!(duplicate_of(&mut client, alike), None);

    let second = assign(&owner, 1, BRIEF).expect("the owner's duplicate");
    assert_eq!(duplicate_of(&mut client, second), Some(stored));
    refused(&mut client, &supervisor, 1, BRIEF, (stored, "assigned"));
    let third = assign(&owner, 1, BRIEF).expect("the owner's second duplicate");
    assert_eq!(duplicate_of(&mut client, third), Some(stored));
}

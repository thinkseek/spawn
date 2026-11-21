//! Work stealing (objective 239): `spawn_task_steal` moves a queued,
//! unstarted task of a busy resident to an idle resident of the same pool
//! by one `task.stolen` record, checked under the append lock, or refuses
//! naming the first rule that fails, recording nothing; and
//! `spawn_task_delivered` never records a delivery into a session that is
//! not the task's worker.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::json;
use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::CommandQueue;
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, store};

/// A dispatcher command into `worker`'s session, under `key`.
fn command(store: &PostgresStore, worker: &str, key: &str) -> i64 {
    let id = store
        .submit(
            &Principal::new("spawn-dispatch").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(format!("agent_resident_{worker}")).expect("agent"),
                session_id: SessionId::new(format!("session_resident_{worker}")).expect("session"),
                idempotency_key: IdempotencyKey::new(key).expect("key"),
                action: CommandAction::Input {
                    text: "the task".to_string(),
                    tags: Vec::new(),
                },
            },
        )
        .expect("submit");
    i64::try_from(id.0).expect("a small id")
}

/// Residents in pool `logs` (ben, the busy worker; ann, cid, dee), sol in
/// `other`, una in none, hal proposed only, ivy retired; mission 1 open.
fn world(store: &PostgresStore) -> i64 {
    let mission = store
        .mission_add("239 work", &json!([]), "human")
        .expect("mission");
    for (name, pool) in [
        ("ann", Some("logs")),
        ("ben", Some("logs")),
        ("cid", Some("logs")),
        ("dee", Some("logs")),
        ("hal", Some("logs")),
        ("ivy", Some("logs")),
        ("sol", Some("other")),
        ("una", None),
    ] {
        store
            .resident_propose_in(name, "Analyses the logs.", pool)
            .expect("propose");
        if name != "hal" {
            store.resident_approve(name).expect("approve");
        }
    }
    store.resident_retire("ivy").expect("retire");
    mission
}

fn assign(store: &PostgresStore, mission: i64, worker: &str, pin: bool) -> i64 {
    store
        .task_assign_with(
            mission,
            "resident",
            worker,
            &format!("work of {worker} {}", fastrand()),
            "default",
            false,
            pin,
        )
        .expect("assign")
}

fn fastrand() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

/// `task` delivered to its worker by a command typed in.
fn deliver(store: &PostgresStore, task: i64, worker: &str) -> i64 {
    let sent = command(store, worker, &format!("deliver-{task}-0"));
    store.task_delivered(task, sent).expect("delivered");
    sent
}

/// What `spawn_task_delivered` answers for `task` and `command`.
fn delivered(client: &mut Client, task: i64, command: i64) -> bool {
    client
        .query_one("select spawn_task_delivered($1, $2)", &[&task, &command])
        .expect("answered")
        .get(0)
}

fn stolen(client: &mut Client) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events where session_id = 'session_control'
               and kind -> 'detail' -> 'record' ->> 'type' = 'task.stolen'",
            &[],
        )
        .expect("count")
        .get(0)
}

fn session(client: &mut Client, task: i64) -> String {
    client
        .query_opt(
            "select session_id from mission_tasks where id = $1",
            &[&task],
        )
        .expect("the task")
        .map_or_else(|| "no such task".to_string(), |row| row.get(0))
}

/// Whether moving `task` to `to` is refused with a message starting `task
/// <id> ` and holding `why`, recording nothing and moving nothing.
fn refused(store: &PostgresStore, client: &mut Client, task: i64, to: &str, why: &str) {
    let (records, owner) = (stolen(client), session(client, task));
    let error = store
        .task_steal(task, to, "human", None)
        .expect_err(&format!("task {task} to {to} refused ({why})"))
        .to_string();
    assert!(error.contains(&format!("task {task} ")), "{error}");
    assert!(error.contains(why), "{task} to {to}: {why}: {error}");
    assert_eq!(
        (stolen(client), session(client, task)),
        (records, owner),
        "{why}"
    );
}

/// The mission rule: closed, paused, halted, each refused while it holds.
fn mission_rule(store: &PostgresStore, client: &mut Client, mission: i64, queued: i64) {
    for (column, value, why) in [
        ("state", "'done'", "(closed)"),
        ("pause_event", "1", "(paused)"),
        ("halted", "'red'", "(halted)"),
    ] {
        client
            .batch_execute(&format!(
                "update mission_missions set {column} = {value} where id = {mission}"
            ))
            .expect("the mission");
        refused(
            store,
            client,
            queued,
            "ann",
            &format!("waits for its mission {mission} {why}"),
        );
        client
            .batch_execute(&format!(
                "update mission_missions set state = 'active', pause_event = null, halted = null \
                 where id = {mission}"
            ))
            .expect("open again");
    }
}

/// The move, to an idle member of the pool: the answer, the record, the
/// fold, and the same owner after a refold.
fn moves_to_an_idle_member(store: &PostgresStore, client: &mut Client, queued: i64) {
    let moved = store
        .task_steal(queued, "ann", "human", Some("ann is idle"))
        .expect("moved");
    assert_eq!(
        moved,
        json!({"task": queued, "from_session": "session_resident_ben",
               "to_session": "session_resident_ann", "agent_id": "agent_resident_ann"})
    );
    let record: serde_json::Value = client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events where session_id = 'session_control'
               and kind -> 'detail' -> 'record' ->> 'type' = 'task.stolen'",
            &[],
        )
        .expect("the record")
        .get(0);
    let created: i64 = client
        .query_one(
            "select created_at_ms from mission_tasks where id = $1",
            &[&queued],
        )
        .expect("created")
        .get(0);
    assert_eq!(
        record,
        json!({"type": "task.stolen", "id": queued,
               "from_session": "session_resident_ben", "from_agent": "agent_resident_ben",
               "to_session": "session_resident_ann", "to_agent": "agent_resident_ann",
               "pool": "logs", "by": "human", "reason": "ann is idle",
               "queued_since_ms": created})
    );
    assert_eq!(session(client, queued), "session_resident_ann");
    // A refold gives the same owner.
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_eq!(session(client, queued), "session_resident_ann");
}

/// Every reason of every check, in order, each recording nothing.
#[test]
fn a_move_is_refused_for_each_rule_and_records_nothing() {
    let (store, schema) = store("steal_refusals");
    let mut client = admin(&schema);
    let mission = world(&store);
    let running = assign(&store, mission, "ben", false);
    deliver(&store, running, "ben");
    let queued = assign(&store, mission, "ben", false);
    let pinned = assign(&store, mission, "ben", true);
    let typed = assign(&store, mission, "ben", false);
    command(&store, "ben", &format!("deliver-{typed}-0"));
    let cancelled = assign(&store, mission, "ben", false);
    store.task_cancel(cancelled).expect("cancel");
    let ephemeral = store
        .task_assign(
            mission,
            "ephemeral",
            "helper",
            "an ephemeral task",
            "default",
        )
        .expect("ephemeral");

    for (task, why) in [
        (999_999, "not queued (no such task)"),
        (ephemeral, "not queued (not a resident task)"),
        (running, "not queued (delivered)"),
        (cancelled, "not queued (cancelled)"),
        (typed, "not queued (a delivery command exists)"),
    ] {
        refused(&store, &mut client, task, "ann", why);
    }
    client
        .execute(
            "update mission_tasks set revisions = 1 where id = $1",
            &[&queued],
        )
        .expect("revised");
    refused(&store, &mut client, queued, "ann", "not queued (revised)");
    client
        .execute(
            "update mission_tasks set revisions = 0 where id = $1",
            &[&queued],
        )
        .expect("not revised");
    refused(&store, &mut client, pinned, "ann", "is pinned to ben");

    mission_rule(&store, &mut client, mission, queued);

    for (to, why) in [
        ("nobody", "cannot go to nobody (no such resident)"),
        ("hal", "cannot go to hal (not approved)"),
        ("ivy", "cannot go to ivy (retired)"),
        ("ben", "cannot go to ben (already its worker)"),
        ("una", "cannot go to una (no pool)"),
        ("sol", "cannot go to sol (pool logs is not other)"),
    ] {
        refused(&store, &mut client, queued, to, why);
    }
    assert_eq!(stolen(&mut client), 0);

    moves_to_an_idle_member(&store, &mut client, queued);
}

/// The taker must be idle: a task of its own assigned, delivered,
/// reported, sent back, landing, or accepted in a gated mission, or a
/// cancel notice not yet told, each refuses it.
#[test]
fn a_taker_holding_any_open_task_or_an_untold_cancel_is_not_idle() {
    let (store, schema) = store("steal_idle");
    let mut client = admin(&schema);
    let mission = world(&store);
    let running = assign(&store, mission, "ben", false);
    deliver(&store, running, "ben");
    let queued = assign(&store, mission, "ben", false);
    let own = assign(&store, mission, "cid", false);
    for state in ["assigned", "delivered", "reported", "revise", "landing"] {
        client
            .execute(
                "update mission_tasks set state = $2 where id = $1",
                &[&own, &state],
            )
            .expect("a state");
        refused(
            &store,
            &mut client,
            queued,
            "cid",
            &format!("(not idle: task {own} {state})"),
        );
    }
    // Accepted: open only in a mission with a gate.
    client
        .execute(
            "update mission_tasks set state = 'accepted' where id = $1",
            &[&own],
        )
        .expect("accepted");
    client
        .execute(
            "update mission_missions set gate = '/gate' where id = $1",
            &[&mission],
        )
        .expect("gated");
    refused(
        &store,
        &mut client,
        queued,
        "cid",
        &format!("(not idle: task {own} accepted)"),
    );
    client
        .execute(
            "update mission_missions set gate = null where id = $1",
            &[&mission],
        )
        .expect("ungated");
    // A cancel notice not yet told: cid's latest delivered task cancelled.
    client
        .execute(
            "update mission_tasks set state = 'assigned' where id = $1",
            &[&own],
        )
        .expect("assigned again");
    deliver(&store, own, "cid");
    store.task_cancel(own).expect("cancel");
    refused(
        &store,
        &mut client,
        queued,
        "cid",
        "(not idle: a cancel notice is not told)",
    );
    // Told: idle.
    let notice = command(&store, "cid", &format!("cancel-notice-{own}-0"));
    client
        .execute(
            "update spawn_commands set succeeded = true where id = $1",
            &[&notice],
        )
        .expect("told");
    store
        .task_steal(queued, "cid", "human", None)
        .expect("cid is idle");
}

/// A task that is its worker's next (its worker idle, the task its oldest)
/// is never taken; its second is, and after the move the taker holds it
/// alone, so a second taker is refused.
#[test]
fn an_idle_workers_next_task_stays_and_two_takers_have_one_winner() {
    let (store, schema) = store("steal_next");
    let mut client = admin(&schema);
    let mission = world(&store);
    let next = assign(&store, mission, "ben", false);
    let second = assign(&store, mission, "ben", false);
    refused(
        &store,
        &mut client,
        next,
        "ann",
        "is its worker's next (its worker is idle)",
    );
    // Two takers of the second task at once: one wins.
    let (url, path) = (common::schema_url(&schema), schema.to_string());
    let takers: Vec<_> = ["ann", "dee"]
        .into_iter()
        .map(|to| {
            let url = url.clone();
            std::thread::spawn(move || {
                PostgresStore::connect(&url)
                    .expect("connect")
                    .task_steal(second, to, "human", None)
                    .is_ok()
            })
        })
        .collect();
    let won: Vec<bool> = takers
        .into_iter()
        .map(|taker| taker.join().expect("a taker"))
        .collect();
    assert_eq!(
        won.iter().filter(|won| **won).count(),
        1,
        "{won:?} ({path})"
    );
    assert_eq!(stolen(&mut client), 1);
    let winner = if won[0] { "ann" } else { "dee" };
    assert_eq!(
        session(&mut client, second),
        format!("session_resident_{winner}")
    );
    // Moved, it is the winner's next: no one else takes it.
    let loser = if won[0] { "dee" } else { "ann" };
    refused(
        &store,
        &mut client,
        second,
        loser,
        "is its worker's next (its worker is idle)",
    );
}

/// A delivery planned before a move is never recorded for the old worker:
/// a command into a session that is not the task's is refused.
#[test]
fn a_delivery_into_a_session_that_is_not_the_tasks_is_refused() {
    let (store, schema) = store("steal_delivered");
    let mut client = admin(&schema);
    let mission = world(&store);
    let running = assign(&store, mission, "ben", false);
    deliver(&store, running, "ben");
    let queued = assign(&store, mission, "ben", false);
    store
        .task_steal(queued, "ann", "human", None)
        .expect("moved");
    let old = command(&store, "ben", &format!("deliver-{queued}-0"));
    assert!(!delivered(&mut client, queued, old));
    let state: String = client
        .query_one("select state from mission_tasks where id = $1", &[&queued])
        .expect("the task")
        .get(0);
    assert_eq!(state, "assigned");
    let new = command(&store, "ann", &format!("deliver-{queued}-1"));
    assert!(delivered(&mut client, queued, new));
}

/// A delivery row carries its mission's state and halt (objective 239 fix
/// F1), so the dispatcher's stealing pass skips a halted or closed
/// mission's task without another read.
#[test]
fn a_delivery_row_carries_its_missions_state_and_halt() {
    let (store, schema) = store("steal_row_state");
    let mut client = admin(&schema);
    let mission = world(&store);
    let running = assign(&store, mission, "ben", false);
    deliver(&store, running, "ben");
    let queued = assign(&store, mission, "ben", false);
    let other = store
        .mission_add("Other work", &json!([]), "human")
        .expect("mission");
    let elsewhere = assign(&store, other, "ann", false);
    let row = |task: i64| -> serde_json::Value {
        store
            .delivery_rows()
            .expect("rows")
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["id"].as_i64() == Some(task))
            .cloned()
            .expect("the row")
    };
    assert_eq!(
        (
            row(queued)["mission_state"].as_str(),
            row(queued)["mission_halted"].as_bool()
        ),
        (Some("open"), Some(false))
    );
    client
        .batch_execute(&format!(
            "update mission_missions set halted = 'red' where id = {mission}"
        ))
        .expect("halt");
    assert_eq!(row(queued)["mission_halted"].as_bool(), Some(true));
    client
        .batch_execute(&format!(
            "update mission_missions set state = 'done' where id = {other}"
        ))
        .expect("close");
    assert_eq!(row(elsewhere)["mission_state"].as_str(), Some("done"));
}

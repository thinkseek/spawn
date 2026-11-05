//! What the dispatcher measures stalls from (`stall_facts`): every open
//! task with the control record that put it at its step and that record's
//! age on the database's clock, the latest fact of a delivered task's
//! session, a gated mission's latest verdict that is not `stale`, and the
//! halted missions with their halt record.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, BranchName, CommitId, SessionId};
use spawn_core::port::EventLog;
use spawn_core::screen::ScreenState;
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, store};

fn db_now(client: &mut Client) -> i64 {
    client
        .query_one("select spawn_now_ms()", &[])
        .expect("the database's clock")
        .get(0)
}

/// The latest control record about task `task`: its event id.
fn latest_record(client: &mut Client, task: i64) -> i64 {
    client
        .query_one(
            "select max(id) from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'id' = $1",
            &[&task.to_string()],
        )
        .expect("the task's latest record")
        .get(0)
}

/// Move control record `id` to `ago_ms` before the database's clock.
fn age_record(client: &mut Client, id: i64, ago_ms: i64) -> i64 {
    let at = db_now(client) - ago_ms;
    client
        .execute(
            "update spawn_events set recorded_at_ms = $1 where id = $2",
            &[&at, &id],
        )
        .expect("age the record");
    at
}

fn task<'a>(facts: &'a Value, id: i64) -> &'a Value {
    facts["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .find(|task| task["id"] == id)
        .unwrap_or_else(|| panic!("task {id} in {facts:#}"))
}

fn append(store: &PostgresStore, session: &str, key: &str, source: Source, kind: EventKind) {
    store
        .append(NewEvent {
            agent_id: AgentId::new(session.replace("session_", "agent_")).expect("valid agent"),
            session_id: SessionId::new(session).expect("valid session"),
            execution_id: None,
            source,
            source_event_id: Some(key.to_string()),
            kind,
        })
        .expect("append");
}

#[test]
#[allow(clippy::too_many_lines)]
fn each_open_task_comes_with_its_steps_record_and_its_age_on_the_databases_clock() {
    let (store, schema) = store("stall_facts");
    let mut client = admin(&schema);
    let mission = store
        .mission_add("Stalls", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let first = store
        .task_assign(mission, "ephemeral", "w", "first", "default")
        .expect("assign");
    let second = store
        .task_assign(mission, "ephemeral", "v", "second", "default")
        .expect("assign another");

    // Assigned: measured from `task.assigned`, aged on the database's clock.
    let assigned = latest_record(&mut client, first);
    let at = age_record(&mut client, assigned, 600_000);
    let facts = store.stall_facts().expect("stall facts");
    let now = facts["now_ms"].as_i64().expect("now");
    let after = db_now(&mut client);
    assert!(now <= after && after - now < 5_000, "now {now}, then {after}");
    let record = &task(&facts, first)["record"];
    assert_eq!(record["id"], assigned);
    assert_eq!(record["type"], "task.assigned");
    assert_eq!(record["at_ms"], at);
    let age = record["age_ms"].as_i64().expect("age");
    assert_eq!(age, now - at);
    assert!((600_000..605_000).contains(&age), "{age}");
    assert_eq!(task(&facts, second)["record"]["type"], "task.assigned");
    assert_eq!(task(&facts, first)["state"], "assigned");

    // Delivered: `task.delivered`, and the session's latest fact, which is
    // neither the Observer's nor storage's.
    store.task_delivered(first, 1).expect("delivered");
    let session = task(&facts, first)["session_id"]
        .as_str()
        .expect("session")
        .to_string();
    append(
        &store,
        &session,
        "tool-1",
        Source::Transcript,
        EventKind::ToolStarted {
            tool_name: "Bash".to_string(),
            tool_use_id: "toolu_1".to_string(),
        },
    );
    let shells = "✻ Sautéed for 6s · done 11:27 PM · 2 shells still running";
    append(
        &store,
        &session,
        "screen-1",
        Source::Bridge,
        EventKind::ScreenObserved {
            state: ScreenState::Ready,
            evidence: shells.to_string(),
        },
    );
    let commit = CommitId::new(format!("{:040x}", 7)).expect("valid commit");
    append(
        &store,
        &session,
        "commit-1",
        Source::Bridge,
        EventKind::ArtifactCommitted {
            reason: CheckpointReason::BaseMerged,
            artifact: ArtifactRef {
                project: "missions".to_string(),
                branch: BranchName::new("agent_worker_w").expect("valid branch"),
                commit,
            },
            files: Vec::new(),
        },
    );
    let facts = store.stall_facts().expect("stall facts");
    assert_eq!(task(&facts, first)["record"]["type"], "task.delivered");
    let latest = facts["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|latest| latest["session_id"] == session.as_str())
        .expect("the delivered task's session");
    assert_eq!(
        latest["event_name"], "screen.observed",
        "the checkpoint after it is storage's, no fact: {latest:#}"
    );
    assert_eq!(latest["screen"], shells);
    assert_eq!(
        latest["age_ms"].as_i64(),
        Some(facts["now_ms"].as_i64().expect("now") - latest["at_ms"].as_i64().expect("at"))
    );

    // Sent back: measured from the review that sent it back.
    assert!(store.task_reported(first, "done").expect("reported"));
    assert_eq!(
        task(&store.stall_facts().expect("stall facts"), first)["record"]["type"],
        "task.reported"
    );
    store
        .task_review(first, false, "again", None)
        .expect("send back");
    let facts = store.stall_facts().expect("stall facts");
    assert_eq!(task(&facts, first)["state"], "revise");
    assert_eq!(task(&facts, first)["record"]["type"], "task.reviewed");
    assert_eq!(
        task(&facts, first)["record"]["id"],
        latest_record(&mut client, first)
    );

    // A cancelled task whose worker never ran is not open.
    store.task_cancel(second).expect("cancel");
    let facts = store.stall_facts().expect("stall facts");
    assert!(
        facts["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .all(|open| open["id"] != second),
        "{facts:#}"
    );
}

#[test]
fn a_gated_missions_verdicts_and_its_halt_are_read_from_their_records() {
    let (store, schema) = store("stall_gate");
    let mut client = admin(&schema);
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
    let task_id = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task_id, 1).expect("delivered");
    assert!(store.task_reported(task_id, "done").expect("reported"));
    store.task_review(task_id, true, "", None).expect("accept");
    let accepted = latest_record(&mut client, task_id);
    let verdict = |verdict: &str| {
        store
            .integrator_record(&json!({"type": "task.gated", "id": task_id,
                                       "verdict": verdict, "revision": 0, "base": "b1",
                                       "reason": format!("{verdict} reason")}))
            .expect("record a verdict")
    };
    verdict("aborted");
    let aborted = latest_record(&mut client, task_id);
    verdict("stale");
    let facts = store.stall_facts().expect("stall facts");
    let open = task(&facts, task_id);
    assert_eq!(open["state"], "accepted");
    assert_eq!(open["gated"], true);
    assert_eq!(open["accept"], "review");
    assert_eq!(
        open["record"]["id"], accepted,
        "a stale verdict moves no step"
    );
    assert_eq!(open["stale"]["reason"], "stale reason");
    assert_eq!(
        facts["verdicts"],
        json!([{"mission_id": mission, "id": aborted, "verdict": "aborted",
                "task": task_id.to_string(),
                "at_ms": facts["verdicts"][0]["at_ms"], "age_ms": facts["verdicts"][0]["age_ms"]}]),
        "the latest verdict that is not stale"
    );

    store
        .integrator_record(&json!({"type": "mission.halted", "id": mission,
                                   "reason": "3 gate aborts in a row"}))
        .expect("halt");
    let halted_at = latest_record(&mut client, mission);
    let facts = store.stall_facts().expect("stall facts");
    let halt = &facts["halted"][0];
    assert_eq!(halt["mission_id"], mission);
    assert_eq!(halt["reason"], "3 gate aborts in a row");
    assert_eq!(halt["record"]["id"], halted_at);
    assert_eq!(halt["record"]["type"], "mission.halted");
    assert_eq!(halt["tasks"], json!([task_id]));
    assert_eq!(task(&facts, task_id)["halted"], true);
}

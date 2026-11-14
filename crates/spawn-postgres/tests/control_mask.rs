//! Control records stored masked (objective 170), through the real store:
//! the free text of a gate's verdict, a host fault, a deploy and an alert
//! is masked before `spawn_control_record` keeps it, and what the folds and
//! readers match keeps its bytes.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, store};
use spawn_postgres::PostgresStore;

const TOKEN: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
const PASSWORD: &str = "PGPASSWORD=s3cretvalue";

fn last_record(client: &mut Client, kind: &str) -> Value {
    client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = $1
              order by id desc limit 1",
            &[&kind],
        )
        .expect("the record")
        .get(0)
}

fn clean(text: &str) {
    assert!(text.contains("[redacted]"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
    assert!(!text.contains("s3cretvalue"), "{text}");
}

/// A gated mission's task, reported, waiting for its gate.
fn reported(store: &PostgresStore) -> i64 {
    let mission = store
        .mission_add("Masked", &json!(["/tmp/masked.git"]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    task
}

#[test]
fn a_rejections_free_text_is_stored_masked_and_its_keys_as_written() {
    let (store, schema) = store("control_mask_rejection");
    let mut client = admin(&schema);
    let task = reported(&store);
    let (base, candidate) = ("b".repeat(40), "c".repeat(40));
    store
        .integrator_record(&json!({
            "type": "task.gated", "id": task, "revision": 0, "verdict": "rejected",
            "base": base, "candidate": candidate, "judging": "0123456789abcdef0123456789abcdef",
            "log": format!("| curl -H 'Authorization: Bearer {TOKEN}'\n| {PASSWORD} psql\n\
                            | stage gate rejected 400ms regression: 092.sh must still pass"),
            "note": format!("092.sh printed {TOKEN}"),
            "reason": format!("regression: 092.sh must still pass ({PASSWORD})"),
            "failed": [format!("092.sh: {PASSWORD}")],
        }))
        .expect("recorded");
    let record = last_record(&mut client, "task.gated");
    for field in ["log", "note", "reason"] {
        clean(record[field].as_str().expect(field));
    }
    clean(record["failed"][0].as_str().expect("failed"));
    assert!(
        record["log"]
            .as_str()
            .expect("log")
            .contains("| stage gate rejected 400ms regression: 092.sh must still pass")
    );
    assert_eq!(record["type"], "task.gated");
    assert_eq!(record["id"], task);
    assert_eq!(record["revision"], 0);
    assert_eq!(record["verdict"], "rejected");
    assert_eq!(record["base"], base.as_str());
    assert_eq!(record["candidate"], candidate.as_str());
    assert_eq!(record["judging"], "0123456789abcdef0123456789abcdef");
    let row = client
        .query_one(
            "select state, review_note, gate_log from mission_tasks where id = $1",
            &[&task],
        )
        .expect("the task");
    let (state, note, log): (String, Option<String>, Option<String>) =
        (row.get(0), row.get(1), row.get(2));
    assert_eq!(state, "revise");
    clean(&note.expect("the review note"));
    if let Some(log) = log {
        assert!(
            !log.contains("s3cretvalue") && !log.contains(TOKEN),
            "{log}"
        );
    }
}

#[test]
fn a_host_faults_fact_keeps_what_the_cap_reads_and_its_reason_is_masked() {
    let (store, schema) = store("control_mask_host_fault");
    let mut client = admin(&schema);
    let task = reported(&store);
    store
        .integrator_record(&json!({
            "type": "task.gated", "id": task, "revision": 0, "verdict": "host_fault",
            "base": "b0", "scenarios": 0,
            "reason": "host fault: postgres://spawn:hunter2pass@db/spawn refused",
            "evidence": "sandbox_never_started", "backoff_ms": 300_000, "host_faults": 1,
            "escalate": false,
            "fact": {"on": "sandbox", "scenario": "092.sh", "runs": "never started",
                     "tree": "candidate", "reason": format!("the sandbox printed {TOKEN}")},
        }))
        .expect("recorded");
    let record = last_record(&mut client, "task.gated");
    assert_eq!(record["fact"]["on"], "sandbox");
    assert_eq!(record["fact"]["scenario"], "092.sh");
    assert_eq!(record["fact"]["runs"], "never started");
    assert_eq!(record["fact"]["tree"], "candidate");
    let reason = record["reason"].as_str().expect("reason");
    assert!(
        reason.contains("postgres://spawn:"),
        "the user stays: {reason}"
    );
    assert!(!reason.contains("hunter2pass"), "{reason}");
    clean(
        record["fact"]["reason"]
            .as_str()
            .expect("the fact's reason"),
    );
}

#[test]
fn a_deploy_reason_at_its_limit_is_taken_masked_within_it() {
    let (store, schema) = store("control_mask_deploy");
    let mut client = admin(&schema);
    let tail = " PWD=x";
    let reason = format!("{}{tail}", "d".repeat(500 - tail.len()));
    store
        .deploy_record(&json!({"type": "deploy.finished", "ok": false,
                                "head": "a".repeat(40), "by": "landing", "reason": reason}))
        .expect("taken");
    let record = last_record(&mut client, "deploy.finished");
    let stored = record["reason"].as_str().expect("reason");
    assert_eq!(stored.chars().count(), 500, "{stored}");
    assert!(!stored.contains("PWD=x"), "{stored}");
    assert_eq!(record["head"], "a".repeat(40).as_str());
}

#[test]
fn an_alerts_text_and_evidence_are_masked_and_it_clears_by_its_key() {
    let (store, schema) = store("control_mask_alert");
    let mut client = admin(&schema);
    let key = "deploy_failed:landing:1";
    store
        .alert_record(
            &json!({"type": "alert.fired", "key": key, "kind": "deploy_failed",
                               "subject": "deploy",
                               "text": format!("deploy failed: {PASSWORD}"),
                               "evidence": {"output": format!("Bearer {TOKEN}")},
                               "notice": false}),
        )
        .expect("fired");
    let record = last_record(&mut client, "alert.fired");
    clean(record["text"].as_str().expect("text"));
    clean(record["evidence"]["output"].as_str().expect("evidence"));
    assert_eq!(record["key"], key);
    store
        .alert_record(
            &json!({"type": "alert.cleared", "key": key, "kind": "deploy_failed",
                               "subject": "deploy"}),
        )
        .expect("cleared by the same key");
    let state: String = client
        .query_one("select state from spawn_alerts where key = $1", &[&key])
        .expect("the alert")
        .get(0);
    assert_ne!(state, "open");
}

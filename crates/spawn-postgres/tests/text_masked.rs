//! Free text the SQL functions write into control records, stored masked
//! (objective 171), through the real store: a mission's text, a brief, a
//! report, a review's note, a hold's, a pause's and a restart's reason, a
//! charter and a closing summary are masked before the function builds its
//! record, in the record and in the rows it folds into; a reason within the
//! store's limit stays within it.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, store};

const TOKEN: &str = "ghp_171madeup0123456789abcdefghijklmnopq";
const PASSWORD: &str = "hunter171hunter171";

fn secret() -> String {
    format!("curl -H 'Authorization: Bearer {TOKEN}' and PGPASSWORD={PASSWORD} psql")
}

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

fn clean(what: &str, text: &str) {
    assert!(text.contains("[redacted]"), "{what}: {text}");
    assert!(
        !text.contains(TOKEN) && !text.contains(PASSWORD),
        "{what}: {text}"
    );
}

fn text_of(client: &mut Client, sql: &str, id: i64) -> String {
    client
        .query_one(sql, &[&id])
        .expect("the row")
        .get::<_, Option<String>>(0)
        .unwrap_or_default()
}

#[test]
fn a_missions_text_a_brief_a_report_and_a_review_note_are_stored_masked() {
    let (store, schema) = store("text_masked_mission");
    let mut client = admin(&schema);
    let mission = store
        .mission_add(&format!("171 mission: {}", secret()), &json!([]), "human")
        .expect("add");
    clean(
        "mission.added's text",
        last_record(&mut client, "mission.added")["text"]
            .as_str()
            .expect("text"),
    );
    clean(
        "the mission's text",
        &text_of(
            &mut client,
            "select text from mission_missions where id = $1",
            mission,
        ),
    );

    let task = store
        .task_assign(
            mission,
            "ephemeral",
            "w",
            &format!("171 brief: {}", secret()),
            "default",
        )
        .expect("assign");
    clean(
        "task.assigned's brief",
        last_record(&mut client, "task.assigned")["brief"]
            .as_str()
            .expect("brief"),
    );
    clean(
        "the task's brief",
        &text_of(
            &mut client,
            "select brief from mission_tasks where id = $1",
            task,
        ),
    );

    store.task_delivered(task, 1).expect("delivered");
    assert!(
        store
            .task_reported(task, &format!("## Outcome\ndone: {}", secret()))
            .expect("reported")
    );
    clean(
        "task.reported's report",
        last_record(&mut client, "task.reported")["report"]
            .as_str()
            .expect("report"),
    );
    clean(
        "the task's report",
        &text_of(
            &mut client,
            "select report from mission_tasks where id = $1",
            task,
        ),
    );

    store
        .task_review(task, false, &format!("redo: {}", secret()), None)
        .expect("review");
    clean(
        "task.reviewed's note",
        last_record(&mut client, "task.reviewed")["note"]
            .as_str()
            .expect("note"),
    );
    clean(
        "the task's review note",
        &text_of(
            &mut client,
            "select review_note from mission_tasks where id = $1",
            task,
        ),
    );
    assert_eq!(
        text_of(
            &mut client,
            "select state from mission_tasks where id = $1",
            task
        ),
        "revise"
    );
}

#[test]
fn a_holds_and_a_pauses_reason_at_its_limit_is_taken_masked_within_it() {
    let (store, schema) = store("text_masked_reasons");
    let mut client = admin(&schema);
    // 494 é and " PWD=x": 500 chars, which masking lengthens.
    let reason = format!("{} PWD=x", "é".repeat(494));
    assert_eq!(reason.chars().count(), 500);
    let mission = store.mission_add("Paused", &json!([]), "human").expect("add");
    store
        .mission_pause(mission, Some(&reason))
        .expect("a 500-char reason is taken");
    let paused = last_record(&mut client, "mission.paused")["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(paused.chars().count() <= 500, "{}", paused.chars().count());
    assert!(!paused.contains("PWD=x"), "{paused}");
    assert!(paused.starts_with("éé"), "{paused}");

    let source = "/tmp/text-masked.git";
    let lock = store.source_hold_lock(source).expect("the landing lock");
    store
        .source_hold(source, &"a".repeat(40), 600, Some(&reason))
        .expect("a 500-char reason is taken");
    drop(lock);
    let held = last_record(&mut client, "source.held")["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(held.chars().count() <= 500, "{}", held.chars().count());
    assert!(!held.contains("PWD=x"), "{held}");

    // A credential in a short reason is masked whole.
    let other = store.mission_add("Paused 2", &json!([]), "human").expect("add");
    store
        .mission_pause(other, Some(&format!("hold: {}", secret())))
        .expect("pause");
    clean(
        "mission.paused's reason",
        last_record(&mut client, "mission.paused")["reason"]
            .as_str()
            .expect("reason"),
    );
}

#[test]
fn a_charter_and_a_closing_summary_are_stored_masked() {
    let (store, schema) = store("text_masked_charter");
    let mut client = admin(&schema);
    store
        .resident_propose("core", &format!("Owns it. {}", secret()))
        .expect("propose");
    clean(
        "resident.proposed's charter",
        last_record(&mut client, "resident.proposed")["charter"]
            .as_str()
            .expect("charter"),
    );
    let charter: String = client
        .query_one(
            "select charter from mission_residents where name = 'core'",
            &[],
        )
        .expect("the resident")
        .get(0);
    clean("the resident's charter", &charter);

    let mission = store.mission_add("Closed", &json!([]), "human").expect("add");
    store
        .mission_close(mission, "done", &format!("over: {}", secret()))
        .expect("close");
    clean(
        "mission.closed's summary",
        last_record(&mut client, "mission.closed")["summary"]
            .as_str()
            .expect("summary"),
    );
}

/// A restart's reason is limited in bytes: 997 é and " PWD=x" is 2000
/// bytes, which masking lengthens; it is taken and stored within 2000
/// bytes. A count in chars would let it grow past the store's limit.
#[test]
fn a_restart_reason_at_its_byte_limit_stays_within_it() {
    let (store, schema) = store("text_masked_restart");
    let mut client = admin(&schema);
    let mission = store.mission_add("Work", &json!([]), "human").expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "restarted", "Do it.", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    let reason = format!("{} PWD=x", "é".repeat(997));
    assert_eq!(reason.len(), 2000);
    store
        .task_restart(task, Some(&reason))
        .expect("a 2000-byte reason is taken");
    let stored = last_record(&mut client, "task.restarted")["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(stored.len() <= 2000, "{}", stored.len());
    assert!(!stored.contains("PWD=x"), "{stored}");
}

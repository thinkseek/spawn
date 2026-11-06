//! TEMPORARY check (deleted after the run): every read `GET /api/attention`
//! makes, run as the `spawn_reader` login that `spawn serve` uses.

use std::time::Duration;

use serde_json::json;
use spawn_core::command::{CommandAction, Outcome, Submission};
use spawn_core::event::{EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::{CommandQueue, EventLog};
use spawn_postgres::{PostgresStore, role_url};

mod common;
use common::{schema_url, store};

#[test]
fn the_reader_runs_every_read_of_the_attention_view() {
    let (store, schema) = store("attention_reader");
    let mission = store
        .mission_add("Ship it", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "w1", "do it", "default")
        .expect("assign");
    let supervisor = SessionId::new("session_supervisor").expect("valid");
    store
        .append(NewEvent {
            agent_id: AgentId::new("agent_supervisor").expect("valid"),
            session_id: supervisor.clone(),
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some("turn:1".to_string()),
            kind: EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
        })
        .expect("append");
    store
        .submit(
            &Principal::new("spawn-dispatch").expect("valid"),
            &Submission {
                agent_id: AgentId::new("agent_supervisor").expect("valid"),
                session_id: supervisor,
                idempotency_key: IdempotencyKey::new("digest-1").expect("valid"),
                action: CommandAction::Input {
                    text: "Spawn dispatcher: 1 update for you.".to_string(),
                    tags: vec!["host disk-1".to_string()],
                },
            },
        )
        .expect("submit");
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim")
        .expect("waits");
    store
        .finish(&claimed, &Outcome::Succeeded)
        .expect("finish");
    store
        .host_record(&json!({
            "type": "host.disk", "label": "tmpdir", "path": "/var/tmp", "state": "low",
            "free_bytes": 1_u64 << 30, "warn_bytes": 20_u64 << 30, "floor_bytes": 2_u64 << 30,
        }))
        .expect("disk record");

    let url = role_url(&schema_url(&schema), "spawn_reader").expect("the reader's URL");
    let reader = PostgresStore::connect_with_statement_timeout(&url, Duration::from_secs(10))
        .expect("the reader connects");
    let facts = reader.dispatch_facts().expect("dispatch_facts");
    assert!(
        facts
            .as_array()
            .expect("rows")
            .iter()
            .any(|row| row["session_id"] == "session_supervisor"
                && row["told"] == json!(["host disk-1"])),
        "{facts:#}"
    );
    let rows = reader.delivery_rows().expect("delivery_rows");
    assert!(
        rows.as_array()
            .expect("rows")
            .iter()
            .any(|row| row["id"] == task),
        "{rows:#}"
    );
    reader.mission_news().expect("mission_news");
    reader.retired_residents().expect("retired_residents");
    reader.cancelled_deliveries().expect("cancelled_deliveries");
    reader.stall_facts().expect("stall_facts");
    reader.gate_room_escalations().expect("gate_room_escalations");
    reader
        .gate_host_fault_escalations()
        .expect("gate_host_fault_escalations");
    let disk = reader.host_disk().expect("host_disk");
    assert_eq!(disk[0]["state"], "low", "{disk:#}");
    let told = reader
        .told_at("session_supervisor", &["host disk-1".to_string()])
        .expect("told_at");
    assert!(told["host disk-1"].is_i64(), "{told:#}");
    let turn = reader.latest_turn("session_supervisor").expect("latest_turn");
    assert!(turn["turn_ended_at_ms"].is_i64(), "{turn:#}");
    eprintln!("reader read every attention fact: told {told}, turn {turn}");
}

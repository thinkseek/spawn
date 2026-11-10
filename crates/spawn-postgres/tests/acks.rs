//! An agent's answer to a message by its id (objective 046): an ack is
//! recorded once, in the message's session, only for a message into the
//! agent whose workspace held it; a task's report answers its delivery; the
//! commands' message reads `acted`.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::{AckRecord, PostgresStore};

mod common;
use common::{admin, store};

/// One command row into `agent`'s session, finished; its id.
fn command(client: &mut Client, agent: &str, key: &str, action: &Value) -> i64 {
    let session = agent.replacen("agent_", "session_", 1);
    client
        .query_one(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, error, created_at_ms, finished_at_ms)
             values ('bb-person', $1, $2, $3, $4, 'finished', true, null,
                     spawn_now_ms(), spawn_now_ms())
             returning id",
            &[&key, &agent, &session, action],
        )
        .expect("a command")
        .get(0)
}

/// What the Bridge records when it types `command`'s marker.
fn typed(client: &mut Client, agent: &str, command: i64) {
    let session = agent.replacen("agent_", "session_", 1);
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             values ($1, $2, null, 'bridge', 'cmd_' || $3::bigint || ':message',
                     jsonb_build_object('name', 'message.typed',
                                        'detail', jsonb_build_object('command_id', $3::bigint)),
                     (select coalesce(max(session_seq), 0) + 1 from spawn_events
                       where session_id = $2),
                     spawn_now_ms())",
            &[&agent, &session, &command],
        )
        .expect("message.typed");
}

/// One control record, written as the stream writes it.
fn record(client: &mut Client, record: &Value) {
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             values ('agent_control', 'session_control', null, 'control', null,
                     jsonb_build_object('name', 'control.recorded',
                                        'detail', jsonb_build_object('record', $1::jsonb)),
                     (select coalesce(max(session_seq), 0) + 1 from spawn_events
                       where session_id = 'session_control'),
                     spawn_now_ms())",
            &[record],
        )
        .expect("a control record");
}

/// The message of `command` as `spawn_command_message` reads it now.
fn message(client: &mut Client, command: i64) -> Value {
    client
        .query_one(
            "select spawn_command_message(c.id, c.session_id, c.action, c.state, c.succeeded,
                                          c.created_at_ms, c.finished_at_ms, 120000, null,
                                          spawn_now_ms())
               from spawn_commands c where c.id = $1",
            &[&command],
        )
        .expect("the message reads")
        .get(0)
}

fn acted_count(client: &mut Client) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events where event_name = 'message.acted'",
            &[],
        )
        .expect("count")
        .get(0)
}

fn refused(answer: &AckRecord, says: &str) -> bool {
    matches!(answer, AckRecord::Refused(why) if why.contains(says))
}

#[test]
fn an_ack_is_recorded_once_and_only_for_the_panes_own_message() {
    let (store, schema): (PostgresStore, _) = store("acks_once");
    let mut client = admin(&schema);
    let input = json!({"type": "input", "text": "please"});
    let mine = command(&mut client, "agent_worker_a_1", "bb-1", &input);
    let theirs = command(&mut client, "agent_worker_b_2", "bb-2", &input);
    let key = command(
        &mut client,
        "agent_worker_a_1",
        "bb-3",
        &json!({"type": "key", "key": "enter"}),
    );
    let bare_start = command(&mut client, "agent_worker_a_1", "bb-4", &json!({"type": "start"}));
    let delivery = command(
        &mut client,
        "agent_worker_a_1",
        "deliver-9-0",
        &json!({"type": "start", "prompt": "Task nine"}),
    );
    let delivered = command(&mut client, "agent_worker_a_1", "bb-5", &input);
    record(
        &mut client,
        &json!({"type": "task.delivered", "id": 9, "command_id": delivered}),
    );

    assert_eq!(message(&mut client, mine)["acted"], Value::Null);
    let first = store
        .record_ack("agent_worker_a_1", mine, "done", None)
        .expect("the ack reads");
    assert_eq!(
        first,
        AckRecord::Recorded {
            session_id: "session_worker_a_1".into()
        }
    );
    let again = store
        .record_ack("agent_worker_a_1", mine, "declined", Some("changed my mind"))
        .expect("the second ack reads");
    assert_eq!(again, AckRecord::Answered, "the first ack stands");
    let read = message(&mut client, mine);
    assert_eq!(read["state"], "acted");
    assert_eq!(read["acted"], json!({"outcome": "done", "why": null}));
    assert!(read["acted_at_ms"].is_i64(), "{read}");

    // Each of these records nothing.
    for (agent, id, says) in [
        ("agent_worker_a_1", theirs, "not this pane's session"),
        ("agent_worker_a_1", 999_999, "no message"),
        ("agent_worker_a_1", key, "types nothing"),
        ("agent_worker_a_1", bare_start, "types nothing"),
        ("agent_worker_a_1", delivery, "report answers it"),
        ("agent_worker_a_1", delivered, "report answers it"),
    ] {
        let answer = store
            .record_ack(agent, id, "done", None)
            .expect("the ack reads");
        assert!(refused(&answer, says), "{id}: {answer:?}");
    }
    assert_eq!(acted_count(&mut client), 1);
    assert_eq!(message(&mut client, theirs)["acted"], Value::Null);

    // The other agent answers its own, declined with a why.
    assert!(matches!(
        store.record_ack("agent_worker_b_2", theirs, "declined", Some("busy elsewhere")),
        Ok(AckRecord::Recorded { .. })
    ));
    assert_eq!(
        message(&mut client, theirs)["acted"],
        json!({"outcome": "declined", "why": "busy elsewhere"})
    );
}

#[test]
fn a_report_answers_the_delivery_it_reports_on_once() {
    let (store, schema): (PostgresStore, _) = store("acks_reports");
    let mut client = admin(&schema);
    let start = json!({"type": "start", "prompt": "Task four"});
    let first = command(&mut client, "agent_worker_r_4", "deliver-4-0", &start);
    let second = command(&mut client, "agent_worker_r_4", "deliver-4-1", &start);
    // An older Spawn's delivery, typed with no marker: no message to answer.
    let old = command(&mut client, "agent_worker_o_5", "deliver-5-0", &start);
    typed(&mut client, "agent_worker_r_4", first);
    typed(&mut client, "agent_worker_r_4", second);
    for (task, delivery) in [(4, first), (5, old)] {
        record(
            &mut client,
            &json!({"type": "task.delivered", "id": task, "command_id": delivery}),
        );
    }
    record(
        &mut client,
        &json!({"type": "task.reported", "id": 4, "report": "## Outcome\nblocked: no data"}),
    );
    record(
        &mut client,
        &json!({"type": "task.reported", "id": 5, "report": "## Outcome\ndone"}),
    );
    // A send-back, delivered again, and its report.
    record(
        &mut client,
        &json!({"type": "task.delivered", "id": 4, "command_id": second}),
    );
    record(
        &mut client,
        &json!({"type": "task.reported", "id": 4, "report": "## Outcome\ndone"}),
    );

    let pending = store.report_answers(0).expect("the reports read");
    let named: Vec<(i64, i64)> = pending
        .iter()
        .map(|answer| (answer.task, answer.command_id))
        .collect();
    assert_eq!(
        named,
        vec![(4, first), (4, second)],
        "each report answers the delivery before it; an untracked one none"
    );
    assert!(pending[0].report.contains("blocked"));
    assert!(matches!(
        store.record_report_answer(first, "declined", Some("blocked: no data")),
        Ok(AckRecord::Recorded { .. })
    ));
    assert!(matches!(
        store.record_report_answer(second, "done", None),
        Ok(AckRecord::Recorded { .. })
    ));
    assert_eq!(
        store
            .record_report_answer(second, "done", None)
            .expect("the answer reads"),
        AckRecord::Answered
    );
    assert!(
        store
            .report_answers(0)
            .expect("the reports read")
            .is_empty(),
        "nothing is answered twice"
    );
    assert_eq!(
        message(&mut client, first)["acted"],
        json!({"outcome": "declined", "why": "blocked: no data"})
    );
    assert_eq!(message(&mut client, second)["state"], "acted");
}

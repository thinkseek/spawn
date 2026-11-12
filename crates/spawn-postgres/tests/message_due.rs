//! When a message is due to be taken (objective 045): an input waits for its
//! turn, so its bound runs from the session's last busy screen or turn end
//! after its creation and before the Bridge began typing it, never while a
//! turn it waits behind runs; one sent to a ready session runs from its
//! creation, and its clock stops moving once the Bridge began typing it.
//!
//! Every time is given (`p_now`), so no case waits on the clock. A real
//! database is required (`SPAWN_TEST_DATABASE_URL`); every case runs in a
//! schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, store};

const AGENT: &str = "agent_worker_due_1";
const SESSION: &str = "session_worker_due_1";
/// The bound, SPAWN_MESSAGE_TAKEN_SECS at its default (120 s).
const BOUND: i64 = 120_000;
/// A fixed origin: every record is this plus some seconds.
const T: i64 = 1_790_000_000_000;

fn s(seconds: i64) -> i64 {
    T + seconds * 1000
}

/// One input command into the session, created at `at`: unfinished (it
/// waits for its turn) when `finished` is None.
fn input(client: &mut Client, key: &str, at: i64, finished: Option<i64>) -> i64 {
    let state = if finished.is_some() {
        "finished"
    } else {
        "pending"
    };
    let succeeded = finished.map(|_| true);
    client
        .query_one(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, error, created_at_ms, finished_at_ms)
             values ('bb-person', $1, $2, $3, '{\"type\": \"input\", \"text\": \"please\"}',
                     $4, $5, null, $6, $7)
             returning id",
            &[&key, &AGENT, &SESSION, &state, &succeeded, &at, &finished],
        )
        .expect("an input command")
        .get(0)
}

/// One event of the session, as its writer records it, at `at`.
fn event(
    client: &mut Client,
    source: &str,
    key: Option<&str>,
    name: &str,
    detail: &Value,
    at: i64,
) {
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             values ($1, $2, 'exec_1', $3, $4,
                     jsonb_build_object('name', $5::text, 'detail', $6::jsonb),
                     (select coalesce(max(session_seq), 0) + 1 from spawn_events
                       where session_id = $2),
                     $7)",
            &[&AGENT, &SESSION, &source, &key, &name, detail, &at],
        )
        .expect("an event");
}

/// The engine's transcript held the marker of an earlier message (its own
/// command, sent a second before): the session's messages are tracked from
/// here on.
fn marker_seen(client: &mut Client, at: i64) {
    let earlier = input(client, "bb-earlier", at - 1000, Some(at - 1000));
    event(
        client,
        "claude",
        None,
        "message.taken",
        &json!({"command_id": earlier}),
        at,
    );
}

fn screen(client: &mut Client, state: &str, at: i64) {
    let evidence = if state == "busy" {
        "Channeling…"
    } else {
        "❯"
    };
    event(
        client,
        "bridge",
        None,
        "screen.observed",
        &json!({"state": state, "evidence": evidence}),
        at,
    );
}

fn turn_stopped(client: &mut Client, at: i64) {
    event(
        client,
        "claude",
        None,
        "turn.stopped",
        &json!({"input_tokens": null, "output_tokens": null}),
        at,
    );
}

/// What the Bridge records when it types `command`'s marked text: the
/// intent, the marked text, the completion.
fn typed(client: &mut Client, command: i64, at: i64) {
    event(
        client,
        "bridge",
        Some(format!("cmd_{command}:typing").as_str()),
        "command.step",
        &json!({"command_id": command, "step": "typing"}),
        at,
    );
    event(
        client,
        "bridge",
        Some(format!("cmd_{command}:message").as_str()),
        "message.typed",
        &json!({"command_id": command}),
        at,
    );
    event(
        client,
        "bridge",
        Some(format!("cmd_{command}:typed").as_str()),
        "command.step",
        &json!({"command_id": command, "step": "typed"}),
        at,
    );
}

/// The message of `command` as `spawn_command_message` reads it at `now`,
/// the first marking loop having started at T.
fn message(client: &mut Client, command: i64, now: i64) -> Value {
    client
        .query_one(
            "select spawn_command_message(c.id, c.session_id, c.action, c.state, c.succeeded,
                                          c.created_at_ms, c.finished_at_ms, $2, $3, $4)
               from spawn_commands c where c.id = $1",
            &[&command, &BOUND, &T, &now],
        )
        .expect("the message reads")
        .get(0)
}

fn not_taken(client: &mut Client, command: i64, now: i64) -> (Value, Value) {
    let read = message(client, command, now);
    (read["not_taken"].clone(), read["due_ms"].clone())
}

/// A turn runs when the input is sent and goes on past the bound: the input
/// waits, never typed, and is not due while the busy screen is still
/// recorded every 30 s.
#[test]
fn an_input_held_behind_a_running_turn_is_not_due_while_the_turn_runs() {
    let (_store, schema): (PostgresStore, _) = store("due_running_turn");
    let mut client = admin(&schema);
    marker_seen(&mut client, s(1));
    screen(&mut client, "busy", s(10));
    let held = input(&mut client, "bb-held", s(20), None);
    for at in [40, 70, 100, 130, 160] {
        screen(&mut client, "busy", s(at));
    }
    // 150 s after it was sent, past the bound, its turn still running.
    assert_eq!(
        not_taken(&mut client, held, s(170)),
        (json!(false), json!(s(160) + BOUND)),
        "an input waiting behind a running turn is not taken late while the turn runs: {}",
        message(&mut client, held, s(170))
    );
}

/// The turn it waited behind ends and the input is still not taken: it is
/// due one bound after that end, not one bound after it was sent.
#[test]
fn a_held_input_is_due_one_bound_after_the_turn_it_waited_behind_ends() {
    let (_store, schema): (PostgresStore, _) = store("due_after_turn_end");
    let mut client = admin(&schema);
    marker_seen(&mut client, s(1));
    turn_stopped(&mut client, s(5));
    let held = input(&mut client, "bb-held", s(20), None);
    turn_stopped(&mut client, s(150));
    screen(&mut client, "ready", s(151));
    assert_eq!(
        not_taken(&mut client, held, s(269)),
        (json!(false), json!(s(150) + BOUND)),
        "one bound has not passed since the turn it waited behind ended: {}",
        message(&mut client, held, s(269))
    );
    assert_eq!(
        not_taken(&mut client, held, s(270)),
        (json!(true), json!(s(150) + BOUND)),
        "one bound after the turn it waited behind ended, it is not taken: {}",
        message(&mut client, held, s(270))
    );
}

/// An input sent to a ready session (its last turn ended before), typed at
/// once and never taken: due one bound after it was sent.
#[test]
fn an_input_to_a_ready_session_is_due_one_bound_after_it_was_sent() {
    let (_store, schema): (PostgresStore, _) = store("due_ready_session");
    let mut client = admin(&schema);
    marker_seen(&mut client, s(1));
    turn_stopped(&mut client, s(5));
    screen(&mut client, "ready", s(6));
    let sent = input(&mut client, "bb-sent", s(20), Some(s(21)));
    typed(&mut client, sent, s(21));
    assert_eq!(
        not_taken(&mut client, sent, s(139)),
        (json!(false), json!(s(20) + BOUND)),
        "{}",
        message(&mut client, sent, s(139))
    );
    assert_eq!(
        not_taken(&mut client, sent, s(140)),
        (json!(true), json!(s(20) + BOUND)),
        "an input typed into a ready session is due one bound after it was sent: {}",
        message(&mut client, sent, s(140))
    );
}

/// Once the Bridge began typing a message, a later turn of the session (one
/// that did not take it) moves its bound no more.
#[test]
fn a_turn_after_the_typing_does_not_move_the_bound() {
    let (_store, schema): (PostgresStore, _) = store("due_after_typing");
    let mut client = admin(&schema);
    marker_seen(&mut client, s(1));
    let sent = input(&mut client, "bb-sent", s(20), Some(s(21)));
    typed(&mut client, sent, s(21));
    screen(&mut client, "busy", s(60));
    turn_stopped(&mut client, s(90));
    assert_eq!(
        not_taken(&mut client, sent, s(140)),
        (json!(true), json!(s(20) + BOUND)),
        "a turn after the typing moved the bound of a message already typed: {}",
        message(&mut client, sent, s(140))
    );
}

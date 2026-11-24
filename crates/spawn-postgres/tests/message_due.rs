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
) -> i64 {
    client
        .query_one(
            "insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             values ($1, $2, 'exec_1', $3, $4,
                     jsonb_build_object('name', $5::text, 'detail', $6::jsonb),
                     (select coalesce(max(session_seq), 0) + 1 from spawn_events
                       where session_id = $2),
                     $7)
             returning id",
            &[&AGENT, &SESSION, &source, &key, &name, detail, &at],
        )
        .expect("an event")
        .get(0)
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

/// Every plan node that reads the transcript through its partial index.
fn claude_index_scans<'a>(plan: &'a Value, found: &mut Vec<&'a Value>) {
    if plan["Index Name"] == "spawn_events_claude_time" {
        found.push(plan);
    }
    for child in plan["Plans"].as_array().into_iter().flatten() {
        claude_index_scans(child, found);
    }
}

/// The real incremental read plans both transcript lookups (the open-alert
/// candidate and `read_since_typed`) as per-session time range scans.
fn assert_claude_plan(client: &mut Client, sent: i64, at: i64) {
    // Give the planner realistic selectivity: the open-alert lookup must seek
    // this session, not hash every session's recent transcript, and the count
    // must prefer the partial transcript index over every event of the
    // session. These unrelated rows also make a sequential scan unattractive.
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source,
                                       source_event_id, kind, session_seq, recorded_at_ms)
             select 'agent_fill', 'session_fill_' || g, 'exec_1',
                    case when g % 2 = 0 then 'claude' else 'bridge' end, null,
                    jsonb_build_object('name', 'transcript.line', 'detail', '{}'::jsonb),
                    n, $1::bigint - 100_000 + n
               from generate_series(1, 200) g cross join generate_series(1, 50) n",
            &[&at],
        )
        .expect("populate unrelated sessions");
    client
        .batch_execute("analyze spawn_events; analyze spawn_commands; set enable_seqscan = off")
        .expect("prefer an index in the plan");
    let explain = client
        .prepare(&format!(
            "explain (format json) {}",
            spawn_postgres::testing::ALERT_MESSAGES_SINCE
        ))
        .expect("prepare the real read's EXPLAIN");
    let after_commands = i64::MAX;
    let due: Vec<i64> = Vec::new();
    let open = vec![sent];
    let since = 0_i64;
    let plan: Value = client
        .query_one(&explain, &[&BOUND, &after_commands, &due, &open, &since])
        .expect("EXPLAIN the real read")
        .get(0);
    let mut scans = Vec::new();
    claude_index_scans(&plan[0]["Plan"], &mut scans);
    assert!(
        scans.len() >= 2,
        "the open-alert candidate and read_since_typed both use the partial index: {plan}"
    );
    for scan in scans {
        let condition = scan["Index Cond"].as_str().unwrap_or_default();
        assert!(
            condition.contains("session_id") && condition.contains("recorded_at_ms"),
            "a per-session time range, not a global or whole-session scan: {scan}"
        );
    }
}

/// A transcript event in the typing's own millisecond is ordered by its event
/// id: one inserted before `:typed` is not read since the typing, while one
/// inserted after it is. The count is present while the non-zero bound is
/// still pending, and the real query's plan uses the partial index.
#[test]
fn same_millisecond_transcript_events_use_event_id_order_and_the_partial_index() {
    let (store, schema): (PostgresStore, _) = store("messages_same_ms");
    let mut client = admin(&schema);
    let at = store.now_ms().expect("the clock");
    marker_seen(&mut client, at - 1_000);
    let sent = input(&mut client, "bb-same-ms", at, Some(at));
    event(
        &mut client,
        "bridge",
        Some(format!("cmd_{sent}:message").as_str()),
        "message.typed",
        &json!({"command_id": sent}),
        at,
    );
    event(
        &mut client,
        "bridge",
        Some(format!("cmd_{sent}:typing").as_str()),
        "command.step",
        &json!({"command_id": sent, "step": "typing"}),
        at,
    );
    let before = event(
        &mut client,
        "claude",
        Some("same-ms-before"),
        "transcript.line",
        &json!({"text": "before"}),
        at,
    );
    let typed_id = event(
        &mut client,
        "bridge",
        Some(format!("cmd_{sent}:typed").as_str()),
        "command.step",
        &json!({"command_id": sent, "step": "typed"}),
        at,
    );
    assert!(before < typed_id);

    let read = |store: &PostgresStore| {
        let reading = store
            .alert_messages_since(BOUND, 0, &[], &[sent], 0)
            .expect("the incremental reading");
        reading["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| row["command_id"] == json!(sent))
            .unwrap_or_else(|| panic!("the command's row: {reading}"))
            .clone()
    };
    let row = read(&store);
    assert_eq!(row["not_taken"], false, "still inside its bound: {row}");
    assert_eq!(
        row["read_since_typed"], 0,
        "the same-ms event before :typed is not after it: {row}"
    );

    let after = event(
        &mut client,
        "claude",
        Some("same-ms-after"),
        "transcript.line",
        &json!({"text": "after"}),
        at,
    );
    assert!(after > typed_id);
    let row = read(&store);
    assert_eq!(row["not_taken"], false, "still inside its bound: {row}");
    assert!(
        row["read_since_typed"].as_i64().is_some_and(|n| n >= 1),
        "the same-ms event after :typed moves the transcript: {row}"
    );

    assert_claude_plan(&mut client, sent, at);
}

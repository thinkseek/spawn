//! A person's intents (objective 049; `spawn_human_intents`): a command of
//! the board or the CLI sent again after a refusal continues the intent of
//! the one before; the experience views and the interventions window count
//! intents, not commands.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, as_login, store};

const M: i64 = 60_000;

/// How a command ended: taken, refused, or not finished.
#[derive(Clone, Copy)]
enum End {
    Ok,
    Failed,
    Pending,
}

/// One command `minutes` after 0, finished a second later unless
/// `finished` (minutes) says when; its id.
fn command(
    client: &mut Client,
    (principal, session, action): (&str, &str, &Value),
    minutes: i64,
    end: End,
    finished: Option<i64>,
) -> i64 {
    let key = format!(
        "k-{}",
        client
            .query_one("select count(*) from spawn_commands", &[])
            .expect("count")
            .get::<_, i64>(0)
    );
    let created = minutes * M;
    let finished_at: Option<i64> = match end {
        End::Pending => None,
        _ => Some(finished.map_or(created + 1000, |at| at * M)),
    };
    let succeeded: Option<bool> = match end {
        End::Ok => Some(true),
        End::Failed => Some(false),
        End::Pending => None,
    };
    let state = if matches!(end, End::Pending) {
        "pending"
    } else {
        "finished"
    };
    client
        .query_one(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, created_at_ms, finished_at_ms)
             values ($1, $2, replace($3, 'session_', 'agent_'), $3, $4, $5, $6, $7, $8)
             returning id",
            &[
                &principal,
                &key,
                &session,
                action,
                &state,
                &succeeded,
                &created,
                &finished_at,
            ],
        )
        .expect("write a command")
        .get(0)
}

fn input(text: &str) -> Value {
    json!({"type": "input", "text": text})
}

/// Every intent: (its commands, its outcome), by its first command.
fn intents(client: &mut Client) -> Vec<(Vec<i64>, String)> {
    client
        .query(
            "select commands, outcome from spawn_human_intents order by intent",
            &[],
        )
        .expect("the view reads")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

/// The intent of `a1`, the first: at 0, three attempts, the board's, its
/// agent's.
fn first_intent(client: &mut Client, a1: i64) {
    let first: (i64, i64, String, String) = client
        .query_one(
            "select at_ms, attempts, principal, agent_id from spawn_human_intents where intent = $1",
            &[&a1],
        )
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .expect("the first intent");
    assert_eq!(first, (0, 3, "human".to_string(), "agent_w1".to_string()));
}

/// The experience views count `intents` intents, one decision and one
/// intervention each, and the supervisor and the reader read the view.
fn views_count_intents(client: &mut Client, schema: &str, intents: i64) {
    // The experience views count intents: one decision and one intervention each.
    let count = |client: &mut Client, sql: &str| -> i64 {
        client.query_one(sql, &[]).expect("count").get(0)
    };
    assert_eq!(
        count(
            client,
            "select count(*) from spawn_interventions where kind = 'command'"
        ),
        intents
    );
    assert_eq!(
        count(
            client,
            "select count(*) from spawn_decisions where tier = 'human'"
        ),
        intents
    );
    assert_eq!(
        count(
            client,
            "select count(*) from spawn_decisions where tier = 'dispatcher'"
        ),
        1,
        "the dispatcher's own commands still count each"
    );
    // The supervisor and the reader read the view.
    for login in ["spawn_supervisor", "spawn_reader"] {
        let mut reader = as_login(schema, login);
        assert_eq!(
            count(&mut reader, "select count(*) from spawn_human_intents"),
            intents,
            "{login}"
        );
    }
}

#[test]
fn a_refused_command_sent_again_continues_its_intent() {
    let (_store, schema) = store("intents_rule");
    let mut client = admin(&schema);
    let note = input("note B");
    let w1 = ("human", "session_w1", &note);
    // Refused twice (a dispatcher's same text between, not a person's),
    // then taken: one intent.
    let a1 = command(&mut client, w1, 0, End::Failed, None);
    let dispatched = ("spawn-dispatch", "session_w1", &note);
    command(&mut client, dispatched, 1, End::Failed, None);
    let a2 = command(&mut client, w1, 2, End::Failed, None);
    let a3 = command(&mut client, w1, 3, End::Ok, None);
    // Sent again after it was taken: a new intent.
    let b = command(&mut client, w1, 4, End::Ok, None);
    // Another principal, another session, another action: new ones.
    let other_principal = command(
        &mut client,
        ("spawn", "session_w1", &note),
        5,
        End::Ok,
        None,
    );
    let other_session = command(
        &mut client,
        ("human", "session_w2", &note),
        6,
        End::Ok,
        None,
    );
    let stop = json!({"type": "stop"});
    let s1 = command(
        &mut client,
        ("human", "session_w1", &stop),
        7,
        End::Failed,
        None,
    );
    let s2 = command(
        &mut client,
        ("human", "session_w1", &stop),
        8,
        End::Ok,
        None,
    );
    // More than 60 minutes after the refusal: a new intent.
    let late = input("late");
    let l1 = command(
        &mut client,
        ("human", "session_w1", &late),
        10,
        End::Failed,
        None,
    );
    let l2 = command(
        &mut client,
        ("human", "session_w1", &late),
        71,
        End::Failed,
        None,
    );
    // Sent again before the refusal finished: a new intent.
    let slow = input("slow");
    let f1 = command(
        &mut client,
        ("human", "session_w1", &slow),
        100,
        End::Failed,
        Some(110),
    );
    let f2 = command(
        &mut client,
        ("human", "session_w1", &slow),
        105,
        End::Ok,
        None,
    );
    // Refused, then still pending: one intent, pending.
    let wait = input("wait");
    let p1 = command(
        &mut client,
        ("human", "session_w1", &wait),
        120,
        End::Failed,
        None,
    );
    let p2 = command(
        &mut client,
        ("human", "session_w1", &wait),
        121,
        End::Pending,
        None,
    );

    let taken = || "taken".to_string();
    let failed = || "failed".to_string();
    assert_eq!(
        intents(&mut client),
        vec![
            (vec![a1, a2, a3], taken()),
            (vec![b], taken()),
            (vec![other_principal], taken()),
            (vec![other_session], taken()),
            (vec![s1, s2], taken()),
            (vec![l1], failed()),
            (vec![l2], failed()),
            (vec![f1], failed()),
            (vec![f2], taken()),
            (vec![p1, p2], "pending".to_string()),
        ]
    );
    first_intent(&mut client, a1);

    views_count_intents(&mut client, &schema, 10);
}

#[test]
fn the_interventions_window_takes_each_intent_as_it_stood() {
    let (store, schema) = store("intents_window");
    let mut client = admin(&schema);
    let long = format!("first line {}\nsecond line", "y".repeat(200));
    let note = input(&long);
    let w = ("human", "session_w", &note);
    let c1 = command(&mut client, w, 10, End::Failed, None);
    let c2 = command(&mut client, w, 20, End::Failed, None);
    let c3 = command(&mut client, w, 30, End::Ok, None);
    let stop = json!({"type": "stop"});
    let s = command(
        &mut client,
        ("spawn", "session_v", &stop),
        40,
        End::Ok,
        None,
    );

    // Before c3 was created: the intent of c1 and c2, refused, not yet sent
    // again; its last refusal finished, so failed.
    let early = store
        .interventions_window(0, 25 * M)
        .expect("the window reads");
    let items = early["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "{early}");
    assert_eq!(items[0]["commands"], json!([c1, c2]));
    assert_eq!(items[0]["attempts"], 2);
    assert_eq!(items[0]["outcome"], "failed");
    let line = items[0]["line"].as_str().unwrap_or_default();
    assert!(line.starts_with("first line y"), "{line}");
    assert_eq!(
        line.chars().count(),
        120,
        "the first line, at most 120 characters"
    );
    // At c2's own creation plus half a second: c2 had not finished.
    let racing = store
        .interventions_window(0, 20 * M + 500)
        .expect("the window reads");
    assert_eq!(racing["items"][0]["outcome"], "pending");

    let all = store
        .interventions_window(0, 60 * M)
        .expect("the window reads");
    assert_eq!(
        all["items"],
        json!([
            {"kind": "command", "at_ms": 40 * M, "principal": "spawn", "agent": "agent_v",
             "session": "session_v", "line": "stop", "commands": [s], "attempts": 1,
             "outcome": "taken"},
            {"kind": "command", "at_ms": 10 * M, "principal": "human", "agent": "agent_w",
             "session": "session_w", "line": line, "commands": [c1, c2, c3], "attempts": 3,
             "outcome": "taken"},
        ])
    );
    // An intent begun before the window is not in it, whatever its retries.
    let later = store
        .interventions_window(15 * M, 60 * M)
        .expect("the window reads");
    assert_eq!(later["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(later["agent_ms"], 0);
}

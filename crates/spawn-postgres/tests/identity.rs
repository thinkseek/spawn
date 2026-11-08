//! One session per agent, one agent per session (`spawn_agent_sessions`):
//! every append and submission holds it, also when two first writes race.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`).

use std::time::Duration;

use postgres::Client;
use postgres::error::SqlState;

mod common;
use common::{admin, first_shapes, migrate_warnings, store};
use spawn_postgres::PostgresStore;

/// Run `first` in an open transaction on one connection, start `second` on
/// another, let it wait on the first, then commit the first. Returns the
/// second's result. Both connect as `login` (a restricted login, logged in
/// as itself), or as the owner when `None`.
fn race(
    schema: &str,
    login: Option<&str>,
    first: &str,
    second: &str,
) -> Result<u64, postgres::Error> {
    let connect = |schema: &str, login: Option<&str>| match login {
        Some(base) => common::as_login(schema, base),
        None => admin(schema),
    };
    let mut one = connect(schema, login);
    let mut transaction = one.transaction().expect("begin");
    transaction.execute(first, &[]).expect("the first write");
    let schema = schema.to_string();
    let login = login.map(str::to_string);
    let second = second.to_string();
    let other = std::thread::spawn(move || {
        let mut two = match login.as_deref() {
            Some(base) => common::as_login(&schema, base),
            None => admin(&schema),
        };
        two.execute(&second, &[])
    });
    // The second is past its own checks and waits on the first's lock.
    std::thread::sleep(Duration::from_millis(300));
    assert!(!other.is_finished(), "the second write waits for the first");
    transaction.commit().expect("commit the first");
    other.join().expect("the second write returns")
}

fn pairs(client: &mut Client, column: &str, value: &str) -> Vec<(String, String)> {
    client
        .query(
            &format!("select agent_id, session_id from spawn_commands where {column} = $1"),
            &[&value],
        )
        .expect("read the commands")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

#[test]
fn two_first_submissions_for_one_session_with_different_agents_cannot_both_win() {
    let (_store, schema) = store("identity_session_race");
    // Through the gateway, as the supervisor, whose grants cover both.
    let insert = |agent: &str| {
        format!(
            "insert into agent_commands (agent_id, session_id, idempotency_key, action)
             values ('{agent}', 'session_worker_shared', 'key_{agent}', '{{\"type\":\"stop\"}}')"
        )
    };
    let second = race(
        &schema,
        Some("spawn_supervisor"),
        &insert("agent_worker_a"),
        &insert("agent_worker_b"),
    );
    let error = second.expect_err("the second agent is refused");
    assert_eq!(
        error.code(),
        Some(&SqlState::INSUFFICIENT_PRIVILEGE),
        "{error}"
    );
    let mut client = admin(&schema);
    assert_eq!(
        pairs(&mut client, "session_id", "session_worker_shared"),
        [(
            "agent_worker_a".to_string(),
            "session_worker_shared".to_string()
        )],
        "exactly the first agent's command is queued"
    );
}

#[test]
fn two_first_submissions_for_one_agent_with_different_sessions_cannot_both_win() {
    let (_store, schema) = store("identity_agent_race");
    // The owner's path, `spawn_submit`, holds it too.
    let submit = |session: &str| {
        format!(
            "select spawn_submit('human', 'key_{session}', 'agent_one', '{session}',
                                 '{{\"type\":\"stop\"}}')"
        )
    };
    let second = race(&schema, None, &submit("session_x"), &submit("session_y"));
    let error = second.expect_err("the second session is refused");
    assert_eq!(
        error.code(),
        Some(&SqlState::INSUFFICIENT_PRIVILEGE),
        "{error}"
    );
    let message = error.as_db_error().map(|db| db.message().to_string());
    assert_eq!(
        message.as_deref(),
        Some("agent agent_one has session session_x, not session_y")
    );
    let mut client = admin(&schema);
    assert_eq!(
        pairs(&mut client, "agent_id", "agent_one"),
        [("agent_one".to_string(), "session_x".to_string())]
    );
}

#[test]
fn a_migration_over_an_existing_log_records_its_pairs_and_reports_broken_ones() {
    let (schema, mut client) = first_shapes("identity_backfill");
    client
        .batch_execute(
            "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
             values ('agent_one', 'session_one', 'claude', '{\"name\": \"message.received\"}', 1, 1),
                    ('agent_two', 'session_two', 'claude', '{\"name\": \"message.received\"}', 1, 2),
                    ('agent_one', 'session_one', 'claude', '{\"name\": \"message.received\"}', 2, 3),
                    ('agent_one', 'session_three', 'claude', '{\"name\": \"message.received\"}', 1, 4),
                    ('agent_four', 'session_two', 'claude', '{\"name\": \"message.received\"}', 2, 5);",
        )
        .expect("a log from before the rule");
    let warnings = migrate_warnings(&schema);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("2 agents and sessions")
            && warnings[0].contains("(agent_one, session_three)")
            && warnings[0].contains("(agent_four, session_two)"),
        "{warnings:?}"
    );
    let recorded: Vec<(String, String)> = client
        .query(
            "select agent_id, session_id from spawn_agent_sessions order by agent_id",
            &[],
        )
        .expect("the pairs")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(
        recorded,
        [
            ("agent_one".to_string(), "session_one".to_string()),
            ("agent_two".to_string(), "session_two".to_string()),
        ],
        "the first pair of each agent and session stands"
    );
    let count: i64 = client
        .query_one("select count(*) from spawn_events", &[])
        .expect("the log")
        .get(0);
    assert_eq!(count, 5, "the log is not changed");

    // Writes go on for the pairs that stand, and stop for the broken ones.
    let store = PostgresStore::connect_in_schema(&common::url(), &schema).expect("migrate again");
    drop(store);
    let append = |client: &mut Client, agent: &str, session: &str| {
        client.execute(
            "select spawn_append($1, $2, null, 'claude', null, '{\"name\": \"message.received\"}')",
            &[&agent, &session],
        )
    };
    append(&mut client, "agent_one", "session_one").expect("the standing pair");
    append(&mut client, "agent_five", "session_five").expect("a new pair");
    for (agent, session) in [
        ("agent_one", "session_three"),
        ("agent_four", "session_two"),
        ("agent_five", "session_one"),
    ] {
        let error = append(&mut client, agent, session).expect_err("a broken pair");
        assert_eq!(
            error.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{error}"
        );
    }
}

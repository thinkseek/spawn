//! A person's input from the web page (`spawn serve`, `POST
//! /api/session/<id>/input`): the `spawn_human` login calls
//! `spawn_human_input`, which makes the command `spawn submit --principal
//! human --action '{"type":"input",...}'` makes, to the session's own agent,
//! with the same idempotency rule. That login can do nothing else.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use std::time::Duration;

use postgres::error::SqlState;
use spawn_core::command::{CommandAction, NamedKey, Submission};
use spawn_core::event::{CommandStep, ENGINE, EventKind, ModelProfile, NewEvent, Source};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, WorkspaceId,
};
use spawn_core::port::{CommandQueue, EventLog};
use spawn_postgres::{HumanInput, HumanKey, PostgresStore, role_url};

mod common;
use common::{admin, schema_url, store};

const SESSION: &str = "session_supervisor";
const AGENT: &str = "agent_supervisor";

fn session() -> SessionId {
    SessionId::new(SESSION).expect("valid session")
}

fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text).expect("valid key")
}

/// The session's first event, which pairs it with its agent.
fn seen(store: &PostgresStore) {
    store
        .append(NewEvent {
            agent_id: AgentId::new(AGENT).expect("valid agent"),
            session_id: session(),
            execution_id: None,
            source: Source::Claude,
            source_event_id: Some("message-1".to_string()),
            kind: EventKind::MessageReceived,
        })
        .expect("append");
}

/// A store connected as this database's `spawn_human` login, in `schema`.
fn human(schema: &str) -> PostgresStore {
    let url = role_url(&schema_url(schema), "spawn_human").expect("the human login's URL");
    assert!(
        url.contains("://spawn_human_"),
        "the database's own login: {}",
        url.split_once('@')
            .map_or("", |(user, _)| user.split(':').next().unwrap_or(""))
    );
    PostgresStore::connect(&url).expect("the human login connects")
}

/// `spawn_commands` rows of the case's schema: principal, key, agent,
/// session, action, state.
fn commands(schema: &str) -> Vec<(String, String, String, String, serde_json::Value, String)> {
    admin(schema)
        .query(
            "select principal, idempotency_key, agent_id, session_id, action, state
               from spawn_commands order by id",
            &[],
        )
        .expect("the commands")
        .iter()
        .map(|row| {
            (
                row.get(0),
                row.get(1),
                row.get(2),
                row.get(3),
                row.get(4),
                row.get(5),
            )
        })
        .collect()
}

#[test]
fn a_persons_input_from_the_page_is_the_command_spawn_submit_makes() {
    let (store, schema) = store("human_input");
    seen(&store);
    let page = human(&schema);
    let text = "Stop objective_87, then report.\nThanks";
    let HumanInput::Submitted(id) = page
        .submit_human_input(&session(), &key("web-1"), text)
        .expect("typed")
    else {
        panic!("the session has an agent");
    };

    let action = CommandAction::Input {
        text: text.to_string(),
        tags: Vec::new(),
    };
    assert_eq!(
        commands(&schema),
        [(
            "human".to_string(),
            "web-1".to_string(),
            AGENT.to_string(),
            SESSION.to_string(),
            serde_json::to_value(&action).expect("an action"),
            "pending".to_string(),
        )],
        "principal human, the session's own agent, an input action and nothing else"
    );

    // A retried POST types once: the same key gives the same command.
    assert_eq!(
        page.submit_human_input(&session(), &key("web-1"), text)
            .expect("retried"),
        HumanInput::Submitted(id)
    );
    // The same rule as `spawn submit --principal human --key web-1`: one
    // principal, one key, one command.
    let cli = store
        .submit(
            &Principal::new("human").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: session(),
                idempotency_key: key("web-1"),
                action,
            },
        )
        .expect("submit");
    assert_eq!(cli, id);
    assert_eq!(commands(&schema).len(), 1, "no second command");

    // A session no agent has used gets nothing.
    let unknown = SessionId::new("session_never_seen").expect("valid session");
    assert_eq!(
        page.submit_human_input(&unknown, &key("web-2"), "hello")
            .expect("answered"),
        HumanInput::NoAgent
    );
    assert_eq!(commands(&schema).len(), 1);

    // It is a person's input: the Bridge claims it with principal `human`,
    // so it waits for the session's turn (crates/spawn-core/src/bridge.rs).
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim")
        .expect("the input");
    assert_eq!(claimed.command_id, id);
    assert_eq!(claimed.principal, "human");

    // The session's commands say whether it waits for the turn.
    let shown = |store: &PostgresStore| store.commands_json(SESSION, 10).expect("commands");
    assert_eq!(shown(&store)[0]["waiting"], false);
    assert_eq!(shown(&store)[0]["principal"], "human");
    assert_eq!(shown(&store)[0]["text"], text);
    store
        .append(NewEvent {
            agent_id: AgentId::new(AGENT).expect("valid agent"),
            session_id: session(),
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(format!("cmd_{}:waiting", id.0)),
            kind: EventKind::CommandStepped {
                command_id: CommandId(id.0),
                step: CommandStep::Waiting,
                workspace: None,
            },
        })
        .expect("the Bridge's waiting step");
    assert_eq!(shown(&store)[0]["waiting"], true);
    assert_eq!(shown(&store)[0]["state"], "pending");
}

/// GitHub #16 item 8: a key one session's input used, sent for another
/// session, with other text or as a named key, is not that command again
/// (it was: `session_b` was answered with `session_supervisor`'s command 1
/// and got nothing). It is refused in the store's words, nothing is
/// recorded, and the first command is untouched; the request that made it
/// still gets it back, and the second session types under a key of its own.
#[test]
fn a_key_used_for_one_session_is_refused_for_another() {
    let (store, schema) = store("human_input_cross");
    seen(&store);
    let other = SessionId::new("session_b").expect("valid session");
    store
        .append(NewEvent {
            agent_id: AgentId::new("agent_b").expect("valid agent"),
            session_id: other.clone(),
            execution_id: Some(ExecutionId::new("exec_b").expect("valid execution")),
            source: Source::Bridge,
            source_event_id: Some("exec_b:started".to_string()),
            kind: EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("1:/workspaces/b").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
            },
        })
        .expect("append");
    let page = human(&schema);
    let first = page
        .submit_human_input(&session(), &key("shared-key"), "for A")
        .expect("typed");
    assert!(matches!(first, HumanInput::Submitted(_)), "{first:?}");
    let used = "idempotency key 'shared-key' was already used for another input or key: send \
                this one with a new key";
    assert_eq!(
        page.submit_human_input(&other, &key("shared-key"), "for B")
            .expect("answered"),
        HumanInput::KeyUsed(used.to_string()),
        "another session's key"
    );
    assert_eq!(
        page.submit_human_input(&session(), &key("shared-key"), "for A, changed")
            .expect("answered"),
        HumanInput::KeyUsed(used.to_string()),
        "the same session, other text"
    );
    assert_eq!(
        page.submit_human_key(&other, &key("shared-key"), NamedKey::Enter)
            .expect("answered"),
        HumanKey::KeyUsed(used.to_string()),
        "an input's key is no named key's"
    );
    let rows = commands(&schema);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].3, SESSION);
    assert_eq!(rows[0].4["text"], "for A");

    // The request that made it still gets its command, and a session that
    // was refused types under a key of its own.
    assert_eq!(
        page.submit_human_input(&session(), &key("shared-key"), "for A")
            .expect("retried"),
        first
    );
    let own = page
        .submit_human_input(&other, &key("key-b"), "for B")
        .expect("typed");
    assert!(matches!(own, HumanInput::Submitted(_)) && own != first);

    // A named key's idempotency key is its session's and its name's too.
    let HumanKey::Submitted(pressed) = page
        .submit_human_key(&other, &key("key-k"), NamedKey::Down)
        .expect("pressed")
    else {
        panic!("session_b runs an execution");
    };
    assert_eq!(
        page.submit_human_key(&other, &key("key-k"), NamedKey::Up)
            .expect("answered"),
        HumanKey::KeyUsed(used.replace("shared-key", "key-k")),
        "the same session, another name"
    );
    assert_eq!(
        page.submit_human_input(&session(), &key("key-k"), "down")
            .expect("answered"),
        HumanInput::KeyUsed(used.replace("shared-key", "key-k")),
        "a named key's key is no input's"
    );
    assert_eq!(
        page.submit_human_key(&other, &key("key-k"), NamedKey::Down)
            .expect("retried"),
        HumanKey::Submitted(pressed)
    );
    let rows = commands(&schema);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(
        (rows[1].3.as_str(), &rows[1].4["text"]),
        ("session_b", &serde_json::json!("for B"))
    );
}

/// What `spawn_human_input` refuses, whoever calls it: blank text, text
/// over 8192 bytes, a control character a terminal would read as a key, no
/// key; and every caller but the `spawn_human` login.
#[test]
fn only_the_human_login_types_and_only_what_a_person_types() {
    let (store, schema) = store("human_input_refused");
    seen(&store);
    let mut page = postgres::Client::connect(
        &role_url(&schema_url(&schema), "spawn_human").expect("the login's URL"),
        postgres::NoTls,
    )
    .expect("the human login connects");
    let call = |client: &mut postgres::Client, key: &str, text: &str| {
        client
            .query_one(
                "select spawn_human_input($1, $2, $3)",
                &[&SESSION, &key, &text],
            )
            .map(|row| row.get::<_, Option<i64>>(0))
    };
    let long = "a".repeat(8193);
    for (key, text, why) in [
        ("k1", "  \n\t ", "blank"),
        ("k2", long.as_str(), "over 8192 bytes"),
        (
            "k3",
            "before\u{1b}[201~after",
            "an escape: a terminal reads keys",
        ),
        ("k4", "a\u{7}b", "a bell"),
        ("k9", "a\u{9b}201~b", "a C1 control sequence introducer"),
        ("", "hello", "no key"),
    ] {
        let error = call(&mut page, key, text).expect_err(why);
        assert_eq!(
            error.code(),
            Some(&SqlState::INVALID_PARAMETER_VALUE),
            "{why}: {error}"
        );
    }
    assert!(
        call(&mut page, "k5", &"字".repeat(2730)).is_ok(),
        "8190 bytes of text, tabs and line ends are a person's"
    );
    assert!(call(&mut page, "k6", "one\ttwo\r\nthree").is_ok());
    let error = call(&mut admin(&schema), "k7", "hello").expect_err("the owner");
    assert_eq!(
        error.code(),
        Some(&SqlState::INSUFFICIENT_PRIVILEGE),
        "the owner submits with spawn_submit, not as the page"
    );
    let supervisor = role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL");
    let error = call(
        &mut postgres::Client::connect(&supervisor, postgres::NoTls).expect("connects"),
        "k8",
        "hello",
    )
    .expect_err("the supervisor");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(commands(&schema).len(), 2, "only the two a person may type");
}

#[test]
fn the_human_login_can_do_nothing_but_type() {
    let (store, schema) = store("human_login");
    seen(&store);
    store
        .submit(
            &Principal::new("human").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: session(),
                idempotency_key: key("cli-1"),
                action: CommandAction::Stop,
            },
        )
        .expect("a command it must not see");
    let mut page = postgres::Client::connect(
        &role_url(&schema_url(&schema), "spawn_human").expect("the login's URL"),
        postgres::NoTls,
    )
    .expect("the human login connects");
    let principal: Option<String> = page
        .query_one("select spawn_principal()", &[])
        .expect("its principal")
        .get(0);
    assert_eq!(principal.as_deref(), Some("human"));

    let refused = [
        (
            "select * from spawn_submit('human', 'k', 'agent_supervisor', 'session_supervisor', \
             '{\"type\":\"stop\"}')",
            "submit another action through spawn_submit",
        ),
        (
            "select spawn_gateway_submit('agent_supervisor', 'session_supervisor', 'k', \
             '{\"type\":\"stop\"}')",
            "submit through the gateway (no grant)",
        ),
        (
            "insert into agent_commands (agent_id, session_id, idempotency_key, action) \
             values ('agent_supervisor', 'session_supervisor', 'k', '{\"type\":\"input\",\"text\":\"x\"}')",
            "insert into the gateway (no grant)",
        ),
        (
            "select * from spawn_append('agent_x', 'session_x', null, 'claude', null, \
             '{\"name\":\"message.received\"}'::jsonb)",
            "append an event",
        ),
        ("select * from spawn_commands", "read the queue"),
        ("select * from spawn_events", "read the log"),
        ("select * from mission_tasks", "read the missions"),
        (
            "select spawn_mission_add('m', '[]'::jsonb, null)",
            "add a mission",
        ),
        ("select * from spawn_claim('w', 1000)", "claim a command"),
    ];
    for (sql, what) in refused {
        let error = page.batch_execute(sql).expect_err(what);
        assert_eq!(
            error.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{what}: {error}"
        );
    }
    let tasks: i64 = page
        .query_one("select count(*) from spawn_read_tasks()", &[])
        .expect("the reader answers")
        .get(0);
    assert_eq!(tasks, 0, "the grant-filtered readers show it nothing");
    assert_eq!(commands(&schema).len(), 1, "nothing was written");
}

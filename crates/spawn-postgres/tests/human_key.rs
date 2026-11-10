//! A person's key from the web page (`spawn serve`, `POST
//! /api/session/<id>/key`, backlog P1): the `spawn_human` login calls
//! `spawn_human_key`, which makes the key command `spawn submit --principal
//! human --action '{"type":"key","key":...}'` makes, to the session's own
//! agent, with the same idempotency rule, as its own action type. A session
//! that runs no execution is refused with the reason, and nothing is
//! recorded.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use std::time::Duration;

use postgres::error::SqlState;
use spawn_core::command::{CommandAction, NamedKey, Submission};
use spawn_core::event::{ENGINE, EndCause, EventKind, ModelProfile, NewEvent, Source};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, WorkspaceId,
};
use spawn_core::port::{CommandQueue, EventLog};
use spawn_postgres::{HumanKey, PostgresStore, role_url};

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

fn execution() -> ExecutionId {
    ExecutionId::new("exec_key").expect("valid execution")
}

fn append(store: &PostgresStore, source: Source, event_key: &str, kind: EventKind) {
    store
        .append(NewEvent {
            agent_id: AgentId::new(AGENT).expect("valid agent"),
            session_id: session(),
            execution_id: matches!(source, Source::Bridge).then(execution),
            source,
            source_event_id: Some(event_key.to_string()),
            kind,
        })
        .expect("append");
}

/// The session's agent seen, and (with `live`) an execution of it open.
fn seen(store: &PostgresStore, live: bool) {
    append(
        store,
        Source::Claude,
        "message-1",
        EventKind::MessageReceived,
    );
    if live {
        start(store);
    }
}

fn start(store: &PostgresStore) {
    append(
        store,
        Source::Bridge,
        "exec_key:started",
        EventKind::ExecutionStarted {
            engine: ENGINE.to_string(),
            workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
            command_id: CommandId(1),
            fork: None,
            model: ModelProfile::Default,
            transcript_from: None,
        },
    );
}

fn end(store: &PostgresStore) {
    append(
        store,
        Source::Bridge,
        "exec_key:ended",
        EventKind::ExecutionEnded {
            cause: EndCause::Stopped,
            exit_code: Some(0),
            signal: None,
            evidence: None,
        },
    );
}

/// A store connected as this database's `spawn_human` login.
fn human(schema: &str) -> PostgresStore {
    let url = role_url(&schema_url(schema), "spawn_human").expect("the human login's URL");
    PostgresStore::connect(&url).expect("the human login connects")
}

/// `spawn_commands` rows: principal, key, agent, session, action.
fn commands(schema: &str) -> Vec<(String, String, String, String, serde_json::Value)> {
    admin(schema)
        .query(
            "select principal, idempotency_key, agent_id, session_id, action
               from spawn_commands order by id",
            &[],
        )
        .expect("the commands")
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
        .collect()
}

#[test]
fn a_persons_key_from_the_page_is_the_key_command_spawn_submit_makes() {
    let (store, schema) = store("human_key");
    seen(&store, true);
    let page = human(&schema);
    let HumanKey::Submitted(id) = page
        .submit_human_key(&session(), &key("web-k1"), NamedKey::Escape)
        .expect("pressed")
    else {
        panic!("the session has an agent and runs an execution");
    };
    let action = CommandAction::Key {
        key: NamedKey::Escape,
    };
    assert_eq!(
        commands(&schema),
        [(
            "human".to_string(),
            "web-k1".to_string(),
            AGENT.to_string(),
            SESSION.to_string(),
            serde_json::json!({"type": "key", "key": "escape"}),
        )],
        "principal human, the session's own agent, a key action and no text"
    );

    // A retried POST makes one command, as `spawn submit` with the key does.
    assert_eq!(
        page.submit_human_key(&session(), &key("web-k1"), NamedKey::Escape)
            .expect("retried"),
        HumanKey::Submitted(id)
    );
    let cli = store
        .submit(
            &Principal::new("human").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: session(),
                idempotency_key: key("web-k1"),
                action: action.clone(),
            },
        )
        .expect("submit");
    assert_eq!(cli, id);
    assert_eq!(commands(&schema).len(), 1, "no second command");

    // A session no agent has used gets nothing.
    let unknown = SessionId::new("session_never_seen").expect("valid session");
    assert_eq!(
        page.submit_human_key(&unknown, &key("web-k2"), NamedKey::Up)
            .expect("answered"),
        HumanKey::NoAgent
    );
    assert_eq!(commands(&schema).len(), 1);

    // The Bridge claims it as the person's key.
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim")
        .expect("the key");
    assert_eq!(claimed.command_id, id);
    assert_eq!(claimed.principal, "human");
    assert_eq!(claimed.action().expect("parses"), action);

    // The session's commands show it as a key, its text the key's name: the
    // trace and the agent page read "You → key · escape".
    let shown = store.commands_json(SESSION, 10).expect("commands");
    assert_eq!(shown[0]["action"], "key");
    assert_eq!(shown[0]["text"], "escape");
    assert_eq!(shown[0]["principal"], "human");
}

/// A key needs a running engine: refused while the session runs nothing,
/// before its first execution and after its last, with the store's reason,
/// and recorded nowhere. A key the page already sent keeps its command.
#[test]
fn a_key_for_a_session_that_runs_nothing_is_refused_and_records_nothing() {
    let (store, schema) = store("human_key_stopped");
    seen(&store, false);
    let page = human(&schema);
    let refused = page
        .submit_human_key(&session(), &key("web-k1"), NamedKey::Escape)
        .expect("answered");
    assert_eq!(
        refused,
        HumanKey::NotRunning(
            "session_supervisor runs no execution: a key needs a running engine; resume the \
             session"
                .to_string()
        )
    );
    assert!(commands(&schema).is_empty(), "nothing recorded");

    start(&store);
    let sent = page
        .submit_human_key(&session(), &key("web-k2"), NamedKey::Y)
        .expect("pressed");
    assert!(matches!(sent, HumanKey::Submitted(_)), "{sent:?}");
    end(&store);
    assert!(matches!(
        page.submit_human_key(&session(), &key("web-k3"), NamedKey::N)
            .expect("answered"),
        HumanKey::NotRunning(_)
    ));
    assert_eq!(
        page.submit_human_key(&session(), &key("web-k2"), NamedKey::Y)
            .expect("retried"),
        sent,
        "a retry of a key already sent returns its command"
    );
    assert_eq!(commands(&schema).len(), 1);
}

/// What `spawn_human_key` refuses, whoever calls it: a name outside the set
/// (named in the error), no idempotency key; and every caller but the
/// `spawn_human` login.
#[test]
fn only_the_human_login_presses_and_only_named_keys() {
    let (store, schema) = store("human_key_refused");
    seen(&store, true);
    let mut page = postgres::Client::connect(
        &role_url(&schema_url(&schema), "spawn_human").expect("the login's URL"),
        postgres::NoTls,
    )
    .expect("the human login connects");
    let call = |client: &mut postgres::Client, key: &str, name: Option<&str>| {
        client
            .query_one(
                "select spawn_human_key($1, $2, $3)",
                &[&SESSION, &key, &name],
            )
            .map(|row| row.get::<_, Option<i64>>(0))
    };
    for (key, name, why) in [
        ("k1", Some("f1"), "a key outside the set"),
        ("k2", Some("Escape"), "names are exact"),
        ("k3", Some(""), "no name"),
        ("k4", None, "a null name"),
        ("k5", Some("escape\u{1b}"), "a name with an escape"),
        ("", Some("escape"), "no idempotency key"),
    ] {
        let error = call(&mut page, key, name).expect_err(why);
        assert_eq!(
            error.code(),
            Some(&SqlState::INVALID_PARAMETER_VALUE),
            "{why}: {error}"
        );
    }
    let error = call(&mut page, "k1", Some("f1")).expect_err("f1");
    let message = error.as_db_error().map(|db| db.message().to_string());
    assert_eq!(
        message.as_deref(),
        Some("key 'f1' is not one of escape, up, down, left, right, enter, y, n")
    );
    for name in NamedKey::ALL {
        assert!(
            call(&mut page, &format!("ok-{}", name.name()), Some(name.name())).is_ok(),
            "{}",
            name.name()
        );
    }
    let error = call(&mut admin(&schema), "k7", Some("up")).expect_err("the owner");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    let supervisor = role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL");
    let error = call(
        &mut postgres::Client::connect(&supervisor, postgres::NoTls).expect("connects"),
        "k8",
        Some("up"),
    )
    .expect_err("the supervisor");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(
        commands(&schema).len(),
        NamedKey::ALL.len(),
        "only the named keys"
    );
}

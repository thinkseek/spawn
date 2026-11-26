//! A person's message to the supervisor from Master Control (`spawn
//! serve`, `POST /api/master/message`, issue #91 P1a): the `spawn_human`
//! login calls `spawn_human_master_message`, which never refuses a
//! supervisor that runs no execution: it wakes it (a resume when a native
//! session is bound, else a start) before the input, in every mode, and
//! restart stops a running one first, steer interrupts its turn with Esc.
//! Each retried request makes nothing more.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::error::SqlState;
use serde_json::{Value, json};
use spawn_core::command::CommandAction;
use spawn_core::event::{ENGINE, EndCause, EventKind, ModelProfile, NewEvent, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, IdempotencyKey, SessionId, WorkspaceId};
use spawn_core::port::EventLog;
use spawn_postgres::{HumanMaster, MasterMessageMode, PostgresStore, role_url};

mod common;
use common::{admin, schema_url, store};

const SESSION: &str = "session_supervisor";
const AGENT: &str = "agent_supervisor";

fn append(store: &PostgresStore, source: Source, event_key: &str, kind: EventKind) {
    store
        .append(NewEvent {
            agent_id: AgentId::new(AGENT).expect("valid agent"),
            session_id: SessionId::new(SESSION).expect("valid session"),
            execution_id: matches!(source, Source::Bridge)
                .then(|| ExecutionId::new("exec_master").expect("valid execution")),
            source,
            source_event_id: Some(event_key.to_string()),
            kind,
        })
        .expect("append");
}

fn seen(store: &PostgresStore) {
    append(
        store,
        Source::Claude,
        "message-1",
        EventKind::MessageReceived,
    );
}

fn bound(store: &PostgresStore) {
    append(
        store,
        Source::Claude,
        "native-1",
        EventKind::NativeSessionStarted {
            engine: ENGINE.to_string(),
            native_session_id: "native-1".to_string(),
            transcript_path: None,
            engine_version: None,
        },
    );
}

fn start(store: &PostgresStore) {
    append(
        store,
        Source::Bridge,
        "exec_master:started",
        EventKind::ExecutionStarted {
            engine: ENGINE.to_string(),
            workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
            command_id: CommandId(1),
            fork: None,
            model: ModelProfile::Default,
            transcript_from: None,
            launch: None,
        },
    );
}

fn end(store: &PostgresStore) {
    append(
        store,
        Source::Bridge,
        "exec_master:ended",
        EventKind::ExecutionEnded {
            cause: EndCause::Stopped,
            exit_code: Some(0),
            signal: None,
            evidence: None,
        },
    );
}

fn human(schema: &str) -> PostgresStore {
    let url = role_url(&schema_url(schema), "spawn_human").expect("the human login's URL");
    PostgresStore::connect(&url).expect("the human login connects")
}

fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text).expect("valid key")
}

/// `(principal, key, session, action)` of every command, in id order.
fn commands(schema: &str) -> Vec<(String, String, String, Value)> {
    admin(schema)
        .query(
            "select principal, idempotency_key, session_id, action from spawn_commands order by id",
            &[],
        )
        .expect("the commands")
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect()
}

fn mode(name: &str) -> MasterMessageMode {
    MasterMessageMode::from_name(name).expect("a mode")
}

fn sent(page: &PostgresStore, k: &str, text: &str, name: &str) -> Value {
    match page
        .human_master_message(&key(k), text, mode(name))
        .expect("the store answers")
    {
        HumanMaster::Submitted(answer) => answer,
        other => panic!("{k}: {other:?}"),
    }
}

fn input(text: &str, mode: &str) -> Value {
    json!({"type": "input", "text": text, "tags": [format!("master:{mode}")]})
}

/// An input's action with only its first tag (`master:<mode>`): the others
/// name the commands the message made, checked where they matter.
fn plain(action: &Value) -> Value {
    let mut action = action.clone();
    if let Some(tags) = action.get_mut("tags").and_then(Value::as_array_mut) {
        tags.truncate(1);
    }
    action
}

#[test]
fn a_message_to_a_supervisor_that_never_ran_starts_it_then_types() {
    let (_store, schema) = store("master_never_ran");
    let page = human(&schema);
    let answer = sent(&page, "m1", "Hello, are you there?", "auto");
    assert_eq!(answer["session_id"], SESSION);
    assert_eq!(answer["principal"], "human");
    assert_eq!(answer["mode"], "auto");
    assert_eq!(answer["woke"], true);
    assert_eq!(answer["replayed"], false);
    let listed = commands(&schema);
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(listed[0].1, "m1:wake");
    assert_eq!(listed[0].3["type"], "start");
    assert_eq!(listed[0].3["engine"], ENGINE);
    assert_eq!(
        listed[1],
        (
            "human".to_string(),
            "m1".to_string(),
            SESSION.to_string(),
            json!({"type": "input", "text": "Hello, are you there?",
                   "tags": ["master:auto",
                            format!("master-wake:{}", answer["wake_command_id"])]}),
        )
    );
    // Every command is one the Bridge parses.
    for command in &listed {
        serde_json::from_value::<CommandAction>(command.3.clone()).expect("parses");
    }
    assert!(answer["wake_command_id"].as_i64() < answer["command_id"].as_i64());
    // A retried POST makes nothing and answers the same commands.
    let again = sent(&page, "m1", "Hello, are you there?", "auto");
    assert_eq!(again["command_id"], answer["command_id"]);
    assert_eq!(again["wake_command_id"], answer["wake_command_id"]);
    assert_eq!(again["replayed"], true);
    assert_eq!(commands(&schema).len(), 2);
    // Other text, or another mode, under the key is refused.
    for (text, name) in [
        ("Something else", "auto"),
        ("Hello, are you there?", "queue"),
    ] {
        let refused = page
            .human_master_message(&key("m1"), text, mode(name))
            .expect("answers");
        assert!(matches!(refused, HumanMaster::KeyUsed(_)), "{refused:?}");
    }
    assert_eq!(commands(&schema).len(), 2);
}

#[test]
fn a_stopped_supervisor_with_a_native_session_is_resumed_never_refused() {
    let (store, schema) = store("master_stopped");
    seen(&store);
    bound(&store);
    start(&store);
    end(&store);
    let page = human(&schema);
    for (k, mode) in [("q1", "queue"), ("s1", "steer")] {
        let answer = sent(&page, k, "Status?", mode);
        assert_eq!(answer["mode"], mode);
        assert!(answer["wake_command_id"].is_i64(), "{answer}");
        assert!(answer["steer_command_id"].is_null(), "nothing to interrupt");
    }
    let listed = commands(&schema);
    // The first message resumes; the second finds that resume in flight
    // and makes none of its own.
    let keys: Vec<&str> = listed.iter().map(|command| command.1.as_str()).collect();
    assert_eq!(keys, ["q1:wake", "q1", "s1"], "{listed:?}");
    assert_eq!(listed[0].3, json!({"type": "resume"}));
    assert_eq!(plain(&listed[1].3), input("Status?", "queue"));
    assert_eq!(plain(&listed[2].3), input("Status?", "steer"));
}

#[test]
fn a_running_supervisor_gets_the_input_steer_interrupts_restart_stops_and_wakes() {
    let (store, schema) = store("master_running");
    seen(&store);
    bound(&store);
    start(&store);
    let page = human(&schema);
    let auto = sent(&page, "a1", "Carry on.", "auto");
    assert!(auto["wake_command_id"].is_null(), "it runs: {auto}");
    assert_eq!(auto["woke"], false);
    let steer = sent(&page, "s1", "Stop that, do this.", "steer");
    assert!(steer["steer_command_id"].is_i64(), "{steer}");
    let restart = sent(&page, "r1", "Start over.", "restart");
    assert!(restart["stop_command_id"].is_i64(), "{restart}");
    assert!(restart["wake_command_id"].is_i64(), "{restart}");
    let listed: Vec<(String, Value)> = commands(&schema)
        .into_iter()
        .map(|command| (command.1, plain(&command.3)))
        .collect();
    assert_eq!(
        listed,
        [
            ("a1".to_string(), input("Carry on.", "auto")),
            (
                "s1:steer".to_string(),
                json!({"type": "key", "key": "escape"})
            ),
            ("s1".to_string(), input("Stop that, do this.", "steer")),
            ("r1:stop".to_string(), json!({"type": "stop"})),
            ("r1:wake".to_string(), json!({"type": "resume"})),
            ("r1".to_string(), input("Start over.", "restart")),
        ]
    );
    let again = sent(&page, "r1", "Start over.", "restart");
    assert_eq!(again["stop_command_id"], restart["stop_command_id"]);
    assert_eq!(again["wake_command_id"], restart["wake_command_id"]);
    assert_eq!(
        commands(&schema).len(),
        6,
        "a retried restart makes nothing"
    );
}

#[test]
fn a_bad_message_is_refused_and_only_the_human_login_may_send() {
    let (_store, schema) = store("master_refusals");
    let page = human(&schema);
    for (k, text, mode, why) in [
        ("k1", "   ", "auto", "a blank text"),
        ("k2", "hi\u{1b}[31m", "auto", "an escape"),
        ("k3", "hi", "shout", "an unknown mode"),
        ("", "hi", "auto", "no key"),
    ] {
        let error = postgres::Client::connect(
            &role_url(&schema_url(&schema), "spawn_human").expect("url"),
            postgres::NoTls,
        )
        .expect("connects")
        .query_one(
            "select spawn_human_master_message($1, $2, $3)",
            &[&k, &text, &mode],
        )
        .expect_err(why);
        assert_eq!(
            error.code(),
            Some(&SqlState::INVALID_PARAMETER_VALUE),
            "{why}"
        );
    }
    let long = "x".repeat(8193);
    assert!(matches!(
        page.human_master_message(&key("k5"), &long, mode("auto"))
            .expect("answers"),
        HumanMaster::Refused(_)
    ));
    assert!(matches!(
        page.human_master_message(&key(&"k".repeat(241)), "hi", mode("auto"))
            .expect("answers"),
        HumanMaster::Refused(_)
    ));
    let error = admin(&schema)
        .query_one(
            "select spawn_human_master_message($1, $2, $3)",
            &[&"k6", &"hi", &"auto"],
        )
        .expect_err("the owner");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    let supervisor = role_url(&schema_url(&schema), "spawn_supervisor").expect("url");
    let error = postgres::Client::connect(&supervisor, postgres::NoTls)
        .expect("connects")
        .query_one(
            "select spawn_human_master_message($1, $2, $3)",
            &[&"k7", &"hi", &"auto"],
        )
        .expect_err("the supervisor");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert!(commands(&schema).is_empty(), "nothing recorded");
    // A key whose derived wake key another request used is refused, not
    // answered with that request's command.
    sent(&page, "x:wake", "first", "auto");
    // Its start ran and ended: the next message needs a wake of its own.
    admin(&schema)
        .execute("update spawn_commands set state = 'finished'", &[])
        .expect("finished");
    let before = commands(&schema).len();
    let collided = page
        .human_master_message(&key("x"), "second", mode("auto"))
        .expect("answers");
    assert!(matches!(collided, HumanMaster::KeyUsed(_)), "{collided:?}");
    assert_eq!(commands(&schema).len(), before, "nothing made");
}

/// Review M3: a restart while a start is still in flight (the supervisor's
/// first start, not claimed yet) makes a wake of its own after its stop,
/// never takes the older start, which the stop would end; a message while
/// that stop is pending is woken after it too.
#[test]
fn a_restarts_wake_is_newer_than_its_stop() {
    let (store, schema) = store("master_wake_after_stop");
    let page = human(&schema);
    let first = sent(&page, "m1", "Wake up.", "auto");
    let start_id = first["wake_command_id"].as_i64().expect("a start");
    // It runs now, its start command not finished yet.
    seen(&store);
    start(&store);
    let restart = sent(&page, "r1", "Start over.", "restart");
    let stop = restart["stop_command_id"].as_i64().expect("a stop");
    let wake = restart["wake_command_id"].as_i64().expect("a wake");
    assert!(stop > start_id, "{restart}");
    assert!(wake > stop, "the wake comes after the stop: {restart}");
    assert_ne!(wake, start_id, "not the older start");
    // Another message while the stop is pending: woken by the wake after
    // the stop, no third one made.
    let after = sent(&page, "a1", "And this.", "auto");
    assert_eq!(after["wake_command_id"], wake, "{after}");
    assert!(after["steer_command_id"].is_null());
    let keys: Vec<String> = commands(&schema)
        .into_iter()
        .map(|command| command.1)
        .collect();
    assert_eq!(keys, ["m1:wake", "m1", "r1:stop", "r1:wake", "r1", "a1"]);
    // A steer behind a pending stop interrupts nothing (the stop ends it):
    // it is woken after the stop instead.
    let steer = sent(&page, "s1", "Steer.", "steer");
    assert!(steer["steer_command_id"].is_null(), "{steer}");
    assert_eq!(steer["wake_command_id"], wake);
}

/// Review L1: a repeat answers exactly what the first call answered, a
/// wake it took (not made) included, whatever was made since.
#[test]
fn a_repeat_answers_the_first_calls_commands_exactly() {
    let (store, schema) = store("master_replay_exact");
    seen(&store);
    bound(&store);
    start(&store);
    end(&store);
    let page = human(&schema);
    let first = sent(&page, "q1", "First.", "queue");
    let taker = sent(&page, "q2", "Second.", "queue");
    assert_eq!(
        taker["wake_command_id"], first["wake_command_id"],
        "took q1's wake"
    );
    // Commands made since, some under keys like the derived ones.
    admin(&schema)
        .execute("update spawn_commands set state = 'finished'", &[])
        .expect("finished");
    let later = sent(&page, "q3", "Third.", "auto");
    assert_ne!(later["wake_command_id"], first["wake_command_id"]);
    for (k, text, name, answer) in [
        ("q1", "First.", "queue", &first),
        ("q2", "Second.", "queue", &taker),
        ("q3", "Third.", "auto", &later),
    ] {
        let again = sent(&page, k, text, name);
        for field in [
            "command_id",
            "wake_command_id",
            "stop_command_id",
            "steer_command_id",
            "woke",
        ] {
            assert_eq!(again[field], answer[field], "{k} {field}");
        }
        assert_eq!(again["replayed"], true);
    }
}

/// Review L2: a key whose derived `:steer` or `:stop` key an earlier
/// request used (a key pressed from the agent page under that key) is
/// refused 23505, nothing made, never answered with that earlier command.
#[test]
fn a_derived_steer_or_stop_key_another_request_used_is_refused() {
    let (store, schema) = store("master_derived_keys");
    seen(&store);
    bound(&store);
    start(&store);
    let page = human(&schema);
    let pressed = page
        .submit_human_key(
            &SessionId::new(SESSION).expect("session"),
            &key("x:steer"),
            spawn_core::command::NamedKey::Escape,
        )
        .expect("a key");
    assert!(
        matches!(pressed, spawn_postgres::HumanKey::Submitted(_)),
        "{pressed:?}"
    );
    let before = commands(&schema).len();
    for name in ["steer", "auto", "restart"] {
        let refused = page
            .human_master_message(&key("x"), "Steer it.", mode(name))
            .expect("answers");
        assert!(
            matches!(refused, HumanMaster::KeyUsed(_)),
            "{name}: {refused:?}"
        );
    }
    let stop_key = page
        .submit_human_key(
            &SessionId::new(SESSION).expect("session"),
            &key("y:stop"),
            spawn_core::command::NamedKey::Enter,
        )
        .expect("a key");
    assert!(matches!(stop_key, spawn_postgres::HumanKey::Submitted(_)));
    let refused = page
        .human_master_message(&key("y"), "Start over.", mode("restart"))
        .expect("answers");
    assert!(matches!(refused, HumanMaster::KeyUsed(_)), "{refused:?}");
    assert_eq!(
        commands(&schema).len(),
        before + 1,
        "only the second key was made"
    );
}

/// Submit `action` to the supervisor's session as the owner, and finish it
/// (an earlier start that ran and ended).
fn finished_start(store: &PostgresStore, schema: &str, name: &str, action: CommandAction) {
    use spawn_core::command::Submission;
    use spawn_core::id::Principal;
    use spawn_core::port::CommandQueue;
    let id = store
        .submit(
            &Principal::new("owner").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: SessionId::new(SESSION).expect("valid session"),
                idempotency_key: key(name),
                action,
            },
        )
        .expect("submit succeeds");
    admin(schema)
        .execute(
            "update spawn_commands set state = 'finished' where id = $1",
            &[&i64::try_from(id.0).expect("an id")],
        )
        .expect("finish it");
}

/// H4r (decision C2, SQL wake): a supervisor that never bound a native
/// session is woken by a start on the engine, model profile and model label
/// of its newest start, each only when that start names it; with no start
/// at all (or a Claude start without them) the wake is what it always was.
/// `latest_engine` reads the same newest start (N4's lookup).
#[test]
fn a_cold_wake_takes_the_engine_model_and_label_of_the_newest_start() {
    let (store, schema) = store("master_wake_engine");
    let session = SessionId::new(SESSION).expect("valid session");
    assert_eq!(store.latest_engine(&session).expect("reads"), None);
    finished_start(
        &store,
        &schema,
        "first-claude",
        CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "p".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
            charter: None,
            model_label: None,
        },
    );
    assert_eq!(
        store.latest_engine(&session).expect("reads").as_deref(),
        Some("claude")
    );
    let page = human(&schema);
    let claude = sent(&page, "c1", "Hello.", "auto");
    let listed = commands(&schema);
    let wake = &listed
        .iter()
        .find(|command| command.1 == "c1:wake")
        .expect("a wake")
        .3;
    assert_eq!(
        wake,
        &json!({"type": "start", "engine": "claude",
                "prompt": "The owner wrote to you from the dashboard (Master Control) while you \
                           were stopped; the message follows as your next input. Read it, then \
                           go on with your work."}),
        "a Claude session's wake is unchanged: {claude}"
    );
    admin(&schema)
        .execute("update spawn_commands set state = 'finished'", &[])
        .expect("finish them");
    finished_start(
        &store,
        &schema,
        "then-agy",
        CommandAction::Start {
            engine: "agy".to_string(),
            prompt: "p".to_string(),
            parent_session_id: None,
            from: None,
            model: Some(ModelProfile::Cheap),
            charter: None,
            model_label: Some("gemini-3.8-pro".to_string()),
        },
    );
    assert_eq!(
        store.latest_engine(&session).expect("reads").as_deref(),
        Some("agy")
    );
    let agy = sent(&page, "a1", "Hello again.", "auto");
    let listed = commands(&schema);
    let wake = &listed
        .iter()
        .find(|command| command.1 == "a1:wake")
        .expect("a wake")
        .3;
    assert_eq!(wake["engine"], "agy", "{agy}");
    assert_eq!(wake["model"], "cheap");
    assert_eq!(wake["model_label"], "gemini-3.8-pro");
    let parsed: CommandAction = serde_json::from_value(wake.clone()).expect("parses");
    let CommandAction::Start {
        engine,
        model,
        model_label,
        ..
    } = parsed
    else {
        panic!("a start: {wake}");
    };
    assert_eq!(engine, "agy");
    assert_eq!(model, Some(ModelProfile::Cheap));
    assert_eq!(model_label.as_deref(), Some("gemini-3.8-pro"));
}

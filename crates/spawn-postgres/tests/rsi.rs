//! Controller state tests.
//!
//! The loop keeps its state outside `spawn_events`, so these cases check the
//! tables and the transitions the controller drives. A real database is
//! required, as in `api.rs`.

use spawn_core::command::CommandAction;
use spawn_core::event::Engine;
use spawn_postgres::PostgresStore;

/// First command of an iteration.
fn start(prompt: &str) -> CommandAction {
    CommandAction::Start {
        engine: Engine::Claude,
        prompt: prompt.to_string(),
        parent_session_id: None,
    }
}

fn url() -> String {
    std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip")
}

fn store(case: &str) -> PostgresStore {
    let schema = format!("spawn_rsi_{}_{case}", std::process::id());
    PostgresStore::connect_in_schema(&url(), &schema).expect("connect and migrate")
}

#[test]
fn the_lock_admits_one_controller_at_a_time() {
    let first = store("lock");
    let second = store("lock");
    assert!(first.rsi_try_lock().expect("first lock"));
    assert!(!second.rsi_try_lock().expect("second lock"));
}

#[test]
fn seeding_keeps_the_first_baseline() {
    let store = store("seed");
    assert_eq!(store.rsi_state().expect("read state"), None);
    store.rsi_seed_baseline("aaa").expect("seed");
    store.rsi_seed_baseline("bbb").expect("seed again");
    let state = store.rsi_state().expect("read state").expect("seeded");
    assert_eq!(state.baseline, "aaa");
    assert_eq!(state.iteration, 0);
}

#[test]
fn beginning_an_iteration_writes_the_row_and_its_command() {
    let store = store("begin");
    store.rsi_seed_baseline("aaa").expect("seed");
    let command = store
        .rsi_begin_iteration(7, 1, start("do the task"))
        .expect("begin");

    let open = store
        .rsi_open_iteration()
        .expect("open")
        .expect("one open iteration");
    assert_eq!(open.iteration, 1);
    assert_eq!(open.task_id, 7);
    assert_eq!(open.phase, "Starting");
    assert_eq!(open.agent_id, spawn_postgres::RSI_AGENT);
    assert_eq!(open.command_id, Some(command));

    let state = store.rsi_state().expect("state").expect("seeded");
    assert_eq!(state.iteration, 1);

    let row = store
        .rsi_command_state(command)
        .expect("command")
        .expect("queued");
    assert_eq!(row.state, "pending");
    assert_eq!(row.succeeded, None);
}

#[test]
fn landing_advances_the_baseline_and_clears_the_failure_counter() {
    let store = store("land");
    store.rsi_seed_baseline("aaa").expect("seed");
    store
        .rsi_begin_iteration(7, 1, start("first"))
        .expect("begin");
    store
        .rsi_close_iteration(1, "Rejected", "gate said no")
        .expect("close");
    let state = store.rsi_state().expect("state").expect("seeded");
    assert_eq!(state.consecutive_rejected, 1);

    store
        .rsi_begin_iteration(7, 2, start("second"))
        .expect("begin");
    assert!(store.rsi_advance_baseline(2, "aaa", "bbb").expect("land"));

    let state = store.rsi_state().expect("state").expect("seeded");
    assert_eq!(state.baseline, "bbb");
    assert_eq!(state.consecutive_rejected, 0);
    assert_eq!(store.rsi_open_iteration().expect("open"), None);
}

#[test]
fn landing_refuses_a_baseline_that_moved() {
    let store = store("stale");
    store.rsi_seed_baseline("aaa").expect("seed");
    store
        .rsi_begin_iteration(7, 1, start("task"))
        .expect("begin");
    assert!(!store.rsi_advance_baseline(1, "zzz", "bbb").expect("land"));
    let state = store.rsi_state().expect("state").expect("seeded");
    assert_eq!(state.baseline, "aaa");
}

#[test]
fn a_recorded_attempt_feeds_the_next_prompt() {
    let store = store("attempt");
    store.rsi_seed_baseline("aaa").expect("seed");
    store
        .rsi_record_attempt(
            7,
            1,
            1,
            Some("diff one"),
            Some("gate one"),
            "rejected",
            false,
        )
        .expect("record");
    assert_eq!(store.rsi_count_attempts(7).expect("count"), 1);
    assert_eq!(store.rsi_solved_tasks().expect("solved"), Vec::<u32>::new());
    assert_eq!(
        store.rsi_last_attempt_of(7).expect("last"),
        Some(("diff one".to_string(), "gate one".to_string()))
    );

    store
        .rsi_record_attempt(7, 2, 2, Some("diff two"), None, "accepted", true)
        .expect("record");
    assert_eq!(store.rsi_solved_tasks().expect("solved"), vec![7]);
}

#[test]
fn the_budget_starts_at_the_iteration_and_counts_from_there() {
    let store = store("budget");
    store.rsi_seed_baseline("aaa").expect("seed");
    store
        .rsi_begin_iteration(7, 1, start("do the task"))
        .expect("begin");

    let budget = store
        .rsi_iteration_budget(1)
        .expect("budget")
        .expect("one iteration");
    // The start is recorded, so a clock that never started cannot excuse a
    // runaway iteration.
    let elapsed = budget.elapsed_ms.expect("started_at_ms is set");
    assert!((0..60_000).contains(&elapsed), "elapsed was {elapsed}");
    assert_eq!(budget.tool_calls, 0);
}

#[test]
fn an_abort_counts_as_infrastructure_not_as_a_rejection() {
    let store = store("abort");
    store.rsi_seed_baseline("aaa").expect("seed");
    store
        .rsi_begin_iteration(7, 1, start("do the task"))
        .expect("begin");
    store
        .rsi_close_iteration(1, "Aborted", "agent did not start")
        .expect("abort");

    let state = store.rsi_state().expect("state").expect("seeded");
    assert_eq!(state.infra_failures, 1);
    assert_eq!(state.consecutive_rejected, 0);
    // An aborted candidate was never judged, so it is not an attempt.
    assert_eq!(store.rsi_count_attempts(7).expect("count"), 0);

    // A landing clears the infrastructure counter.
    store
        .rsi_begin_iteration(7, 2, start("second"))
        .expect("begin");
    assert!(store.rsi_advance_baseline(2, "aaa", "bbb").expect("land"));
    assert_eq!(
        store
            .rsi_state()
            .expect("state")
            .expect("seeded")
            .infra_failures,
        0
    );
}

#[test]
fn the_terminal_login_cannot_write_controller_facts() {
    // Built here, not through `store()`: that helper builds its own name, and
    // the role below must enter the same schema.
    let schema = format!("spawn_terminal_{}", std::process::id());
    let owner = PostgresStore::connect_in_schema(&url(), &schema).expect("connect and migrate");
    owner.rsi_seed_baseline("aaa").expect("seed");

    // The agent runs in the pane and reads its environment, so this is the
    // credential it holds.
    let url = spawn_postgres::terminal_url(&url());
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("terminal connect");
    client
        .batch_execute(&format!("set search_path to {schema}"))
        .expect("search path");

    // The NULLs need types: an untyped call is ambiguous and would fail for
    // the wrong reason, which is how this test passed once without testing.
    let forged = client.query(
        "select * from spawn_append('agent_rsi','session_rsi',null::text,'observer',
             null::text, '{\"name\":\"observation.recorded\"}'::jsonb)",
        &[],
    );
    // The reason matters: any error would pass a bare `is_err`, including a
    // missing function or a schema the role never entered.
    let refused = format!("{forged:?}");
    assert!(
        refused.contains("spawn_terminal may not write"),
        "the whitelist, not some other failure, must refuse it: {refused}"
    );

    let engine = client.query(
        "select * from spawn_append('agent_rsi','session_rsi',null::text,'claude',
             null::text, '{\"name\":\"message.received\"}'::jsonb)",
        &[],
    );
    assert!(
        engine.is_ok(),
        "the terminal must still write engine events: {engine:?}"
    );

    let direct = client.execute("update rsi_state set baseline_commit = 'evil'", &[]);
    let denied = format!("{direct:?}");
    assert!(
        denied.contains("permission denied"),
        "the table privilege, not a syntax error, must refuse it: {denied}"
    );
}

#[test]
fn the_scenario_floor_round_trips() {
    let store = store("floor");
    store.rsi_seed_baseline("aaa").expect("seed");
    let state = store.rsi_state().expect("state").expect("seeded");
    assert_eq!(state.scenario_count, 0);

    store.rsi_set_scenario_count(4).expect("set");
    let state = store.rsi_state().expect("state").expect("seeded");
    assert_eq!(state.scenario_count, 4);
}

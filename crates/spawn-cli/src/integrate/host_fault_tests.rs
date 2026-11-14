use std::cell::Cell;
use std::time::Duration;

use serde_json::json;

use super::host::{self, Class, Failure};
use super::{
    Mission, halt_reason, host_fault_backoff, host_fault_base, host_fault_fields, host_wait,
};

const REFUSED: &str = "connect to the test database: error connecting to server: Connection \
                           refused (os error 111)";
const CRASHED: &str = "called `Result::unwrap()` on an `Err` value: Error { kind: Db, cause: \
                           Some(DbError { severity: \"FATAL\", parsed_severity: Some(Fatal), \
                           code: SqlState(E57P02), message: \"terminating connection because \
                           of crash of another server process\" }) }";
const RECOVERING: &str = "db error: FATAL: the database system is in recovery mode";
const LOST_OID: &str = "called `Result::unwrap()` on an `Err` value: Database(\"db error: \
                            ERROR: could not open relation with OID 16421\")";
const LOST_CACHE: &str = "db error: ERROR: cache lookup failed for relation 16388";
const ASSERTION: &str = "assertion `left == right` failed: the landing moved main";
const NOTE: &str = "note: run with `RUST_BACKTRACE=1` environment variable to display a \
                        backtrace";

/// What gate.sh prints around check-tests.sh's output when it rejects
/// the candidate for it: the stages before, the section's opening line,
/// the check's lines (`| `-prefixed), the stage that closes it and the
/// verdict.
fn gate_output(lines: &[&str]) -> String {
    let mut log = String::from(
        "== candidate: build\n| build: ok\n| stage gate ok 900ms candidate: build\n\
             | == regression: check-tests.sh must still pass\n",
    );
    for line in lines {
        log.push_str("| ");
        log.push_str(line);
        log.push('\n');
    }
    log.push_str(
        "| stage gate rejected 1200ms regression: check-tests.sh must still pass\n\
             | gate time: base 0 s, candidate 1 s, setup 0 s, total 1 s\n\
             {\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the \
             candidate\"}\n",
    );
    log
}

/// One failing test as cargo prints it and the excerpt keeps it.
fn panic_lines<'a>(test: &'a str, message: &[&'a str]) -> Vec<String> {
    let mut lines = vec![
        format!("---- {test} stdout ----"),
        String::new(),
        format!("thread '{test}' panicked at crates/spawn-postgres/tests/common/mod.rs:31:10:"),
    ];
    lines.extend(message.iter().map(ToString::to_string));
    lines.push(NOTE.to_string());
    lines.push("--".to_string());
    lines
}

fn output_of(lines: &[String]) -> String {
    gate_output(&lines.iter().map(String::as_str).collect::<Vec<_>>())
}

fn failure(test: &str, message: &str) -> Failure {
    Failure {
        test: test.to_string(),
        message: message.to_string(),
    }
}

fn tests_of(failures: &[Failure]) -> Vec<&str> {
    failures
        .iter()
        .map(|failure| failure.test.as_str())
        .collect()
}

/// The owner's form: the FAILED lines are the failures, all of them,
/// even though the excerpt after them was cut after the first.
#[test]
fn the_failed_lines_win_over_a_cut_excerpt() {
    let mut lines = vec![
        format!("FAILED api::tests::claims_in_order: {REFUSED}"),
        format!("FAILED control::tests::the_fold_replays: {CRASHED}"),
        format!("FAILED stalls::tests::delivery_is_measured_from_its_record: {RECOVERING}"),
    ];
    lines.extend(panic_lines("api::tests::claims_in_order", &[REFUSED]));
    let failures = host::check_tests_failures(&output_of(&lines));
    assert_eq!(
        failures,
        [
            failure("api::tests::claims_in_order", REFUSED),
            failure("control::tests::the_fold_replays", CRASHED),
            failure(
                "stalls::tests::delivery_is_measured_from_its_record",
                RECOVERING
            ),
        ]
    );
    // check-tests.sh puts the panic's location before the message.
    let located = gate_output(&[
        "FAILED merges::tests::a_conflict: crates/spawn-cli/src/merges.rs:12:5: boom",
        "FAILED sources::tests::empty:",
    ]);
    assert_eq!(
        host::check_tests_failures(&located),
        [
            failure(
                "merges::tests::a_conflict",
                "crates/spawn-cli/src/merges.rs:12:5: boom"
            ),
            failure("sources::tests::empty", ""),
        ]
    );
}

/// check-tests.sh prints what a suite run by one test says of its own
/// failures (the relay's Python unittest) as `  in <test>: <line>`,
/// before the FAILED lines. Those lines are never failures of their own,
/// whatever they say: the classification reads the FAILED lines alone.
#[test]
fn a_suites_own_lines_are_not_failures() {
    let relay = "the_model_relay_passes_its_self_test";
    let relay_failed = format!("FAILED {relay}: crates/spawn-cli/tests/model_relay.rs:19:5: ..F");
    let refused_in =
        format!("  in {relay}: ConnectionRefusedError: [Errno 111] Connection refused");
    let assertion_in = format!("  in {relay}: AssertionError: 2 != 3");
    let reason = host::CHECK_TESTS_REGRESSION;
    let no_log = || -> Vec<String> { panic!("connection errors need no log") };

    // A real failure, whose suite also printed a connection error: only
    // the FAILED line is read, and it stays a rejection.
    let work = output_of(&[refused_in.clone(), assertion_in.clone(), relay_failed]);
    assert_eq!(tests_of(&host::check_tests_failures(&work)), [relay]);
    assert_eq!(host::host_fault(reason, &work, Vec::new), None);
    // Connection errors only, beside a suite's assertion: still the
    // host's.
    let refused = output_of(&[
        assertion_in,
        refused_in,
        format!("FAILED api::tests::claims_in_order: {REFUSED}"),
        format!("FAILED control::tests::the_fold_replays: {REFUSED}"),
    ]);
    assert_eq!(
        tests_of(&host::check_tests_failures(&refused)),
        [
            "api::tests::claims_in_order",
            "control::tests::the_fold_replays"
        ]
    );
    assert!(host::host_fault(reason, &refused, no_log).is_some());
}

/// Today's form, one failure: the panic's test and its message, every
/// line up to the note.
#[test]
fn the_excerpt_alone_names_one_failure_with_its_message() {
    let mut lines = vec![
        "test merges::tests::a_conflict ... FAILED".to_string(),
        String::new(),
        "failures:".to_string(),
    ];
    lines.extend(panic_lines(
        "merges::tests::a_conflict",
        &["assertion `left == right` failed", "  left: 1", " right: 2"],
    ));
    lines.push("test result: FAILED. 41 passed; 1 failed; 0 ignored".to_string());
    let failures = host::check_tests_failures(&output_of(&lines));
    assert_eq!(
        failures,
        [failure(
            "merges::tests::a_conflict",
            "assertion `left == right` failed\n  left: 1\n right: 2"
        )]
    );
    assert_eq!(host::classify(&failures[0].message), Class::Work);
    // Lines outside the check-tests section are never read.
    let elsewhere = format!(
        "| == regression: check-e2e.sh must still pass\n\
             | thread 'e2e' panicked at e2e.rs:1:1:\n| {REFUSED}\n{}",
        gate_output(&[])
    );
    assert!(host::check_tests_failures(&elsewhere).is_empty());
    assert!(host::check_tests_failures("{\"verdict\":\"rejected\"}\n").is_empty());
}

/// Today's form, several test binaries: each binary's failures, its
/// summary and cargo's error line between them.
#[test]
fn the_excerpt_names_the_failures_of_several_binaries() {
    let mut lines = Vec::new();
    lines.extend(panic_lines(
        "sources::tests::lists_every_source",
        &[REFUSED],
    ));
    lines.push(
        "test result: FAILED. 41 passed; 1 failed; 0 ignored; 0 measured; finished in 3.02s"
            .to_string(),
    );
    lines.push("error: test failed, to rerun pass `-p spawn-cli --bin spawn`".to_string());
    lines.push("--".to_string());
    // Rust 1.88 on names the thread's id too.
    lines.extend([
        "---- a_gate_verdict_lands_only_on_its_revision stdout ----".to_string(),
        "thread 'a_gate_verdict_lands_only_on_its_revision' (2794460) panicked at \
             tests/control.rs:9:9:"
            .to_string(),
        CRASHED.to_string(),
        NOTE.to_string(),
    ]);
    lines.extend(panic_lines(
        "objective_tasks_keep_their_history",
        &[RECOVERING],
    ));
    lines.push("test result: FAILED. 20 passed; 2 failed; 0 ignored".to_string());
    lines.push("error: 2 targets failed:".to_string());
    let failures = host::check_tests_failures(&output_of(&lines));
    assert_eq!(
        tests_of(&failures),
        [
            "sources::tests::lists_every_source",
            "a_gate_verdict_lands_only_on_its_revision",
            "objective_tasks_keep_their_history"
        ]
    );
    assert_eq!(failures[1].message, CRASHED);
    assert!(
        failures
            .iter()
            .all(|failure| host::classify(&failure.message) == Class::Connection)
    );
}

/// Today's form cut at its 40 lines: a panic whose message was cut off
/// is not seen; one cut inside its message keeps what was shown.
#[test]
fn a_truncated_excerpt_shows_what_it_kept() {
    let mut lines = panic_lines("api::tests::claims_in_order", &[REFUSED]);
    lines.extend([
        "---- control::tests::the_fold_replays stdout ----".to_string(),
        String::new(),
        "thread 'control::tests::the_fold_replays' panicked at tests/control.rs:9:9:".to_string(),
        RECOVERING.to_string(),
        "---- stalls::tests::a_stall stdout ----".to_string(),
        String::new(),
        "thread 'stalls::tests::a_stall' panicked at tests/stalls.rs:4:4:".to_string(),
    ]);
    let failures = host::check_tests_failures(&output_of(&lines));
    assert_eq!(
        failures,
        [
            failure("api::tests::claims_in_order", REFUSED),
            failure("control::tests::the_fold_replays", RECOVERING),
        ],
        "the last panic's message was cut off: not seen"
    );
}

/// `cargo test` as Rust 1.95 prints it (thread ids, an `Error:` test),
/// through check-tests.sh in both of its forms: the FAILED lines name all
/// three; the excerpt, cut by grep, shows the two panics only.
#[test]
fn real_cargo_output_reads_in_both_forms() {
    let owners = [
        "FAILED tests::asserted: src/lib.rs:6:21: assertion `left == right` failed: the \
             landing moved main",
        "FAILED tests::returns: Error: \"db error: FATAL: the database system is in recovery \
             mode\"",
        "FAILED tests::refused: src/lib.rs:4:98: connect to the test database: \"error \
             connecting to server: Connection refused (os error 111)\"",
    ];
    let excerpt = [
        "tests::asserted --- FAILED",
        "tests::returns --- FAILED",
        "tests::refused --- FAILED",
        "",
        "failures:",
        "",
        "---- tests::asserted stdout ----",
        "",
        "thread 'tests::asserted' (2819485) panicked at src/lib.rs:6:21:",
        "assertion `left == right` failed: the landing moved main",
        "  left: 1",
        " right: 2",
        "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
        "",
        "--",
        "thread 'tests::refused' (2819487) panicked at src/lib.rs:4:98:",
        "connect to the test database: \"error connecting to server: Connection refused (os \
             error 111)\"",
        "",
        "",
        "failures:",
        "    tests::asserted",
        "--",
        "test result: FAILED. 1 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in 0.00s",
        "",
        "error: test failed, to rerun pass `--lib`",
    ];
    let both: Vec<&str> = owners.iter().chain(excerpt.iter()).copied().collect();
    let failures = host::check_tests_failures(&gate_output(&both));
    assert_eq!(
        tests_of(&failures),
        ["tests::asserted", "tests::returns", "tests::refused"]
    );
    let classes: Vec<Class> = failures
        .iter()
        .map(|failure| host::classify(&failure.message))
        .collect();
    assert_eq!(classes, [Class::Work, Class::Connection, Class::Connection]);
    let failures = host::check_tests_failures(&gate_output(&excerpt));
    assert_eq!(tests_of(&failures), ["tests::asserted", "tests::refused"]);
    assert_eq!(
        failures[0].message,
        "assertion `left == right` failed: the landing moved main\n  left: 1\n right: 2"
    );
    assert_eq!(host::classify(&failures[1].message), Class::Connection);
}

/// A test that returned an error prints `Error: ...` under its stdout
/// header: a failure of that test, with that line as its message.
#[test]
fn an_error_line_is_the_failure_of_the_test_that_returned_it() {
    let lines = [
        "---- api::tests::returns_its_error stdout ----",
        "Error: Db(Error { kind: Db, cause: Some(DbError { code: SqlState(E57P01) }) })",
        "",
        "--",
        "Error: \"no header\"",
        "test result: FAILED. 9 passed; 2 failed; 0 ignored",
    ];
    let failures = host::check_tests_failures(&gate_output(&lines));
    assert_eq!(
        failures,
        [
            failure("api::tests::returns_its_error", lines[1]),
            failure("an unnamed test", lines[4]),
        ]
    );
    assert_eq!(host::classify(&failures[0].message), Class::Connection);
    assert_eq!(host::classify(&failures[1].message), Class::Work);
}

/// Every connection pattern makes a failure the host's, in any case,
/// with no log needed; a database error is not one.
#[test]
fn every_connection_pattern_is_the_hosts() {
    assert_eq!(host::CONNECTION_ERRORS.len(), 12);
    for pattern in host::CONNECTION_ERRORS {
        for message in [
            format!("connect to the test database: {pattern} (os error 111)"),
            format!(
                "called `Result::unwrap()`: {}",
                pattern.to_ascii_uppercase()
            ),
        ] {
            assert_eq!(host::classify(&message), Class::Connection, "{message}");
            let line = format!("FAILED api::tests::one: {message}");
            let fault =
                host::host_fault(host::CHECK_TESTS_REGRESSION, &gate_output(&[&line]), || {
                    panic!("connection errors need no log")
                })
                .unwrap_or_else(|| panic!("{pattern}: not a host fault"));
            assert_eq!(fault.evidence(), "failures");
            assert!(fault.crash.is_empty());
            assert_eq!(fault.failures_value()[0]["class"], "connection");
        }
    }
    assert_eq!(host::classify(LOST_OID), Class::Database);
    assert_eq!(host::classify(LOST_CACHE), Class::Database);
    assert_eq!(host::classify(ASSERTION), Class::Work);
    assert_eq!(host::classify("test timed out after 60s"), Class::Work);
}

/// A real failure among connection errors keeps the rejection, also
/// when only its FAILED line shows it; and no failure read, or another
/// reason, is judged as before. The test database's log is not read.
#[test]
fn a_real_failure_among_connection_errors_stays_rejected() {
    let asked = Cell::new(false);
    let crash = || {
        asked.set(true);
        vec![
            "2026-11-06 01:46:58.017 UTC [1] LOG:  server process (PID 9) was terminated \
                  by signal 11"
                .to_string(),
        ]
    };
    let mut owners = vec![
        format!("FAILED api::tests::claims_in_order: {REFUSED}"),
        format!("FAILED control::tests::the_fold_replays: {CRASHED}"),
        format!("FAILED integrate::tests::a_landing_moves_main_once: {ASSERTION}"),
    ];
    owners.extend(panic_lines("api::tests::claims_in_order", &[REFUSED]));
    owners.extend(panic_lines("control::tests::the_fold_replays", &[CRASHED]));
    let reason = host::CHECK_TESTS_REGRESSION;
    assert_eq!(host::host_fault(reason, &output_of(&owners), crash), None);
    let mut today = panic_lines("api::tests::claims_in_order", &[REFUSED]);
    today.extend(panic_lines(
        "merges::tests::a_conflict",
        &[ASSERTION, "  left: 1"],
    ));
    assert_eq!(host::host_fault(reason, &output_of(&today), crash), None);
    assert!(!asked.get(), "a real failure needs no log");

    // Nothing read: a rejection, as today.
    let compile = gate_output(&["error[E0425]: cannot find value `x` in this scope"]);
    assert_eq!(host::host_fault(reason, &compile, crash), None);
    // Another reason is judged as today, whatever the output holds.
    let refused = gate_output(&[&format!("FAILED api::tests::one: {REFUSED}")]);
    assert_eq!(
        host::host_fault(
            "regression: check-e2e.sh fails on the candidate",
            &refused,
            crash
        ),
        None
    );
    assert_eq!(
        host::host_fault("scenario fails on the candidate", &refused, crash),
        None
    );
    assert!(host::host_fault(reason, &refused, crash).is_some());
}

fn at(text: &str) -> i64 {
    spawn_core::ingest::utc_ms(text).expect("a time")
}

/// The test database's log as postgres writes it: an old crash, then
/// one at 01:46:58.017 UTC.
fn server_log() -> String {
    [
        "2026-11-06 00:46:58.000 UTC [1] LOG:  server process (PID 311) was terminated by \
             signal 11: Segmentation fault",
        "2026-11-06 00:46:58.000 UTC [1] LOG:  terminating any other active server processes",
        "2026-11-06 01:40:00.000 UTC [77] ERROR:  relation \"x\" does not exist",
        "2026-11-06 01:46:58.017 UTC [1] LOG:  server process (PID 4242) was terminated by \
             signal 11: Segmentation fault",
        "2026-11-06 01:46:58.017 UTC [1] DETAIL:  Failed process was running: select 1",
        "2026-11-06 01:46:58.018 UTC [1] LOG:  terminating any other active server processes",
        "2026-11-06 01:46:58.020 UTC [1] LOG:  all server processes terminated; reinitializing",
        "\tcontinued statement text that was terminated by signal 9",
    ]
    .join("\n")
}

/// Database errors that are no connection errors are the host's only
/// with a crash in the test database's log within the gate's window
/// (give or take a second): one an hour before keeps the rejection.
#[test]
fn database_errors_are_the_hosts_only_with_a_crash_in_the_window() {
    let log = server_log();
    let crash = host::crash_lines(&log, at("2026-11-06T01:46:40Z"), at("2026-11-06T01:47:10Z"));
    assert_eq!(crash.len(), 3, "{crash:?}");
    assert!(crash[0].contains("(PID 4242) was terminated by signal 11"));
    assert!(crash[2].contains("all server processes terminated; reinitializing"));
    let output = gate_output(&[
        &format!("FAILED merges::tests::a_conflict_is_recorded: {LOST_OID}"),
        &format!("FAILED sources::tests::lists_every_source: {LOST_CACHE}"),
        &format!("FAILED api::tests::claims_in_order: {REFUSED}"),
    ]);
    let reason = host::CHECK_TESTS_REGRESSION;
    let fault = host::host_fault(reason, &output, || crash.clone()).expect("a host fault");
    assert_eq!(fault.evidence(), "server_log");
    assert_eq!(fault.crash, crash);
    let classes: Vec<serde_json::Value> = fault
        .failures_value()
        .as_array()
        .expect("failures")
        .iter()
        .map(|failure| failure["class"].clone())
        .collect();
    assert_eq!(
        classes,
        [json!("database"), json!("database"), json!("connection")]
    );

    // The gate ran an hour after the old crash, and before the new one.
    let before = host::crash_lines(&log, at("2026-11-06T01:30:00Z"), at("2026-11-06T01:46:57Z"));
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(host::host_fault(reason, &output, || before.clone()), None);
    // Today's form, the same.
    let mut today = panic_lines("merges::tests::a_conflict_is_recorded", &[LOST_OID]);
    today.extend(panic_lines(
        "sources::tests::lists_every_source",
        &[LOST_CACHE],
    ));
    assert_eq!(host::host_fault(reason, &output_of(&today), Vec::new), None);
    assert!(host::host_fault(reason, &output_of(&today), || crash.clone()).is_some());

    // A second of slack on either side of the measured window, no more.
    let started = at("2026-11-06T01:46:59.020Z");
    assert_eq!(host::crash_lines(&log, started, started + 5000).len(), 1);
    assert!(host::crash_lines(&log, started + 1, started + 5000).is_empty());
    let ended = at("2026-11-06T01:46:57.017Z");
    assert_eq!(host::crash_lines(&log, ended - 5000, ended).len(), 1);
    assert!(host::crash_lines(&log, ended - 5000, ended - 1).is_empty());
}

/// The log's times: `UTC` (or `GMT`), or a numeric offset, with or
/// without milliseconds; anything else is no time.
#[test]
fn the_logs_times_read_with_utc_and_with_an_offset() {
    let utc = at("2026-11-06T01:46:58.017Z");
    for line in [
        "2026-11-06 01:46:58.017 UTC [1] LOG:  database system is ready",
        "2026-11-06 01:46:58.017 GMT [1] LOG:  x",
        "2026-11-06 10:46:58.017 +09 [1] LOG:  x",
        "2026-11-06 10:46:58.017 +0900 [1] LOG:  x",
        "2026-11-06 07:16:58.017 +05:30 [1] LOG:  x",
        "2026-11-05 22:16:58.017 -0330 [1] LOG:  x",
    ] {
        assert_eq!(host::log_time_ms(line), Some(utc), "{line}");
    }
    assert_eq!(
        host::log_time_ms("2026-11-06 01:46:58 UTC [1] LOG:  x"),
        Some(utc - 17)
    );
    for line in [
        "",
        "\tcontinued statement",
        "2026-11-06 01:46:58.017 CET [1] LOG:  x",
        "2026-11-06 01:46:58.017 +9 [1] LOG:  x",
        "2026-11-06 01:46:58.017",
        "2026-13-06 01:46:58.017 UTC [1] LOG:  x",
        "LOG:  server process (PID 1) was terminated by signal 11",
    ] {
        assert_eq!(host::log_time_ms(line), None, "{line:?}");
    }
    // A crash stamped with an offset counts in the window as its instant.
    let shifted = "2026-11-06 10:46:58.017 +09 [1] LOG:  server process (PID 1) was \
                       terminated by signal 6: Aborted";
    assert_eq!(host::crash_lines(shifted, utc, utc).len(), 1);
}

fn with_host_faults(faults: u32, task: Option<i64>, due: Option<i64>) -> Mission {
    serde_json::from_value(json!({
        "id": 1, "gate": "/g", "accept": "gate", "gate_rejections": 2, "gate_aborts": 2,
        "gate_host_faults": faults, "gate_host_task": task, "gate_host_due_ms": due,
    }))
    .expect("a mission")
}

/// 5 minutes after the first host fault in a row, doubled after each,
/// at most an hour; the first is a setting.
#[test]
fn host_faults_back_off_doubling_to_an_hour() {
    let base = host_fault_base(None);
    assert_eq!(base, Duration::from_mins(5));
    let minutes: Vec<u64> = (0..=7)
        .map(|faults| host_fault_backoff(base, faults).as_secs() / 60)
        .collect();
    assert_eq!(minutes, [0, 5, 10, 20, 40, 60, 60, 60]);
    assert_eq!(
        host_fault_backoff(base, u32::MAX),
        Duration::from_hours(1),
        "no overflow"
    );
    assert_eq!(host_fault_base(Some("5")), Duration::from_secs(5));
    assert_eq!(host_fault_base(Some(" 30 ")), Duration::from_secs(30));
    for bad in ["", "0", "-5", "1.5", "five"] {
        assert_eq!(host_fault_base(Some(bad)), base, "{bad:?}");
    }
    let short = Duration::from_secs(5);
    assert_eq!(host_fault_backoff(short, 1), Duration::from_secs(5));
    assert_eq!(host_fault_backoff(short, 2), Duration::from_secs(10));
}

/// The wait follows the database's clock, the one that stamped the due
/// time, whatever the host's says: a database clock before the due time
/// holds the task even when the host's is long past it (the due time
/// here is in 2001), and one past it lets the gate run even when the
/// host's is before it (the due time here is in 2200).
#[test]
fn a_host_faults_wait_follows_the_databases_clock_not_the_hosts() {
    let task: super::Task = serde_json::from_value(json!({
        "id": 4, "agent_id": "a", "session_id": "s", "state": "reported",
    }))
    .expect("a task");
    let at = |due: i64, database_now: i64| {
        let mut mission = with_host_faults(1, Some(4), Some(due));
        mission.now_ms = Some(database_now);
        mission
    };
    let past = 1_000_000_000_000; // 2001-09-09, long before the host's now
    assert!(super::now_ms() > past + 60_000);
    assert!(
        super::gate_wait(&at(past, past - 60_000), &task).is_some(),
        "the database's clock is before it"
    );
    assert!(
        super::gate_wait(&at(past, past), &task).is_none(),
        "the database's clock is at it"
    );
    let future = 7_258_118_400_000; // 2200-01-01, after the host's now
    assert!(super::now_ms() < future);
    assert!(
        super::gate_wait(&at(future, future + 1), &task).is_none(),
        "the database's clock is past it"
    );
    assert!(super::gate_wait(&at(future, future - 1), &task).is_some());
    // A mission read without the database's clock (never from the store)
    // waits for nothing.
    assert!(super::gate_wait(&with_host_faults(1, Some(4), Some(future)), &task).is_none());
}

/// The task waits until the recorded due time, read from the mission's
/// fold; another task, or no run, does not wait.
#[test]
fn a_host_fault_holds_its_task_until_its_due_time() {
    let due = 10_000_000;
    let waiting = with_host_faults(1, Some(4), Some(due));
    assert_eq!(
        host_wait(&waiting, 4, due - 4_500),
        Some(Duration::from_millis(4_500))
    );
    assert_eq!(host_wait(&waiting, 4, due), None, "due: judged again");
    assert_eq!(host_wait(&waiting, 4, due + 1), None);
    assert_eq!(host_wait(&waiting, 5, 0), None, "another task's");
    assert_eq!(
        host_wait(&with_host_faults(0, Some(4), Some(due)), 4, 0),
        None,
        "the run ended"
    );
    assert_eq!(host_wait(&with_host_faults(2, Some(4), None), 4, 0), None);
}

fn connection_fault() -> host::HostFault {
    host::host_fault(
        host::CHECK_TESTS_REGRESSION,
        &gate_output(&[
            &format!("FAILED api::tests::claims_in_order: {REFUSED}"),
            &format!("FAILED control::tests::the_fold_replays: {CRASHED}"),
        ]),
        Vec::new,
    )
    .expect("a host fault")
}

/// The record: the reason naming the evidence, the failures, the count,
/// the backoff, and `escalate` at the second in a row only.
#[test]
fn a_host_fault_escalates_only_at_the_second_in_a_row() {
    let fault = connection_fault();
    let escalations: Vec<bool> = (1..=5)
        .map(|faults| {
            host_fault_fields(&fault, faults, Duration::from_secs(5), "log")["escalate"]
                .as_bool()
                .expect("escalate")
        })
        .collect();
    assert_eq!(escalations, [false, true, false, false, false]);
    let fields = host_fault_fields(&fault, 1, Duration::from_mins(5), "the gate's log");
    assert_eq!(
        fields["reason"],
        "host fault: every failure check-tests.sh printed (2) is a connection error to the \
             test database; not a verdict on the work, judged again in 5 min"
    );
    assert_eq!(fields["evidence"], "failures");
    assert_eq!(fields["host_faults"], 1);
    assert_eq!(fields["backoff_ms"], 300_000);
    assert_eq!(fields["log"], "the gate's log");
    assert!(fields.get("crash").is_none() && fields.get("due_ms").is_none());
    assert_eq!(
        fields["failures"][0],
        json!({"test": "api::tests::claims_in_order", "message": REFUSED,
               "class": "connection"})
    );

    // By the log: the crash lines named, the first in the reason.
    let crash = vec![
        "2026-11-06 01:46:58.017 UTC [1] LOG:  server process (PID 4242) was \
                          terminated by signal 11: Segmentation fault"
            .to_string(),
    ];
    let fault = host::host_fault(
        host::CHECK_TESTS_REGRESSION,
        &gate_output(&[&format!("FAILED merges::tests::a: {LOST_OID}\nwith more")]),
        || crash.clone(),
    )
    .expect("a host fault");
    let fields = host_fault_fields(&fault, 2, Duration::from_secs(10), "");
    assert_eq!(fields["evidence"], "server_log");
    assert_eq!(fields["crash"], json!(crash));
    assert_eq!(fields["escalate"], true);
    assert_eq!(fields["backoff_ms"], 10_000);
    assert!(
        fields["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with(
                "host fault: the test database's log shows a crash during the gate (2026-11-06 \
                 01:46:58.017 UTC [1] LOG:  server process (PID 4242) was terminated by signal \
                 11"
            ) && reason
                .ends_with("is a database error; not a verdict on the work, judged again in 10 s")),
        "{fields}"
    );
    // A long message is bounded, its first line only.
    let long = "x".repeat(1000);
    assert_eq!(host::first_line(&format!("{long}\nsecond")).len(), 300);
}

/// No halt rule counts host faults, however many in a row.
#[test]
fn no_number_of_host_faults_halts_a_mission() {
    for faults in [1, 2, 3, 10, 1000, u32::MAX] {
        let mission = with_host_faults(faults, Some(4), Some(0));
        assert_eq!(halt_reason(&mission, 7), None, "{faults}");
    }
}

/// The reason and the note's first line of the record of a rejection for
/// `reason`, with gate output `log`.
fn rejected(reason: &str, log: &str) -> (String, String) {
    let record = super::rejected_fields(reason, log);
    let note = record["note"].as_str().unwrap_or_default();
    (
        record["reason"].as_str().unwrap_or_default().to_string(),
        note.lines().next().unwrap_or_default().to_string(),
    )
}

#[test]
fn a_check_tests_rejection_names_each_failed_test() {
    let log = gate_output(&[
        "FAILED bb::alpha::adds_up: assertion failed",
        "FAILED bb::beta::keeps_order: left != right",
    ]);
    let (reason, note) = rejected(host::CHECK_TESTS_REGRESSION, &log);
    let expected = "check-tests.sh failed: bb::alpha::adds_up; check-tests.sh failed: \
                    bb::beta::keeps_order (regression: check-tests.sh fails on the candidate)";
    assert_eq!(reason, expected);
    assert_eq!(note, format!("The gate rejected this work: {expected}."));
    let record = super::rejected_fields(host::CHECK_TESTS_REGRESSION, &log);
    assert_eq!(record["log"], json!(log));
    assert!(record["note"].as_str().unwrap_or_default().contains(
        "{\"verdict\":\"rejected\",\"reason\":\"regression: check-tests.sh fails on the \
             candidate\"}"
    ));
}

#[test]
fn a_check_tests_rejection_names_five_tests_and_counts_the_rest() {
    let lines: Vec<String> = (1..=7)
        .map(|n| format!("FAILED bb::t::case_{n}: boom"))
        .collect();
    let (reason, _) = rejected(host::CHECK_TESTS_REGRESSION, &output_of(&lines));
    let named: Vec<String> = (1..=5)
        .map(|n| format!("check-tests.sh failed: bb::t::case_{n}"))
        .collect();
    assert_eq!(
        reason,
        format!(
            "{}; and 2 more (regression: check-tests.sh fails on the candidate)",
            named.join("; ")
        )
    );
}

#[test]
fn a_check_tests_rejection_without_failed_lines_names_the_excerpts_test() {
    let (reason, _) = rejected(
        host::CHECK_TESTS_REGRESSION,
        &output_of(&panic_lines("t", &[ASSERTION])),
    );
    assert_eq!(
        reason,
        "check-tests.sh failed: t (regression: check-tests.sh fails on the candidate)"
    );
}

#[test]
fn another_reason_or_an_output_naming_no_test_stays_as_it_is() {
    let failing = gate_output(&["FAILED bb::alpha::adds_up: assertion failed"]);
    let other = "regression: 046.sh fails on the candidate";
    assert_eq!(rejected(other, &failing).0, other);
    let silent = gate_output(&["error: could not compile `spawn-cli`"]);
    assert_eq!(
        rejected(host::CHECK_TESTS_REGRESSION, &silent).0,
        host::CHECK_TESTS_REGRESSION
    );
}

/// What gate.sh prints when a scenario's run decides a rejection: a build,
/// the sections before (each with its stage line), the deciding section's
/// opening line, its lines, its stage line and the closing total.
fn scenario_output(before: &[&str], phase: &str, scenario: &str, lines: &[&str]) -> String {
    let mut all: Vec<String> = [
        "== candidate: build",
        "| build: ok",
        "| stage gate ok 900ms candidate: build",
    ]
    .iter()
    .chain(before)
    .map(ToString::to_string)
    .collect();
    all.push(format!("| == {phase}: {scenario} must pass"));
    all.extend(lines.iter().map(|line| format!("| {line}")));
    all.push(format!(
        "| stage gate rejected 2323ms {phase}: {scenario} must pass"
    ));
    all.push("| stage gate rejected 5000ms total".to_string());
    all.push(
        "{\"verdict\":\"rejected\",\"reason\":\"scenario fails on the candidate\"}".to_string(),
    );
    all.push(String::new());
    all.join("\n")
}

#[test]
fn a_scenario_rejection_names_its_first_failed_check() {
    let reason = "scenario fails on the candidate";
    let log = scenario_output(
        &[
            "| == regression: 012.sh must still pass",
            "| blackbox: (9) not the deciding run's line",
            "| stage gate ok 700ms regression: 012.sh must still pass",
        ],
        "candidate",
        "149.sh",
        &[
            "149: (1) checked: made-up",
            "blackbox: ",
            "blackbox: (2) the checkout has no made-up.service",
            "149: the stand-in's last requests:",
        ],
    );
    let expected =
        "149.sh failed: (2) the checkout has no made-up.service (scenario fails on the candidate)";
    let (got, note) = rejected(reason, &log);
    assert_eq!(got, expected);
    assert_eq!(note, format!("The gate rejected this work: {expected}."));
    let record = super::rejected_fields(reason, &log);
    assert_eq!(
        record["failed"],
        json!({"scenario": "149.sh", "first": "(2) the checkout has no made-up.service", "count": 1})
    );
    assert_eq!(record["log"], json!(log));
}

#[test]
fn a_scenario_rejection_counts_the_checks_after_the_first() {
    let reason = "regression: 092.sh fails on the candidate";
    let log = scenario_output(
        &[],
        "regression",
        "092.sh",
        &[
            "blackbox: (1) one",
            "blackbox: (2) two",
            "blackbox: (3) three",
        ],
    );
    assert_eq!(
        rejected(reason, &log).0,
        "092.sh failed: (1) one; and 2 more (regression: 092.sh fails on the candidate)"
    );
    let record = super::rejected_fields(reason, &log);
    assert_eq!(record["failed"]["count"], json!(3));
    assert_eq!(record["failed"]["first"], json!("(1) one"));
}

#[test]
fn a_quoted_check_is_masked_and_cut() {
    let token = "ghp_169madeup0123456789abcdefghijklmnopqrs";
    let password = "hunter169hunter169";
    let line = format!(
        "blackbox: (3) curl -H 'Authorization: Bearer {token}' failed with PGPASSWORD={password} set"
    );
    let reason = "regression: 092.sh fails on the candidate";
    let log = scenario_output(&[], "regression", "092.sh", &[&line]);
    let record = super::rejected_fields(reason, &log);
    for text in [&record["reason"], &record["failed"]["first"]] {
        let text = text.as_str().unwrap_or_default();
        assert!(text.contains("[redacted]"), "{text}");
        assert!(!text.contains(token) && !text.contains(password), "{text}");
    }
    let accents = "é".repeat(500);
    let cut = scenario_output(
        &[],
        "regression",
        "093.sh",
        &[&format!("blackbox: {accents}")],
    );
    let checks = host::rejected_checks(&cut).unwrap_or_else(|| panic!("no check"));
    assert_eq!(checks.first, "é".repeat(host::CHECK_CHARS));
    assert_eq!(checks.first.chars().count(), 200);
    assert_eq!(checks.count, 1);
}

#[test]
fn a_run_that_names_no_check_is_as_before() {
    let reason = "regression: 046.sh fails on the candidate";
    let silent = scenario_output(&[], "regression", "046.sh", &["046: made-up failure"]);
    let record = super::rejected_fields(reason, &silent);
    assert_eq!(record["reason"], json!(reason));
    assert!(record.get("failed").is_none());
    let unnamed = "| == regression: 046.sh must still pass\n| blackbox: (1) a check\n\
                   | stage gate rejected 5000ms total\n";
    let record = super::rejected_fields(reason, unnamed);
    assert_eq!(record["reason"], json!(reason));
    assert!(record.get("failed").is_none());
    assert_eq!(host::rejected_checks(unnamed), None);
}

#[test]
fn a_check_tests_rejection_records_its_first_test_and_the_count() {
    let lines: Vec<String> = (1..=7)
        .map(|n| format!("FAILED bb::t::case_{n}: boom"))
        .collect();
    let record = super::rejected_fields(host::CHECK_TESTS_REGRESSION, &output_of(&lines));
    assert_eq!(
        record["failed"],
        json!({"scenario": "check-tests.sh", "first": "bb::t::case_1", "count": 7})
    );
    let named: Vec<String> = (1..=5)
        .map(|n| format!("check-tests.sh failed: bb::t::case_{n}"))
        .collect();
    assert_eq!(
        record["reason"],
        json!(format!(
            "{}; and 2 more (regression: check-tests.sh fails on the candidate)",
            named.join("; ")
        ))
    );
}

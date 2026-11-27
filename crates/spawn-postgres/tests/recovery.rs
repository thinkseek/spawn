//! The recovery's records (objective 028, docs/fleet-architecture.md §3.1,
//! "the recovery"): `spawn_host_record` accepts an interruption and a clear
//! with their fields, from the owner only, and refuses a cause or a path it
//! does not know — a record no reader could act on is never written.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, store};

/// The control records of `kind` in `client`'s schema, oldest first.
fn records(client: &mut Client, kind: &str) -> Vec<Value> {
    client
        .query(
            "select kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control' and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = $1
              order by session_seq",
            &[&kind],
        )
        .expect("read the control records")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// What a refused record's database error says: the message the function
/// raised, not the client's one-line summary of it.
fn refused(error: &postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |error| error.message().to_string())
}

/// An interruption of `execution`, as the recovery writes it.
fn interruption(execution: &str, cause: &str, path: &str, due: &Value) -> Value {
    json!({
        "type": "execution.interrupted",
        "agent_id": "agent_worker_recovery",
        "session_id": "session_worker_recovery",
        "execution_id": execution,
        "cause": cause,
        "evidence": {"exit_code": 0},
        "path": path,
        "attempt": 1,
        "due_ms": due.clone(),
        "fact_event_id": 7,
    })
}

#[test]
fn the_record_function_takes_an_interruption_and_a_clear_from_the_owner_only() {
    let (_store, schema) = store("recovery_records");
    let mut client = admin(&schema);
    // An interruption through the owner's function, with its fields.
    for (cause, path, due) in [
        ("crashed", "resume", json!(1_000)),
        ("context_exhausted", "handoff", json!(1_000)),
        ("model_unavailable", "wait", json!(2_000)),
        ("database_unavailable", "wait", json!(1_000)),
        ("disk_full", "wait", json!(1_000)),
        ("stopped", "final", Value::Null),
    ] {
        let id: i64 = client
            .query_one(
                "select spawn_host_record($1)",
                &[&interruption("exec_1", cause, path, &due)],
            )
            .expect("the owner records an interruption")
            .get(0);
        assert!(id > 0);
    }
    let written = records(&mut client, "execution.interrupted");
    assert_eq!(written.len(), 6, "{written:?}");
    assert_eq!(written[0]["cause"], "crashed");
    assert_eq!(written[0]["path"], "resume");
    assert_eq!(
        written[5]["due_ms"],
        Value::Null,
        "a final path is due never"
    );

    // The clear of a wait, with its fields.
    let cleared = json!({
        "type": "execution.cleared",
        "execution_id": "exec_1",
        "session_id": "session_worker_recovery",
        "cause": "model_unavailable",
        "evidence": {"answer_event_id": 71},
        "waited_ms": 4_000,
    });
    client
        .query_one("select spawn_host_record($1)", &[&cleared])
        .expect("the owner records a clear");
    assert_eq!(records(&mut client, "execution.cleared").len(), 1);

    // A restricted login writes none of it: these records are the owner's.
    // Logged in as the login, not `set role`, which needs a superuser.
    let mut restricted = common::as_login(&schema, "spawn_supervisor");
    let refused = restricted.query_one(
        "select spawn_host_record($1)",
        &[&interruption("exec_1", "crashed", "resume", &json!(1_000))],
    );
    assert!(refused.is_err(), "a restricted login records no host facts");
    assert_eq!(
        records(&mut client, "execution.interrupted").len(),
        6,
        "the refusal wrote nothing"
    );
}

#[test]
fn the_record_function_refuses_a_cause_or_a_path_it_does_not_know() {
    let (_store, schema) = store("recovery_refused");
    let mut client = admin(&schema);
    // A cause outside the vocabulary: the recovery could not act on it.
    let error = client
        .query_one(
            "select spawn_host_record($1)",
            &[&interruption(
                "exec_1",
                "went_wrong",
                "resume",
                &json!(1_000),
            )],
        )
        .expect_err("an unknown cause is refused");
    assert!(
        refused(&error).contains("not an interruption cause"),
        "{error}"
    );
    // So is a path.
    let error = client
        .query_one(
            "select spawn_host_record($1)",
            &[&interruption(
                "exec_1",
                "crashed",
                "retry-forever",
                &json!(1_000),
            )],
        )
        .expect_err("an unknown path is refused");
    assert!(refused(&error).contains("not a recovery path"), "{error}");
    // A revival with no due time, and a final one that claims one.
    assert!(
        client
            .query_one(
                "select spawn_host_record($1)",
                &[&interruption("exec_1", "crashed", "resume", &Value::Null)],
            )
            .is_err(),
        "a revival is due at due_ms"
    );
    assert!(
        client
            .query_one(
                "select spawn_host_record($1)",
                &[&interruption("exec_1", "stopped", "final", &json!(1_000))],
            )
            .is_err(),
        "a final path is due never"
    );
    // A clear names its execution, cause and waited_ms.
    let mut clear = json!({
        "type": "execution.cleared",
        "execution_id": "exec_1",
        "session_id": "session_worker_recovery",
        "cause": "disk_full",
        "evidence": {},
        "waited_ms": 1,
    });
    clear["waited_ms"] = Value::Null;
    assert!(
        client
            .query_one("select spawn_host_record($1)", &[&clear])
            .is_err(),
        "a clear names waited_ms"
    );
    assert!(
        records(&mut client, "execution.interrupted").is_empty(),
        "nothing refused was written"
    );
    assert!(records(&mut client, "execution.cleared").is_empty());
}

/// Objective 39: `model_error` is a cause the record function accepts, and
/// every final model record (`model_error`, or `model_unavailable` with its
/// retries spent) is read by the dispatcher until a digest carrying its tag
/// `<session> model-<execution>` reached the supervisor: told once.
#[test]
fn a_final_model_record_is_read_until_a_digest_tells_it() {
    let (store, schema) = store("recovery_model_finals");
    let mut client = admin(&schema);
    let mut refused = interruption("exec_r", "model_error", "final", &Value::Null);
    refused["evidence"] = json!({"event_id": 11, "status": 400, "error": "invalid_request",
                                 "text": "API Error: 400 invalid"});
    let mut spent = interruption("exec_s", "model_unavailable", "final", &Value::Null);
    spent["evidence"] = json!({"event_id": 12, "status": 529, "error": "overloaded",
                               "text": "API Error: 529", "retries": 2});
    for record in [
        &refused,
        &spent,
        // Neither a wait nor another cause's final record is told.
        &interruption("exec_w", "model_unavailable", "wait", &json!(2_000)),
        &interruption("exec_x", "stopped", "final", &Value::Null),
    ] {
        client
            .query_one("select spawn_host_record($1)", &[record])
            .expect("the owner records it");
    }
    let finals = store.model_finals().expect("the final model records");
    let finals = finals.as_array().expect("a list");
    assert_eq!(finals.len(), 2, "{finals:?}");
    assert_eq!(finals[0]["execution_id"], "exec_r");
    assert_eq!(finals[0]["cause"], "model_error");
    assert_eq!(finals[0]["status"], 400);
    assert_eq!(finals[0]["session_id"], "session_worker_recovery");
    assert_eq!(finals[1]["execution_id"], "exec_s");
    assert_eq!(finals[1]["retries"], 2);
    client
        .execute(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, created_at_ms)
             values ('spawn-dispatch', 'digest-1', 'agent_supervisor', 'session_supervisor',
                     jsonb_build_object('type', 'input', 'text', 'x',
                                        'tags', jsonb_build_array($1::text)),
                     'finished', true, 1)",
            &[&"session_worker_recovery model-exec_r"],
        )
        .expect("a digest that told it");
    let finals = store.model_finals().expect("the final model records");
    assert_eq!(finals.as_array().map(Vec::len), Some(1), "{finals}");
    assert_eq!(finals[0]["execution_id"], "exec_s");
}

/// The recovery reads the terminal server an execution's terminal started
/// on (`execution.terminal_started`, the Bridge's record after the start)
/// with the execution's facts: the server that ran it, which a host restart
/// is told by (objective 44). An execution with none recorded reads null.
#[test]
fn the_facts_name_the_terminal_server_the_execution_started_on() {
    use spawn_core::event::{ENGINE, EventKind, ModelProfile, NewEvent, Source};
    use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
    use spawn_core::port::{EventLog, TerminalServer};

    let (store, _schema) = store("recovery_server");
    let session = SessionId::new("session_worker_server").expect("valid session");
    let append = |execution: u64, key: &str, kind: EventKind| {
        store
            .append(NewEvent {
                agent_id: AgentId::new("agent_worker_server").expect("valid agent"),
                session_id: session.clone(),
                execution_id: Some(ExecutionId::for_command(CommandId(execution))),
                source: Source::Bridge,
                source_event_id: Some(key.to_string()),
                kind,
            })
            .expect("append");
    };
    let started = |execution: u64| EventKind::ExecutionStarted {
        engine: ENGINE.to_string(),
        workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
        command_id: CommandId(execution),
        fork: None,
        model: ModelProfile::Default,
        transcript_from: None,
        launch: None,
    };
    let server = TerminalServer {
        pid: 812,
        started_at_ms: 1_794_330_336_000,
    };
    append(1, "exec_1:started", started(1));
    append(
        1,
        "exec_1:terminal_started",
        EventKind::TerminalStarted { server },
    );
    let facts = store.recovery_facts(&session).expect("read the facts");
    assert_eq!(facts["execution"]["id"], "exec_1");
    let read: TerminalServer =
        serde_json::from_value(facts["execution"]["terminal_server"].clone())
            .expect("the server as recorded");
    assert_eq!(read, server);

    // The next execution, with no record of its server (one started before
    // it was kept): null, never the earlier execution's.
    append(
        1,
        "exec_1:ended",
        EventKind::ExecutionEnded {
            cause: spawn_core::event::EndCause::Lost,
            exit_code: None,
            signal: None,
            evidence: None,
        },
    );
    append(2, "exec_2:started", started(2));
    let facts = store.recovery_facts(&session).expect("read the facts");
    assert_eq!(facts["execution"]["id"], "exec_2");
    assert_eq!(facts["execution"]["terminal_server"], Value::Null);
}

fn summarize_latency_ms(samples_us: &[u64]) -> (f64, f64, f64) {
    if samples_us.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let idx = |pct: usize| -> usize { ((n - 1) * pct) / 100 };
    let to_ms = |us: u64| f64::from(u32::try_from(us).unwrap_or(u32::MAX)) / 1000.0;
    (
        to_ms(sorted[idx(50)]),
        to_ms(sorted[idx(95)]),
        to_ms(sorted[n - 1]),
    )
}

fn storm_input_submission(
    agent: &str,
    session: &str,
    key: &str,
    text: &str,
) -> spawn_core::command::Submission {
    use spawn_core::command::{CommandAction, Submission};
    use spawn_core::id::{AgentId, IdempotencyKey, SessionId};

    Submission {
        agent_id: AgentId::new(agent).expect("valid agent"),
        session_id: SessionId::new(session).expect("valid session"),
        idempotency_key: IdempotencyKey::new(key).expect("valid key"),
        action: CommandAction::Input {
            text: text.to_string(),
            tags: Vec::new(),
        },
    }
}

fn run_storm_submit_phase(
    base_store: &spawn_postgres::PostgresStore,
    threads: usize,
    shared_keys: usize,
    unique_per_thread: usize,
) -> (
    Vec<u64>,
    std::collections::BTreeSet<spawn_core::id::CommandId>,
) {
    use spawn_core::id::Principal;
    use spawn_core::port::CommandQueue;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::Instant;

    let principal = Principal::new("spawn-dispatch").expect("valid principal");
    let barrier = Arc::new(Barrier::new(threads));
    let shared_results: Arc<Vec<Mutex<Vec<spawn_core::id::CommandId>>>> =
        Arc::new((0..shared_keys).map(|_| Mutex::new(Vec::new())).collect());
    let all_ids = Arc::new(Mutex::new(BTreeSet::new()));
    let latencies = Arc::new(Mutex::new(Vec::new()));

    std::thread::scope(|scope| {
        for tid in 0..threads {
            let conn = base_store.new_connection().expect("thread connection");
            let barrier = Arc::clone(&barrier);
            let shared_results = Arc::clone(&shared_results);
            let all_ids = Arc::clone(&all_ids);
            let latencies = Arc::clone(&latencies);
            let principal = principal.clone();

            scope.spawn(move || {
                let mut local_lat = Vec::with_capacity(shared_keys + unique_per_thread);
                let mut local_ids = Vec::with_capacity(unique_per_thread + shared_keys);
                barrier.wait();

                // Interleave colliding shared keys and thread-unique keys to stress PG advisory locks.
                for k in 0..shared_keys {
                    let sub = storm_input_submission(
                        &format!("agent_storm_s{k}"),
                        &format!("session_storm_s{k}"),
                        &format!("storm-shared-{k}"),
                        &format!("shared payload {k}"),
                    );
                    let started = Instant::now();
                    let id = conn.submit(&principal, &sub).expect("shared submit");
                    local_lat
                        .push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
                    shared_results[k].lock().expect("shared vec").push(id);
                    local_ids.push(id);
                }

                for u in 0..unique_per_thread {
                    let sub = storm_input_submission(
                        &format!("agent_storm_t{tid}"),
                        &format!("session_storm_t{tid}"),
                        &format!("storm-uniq-t{tid}-{u}"),
                        &format!("unique payload {tid}-{u}"),
                    );
                    let started = Instant::now();
                    let id = conn.submit(&principal, &sub).expect("unique submit");
                    local_lat
                        .push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
                    local_ids.push(id);
                }

                all_ids.lock().expect("all_ids").extend(local_ids);
                latencies.lock().expect("latencies").extend(local_lat);
            });
        }
    });

    for (k, bucket) in shared_results.iter().enumerate() {
        let ids = bucket.lock().expect("bucket");
        assert_eq!(ids.len(), threads, "key {k} must have {threads} results");
        let first = ids[0];
        assert!(
            ids.iter().all(|id| *id == first),
            "key {k} must deduplicate identically across all threads: {ids:?}"
        );
    }

    let lat = latencies.lock().expect("latencies").clone();
    let unique = all_ids.lock().expect("all_ids").clone();
    (lat, unique)
}

fn run_storm_claim_append_finish_phase(
    base_store: &spawn_postgres::PostgresStore,
    threads: usize,
    expected_commands: usize,
) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
    use spawn_core::command::Outcome;
    use spawn_core::event::{EventKind, NewEvent, Source};
    use spawn_core::id::ExecutionId;
    use spawn_core::port::{CommandQueue, EventLog, TerminalServer};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};

    let barrier = Arc::new(Barrier::new(threads));
    let finished = Arc::new(AtomicUsize::new(0));
    let claim_counts: Arc<Mutex<HashMap<spawn_core::id::CommandId, usize>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let claim_lat = Arc::new(Mutex::new(Vec::new()));
    let append_lat = Arc::new(Mutex::new(Vec::new()));
    let finish_lat = Arc::new(Mutex::new(Vec::new()));

    std::thread::scope(|scope| {
        for tid in 0..threads {
            let conn = base_store.new_connection().expect("claim connection");
            let barrier = Arc::clone(&barrier);
            let finished = Arc::clone(&finished);
            let claim_counts = Arc::clone(&claim_counts);
            let claim_lat = Arc::clone(&claim_lat);
            let append_lat = Arc::clone(&append_lat);
            let finish_lat = Arc::clone(&finish_lat);

            scope.spawn(move || {
                let worker = format!("storm-worker-{tid}");
                let deadline = Instant::now() + Duration::from_secs(30);
                barrier.wait();

                while finished.load(Ordering::Acquire) < expected_commands {
                    assert!(
                        Instant::now() < deadline,
                        "storm worker {tid} timed out waiting to drain {expected_commands} commands"
                    );
                    let c_start = Instant::now();
                    let Some(claimed) = conn
                        .claim(&worker, Duration::from_secs(30))
                        .expect("concurrent claim")
                    else {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    };
                    claim_lat
                        .lock()
                        .expect("claim lat")
                        .push(u64::try_from(c_start.elapsed().as_micros()).unwrap_or(u64::MAX));

                    let prev = {
                        let mut counts = claim_counts.lock().expect("counts");
                        let entry = counts.entry(claimed.command_id).or_insert(0);
                        *entry += 1;
                        *entry
                    };
                    assert_eq!(prev, 1, "live command {} claimed twice", claimed.command_id);

                    let event = NewEvent {
                        agent_id: claimed.agent_id.clone(),
                        session_id: claimed.session_id.clone(),
                        execution_id: Some(ExecutionId::for_command(claimed.command_id)),
                        source: Source::Bridge,
                        source_event_id: Some(format!("storm-work-{}", claimed.command_id.0)),
                        kind: EventKind::TerminalStarted {
                            server: TerminalServer {
                                pid: 4000 + u32::try_from(tid).unwrap_or(0),
                                started_at_ms: 1_795_000_000_000,
                            },
                        },
                    };
                    let a_start = Instant::now();
                    let first_app = conn.append(event.clone()).expect("append side effect");
                    append_lat
                        .lock()
                        .expect("append lat")
                        .push(u64::try_from(a_start.elapsed().as_micros()).unwrap_or(u64::MAX));
                    assert!(first_app.inserted, "first append must insert");
                    let dup_app = conn.append(event).expect("idempotent duplicate append");
                    assert!(!dup_app.inserted, "duplicate append must be idempotent");
                    assert_eq!(dup_app.record.position, first_app.record.position);

                    let f_start = Instant::now();
                    conn.finish(&claimed, &Outcome::Succeeded)
                        .expect("finish claimed command");
                    finish_lat
                        .lock()
                        .expect("finish lat")
                        .push(u64::try_from(f_start.elapsed().as_micros()).unwrap_or(u64::MAX));
                    finished.fetch_add(1, Ordering::Release);
                }
            });
        }
    });

    assert_eq!(finished.load(Ordering::Acquire), expected_commands);
    assert_eq!(
        claim_counts.lock().expect("counts").len(),
        expected_commands
    );
    (
        claim_lat.lock().expect("claim lat").clone(),
        append_lat.lock().expect("append lat").clone(),
        finish_lat.lock().expect("finish lat").clone(),
    )
}

fn verify_storm_event_order(client: &mut Client, all_events: &[spawn_core::event::EventRecord]) {
    use std::collections::{HashMap, HashSet};

    let xids_by_id: HashMap<u64, u64> = client
        .query("select id, commit_xid::text from spawn_events", &[])
        .expect("read commit_xids")
        .into_iter()
        .map(|row| {
            let id = u64::try_from(row.get::<_, i64>(0)).expect("positive id");
            let xid: u64 = row.get::<_, String>(1).parse().expect("u64 xid8");
            (id, xid)
        })
        .collect();

    let mut seen_positions = HashSet::with_capacity(all_events.len());
    let mut prev_key: Option<(u64, u64)> = None;
    let mut prev_pos_by_session: HashMap<String, u64> = HashMap::new();
    let mut seq_by_session: HashMap<String, i64> = HashMap::new();
    for rec in all_events {
        let pos = rec.position.0;
        assert!(
            seen_positions.insert(pos),
            "duplicate global position {pos}"
        );
        let xid = *xids_by_id.get(&pos).expect("commit_xid for position");
        let key = (xid, pos);
        if let Some(prev) = prev_key {
            assert!(
                key > prev,
                "global (commit_xid, id) order not strictly monotonic: {key:?} <= {prev:?}"
            );
        }
        prev_key = Some(key);
        let sess = rec.event.session_id.as_str().to_string();
        if let Some(&prev_sess_pos) = prev_pos_by_session.get(&sess) {
            assert!(
                pos > prev_sess_pos,
                "session {sess} event position not monotonic: {pos} <= {prev_sess_pos}"
            );
        }
        prev_pos_by_session.insert(sess.clone(), pos);
        let entry = seq_by_session.entry(sess).or_insert(0);
        *entry += 1;
        assert_eq!(
            i64::try_from(rec.session_seq.0).expect("seq fits i64"),
            *entry,
            "session seq must arrive in dense 1..=count order"
        );
    }
    for (sess, count) in seq_by_session {
        let row = client
            .query_one(
                "select count(*), min(session_seq), max(session_seq) from spawn_events where session_id = $1",
                &[&sess],
            )
            .expect("session seq check");
        let n: i64 = row.get(0);
        let min_seq: i64 = row.get(1);
        let max_seq: i64 = row.get(2);
        assert_eq!(n, count, "session {sess} row count mismatch");
        assert_eq!(min_seq, 1, "session {sess} min seq must be 1");
        assert_eq!(max_seq, count, "session {sess} max seq must equal count");
    }
}

#[test]
fn pg_concurrency_idempotency_replay_storm_claim_contention_and_event_monotonicity() {
    use spawn_core::event::GlobalPosition;
    use spawn_core::port::EventLog;

    const THREADS: usize = 16;
    const SHARED_KEYS: usize = 8;
    const UNIQUE_PER_THREAD: usize = 16;
    const EXPECTED_UNIQUE: usize = SHARED_KEYS + (THREADS * UNIQUE_PER_THREAD);

    let (base_store, schema) = store("chaos_idempotency_storm");
    let (submit_us, unique_ids) =
        run_storm_submit_phase(&base_store, THREADS, SHARED_KEYS, UNIQUE_PER_THREAD);
    assert_eq!(unique_ids.len(), EXPECTED_UNIQUE);

    let mut client = admin(&schema);
    let (cmd_rows, distinct_keys, accepted_events): (i64, i64, i64) = client
        .query_one(
            "select
               (select count(*) from spawn_commands),
               (select count(*) from (select distinct principal, idempotency_key from spawn_commands) d),
               (select count(*) from spawn_events where event_name = 'command.accepted')",
            &[],
        )
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .expect("sql counts after submit storm");
    let expected_i64 = i64::try_from(EXPECTED_UNIQUE).expect("fits i64");
    assert_eq!(cmd_rows, expected_i64);
    assert_eq!(distinct_keys, expected_i64);
    assert_eq!(accepted_events, expected_i64);

    let (claim_us, append_us, finish_us) =
        run_storm_claim_append_finish_phase(&base_store, THREADS, EXPECTED_UNIQUE);

    let unfinished: i64 = client
        .query_one(
            "select count(*) from spawn_commands where state <> 'finished' or not succeeded or lease_attempt <> 1",
            &[],
        )
        .expect("unfinished count")
        .get(0);
    assert_eq!(unfinished, 0, "all commands must finish on attempt 1");

    let mut all_events = Vec::new();
    let mut cursor = GlobalPosition(0);
    loop {
        let page = base_store.read_global(cursor, 500).expect("read_global");
        if page.is_empty() {
            break;
        }
        cursor = page.last().expect("non-empty").position;
        all_events.extend(page);
    }
    assert_eq!(all_events.len(), EXPECTED_UNIQUE * 3);

    verify_storm_event_order(&mut client, &all_events);

    let (s50, s95, smax) = summarize_latency_ms(&submit_us);
    let (c50, c95, cmax) = summarize_latency_ms(&claim_us);
    let (a50, a95, amax) = summarize_latency_ms(&append_us);
    let (f50, f95, fmax) = summarize_latency_ms(&finish_us);
    eprintln!(
        "N252_METRIC pg_replay_storm threads={THREADS} submits={} unique_cmds={EXPECTED_UNIQUE} events={} \
         submit_ms=({s50:.2}/{s95:.2}/{smax:.2}) claim_ms=({c50:.2}/{c95:.2}/{cmax:.2}) \
         append_ms=({a50:.2}/{a95:.2}/{amax:.2}) finish_ms=({f50:.2}/{f95:.2}/{fmax:.2})",
        THREADS * (SHARED_KEYS + UNIQUE_PER_THREAD),
        all_events.len()
    );
}

fn crash_side_effect_event(
    claimed: &spawn_core::command::Claimed,
    tag: &str,
) -> spawn_core::event::NewEvent {
    use spawn_core::event::{EventKind, NewEvent, Source};
    use spawn_core::id::ExecutionId;
    use spawn_core::port::TerminalServer;

    NewEvent {
        agent_id: claimed.agent_id.clone(),
        session_id: claimed.session_id.clone(),
        execution_id: Some(ExecutionId::for_command(claimed.command_id)),
        source: Source::Bridge,
        source_event_id: Some(format!("chaos-side-effect-{tag}-{}", claimed.command_id.0)),
        kind: EventKind::TerminalStarted {
            server: TerminalServer {
                pid: 9001,
                started_at_ms: 1_795_000_100_000,
            },
        },
    }
}

fn run_ttl_expiry_takeover_subscenario(
    base_store: &spawn_postgres::PostgresStore,
    conn_reconnected: &spawn_postgres::PostgresStore,
    principal: &spawn_core::id::Principal,
) -> (usize, f64) {
    use spawn_core::command::Outcome;
    use spawn_core::port::{CommandQueue, EventLog, StoreError};
    use std::time::{Duration, Instant};

    for s in 0..4 {
        let sub = storm_input_submission(
            &format!("agent_ttl_{s}"),
            &format!("session_ttl_{s}"),
            &format!("ttl-k-{s}"),
            &format!("ttl job {s}"),
        );
        base_store.submit(principal, &sub).expect("submit ttl");
    }
    let conn_hung = base_store.new_connection().expect("hung conn");
    let mut hung_claims = Vec::new();
    while let Some(c) = conn_hung
        .claim("chaos-worker-hung", Duration::from_millis(200))
        .expect("hung claim")
    {
        assert_eq!(c.attempt, 1);
        hung_claims.push(c);
    }
    assert_eq!(hung_claims.len(), 4);
    std::thread::sleep(Duration::from_millis(320));

    let ttl_started = Instant::now();
    let mut ttl_recovered = 0usize;
    while let Some(c) = conn_reconnected
        .claim("chaos-worker-ttl-takeover", Duration::from_secs(30))
        .expect("ttl takeover claim")
    {
        assert_eq!(c.attempt, 2);
        if let Some(stale) = hung_claims.iter().find(|h| h.command_id == c.command_id) {
            assert!(matches!(
                conn_hung.finish(stale, &Outcome::Failed("stale hung".to_owned())),
                Err(StoreError::LeaseLost { .. })
            ));
        }
        let app = conn_reconnected
            .append(crash_side_effect_event(&c, "ttl"))
            .expect("ttl side effect");
        assert!(app.inserted);
        conn_reconnected
            .finish(&c, &Outcome::Succeeded)
            .expect("finish ttl takeover");
        ttl_recovered += 1;
    }
    let ttl_takeover_ms =
        f64::from(u32::try_from(ttl_started.elapsed().as_micros()).unwrap_or(u32::MAX)) / 1000.0;
    assert_eq!(ttl_recovered, 4);
    assert!(
        conn_hung
            .finish(&hung_claims[0], &Outcome::Failed("post finish".to_owned()))
            .is_ok(),
        "idempotent post-finish retry must not mutate finished outcome"
    );
    (ttl_recovered, ttl_takeover_ms)
}

fn verify_crash_recovery_projection_and_log(
    base_store: &spawn_postgres::PostgresStore,
    schema: &str,
) -> i64 {
    use spawn_core::event::GlobalPosition;
    use spawn_core::port::EventLog;

    let mut client = admin(schema);
    let (total_cmds, finished_ok): (i64, i64) = client
        .query_one(
            "select count(*), count(*) filter (where state = 'finished' and succeeded) from spawn_commands",
            &[],
        )
        .map(|row| (row.get(0), row.get(1)))
        .expect("command projection");
    assert_eq!(total_cmds, 12);
    assert_eq!(finished_ok, 12);

    let (accepted_n, terminal_n, finished_n): (i64, i64, i64) = client
        .query_one(
            "select
               count(*) filter (where event_name = 'command.accepted'),
               count(*) filter (where event_name = 'execution.terminal_started'),
               count(*) filter (where event_name = 'command.finished')
             from spawn_events",
            &[],
        )
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .expect("event counts");
    assert_eq!((accepted_n, terminal_n, finished_n), (12, 12, 12));

    let global = base_store
        .read_global(GlobalPosition(0), 200)
        .expect("read_global");
    assert_eq!(global.len(), 36);
    assert!(global.windows(2).all(|w| w[0].position.0 < w[1].position.0));
    total_cmds
}

#[test]
#[expect(clippy::too_many_lines)]
fn pg_worker_crash_lease_expiry_and_reconnect_takeover_zero_duplicate_side_effects() {
    use spawn_core::command::Outcome;
    use spawn_core::id::Principal;
    use spawn_core::port::{CommandQueue, EventLog, StoreError};
    use std::time::{Duration, Instant};

    let (base_store, schema) = store("chaos_crash_takeover");
    let principal = Principal::new("spawn-dispatch").expect("principal");
    for s in 0..4 {
        for j in 0..2 {
            let sub = storm_input_submission(
                &format!("agent_crash_{s}"),
                &format!("session_crash_{s}"),
                &format!("crash-k-{s}-{j}"),
                &format!("crash job {s}-{j}"),
            );
            base_store.submit(&principal, &sub).expect("submit");
        }
    }

    let worker_name = "chaos-worker-sigkill";
    let conn_a = base_store.new_connection().expect("conn_a");
    let conn_b = base_store.new_connection().expect("conn_b");
    let (guard_a, initial_rec) = conn_a.recover_worker(worker_name).expect("guard_a");
    assert_eq!(initial_rec, 0);
    assert!(
        matches!(
            conn_b.recover_worker(worker_name),
            Err(StoreError::Rejected(_))
        ),
        "live worker ownership must fence out concurrent recover_worker"
    );

    let mut held_by_a = Vec::new();
    while let Some(c) = conn_a
        .claim(worker_name, Duration::from_mins(1))
        .expect("claim a")
    {
        assert_eq!(c.attempt, 1);
        let app = conn_a
            .append(crash_side_effect_event(&c, "sigkill"))
            .expect("side effect by a");
        assert!(app.inserted);
        held_by_a.push(c);
    }
    assert_eq!(held_by_a.len(), 4, "one live command per session");

    // Simulate SIGKILL: drop ownership guard and close worker connection with live leases held.
    drop(guard_a);
    drop(conn_a);

    let recover_started = Instant::now();
    let conn_reconnected = base_store.new_connection().expect("reconnected");
    let (_guard_b, recovered) = conn_reconnected
        .recover_worker(worker_name)
        .expect("takeover recover");
    let recover_ms =
        f64::from(u32::try_from(recover_started.elapsed().as_micros()).unwrap_or(u32::MAX))
            / 1000.0;
    assert_eq!(
        recovered, 4,
        "all 4 abandoned live leases recovered immediately"
    );

    let mut b_finished = 0usize;
    while let Some(c) = conn_reconnected
        .claim(worker_name, Duration::from_secs(30))
        .expect("claim b")
    {
        let expected_attempt = if held_by_a.iter().any(|old| old.command_id == c.command_id) {
            2
        } else {
            1
        };
        assert_eq!(c.attempt, expected_attempt);
        if let Some(stale) = held_by_a.iter().find(|old| old.command_id == c.command_id) {
            assert!(matches!(
                conn_reconnected.finish(stale, &Outcome::Failed("stale sigkill".to_owned())),
                Err(StoreError::LeaseLost { .. })
            ));
        }
        let dup_or_new = conn_reconnected
            .append(crash_side_effect_event(&c, "sigkill"))
            .expect("idempotent side effect replay");
        assert_eq!(
            dup_or_new.inserted,
            expected_attempt == 1,
            "recovered attempt 2 must reuse earlier side effect without duplication"
        );
        conn_reconnected
            .finish(&c, &Outcome::Succeeded)
            .expect("finish recovered");
        b_finished += 1;
    }
    assert_eq!(b_finished, 8);
    assert!(
        conn_reconnected
            .finish(
                &held_by_a[0],
                &Outcome::Failed("post finish stale".to_owned())
            )
            .is_ok(),
        "idempotent finish after completion must preserve Succeeded"
    );

    let (ttl_recovered, ttl_takeover_ms) =
        run_ttl_expiry_takeover_subscenario(&base_store, &conn_reconnected, &principal);
    let total_cmds = verify_crash_recovery_projection_and_log(&base_store, &schema);

    eprintln!(
        "N252_METRIC pg_crash_recovery sigkill_recovered={recovered} ttl_recovered={ttl_recovered} \
         total_cmds={total_cmds} recover_ms={recover_ms:.2} ttl_takeover_ms={ttl_takeover_ms:.2}"
    );
}

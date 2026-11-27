//! Migration in place: every start runs `sql/schema.sql` over whatever an
//! earlier install left, so a database from the first shape of every table
//! (tests/first-shapes.sql) must come out as a fresh one does.
//!
//! Views are not covered here: first-shapes.sql holds tables only, so every
//! view is created fresh, and a `create or replace view` that renames or
//! reorders an existing view's columns (42P16) passes. The gate's
//! rsi/scenarios/check-schema-upgrade.sh migrates the base's whole schema to
//! the candidate's for that.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`).

use postgres::Client;
use spawn_core::event::{ENGINE, EventKind, GlobalPosition, ModelProfile, NewEvent, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
use spawn_core::port::{EventLog, StoreError};

mod common;
use common::{admin, first_shapes, migrate_warnings, store, url};
use spawn_postgres::PostgresStore;

/// Every column of the schema's tables, with what decides how a row reads:
/// position, type, nullability, default.
fn columns(client: &mut Client, schema: &str) -> Vec<String> {
    client
        .query(
            "select format('%s.%s #%s %s %s default %s', c.table_name, c.column_name,
                           c.ordinal_position, c.data_type, c.is_nullable,
                           coalesce(c.column_default, '-'))
               from information_schema.columns c
               join information_schema.tables t
                 on t.table_schema = c.table_schema and t.table_name = c.table_name
              where c.table_schema = $1 and t.table_type = 'BASE TABLE'
              order by 1",
            &[&schema],
        )
        .expect("read the columns")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// Every check, key and reference of the schema's tables, without the
/// schema's own name (a definition qualifies names off the search path).
fn constraints(client: &mut Client, schema: &str) -> Vec<String> {
    client
        .query(
            "select format('%s %s %s', r.relname, c.conname, pg_get_constraintdef(c.oid))
               from pg_constraint c
               join pg_class r on r.oid = c.conrelid
               join pg_namespace n on n.oid = r.relnamespace
              where n.nspname = $1 and c.contype in ('c', 'u', 'p', 'f')
              order by 1",
            &[&schema],
        )
        .expect("read the constraints")
        .iter()
        .map(|row| row.get::<_, String>(0).replace(&format!("{schema}."), ""))
        .collect()
}

/// Every index (`create index if not exists` keeps an old definition), and
/// every function by signature, with how it runs.
fn indexes_and_functions(client: &mut Client, schema: &str) -> Vec<String> {
    // Read with neither schema on the search path: pg_get_indexdef leaves a
    // function in an index expression unqualified when its schema is on the
    // reader's search path, so the client's own schema (the upgraded one)
    // printed `spawn_experience_int(…)` and the fresh one
    // `<schema>.spawn_experience_int(…)` for the same index (the first
    // indexes that call a function, 2026-11-11).
    let mut read = client.transaction().expect("a transaction");
    read.batch_execute("set local search_path to pg_catalog")
        .expect("no schema on the search path");
    let rows = read
        .query(
            "select indexdef from pg_indexes where schemaname = $1
             union all
             select format('%s(%s) definer=%s %s', p.proname,
                           pg_get_function_identity_arguments(p.oid), p.prosecdef,
                           array_to_string(p.proconfig, ','))
               from pg_proc p join pg_namespace n on n.oid = p.pronamespace
              where n.nspname = $1
              order by 1",
            &[&schema],
        )
        .expect("read the indexes and functions");
    read.commit().expect("end the read");
    rows.iter()
        .map(|row| row.get::<_, String>(0).replace(schema, "<schema>"))
        .collect()
}

#[test]
fn the_first_shape_of_every_table_migrates_to_the_current_columns() {
    let (schema, mut client) = first_shapes("first_shapes");
    // Rows as the first installs wrote them.
    client
        .batch_execute(
            "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
                 values ('agent_one', 'session_one', 'claude', '{\"name\": \"message.received\"}', 1, 1000);
             insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         created_at_ms)
                 values ('human', 'key_1', 'agent_one', 'session_one', '{\"type\": \"stop\"}', 1000);
             insert into spawn_grants (principal, agent_id) values ('someone', 'agent_one');
             insert into mission_missions (text, origin, created_at_ms, updated_at_ms)
                 values ('Ship it', 'human', 1000, 1000);
             insert into mission_residents (name, agent_id, session_id, charter, proposed_by,
                                            created_at_ms, updated_at_ms)
                 values ('core', 'agent_resident_core', 'session_resident_core', 'Owns the core',
                         'human', 1000, 1000);
             insert into mission_tasks (mission_id, kind, agent_id, session_id, brief, created_by,
                                        created_at_ms, updated_at_ms)
                 values (1, 'resident', 'agent_resident_core', 'session_resident_core', 'Do it',
                         'human', 1000, 1000);",
        )
        .expect("rows of the first installs");

    let upgraded = PostgresStore::connect_in_schema(&url(), &schema).expect("migrate");
    let (_fresh, fresh_schema) = store("first_shapes_fresh");
    assert_eq!(
        columns(&mut client, &schema),
        columns(&mut client, &fresh_schema),
        "every column a fresh schema has, in the same place"
    );
    assert_eq!(
        constraints(&mut client, &schema),
        constraints(&mut client, &fresh_schema),
        "every check a fresh schema has"
    );
    assert_eq!(
        indexes_and_functions(&mut client, &schema),
        indexes_and_functions(&mut client, &fresh_schema),
        "every index and function a fresh schema has, and no other"
    );

    // The rows read through the code: the grant took the default scope, the
    // mission rows moved into the control stream with the columns added
    // since, and a migration again changes nothing.
    let mut client = admin(&schema);
    let scope: String = client
        .query_one(
            "select scope from spawn_grants where principal = 'someone'",
            &[],
        )
        .expect("the grant")
        .get(0);
    assert_eq!(scope, "all");
    let board = upgraded.mission_board(10).expect("the board reads");
    assert_eq!(board["missions"][0]["tasks"][0]["model"], "default");
    assert_eq!(board["missions"][0]["tasks"][0]["state"], "assigned");
    assert!(
        upgraded.delivery_rows().is_ok(),
        "the dispatcher's read works"
    );
    assert!(
        upgraded.gated_missions().is_ok(),
        "the integrator's read works"
    );
    let before = columns(&mut client, &schema);
    upgraded.migrate().expect("migrate again");
    assert_eq!(columns(&mut client, &schema), before);
}

fn exists(client: &mut Client, table: &str) -> bool {
    client
        .query_one("select to_regclass($1) is not null", &[&table])
        .expect("look the table up")
        .get(0)
}

#[test]
fn the_retired_rsi_tables_are_dropped_only_when_nothing_in_them_is_lost() {
    // Empty: dropped without a word.
    let (schema, mut client) = first_shapes("rsi_empty");
    assert!(migrate_warnings(&schema).is_empty());
    for table in ["rsi_attempts", "rsi_iterations", "rsi_state"] {
        assert!(!exists(&mut client, table), "{table} is dropped");
    }

    // Rows never imported: kept, and the migration says so.
    let (schema, mut client) = first_shapes("rsi_rows");
    client
        .batch_execute(
            "insert into rsi_attempts (task_id, attempt, iteration, gate_log, accept_ok, created_at_ms)
                 values (3, 1, 1, 'METRIC x baseline=1 candidate=2', true, 5000);
             insert into rsi_iterations (iteration, task_id, phase, agent_id, session_id, patch,
                                         updated_at_ms)
                 values (1, 3, 'landed', 'agent_resident_rsi', 'session_resident_rsi', 'diff', 5000);",
        )
        .expect("an attempt and its iteration");
    let warnings = migrate_warnings(&schema);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("kept rsi_attempts")
            && warnings[0].contains("1 attempts have no imported record")
            && warnings[0].contains("1 iteration or state rows"),
        "{warnings:?}"
    );
    for table in ["rsi_attempts", "rsi_iterations", "rsi_state"] {
        assert!(exists(&mut client, table), "{table} is kept");
    }
    let kept: String = client
        .query_one("select patch from rsi_iterations", &[])
        .expect("the iteration is still there")
        .get(0);
    assert_eq!(kept, "diff");

    // The attempt imported (as `spawn mission import-rsi` did) but the
    // iteration not: still kept.
    let mission: i64 = client
        .query_one("select spawn_mission_add('RSI', '[]', 'human')", &[])
        .expect("a mission")
        .get(0);
    client
        .execute(
            "select spawn_control_record(jsonb_build_object(
                 'type', 'legacy.imported', 'table', 'mission_tasks', 'row', jsonb_build_object(
                     'id', 90, 'mission_id', $1::bigint, 'kind', 'resident',
                     'agent_id', 'agent_resident_rsi', 'session_id', 'session_resident_rsi',
                     'brief', 'RSI attempt', 'state', 'landed', 'revisions', 0,
                     'created_by', 'rsi', 'created_at_ms', 5000, 'updated_at_ms', 5000,
                     'objective', 3)))",
            &[&mission],
        )
        .expect("the attempt's import record");
    let warnings = migrate_warnings(&schema);
    assert!(
        warnings.len() == 1 && warnings[0].contains("0 attempts have no imported record"),
        "{warnings:?}"
    );
    assert!(exists(&mut client, "rsi_iterations"));

    // Everything accounted for: dropped.
    client
        .batch_execute("delete from rsi_iterations")
        .expect("the iteration goes");
    assert!(migrate_warnings(&schema).is_empty());
    for table in ["rsi_attempts", "rsi_iterations", "rsi_state"] {
        assert!(!exists(&mut client, table), "{table} is dropped");
    }
}

fn insert_raw_kind_json(
    client: &mut Client,
    agent: &AgentId,
    session: &SessionId,
    kind_json: &str,
    seq: i64,
) -> i64 {
    client
        .query_one(
            "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
             values ($1, $2, 'claude', $3::text::jsonb, $4, 2000)
             returning id",
            &[&agent.as_str(), &session.as_str(), &kind_json, &seq],
        )
        .expect("insert raw spawn_events row")
        .get(0)
}

#[test]
fn unknown_future_event_kind_is_skipped_without_stalling_global_reads() {
    let (pg, schema) = store("upgrade_unknown_event_kind");
    let mut client = admin(&schema);
    let agent = AgentId::new("agent_upgrade_unknown").expect("agent");
    let session = SessionId::new("session_upgrade_unknown").expect("session");

    let first = pg
        .append(NewEvent {
            agent_id: agent.clone(),
            session_id: session.clone(),
            execution_id: None,
            source: Source::Claude,
            source_event_id: Some("known-1".to_string()),
            kind: EventKind::MessageReceived,
        })
        .expect("append known 1");

    // Newer-release dotted event kinds stored before a rollback.
    insert_raw_kind_json(
        &mut client,
        &agent,
        &session,
        "{\"name\":\"future.v99_event\",\"detail\":{\"phase\":1,\"nested\":{\"ok\":true}}}",
        2,
    );
    insert_raw_kind_json(
        &mut client,
        &agent,
        &session,
        "{\"name\":\"future.rollback_unit\"}",
        3,
    );

    pg.append(NewEvent {
        agent_id: agent.clone(),
        session_id: session,
        execution_id: None,
        source: Source::Claude,
        source_event_id: Some("known-2".to_string()),
        kind: EventKind::TurnStopped {
            input_tokens: Some(12),
            output_tokens: Some(4),
        },
    })
    .expect("append known 2");

    // Batch fill continues past skipped unknown rows without losing known records.
    let batch = pg
        .read_global(GlobalPosition(0), 2)
        .expect("read_global skips unknown dotted events");
    let names: Vec<&str> = batch
        .iter()
        .map(|record| record.event.kind.name())
        .collect();
    assert_eq!(names, ["message.received", "turn.stopped"]);

    let tail = pg
        .read_global(first.record.position, 10)
        .expect("read_global from cursor skips intervening unknown rows");
    let tail_names: Vec<&str> = tail.iter().map(|record| record.event.kind.name()).collect();
    assert_eq!(tail_names, ["turn.stopped"]);

    let raw_count: i64 = client
        .query_one("select count(*) from spawn_events", &[])
        .expect("count rows")
        .get(0);
    assert_eq!(raw_count, 4);

    // A trailing unknown dotted event on an active session does not break `open_execution`.
    let exec_agent = AgentId::new("agent_upgrade_open_exec").expect("exec agent");
    let exec_session = SessionId::new("session_upgrade_open_exec").expect("exec session");
    pg.append(NewEvent {
        agent_id: exec_agent.clone(),
        session_id: exec_session.clone(),
        execution_id: Some(ExecutionId::new("exec_upgrade_open").expect("exec id")),
        source: Source::Bridge,
        source_event_id: Some("start-1".to_string()),
        kind: EventKind::ExecutionStarted {
            engine: ENGINE.to_string(),
            workspace: WorkspaceId::new("1:/workspaces/1").expect("ws"),
            command_id: CommandId(1),
            fork: None,
            model: ModelProfile::Default,
            transcript_from: None,
            launch: None,
        },
    })
    .expect("append execution.started");
    insert_raw_kind_json(
        &mut client,
        &exec_agent,
        &exec_session,
        "{\"name\":\"future.trailing_note\",\"detail\":{\"step\":1}}",
        2,
    );
    let open = pg
        .open_execution(&exec_session)
        .expect("open_execution stays unaffected")
        .expect("open execution remains visible");
    assert_eq!(open.event.kind.name(), "execution.started");
}

#[test]
fn corrupt_event_kind_rows_fail_closed_with_corrupt_error() {
    let (pg, schema) = store("upgrade_corrupt_event_kind");
    let mut client = admin(&schema);
    let agent = AgentId::new("agent_upgrade_corrupt").expect("agent");
    let session = SessionId::new("session_upgrade_corrupt").expect("session");

    // Syntactically malformed JSON is refused by PostgreSQL's `jsonb` boundary itself.
    let malformed = client.execute(
        "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
         values ($1, $2, 'claude', '{not-json'::jsonb, 1, 2000)",
        &[&agent.as_str(), &session.as_str()],
    );
    assert!(
        malformed.is_err(),
        "PostgreSQL jsonb must reject invalid JSON text"
    );

    let cases = [
        (
            "known event with wrong field type",
            "{\"name\":\"message.typed\",\"detail\":{\"command_id\":\"not-a-number\"}}",
        ),
        (
            "known event missing required detail fields",
            "{\"name\":\"tool.started\",\"detail\":{}}",
        ),
        (
            "known event grading.recorded missing record detail",
            "{\"name\":\"grading.recorded\",\"detail\":{}}",
        ),
        (
            "known event with invalid inner enum variant",
            "{\"name\":\"job.started\",\"detail\":{\"job_id\":\"j1\",\"kind\":\"bogus_kind\"}}",
        ),
        (
            "known event with inner enum variant matching outer name",
            "{\"name\":\"job.started\",\"detail\":{\"job_id\":\"j1\",\"kind\":\"job.started\"}}",
        ),
        ("non-object number", "42"),
        ("non-object array", "[\"message.received\"]"),
        ("non-object string", "\"message.received\""),
        ("non-object boolean", "true"),
        ("non-object null", "null"),
        ("missing name field", "{\"detail\":{}}"),
        ("non-string name field", "{\"name\":123,\"detail\":{}}"),
        ("null name field", "{\"name\":null}"),
        ("empty name", "{\"name\":\"\"}"),
        ("name without dot", "{\"name\":\"nodot\"}"),
        ("leading dot name", "{\"name\":\".leading\"}"),
        ("trailing dot name", "{\"name\":\"trailing.\"}"),
        ("double dot name", "{\"name\":\"a..b\"}"),
        ("uppercase name", "{\"name\":\"Future.Event\"}"),
        ("space/punctuation name", "{\"name\":\"BAD NAME!\"}"),
    ];
    for (idx, (label, kind_json)) in cases.iter().enumerate() {
        let seq = 10 + i64::try_from(idx).expect("seq");
        let id = insert_raw_kind_json(&mut client, &agent, &session, kind_json, seq);
        let decoded = pg.read_global(GlobalPosition(0), 20);
        client
            .execute("delete from spawn_events where id = $1", &[&id])
            .expect("delete corrupt row");
        assert!(
            matches!(decoded, Err(StoreError::Corrupt(_))),
            "{label} ({kind_json}) must return StoreError::Corrupt, got {decoded:?}"
        );
    }
}

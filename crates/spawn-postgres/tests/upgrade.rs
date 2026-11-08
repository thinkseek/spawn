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
    client
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
        .expect("read the indexes and functions")
        .iter()
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

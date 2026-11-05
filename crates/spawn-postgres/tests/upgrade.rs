//! Migration in place: every start runs `sql/schema.sql` over whatever an
//! earlier install left, so a database from the first shape of every table
//! (tests/first-shapes.sql) must come out as a fresh one does.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`).

use postgres::Client;

mod common;
use common::{admin, store, url};
use spawn_postgres::PostgresStore;

const FIRST_SHAPES: &str = include_str!("first-shapes.sql");

/// A schema holding the first shape of every table, and an owner
/// connection in it.
fn first_shapes(case: &str) -> (String, Client) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let schema = format!("spawn_test_{}_{nanos:x}_{case}", std::process::id());
    let mut client = Client::connect(&url(), postgres::NoTls).expect("admin connection");
    client
        .batch_execute(&format!(
            "create schema {schema}; set search_path to {schema}; {FIRST_SHAPES}"
        ))
        .expect("the first shapes");
    (schema, client)
}

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

//! The schema guard: a case that makes its schema some other way than
//! `store` (through `schema_url` alone, as a burst of migrations does)
//! leaves nothing behind once its guard goes.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`).

use postgres::{Client, NoTls};
use spawn_postgres::PostgresStore;

mod common;
use common::{Schema, schema_url, url};

fn exists(schema: &str) -> bool {
    Client::connect(&url(), NoTls)
        .expect("a connection")
        .query_one(
            "select exists (select 1 from pg_namespace where nspname = $1)",
            &[&schema],
        )
        .expect("the catalog")
        .get(0)
}

#[test]
fn a_schema_made_through_its_url_alone_is_dropped_with_its_guard() {
    let schema = Schema::new("guard_url");
    let name = schema.to_string();
    assert!(!exists(&name), "a fresh name names no schema yet");
    PostgresStore::connect_with_options(
        &schema_url(&schema),
        &spawn_postgres::ConnectOptions::default(),
    )
    .expect("connect")
    .migrate()
    .expect("the migration makes the schema");
    assert!(exists(&name), "the migration made it");
    drop(schema);
    assert!(!exists(&name), "the guard dropped it");
}

#[test]
fn several_schemas_of_one_case_are_each_dropped_with_their_guards() {
    let schemas: Vec<Schema> = (0..3)
        .map(|number| Schema::new(&format!("guard_many_{number}")))
        .collect();
    let names: Vec<String> = schemas.iter().map(ToString::to_string).collect();
    for schema in &schemas {
        PostgresStore::connect_with_options(
            &schema_url(schema),
            &spawn_postgres::ConnectOptions::default(),
        )
        .expect("connect")
        .migrate()
        .expect("migrate");
    }
    assert!(names.iter().all(|name| exists(name)), "each was made");
    drop(schemas);
    assert!(
        names.iter().all(|name| !exists(name)),
        "each was dropped: {names:?}"
    );
}

#[test]
fn a_stores_schema_is_dropped_with_its_guard() {
    let (store, schema) = common::store("guard_store");
    let name = schema.to_string();
    assert!(exists(&name));
    drop(store);
    drop(schema);
    assert!(!exists(&name), "the store's guard dropped its schema");
}

struct FunctionContract {
    name: &'static str,
    identity_args: &'static str,
    result_type: &'static str,
    security_definer: bool,
    pinned_search_path: bool,
}

/// Issue #98 Item 2: pin the exact parameter signatures, return types,
/// SECURITY DEFINER/INVOKER mode, search_path pinning, and absence of PUBLIC
/// EXECUTE for every `spawn_human_*` entrypoint/helper and core control /
/// command-completion functions against live PG 16.
const LOCKED_FUNCTION_CONTRACTS: &[FunctionContract] = &[
    FunctionContract {
        name: "spawn_control_apply",
        identity_args: "p_record jsonb, p_at bigint, p_event bigint",
        result_type: "void",
        security_definer: false,
        pinned_search_path: true,
    },
    FunctionContract {
        name: "spawn_control_record",
        identity_args: "p_record jsonb",
        result_type: "bigint",
        security_definer: false,
        pinned_search_path: true,
    },
    FunctionContract {
        name: "spawn_control_refold",
        identity_args: "",
        result_type: "void",
        security_definer: false,
        pinned_search_path: true,
    },
    FunctionContract {
        name: "spawn_finish",
        identity_args: "p_command bigint, p_attempt integer, p_succeeded boolean, p_error text",
        result_type: "text",
        security_definer: false,
        pinned_search_path: false,
    },
    FunctionContract {
        name: "spawn_human_input",
        identity_args: "p_session text, p_key text, p_text text",
        result_type: "bigint",
        security_definer: true,
        pinned_search_path: true,
    },
    FunctionContract {
        name: "spawn_human_key",
        identity_args: "p_session text, p_key text, p_name text",
        result_type: "bigint",
        security_definer: true,
        pinned_search_path: true,
    },
    FunctionContract {
        name: "spawn_human_master_message",
        identity_args: "p_key text, p_text text, p_mode text",
        result_type: "jsonb",
        security_definer: true,
        pinned_search_path: true,
    },
    FunctionContract {
        name: "spawn_human_replay",
        identity_args: "p_id bigint, p_key text, p_session text, p_action jsonb",
        result_type: "bigint",
        security_definer: false,
        pinned_search_path: false,
    },
    FunctionContract {
        name: "spawn_human_resume",
        identity_args: "p_mission bigint, p_halt bigint, p_key text",
        result_type: "bigint",
        security_definer: true,
        pinned_search_path: true,
    },
    FunctionContract {
        name: "spawn_human_task",
        identity_args: "p_session text, p_action text, p_reason text, p_key text",
        result_type: "jsonb",
        security_definer: true,
        pinned_search_path: true,
    },
];

#[test]
fn human_and_control_function_contracts_match_live_pg_catalog() {
    let (store, schema) = common::store("guard_contracts");
    let mut client = common::admin(&schema);
    let control_names = [
        "spawn_control_record",
        "spawn_control_apply",
        "spawn_control_refold",
        "spawn_finish",
    ];
    let rows = client
        .query(
            "select p.proname::text,
                    pg_get_function_identity_arguments(p.oid)::text,
                    pg_get_function_result(p.oid)::text,
                    p.prosecdef,
                    coalesce(p.proconfig, '{}') = array['search_path='
                        || quote_ident(current_schema()) || ', pg_temp'],
                    has_function_privilege('public', p.oid, 'execute'),
                    coalesce(array_to_string(p.proconfig, ','), '')
               from pg_proc p
               join pg_namespace n on n.oid = p.pronamespace
              where n.nspname = current_schema()
                and (p.proname like 'spawn_human\\_%' escape '\\'
                     or p.proname = any($1::text[]))
              order by p.proname, pg_get_function_identity_arguments(p.oid)",
            &[&control_names.as_slice()],
        )
        .expect("query pg_proc contracts");

    let actual_names: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
    let expected_names: Vec<&str> = LOCKED_FUNCTION_CONTRACTS
        .iter()
        .map(|contract| contract.name)
        .collect();
    assert_eq!(
        actual_names, expected_names,
        "locked spawn_human_* and core control functions must match without extra overloads or omissions"
    );

    for (row, expected) in rows.iter().zip(LOCKED_FUNCTION_CONTRACTS.iter()) {
        let name: String = row.get(0);
        let identity_args: String = row.get(1);
        let result_type: String = row.get(2);
        let security_definer: bool = row.get(3);
        let pinned_search_path: bool = row.get(4);
        let public_execute: bool = row.get(5);
        let proconfig: String = row.get(6);

        assert_eq!(name, expected.name);
        assert_eq!(
            identity_args, expected.identity_args,
            "{name} identity arguments drifted"
        );
        assert_eq!(
            result_type, expected.result_type,
            "{name} return type drifted"
        );
        assert_eq!(
            security_definer, expected.security_definer,
            "{name} SECURITY DEFINER (prosecdef) drifted"
        );
        assert_eq!(
            pinned_search_path, expected.pinned_search_path,
            "{name} search_path pinning drifted (proconfig={proconfig:?})"
        );
        assert!(!public_execute, "{name} must never grant EXECUTE to PUBLIC");
    }

    drop(client);
    drop(store);
    drop(schema);
}

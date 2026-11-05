//! The restricted logins: one set per database, passwords derived slowly
//! from the owner's with a per-login salt, off without an owner password,
//! and the cluster-wide logins of earlier installs cut off.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`). Cases that
//! change a database's logins make a database of their own, so the shared
//! test database's logins, which every other case uses, stay as they are.

use postgres::{Client, NoTls};

mod common;
use common::{admin, schema_name, store, url};
use spawn_postgres::{PostgresStore, role_url};

/// The test URL taken apart: `scheme://user:password@host/database`.
struct Parts {
    scheme: String,
    user: String,
    password: String,
    host: String,
}

fn parts() -> Parts {
    let url = url();
    let (scheme, rest) = url.split_once("://").expect("a URL");
    let (userinfo, tail) = rest.rsplit_once('@').expect("credentials");
    let (user, password) = userinfo.split_once(':').expect("a password");
    let (host, _) = tail.split_once('/').expect("a database");
    Parts {
        scheme: scheme.to_string(),
        user: user.to_string(),
        password: password.to_string(),
        host: host.to_string(),
    }
}

fn database_url(user: &str, password: &str, database: &str) -> String {
    let Parts { scheme, host, .. } = parts();
    format!("{scheme}://{user}:{password}@{host}/{database}")
}

/// A database of its own, made and owned by `owner` (the test owner when
/// `None`), dropped with the logins it made when the guard goes.
struct Database {
    name: String,
    owner: Option<String>,
}

impl Database {
    fn new(case: &str, owner: Option<(&str, &str)>) -> Self {
        let name = schema_name(case);
        let mut client = admin("public");
        if let Some((role, password)) = owner {
            client
                .batch_execute(&format!(
                    "create role {role} login createrole password '{password}'"
                ))
                .expect("a second owner");
        }
        let owner_clause = owner.map_or(String::new(), |(role, _)| format!(" owner {role}"));
        client
            .batch_execute(&format!("create database {name}{owner_clause}"))
            .expect("a database of its own");
        Self {
            name,
            owner: owner.map(|(role, _)| role.to_string()),
        }
    }

    fn url(&self) -> String {
        let Parts { user, password, .. } = parts();
        database_url(&user, &password, &self.name)
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let mut client = admin("public");
        let logins: Vec<String> = client
            .query(
                "select rolname::text from pg_roles
                  where rolname in ('spawn_supervisor_' || $1, 'spawn_reader_' || $1)",
                &[&self.name],
            )
            .map(|rows| rows.iter().map(|row| row.get(0)).collect())
            .unwrap_or_default();
        let _ = client.batch_execute(&format!(
            "drop database if exists {} with (force)",
            self.name
        ));
        for role in logins.iter().chain(self.owner.iter()) {
            let _ = client.batch_execute(&format!("drop role if exists {role}"));
        }
    }
}

fn can_log_in(url: &str) -> bool {
    Client::connect(url, NoTls).is_ok()
}

#[test]
fn each_database_has_its_own_logins_and_another_install_sets_none_of_ours() {
    let (_store, schema) = store("roles_ours");
    let ours = role_url(&common::schema_url(&schema), "spawn_supervisor").expect("our login");
    assert!(can_log_in(&ours));

    // Another install on the cluster, with another owner and password.
    let owner = schema_name("owner").replace("spawn_test_", "spawn_owner_");
    let password = format!("{}x", owner.len() * 7919);
    let other = Database::new("roles_other", Some((&owner, &password)));
    let other_url = database_url(&owner, &password, &other.name);
    PostgresStore::connect(&other_url)
        .expect("the other owner connects")
        .migrate()
        .expect("the other install migrates");
    let theirs = role_url(&other_url, "spawn_supervisor").expect("their login");
    assert!(can_log_in(&theirs));
    assert!(
        theirs.contains(&format!("://spawn_supervisor_{}:", other.name)),
        "their login is named after their database"
    );
    assert!(
        can_log_in(&ours),
        "the other install's migration did not set our login's password"
    );
    assert_ne!(
        ours.split_once('@').map(|(credentials, _)| credentials),
        theirs.split_once('@').map(|(credentials, _)| credentials)
    );

    // Neither login reaches the other install's tables.
    let ours_database = url().rsplit_once('/').expect("a database").1.to_string();
    let theirs_here = theirs.replace(&format!("/{}", other.name), &format!("/{ours_database}"));
    if let Ok(mut client) = Client::connect(&theirs_here, NoTls) {
        let error = client
            .query(&format!("select * from {schema}.mission_missions"), &[])
            .expect_err("their login reads none of our tables");
        assert_eq!(
            error.code(),
            Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
        );
    }
}

#[test]
fn a_migration_without_the_owner_password_turns_the_logins_off() {
    let database = Database::new("roles_off", None);
    PostgresStore::connect(&database.url())
        .expect("connect")
        .migrate()
        .expect("migrate with the owner's password");
    let supervisor = role_url(&database.url(), "spawn_supervisor").expect("the login's URL");
    assert!(can_log_in(&supervisor));
    let login = format!("spawn_supervisor_{}", database.name);
    // As an earlier install left it: a fixed password anyone can guess.
    admin("public")
        .batch_execute(&format!("alter role {login} login password '{login}'"))
        .expect("an earlier install's password");
    let fixed = database_url(&login, &login, &database.name);
    assert!(can_log_in(&fixed));

    // The owner without a password in the URL (it travels as a parameter,
    // as PGPASSWORD or a peer login would leave it out of the URL).
    let Parts {
        scheme,
        user,
        password,
        host,
    } = parts();
    let encoded = password
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('+', "%2B");
    let passwordless = format!(
        "{scheme}://{user}@{host}/{}?password={encoded}",
        database.name
    );
    PostgresStore::connect(&passwordless)
        .expect("the owner connects")
        .migrate()
        .expect("migrate without the owner's password");
    assert!(!can_log_in(&fixed), "the earlier password is gone");
    assert!(!can_log_in(&supervisor), "the derived one too");
    let rows = admin("public")
        .query(
            "select rolname::text, rolcanlogin, rolpassword is null from pg_authid
              where rolname in ('spawn_supervisor_' || $1, 'spawn_reader_' || $1)
              order by 1",
            &[&database.name],
        )
        .expect("the logins");
    let logins: Vec<(String, bool, bool)> = rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    assert_eq!(
        logins,
        [
            (format!("spawn_reader_{}", database.name), false, true),
            (format!("spawn_supervisor_{}", database.name), false, true),
        ]
    );

    // It says so.
    let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&warnings);
    let mut config: postgres::Config = passwordless.parse().expect("parses");
    config.notice_callback(move |notice| {
        if notice.parsed_severity() == Some(postgres::error::Severity::Warning) {
            sink.lock()
                .expect("warnings")
                .push(notice.message().to_string());
        }
    });
    config
        .connect(NoTls)
        .expect("connect")
        .batch_execute("select spawn_logins_off()")
        .expect("turn them off again");
    let warnings = warnings.lock().expect("warnings").clone();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("no owner password") && warnings[0].contains(&login),
        "{warnings:?}"
    );

    // A migration with the password turns them on again.
    PostgresStore::connect(&database.url())
        .expect("connect")
        .migrate()
        .expect("migrate with the owner's password");
    assert!(can_log_in(&supervisor));
    assert!(!can_log_in(&fixed));
}

#[test]
fn the_logins_of_earlier_installs_lose_everything_here() {
    let (store, schema) = store("roles_earlier");
    let mut client = admin(&schema);
    // What an earlier install granted its cluster-wide supervisor login.
    client
        .batch_execute(&format!(
            "do $$ begin
                 if not exists (select 1 from pg_roles where rolname = 'spawn_supervisor') then
                     create role spawn_supervisor nologin;
                 end if;
             end $$;
             grant usage on schema {schema} to spawn_supervisor;
             grant select on mission_tasks to spawn_supervisor;
             grant execute on function spawn_mission_add(text, jsonb, text) to spawn_supervisor;"
        ))
        .expect("an earlier install's grants");
    store.migrate().expect("migrate");
    let row = client
        .query_one(
            "select has_schema_privilege('spawn_supervisor', $1, 'usage'),
                    has_table_privilege('spawn_supervisor', 'mission_tasks', 'select'),
                    has_function_privilege('spawn_supervisor',
                                           'spawn_mission_add(text, jsonb, text)', 'execute'),
                    (select rolcanlogin from pg_roles where rolname = 'spawn_supervisor')",
            &[&schema],
        )
        .expect("the privileges");
    let (usage, select, execute, can_login): (bool, bool, bool, bool) =
        (row.get(0), row.get(1), row.get(2), row.get(3));
    assert!(!usage && !select && !execute, "nothing granted is left");
    let warned = common::all_migrate_warnings(&schema)
        .iter()
        .any(|warning| warning.contains("role spawn_supervisor from an earlier install"));
    assert_eq!(warned, can_login, "a login that can still log in is named");

    // And it acts as nobody: even let into the schema again, none of the
    // supervisor's grants reaches it.
    client
        .batch_execute(&format!(
            "select spawn_submit('human', 'k', 'agent_worker_x', 'session_worker_x',
                                 '{{\"type\":\"stop\"}}');
             grant usage on schema {schema} to spawn_supervisor;
             set session authorization spawn_supervisor"
        ))
        .expect("a worker's command, and the earlier login in the schema");
    let principal: Option<String> = client
        .query_one("select spawn_principal()", &[])
        .expect("the principal")
        .get(0);
    assert_eq!(principal, None);
    let tasks: i64 = client
        .query_one("select count(*) from spawn_read_tasks()", &[])
        .expect("the reader answers")
        .get(0);
    assert_eq!(tasks, 0, "the supervisor's read grant does not reach it");
    let error = client
        .execute(
            "insert into agent_commands (agent_id, session_id, idempotency_key, action)
             values ('agent_worker_x', 'session_worker_x', 'k2', '{\"type\":\"stop\"}')",
            &[],
        )
        .expect_err("nor its command grant");
    assert_eq!(
        error.code(),
        Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    );
    client
        .batch_execute(&format!(
            "reset session authorization; revoke usage on schema {schema} from spawn_supervisor"
        ))
        .expect("back to the owner");
}

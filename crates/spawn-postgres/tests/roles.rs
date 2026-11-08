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
                    "create role {role} login createrole password '{password}';
                     grant {role} to current_user"
                ))
                .expect("a second owner");
            // Granted to the test's login so it may make the database
            // theirs and drop it, and their logins, afterwards: a login
            // that is not a superuser needs the role's privileges for both
            // (a superuser has them anyway).
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
                  where rolname in ('spawn_supervisor_' || $1, 'spawn_reader_' || $1,
                                    'spawn_human_' || $1)",
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

/// The owner's password is the one the driver connects with, however the
/// URL spells it: as a `password=` parameter it derives the same logins as
/// in the user info, and the migration replaces a fixed password an earlier
/// install left. (A migration with no owner password at all turns the
/// logins off: `lib.rs`, `a_migration_without_the_owner_password_turns_the_logins_off`.)
#[test]
fn an_owner_password_given_as_a_parameter_is_the_same_password() {
    let database = Database::new("roles_param", None);
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
    let as_parameter = format!(
        "{scheme}://{user}@{host}/{}?password={encoded}",
        database.name
    );
    PostgresStore::connect(&as_parameter)
        .expect("the owner connects")
        .migrate()
        .expect("migrate");
    assert!(!can_log_in(&fixed), "the earlier password is gone");
    assert!(
        can_log_in(&supervisor),
        "the same derived password as before"
    );
    assert_eq!(
        role_url(&as_parameter, "spawn_supervisor").expect("the login's URL"),
        supervisor,
        "one login URL, whichever way the owner's password is given"
    );
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
    let superuser: bool = client
        .query_one(
            "select rolsuper from pg_roles where rolname = current_user",
            &[],
        )
        .expect("the test login")
        .get(0);

    // And it acts as nobody: even let into the schema again, none of the
    // supervisor's grants reaches it.
    client
        .batch_execute(&format!(
            "select spawn_submit('human', 'k', 'agent_worker_x', 'session_worker_x',
                                 '{{\"type\":\"stop\"}}');
             grant usage on schema {schema} to spawn_supervisor"
        ))
        .expect("a worker's command, and the earlier login in the schema");
    check_earlier_login_acts_as_nobody(&schema, &mut client, superuser);
    client
        .batch_execute(&format!(
            "revoke usage on schema {schema} from spawn_supervisor"
        ))
        .expect("back to the owner");

    // Its grant on the database goes too (in a database of its own: other
    // installs on the shared test cluster grant it on theirs).
    let database = Database::new("roles_earlier_db", None);
    admin("public")
        .batch_execute(&format!(
            "grant connect, temporary on database {} to spawn_supervisor",
            database.name
        ))
        .expect("an earlier install's database grant");
    PostgresStore::connect(&database.url())
        .expect("connect")
        .migrate()
        .expect("migrate");
    let left: i64 = admin("public")
        .query_one(
            "select count(*) from pg_database d cross join lateral aclexplode(d.datacl) a
              where d.datname = $1
                and a.grantee = (select oid from pg_roles where rolname = 'spawn_supervisor')",
            &[&database.name],
        )
        .expect("the database's grants")
        .get(0);
    assert_eq!(left, 0, "nothing on the database is left");
}

/// [`acts_as_nobody`] for whichever way this login may act as the earlier
/// install's `spawn_supervisor`: logged in as it (when the test may give it
/// a password: it made it), or with SET SESSION AUTHORIZATION (a
/// superuser). A login that may neither skips that session check with its
/// reason (its grants were checked either way).
fn check_earlier_login_acts_as_nobody(schema: &str, client: &mut Client, superuser: bool) {
    let alterable: bool = client
        .query_one(
            "select not r.rolcanlogin and pg_has_role(current_user, r.oid, 'member with admin option')
               from pg_roles r where r.rolname = 'spawn_supervisor'",
            &[],
        )
        .expect("whether the earlier login may be given a password")
        .get(0);
    if alterable {
        let password = common::password();
        client
            .batch_execute(&format!(
                "alter role spawn_supervisor login password '{password}'"
            ))
            .expect("a password for the while");
        let mut login = common::connect_as(schema, "spawn_supervisor", &password);
        acts_as_nobody(&mut login);
        drop(login);
        client
            .batch_execute("alter role spawn_supervisor nologin password null")
            .expect("no login again");
    } else if superuser {
        client
            .batch_execute("set session authorization spawn_supervisor")
            .expect("become the earlier login");
        acts_as_nobody(client);
        client
            .batch_execute("reset session authorization")
            .expect("back to the owner");
    } else {
        eprintln!(
            "the_logins_of_earlier_installs_lose_everything_here: spawn_supervisor can log in \
             with another install's password and this login may not become it; that session \
             is not checked here (its grants are, above)"
        );
    }
}

/// The earlier install's login acts as nobody: no principal, no read, no
/// write, even with its grants back.
fn acts_as_nobody(login: &mut Client) {
    let principal: Option<String> = login
        .query_one("select spawn_principal()", &[])
        .expect("the principal")
        .get(0);
    assert_eq!(principal, None);
    let tasks: i64 = login
        .query_one("select count(*) from spawn_read_tasks()", &[])
        .expect("the reader answers")
        .get(0);
    assert_eq!(tasks, 0, "the supervisor's read grant does not reach it");
    let error = login
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
}

#[test]
fn many_migrations_at_once_on_one_cluster_all_succeed() {
    // Fresh databases (each makes its logins) and fresh schemas of the
    // shared test database (they share its logins), all started together.
    const DATABASES: usize = 8;
    const SCHEMAS: usize = 8;
    let databases: Vec<Database> = (0..DATABASES)
        .map(|number| Database::new(&format!("roles_burst_{number}"), None))
        .collect();
    let mut urls: Vec<String> = databases.iter().map(Database::url).collect();
    urls.extend(
        (0..SCHEMAS).map(|number| common::schema_url(&schema_name(&format!("burst_{number}")))),
    );
    let start = std::sync::Arc::new(std::sync::Barrier::new(urls.len()));
    let migrations: Vec<_> = urls
        .into_iter()
        .map(|url| {
            let start = std::sync::Arc::clone(&start);
            std::thread::spawn(move || {
                let store = PostgresStore::connect(&url).map_err(|error| error.to_string())?;
                start.wait();
                store.migrate().map_err(|error| error.to_string())?;
                role_url(&url, "spawn_supervisor").map(|login| can_log_in(&login))
            })
        })
        .collect();
    for migration in migrations {
        assert_eq!(
            migration.join().expect("the migration returns"),
            Ok(true),
            "every migration succeeds and its supervisor logs in"
        );
    }
}

/// Audit L8: a migration's `alter role ... password` carries a SCRAM
/// verifier computed by the client, never the login's password. Held
/// behind another transaction's update of the role's row, the statement
/// sits in `pg_stat_activity` (readable by superusers and
/// `pg_read_all_stats`), and the "tuple concurrently updated" error it then
/// hits is logged with its statement under the default
/// `log_min_error_statement = error`: what both show is the verifier.
#[test]
fn a_login_password_never_appears_in_statement_text() {
    let database = Database::new("roles_l8", None);
    PostgresStore::connect(&database.url())
        .expect("connect")
        .migrate()
        .expect("the first migration");
    let real = role_url(&database.url(), "spawn_supervisor").expect("the login's URL");
    let login = format!("spawn_supervisor_{}", database.name);

    // Hold the role's catalog row in an open transaction.
    let mut holder = Client::connect(&database.url(), NoTls).expect("holder");
    let mut transaction = holder.transaction().expect("begin");
    transaction
        .batch_execute(&format!("alter role {login} valid until 'infinity'"))
        .expect("touch the role's row");

    let url = database.url();
    let migration = std::thread::spawn(move || {
        PostgresStore::connect(&url)
            .expect("connect")
            .migrate()
            .map_err(|error| error.to_string())
    });

    // The second migration waits on that row, at its `alter role ... password`.
    let mut watcher = admin("public");
    let prefix = format!("alter role \"{login}\" with login password '");
    let mut seen: Option<String> = None;
    // Other tests' migrations hold the server-wide migration lock first, so
    // the wait is long; it ends early once the statement shows or the
    // migration finished without blocking.
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(3);
    while std::time::Instant::now() < deadline && !migration.is_finished() {
        seen = watcher
            .query(
                "select query from pg_stat_activity where starts_with(query, $1)",
                &[&prefix],
            )
            .expect("pg_stat_activity")
            .first()
            .map(|row| row.get(0));
        if seen.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    transaction.commit().expect("release the row");
    migration
        .join()
        .expect("the migration returns")
        .expect("the retried migration succeeds");

    let seen = seen.expect("the blocked alter role is in pg_stat_activity");
    let leaked = seen
        .strip_prefix(&prefix)
        .and_then(|rest| rest.split_once('\''))
        .map(|(password, _)| password.to_string())
        .expect("a quoted password");
    let real_password = real
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .and_then(|(userinfo, _)| userinfo.split_once(':'))
        .map(|(_, password)| password.to_string())
        .expect("the login URL's password");
    // Compare without printing either.
    assert!(
        !leaked.contains(&real_password),
        "pg_stat_activity showed the supervisor login's real password ({} chars)",
        leaked.len()
    );
    assert!(
        leaked.starts_with("SCRAM-SHA-256$"),
        "the statement carries a SCRAM verifier"
    );
    // And the login still logs in with its password.
    let mut supervisor = Client::connect(&real, NoTls).expect("the supervisor logs in");
    supervisor.batch_execute("select 1").expect("a query");
}

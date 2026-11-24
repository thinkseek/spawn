//! A test's schema in the test database, dropped when the test ends
//! (backlog P19): as the test passes, and as a failing test unwinds; kept
//! only under `SPAWN_TEST_KEEP_FAILED=1` (objective 139).
//!
//! A test that needs the database makes a [`TestSchema`] and holds it for as
//! long as it uses the schema. Dropping the guard drops the schema and
//! everything in it, whether the test passed or failed (the guard dropped
//! while the thread panics). With [`KEEP_FAILED`] set to `1` when the guard
//! was made, a failed test's schema is kept for debugging and its name is
//! printed. Only the schema the guard named is ever dropped, by that
//! name: never one found by a pattern, since the tests of other processes
//! (another suite, another gate) keep theirs in the same database.
//!
//! Before it, crate tests left their schemas behind, in the fleet's test
//! database 2533 of them, 2.7 GB, by 2026-11-11; and while failed tests
//! kept theirs, every mutant a mutation run caught left one (about 100 to
//! 270 a day on the ccd server, 2026-11-14). A process that is killed or
//! aborts never unwinds, and its schemas are still left.

use crate::PostgresStore;

/// The variable that names the test database.
pub const TEST_DATABASE_URL: &str = "SPAWN_TEST_DATABASE_URL";

/// The variable that keeps a failed test's schema for debugging: only the
/// value `1` keeps it; unset, or any other value, drops it.
pub const KEEP_FAILED: &str = "SPAWN_TEST_KEEP_FAILED";

/// How long the drop's connection waits for the server: a failing test's
/// guard connects as it unwinds, and a server that never answers would
/// otherwise hold it for the OS's TCP timeout (about 2 min).
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A schema of the test's own, by name (it derefs to `str`), dropped on drop,
/// a failing test's too unless [`KEEP_FAILED`] is `1` (then it is kept, and
/// its name said).
///
/// Hold it before the connections that use the schema, so that they close
/// first (a guard declared first is dropped last): the drop waits at most
/// 5 s for a lock and then gives up, saying so.
#[derive(Debug)]
pub struct TestSchema {
    name: String,
    url: String,
    /// Whether a failed test keeps the schema: [`KEEP_FAILED`] was `1`
    /// when the guard was made.
    keep_failed: bool,
}

impl TestSchema {
    /// A fresh schema name for `case` in the test database:
    /// `spawn_test_<pid>_<nanos>_<n>_<case>`, not the pid alone (in a gate's
    /// PID namespace the pid is the same on every run), and `n` counts the
    /// schemas this process named, so two cases started in the same
    /// nanosecond differ. The schema itself is made by whatever connects
    /// in it first ([`TestSchema::store`], `PostgresStore::connect_in_schema`),
    /// which dates it with the comment `{"made_at_ms": <ms>}` and names the
    /// connection after it (`application_name`, objective 094), so a reader
    /// tells a live test's schema from a leftover.
    ///
    /// It reads [`KEEP_FAILED`] now: a failed test keeps its schema only
    /// when the variable was `1` here.
    ///
    /// # Panics
    ///
    /// When `SPAWN_TEST_DATABASE_URL` is not set (tests do not skip), or
    /// `case` is not lowercase ASCII letters, digits and `_` (the schema
    /// name goes into SQL unquoted, as `connect_in_schema` requires).
    pub fn new(case: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        assert!(
            !case.is_empty()
                && case
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
            "a test schema's case is lowercase letters, digits and _: {case:?}"
        );
        let ordinal = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let url = std::env::var(TEST_DATABASE_URL).unwrap_or_else(|_| {
            panic!("{TEST_DATABASE_URL} must point at a disposable database; tests do not skip")
        });
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let name = format!(
            "spawn_test_{}_{nanos:x}_{ordinal:x}_{case}",
            std::process::id()
        );
        let keep_failed = std::env::var(KEEP_FAILED).is_ok_and(|value| value == "1");
        Self {
            name,
            url,
            keep_failed,
        }
    }

    /// The schema's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The test database's URL (never printed: it may hold a password).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// A store in the schema, which it makes and migrates when absent.
    ///
    /// # Panics
    ///
    /// When the store cannot connect or migrate.
    pub fn store(&self) -> PostgresStore {
        PostgresStore::connect_in_schema(&self.url, &self.name)
            .unwrap_or_else(|error| panic!("connect and migrate in {}: {error}", self.name))
    }
}

impl std::ops::Deref for TestSchema {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for TestSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.name)
    }
}

/// A connection to `url`, its password the passfile's when the URL holds
/// none (objective 109), so a schema made under a password-less URL is
/// dropped as it was made; it gives up connecting after
/// [`CONNECT_TIMEOUT`].
fn client(url: &str) -> Result<postgres::Client, postgres::Error> {
    crate::passfile::config(url)?
        .connect_timeout(CONNECT_TIMEOUT)
        .connect(postgres::NoTls)
}

/// Drops the schema: as the test passes, and as a failing test unwinds (the
/// thread panicking), by the same statement, unless [`KEEP_FAILED`] was `1`
/// when the guard was made (then it is kept, and its name said). A drop
/// that fails is said, never a panic: a panic while a failing test unwinds
/// would abort the whole test process.
///
/// Never drop the guard inside a tokio runtime: `postgres::Client` panics
/// there, and a panic while a failing test unwinds aborts the test binary.
/// A passing `#[should_panic]` test unwinds too, so its schema now goes as
/// well (it was kept before, although the test passed).
impl Drop for TestSchema {
    fn drop(&mut self) {
        let failed = std::thread::panicking();
        if failed && self.keep_failed {
            eprintln!(
                "the failed test's schema is kept: {} (in the database {TEST_DATABASE_URL} names; \
                 {KEEP_FAILED}=1)",
                self.name
            );
            return;
        }
        let dropped = client(&self.url).and_then(|mut client| {
            client.batch_execute(&format!(
                "set lock_timeout = '5s'; drop schema if exists {} cascade",
                self.name
            ))
        });
        match dropped {
            Err(error) => eprintln!("cannot drop the test schema {}: {error}", self.name),
            Ok(()) if failed => eprintln!(
                "the failed test's schema is dropped: {} ({KEEP_FAILED}=1 keeps it)",
                self.name
            ),
            Ok(()) => {}
        }
    }
}

/// `fleet_health`'s statement, for a test to read its plan (its
/// interventions count must read the day, not the log: tests/experience.rs).
pub const FLEET_HEALTH: &str = crate::fleet::FLEET_HEALTH;

/// The message alert's incremental read, for a test to read its plan (the
/// transcript lookup uses its session/time/event-id index: tests/message_due.rs).
pub const ALERT_MESSAGES_SINCE: &str = crate::alerts::MESSAGES_SINCE;

/// The nodes' two reads, for a test to read their plans (each fact one
/// seek, never a scan of a node's heartbeats: tests/nodes.rs).
pub const NODES: &str = crate::nodes::NODES;
pub const NODE_MEMBERS: &str = crate::nodes::MEMBERS;

#[cfg(test)]
mod tests {
    use super::TestSchema;

    fn exists(name: &str) -> bool {
        let url = std::env::var(super::TEST_DATABASE_URL).expect("the test database");
        let mut client = super::client(&url).expect("connect");
        client
            .query_one(
                "select count(*) from pg_namespace where nspname = $1",
                &[&name],
            )
            .expect("count")
            .get::<_, i64>(0)
            == 1
    }

    /// A case that makes its schema and fails, its guard's `keep_failed` set
    /// to `keep` (not the variable, which parallel tests share) and its url
    /// to `url` when given, run on a thread of its own: the schema's name,
    /// which the case panics with.
    fn failed_case(keep: bool, url: Option<&'static str>) -> String {
        let failed = std::thread::spawn(move || {
            let mut schema = TestSchema::new("guard_failed");
            schema.keep_failed = keep;
            let name = schema.name().to_string();
            match url {
                Some(url) => schema.url = url.to_string(),
                None => drop(schema.store()),
            }
            std::panic::panic_any(name);
        })
        .join()
        .expect_err("the case fails");
        *failed.downcast::<String>().expect("its schema's name")
    }

    /// A failed test's schema goes with its guard as the test unwinds.
    #[test]
    fn a_failed_tests_schema_goes_unless_kept() {
        let name = failed_case(false, None);
        assert!(!exists(&name), "a failed test's schema is dropped");
    }

    /// A failed test's guard whose server never answers (192.0.2.1,
    /// TEST-NET-1, which drops the SYN) gives up connecting: the test
    /// finishes unwinding within seconds, not the OS's TCP timeout. Where a
    /// network refuses the address at once, it passes without showing it.
    #[test]
    fn a_failed_tests_drop_gives_up_on_a_silent_server_within_seconds() {
        let started = std::time::Instant::now();
        failed_case(false, Some("postgres://spawn@192.0.2.1:5432/spawn_test"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "the drop waited {:?}",
            started.elapsed()
        );
    }

    /// A passed test's schema goes with its guard; a failed test's, kept,
    /// stays, and is dropped here by its name.
    #[test]
    fn a_schema_goes_with_a_passed_test_and_stays_after_a_failed_one() {
        let passed = TestSchema::new("guard_passed");
        let name = passed.name().to_string();
        drop(passed.store());
        assert!(exists(&name), "the store made the schema");
        drop(passed);
        assert!(!exists(&name), "a passed test's schema is dropped");

        let name = failed_case(true, None);
        assert!(exists(&name), "a failed test's schema is kept");
        let url = std::env::var(super::TEST_DATABASE_URL).expect("the test database");
        super::client(&url)
            .expect("connect")
            .batch_execute(&format!("drop schema {name} cascade"))
            .expect("drop the kept schema");
    }

    #[test]
    #[should_panic(expected = "a test schema's case is lowercase letters, digits and _")]
    fn a_case_that_is_no_bare_identifier_is_refused() {
        let _ = TestSchema::new("x; drop schema public");
    }

    /// Objective 094: a schema made through the guard carries its comment,
    /// `{"made_at_ms": <ms>}` at the server's clock, and the store's
    /// connection carries its name as `application_name`.
    #[test]
    fn a_schema_made_by_the_guard_carries_its_comment_and_its_session_name() {
        let schema = TestSchema::new("guard_comment");
        let url = std::env::var(super::TEST_DATABASE_URL).expect("the test database");
        let mut client = super::client(&url).expect("connect");
        let before: i64 = client
            .query_one(
                "select floor(extract(epoch from clock_timestamp()) * 1000)::bigint",
                &[],
            )
            .expect("the clock")
            .get(0);
        let store = schema.store();
        let row = client
            .query_one(
                "select obj_description(n.oid, 'pg_namespace'),
                        (select count(*) from pg_stat_activity a
                          where a.application_name = n.nspname::text)
                   from pg_namespace n where n.nspname = $1",
                &[&schema.name()],
            )
            .expect("the schema");
        let comment: String = row.get(0);
        let made: serde_json::Value = serde_json::from_str(&comment).expect("JSON");
        let made_at = made["made_at_ms"].as_i64().expect("made_at_ms");
        assert!(made_at >= before, "{made_at} is at or after {before}");
        assert_eq!(
            made.as_object().map(serde_json::Map::len),
            Some(1),
            "{made}"
        );
        assert_eq!(
            row.get::<_, i64>(1),
            1,
            "the store's session is named after it"
        );
        // A second connection into it keeps the first comment.
        let again = schema.store();
        let kept: String = client
            .query_one(
                "select obj_description(n.oid, 'pg_namespace') from pg_namespace n
                  where n.nspname = $1",
                &[&schema.name()],
            )
            .expect("the schema")
            .get(0);
        assert_eq!(kept, comment);
        drop(again);
        drop(store);
        drop(client);
    }

    #[test]
    fn two_schemas_named_at_once_differ() {
        let first = TestSchema::new("same");
        let second = TestSchema::new("same");
        assert_ne!(first.name(), second.name());
    }
}

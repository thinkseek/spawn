//! A test's schema in the test database, dropped when the test ends
//! (backlog P19).
//!
//! A test that needs the database makes a [`TestSchema`] and holds it for as
//! long as it uses the schema. When the test passes, dropping the guard drops
//! the schema and everything in it; when the test fails (the guard is
//! dropped while the thread panics), the schema is kept for debugging and its
//! name is printed. Only the schema the guard named is ever dropped, by that
//! name: never one found by a pattern, since the tests of other processes
//! (another suite, another gate) keep theirs in the same database.
//!
//! Before it, crate tests left their schemas behind, in the fleet's test
//! database 2533 of them, 2.7 GB, by 2026-11-11.

use crate::PostgresStore;

/// The variable that names the test database.
pub const TEST_DATABASE_URL: &str = "SPAWN_TEST_DATABASE_URL";

/// A schema of the test's own, by name (it derefs to `str`), dropped on drop
/// unless the thread is panicking (a failing test keeps it, and says which).
///
/// Hold it before the connections that use the schema, so that they close
/// first (a guard declared first is dropped last): the drop waits at most
/// 5 s for a lock and then gives up, saying so.
#[derive(Debug)]
pub struct TestSchema {
    name: String,
    url: String,
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
        Self { name, url }
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

impl Drop for TestSchema {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "the failed test's schema is kept: {} (in the database {TEST_DATABASE_URL} names)",
                self.name
            );
            return;
        }
        let dropped =
            client(&self.url).and_then(|mut client| {
                client.batch_execute(&format!(
                    "set lock_timeout = '5s'; drop schema if exists {} cascade",
                    self.name
                ))
            });
        if let Err(error) = dropped {
            eprintln!("cannot drop the test schema {}: {error}", self.name);
        }
    }
}

/// `fleet_health`'s statement, for a test to read its plan (its
/// interventions count must read the day, not the log: tests/experience.rs).
pub const FLEET_HEALTH: &str = crate::fleet::FLEET_HEALTH;

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

    /// A passed test's schema goes with its guard; a failed test's stays,
    /// and is dropped here by its name.
    #[test]
    fn a_schema_goes_with_a_passed_test_and_stays_after_a_failed_one() {
        let passed = TestSchema::new("guard_passed");
        let name = passed.name().to_string();
        drop(passed.store());
        assert!(exists(&name), "the store made the schema");
        drop(passed);
        assert!(!exists(&name), "a passed test's schema is dropped");

        let failed = std::thread::spawn(|| {
            let schema = TestSchema::new("guard_failed");
            drop(schema.store());
            let name = schema.name().to_string();
            std::panic::panic_any(name);
        })
        .join()
        .expect_err("the case fails");
        let name = failed.downcast::<String>().expect("its schema's name");
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

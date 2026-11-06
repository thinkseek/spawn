//! The test database's probe before a gate (objective 018,
//! `spawn_postgres::parallel_query_probe`): a connection, then a forced
//! parallel query, each bounded in time.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`).

use std::time::{Duration, Instant};

mod common;
use common::url;
use spawn_postgres::parallel_query_probe;

#[test]
fn the_test_database_runs_the_parallel_probe() {
    parallel_query_probe(&url(), Duration::from_secs(10))
        .expect("the test database connects and runs a parallel query");
}

#[test]
fn a_socket_directory_with_no_server_fails_the_connect_probe_without_the_password() {
    let empty = std::env::temp_dir().join(format!(
        "spawn-probe-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos())
    ));
    std::fs::create_dir_all(&empty).expect("an empty socket directory");
    let directory = empty.to_string_lossy().replace('/', "%2F");
    let url = format!("postgres://probe:pr0besecret@{directory}/nothing");
    let started = Instant::now();
    let failure = parallel_query_probe(&url, Duration::from_secs(10)).expect_err("no server");
    assert!(started.elapsed() < Duration::from_secs(10), "refused at once");
    assert_eq!(failure.probe, "connect within 10 s");
    assert!(!failure.error.is_empty());
    assert!(
        !format!("{failure:?}").contains("pr0besecret"),
        "{failure:?}"
    );
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn an_unreadable_url_fails_the_probe_that_reads_it() {
    let failure = parallel_query_probe("postgres://u:p@host:notaport/db", Duration::from_secs(1))
        .expect_err("not a URL");
    assert!(failure.probe.contains("postgres URL"), "{failure:?}");
}

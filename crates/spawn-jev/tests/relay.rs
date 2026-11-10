//! The Jev relay (`scripts/jev-relay.py`) against this client: its
//! self-test, run with the workspace's tests so the gate judges the relay
//! too (python3 and loopback only: a fake hosted API, no key, no network),
//! and the order of the two timeouts.

use std::path::Path;
use std::process::Command;

use spawn_jev::JevConfig;

fn script(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts")
        .join(name)
}

#[test]
fn the_jev_relay_passes_its_self_test() {
    let test = script("jev-relay-test.py");
    let output = match Command::new("python3").arg("-I").arg(&test).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("python3 is not installed; the relay self-test is skipped");
            return;
        }
        Err(error) => panic!("run {}: {error}", test.display()),
    };
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The client must outwait the relay: a relay that gives up on the hosted
/// API answers 502 with its reason, and a client that hung up first turns
/// that into a broken pipe with no reason (every 502 of 2026-11-07..10).
#[test]
fn the_client_waits_longer_than_the_relay() {
    let relay = std::fs::read_to_string(script("jev-relay.py")).expect("read the relay");
    let line = relay
        .lines()
        .find_map(|line| line.strip_prefix("DEFAULT_TIMEOUT_SECS = "))
        .expect("the relay names DEFAULT_TIMEOUT_SECS");
    let relay_secs: u64 = line.trim().parse().expect("a whole number of seconds");
    let client = JevConfig::new("http://127.0.0.1:8790").timeout;
    assert_eq!(client, JevConfig::DEFAULT_TIMEOUT);
    assert!(
        client.as_secs() >= relay_secs + 2,
        "the client's {client:?} must outwait the relay's {relay_secs} s by a margin"
    );
}

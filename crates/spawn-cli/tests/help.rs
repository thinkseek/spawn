//! The usage, asked for (GitHub #63, part 3 b): `spawn --help`, `spawn
//! help` and `spawn <command> --help` print it on stdout with rc 0 and open
//! no store, and a command run without `SPAWN_DATABASE_URL` says where the
//! setting comes from. A `--help` among the free words of `spawn ack`'s
//! why is part of the why, not a help request. Exercises the compiled
//! `spawn` binary (`CARGO_BIN_EXE_spawn`) with an empty environment.

use std::process::{Command, Output};

/// `spawn <arguments>` with an empty environment, plus `extra`.
fn spawn(arguments: &[&str], extra: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(arguments)
        .env_clear()
        .envs(extra.iter().copied())
        .output()
        .expect("run spawn")
}

#[test]
fn the_usage_asked_for_is_printed_with_rc_0_and_opens_no_store() {
    // A database that answers nobody: had the store been opened, the
    // command would fail.
    let nowhere = [("SPAWN_DATABASE_URL", "postgres://nobody@127.0.0.1:1/none")];
    for arguments in [
        &["--help"][..],
        &["-h"],
        &["help"],
        &["task", "--help"],
        &["task", "accept", "--help"],
        &["fleet", "--help"],
        &["mission", "--help"],
    ] {
        for extra in [&[][..], &nowhere[..]] {
            let output = spawn(arguments, extra);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(0), "{arguments:?}: {stderr}");
            assert!(
                stdout.starts_with("usage: spawn <init-db|"),
                "{arguments:?}: {stdout}"
            );
            assert!(
                stdout.contains("SPAWN_DATABASE_URL") && stdout.contains("spawn.env"),
                "{arguments:?}: {stdout}"
            );
            assert!(stderr.is_empty(), "{arguments:?}: {stderr}");
        }
    }
}

#[test]
fn no_command_still_prints_the_usage_as_an_error() {
    let output = spawn(&[], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr.starts_with("spawn: usage: spawn <init-db|"),
        "{stderr}"
    );
    assert!(stderr.contains("spawn.env"), "{stderr}");
}

#[test]
fn a_command_without_the_database_says_where_the_setting_comes_from() {
    for arguments in [&["fleet"][..], &["task", "list"]] {
        let output = spawn(arguments, &[]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(
            stderr.contains("SPAWN_DATABASE_URL is not set; ") && stderr.contains("spawn.env"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn a_help_word_in_an_ack_why_is_part_of_the_why_and_the_ack_is_taken() {
    // The agent's own workspace, named after it, as the Bridge launches
    // the pane; removed with the test, passed or failed.
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let root = Scratch(std::env::temp_dir().join(format!("spawn-help-ack-{}", std::process::id())));
    let workspace = root.0.join("agent_worker_help_1");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let pane = [
        ("SPAWN_AGENT_ID", "agent_worker_help_1"),
        ("SPAWN_EXECUTION_ID", "exec_1"),
        ("SPAWN_WORKSPACE", workspace.to_str().expect("utf-8 path")),
    ];
    let acks = workspace.join(".spawn/acks");
    for (arguments, outcome, why) in [
        (
            &["ack", "99", "declined", "see", "--help", "for", "why"][..],
            "declined",
            "see --help for why",
        ),
        (
            &["ack", "98", "done", "--help", "works", "now"],
            "done",
            "--help works now",
        ),
    ] {
        let _ = std::fs::remove_dir_all(&acks);
        let output = spawn(arguments, &pane);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{arguments:?}: {stderr}");
        assert!(
            stdout.contains(&format!("({outcome}) taken")) && !stdout.contains("usage:"),
            "{arguments:?}: {stdout}"
        );
        let files: Vec<_> = std::fs::read_dir(&acks)
            .expect("the ack was written")
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(files.len(), 1, "{arguments:?}: {files:?}");
        let ack: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&files[0]).expect("read the ack"))
                .expect("the ack is JSON");
        assert_eq!(ack["command_id"], arguments[1].parse::<i64>().expect("id"));
        assert_eq!(ack["outcome"], outcome, "{ack}");
        assert_eq!(ack["why"], why, "{ack}");
    }
    // `spawn ack --help` still asks for the usage, writing no ack.
    let _ = std::fs::remove_dir_all(&acks);
    let output = spawn(&["ack", "--help"], &pane);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("usage: spawn <init-db|"));
    assert!(!acks.exists(), "no ack written");
}

struct ScratchDir(std::path::PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonical repo root")
}

fn spawn_json(arguments: &[&str], extra: &[(&str, &str)]) -> serde_json::Value {
    let output = spawn(arguments, extra);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "command {arguments:?} failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!("command {arguments:?} produced invalid JSON ({err}): {stdout}")
    })
}

fn schema_url(schema: &spawn_postgres::testing::TestSchema) -> String {
    let base = schema.url();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-csearch_path%3D{}", schema.name())
}

fn assert_scan_report_contract(report: &serde_json::Value) {
    let obj = report.as_object().expect("scan report JSON object");
    assert!(obj["schema_version"].as_u64().is_some_and(|v| v >= 1));
    assert!(obj["scanned_at_ms"].as_i64().is_some_and(|v| v > 0));
    assert!(obj["initiative_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(obj["boundary"].is_object());
    let counts = obj["counts"].as_object().expect("counts object");
    for key in [
        "in_scope",
        "covered",
        "unassociated_wild",
        "stale_references",
    ] {
        assert!(counts[key].is_u64(), "counts.{key} must be u64");
    }
    assert!(obj["unassociated_files"].is_array());
    assert!(obj["stale_references"].is_array());
    assert!(obj["excluded_files"].is_array());
    let bi = obj["bidirectional_index"]
        .as_object()
        .expect("bidirectional_index object");
    assert!(bi["file_to_features"].is_object());
    assert!(bi["feature_to_files"].is_object());
    assert!(obj["consistency"].is_object());
}

#[test]
fn offline_json_commands_return_stable_contract_schemas() {
    let root = repo_root();
    let registry = root.join("docs/feature-map/registry.json");
    let root_str = root.to_str().expect("utf-8 repo root");
    let reg_str = registry.to_str().expect("utf-8 registry path");

    let check = spawn_json(
        &[
            "features",
            "check",
            "--root",
            root_str,
            "--registry",
            reg_str,
            "--json",
        ],
        &[],
    );
    assert_scan_report_contract(&check);

    let scan = spawn_json(
        &[
            "features",
            "scan",
            "--root",
            root_str,
            "--registry",
            reg_str,
            "--json",
        ],
        &[],
    );
    assert_scan_report_contract(&scan);

    let graph = spawn_json(
        &[
            "features",
            "graph",
            "--root",
            root_str,
            "--registry",
            reg_str,
            "--json",
        ],
        &[],
    );
    assert_eq!(graph["schema_version"].as_u64(), Some(1));
    assert_eq!(graph["level"].as_str(), Some("feature"));
    assert!(graph["root"].is_string());
    assert!(graph["registry_path"].is_string());
    assert!(graph["feature"].is_null());
    assert!(graph["graph"]["files_analyzed"].is_u64());
    assert!(graph["graph"]["edges"].is_u64());
    assert!(graph["projection"].is_object());
    assert!(graph["feature_edges"].is_array());
    assert!(graph["truth"].is_object());
}

fn assert_episodes_empty_contract(episodes: &serde_json::Value) {
    assert!(episodes["mission"].is_null());
    assert_eq!(episodes["strict"].as_bool(), Some(false));
    assert_eq!(episodes["episodes"].as_array().map(Vec::len), Some(0));
    assert_eq!(episodes["warnings"].as_u64(), Some(0));
    assert_eq!(episodes["parity_ok"].as_bool(), Some(true));
    assert_eq!(episodes["drift_count"].as_u64(), Some(0));
    assert_eq!(episodes["drifts"].as_array().map(Vec::len), Some(0));
    let summary = episodes["summary"].as_object().expect("episodes summary");
    for key in [
        "total",
        "landed",
        "accepted",
        "in_flight",
        "rejected",
        "cancelled",
        "aborted",
        "over_budget",
        "total_rework",
        "total_reroutes",
        "review_sync_points",
        "gate_verdicts",
        "warnings",
    ] {
        assert_eq!(summary[key].as_u64(), Some(0), "summary.{key} must be 0");
    }
}

#[test]
fn empty_postgres_json_read_commands_return_stable_contract_schemas() {
    let schema = spawn_postgres::testing::TestSchema::new("help_json_contracts");
    let store = schema.store();
    let db_url = schema_url(&schema);
    let env = [("SPAWN_DATABASE_URL", db_url.as_str())];

    let mission_list = spawn_json(&["mission", "list", "--json"], &env);
    assert_eq!(mission_list["holds"].as_array().map(Vec::len), Some(0));
    assert_eq!(mission_list["missions"].as_array().map(Vec::len), Some(0));
    assert_eq!(mission_list["residents"].as_array().map(Vec::len), Some(0));

    let episodes = spawn_json(&["mission", "episodes", "--verify-parity", "--json"], &env);
    assert_episodes_empty_contract(&episodes);

    let grade_list = spawn_json(&["grade", "list", "--json"], &env);
    assert_eq!(grade_list.as_array().map(Vec::len), Some(0));

    let node_list = spawn_json(&["node", "list", "--json"], &env);
    assert_eq!(node_list.as_array().map(Vec::len), Some(0));

    let alerts_once = spawn_json(&["alerts", "--once", "--json"], &env);
    assert_eq!(alerts_once.as_array().map(Vec::len), Some(0));

    let verify_list = spawn_json(&["verify", "list"], &env);
    assert_eq!(verify_list["unverified"].as_array().map(Vec::len), Some(0));
    assert_eq!(verify_list["over_a_day"].as_u64(), Some(0));
    assert_eq!(verify_list["older_unverified"].as_u64(), Some(0));
    assert!(
        verify_list["listed_since_ms"]
            .as_i64()
            .is_some_and(|v| v > 0)
    );
    assert!(verify_list["deploy"].is_null());

    let scratch = ScratchDir(
        std::env::temp_dir().join(format!("spawn-help-scorecard-{}", std::process::id())),
    );
    std::fs::create_dir_all(&scratch.0).expect("scratch dir");
    let root_str = scratch.0.to_str().expect("utf-8 scratch dir");
    let scorecard = spawn_json(&["scorecard", "--json", "--root", root_str], &env);
    let since_ms = scorecard["since_ms"].as_i64().expect("since_ms");
    let until_ms = scorecard["until_ms"].as_i64().expect("until_ms");
    assert!(until_ms > since_ms);
    assert!(scorecard["previous_since_ms"].as_i64().is_some());
    assert!(scorecard["min_n"].as_u64().is_some_and(|n| n >= 1));
    assert!(scorecard["settling"].is_boolean());
    assert!(scorecard["owner_runs_from_ms"].is_null());
    let scopes = scorecard["scopes"].as_array().expect("scopes array");
    let fleet = scopes
        .iter()
        .find(|scope| scope["scope"].as_str() == Some("fleet"))
        .expect("fleet scope");
    assert!(fleet["mission"].is_null());
    assert!(fleet["dimensions"].is_object());
    assert!(fleet["days"].is_array());

    drop(scratch);
    drop(store);
    drop(schema);
}

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

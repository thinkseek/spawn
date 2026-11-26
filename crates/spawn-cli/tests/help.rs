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
        &["status", "--help"],
        &["say", "--help"],
        &["submit", "--help"],
        &["task", "--help"],
        &["task", "accept", "--help"],
        &["task", "commits", "--help"],
        &["task", "diff", "--help"],
        &["task", "land", "--help"],
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
    for arguments in [
        &["status"][..],
        &["say", "agent_supervisor", "hello"],
        &["fleet"],
        &["task", "list"],
    ] {
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
fn status_say_and_submit_help_contracts_are_locked() {
    let top = spawn(&["--help"], &[]);
    assert_eq!(top.status.code(), Some(0));
    let top_out = String::from_utf8_lossy(&top.stdout);
    assert!(top_out.contains("spawn status [--json]"), "{top_out}");
    assert!(
        top_out.contains(
            "spawn say <agent> <message> [--session <sid>] [--key <idempotency-key>] [--json]"
        ),
        "{top_out}"
    );

    let status_help = spawn(&["status", "--help"], &[]);
    assert_eq!(status_help.status.code(), Some(0));
    let status_out = String::from_utf8_lossy(&status_help.stdout);
    assert!(status_out.contains("spawn status [--json]"), "{status_out}");

    let say_help = spawn(&["say", "--help"], &[]);
    assert_eq!(say_help.status.code(), Some(0));
    let say_out = String::from_utf8_lossy(&say_help.stdout);
    assert!(
        say_out.contains(
            "spawn say <agent> <message> [--session <sid>] [--key <idempotency-key>] [--json]"
        ),
        "{say_out}"
    );

    let submit_help = spawn(&["submit", "--help"], &[]);
    assert_eq!(submit_help.status.code(), Some(0));
    let submit_out = String::from_utf8_lossy(&submit_help.stdout);
    assert!(
        submit_out.contains(
            "spawn submit --agent <agent id> [--session <session id>] [--key <idempotency key>] --action <command action JSON> [--principal <name>]"
        ),
        "{submit_out}"
    );
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

    let status = spawn_json(&["status", "--json"], &env);
    assert_status_json_schema(&status);
    assert_eq!(status["summary"]["sessions_total"].as_u64(), Some(0));
    assert_eq!(status["summary"]["sessions_live"].as_u64(), Some(0));
    assert_eq!(status["summary"]["sessions_attention"].as_u64(), Some(0));
    assert_eq!(status["summary"]["open_tasks"].as_u64(), Some(0));
    assert_eq!(status["summary"]["open_alerts"].as_u64(), Some(0));
    assert_eq!(status["sessions"].as_array().map(Vec::len), Some(0));
    assert_eq!(status["tasks"].as_array().map(Vec::len), Some(0));
    assert_eq!(status["alerts"].as_array().map(Vec::len), Some(0));

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

fn assert_status_json_schema(status: &serde_json::Value) {
    let obj = status.as_object().expect("status JSON object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "alerts",
            "generated_at_ms",
            "schema_version",
            "sessions",
            "summary",
            "tasks",
        ]
    );
    assert_eq!(obj["schema_version"].as_u64(), Some(1));
    assert!(obj["generated_at_ms"].as_i64().is_some_and(|v| v > 0));
    let summary = obj["summary"].as_object().expect("status summary object");
    let mut sum_keys: Vec<&str> = summary.keys().map(String::as_str).collect();
    sum_keys.sort_unstable();
    assert_eq!(
        sum_keys,
        vec![
            "open_alerts",
            "open_tasks",
            "sessions_attention",
            "sessions_live",
            "sessions_total",
        ]
    );
    assert!(obj["sessions"].is_array());
    assert!(obj["tasks"].is_array());
    assert!(obj["alerts"].is_array());
}

fn seed_execution_session(
    store: &spawn_postgres::PostgresStore,
    agent_id: &str,
    session_id: &str,
    cmd_id: u64,
    keep_live: bool,
) {
    use spawn_core::event::{ENGINE, EndCause, EventKind, ModelProfile, NewEvent, Source};
    use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
    use spawn_core::port::EventLog;

    let agent = AgentId::new(agent_id).expect("valid agent");
    let session = SessionId::new(session_id).expect("valid session");
    let exec_id = ExecutionId::for_command(CommandId(cmd_id));
    store
        .append(NewEvent {
            agent_id: agent.clone(),
            session_id: session.clone(),
            execution_id: Some(exec_id.clone()),
            source: Source::Claude,
            source_event_id: Some(format!("start-{cmd_id}")),
            kind: EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
                command_id: CommandId(cmd_id),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
                launch: None,
            },
        })
        .expect("append execution.started");
    if !keep_live {
        store
            .append(NewEvent {
                agent_id: agent,
                session_id: session,
                execution_id: Some(exec_id),
                source: Source::Bridge,
                source_event_id: Some(format!("end-{cmd_id}")),
                kind: EventKind::ExecutionEnded {
                    cause: EndCause::Exited,
                    exit_code: Some(0),
                    signal: None,
                    evidence: None,
                },
            })
            .expect("append execution.ended");
    }
}

fn verify_status_contracts(env: &[(&str, &str)], mission_id: i64, task_id: i64) {
    let status = spawn_json(&["status", "--json"], env);
    assert_status_json_schema(&status);
    assert_eq!(status["summary"]["sessions_total"].as_u64(), Some(2));
    assert_eq!(status["summary"]["sessions_live"].as_u64(), Some(1));
    assert_eq!(status["summary"]["open_tasks"].as_u64(), Some(1));
    assert_eq!(status["summary"]["open_alerts"].as_u64(), Some(1));

    let sessions = status["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], "session_sup_live");
    assert_eq!(sessions[0]["agent_id"], "agent_supervisor");
    assert!(sessions[0].get("launch").is_none());

    let tasks = status["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], task_id);
    assert_eq!(tasks[0]["mission_id"], mission_id);
    assert_eq!(tasks[0]["state"], "assigned");
    assert_eq!(tasks[0]["title"], "Check operator overview first line");

    let alerts = status["alerts"].as_array().expect("alerts array");
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0]["key"], "op-cli-open-1");
    assert_eq!(alerts[0]["state"], "open");
    assert!(alerts[0].get("evidence").is_none());

    let status_human = spawn(&["status"], env);
    assert_eq!(status_human.status.code(), Some(0));
    let human_out = String::from_utf8_lossy(&status_human.stdout);
    assert!(human_out.contains("SPAWN STATUS"), "{human_out}");
    assert!(human_out.contains("SESSIONS (1 live)"), "{human_out}");
    assert!(human_out.contains("OPEN TASKS (1)"), "{human_out}");
    assert!(human_out.contains("OPEN ALERTS (1)"), "{human_out}");
    assert!(human_out.contains("session_sup_live"), "{human_out}");
    assert!(!human_out.contains("session_worker_stopped"), "{human_out}");
    assert!(
        human_out.contains("Check operator overview first line"),
        "{human_out}"
    );
    assert!(
        human_out.contains("op-cli-open-1") || human_out.contains("memory_low"),
        "{human_out}"
    );
    assert!(
        !human_out.contains("Cleared alert should stay hidden")
            && !human_out.contains("never-echoed"),
        "{human_out}"
    );
}

fn verify_say_contracts(env: &[(&str, &str)]) {
    let say_json = spawn_json(&["say", "agent_supervisor", "check main", "--json"], env);
    let mut say_keys: Vec<&str> = say_json
        .as_object()
        .expect("say object")
        .keys()
        .map(String::as_str)
        .collect();
    say_keys.sort_unstable();
    assert_eq!(
        say_keys,
        vec![
            "action",
            "agent_id",
            "command_id",
            "idempotency_key",
            "principal",
            "schema_version",
            "session_id",
        ]
    );
    assert_eq!(say_json["schema_version"].as_u64(), Some(1));
    assert!(say_json["command_id"].as_u64().is_some_and(|v| v >= 1));
    assert_eq!(say_json["agent_id"], "agent_supervisor");
    assert_eq!(say_json["session_id"], "session_sup_live");
    assert_eq!(say_json["principal"], "human");
    assert_eq!(say_json["action"], "input");
    assert!(
        say_json["idempotency_key"]
            .as_str()
            .is_some_and(|k| k.starts_with("say-"))
    );

    let say_plain = spawn(&["say", "agent_supervisor", "plain message"], env);
    assert_eq!(say_plain.status.code(), Some(0));
    let say_plain_out = String::from_utf8_lossy(&say_plain.stdout);
    assert!(
        say_plain_out.starts_with("command=")
            && say_plain_out.contains("agent=agent_supervisor session=session_sup_live key=say-"),
        "{say_plain_out}"
    );

    let missing_say = spawn(&["say", "agent_missing", "hello"], env);
    assert_eq!(missing_say.status.code(), Some(1));
    let missing_err = String::from_utf8_lossy(&missing_say.stderr);
    assert!(
        missing_err.contains("no active session found") && missing_err.contains("--session"),
        "{missing_err}"
    );
}

fn verify_submit_contracts(env: &[(&str, &str)]) {
    let explicit_submit = spawn(
        &[
            "submit",
            "--agent",
            "agent_supervisor",
            "--session",
            "session_sup_live",
            "--key",
            "explicit-submit-1",
            "--action",
            r#"{"type":"input","text":"explicit"}"#,
        ],
        env,
    );
    assert_eq!(explicit_submit.status.code(), Some(0));
    let explicit_out = String::from_utf8_lossy(&explicit_submit.stdout)
        .trim()
        .to_string();
    assert!(explicit_out.starts_with("command="), "{explicit_out}");

    let replay_submit = spawn(
        &[
            "submit",
            "--agent",
            "agent_supervisor",
            "--session",
            "session_sup_live",
            "--key",
            "explicit-submit-1",
            "--action",
            r#"{"type":"input","text":"explicit"}"#,
        ],
        env,
    );
    assert_eq!(replay_submit.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&replay_submit.stdout).trim(),
        explicit_out
    );

    let smart_submit = spawn(
        &[
            "submit",
            "--agent",
            "agent_supervisor",
            "--action",
            r#"{"type":"input","text":"smart submit"}"#,
        ],
        env,
    );
    assert_eq!(smart_submit.status.code(), Some(0));
    let smart_out = String::from_utf8_lossy(&smart_submit.stdout)
        .trim()
        .to_string();
    assert!(
        smart_out.starts_with("command=") && smart_out != explicit_out,
        "{smart_out}"
    );
}

fn verify_stopped_session_and_explicit_override_contracts(env: &[(&str, &str)]) {
    let stopped_say = spawn(&["say", "agent_worker_stopped", "should fail"], env);
    assert_eq!(stopped_say.status.code(), Some(1));
    let stopped_say_err = String::from_utf8_lossy(&stopped_say.stderr);
    assert!(
        stopped_say_err.contains("no active session found")
            && stopped_say_err.contains("session_worker_stopped")
            && stopped_say_err.contains("--session"),
        "{stopped_say_err}"
    );

    let stopped_submit = spawn(
        &[
            "submit",
            "--agent",
            "agent_worker_stopped",
            "--action",
            r#"{"type":"input","text":"should fail"}"#,
        ],
        env,
    );
    assert_eq!(stopped_submit.status.code(), Some(1));
    let stopped_submit_err = String::from_utf8_lossy(&stopped_submit.stderr);
    assert!(
        stopped_submit_err.contains("no active session found")
            && stopped_submit_err.contains("session_worker_stopped"),
        "{stopped_submit_err}"
    );

    let explicit_stopped_say = spawn_json(
        &[
            "say",
            "agent_worker_stopped",
            "targeted stopped session",
            "--session",
            "session_worker_stopped",
            "--key",
            "say-explicit-stopped-1",
            "--json",
        ],
        env,
    );
    assert_eq!(explicit_stopped_say["session_id"], "session_worker_stopped");
    assert_eq!(
        explicit_stopped_say["idempotency_key"],
        "say-explicit-stopped-1"
    );
}

#[test]
fn operator_cli_status_say_and_submit_smart_resolution_contracts() {
    let schema = spawn_postgres::testing::TestSchema::new("help_operator_cli");
    let store = schema.store();
    let db_url = schema_url(&schema);
    let env = [("SPAWN_DATABASE_URL", db_url.as_str())];

    seed_execution_session(&store, "agent_supervisor", "session_sup_live", 1, true);
    seed_execution_session(
        &store,
        "agent_worker_stopped",
        "session_worker_stopped",
        2,
        false,
    );

    let mission_id = store
        .mission_add(
            "Operator CLI status mission",
            &serde_json::json!([]),
            "human",
        )
        .expect("mission_add");
    let task_id = store
        .task_assign_with(
            mission_id,
            "ephemeral",
            "worker-1",
            "Check operator overview first line\nExtra detail line",
            "default",
            false,
            false,
        )
        .expect("task_assign_with");

    store
        .alert_record(&serde_json::json!({
            "type": "alert.fired",
            "key": "op-cli-open-1",
            "kind": "memory_low",
            "subject": "host",
            "text": "Open alert for status contract",
            "evidence": { "reading_ms": 1000, "secret_field": "never-echoed" }
        }))
        .expect("open alert");
    store
        .alert_record(&serde_json::json!({
            "type": "alert.fired",
            "key": "op-cli-cleared-1",
            "kind": "disk_low",
            "subject": "host",
            "text": "Cleared alert should stay hidden",
            "evidence": { "reading_ms": 1000 }
        }))
        .expect("cleared alert fire");
    store
        .alert_record(&serde_json::json!({
            "type": "alert.cleared",
            "key": "op-cli-cleared-1",
            "kind": "disk_low",
            "subject": "host",
            "text": "Cleared alert now",
            "cleared_evidence": { "reading_ms": 2000 }
        }))
        .expect("cleared alert clear");

    verify_status_contracts(&env, mission_id, task_id);
    verify_say_contracts(&env);
    verify_submit_contracts(&env);
    verify_stopped_session_and_explicit_override_contracts(&env);

    drop(store);
    drop(schema);
}

#[test]
fn task_review_and_land_help_contracts_are_locked() {
    let top = spawn(&["--help"], &[]);
    assert_eq!(top.status.code(), Some(0));
    let top_out = String::from_utf8_lossy(&top.stdout);
    assert!(
        top_out.contains("spawn task assign|accept|")
            && top_out.contains("stages|commits|diff|land"),
        "{top_out}"
    );

    let task_help = spawn(&["task", "--help"], &[]);
    assert_eq!(task_help.status.code(), Some(0));
    let task_out = String::from_utf8_lossy(&task_help.stdout);
    for expected in [
        "spawn task assign",
        "spawn task accept",
        "spawn task stages",
        "spawn task commits <id> [--json]",
        "spawn task diff <id> [--stat]",
        "spawn task land <id> [--ff-only | --squash] [--dry-run] [--json]",
    ] {
        assert!(task_out.contains(expected), "{expected}: {task_out}");
    }
}

fn run_test_git(cwd: Option<&std::path::Path>, args: &[&str]) -> String {
    let mut cmd = spawn_host::test_git_command();
    if let Some(path) = cwd {
        cmd.arg("-C").arg(path);
    }
    let out = cmd.args(args).output().expect("run test git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn bare_ref_sha(bare_repo: &std::path::Path, reference: &str) -> String {
    run_test_git(
        None,
        &[
            "--git-dir",
            bare_repo.to_str().expect("utf8"),
            "rev-parse",
            reference,
        ],
    )
}

fn push_branch_from_base(
    work: &std::path::Path,
    base_sha: &str,
    file: &str,
    content: &str,
    message: &str,
    checkpoint_msg: Option<&str>,
    dest_ref: &str,
) -> String {
    run_test_git(Some(work), &["checkout", "-B", "work-branch", base_sha]);
    std::fs::write(work.join(file), content).expect("write file");
    run_test_git(Some(work), &["add", file]);
    run_test_git(Some(work), &["commit", "-m", message]);
    if let Some(ckpt) = checkpoint_msg {
        std::fs::write(work.join(format!("{file}.ckpt")), "ckpt\n").expect("write ckpt");
        run_test_git(Some(work), &["add", "."]);
        run_test_git(Some(work), &["commit", "-m", ckpt]);
    }
    let sha = run_test_git(Some(work), &["rev-parse", "HEAD"]);
    run_test_git(
        Some(work),
        &["push", "origin", &format!("{sha}:{dest_ref}")],
    );
    sha
}

struct TaskLandFixture {
    bare: std::path::PathBuf,
    work: std::path::PathBuf,
    base_sha: String,
    t1: i64,
    t2: i64,
    t3: i64,
    t4: i64,
    t5_unpinned: i64,
    agent2: String,
    agent3: String,
}

fn setup_mission_repo_and_tasks(
    store: &spawn_postgres::PostgresStore,
    root: &std::path::Path,
) -> TaskLandFixture {
    let bare = root.join("mission.git");
    let work = root.join("seed");
    run_test_git(None, &["init", "--bare", bare.to_str().expect("utf8")]);
    run_test_git(
        None,
        &[
            "clone",
            bare.to_str().expect("utf8"),
            work.to_str().expect("utf8"),
        ],
    );
    run_test_git(Some(&work), &["checkout", "-B", "main"]);
    std::fs::write(work.join("README.md"), "base\n").expect("write readme");
    run_test_git(Some(&work), &["add", "README.md"]);
    run_test_git(Some(&work), &["commit", "-m", "initial commit"]);
    let base_sha = run_test_git(Some(&work), &["rev-parse", "HEAD"]);
    run_test_git(Some(&work), &["push", "origin", "main:refs/heads/main"]);

    let sources = serde_json::json!([bare.to_str().expect("utf8")]);
    let mission_id = store
        .mission_add("Task review and land E2E", &sources, "human")
        .expect("mission_add");
    let assign = |worker: &str, brief: &str| {
        store
            .task_assign_with(
                mission_id,
                "ephemeral",
                worker,
                brief,
                "default",
                false,
                false,
            )
            .expect("task_assign_with")
    };
    let t1 = assign("w1", "Implement feature one");
    let t2 = assign("w2", "Implement feature two");
    let t3 = assign("w3", "Fallback branch read");
    let t4 = assign("w4", "Conflicting update");
    let t5_unpinned = assign("w5", "Task without preserved ref");
    let agent_of = |task_id: i64| -> String {
        store.objective_task(task_id).unwrap().unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    TaskLandFixture {
        bare,
        work,
        base_sha,
        t1,
        t2,
        t3,
        t4,
        t5_unpinned,
        agent2: agent_of(t2),
        agent3: agent_of(t3),
    }
}

fn verify_commits_and_diff_contracts(
    env: &[(&str, &str)],
    fx: &TaskLandFixture,
) -> (String, String) {
    let t1_str = fx.t1.to_string();
    let t2_str = fx.t2.to_string();
    let t3_str = fx.t3.to_string();

    let head1 = push_branch_from_base(
        &fx.work,
        &fx.base_sha,
        "feat1.txt",
        "feature one\n",
        "Add feature one",
        Some("spawn checkpoint exec_1"),
        &spawn_host::git::task_head_ref(fx.t1, "head"),
    );
    let head2 = push_branch_from_base(
        &fx.work,
        &fx.base_sha,
        "feat2.txt",
        "feature two\n",
        "Add feature two",
        Some("spawn checkpoint exec_2"),
        &spawn_host::git::worker_head_ref(&fx.agent2),
    );
    let _head3 = push_branch_from_base(
        &fx.work,
        &fx.base_sha,
        "feat3.txt",
        "feature three\n",
        "Add feature three",
        None,
        &format!("refs/heads/spawn/{}", fx.agent3),
    );

    let commits_json = spawn_json(&["task", "commits", &t1_str, "--json"], env);
    assert_eq!(commits_json["schema_version"], 1);
    assert_eq!(commits_json["task_id"], fx.t1);
    assert_eq!(
        commits_json["resolved_ref"],
        spawn_host::git::task_head_ref(fx.t1, "head")
    );
    assert_eq!(commits_json["base_sha"], fx.base_sha);
    assert_eq!(commits_json["head_sha"], head1);
    assert_eq!(commits_json["commit_count"], 2);
    assert_eq!(commits_json["author_commit_count"], 1);
    assert_eq!(commits_json["checkpoint_commit_count"], 1);
    assert_eq!(commits_json["commits"][0]["kind"], "checkpoint");
    assert_eq!(commits_json["commits"][1]["kind"], "author");

    let commits_table = spawn(&["task", "commits", &t1_str], env);
    assert_eq!(commits_table.status.code(), Some(0));
    let table_out = String::from_utf8_lossy(&commits_table.stdout);
    assert!(table_out.contains("1 author, 1 checkpoint"), "{table_out}");
    assert!(table_out.contains("Add feature one"), "{table_out}");
    assert!(!table_out.contains("postgres://"), "{table_out}");

    let t2_json = spawn_json(&["task", "commits", &t2_str, "--json"], env);
    assert_eq!(
        t2_json["resolved_ref"],
        spawn_host::git::worker_head_ref(&fx.agent2)
    );
    let t3_json = spawn_json(&["task", "commits", &t3_str, "--json"], env);
    assert_eq!(
        t3_json["resolved_ref"],
        format!("refs/heads/spawn/{}", fx.agent3)
    );

    let diff_out = spawn(&["task", "diff", &t1_str], env);
    assert_eq!(diff_out.status.code(), Some(0));
    let diff_text = String::from_utf8_lossy(&diff_out.stdout);
    assert!(
        diff_text.contains("feat1.txt") && diff_text.contains("+feature one"),
        "{diff_text}"
    );

    let stat_out = spawn(&["task", "diff", &t1_str, "--stat"], env);
    assert_eq!(stat_out.status.code(), Some(0));
    let stat_text = String::from_utf8_lossy(&stat_out.stdout);
    assert!(
        stat_text.contains("feat1.txt") && stat_text.contains("insertions(+)"),
        "{stat_text}"
    );

    assert_eq!(bare_ref_sha(&fx.bare, "refs/heads/main"), fx.base_sha);
    (head1, head2)
}

fn verify_land_contracts(env: &[(&str, &str)], fx: &TaskLandFixture, head1: &str) -> String {
    let t1_str = fx.t1.to_string();
    let t2_str = fx.t2.to_string();

    let dry_ff = spawn_json(&["task", "land", &t1_str, "--dry-run", "--json"], env);
    assert_eq!(dry_ff["mode"], "ff-only");
    assert_eq!(dry_ff["dry_run"], true);
    assert_eq!(dry_ff["landed"], false);
    assert_eq!(dry_ff["can_land"], true);
    assert_eq!(bare_ref_sha(&fx.bare, "refs/heads/main"), fx.base_sha);

    let ff = spawn_json(&["task", "land", &t1_str, "--ff-only", "--json"], env);
    assert_eq!(ff["mode"], "ff-only");
    assert_eq!(ff["landed"], true);
    assert_eq!(ff["new_main_sha"], head1);
    assert_eq!(bare_ref_sha(&fx.bare, "refs/heads/main"), head1);

    let div_ff = spawn(&["task", "land", &t2_str, "--ff-only"], env);
    assert_eq!(div_ff.status.code(), Some(1));
    let both_flags = spawn(&["task", "land", &t2_str, "--ff-only", "--squash"], env);
    assert_eq!(both_flags.status.code(), Some(1));
    assert_eq!(bare_ref_sha(&fx.bare, "refs/heads/main"), head1);

    let dry_sq = spawn_json(
        &["task", "land", &t2_str, "--squash", "--dry-run", "--json"],
        env,
    );
    assert_eq!(dry_sq["mode"], "squash");
    assert_eq!(dry_sq["dry_run"], true);
    assert_eq!(dry_sq["landed"], false);
    assert!(dry_sq["new_main_sha"].is_null(), "{dry_sq}");
    assert_eq!(bare_ref_sha(&fx.bare, "refs/heads/main"), head1);

    let sq = spawn_json(&["task", "land", &t2_str, "--squash", "--json"], env);
    assert_eq!(sq["mode"], "squash");
    assert_eq!(sq["landed"], true);
    let new_main = bare_ref_sha(&fx.bare, "refs/heads/main");
    assert_ne!(new_main, head1);
    assert_eq!(sq["new_main_sha"], new_main);
    let bare_str = fx.bare.to_str().expect("utf8");
    let log_msg = run_test_git(
        None,
        &["--git-dir", bare_str, "log", "-1", "--format=%B", &new_main],
    );
    assert!(
        log_msg.contains("Add feature two") && log_msg.contains(&format!("Spawn-Task: {}", fx.t2)),
        "{log_msg}"
    );
    assert!(!log_msg.contains("spawn checkpoint"), "{log_msg}");
    let parents = run_test_git(
        None,
        &[
            "--git-dir",
            bare_str,
            "rev-list",
            "--parents",
            "-n",
            "1",
            &new_main,
        ],
    );
    assert_eq!(parents.split_whitespace().count(), 2, "{parents}");
    let all_refs = run_test_git(
        None,
        &["--git-dir", bare_str, "for-each-ref", "--format=%(refname)"],
    );
    assert!(!all_refs.contains("spawn-land"), "{all_refs}");
    new_main
}

fn verify_conflict_and_error_contracts(env: &[(&str, &str)], fx: &TaskLandFixture, new_main: &str) {
    let t4_str = fx.t4.to_string();
    let t5_str = fx.t5_unpinned.to_string();

    let _c_head = push_branch_from_base(
        &fx.work,
        &fx.base_sha,
        "README.md",
        "worker change\n",
        "Conflicting readme change",
        None,
        &spawn_host::git::task_head_ref(fx.t4, "head"),
    );
    run_test_git(Some(&fx.work), &["fetch", "origin", "main"]);
    run_test_git(Some(&fx.work), &["checkout", "-B", "main_adv", new_main]);
    std::fs::write(fx.work.join("README.md"), "main change\n").expect("write conflict main");
    run_test_git(Some(&fx.work), &["add", "README.md"]);
    run_test_git(Some(&fx.work), &["commit", "-m", "main readme change"]);
    let conflict_base = run_test_git(Some(&fx.work), &["rev-parse", "HEAD"]);
    run_test_git(
        Some(&fx.work),
        &[
            "push",
            "origin",
            &format!("{conflict_base}:refs/heads/main"),
        ],
    );

    for args in [
        &["task", "land", &t4_str, "--squash", "--dry-run"][..],
        &["task", "land", &t4_str, "--squash"],
    ] {
        let out = spawn(args, env);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(bare_ref_sha(&fx.bare, "refs/heads/main"), conflict_base);
    }

    let missing = spawn(&["task", "commits", "999999"], env);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));

    let no_ref = spawn(&["task", "commits", &t5_str], env);
    assert_eq!(no_ref.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&no_ref.stderr).contains("no preserved ref"));
}

#[test]
fn task_commits_diff_and_land_postgres_and_bare_git_contracts() {
    let schema = spawn_postgres::testing::TestSchema::new("help_task_land");
    let store = schema.store();
    let db_url = schema_url(&schema);
    let path_env = std::env::var("PATH").unwrap_or_default();
    let env = [
        ("SPAWN_DATABASE_URL", db_url.as_str()),
        ("PATH", path_env.as_str()),
        ("TMPDIR", "/tmp"),
    ];

    let scratch = ScratchDir(
        std::path::PathBuf::from("/tmp")
            .join(format!("spawn-help-task-land-{}", std::process::id())),
    );
    let _ = std::fs::remove_dir_all(&scratch.0);
    std::fs::create_dir_all(&scratch.0).expect("scratch dir");

    let fx = setup_mission_repo_and_tasks(&store, &scratch.0);
    let (head1, _head2) = verify_commits_and_diff_contracts(&env, &fx);
    let new_main = verify_land_contracts(&env, &fx, &head1);
    verify_conflict_and_error_contracts(&env, &fx, &new_main);

    drop(store);
    drop(schema);
}

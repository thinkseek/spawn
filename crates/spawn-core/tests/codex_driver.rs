//! Offline conformance tests for [`CodexDriver`] (#91 P2 / #113 D1).

use serde_json::json;
use spawn_core::driver::{
    AGY_DIALOG_RULES, AgentDriver, AgyDriver, Answer, CLAUDE_DIALOG_RULES, CODEX_DIALOG_RULES,
    CODEX_ENV_ALLOWLIST, CODEX_HARD_BLOCK_RULES, CODEX_HOME, ClaudeDriver, CodexDriver,
    DIALOG_RULES, Dialog, DriverTraits, EngineHome, EngineLaunch, Guard, LineCursor, NativeSession,
    NativeSessionHook, SessionBinding, TurnSource, codex_arguments, codex_line, driver, drivers,
    excerpt_for, hard_block_for, parse_cli_version, read_codex_screen, table_answer,
    version_matches_tested,
};
use spawn_core::driver_codex::{
    CODEX_ID, CODEX_SPEC, CODEX_TESTED, CODEX_TITLE, filter_codex_env, is_codex_trust_dialog,
    is_safe_codex_model, is_safe_codex_session_id,
};
use spawn_core::event::{EventKind, ModelProfile};
use spawn_core::id::{CommandId, ExecutionId, WorkspaceId};
use spawn_core::screen::ScreenState;

fn codex_home() -> EngineHome {
    EngineHome {
        program: "/home/u/.local/bin/codex".to_string(),
        sessions: "/home/u/.codex/".to_string(),
        model: Some("gpt-5.4".to_string()),
        data_dir: None,
        isolate_brain: false,
    }
}

fn ws() -> WorkspaceId {
    WorkspaceId::new("3:/workspaces/agent_codex").expect("workspace")
}

#[test]
fn registry_registers_codex_as_third_driver_and_keeps_claude_default() {
    assert_eq!(drivers().len(), 3);
    assert_eq!(drivers()[0].id(), "claude");
    assert_eq!(drivers()[1].id(), "agy");
    assert_eq!(drivers()[2].id(), CODEX_ID);

    let registered = driver("codex").expect("codex registered");
    assert_eq!(registered.id(), "codex");
    assert_eq!(registered.title(), CODEX_TITLE);
    assert_eq!(
        registered.traits(),
        DriverTraits {
            session: SessionBinding::Reported,
            turns: TurnSource::Transcript,
            guard: Guard::Sandbox,
        }
    );
    assert_eq!(CODEX_HOME, ".codex");
    assert!(driver("CODEX").is_none());
    let execution = ExecutionId::for_command(CommandId(11));
    assert_eq!(registered.new_session_id(&execution), None);
}

#[test]
fn launch_builds_fresh_and_resume_args_and_strict_env_allowlist() {
    let home = codex_home();
    let fresh = CodexDriver.launch(
        &home,
        &EngineLaunch {
            session: NativeSession::New("should_not_appear_without_resume"),
            relay: None,
            profile: ModelProfile::Default,
            config_dir: None,
            cwd: Some("/workspaces/agent_codex"),
            model: Some("gpt-5.4-mini"),
        },
    );
    assert_eq!(fresh.program, "/home/u/.local/bin/codex");
    assert_eq!(
        fresh.arguments,
        vec![
            "--model",
            "gpt-5.4-mini",
            "--cd",
            "/workspaces/agent_codex",
            "--dangerously-bypass-approvals-and-sandbox",
            "--no-alt-screen",
            "--no-daemon",
        ]
    );
    assert_eq!(
        fresh.environment,
        vec![("CODEX_HOME".to_string(), "/home/u/.codex".to_string())]
    );
    assert!(fresh.settings.is_empty());

    let resumed = CodexDriver.launch(
        &home,
        &EngineLaunch {
            session: NativeSession::Resume("0194b1e2-7c4d-7000-8000-000000000001"),
            relay: None,
            profile: ModelProfile::Default,
            config_dir: None,
            cwd: Some("/workspaces/agent_codex"),
            model: None,
        },
    );
    assert_eq!(
        resumed.arguments,
        vec![
            "resume",
            "--model",
            "gpt-5.4",
            "--cd",
            "/workspaces/agent_codex",
            "--dangerously-bypass-approvals-and-sandbox",
            "--no-alt-screen",
            "--no-daemon",
            "0194b1e2-7c4d-7000-8000-000000000001",
        ]
    );

    // Unsafe flag-injection resume id / unsafe model / leading-dash cwd are dropped fail-closed.
    assert_eq!(
        codex_arguments(Some("--evil-flag"), Some("/tmp/ws"), Some("--danger-id"),),
        vec![
            "--cd",
            "/tmp/ws",
            "--dangerously-bypass-approvals-and-sandbox",
            "--no-alt-screen",
            "--no-daemon",
        ]
    );
    for bad_cwd in ["--evil-cwd", "-C", " -c "] {
        assert_eq!(
            codex_arguments(Some("gpt-5.4"), Some(bad_cwd), None),
            vec![
                "--model",
                "gpt-5.4",
                "--dangerously-bypass-approvals-and-sandbox",
                "--no-alt-screen",
                "--no-daemon",
            ],
            "{bad_cwd:?}"
        );
    }
    assert!(!is_safe_codex_model("-o"));
    assert!(!is_safe_codex_model("bad model"));
    assert!(is_safe_codex_model("gpt-5.4"));
    assert!(is_safe_codex_model("o3"));

    // Allowlist strictly strips secrets such as OPENAI_API_KEY and control chars.
    assert!(!CODEX_ENV_ALLOWLIST.contains(&"OPENAI_API_KEY"));
    let filtered = filter_codex_env(&[
        ("OPENAI_API_KEY", "sk-proj-secret123456"),
        ("AWS_SECRET_ACCESS_KEY", "topsecret"),
        ("CODEX_HOME", " /home/u/.codex "),
        ("TERM", "xterm-256color"),
        ("HOME", "/bad\nline"),
        ("NO_COLOR", ""),
    ]);
    assert_eq!(
        filtered,
        vec![
            ("CODEX_HOME".to_string(), "/home/u/.codex".to_string()),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ]
    );
}

#[test]
fn transcript_path_session_scan_and_reported_session_validation() {
    let home = codex_home();
    let codex = driver(CODEX_ID).expect("codex registered");
    assert_eq!(
        codex.session_scan_dir(&home).as_deref(),
        Some("/home/u/.codex/sessions")
    );
    assert_eq!(
        codex.transcript_path(&home, &ws(), "0194b1e2-7c4d-7000-8000-000000000001"),
        "/home/u/.codex/sessions/0194b1e2-7c4d-7000-8000-000000000001.jsonl"
    );
    // Traversal cannot escape `<sessions>/sessions/`.
    let escaped = codex.transcript_path(&home, &ws(), "../etc/passwd");
    assert!(!escaped.contains(".."), "{escaped}");

    for bad in ["", "../x", "a/b", "-flag", ".hidden", "word"] {
        assert!(!is_safe_codex_session_id(bad) || bad == "word", "{bad:?}");
    }

    assert_eq!(
        codex.reported_session(
            r#"{"type":"thread.started","thread_id":"0194b1e2-7c4d-7000-8000-000000000001"}"#
        ),
        Some("0194b1e2-7c4d-7000-8000-000000000001".to_string())
    );
    assert_eq!(
        codex.reported_session(r#"{"type":"session_meta","payload":{"id":"thr_0194b1e2"}}"#),
        Some("thr_0194b1e2".to_string())
    );
    assert_eq!(
        codex
            .reported_session("\u{1b}[2mSession id: 0194b1e2-7c4d-7000-8000-000000000001\u{1b}[0m"),
        Some("0194b1e2-7c4d-7000-8000-000000000001".to_string())
    );
    assert_eq!(
        codex.reported_session("Resume this session with codex resume thr_0194b1e2,"),
        Some("thr_0194b1e2".to_string())
    );
    assert_eq!(codex.reported_session("Session id: none"), None);
    assert_eq!(
        codex.reported_session("Session id: ../escape-attempt"),
        None
    );
}

#[test]
fn codex_line_parses_exec_json_with_redaction() {
    let mut cursor = LineCursor::default();

    let meta = CodexDriver.read_line(
        r#"{"timestamp":"2026-11-27T01:00:00.000Z","type":"session_meta","payload":{"id":"thr_1234abcd","cli_version":"0.171.0"}}"#,
        &mut cursor,
    );
    assert_eq!(meta.version.as_deref(), Some("0.171.0"));
    assert!(meta.events.is_empty());

    // exec --json tool start + completed + turn usage/stop
    let started = codex_line(
        r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"cargo test -p spawn-core --token=supersecret123","status":"in_progress"}}"#,
        &mut cursor,
    );
    assert_eq!(started.events.len(), 1);
    assert_eq!(
        started.events[0].source_event_id.as_deref(),
        Some("item_1:started")
    );
    match &started.events[0].kind {
        EventKind::ToolStarted {
            tool_name,
            tool_use_id,
            summary,
            ..
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(tool_use_id, "item_1");
            assert_eq!(summary.as_deref(), Some("cargo test …"));
            assert!(
                !summary
                    .as_deref()
                    .unwrap_or_default()
                    .contains("supersecret123"),
                "{summary:?}"
            );
        }
        other => panic!("expected ToolStarted, got {other:?}"),
    }

    let failed = codex_line(
        r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","status":"failed","exit_code":101,"error":{"message":"Authorization: Bearer secret-bearer-token-999 failed"}}}"#,
        &mut cursor,
    );
    assert_eq!(
        failed.events[0].source_event_id.as_deref(),
        Some("item_1:ended")
    );
    match &failed.events[0].kind {
        EventKind::ToolFailed { tool_use_id, error } => {
            assert_eq!(tool_use_id, "item_1");
            assert!(!error.contains("secret-bearer-token-999"), "{error}");
        }
        other => panic!("expected ToolFailed, got {other:?}"),
    }

    let turn_done = codex_line(
        r#"{"type":"turn.completed","timestamp":"2026-11-27T01:01:00.000Z","model":"gpt-5.4","usage":{"input_tokens":1200,"cached_input_tokens":300,"output_tokens":85}}"#,
        &mut cursor,
    );
    assert_eq!(turn_done.events.len(), 2);
    assert!(matches!(
        &turn_done.events[0].kind,
        EventKind::ModelUsage {
            model,
            input_tokens: 1200,
            output_tokens: 85,
            cache_write_tokens: 0,
            cache_read_tokens: 300,
            ..
        } if model == "gpt-5.4"
    ));
    assert!(matches!(
        &turn_done.events[1].kind,
        EventKind::TurnStopped {
            input_tokens: Some(1200),
            output_tokens: Some(85),
        }
    ));
}

#[test]
fn codex_line_parses_rollout_jsonl_and_ignores_non_json() {
    let mut cursor = LineCursor::default();

    // Rollout format: user marker, function_call, function_call_output, token_count, task_complete
    let user = codex_line(
        r#"{"timestamp":"2026-11-27T01:02:00.000Z","type":"event_msg","payload":{"type":"user_message","message":"[spawn message #42] finish task"}}"#,
        &mut cursor,
    );
    assert!(matches!(user.events[0].kind, EventKind::MessageReceived));
    assert!(matches!(
        user.events[1].kind,
        EventKind::MessageTaken {
            command_id: CommandId(42)
        }
    ));

    let fn_call = codex_line(
        &json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "exec_command",
                "call_id": "call_99",
                "arguments": "{\"cmd\":\"git status --short\"}"
            }
        })
        .to_string(),
        &mut cursor,
    );
    assert!(matches!(
        &fn_call.events[0].kind,
        EventKind::ToolStarted { tool_name, tool_use_id, .. }
            if tool_name == "Bash" && tool_use_id == "call_99"
    ));

    let fn_out = codex_line(
        &json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_99",
                "output": "{\"output\":\"ok\",\"metadata\":{\"exit_code\":0}}"
            }
        })
        .to_string(),
        &mut cursor,
    );
    assert!(matches!(
        &fn_out.events[0].kind,
        EventKind::ToolCompleted { tool_use_id, .. } if tool_use_id == "call_99"
    ));

    let non_json = codex_line("plain terminal prose line", &mut cursor);
    assert!(non_json.events.is_empty());
    assert!(non_json.version.is_none());
}

#[test]
fn screen_classification_and_pii_masking_cover_all_codex_states() {
    let ready = "\
• Fixed the parser.\n\
\n\
› \n\
  gpt-5.4 high · 92% context left · ? for shortcuts\n";
    let codex = driver(CODEX_ID).expect("codex registered");
    assert_eq!(codex.read_screen(ready).state, ScreenState::Ready);

    let placeholder_ready = "\
› Ask Codex to do anything\n\
  ? for shortcuts · workspace-write\n";
    assert_eq!(
        read_codex_screen(placeholder_ready).state,
        ScreenState::Ready
    );

    let drafted = "\
› implement driver_codex\n\
  ? for shortcuts · 95% context left\n";
    assert_eq!(read_codex_screen(drafted).state, ScreenState::Drafted);

    let busy = "\
• Working on crates/spawn-core/src/driver_codex.rs (12s • esc to interrupt)\n\
› \n\
  gpt-5.4 · ? for shortcuts\n";
    assert_eq!(read_codex_screen(busy).state, ScreenState::Busy);

    let err_at_prompt = "\
■ API error: unexpected status 503 Service Unavailable\n\
› \n\
  ? for shortcuts\n";
    assert_eq!(read_codex_screen(err_at_prompt).state, ScreenState::Error);

    let permission = "\
Allow command?\n\
  $ git push origin main\n\
\n\
▌ 1. Yes, allow once\n\
  2. Yes, always allow this command for this session\n\
  3. No, provide feedback\n\
\n\
Press Enter to confirm or Esc to cancel\n";
    assert_eq!(read_codex_screen(permission).state, ScreenState::Permission);

    let trust = "\
Do you trust the contents of this directory?\n\
/workspaces/agent_codex\n\
1. Yes, continue\n\
2. No, quit\n";
    assert!(is_codex_trust_dialog(trust));
    assert_eq!(read_codex_screen(trust).state, ScreenState::Unknown);
    assert_eq!(codex.startup_answer(trust), None);

    // Unrecognized plain output stays fail-closed Unknown (never Ready).
    assert_eq!(
        read_codex_screen("> just prose\n").state,
        ScreenState::Unknown
    );
    assert_eq!(read_codex_screen("").state, ScreenState::Unknown);

    // PII & API key masking on excerpts while preserving legitimate hyphenated words like disk-usage.
    let raw_excerpt = "\
Account: user@example.invalid (Pro)\n\
OPENAI_API_KEY=sk-proj-abc123456789\n\
sk-testsecret987654321\n\
checking disk-usage and ok output\n";
    let masked = excerpt_for(Some("codex"), raw_excerpt);
    assert!(!masked.contains("user@example.invalid"), "{masked}");
    assert!(!masked.contains("sk-proj-abc123456789"), "{masked}");
    assert!(!masked.contains("sk-testsecret987654321"), "{masked}");
    assert!(masked.contains("disk-usage"), "{masked}");
}

#[test]
fn dialog_hard_block_start_refusal_and_version_checks() {
    let codex = driver(CODEX_ID).expect("codex registered");
    let dialog = Dialog {
        rule: Some("trust_directory"),
        kind: "trust",
        cursor: None,
        choices: &["1. Yes".to_string(), "2. No".to_string()],
        body: &[],
    };
    assert!(matches!(
        codex.dialog_answer(&dialog, &ws()),
        Answer::Escalate(_)
    ));
    assert!(matches!(
        table_answer("codex", &dialog, &ws()),
        Answer::Escalate(_)
    ));
    assert!(!CODEX_DIALOG_RULES.is_empty());
    assert!(!CODEX_HARD_BLOCK_RULES.is_empty());

    assert_eq!(
        spawn_core::driver::eval_hard_block_rules(
            CODEX_HARD_BLOCK_RULES,
            "Error: Please run /login to authenticate Codex",
            &[],
        ),
        Some("login required")
    );
    assert!(
        codex
            .start_refusal("Error: The model `gpt-99` does not exist or you do not have access")
            .is_some()
    );
    assert!(
        codex
            .start_refusal("Error: missing OPENAI_API_KEY")
            .is_some()
    );
    assert_eq!(codex.start_refusal("Ready."), None);

    assert_eq!(
        CodexDriver::version("codex-cli 0.171.0\n").as_deref(),
        Some("0.171.0")
    );
    assert!(CodexDriver::version_tested("0.171.0"));
    assert!(!CodexDriver::version_tested("0.99.0"));
    assert_eq!(CODEX_TESTED, ["0.171"]);
}

#[test]
fn three_hook_contract_holds_for_all_registered_drivers() {
    let exec = ExecutionId::for_command(CommandId(77));
    for d in drivers() {
        let spec = d.spec();
        assert_eq!(d.id(), spec.id);
        assert_eq!(d.title(), spec.title);
        assert_eq!(d.traits(), spec.traits);
        assert_eq!(d.tested_versions(), spec.tested_versions);
        let hook = d.session_binding();
        assert_eq!(hook.binding(), spec.traits.session, "{}", d.id());
        match spec.traits.session {
            SessionBinding::Assigned => {
                assert!(matches!(hook, NativeSessionHook::Assigned { .. }));
                assert!(d.new_session_id(&exec).is_some());
                assert_eq!(d.reported_session("{}"), None);
            }
            SessionBinding::Reported => {
                assert!(matches!(hook, NativeSessionHook::Reported { .. }));
                assert_eq!(d.new_session_id(&exec), None);
            }
        }
    }

    assert_eq!(driver(CODEX_ID).unwrap().spec().id, CODEX_SPEC.id);
    assert_eq!(
        parse_cli_version("codex-cli v0.171.2\n").as_deref(),
        Some("0.171.2")
    );
    assert!(version_matches_tested("0.171.2", &CODEX_TESTED));
    assert!(!version_matches_tested("0.172.0", &CODEX_TESTED));
    assert_eq!(
        hard_block_for(Some("codex"), "Rate limit reached · codex login", &[]),
        Some("login required")
    );
    assert_eq!(
        hard_block_for(Some("agy"), "Quota exceeded", &[]),
        Some("quota reached")
    );

    // Ensure master DIALOG_RULES table stays a strict superset of each engine's static table.
    for row in CLAUDE_DIALOG_RULES
        .iter()
        .chain(AGY_DIALOG_RULES)
        .chain(CODEX_DIALOG_RULES)
    {
        assert!(
            DIALOG_RULES.contains(row),
            "missing row in DIALOG_RULES: {row:?}"
        );
    }
}

#[test]
fn driver_isolation_contracts_enforced_across_all_three_engines() {
    let claude = driver("claude").unwrap();
    let agy = driver("agy").unwrap();
    let codex = driver("codex").unwrap();

    // 1. Claude enforces Guard::Settings with crossSessionInbound: refuse + managed config deny.
    assert_eq!(claude.traits().guard, Guard::Settings);
    let claude_home = EngineHome {
        program: "/usr/bin/claude".to_string(),
        sessions: "/srv/claude-config/projects".to_string(),
        model: None,
        data_dir: None,
        isolate_brain: false,
    };
    let claude_plan = claude.launch(
        &claude_home,
        &EngineLaunch {
            session: NativeSession::Unnamed,
            relay: None,
            profile: ModelProfile::Default,
            config_dir: Some("/srv/claude-config"),
            cwd: Some("/workspaces/w1"),
            model: None,
        },
    );
    let settings_json: serde_json::Value =
        serde_json::from_str(&claude_plan.settings).expect("json");
    assert_eq!(settings_json["crossSessionInbound"], "refuse");
    let deny = settings_json["permissions"]["deny"]
        .as_array()
        .expect("deny");
    assert!(
        deny.iter()
            .any(|v| v == "Read(//srv/claude-config/settings*)")
    );
    assert!(deny.iter().any(|v| v == "Bash(*CLAUDE_CONFIG_DIR*)"));

    // 2. Both non-Settings engines require kernel bwrap sandbox isolation.
    assert_eq!(agy.traits().guard, Guard::Sandbox);
    assert_eq!(codex.traits().guard, Guard::Sandbox);

    // 3. Codex launch & env filter scrub secrets and sibling engine environment variables.
    let scrubbed = filter_codex_env(&[
        ("CODEX_HOME", "/srv/codex-w1"),
        ("OPENAI_API_KEY", "sk-secret"),
        ("ANTHROPIC_AUTH_TOKEN", "anth-secret"),
        ("CLAUDE_CONFIG_DIR", "/home/u/.claude"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("SPAWN_TEST_DATABASE_URL", "postgres://invalid"),
        ("SPAWN_AGY_DATA_DIR", "/srv/agy"),
    ]);
    assert_eq!(
        scrubbed,
        vec![("CODEX_HOME".to_string(), "/srv/codex-w1".to_string())]
    );
    assert_eq!(
        ClaudeDriver
            .launch(
                &claude_home,
                &EngineLaunch {
                    session: NativeSession::Unnamed,
                    relay: None,
                    profile: ModelProfile::Default,
                    config_dir: None,
                    cwd: None,
                    model: None,
                }
            )
            .environment,
        Vec::<(String, String)>::new()
    );
    assert_eq!(
        AgyDriver
            .launch(
                &codex_home(),
                &EngineLaunch {
                    session: NativeSession::Unnamed,
                    relay: None,
                    profile: ModelProfile::Default,
                    config_dir: None,
                    cwd: Some("/workspaces/w2"),
                    model: Some("gemini-3.8-flash-low"),
                }
            )
            .environment,
        Vec::<(String, String)>::new()
    );
}

/// Each [`NativeSession`] gives every driver the arguments the launch gave
/// before `(native, resume)` became one closed type, read from the base
/// (0d400b2e) and written out here. The old `(None, true)`, which no longer
/// has a value, launched exactly like `Unnamed` in all three.
#[test]
fn native_session_launch_arguments_match_the_pre_refactor_launch() {
    let home = |program: &str| EngineHome {
        program: program.to_string(),
        sessions: "/h/s".to_string(),
        model: Some("m-1".to_string()),
        data_dir: None,
        isolate_brain: false,
    };
    let launch = |driver: &dyn AgentDriver, session| {
        driver
            .launch(
                &home(driver.id()),
                &EngineLaunch {
                    session,
                    relay: None,
                    profile: ModelProfile::Default,
                    config_dir: Some("/cfg"),
                    cwd: Some("/w"),
                    model: None,
                },
            )
            .arguments
    };
    let settings =
        spawn_core::bridge::claude_settings_in(None, ModelProfile::Default, Some("/cfg"));
    let claude_base = [
        "--ax-screen-reader",
        "--allow-dangerously-skip-permissions",
        "--settings",
        settings.as_str(),
    ];
    let agy_flags = [
        "--model",
        "m-1",
        "--dangerously-skip-permissions",
        "--settings",
        r#"{"toolPermission":"always-proceed","trustedWorkspaces":["/w"]}"#,
    ];
    let codex_flags = [
        "--model",
        "m-1",
        "--cd",
        "/w",
        "--dangerously-bypass-approvals-and-sandbox",
        "--no-alt-screen",
        "--no-daemon",
    ];
    let cases = [
        (
            NativeSession::Unnamed,
            claude_base.to_vec(),
            agy_flags.to_vec(),
            codex_flags.to_vec(),
        ),
        (
            NativeSession::New("sid-1"),
            [&claude_base[..], &["--session-id", "sid-1"]].concat(),
            agy_flags.to_vec(),
            codex_flags.to_vec(),
        ),
        (
            NativeSession::Resume("sid-1"),
            [&claude_base[..], &["--resume", "sid-1"]].concat(),
            [&["--conversation", "sid-1"][..], &agy_flags].concat(),
            [&["resume"][..], &codex_flags, &["sid-1"]].concat(),
        ),
    ];
    for (session, claude, agy, codex) in cases {
        assert_eq!(launch(&ClaudeDriver, session), claude, "claude {session:?}");
        assert_eq!(launch(&AgyDriver, session), agy, "agy {session:?}");
        assert_eq!(launch(&CodexDriver, session), codex, "codex {session:?}");
    }
}

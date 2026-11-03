use serde_json::json;

use spawn_engine::{
    ClaudeHookInput, CodexStreamEvent, build_launch, normalize_claude_hook, normalize_codex_event,
};
use spawn_types::engine::Engine;
use spawn_types::event::EventKind;

#[test]
fn maps_claude_tool_lifecycle() {
    let pre: ClaudeHookInput = serde_json::from_value(json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-1",
        "tool_name": "Bash",
        "tool_input": { "tool_use_id": "tu-9", "command": "ls" }
    }))
    .unwrap();
    let started = normalize_claude_hook(&pre).unwrap();
    assert!(matches!(started.kind, EventKind::ToolStarted { .. }));
    assert_eq!(
        started.source_event_id.as_deref(),
        Some("claude:tool:tu-9:PreToolUse")
    );

    let post: ClaudeHookInput = serde_json::from_value(json!({
        "hook_event_name": "PostToolUse",
        "session_id": "sess-1",
        "tool_name": "Bash",
        "tool_response": { "tool_use_id": "tu-9" }
    }))
    .unwrap();
    let completed = normalize_claude_hook(&post).unwrap();
    assert!(matches!(completed.kind, EventKind::ToolCompleted { .. }));
}

#[test]
fn maps_claude_session_start_and_stop() {
    let start: ClaudeHookInput = serde_json::from_value(json!({
        "hook_event_name": "SessionStart",
        "session_id": "sess-7",
        "transcript_path": "/tmp/rollout.jsonl"
    }))
    .unwrap();
    let event = normalize_claude_hook(&start).unwrap();
    match event.kind {
        EventKind::NativeSessionStarted {
            engine,
            native_session_id,
            ..
        } => {
            assert_eq!(engine, Engine::ClaudeCode);
            assert_eq!(native_session_id, "sess-7");
        }
        other => panic!("unexpected {other:?}"),
    }

    let stop: ClaudeHookInput = serde_json::from_value(json!({
        "hook_event_name": "Stop",
        "session_id": "sess-7"
    }))
    .unwrap();
    assert!(matches!(
        normalize_claude_hook(&stop).unwrap().kind,
        EventKind::TurnStopped { .. }
    ));
}

#[test]
fn rejects_unknown_claude_hook() {
    let unknown: ClaudeHookInput = serde_json::from_value(json!({
        "hook_event_name": "PreCompact",
        "session_id": "sess-1"
    }))
    .unwrap();
    assert!(normalize_claude_hook(&unknown).is_err());
}

#[test]
fn maps_codex_stream_events() {
    let thread: CodexStreamEvent = serde_json::from_value(json!({
        "type": "thread.started",
        "thread_id": "thread-3",
        "rollout_path": "/tmp/codex.jsonl"
    }))
    .unwrap();
    let started = normalize_codex_event(&thread).unwrap();
    assert!(matches!(
        started.kind,
        EventKind::NativeSessionStarted { .. }
    ));

    let item_start: CodexStreamEvent = serde_json::from_value(json!({
        "type": "item.started",
        "id": "item-1",
        "name": "shell.exec",
        "status": "in_progress"
    }))
    .unwrap();
    assert!(matches!(
        normalize_codex_event(&item_start).unwrap().kind,
        EventKind::ToolStarted { .. }
    ));

    let item_fail: CodexStreamEvent = serde_json::from_value(json!({
        "type": "item.completed",
        "id": "item-1",
        "name": "shell.exec",
        "status": "failed",
        "error": "exit 2"
    }))
    .unwrap();
    assert!(matches!(
        normalize_codex_event(&item_fail).unwrap().kind,
        EventKind::ToolFailed { .. }
    ));
}

#[test]
fn builds_engine_launch_with_resume() {
    let claude = build_launch(
        Engine::ClaudeCode,
        "/workspace/mnt",
        "hi",
        Some("native-1"),
        "spawn hook",
    )
    .unwrap();
    assert_eq!(claude.program, "claude");
    assert!(claude.arguments.contains(&"--resume".to_string()));
    assert!(claude.arguments.contains(&"native-1".to_string()));

    let codex = build_launch(Engine::Codex, "/workspace/mnt", "", None, "spawn hook").unwrap();
    assert_eq!(codex.program, "codex");
    assert!(codex.arguments.iter().any(|arg| arg == "--json"));
}

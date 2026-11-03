//! Throughput baseline for `ingest`. Run with
//! `cargo run --release -p spawn-core --example ingest_bench`.
// ponytail: stdlib timer, no criterion; add criterion when results need statistical comparison.

use std::hint::black_box;
use std::time::Instant;

use spawn_core::ingest::{claude_hook, codex_line};

const ITERS: u32 = 200_000;

const CLAUDE: &[(&str, &str)] = &[
    (
        "pre_tool_use",
        r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_use_id":"toolu_01","tool_input":{"command":"ls -la"}}"#,
    ),
    (
        "post_tool_use",
        r#"{"hook_event_name":"PostToolUse","session_id":"s1","tool_use_id":"toolu_01","duration_ms":42,"tool_response":{"stdout":"a\nb\nc"}}"#,
    ),
    (
        "ignored",
        r#"{"hook_event_name":"SubagentStop","session_id":"s1"}"#,
    ),
];

const CODEX: &[(&str, &str)] = &[
    (
        "item_completed",
        r#"{"type":"item.completed","item":{"id":"item_3","type":"command_execution","status":"completed","command":"ls","aggregated_output":"a\nb"}}"#,
    ),
    (
        "turn_completed",
        r#"{"type":"turn.completed","usage":{"input_tokens":1200,"output_tokens":340}}"#,
    ),
    (
        "agent_message",
        r#"{"type":"item.completed","item":{"id":"item_4","type":"agent_message","text":"done"}}"#,
    ),
];

fn bench(name: &str, payload: &str, f: fn(&str) -> bool) {
    assert!(f(payload), "{name}: payload failed to parse");
    let start = Instant::now();
    for _ in 0..ITERS {
        black_box(f(black_box(payload)));
    }
    let ns = start.elapsed().as_nanos() / u128::from(ITERS);
    println!("{name:<24} {ns:>6} ns/op  {:>5} B", payload.len());
}

fn main() {
    for (name, payload) in CLAUDE {
        bench(&format!("claude/{name}"), payload, |p| {
            claude_hook(p).is_ok()
        });
    }
    for (name, payload) in CODEX {
        bench(&format!("codex/{name}"), payload, |p| codex_line(p).is_ok());
    }
}

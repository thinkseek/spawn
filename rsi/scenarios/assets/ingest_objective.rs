//! Controller-owned bench for the ingest-speed objective (`rsi/backlog/002.json`).
//!
//! The scenario copies this file into each tree under test as an example, so
//! a candidate cannot change how it is measured. Payloads are generated at run
//! time from `RSI_SEED`, so a candidate cannot special-case fixed inputs.
//!
//! `check` prints one canonical line per payload (the serialized event, or
//! `none` / `error`). Baseline and candidate must print identical lines for the
//! same seed: faster ingest that changes what is ingested does not count.
//!
//! `time` prints `total_ns <n>`: the sum over the timed shapes of the mean
//! ns per call.

use std::hint::black_box;
use std::time::Instant;

use spawn_core::ingest::{Normalized, claude_hook, codex_line};

const ITERS: u32 = 100_000;

/// Small deterministic generator; no `rand` dependency in the tree.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn word(&mut self, max: usize) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-./ ";
        let length = 1 + (self.next() as usize) % max;
        (0..length)
            .map(|_| ALPHABET[(self.next() as usize) % ALPHABET.len()] as char)
            .collect()
    }

    fn id(&mut self, prefix: &str) -> String {
        format!("{prefix}_{:016x}", self.next() ^ (self.next() << 31))
    }
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("string serializes")
}

/// The six timed shapes, with fresh ids and text.
fn timed(rng: &mut Lcg) -> Vec<(bool, String)> {
    let session = rng.id("s");
    let tool = rng.id("toolu");
    vec![
        (true, format!(
            r#"{{"hook_event_name":"PreToolUse","session_id":{},"tool_name":"Bash","tool_use_id":{},"tool_input":{{"command":{}}}}}"#,
            quoted(&session), quoted(&tool), quoted(&rng.word(24)))),
        (true, format!(
            r#"{{"hook_event_name":"PostToolUse","session_id":{},"tool_use_id":{},"duration_ms":{},"tool_response":{{"stdout":{}}}}}"#,
            quoted(&session), quoted(&tool), rng.next() % 100_000, quoted(&rng.word(40)))),
        (true, format!(
            r#"{{"hook_event_name":"SubagentStop","session_id":{}}}"#, quoted(&session))),
        (false, format!(
            r#"{{"type":"item.completed","item":{{"id":{},"type":"command_execution","status":"completed","command":{},"aggregated_output":{}}}}}"#,
            quoted(&rng.id("item")), quoted(&rng.word(16)), quoted(&rng.word(40)))),
        (false, format!(
            r#"{{"type":"turn.completed","usage":{{"input_tokens":{},"output_tokens":{}}}}}"#,
            rng.next() % 1_000_000, rng.next() % 100_000)),
        (false, format!(
            r#"{{"type":"item.completed","item":{{"id":{},"type":"agent_message","text":{}}}}}"#,
            quoted(&rng.id("item")), quoted(&rng.word(60)))),
    ]
}

/// A wider corpus for the equivalence check: every event name the ingest
/// maps, malformed and partial payloads, unicode and long text.
fn corpus(rng: &mut Lcg) -> Vec<(bool, String)> {
    let mut cases = Vec::new();
    for _ in 0..8 {
        cases.extend(timed(rng));
    }
    let long = "é∑漢🙂".repeat(700);
    let claude_names = [
        "SessionStart", "SessionEnd", "UserPromptSubmit", "Notification", "PreToolUse",
        "PostToolUse", "PostToolUseFailure", "Stop", "StopFailure", "SubagentStop", "Bogus",
    ];
    for name in claude_names {
        let session = rng.id("s");
        let tool = rng.id("toolu");
        cases.push((true, format!(
            r#"{{"hook_event_name":"{name}","session_id":{},"transcript_path":{},"tool_name":"Edit","tool_use_id":{},"duration_ms":{},"error":{},"notification_type":"idle_prompt","reason":"exit"}}"#,
            quoted(&session), quoted(&rng.word(30)), quoted(&tool), rng.next() % 5000, quoted(&rng.word(50)))));
        cases.push((true, format!(r#"{{"hook_event_name":"{name}"}}"#)));
        cases.push((true, format!(
            r#"{{"hook_event_name":"{name}","session_id":{},"tool_use_id":{},"error":{{"message":{},"code":7}}}}"#,
            quoted(&session), quoted(&tool), quoted(&long))));
    }
    let codex_types = [
        "thread.started", "turn.started", "turn.completed", "turn.failed", "error",
        "item.started", "item.completed", "item.updated", "bogus",
    ];
    let item_types = ["command_execution", "file_change", "mcp_tool_call", "web_search", "agent_message", "reasoning"];
    for kind in codex_types {
        for item in item_types {
            for status in ["completed", "failed", "in_progress"] {
                cases.push((false, format!(
                    r#"{{"type":"{kind}","thread_id":{},"item":{{"id":{},"type":"{item}","status":"{status}","command":{}}},"usage":{{"input_tokens":{},"output_tokens":{}}},"error":{{"message":{}}},"message":{}}}"#,
                    quoted(&rng.id("th")), quoted(&rng.id("item")), quoted(&rng.word(20)),
                    rng.next() % 9999, rng.next() % 999, quoted(&rng.word(30)), quoted(&long))));
            }
        }
        cases.push((false, format!(r#"{{"type":"{kind}"}}"#)));
    }
    for broken in ["", "{", "[]", "null", r#"{"hook_event_name":7}"#, r#"{"type":["x"]}"#] {
        cases.push((true, broken.to_string()));
        cases.push((false, broken.to_string()));
    }
    cases
}

fn canonical(result: Result<Option<Normalized>, impl std::fmt::Debug>) -> String {
    match result {
        Err(_) => "error".to_string(),
        Ok(None) => "none".to_string(),
        Ok(Some(event)) => format!(
            "{} {}",
            serde_json::to_string(&event.kind).expect("event serializes"),
            event.source_event_id.as_deref().unwrap_or("-")
        ),
    }
}

fn ingest(claude: bool, payload: &str) -> String {
    if claude {
        canonical(claude_hook(payload))
    } else {
        canonical(codex_line(payload))
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "time".to_string());
    let seed: u64 = std::env::var("RSI_SEED")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(1);
    let mut rng = Lcg(seed);
    match mode.as_str() {
        "check" => {
            for (claude, payload) in corpus(&mut rng) {
                println!("{}", ingest(claude, &payload));
            }
        }
        _ => {
            let mut total = 0u128;
            for (claude, payload) in timed(&mut rng) {
                let start = Instant::now();
                if claude {
                    for _ in 0..ITERS {
                        black_box(claude_hook(black_box(&payload)).is_ok());
                    }
                } else {
                    for _ in 0..ITERS {
                        black_box(codex_line(black_box(&payload)).is_ok());
                    }
                }
                total += start.elapsed().as_nanos() / u128::from(ITERS);
            }
            println!("total_ns {total}");
        }
    }
}

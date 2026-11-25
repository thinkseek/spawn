//! The digest composed from the boards' parts, without a database: what it
//! keeps of each, the latest gate failure of a mission, and its Markdown
//! block's bound.

use super::{DIGEST_TEXT_MAX, Parts, compose};
use serde_json::{Value, json};

fn parts() -> Parts {
    Parts {
        sessions: vec![
            json!({"session_id": "session_supervisor", "observation": {"state": "WORKING"}}),
            json!({"session_id": "session_worker_a", "observation": {"state": "WAITING"}}),
            json!({"session_id": "session_worker_b", "observation": {"state": "WORKING"}}),
            json!({"session_id": "session_worker_c", "observation": null}),
        ],
        live: 3,
        nodes: json!([
            {"name": "mac24", "state": "live", "heartbeat": {"load1": 1.5, "mem_available": 8},
             "power": {"on_ac": false, "battery_pct": 12, "charging": false, "remaining_mins": 20},
             "low_battery_drain": true, "effective_drained": true, "schedulable": false,
             "ssh": "ckl@mac24"},
            {"name": "runner", "state": "drained", "heartbeat": null, "power": null,
             "low_battery_drain": false, "effective_drained": true, "schedulable": false},
        ]),
        board: json!({"missions": [
            {"id": 7, "text": "Issue 91 P1a\nlong body", "state": "active",
             "halt": {"event": 1, "reason": "3 rejections", "at_ms": 1},
             "tasks": [{"id": 70, "state": "landed"}, {"id": 71, "state": "gating"},
                       {"id": 72, "state": "assigned"}]},
            {"id": 6, "text": "Old", "state": "done", "halt": null, "tasks": []},
        ]}),
        gates: json!({
            "70": {"verdict": "accepted", "at_ms": 30},
            "71": {"verdict": "rejected", "reason": "check-tests failed: 2 tests", "at_ms": 20},
            "72": {"verdict": "aborted", "reason": "timeout", "at_ms": 10},
        }),
        approvals: vec![json!({"id": "session_worker_a:1", "kind": "permission",
                               "session_id": "session_worker_a", "label": "Do you want to proceed?",
                               "excerpt": "long"})],
        for_owner: json!([
            {"text": "deploy ffc9fe7", "at_ms": 5, "settled": false},
            {"text": "review T12", "at_ms": 4, "settled": true},
        ]),
        warnings: vec!["digest: something".to_string()],
    }
}

#[test]
fn the_digest_keeps_a_few_facts_of_every_board() {
    let digest = compose(&parts(), 99);
    assert_eq!(digest["generated_at_ms"], 99);
    assert_eq!(
        digest["fleet"],
        json!({"sessions": 4, "live": 3, "states": {"UNJUDGED": 1, "WAITING": 1, "WORKING": 2}})
    );
    assert_eq!(digest["nodes"][0]["low_battery_drain"], true);
    assert_eq!(digest["nodes"][0]["power"]["battery_pct"], 12);
    assert!(digest["nodes"][0].get("ssh").is_none(), "no destination");
    assert_eq!(digest["nodes"][1]["effective_drained"], true);
    let missions = digest["missions"].as_array().expect("missions");
    assert_eq!(missions.len(), 1, "a closed mission is left out");
    assert_eq!(missions[0]["title"], "Issue 91 P1a");
    assert_eq!(missions[0]["open_tasks"], 2);
    assert_eq!(missions[0]["halted"], "3 rejections");
    assert_eq!(
        missions[0]["last_gate_failure"],
        json!({"task": 71, "verdict": "rejected", "reason": "check-tests failed: 2 tests",
               "at_ms": 20}),
        "the latest gate that did not accept"
    );
    assert_eq!(
        digest["approvals"],
        json!([{"id": "session_worker_a:1", "kind": "permission",
                "session_id": "session_worker_a", "label": "Do you want to proceed?"}])
    );
    assert_eq!(
        digest["for_owner"],
        json!([{"text": "deploy ffc9fe7", "at_ms": 5}])
    );
    assert_eq!(digest["warnings"], json!(["digest: something"]));
    let text = digest["digest_text"].as_str().expect("text");
    for line in [
        "## Spawn digest",
        "- Fleet: 4 sessions, 3 live (UNJUDGED 1, WAITING 1, WORKING 2)",
        "- mac24 live, load 1.50, battery 12%, LOW-BATTERY soft-drain",
        "- runner drained, drained",
        "- #7 Issue 91 P1a (active, 2 open tasks) HALTED: 3 rejections",
        "  - last gate failure: task 71 rejected: check-tests failed: 2 tests",
        "### Pending approvals (1)",
        "- [permission] session_worker_a: Do you want to proceed?",
        "### For the owner",
        "- deploy ffc9fe7",
    ] {
        assert!(text.contains(line), "{line:?} in\n{text}");
    }
}

#[test]
fn the_markdown_block_stays_under_its_bound_and_masks_secrets() {
    let mut big = parts();
    big.approvals = (0..10)
        .map(|n| {
            json!({"id": n, "kind": "choice", "session_id": format!("session_{n}"),
                        "label": "x".repeat(400)})
        })
        .collect();
    big.for_owner = Value::Array(
        (0..6)
            .map(|_| json!({"text": format!("rotate sk-ant-api03-{} {}", "k".repeat(40), "y".repeat(400))}))
            .collect(),
    );
    big.nodes = Value::Array(
        (0..60)
            .map(|n| json!({"name": format!("node-{n}-{}", "z".repeat(40)), "state": "live"}))
            .collect(),
    );
    let digest = compose(&big, 1);
    let text = digest["digest_text"].as_str().expect("text");
    assert!(
        text.len() < 4096 && text.len() <= DIGEST_TEXT_MAX,
        "{}",
        text.len()
    );
    assert!(
        text.ends_with("- (cut short: see /api/context/digest)\n"),
        "{text}"
    );
    assert!(
        !digest.to_string().contains("sk-ant-api03-kkkk"),
        "a key is masked"
    );
    assert_eq!(digest["approvals"].as_array().map(Vec::len), Some(10));
}

#[test]
fn empty_parts_still_make_a_digest() {
    let digest = compose(&Parts::default(), 0);
    assert_eq!(digest["fleet"]["sessions"], 0);
    assert_eq!(digest["missions"], json!([]));
    assert_eq!(digest["nodes"], json!([]));
    let text = digest["digest_text"].as_str().expect("text");
    assert!(text.contains("### Pending approvals (0)"), "{text}");
}

//! The cost page's words (objective 060): web/src/cost.ts's `costView` and
//! `perLanding` under node alone (type stripping, no bundler), as the page
//! reads `/api/cost`. The test file is written here, not under web/test:
//! the gate keeps web/ outside web/src protected. It needs `node` only.

use std::path::Path;
use std::process::Command;

const TEST: &str = r##"
import assert from "node:assert/strict";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

const { costView, perLanding } = await import(pathToFileURL(process.env.COST_TS).href);

const tokens = { input: 0, output: 0, cache_write: 0, cache_read: 0 };
const answer = {
  window: "24h",
  since_ms: 0,
  until_ms: 1,
  total: { usd: 0.66, tokens, unpriced_tokens: 0 },
  landed: 2,
  usd_per_landing: 0.33,
  models: [],
  missions: [
    { mission: 1, usd: 0.51, tokens, unpriced_tokens: 0, landed: 1, usd_per_landing: 0.51 },
    { mission: 2, usd: 0.15, tokens, unpriced_tokens: 0, landed: 1, usd_per_landing: 0.15 },
    { mission: 3, usd: 0.02, tokens, unpriced_tokens: 0, landed: 0, usd_per_landing: null },
  ],
  tasks: [{ task: 7, mission: 1, usd: 0.45, tokens, unpriced_tokens: 0 }],
  agents: [],
  executions: [],
  counted_since_ms: null,
};

test("the window's line: dollars per landing and how many landed", () => {
  assert.equal(costView(answer).per_landing, "$0.33 per landing (2 landed)");
});

test("no landing in the window says so, with no figure", () => {
  assert.equal(costView({ ...answer, landed: 0, usd_per_landing: null }).per_landing, "no landing in the window");
  assert.equal(costView(null).per_landing, "no landing in the window");
  const { landed: _l, usd_per_landing: _u, ...older } = answer;
  assert.equal(costView(older).per_landing, "no landing in the window");
});

test("each mission's row carries its own dollars per landing", () => {
  const view = costView(answer);
  assert.deepEqual(
    view.missions.map((row) => [row.key, row.usd, row.per_landing]),
    [
      ["#1", "$0.51", "$0.51 per landing (1 landed)"],
      ["#2", "$0.15", "$0.15 per landing (1 landed)"],
      ["#3", "$0.02", "no landing in the window"],
    ],
  );
  assert.equal(view.tasks[0].per_landing, undefined, "a task's line is no landing's");
});

test("the dollars read as formatUsd says them: grouped, never $0.00 for a cost", () => {
  assert.equal(perLanding({ landed: 1209, usd_per_landing: 1234.5678 }), "$1,234.57 per landing (1,209 landed)");
  assert.equal(perLanding({ landed: 3, usd_per_landing: 0.001 }), "<$0.01 per landing (3 landed)");
  assert.equal(perLanding({ landed: 2, usd_per_landing: 0 }), "$0.00 per landing (2 landed)");
});
"##;

#[test]
fn the_cost_page_says_what_a_landing_cost() {
    let cost_ts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/cost.ts");
    let cost_ts = cost_ts.canonicalize().expect("web/src/cost.ts");
    let dir = std::env::temp_dir().join(format!("spawn-cost-page-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory of its own");
    let test = dir.join("cost.test.mjs");
    std::fs::write(&test, TEST).expect("write the test");
    let run = Command::new("node")
        .arg("--test")
        .arg(&test)
        .env("COST_TS", &cost_ts)
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    let output = match run {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("node is not installed; the cost page's test is skipped");
            return;
        }
        Err(error) => panic!("run node: {error}"),
    };
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

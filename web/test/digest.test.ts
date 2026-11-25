import assert from "node:assert/strict";
import { test } from "node:test";

import { digestBlocks, digestSummary, normalizeDigest } from "../src/digest.ts";

/** `/api/context/digest` as the server answers it (serve/digest.rs `compose`). */
const ANSWER = {
  generated_at_ms: 5_000,
  fleet: { sessions: 12, live: 4, states: { WORKING: 3, WAITING: 5, BLOCKED: 1, odd: "x" } },
  nodes: [
    {
      name: "laptop",
      state: "live",
      load1: 0.5,
      mem_available: 1,
      power: { on_ac: false, battery_pct: 12, charging: false, remaining_mins: 30 },
      low_battery_drain: true,
      effective_drained: true,
      schedulable: false,
    },
    { name: "desk", state: "live", load1: null, power: null, low_battery_drain: false, schedulable: true },
    { state: "live" },
  ],
  missions: [
    {
      id: 7,
      title: "Ship P1b",
      state: "open",
      halted: "gate failed",
      open_tasks: 2,
      last_gate_failure: { task: 571, verdict: "rejected", reason: "clippy", at_ms: 4_000 },
    },
    { id: 8, title: "Other", state: "open", halted: null, open_tasks: 0, last_gate_failure: null },
  ],
  approvals: [{ id: "s:1", kind: "permission", session_id: "s", label: "Allow Bash?" }],
  for_owner: [{ text: "Please look at mission 7", at_ms: 3_000 }, { at_ms: 1 }],
  digest_text: "## Fleet\n\n- 12 sessions, **4 live**\n- `laptop` on battery\n### Missions\nfree line\n",
  warnings: [],
};

test("the digest reads each part, states by count", () => {
  const digest = normalizeDigest(ANSWER);
  assert.deepEqual(digest.fleet, {
    sessions: 12,
    live: 4,
    states: [
      ["WAITING", 5],
      ["WORKING", 3],
      ["BLOCKED", 1],
    ],
  });
  assert.deepEqual(digest.nodes[0], {
    name: "laptop",
    state: "live",
    load1: 0.5,
    battery_pct: 12,
    on_ac: false,
    low_battery_drain: true,
    schedulable: false,
  });
  assert.equal(digest.nodes[1].on_ac, null);
  assert.equal(digest.nodes.length, 2);
  assert.deepEqual(digest.missions[0].last_gate_failure, { task: 571, verdict: "rejected", reason: "clippy" });
  assert.equal(digest.for_owner.length, 1);
  assert.equal(digestSummary(digest), "12 sessions, 4 live · 1 to answer · 1 halted");
});

test("an answer without digest_text is a failed read; a missing part is empty", () => {
  assert.throws(() => normalizeDigest({ fleet: {} }), /not a digest/);
  assert.throws(() => normalizeDigest(null), /not a digest/);
  const bare = normalizeDigest({ digest_text: "" });
  assert.deepEqual(bare.fleet, { sessions: 0, live: 0, states: [] });
  assert.deepEqual([bare.nodes, bare.missions, bare.approvals, bare.for_owner], [[], [], [], []]);
  assert.equal(digestSummary(bare), "0 sessions, 0 live");
});

test("the Markdown becomes headings, bullet runs and lines, as text only", () => {
  assert.deepEqual(digestBlocks(ANSWER.digest_text), [
    { kind: "heading", level: 2, text: "Fleet" },
    { kind: "bullets", items: ["12 sessions, 4 live", "laptop on battery"] },
    { kind: "heading", level: 3, text: "Missions" },
    { kind: "text", text: "free line" },
  ]);
  // HTML stays text: nothing here is ever markup.
  assert.deepEqual(digestBlocks("<img src=x onerror=alert(1)>"), [
    { kind: "text", text: "<img src=x onerror=alert(1)>" },
  ]);
  assert.deepEqual(digestBlocks(""), []);
});

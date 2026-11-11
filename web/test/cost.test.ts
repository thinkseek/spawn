import assert from "node:assert/strict";
import { test } from "node:test";

import { costView, perLanding } from "../src/cost.ts";

// Objective 060: the cost page says what a landing cost, per window and per
// mission.
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
  assert.equal(costView({ ...answer, landed: undefined, usd_per_landing: undefined }).per_landing, "no landing in the window");
});

test("each mission's row carries its own dollars per landing", () => {
  const missions = costView(answer).missions;
  assert.deepEqual(
    missions.map((row) => [row.key, row.usd, row.per_landing]),
    [
      ["#1", "$0.51", "$0.51 per landing (1 landed)"],
      ["#2", "$0.15", "$0.15 per landing (1 landed)"],
      ["#3", "$0.02", "no landing in the window"],
    ],
  );
  // A task's line is no landing's.
  assert.equal(costView(answer).tasks[0].per_landing, undefined);
});

test("the dollars read as formatUsd says them: grouped, never $0.00 for a cost", () => {
  assert.equal(perLanding({ landed: 1209, usd_per_landing: 1234.5678 }), "$1,234.57 per landing (1,209 landed)");
  assert.equal(perLanding({ landed: 3, usd_per_landing: 0.001 }), "<$0.01 per landing (3 landed)");
  assert.equal(perLanding({ landed: 2, usd_per_landing: 0 }), "$0.00 per landing (2 landed)");
});

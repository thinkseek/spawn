import assert from "node:assert/strict";
import { test } from "node:test";

import { formatCost, formatDecimal, modelCostText, tileNote, undecidedText } from "../src/scorecardFormat.ts";

// GitHub #16 item 3: tokens per landing were shown with no decimals, so a
// positive cost below half a token read "0".
test("a positive cost never reads as zero", () => {
  assert.equal(formatCost(0.333), "0.333");
  assert.equal(formatCost(0.5), "0.5");
  assert.equal(formatCost(0.999), "0.999");
  // Below the three decimals the server rounds to, still not "0".
  assert.equal(formatCost(0.0004), "<0.001");
  assert.equal(formatCost(0.9996), "1");
});

test("zero is zero and a token or more is whole tokens", () => {
  assert.equal(formatCost(0), "0");
  assert.equal(formatCost(1), "1");
  assert.equal(formatCost(1.4), "1");
  assert.equal(formatCost(2033.333), "2,033");
  assert.equal(formatCost(null), "—");
  assert.equal(formatCost(Number.NaN), "—");
});

test("a model's cost line keeps a small cost per landing", () => {
  assert.equal(modelCostText(1, 0.333), "1 tokens · 0.333 per landing");
  assert.equal(modelCostText(6100, 2033.333), "6,100 tokens · 2,033 per landing");
  assert.equal(modelCostText(0, 0), "0 tokens · 0 per landing");
  assert.equal(modelCostText(5, null), "5 tokens · — per landing");
});

test("a decimal is grouped with at most the digits asked", () => {
  assert.equal(formatDecimal(2033.3333), "2,033.333");
  assert.equal(formatDecimal(2033.333, 0), "2,033");
  assert.equal(formatDecimal(null), "—");
});

// Objective 067: how much gate time decided nothing.
const MIN = 60_000;
const HOUR = 60 * MIN;

test("undecidedText says how much of the gate time decided nothing, and of how much", () => {
  // rsi/scenarios/067.sh: 65 min over six runs, 35 of them undecided.
  assert.equal(
    undecidedText({ total_ms: 65 * MIN, undecided_ms: 35 * MIN }),
    "35m of 1.1 h gate time decided nothing (54%)",
  );
  assert.equal(
    undecidedText({ total_ms: 10 * HOUR, undecided_ms: 1.5 * HOUR }),
    "1.5 h of 10.0 h gate time decided nothing (15%)",
  );
  // Minutes under an hour, whole; hours from one.
  assert.equal(undecidedText({ total_ms: 40 * MIN, undecided_ms: 0 }), "0m of 40m gate time decided nothing (0%)");
  assert.equal(undecidedText({ total_ms: HOUR, undecided_ms: HOUR }), "1.0 h of 1.0 h gate time decided nothing (100%)");
  assert.equal(
    undecidedText({ total_ms: 3 * HOUR, undecided_ms: MIN / 3 }),
    "0m of 3.0 h gate time decided nothing (0%)",
  );
  // No gate time: nothing to say.
  assert.equal(undecidedText({ total_ms: 0, undecided_ms: 0 }), null);
  assert.equal(undecidedText({}), null);
  assert.equal(undecidedText(null), null);
  assert.equal(undecidedText(undefined), null);
  // An older server's answer has no parts.
  assert.equal(undecidedText({ total_ms: "x", undecided_ms: 5 }), null);
});

test("the Scorecard page's gate time tile, the fleet's and each mission's, shows the undecided line", () => {
  // The fleet's and mission 1's gate_time as /api/scorecard gives them.
  const fleet = { value: 600_000, n: 7, runs: 7, total_ms: 70 * MIN, undecided_ms: 35 * MIN, undecided_runs: 3 };
  const mission = { value: 600_000, n: 6, runs: 6, total_ms: 65 * MIN, undecided_ms: 35 * MIN, undecided_runs: 3 };
  assert.equal(tileNote("gate_time", fleet), "35m of 1.2 h gate time decided nothing (50%)");
  assert.equal(tileNote("gate_time", mission), "35m of 1.1 h gate time decided nothing (54%)");
  // A mission with no gate run shows no line; no other tile has one.
  assert.equal(tileNote("gate_time", { ...mission, runs: 0, total_ms: 0, undecided_ms: 0 }), null);
  assert.equal(tileNote("cost", { ...fleet, gate_ms: 70 * MIN } as never), null);
  assert.equal(tileNote("throughput", fleet), null);
});

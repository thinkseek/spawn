import assert from "node:assert/strict";
import { test } from "node:test";

import { formatCost, formatDecimal, modelCostText } from "../src/scorecardFormat.ts";

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

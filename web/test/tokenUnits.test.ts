// t505u P4: every token count on the page in decimal units (K, M, B; counts,
// not bytes), its exact value kept for a title.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { compactCount as fromCost, costView, formatTokens, unpricedNote, unpricedNoteTitle } from "../src/cost.ts";
import { compactCount } from "../src/format.ts";
import { modelCostTitle } from "../src/scorecardFormat.ts";

test("compactCount lives in format.ts, and cost.ts re-exports the same function", () => {
  assert.equal(fromCost, compactCount);
  const cases: [number, string][] = [
    [0, "0"],
    [999, "999"],
    [1_000, "1.0K"],
    [12_345, "12K"],
    [999_499, "999K"],
    [999_600, "1.0M"],
    [4_512_331, "4.5M"],
    [1_463_163_060, "1.5B"],
  ];
  for (const [n, want] of cases) assert.equal(compactCount(n), want, String(n));
});

const answer = {
  total: {
    tokens: { input: 100_000, output: 85_000, cache_write: 0, cache_read: 200_000 },
    unpriced_tokens: 275_000,
  },
  models: [
    { model: "bb-model-z", tokens: { input: 75_000, output: 0, cache_write: 0, cache_read: 0 }, usd: null, unpriced_tokens: 75_000 },
    { model: "bb-model-y", tokens: { input: 200_000, output: 0, cache_write: 0, cache_read: 0 }, usd: null, unpriced_tokens: 200_000 },
  ],
} as unknown as Parameters<typeof unpricedNote>[0];

test("the unpriced note is compact by default and in full with formatTokens", () => {
  const compact = unpricedNote(answer) ?? "";
  const exact = unpricedNote(answer, formatTokens) ?? "";
  assert.match(compact, /^275K of 385K tokens \(71%\) ran on models with no price \(bb-model-y 200K, bb-model-z 75K\)/);
  assert.match(exact, /^275,000 of 385,000 tokens \(71%\) ran on models with no price \(bb-model-y 200,000, bb-model-z 75,000\)/);
  assert.equal(unpricedNote({ total: { tokens: {}, unpriced_tokens: 0 } } as unknown as Parameters<typeof unpricedNote>[0]), null);
});

// rev-k F1: the exact note is reached through costView and the page's title
// function, not only by calling unpricedNote directly.
test("costView carries the note compact and its exact twin; the note's title starts with the exact one", () => {
  const view = costView(answer as Parameters<typeof costView>[0]);
  assert.match(view.unpriced_note ?? "", /^275K of 385K tokens \(71%\)/);
  assert.match(view.unpriced_note_exact ?? "", /^275,000 of 385,000 tokens \(71%\)/);
  const title = unpricedNoteTitle(view);
  assert.equal(title.split("\n\n")[0], view.unpriced_note_exact);
  assert.match(title, /\n\nSPAWN_MODEL_PRICES is a JSON object/);
  assert.match(unpricedNoteTitle({ unpriced_note_exact: null }), /^SPAWN_MODEL_PRICES is a JSON object/);
});

// rev-k n1: the per-model title in full, per landing included.
test("a model's cost line is titled with every count in full", () => {
  assert.equal(modelCostTitle("m", 1_463_163_060, 209_023_294), "m: 1,463,163,060 tokens, 209,023,294 per landing");
  assert.equal(modelCostTitle("m", 6_100, 2_033.333), "m: 6,100 tokens, 2,033.333 per landing");
  assert.equal(modelCostTitle("m", 6_100, null), "m: 6,100 tokens");
});

test("the pages use those titles: the cost note's and the scorecard's", () => {
  const page = readFileSync(new URL("../src/components/cost/CostView.tsx", import.meta.url), "utf8");
  assert.match(page, /title=\{unpricedNoteTitle\(figures\)\}/);
  const scorecard = readFileSync(new URL("../src/scorecard.ts", import.meta.url), "utf8");
  assert.match(scorecard, /\{ label: "tokens", value: compactCount\(d\.tokens\), title: `\$\{formatDecimal\(d\.tokens, 0\)\} tokens` \}/);
  assert.match(scorecard, /title: modelCostTitle\(model, entry\.tokens, entry\.per_landed\)/);
});

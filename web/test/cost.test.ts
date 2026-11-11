import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { costView, formatTokens, unpricedNote, unpricedText } from "../src/cost.ts";

const tokens = (input: number, output: number) => ({ input, output, cache_write: 0, cache_read: 0 });

/** 059's window: bb-model-x priced ($0.45), bb-model-z not (275,000). */
function answer() {
  return {
    window: "24h",
    since_ms: 0,
    until_ms: 86_400_000,
    counted_since_ms: 0,
    total: { usd: 0.45, tokens: tokens(350_000, 35_000), unpriced_tokens: 275_000 },
    models: [
      { model: "bb-model-x", usd: 0.45, tokens: tokens(100_000, 10_000), unpriced_tokens: 0 },
      { model: "bb-model-z", usd: null, tokens: tokens(250_000, 25_000), unpriced_tokens: 275_000 },
    ],
    missions: [
      { mission: 1, usd: 0.45, tokens: tokens(300_000, 30_000), unpriced_tokens: 220_000 },
      { mission: 2, usd: 0, tokens: tokens(50_000, 5000), unpriced_tokens: 55_000 },
    ],
    tasks: [
      {
        task: 7,
        mission: 1,
        usd: 0.45,
        tokens: tokens(300_000, 30_000),
        unpriced_tokens: 220_000,
        label: "Spawn · owner work · Write the docs",
      },
      { task: 8, mission: 2, usd: 0, tokens: tokens(50_000, 5000), unpriced_tokens: 55_000, label: null },
    ],
    agents: [],
    executions: [],
  };
}

test("token counts are grouped the en-US way", () => {
  assert.equal(formatTokens(275_000), "275,000");
  assert.equal(formatTokens(2_128_602_277), "2,128,602,277");
  assert.equal(unpricedText(220_000), "+ 220,000 tokens unpriced");
  assert.equal(unpricedText(1), "+ 1 token unpriced");
  assert.equal(unpricedText(0), null);
});

test("the window's note says once how many tokens have no price, on which model, how to price them", () => {
  const note = costView(answer()).unpriced_note;
  assert.equal(typeof note, "string");
  assert.match(note ?? "", /^275,000 of 385,000 tokens \(71%\)/);
  assert.match(note ?? "", /bb-model-z/);
  assert.doesNotMatch(note ?? "", /bb-model-x/, "a priced model is not named");
  assert.match(note ?? "", /SPAWN_MODEL_PRICES/);
});

test("several unpriced models are each named with their count, most first", () => {
  const live = {
    total: {
      usd: 1778.71,
      tokens: { input: 5_576_854_833, output: 0, cache_write: 0, cache_read: 0 },
      unpriced_tokens: 2_128_602_277,
    },
    models: [
      { model: "claude-opus-5-5", usd: 1778.71, tokens: tokens(3_448_252_556, 0), unpriced_tokens: 0 },
      { model: "deepseek-v4-pro", usd: null, tokens: tokens(316_493_617, 0), unpriced_tokens: 316_493_617 },
      { model: "deepseek-flash", usd: null, tokens: tokens(1_812_108_660, 0), unpriced_tokens: 1_812_108_660 },
    ],
  };
  assert.equal(
    unpricedNote(live),
    "2,128,602,277 of 5,576,854,833 tokens (38%) ran on models with no price " +
      "(deepseek-flash 1,812,108,660, deepseek-v4-pro 316,493,617): their cost is not in these figures. " +
      "Price them in SPAWN_MODEL_PRICES (US dollars per million tokens: input, output, cache_write, cache_read).",
  );
});

test("no note when every token is priced", () => {
  const priced = answer();
  priced.total.unpriced_tokens = 0;
  priced.models = priced.models.filter((model) => model.usd !== null);
  assert.equal(costView(priced).unpriced_note, null);
  assert.equal(costView(null).unpriced_note, null);
});

test("an answer whose models rows lack their count still names the model with no price", () => {
  const old = answer() as unknown as { models: Array<Record<string, unknown>> };
  for (const model of old.models) delete model.unpriced_tokens;
  const note = unpricedNote(old as never) ?? "";
  assert.match(note, /bb-model-z/);
  assert.doesNotMatch(note, /bb-model-x/);
});

test("each row carries its unpriced count and what it says of it", () => {
  const view = costView(answer());
  assert.deepEqual(
    view.missions.map((row) => [row.key, row.unpriced_tokens, row.unpriced_text]),
    [
      ["#1", 220_000, "+ 220,000 tokens unpriced"],
      ["#2", 55_000, "+ 55,000 tokens unpriced"],
    ],
  );
  const priced = answer();
  priced.missions[0].unpriced_tokens = 0;
  const row = costView(priced).missions.find((entry) => entry.key === "#1");
  assert.equal(row?.unpriced_tokens, 0);
  assert.equal(row?.unpriced_text, null);
});

test("a task row carries the API's label beside its id; null when the API has none", () => {
  const view = costView(answer());
  const seven = view.tasks.find((row) => row.key === "T7");
  assert.equal(seven?.label, "Spawn · owner work · Write the docs");
  assert.equal(view.tasks.find((row) => row.key === "T8")?.label, null);
  assert.equal(view.missions[0].label, null, "a mission row has no task label");
});

test("the page shows the note under the total, each row's count and a task's label", () => {
  const page = readFileSync(new URL("../src/components/cost/CostView.tsx", import.meta.url), "utf8");
  assert.match(page, /figures\?\.unpriced_note/, "the window's note");
  assert.doesNotMatch(page, /UnpricedMark on=\{figures\.unpriced\}/, "no bare badge on the total");
  assert.match(page, /row\.unpriced_text/, "a row's count");
  assert.match(page, /row\.label/, "a task's label");
  assert.doesNotMatch(page, />\s*unpriced use\s*</, "no bare 'unpriced use' badge");
});

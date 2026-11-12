import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import type { CostAnswer, CostEntry } from "../src/cost.ts";
import { compactCount, costView, unpricedShort } from "../src/cost.ts";

function entry(usd: number, unpriced: number): CostEntry {
  return { usd, tokens: { input: 100_000 + unpriced, output: 0, cache_write: 0, cache_read: 0 }, unpriced_tokens: unpriced };
}

/** Two missions as objective 075's scenario seeds them, and a third the
 * record does not hold (its name null). */
function answer(): Partial<CostAnswer> {
  return {
    window: "24h",
    total: entry(0.3, 486_469_876),
    missions: [
      { mission: 1, name: "Bb Alpha", ...entry(0.3, 485_321_042) },
      { mission: 2, name: "Bb Beta · owner work", ...entry(0, 1_148_834) },
      { mission: 3, name: null, ...entry(0, 0) },
    ],
    tasks: [
      { task: 1, mission: 1, label: "T1 label", ...entry(0.3, 485_321_042) },
      { task: 2, mission: 2, label: null, ...entry(0, 1_148_834) },
    ],
    agents: [
      { agent: "agent_worker_objective_1", ...entry(0.3, 485_321_042) },
      { agent: "agent_worker_objective_2", ...entry(0, 1_148_834) },
    ],
  };
}

test("(a) a mission row carries the mission's name as its label; null when the record does not hold it", () => {
  const view = costView(answer());
  assert.deepEqual(
    view.missions.map((row) => [row.key, row.label]),
    [
      ["#1", "Bb Alpha"],
      ["#2", "Bb Beta · owner work"],
      ["#3", null],
    ],
  );
  // An answer from before the rows carried a name: no label, no error.
  const old = answer();
  old.missions = old.missions?.map(({ name: _name, ...rest }) => rest);
  assert.deepEqual(
    costView(old).missions.map((row) => row.label),
    [null, null, null],
  );
  // Task labels are as before.
  assert.equal(view.tasks.find((row) => row.key === "T1")?.label, "T1 label");
});

test("(b) every row carries a short unpriced badge; the full count stays", () => {
  const view = costView(answer());
  const short = (rows: { key: string; unpriced_short: string | null }[]) =>
    Object.fromEntries(rows.map((row) => [row.key, row.unpriced_short]));
  assert.deepEqual(short(view.missions), { "#1": "+485M unpriced", "#2": "+1.1M unpriced", "#3": null });
  assert.deepEqual(short(view.tasks), { T1: "+485M unpriced", T2: "+1.1M unpriced" });
  assert.deepEqual(short(view.agents), {
    agent_worker_objective_1: "+485M unpriced",
    agent_worker_objective_2: "+1.1M unpriced",
  });
  assert.equal(view.tasks[0].unpriced_text, "+ 485,321,042 tokens unpriced");
});

test("(b) the compact count: one decimal under 10, whole from 10, rounded before the step is picked", () => {
  const cases: [number, string][] = [
    [1, "1"],
    [950, "950"],
    [999, "999"],
    [999.6, "1.0K"],
    [1_000, "1.0K"],
    [1_049, "1.0K"],
    [9_949, "9.9K"],
    [9_960, "10K"],
    [55_000, "55K"],
    [999_499, "999K"],
    [999_600, "1.0M"],
    [1_148_834, "1.1M"],
    [17_000_456, "17M"],
    [485_321_042, "485M"],
    [999_700_000, "1.0B"],
    [1_463_163_060, "1.5B"],
    [2_500_000_000_000, "2500B"],
  ];
  for (const [n, want] of cases) assert.equal(compactCount(n), want, String(n));
  assert.equal(unpricedShort(0), null);
  assert.equal(unpricedShort(-5), null);
  assert.equal(unpricedShort(Number.NaN), null);
  assert.equal(unpricedShort(9_960), "+10K unpriced");
});

test("(c) the page's badge shows the short text, the full count in its title; the dollars never shrink or wrap", () => {
  const page = readFileSync(new URL("../src/components/cost/CostView.tsx", import.meta.url), "utf8");
  assert.match(page, /<UnpricedMark short=\{row\.unpriced_short\} full=\{row\.unpriced_text\} \/>/);
  assert.match(page, /title=\{`\$\{full \?\? short\}/);
  assert.match(page, /"tnum shrink-0 whitespace-nowrap"/);
  assert.match(page, /row\.label \? "shrink-0" : "min-w-0 truncate"/);
  assert.match(page, /<span className="min-w-0 truncate text-foreground" title=\{row\.label\}>/);
});

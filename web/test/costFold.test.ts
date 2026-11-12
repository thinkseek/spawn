import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import type { CostAnswer, CostEntry } from "../src/cost.ts";
import { costView, fold, shareText, TOP_ROWS } from "../src/cost.ts";

const TOKENS = { input: 1000, output: 0, cache_write: 0, cache_read: 0 };

function entry(usd: number): CostEntry {
  return { usd, tokens: TOKENS, unpriced_tokens: 0 };
}

/** n tasks of one mission, task k costing k x $0.30, each with its own agent. */
function answer(n: number): Partial<CostAnswer> {
  const ks = Array.from({ length: n }, (_, i) => i + 1);
  const total = ks.reduce((sum, k) => sum + k * 0.3, 0);
  return {
    window: "24h",
    total: entry(total),
    missions: [{ mission: 1, ...entry(total) }],
    tasks: ks.map((k) => ({ task: k, mission: 1, label: `task ${k}`, ...entry(k * 0.3) })),
    agents: ks.map((k) => ({ agent: `agent_${k}`, ...entry(k * 0.3) })),
  };
}

test("(a) the ten costliest tasks and agents, the rest folded into one line", () => {
  assert.equal(TOP_ROWS, 10);
  const view = costView(answer(13));
  assert.deepEqual(
    view.tasks.map((row) => row.key),
    [13, 12, 11, 10, 9, 8, 7, 6, 5, 4].map((k) => `T${k}`),
  );
  assert.deepEqual(view.tasks_more, { count: 3, usd: 1.8, text: "3 more tasks · $1.80" });
  assert.equal(view.agents.length, 10);
  assert.equal(view.agents[0].key, "agent_13");
  assert.deepEqual(view.agents_more, { count: 3, usd: 1.8, text: "3 more agents · $1.80" });
  assert.equal(view.missions.length, 1, "missions are not folded");
});

test("(a) the fold's edges: ten rows fold nothing, eleven fold one", () => {
  const ten = costView(answer(10));
  assert.equal(ten.tasks.length, 10);
  assert.equal(ten.tasks_more, null);
  assert.equal(ten.agents_more, null);
  const eleven = costView(answer(11));
  assert.equal(eleven.tasks.length, 10);
  assert.deepEqual(eleven.tasks_more, { count: 1, usd: 0.3, text: "1 more task · $0.30" });
  assert.deepEqual(eleven.agents_more, { count: 1, usd: 0.3, text: "1 more agent · $0.30" });
  assert.equal(costView(answer(0)).tasks_more, null);
  assert.equal(costView(null).agents_more, null);
});

test("(a) the folded dollars are summed and rounded to the cent in any order", () => {
  const top = Array.from({ length: 10 }, () => 5);
  for (const rest of [[0.9, 0.6, 0.3], [0.3, 0.6, 0.9], [0.6, 0.3, 0.9]]) {
    assert.deepEqual(fold([...top, ...rest], "task"), { count: 3, usd: 1.8, text: "3 more tasks · $1.80" });
  }
  assert.equal(fold([...top, 0.001], "task")?.text, "1 more task · <$0.01");
  assert.equal(fold([...top, 0.001], "task")?.usd, 0);
  assert.equal(fold(top, "task"), null);
});

test("(b) each row's share of the window, rounded to the nearest whole percent", () => {
  const view = costView(answer(13));
  // T13 $3.90 of $27.30 is 14.3%; T4 $1.20 is 4.4%; the mission is all of it.
  assert.equal(view.tasks[0].share, "14%");
  assert.equal(view.tasks[9].share, "4%");
  assert.equal(view.agents[0].share, "14%");
  assert.equal(view.missions[0].share, "100%");
  assert.equal(shareText(14.5, 100), "15%");
  assert.equal(shareText(14.49, 100), "14%");
  assert.equal(shareText(1, 100), "1%");
  assert.equal(shareText(0.995, 100), "<1%", "under one percent, though it rounds to 1");
});

test("(b) a share above zero and under one percent is '<1%'; nothing priced is null", () => {
  const base = answer(13);
  const tiny = costView({ ...base, total: entry(1000), tasks: [{ task: 1, mission: 1, label: null, ...entry(3.9) }] });
  assert.equal(tiny.tasks[0].share, "<1%");
  assert.equal(shareText(0.001, 1000), "<1%");
  assert.equal(shareText(0, 1000), "0%");
  const nothing = costView({ ...base, total: entry(0) });
  assert.equal(nothing.tasks[0].share, null);
  assert.equal(nothing.missions[0].share, null);
  assert.equal(shareText(1, 0), null);
  assert.equal(shareText(1, Number.NaN), null);
});

test("(c) the Cost page shows each row's share and the fold line under tasks and agents", () => {
  const page = readFileSync(new URL("../src/components/cost/CostView.tsx", import.meta.url), "utf8");
  assert.match(page, /<Rows title="Tasks" rows=\{figures\.tasks\} more=\{figures\.tasks_more\} \/>/);
  assert.match(page, /<Rows title="Agents" rows=\{figures\.agents\} more=\{figures\.agents_more\} \/>/);
  assert.match(page, /<ShareMark share=\{first\.share\} \/>/);
  assert.match(page, /\{more && \([\s\S]*?\{more\.text\}[\s\S]*?<\/li>/);
  assert.ok(page.indexOf("{rows.map(") < page.indexOf("{more.text}"), "the fold line follows the rows");
});

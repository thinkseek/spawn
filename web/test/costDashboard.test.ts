import assert from "node:assert/strict";
import { test } from "node:test";

import type { CostAnswer, CostEntry } from "../src/cost.ts";
import { budgetGauges, cacheShare, costView, dashboard, spendDelta, TOP_BARS, trendBars } from "../src/cost.ts";

const DAY = 86_400_000;
const HOUR = 3_600_000;
const SINCE = Date.UTC(2026, 10, 16, 6, 45);

function entry(usd: number, tokens = { input: 1000, output: 0, cache_write: 0, cache_read: 3000 }, unpriced = 0): CostEntry {
  return { usd, tokens, unpriced_tokens: unpriced };
}

function answer(extra: Partial<CostAnswer> = {}): Partial<CostAnswer> {
  return {
    window: "24h",
    since_ms: SINCE,
    until_ms: SINCE + DAY,
    total: entry(10),
    landed: 4,
    usd_per_landing: 2.5,
    models: [],
    missions: [],
    tasks: [],
    agents: [],
    executions: [],
    counted_since_ms: null,
    ...extra,
  };
}

const previous = (usd: number) => ({ total: entry(usd), landed: 1, usd_per_landing: usd, since_ms: SINCE - DAY, until_ms: SINCE });

test("the spend's change against the previous window of equal length", () => {
  assert.equal(spendDelta(answer({ previous: previous(8) })), "▲ 25% vs previous 24 h");
  assert.equal(spendDelta(answer({ previous: previous(20) })), "▼ 50% vs previous 24 h");
  assert.equal(spendDelta(answer({ previous: previous(10) })), "same as previous 24 h");
  assert.equal(spendDelta(answer({ previous: previous(0) })), "previous 24 h: $0.00");
  const week = answer({ window: "7d", since_ms: SINCE, until_ms: SINCE + 7 * DAY, previous: previous(5) });
  assert.equal(spendDelta(week), "▲ 100% vs previous 7 days");
  // An answer from before #64: no change said, the tile says its window.
  assert.equal(spendDelta(answer()), null);
  assert.equal(dashboard(answer()).spend.detail, "last 24 h");
  assert.equal(dashboard(null).spend.value, "$0.00");
});

test("a previous window of only unpriced use reads unpriced, never $0.00", () => {
  const before = { ...previous(0), total: entry(0, { input: 500, output: 0, cache_write: 0, cache_read: 0 }, 500) };
  assert.equal(spendDelta(answer({ previous: before })), "previous 24 h: unpriced");
});

test("the tokens tile splits priced and unpriced, the unpriced part marked", () => {
  const view = dashboard(answer({ total: entry(10, { input: 1_000_000, output: 200_000, cache_write: 0, cache_read: 3_000_000 }, 1_200_000) }));
  assert.equal(view.tokens.value, "4.2M");
  assert.equal(view.tokens.detail, "3.0M priced");
  assert.equal(view.tokens.unpriced_short, "+1.2M unpriced");
  assert.equal(view.tokens.unpriced_text, "+ 1,200,000 tokens unpriced");
  assert.equal(view.spend.unpriced_short, "+1.2M unpriced", "the spend leaves that use out");
  assert.equal(dashboard(answer()).tokens.unpriced_short, null);
});

test("the cache-read share is cache_read over cache_read plus input", () => {
  assert.equal(cacheShare(entry(1, { input: 1000, output: 9999, cache_write: 9999, cache_read: 3000 })), "75%");
  assert.equal(cacheShare(entry(1, { input: 0, output: 5, cache_write: 5, cache_read: 0 })), "—");
  assert.equal(cacheShare(entry(1, { input: 100_000, output: 0, cache_write: 0, cache_read: 1 })), "<1%");
  assert.equal(cacheShare(entry(1, { input: 4, output: 0, cache_write: 0, cache_read: 996 })), ">99%");
  assert.equal(cacheShare(entry(1, { input: 0, output: 0, cache_write: 0, cache_read: 996 })), "100%");
  assert.equal(dashboard(answer()).cache.detail, "3.0K of 4.0K read from cache");
});

test("the cost per landed task, or a dash with no landing", () => {
  assert.equal(dashboard(answer()).landing.value, "$2.50");
  assert.equal(dashboard(answer()).landing.detail, "4 landed");
  const none = dashboard(answer({ landed: 0, usd_per_landing: null }));
  assert.equal(none.landing.value, "—");
  assert.equal(none.landing.detail, "no landing in the window");
});

test("the trend: each step's height of the costliest, a cost never flat, unpriced steps marked", () => {
  const trend = [
    { since_ms: SINCE, usd: 4, unpriced_tokens: 0 },
    { since_ms: SINCE + HOUR, usd: 0, unpriced_tokens: 0 },
    { since_ms: SINCE + 2 * HOUR, usd: 0.01, unpriced_tokens: 0 },
    { since_ms: SINCE + 3 * HOUR, usd: 0, unpriced_tokens: 220_000 },
    { since_ms: SINCE + 4 * HOUR, usd: 2, unpriced_tokens: 0 },
  ];
  const bars = trendBars(answer({ trend, trend_step_ms: HOUR }));
  assert.deepEqual(
    bars.map((bar) => [bar.label, bar.height, bar.unpriced]),
    [
      ["06:45", 100, false],
      ["07:45", 0, false],
      ["08:45", 2, false],
      ["09:45", 0, true],
      ["10:45", 50, false],
    ],
  );
  assert.equal(bars[3].title, "2026-11-16 09:45 – 2026-11-16 10:45 UTC: $0.00 + 220,000 tokens unpriced");
  const view = dashboard(answer({ trend, trend_step_ms: HOUR }));
  assert.equal(view.trend_note, "hours back from now, UTC");
  assert.equal(view.trend_unpriced, 1);
  // Steps run back from until_ms (rolling, not calendar days); the last
  // one ends at until_ms, however short.
  const week = answer({
    since_ms: SINCE,
    until_ms: SINCE + DAY + HOUR,
    trend: [
      { since_ms: SINCE, usd: 1, unpriced_tokens: 0 },
      { since_ms: SINCE + DAY, usd: 0.5, unpriced_tokens: 0 },
    ],
    trend_step_ms: DAY,
  });
  const days = trendBars(week);
  assert.deepEqual(
    days.map((bar) => [bar.label, bar.title]),
    [
      ["11-16", "2026-11-16 06:45 – 2026-11-17 06:45 UTC: $1.00"],
      ["11-17", "2026-11-17 06:45 – 2026-11-17 07:45 UTC: $0.50"],
    ],
  );
  assert.equal(dashboard(week).trend_note, "days back from now, UTC");
  assert.deepEqual(trendBars(answer()), [], "an answer from before #64 has no trend");
  assert.deepEqual(trendBars(answer({ trend: [{ since_ms: SINCE, usd: 0, unpriced_tokens: 0 }] }))[0].height, 0);
});

test("a daily budget's gauge: over exactly when the alert fires, capped at full", () => {
  const gauges = budgetGauges(
    answer({
      missions: [{ mission: 4, name: "Spawn RSI", ...entry(3) }],
      budgets: [
        { mission: 4, budget_usd: 5, spent_usd: 3.2 },
        { mission: 9, budget_usd: 2, spent_usd: 2 },
        { mission: 6, budget_usd: 1, spent_usd: 2.5 },
        { mission: 7, budget_usd: 0, spent_usd: 1 },
      ],
    }),
  );
  assert.deepEqual(
    gauges.map((g) => [g.key, g.label, g.text, g.percent, g.width, g.over]),
    [
      ["#4", "Spawn RSI", "$3.20 of $5.00 today (UTC)", "64%", 64, false],
      ["#9", null, "$2.00 of $2.00 today (UTC)", "100%", 100, true],
      ["#6", null, "$2.50 of $1.00 today (UTC)", "250%", 100, true],
    ],
  );
  assert.equal(budgetGauges(answer({ budgets: [{ mission: 1, budget_usd: 1000, spent_usd: 1 }] }))[0].percent, "<1%");
  assert.deepEqual(budgetGauges(answer()), []);
});

test("the ranked bars: the top five by share, an unpriced model never a zero-cost bar", () => {
  const tokens = { input: 10, output: 0, cache_write: 0, cache_read: 0 };
  const missions = Array.from({ length: 7 }, (_, i) => ({ mission: i + 1, ...entry(i + 1, tokens) }));
  const view = dashboard(
    answer({
      total: entry(28, tokens, 5000),
      missions,
      models: [
        { model: "priced", usd: 28, tokens, unpriced_tokens: 0 },
        { model: "free-looking", usd: null, tokens: { ...tokens, input: 5000 }, unpriced_tokens: 5000 },
      ],
    }),
  );
  assert.equal(view.missions.length, TOP_BARS);
  assert.deepEqual(
    view.missions.map((bar) => [bar.key, bar.share, Math.round(bar.width)]),
    [
      ["#7", "25%", 25],
      ["#6", "21%", 21],
      ["#5", "18%", 18],
      ["#4", "14%", 14],
      ["#3", "11%", 11],
    ],
  );
  const [priced, unpriced] = view.models;
  assert.deepEqual([priced.key, priced.usd, priced.share, priced.width], ["priced", "$28.00", "100%", 100]);
  assert.deepEqual(
    [unpriced.key, unpriced.usd, unpriced.share, unpriced.width, unpriced.unpriced, unpriced.unpriced_short],
    ["free-looking", "unpriced", null, 0, true, "+5.0K unpriced"],
  );
});

test("the detail view keeps its figures", () => {
  const full = answer({ missions: [{ mission: 4, ...entry(10) }] });
  assert.equal(costView(full).total, "$10.00");
  assert.equal(costView(full).missions[0].share, "100%");
});

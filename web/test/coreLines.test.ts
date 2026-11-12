import assert from "node:assert/strict";
import { test } from "node:test";

import type { CoreFigure, CoreLinesAnswer } from "../src/coreLines.ts";
import { coreLinesView, rangeWords, spanWords, trendView, unitWords } from "../src/coreLines.ts";

const GIB = 1024 ** 3;
const MIB = 1024 ** 2;

function fig(key: string, name: string, unit: string, value: number | null, range: CoreFigure["range"], state: string): CoreFigure {
  return { key, name, unit, value, range, state };
}

function answer(fields: Partial<CoreLinesAnswer> = {}): CoreLinesAnswer {
  return {
    ranges_file: "/ws/core-lines.json",
    ranges_error: null,
    ignored_ranges: [],
    lines: [
      { line: "performance", north_star: fig("performance.report_to_live", "Time from report to live", "ms", null, null, "not_collected"), guardrails: [] },
      {
        line: "storage",
        north_star: fig("storage.days_left", "Days until the disk is full", "days", 1234, { min: 180 }, "in"),
        guardrails: [
          fig("storage.free_bytes", "Free disk", "bytes", 104 * GIB, { min: 40 * GIB }, "in"),
          fig("storage.archive_growth", "Transcript archive growth", "bytes_per_day", MIB, { max: 512 * 1024 }, "out"),
          fig("storage.database_ratio", "Database against its log", "ratio", 4.7123, { max: 10 }, "in"),
          fig("storage.test_database_bytes", "Largest test database", "bytes", 3 * GIB, { min: MIB, max: 500 * MIB }, "out"),
          fig("storage.test_database_schemas", "Most test schemas", "count", 1214, null, "no_range"),
        ],
      },
      { line: "observability", north_star: fig("observability.needs_you_right", "Needs you is right", "share", null, null, "not_collected"), guardrails: [] },
      { line: "recoverability", north_star: fig("recoverability.restores_passed", "Critical resources with a restore passed in 7 days", "count", null, { min: 6 }, "not_collected"), guardrails: [] },
    ],
    ...fields,
  };
}

test("coreLinesView names the four lines and says each figure, its range and its tone", () => {
  const view = coreLinesView(answer());
  assert.deepEqual(view.rows.map((row) => row.line), ["Performance", "Storage", "Observability", "Recoverability"]);
  assert.deepEqual(view.rows[0].north_star, { name: "Time from report to live", value: "not collected yet", range: "no range set", tone: "muted" });
  assert.deepEqual(view.rows[1].north_star, { name: "Days until the disk is full", value: "1,234 days", range: "180 days or more", tone: "plain" });
  assert.deepEqual(view.rows[1].guardrails, [
    { name: "Free disk", value: "104.0 GB", range: "40.0 GB or more", tone: "plain" },
    { name: "Transcript archive growth", value: "1.0 MB a day", range: "0.5 MB a day or less", tone: "warning" },
    { name: "Database against its log", value: "4.7×", range: "10.0× or less", tone: "plain" },
    { name: "Largest test database", value: "3.0 GB", range: "between 1.0 MB and 500.0 MB", tone: "warning" },
    { name: "Most test schemas", value: "1,214", range: "no range set", tone: "muted" },
  ]);
  assert.deepEqual(view.rows[3].north_star, {
    name: "Critical resources with a restore passed in 7 days",
    value: "not collected yet",
    range: "6 or more",
    tone: "muted",
  });
  assert.deepEqual(view.rows[2].guardrails, []);
  assert.equal(view.notice, null);
  assert.deepEqual(view.ignored, []);
});

test("the card says a broken ranges file and each range left out", () => {
  const broken = coreLinesView(answer({ ranges_error: "expected value at line 1 column 34" }));
  assert.equal(broken.notice, "ranges file unreadable: expected value at line 1 column 34");
  const ignoring = coreLinesView(
    answer({
      ignored_ranges: [
        { key: "storage.free_bites", reason: "unknown figure" },
        { key: "storage.test_database_schemas", reason: "needs a number min or max" },
      ],
    }),
  );
  assert.equal(ignoring.notice, null);
  assert.deepEqual(ignoring.ignored, ["storage.free_bites: unknown figure", "storage.test_database_schemas: needs a number min or max"]);
});

test("each unit in words, and each range form", () => {
  assert.equal(unitWords("bytes", 40 * GIB), "40.0 GB");
  assert.equal(unitWords("bytes_per_day", 512 * MIB), "512.0 MB a day");
  assert.equal(unitWords("days", 1), "1 days");
  assert.equal(unitWords("ratio", 10), "10.0×");
  assert.equal(unitWords("count", 1_234_567), "1,234,567");
  assert.equal(unitWords("ms", 5_400_000), "1 h 30 min");
  assert.equal(unitWords("share", 0.954), "95.4%");
  assert.equal(unitWords("usd", 8.08), "$8.08");
  assert.equal(unitWords("ms", 1_920_000), "32 min");
  assert.equal(unitWords("ms", 108_000_000), "30 h");
  assert.equal(rangeWords("ms", { max: 7_200_000 }), "2 h or less");
  assert.equal(rangeWords("ms", { max: 93_600_000 }), "26 h or less");
  assert.equal(rangeWords("count", { min: 6 }), "6 or more");
  assert.equal(rangeWords("count", { max: 200 }), "200 or less");
  assert.equal(rangeWords("count", { min: 1, max: 200 }), "between 1 and 200");
  assert.equal(rangeWords("count", null), "no range set");
  assert.equal(rangeWords("count", {}), "no range set");
});

test("an answer missing its parts, or none, reads as an empty card", () => {
  assert.deepEqual(coreLinesView(null), { rows: [], notice: null, ignored: [] });
  assert.deepEqual(coreLinesView({}), { rows: [], notice: null, ignored: [] });
});

// Objective 086: the performance line's units.
test("ms reads as a span, the minutes rounded", () => {
  assert.equal(spanWords(45_000), "45 s");
  assert.equal(spanWords(0), "0 s");
  assert.equal(spanWords(59_400), "59 s");
  assert.equal(spanWords(60_000), "1 min");
  assert.equal(spanWords(840_000), "14 min");
  assert.equal(spanWords(869_000), "14 min");
  assert.equal(spanWords(3_570_000), "1 h", "59.5 min rounds to the hour");
  assert.equal(spanWords(3_600_000), "1 h");
  assert.equal(spanWords(5_520_000), "1 h 32 min");
  assert.equal(spanWords(26 * 3_600_000 + 5 * 60_000), "26 h 5 min");
  assert.equal(rangeWords("ms", { max: 3_600_000 }), "1 h or less");
  assert.equal(rangeWords("ms", { max: 900_000 }), "15 min or less");
});

test("share reads as a percent to one decimal, usd as dollars and cents", () => {
  assert.equal(unitWords("share", 0.749), "74.9%");
  assert.equal(unitWords("share", 0.08), "8.0%");
  assert.equal(unitWords("share", 1), "100.0%");
  assert.equal(rangeWords("share", { min: 0.7 }), "70.0% or more");
  assert.equal(rangeWords("share", { max: 0.1 }), "10.0% or less");
  assert.equal(unitWords("usd", 8.08), "$8.08");
  assert.equal(unitWords("usd", 1234.5), "$1,234.50");
  assert.equal(unitWords("usd", 0.004), "$0.00");
  assert.equal(rangeWords("usd", { max: 10 }), "$10.00 or less");
});

test("the performance row in words, an out-of-range north star in the warning tone", () => {
  const fig = (key: string, name: string, unit: string, value: number | null, range: CoreFigure["range"], state: string) =>
    ({ key, name, unit, value, range, state }) as CoreFigure;
  const view = coreLinesView({
    lines: [
      {
        line: "performance",
        north_star: fig("performance.report_to_live", "Time from report to live", "ms", 5_520_000, { max: 3_600_000 }, "out"),
        guardrails: [
          fig("performance.gate_time", "Gate time per run", "ms", 840_000, { max: 900_000 }, "in"),
          fig("performance.decided_nothing", "Gate time that decided nothing", "share", 0.08, { max: 0.1 }, "in"),
          fig("performance.first_pass", "Landings that pass first time", "share", null, { min: 0.7 }, "not_collected"),
          fig("performance.usd_per_landing", "Dollars per landing", "usd", 8.08, { max: 10 }, "in"),
        ],
      },
    ],
  });
  const [row] = view.rows;
  assert.equal(row.line, "Performance");
  assert.deepEqual(row.north_star, { name: "Time from report to live", value: "1 h 32 min", range: "1 h or less", tone: "warning" });
  assert.deepEqual(row.guardrails, [
    { name: "Gate time per run", value: "14 min", range: "15 min or less", tone: "plain" },
    { name: "Gate time that decided nothing", value: "8.0%", range: "10.0% or less", tone: "plain" },
    { name: "Landings that pass first time", value: "not collected yet", range: "70.0% or more", tone: "muted" },
    { name: "Dollars per landing", value: "$8.08", range: "$10.00 or less", tone: "plain" },
  ]);
});

test("the recoverability row (objective 087): the ages as spans, the reds as a count", () => {
  const view = coreLinesView({
    lines: [
      {
        line: "recoverability",
        north_star: fig("recoverability.restores_passed", "Critical resources with a restore passed in 7 days", "count", null, null, "not_collected"),
        guardrails: [
          fig("recoverability.cloud_dump_age", "Age of the newest database copy", "ms", 1_920_000, { max: 7_200_000 }, "in"),
          fig("recoverability.cloud_bundle_age", "Age of the newest bundle", "ms", 108_000_000, { max: 93_600_000 }, "out"),
          fig("recoverability.nightly_age", "Age of the oldest nightly judging", "ms", null, { max: 93_600_000 }, "not_collected"),
          fig("recoverability.nightly_red", "Nightly sources held red", "count", 1, { max: 0 }, "out"),
        ],
      },
    ],
    ranges_file: null,
    ranges_error: null,
    ignored_ranges: [],
  });
  const row = view.rows[0];
  assert.equal(row.line, "Recoverability");
  assert.equal(row.north_star.value, "not collected yet");
  assert.deepEqual(
    row.guardrails.map((mark) => [mark.value, mark.range, mark.tone]),
    [
      ["32 min", "2 h or less", "plain"],
      ["30 h", "26 h or less", "warning"],
      ["not collected yet", "26 h or less", "muted"],
      ["1", "0 or less", "warning"],
    ],
  );
});

function trended(...values: number[]): CoreFigure {
  const figure = fig("storage.free_bytes", "Free disk", "bytes", 120 * GIB, null, "no_range");
  figure.trend = values.map((value, i) => ({ day: `2026-11-0${i + 1}`, value }));
  return figure;
}

test("trendView gives the points and which way they went, first to last (objective 090)", () => {
  assert.deepEqual(trendView(trended(100 * GIB, 110 * GIB, 120 * GIB)), { points: [100 * GIB, 110 * GIB, 120 * GIB], change: "up 20%" });
  assert.deepEqual(trendView(trended(200, 150)), { points: [200, 150], change: "down 25%" });
  // Under half a percent either way is steady; half a percent is not.
  assert.equal(trendView(trended(1000, 1004)).change, "steady");
  assert.equal(trendView(trended(1000, 996)).change, "steady");
  assert.equal(trendView(trended(1000, 1005)).change, "up 1%");
  // Relative to the first point's size: a negative start that rises is up.
  assert.equal(trendView(trended(-200, -100)).change, "up 50%");
  // Only the first and the last count.
  assert.equal(trendView(trended(100, 500, 1, 150)).change, "up 50%");
});

test("trendView has no change with fewer than two points or a first point of 0", () => {
  assert.deepEqual(trendView(trended(5)), { points: [5], change: null });
  assert.deepEqual(trendView(trended(0, 5)), { points: [0, 5], change: null });
  assert.deepEqual(trendView(trended()), { points: [], change: null });
  // A figure from before the trends, or none.
  assert.deepEqual(trendView(fig("a", "A", "count", 1, null, "no_range")), { points: [], change: null });
  assert.deepEqual(trendView(null), { points: [], change: null });
});

test("coreLinesView's marks stay as they were with trends in the answer", () => {
  const plain = coreLinesView(answer());
  const withTrends = answer();
  withTrends.lines[1].guardrails[0].trend = [{ day: "2026-11-12", value: 1 }];
  assert.deepEqual(coreLinesView(withTrends), plain);
});


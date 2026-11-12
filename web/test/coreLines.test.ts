import assert from "node:assert/strict";
import { test } from "node:test";

import type { CoreFigure, CoreLinesAnswer } from "../src/coreLines.ts";
import { coreLinesView, rangeWords, spanWords, unitWords } from "../src/coreLines.ts";

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
      {
        line: "observability",
        north_star: fig("observability.needs_you_right", "Needs you is right", "share", null, null, "not_collected"),
        guardrails: [
          fig("observability.unpriced_share", "Use with no price", "share", 0.38, null, "no_range"),
          fig("observability.interventions_per_agent_hour", "Human interventions per agent-hour", "ratio", 0.125, { max: 0.05 }, "out"),
          fig("observability.not_measured", "Figures not measured", "count", 2, null, "no_range"),
        ],
      },
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
  assert.deepEqual(view.rows[2].guardrails, [
    { name: "Use with no price", value: "38%", range: "no range set", tone: "muted" },
    { name: "Human interventions per agent-hour", value: "0.13", range: "0.05 or less", tone: "warning" },
    { name: "Figures not measured", value: "2", range: "no range set", tone: "muted" },
  ]);
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
  assert.equal(unitWords("ratio", 0.125), "0.13");
  assert.equal(unitWords("ratio", 0.05), "0.05");
  assert.equal(unitWords("ratio", 0), "0");
  assert.equal(rangeWords("ratio", { max: 0.05 }), "0.05 or less");
  assert.equal(unitWords("count", 1_234_567), "1,234,567");
  assert.equal(unitWords("ms", 5_400_000), "1 h 30 min");
  assert.equal(unitWords("share", 0.954), "95.4%");
  assert.equal(unitWords("usd", 8.08), "$8.08");
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

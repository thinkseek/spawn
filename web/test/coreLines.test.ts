import assert from "node:assert/strict";
import { test } from "node:test";

import type { CoreFigure, CoreLinesAnswer } from "../src/coreLines.ts";
import { coreLinesView, drillMark, rangeWords, sinceWords, spanWords, trendView, unitWords } from "../src/coreLines.ts";

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
    { name: "Use with no price", value: "38.0%", range: "no range set", tone: "muted" },
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
  // On Home, in words, never the key and reason (lead-mac 2026-11-12 18:15).
  assert.deepEqual(ignoring.pending, [
    "free bites: figure not built yet",
    "Most test schemas: range not read (it needs a number min or max)",
  ]);
  const leaked = coreLinesView(answer({ ignored_ranges: [{ key: "storage.leaked_test_schemas", reason: "unknown figure" }] }));
  assert.deepEqual(leaked.pending, ["leaked test schemas: figure not built yet"]);
});

test("a byte range reads as core-lines.json wrote it", () => {
  assert.equal(rangeWords("bytes_per_day", { max: 500_000_000 }), "500 MB a day or less");
  assert.equal(rangeWords("bytes", { max: 500_000_000 }), "500 MB or less");
  assert.equal(rangeWords("bytes", { min: 40_000_000_000 }), "40 GB or more");
  assert.equal(rangeWords("bytes", { min: 1_500_000_000 }), "1.5 GB or more");
  assert.equal(rangeWords("bytes", { min: 1_000_000, max: 500_000_000 }), "between 1 MB and 500 MB");
  // Written in 1024s, it reads as storageSize says it (084's words).
  assert.equal(rangeWords("bytes_per_day", { max: 512 * MIB }), "512.0 MB a day or less");
  assert.equal(rangeWords("bytes", { min: 40 * GIB }), "40.0 GB or more");
  assert.equal(rangeWords("bytes", { min: MIB, max: 500 * MIB }), "between 1.0 MB and 500.0 MB");
  assert.equal(rangeWords("bytes", { max: 0 }), "0.0 MB or less");
  // A value is still storageSize's.
  assert.equal(unitWords("bytes", 500_000_000), "476.8 MB");
});

test("report to live while its window is not covered: measuring since, local time", () => {
  const since = new Date(2026, 10, 12, 9, 5).getTime();
  assert.equal(sinceWords(since), "11-12 09:05");
  const view = coreLinesView({
    lines: [
      {
        line: "performance",
        north_star: {
          key: "performance.report_to_live",
          name: "Time from report to live",
          unit: "ms",
          value: null,
          range: { max: 3_600_000 },
          state: "not_collected",
          measuring_since_ms: since,
        },
        guardrails: [],
      },
    ],
  });
  assert.deepEqual(view.rows[0].north_star, {
    name: "Time from report to live",
    value: "measuring since 11-12 09:05",
    range: "1 h or less",
    tone: "muted",
  });
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
  assert.deepEqual(coreLinesView(null), { rows: [], notice: null, ignored: [], pending: [], drills: [] });
  assert.deepEqual(coreLinesView({}), { rows: [], notice: null, ignored: [], pending: [], drills: [] });
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


test("a figure whose window is not covered yet says measuring since, in local time (objective 095)", () => {
  // Local wall-clock fields, so the words hold in any time zone.
  const since = new Date(2026, 10, 2, 7, 5).getTime();
  const waiting = answer();
  waiting.lines[0].north_star = { ...waiting.lines[0].north_star, measuring_since_ms: since };
  assert.deepEqual(coreLinesView(waiting).rows[0].north_star, {
    name: "Time from report to live",
    value: "measuring since 11-02 07:05",
    range: "no range set",
    tone: "muted",
  });
  // Covered: the figure's own value, no matter a stray null.
  const covered = answer();
  covered.lines[0].north_star = { ...covered.lines[0].north_star, value: 5_400_000, state: "no_range", measuring_since_ms: null };
  assert.equal(coreLinesView(covered).rows[0].north_star.value, "1 h 30 min");
});

// Objective 093: the recovery table's rows under the recoverability line.
test("each drill row says its last pass, never passed, or no drill", () => {
  const now = Date.UTC(2026, 10, 12, 12, 0);
  const view = coreLinesView(
    {
      lines: [],
      drills: [
        { name: "Database", source: "record backup.verified", passed_at_ms: now - 3 * 3_600_000 },
        { name: "Old drill", source: "unit bb-drill-c.service", passed_at_ms: now - 8 * 86_400_000 },
        { name: "Off-host copy", source: "unit bb-drill-b.service", passed_at_ms: null },
        { name: "Configuration and credentials", source: null, passed_at_ms: null },
      ],
    },
    now,
  );
  assert.deepEqual(
    view.drills.map((drill) => [drill.name, drill.tone]),
    [
      ["Database", "plain"],
      ["Old drill", "warning"],
      ["Off-host copy", "warning"],
      ["Configuration and credentials", "muted"],
    ],
  );
  assert.match(view.drills[0].text, /^passed \d\d-\d\d \d\d:\d\d \(3 h ago\)$/);
  assert.match(view.drills[1].text, /\(192 h ago\)$/);
  assert.equal(view.drills[2].text, "never passed");
  assert.equal(view.drills[3].text, "no drill");
  assert.equal(drillMark(null, now).text, "no drill");
});

// Objective 112: a row whose newest drill failed says so, with its reason.
test("a drill row whose newest drill failed says failed, its last pass, and its reason", () => {
  const now = Date.UTC(2026, 10, 12, 12, 0);
  const view = coreLinesView(
    {
      lines: [],
      drills: [
        {
          name: "Database",
          source: "record backup.verified",
          passed_at_ms: now - 3 * 3_600_000,
          last: { at_ms: now - 3_600_000, ok: false, reason: "pg_restore exited 1" },
        },
        {
          name: "Off-host copy",
          source: "unit bb-drill-b.service",
          passed_at_ms: null,
          last: { at_ms: null, ok: false, reason: "Result=exit-code" },
        },
        {
          name: "Archive",
          source: "unit bb-drill-a.service",
          passed_at_ms: now - 3_600_000,
          last: { at_ms: now - 3_600_000, ok: true, reason: "" },
        },
      ],
    },
    now,
  );
  const [db, off, archive] = view.drills;
  assert.match(db.text, /^failed \d\d-\d\d \d\d:\d\d \(1 h ago\); last passed \d\d-\d\d \d\d:\d\d \(3 h ago\)$/);
  assert.equal(db.tone, "warning");
  assert.equal(db.detail, "pg_restore exited 1");
  assert.equal(off.text, "failed (time unknown); never passed");
  assert.equal(off.tone, "warning");
  assert.equal(off.detail, "Result=exit-code");
  assert.match(archive.text, /^passed .* \(1 h ago\)$/);
  assert.equal(archive.tone, "plain");
  assert.equal(archive.detail, "");
  assert.equal(drillMark(null, now).detail, "");
});

// Objective 094: a test database URL file the reader refused is said by its
// name and why, not as a figure not built.
test("a refused test database URL file is said by its name and its reason", () => {
  const view = coreLinesView({
    lines: [],
    ignored_ranges: [{ key: "storage.test_database_url_files.loose", reason: "mode 0644, not 0600" }],
  });
  assert.deepEqual(view.pending, ["test database URL file loose not used: mode 0644, not 0600"]);
});

// Objective 100: interventions per agent-hour is the Home card's, over its
// 24 h window, said beside its name; a figure without a window is named as
// it was.
test("a figure that says its window is named with it", () => {
  const view = coreLinesView({
    lines: [
      {
        line: "observability",
        north_star: { key: "observability.needs_you_right", name: "Needs you is right", unit: "share", value: null, range: null, state: "not_collected" },
        guardrails: [
          { key: "observability.interventions_per_agent_hour", name: "Human interventions per agent-hour", unit: "ratio", value: 0.25, range: null, state: "no_range", window: "24h" },
          { key: "observability.unpriced_share", name: "Use with no price", unit: "share", value: 0, range: null, state: "no_range" },
        ],
      },
    ],
  });
  assert.deepEqual(view.rows[0].guardrails.map((mark) => mark.name), [
    "Human interventions per agent-hour (last 24 h)",
    "Use with no price",
  ]);
  assert.equal(view.rows[0].north_star.name, "Needs you is right");
});

// Objective 141: the store that grew most says its store after its name;
// a figure without one is named as it was.
test("a figure that names its store says it after its name", () => {
  const view = coreLinesView({
    lines: [
      {
        line: "storage",
        north_star: { key: "storage.days_left", name: "Days until the disk is full", unit: "days", value: 7, range: null, state: "no_range" },
        guardrails: [
          { key: "storage.disk_fall", name: "Free disk lost a day", unit: "bytes_per_day", value: 10737418240, range: null, state: "no_range" },
          { key: "storage.grew_most", name: "Store that grew most", unit: "bytes_per_day", value: 10737418240, range: null, state: "no_range", store: "unattributed" },
        ],
      },
    ],
  });
  assert.deepEqual(view.rows[0].guardrails.map((mark) => mark.name), ["Free disk lost a day", "Store that grew most: unattributed"]);
  assert.equal(view.rows[0].north_star.name, "Days until the disk is full");
});

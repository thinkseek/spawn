import assert from "node:assert/strict";
import { test } from "node:test";

import type { CoreFigure, CoreLinesAnswer } from "../src/coreLines.ts";
import { coreLinesView, drillMark, missionMark, rangeWords, scratchMark, sinceWords, spanWords, trendView, unitWords } from "../src/coreLines.ts";

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
  const empty = { rows: [], notice: null, ignored: [], pending: [], drills: [], scratch: [], byMission: [], diagnostics: [] };
  assert.deepEqual(coreLinesView(null), empty);
  assert.deepEqual(coreLinesView({}), empty);
});

// Objective 176: the owner's core metric set (2026-11-14).
test("the adopted set (objective 176): three rows, not measured yet, the diagnostics apart, a retired range named", () => {
  const figure = (key: string, name: string, unit: string, value: number | null, state: string, range: object | null = null) => ({
    key, name, unit, value, range, state, trend: [],
  });
  const view = coreLinesView({
    lines: [
      {
        line: "storage",
        north_star: figure("storage.bytes_per_landing", "Bytes a landing leaves behind", "bytes", null, "not_measured_yet", { max: 52_428_800 }),
        guardrails: [
          figure("storage.rebuildable_bytes", "Rebuildable bytes", "bytes", null, "not_measured_yet"),
          figure("storage.kept_share", "Share kept on purpose", "share", null, "not_measured_yet"),
        ],
      },
      { line: "performance", north_star: figure("performance.report_to_live", "Time from report to live", "ms", null, "not_collected"), guardrails: [] },
      {
        line: "sustainability",
        north_star: { ...figure("observability.interventions_per_agent_hour", "Human interventions per agent-hour", "ratio", 0.125, "out", { max: 0.1 }), window: "24h" },
        guardrails: [],
      },
    ],
    alert_figures: [{ ...figure("storage.free_bytes", "Free disk", "bytes", 1024, "out", { min: 1e18 }), line: "storage" }],
    diagnostics: [
      figure("storage.unowned_scratch_bytes", "Scratch nobody owns", "bytes", 400_000, "out", { max: 0 }),
      { ...figure("storage.leaked_test_schemas", "Leaked test schemas", "count", 0, "in", { max: 0 }), line: "storage" },
    ],
    ranges_file: null,
    ranges_error: null,
    ignored_ranges: [{ key: "storage.days_left", reason: "retired by the owner's core metric set (2026-11-14)" }],
  });
  assert.deepEqual(view.rows.map((row) => row.line), ["Storage", "Performance", "Sustainability"]);
  assert.deepEqual(view.rows[0].north_star, {
    name: "Bytes a landing leaves behind",
    value: "not measured yet",
    range: "50.0 MB or less",
    tone: "muted",
  });
  assert.deepEqual(view.rows[0].guardrails.map((mark) => mark.value), ["not measured yet", "not measured yet"]);
  assert.equal(view.rows[1].north_star.value, "not collected yet", "not collected is not not measured yet");
  assert.equal(view.rows[2].north_star.tone, "warning");
  const onRows = view.rows.flatMap((row) => [row.north_star, ...row.guardrails]).map((mark) => mark.name);
  assert.ok(!onRows.includes("Free disk") && !onRows.some((name) => name.startsWith("Scratch nobody owns")), `${onRows}`);
  assert.deepEqual(view.diagnostics.map((mark) => [mark.name, mark.tone]), [
    ["Scratch nobody owns", "warning"],
    ["Leaked test schemas", "plain"],
  ]);
  assert.deepEqual(view.pending, ["days left: retired from the core metric set"]);
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

// Objective 225: storage.disk_fall says which fall it is (its basis since
// 222) after its name; without a basis, for null or for one it does not
// know, the name is as it was, and the value never carries it.
test("the free disk lost a day says its basis after its name", () => {
  const fall = (basis?: unknown) => {
    const view = coreLinesView({
      lines: [],
      diagnostics: [
        { key: "storage.disk_fall", name: "Free disk lost a day", unit: "bytes_per_day", value: 77104383700, range: null, state: "no_range", ...(basis === undefined ? {} : { basis }) } as CoreFigure,
      ],
    });
    const mark = view.diagnostics[0];
    assert.doesNotMatch(mark.value, /fit|fall/);
    return mark.name;
  };
  assert.equal(fall("day"), "Free disk lost a day (the last day's fall)");
  assert.equal(fall("fit"), "Free disk lost a day (the week's fit)");
  assert.equal(fall(), "Free disk lost a day");
  assert.equal(fall(null), "Free disk lost a day");
  assert.equal(fall("median"), "Free disk lost a day");
});

// Objective 230: below zero, the disk fall says "gained" and the store that
// grew most "shrank least", each with the size as a positive number; at
// zero and above, and for every other figure, the mark is as it was.
test("a disk that gained and stores that all shrank say so with positive sizes", () => {
  const range = { min: 0, max: 50e9 };
  const one = (figure: Partial<CoreFigure>) =>
    coreLinesView({
      lines: [],
      diagnostics: [{ unit: "bytes_per_day", range: null, state: "no_range", ...figure } as CoreFigure],
    }).diagnostics[0];
  const fall = (value: number, basis?: unknown) =>
    one({ key: "storage.disk_fall", name: "Free disk lost a day", value, range, state: "in", ...(basis === undefined ? {} : { basis }) });
  const gained = -2_792_160_000;
  // Below zero under each basis, and none: gained, the size positive.
  for (const [basis, words] of [
    ["fit", "Free disk gained a day (the week's fit)"],
    ["day", "Free disk gained a day (over the last day)"],
    [undefined, "Free disk gained a day"],
    [null, "Free disk gained a day"],
  ] as const) {
    const mark = fall(gained, basis);
    assert.equal(mark.name, words, String(basis));
    assert.equal(mark.value, unitWords("bytes_per_day", -gained));
    assert.doesNotMatch(mark.value, /-|−/);
    // The range and the tone are as they were.
    assert.equal(mark.range, fall(-gained, basis).range);
    assert.equal(mark.tone, "plain");
  }
  // rev-j's "lost -2662.8 MB a day": the positive size's own words now.
  assert.equal(fall(gained).value, "2.6 GB a day");
  // Above zero and at zero: as before.
  assert.equal(fall(10e9, "day").name, "Free disk lost a day (the last day's fall)");
  assert.equal(fall(10e9, "fit").name, "Free disk lost a day (the week's fit)");
  assert.equal(fall(10e9, "day").value, unitWords("bytes_per_day", 10e9));
  assert.equal(fall(0, "day").name, "Free disk lost a day (the last day's fall)");
  assert.equal(fall(0, "fit").value, unitWords("bytes_per_day", 0));

  // The store that grew most: below zero shrank least, the size positive.
  const most = (value: number) =>
    one({ key: "storage.grew_most", name: "Store that grew most", value, store: "unattributed" });
  assert.equal(most(-5e9).name, "Store that shrank least: unattributed");
  assert.equal(most(-5e9).value, unitWords("bytes_per_day", 5e9));
  assert.equal(most(-5e9).tone, "muted");
  assert.equal(most(5e9).name, "Store that grew most: unattributed");
  assert.equal(most(5e9).value, unitWords("bytes_per_day", 5e9));
  assert.equal(most(0).name, "Store that grew most: unattributed");

  // Another figure below zero: as before, minus and all.
  const other = one({ key: "storage.archive_growth", name: "Archive grows a day, lost or grew most", value: -5e9 });
  assert.equal(other.name, "Archive grows a day, lost or grew most");
  assert.equal(other.value, unitWords("bytes_per_day", -5e9));
});

// Objective 148: "Needs you is right" says what it is read from after its
// name, the needs judged and not; a figure without it is named as it was.
test("a judged share says what it is read from after its name", () => {
  const line = (judged?: unknown) =>
    coreLinesView({
      lines: [
        {
          line: "observability",
          north_star: { key: "observability.needs_you_right", name: "Needs you is right", unit: "share", value: 0.6, range: null, state: "no_range", ...(judged === undefined ? {} : { judged }) } as CoreFigure,
          guardrails: [{ key: "observability.unpriced_share", name: "Use with no price", unit: "share", value: 0, range: null, state: "no_range" }],
        },
      ],
    }).rows[0];
  const row = line({ fired: 6, judged: 5, unjudged: 1 });
  assert.equal(row.north_star.name, "Needs you is right (5 judged, 1 unjudged of 6)");
  assert.equal(row.north_star.value, "60.0%");
  assert.deepEqual(row.guardrails.map((mark) => mark.name), ["Use with no price"]);
  // Not collected under 5 judged: still says what it is read from.
  assert.equal(line({ fired: 6, judged: 4, unjudged: 2 }).north_star.name, "Needs you is right (4 judged, 2 unjudged of 6)");
  // Not read: the name alone.
  assert.equal(line().north_star.name, "Needs you is right");
  assert.equal(line(null).north_star.name, "Needs you is right");
});

// Objective 140: each claimed scratch folder is said by its owner and
// objective, its bytes and its build's, its age, its lease and whether its
// objective landed; landed with its lease ended is the one to clear.
test("a claimed scratch folder says its owner, size, age, lease and landing", () => {
  const now = Date.UTC(2026, 10, 14, 12, 0);
  const lease = now + 3 * 3_600_000;
  const folder = {
    path: "/s/rev-e-140",
    agent: "rev-e",
    objective: "4/140",
    created_ms: now - 2 * 3_600_000,
    lease_until_ms: lease,
    bytes: 3 * GIB,
    build_bytes: 2 * GIB,
    landed: false,
  };
  assert.deepEqual(scratchMark(folder, now), {
    name: "rev-e · 4/140",
    text: `3.0 GB (build 2.0 GB) · 2 h old · lease until ${sinceWords(lease)} · not landed`,
    tone: "plain",
    detail: "/s/rev-e-140",
  });
  const ended = { ...folder, lease_until_ms: now, landed: true };
  assert.equal(scratchMark(ended, now).text, "3.0 GB (build 2.0 GB) · 2 h old · lease ended · landed");
  assert.equal(scratchMark(ended, now).tone, "warning", "landed and its lease ended");
  assert.equal(scratchMark({ ...folder, landed: true }, now).tone, "plain", "its lease still runs");
  assert.equal(scratchMark({ ...ended, landed: false }, now).tone, "plain", "not landed");
  assert.match(scratchMark({ ...folder, landed: null }, now).text, / · landing not read$/);
  const view = coreLinesView({ lines: [], scratch: { claimed: [folder, ended] } }, now);
  assert.deepEqual(view.scratch.map((mark) => mark.tone), ["plain", "warning"]);
});

test("no claimed scratch folder, or no walk, is an empty list", () => {
  assert.deepEqual(coreLinesView({ lines: [], scratch: { claimed: [] } }).scratch, []);
  assert.deepEqual(coreLinesView({ lines: [] }).scratch, []);
  assert.deepEqual(coreLinesView(null).scratch, []);
});

// Objective 155: a folder whose lease runs is never walked whole: its bytes
// are null, and the card says its build paths' size only.
test("a leased scratch folder says its build size, not walked whole", () => {
  const now = Date.UTC(2026, 10, 14, 12, 0);
  const lease = now + 3 * 3_600_000;
  const folder = {
    path: "/s/held",
    agent: "rev-e",
    objective: "1/155",
    created_ms: now - 2 * 3_600_000,
    lease_until_ms: lease,
    bytes: null,
    build_bytes: 2 * GIB,
    landed: false,
  };
  assert.equal(
    scratchMark(folder, now).text,
    `build 2.0 GB (not walked whole while leased) · 2 h old · lease until ${sinceWords(lease)} · not landed`,
  );
  assert.equal(scratchMark({ ...folder, bytes: 0 }, now).text.startsWith("0.0 MB (build 2.0 GB)"), true, "walked: its bytes");
});

// Objective 159: each mission's report to live beside the fleet's, from the
// performance north star's `by_mission`; a mission no deploy ships says its
// landings were live when they landed.
test("the card says each mission's report to live beside the fleet's", () => {
  const figure = (key: string, extra: Partial<CoreFigure>): CoreFigure => ({
    key,
    name: key,
    unit: "ms",
    value: 360_000,
    range: null,
    state: "no_range",
    ...extra,
  });
  const answer: Partial<CoreLinesAnswer> = {
    lines: [
      {
        line: "storage",
        north_star: figure("storage.days_left", { by_mission: [{ mission: 1, value: 1, n: 3, live_at_landing: 0 }] }),
        guardrails: [],
      },
      {
        line: "performance",
        north_star: figure("performance.report_to_live", {
          by_mission: [
            { mission: 4, value: 3_600_000, n: 3, live_at_landing: 0 },
            { mission: 6, value: null, n: 2, live_at_landing: 1 },
            { mission: 9, value: 360_000, n: 3, live_at_landing: 3 },
          ],
        }),
        guardrails: [],
      },
    ],
  };
  assert.deepEqual(coreLinesView(answer).byMission, [
    { name: "mission 4", text: "1 h (3 live)", tone: "plain" },
    { name: "mission 6", text: "2 live, too few for a median", tone: "plain" },
    { name: "mission 9", text: "6 min (3 live) · live when landed (no deploy ships it)", tone: "muted" },
  ]);
  assert.deepEqual(
    coreLinesView({ lines: [answer.lines![0]] }).byMission,
    [],
    "another line's by_mission is not the performance line's",
  );
  assert.deepEqual(missionMark(null), { name: "mission ?", text: "0 live, too few for a median", tone: "plain" });
});

test("a target is said apart from the range, a count out of its rows, a held range by its note (objective 181)", () => {
  const view = coreLinesView(
    answer({
      lines: [
        {
          line: "sustainability",
          north_star: { ...fig("observability.interventions_per_agent_hour", "Human interventions per agent-hour", "ratio", 0.05, { max: 0.1 }, "in"), target: 0.1 },
          guardrails: [
            { ...fig("recoverability.restores_passed", "Critical resources with a restore passed in 7 days", "count", 1, null, "no_range"), of: 3 },
            {
              ...fig("observability.needs_you_right", "Needs you is right", "share", 0.5, null, "no_range"),
              range_note: "range proposed after 2026-11-20",
            },
          ],
        },
        {
          line: "storage",
          north_star: { ...fig("storage.bytes_per_landing", "Storage per landing", "bytes", 40 * MIB, null, "no_range"), target: 52428800 },
          guardrails: [],
        },
      ],
    }),
  );
  const [sustainability, storage] = view.rows;
  assert.deepEqual(sustainability.north_star, {
    name: "Human interventions per agent-hour",
    value: "0.05",
    range: "0.1 or less",
    target: "target 0.1",
    tone: "plain",
  });
  // A target alone: no range, its target apart, never judged.
  assert.equal(storage.north_star.range, "no range set");
  assert.equal(storage.north_star.target, "target 50.0 MB");
  assert.equal(storage.north_star.tone, "muted");
  const [u1, u2] = sustainability.guardrails;
  assert.equal(u1.value, "1 of 3");
  assert.equal(u1.target, undefined, "no target, none said");
  assert.equal(u2.value, "50.0%");
  assert.equal(u2.range, "range proposed after 2026-11-20", "the note where the range would be");
  assert.equal(u2.tone, "muted");
  // Without `of` or a note, as before.
  const bare = coreLinesView(answer()).rows[1].north_star;
  assert.deepEqual(bare, { name: "Days until the disk is full", value: "1,234 days", range: "180 days or more", tone: "plain" });
});

test("a line's plain reading says its note, never a range; free disk is a diagnostic (objective 182)", () => {
  const note = "no range until use with no price is under 10%";
  const view = coreLinesView(
    answer({
      lines: [
        {
          line: "storage",
          north_star: fig("storage.bytes_per_landing", "Bytes a landing leaves behind", "bytes", null, null, "not_measured_yet"),
          guardrails: [],
          readings: [],
        },
        {
          line: "sustainability",
          north_star: fig("observability.interventions_per_agent_hour", "Human interventions per agent-hour", "ratio", 0.05, null, "no_range"),
          guardrails: [],
          readings: [{ ...fig("performance.usd_per_landing", "Dollars per landing", "usd", 2.5, null, "no_range"), range_note: note }],
        },
      ],
      diagnostics: [
        fig("storage.free_bytes", "Free disk", "bytes", 40 * GIB, null, "no_range"),
        fig("storage.disk_fall", "Disk fall", "bytes_per_day", 5 * MIB, { max: 500_000_000 }, "in"),
      ],
      ignored_ranges: [
        { key: "performance.usd_per_landing", reason: "shown with no range until use with no price is under 10% (the owner's core metric set)" },
        { key: "storage.free_bytes", reason: "an alert of its own (disk_low, objective 204)" },
      ],
    }),
  );
  const [storage, sustainability] = view.rows;
  assert.deepEqual(storage.readings, []);
  assert.equal(sustainability.readings.length, 1);
  const [usd] = sustainability.readings;
  assert.equal(usd.name, "Dollars per landing");
  assert.equal(usd.range, note, "the note where the range would be");
  assert.equal(usd.target, undefined);
  assert.equal(usd.tone, "muted");
  // Free disk is the first diagnostic, with no range.
  assert.deepEqual(
    view.diagnostics.map((mark) => [mark.name, mark.range]),
    [
      ["Free disk", "no range set"],
      ["Disk fall", "500 MB a day or less"],
    ],
  );
  // A range ignored on purpose is said by its figure's name and why.
  assert.deepEqual(view.pending, [
    "Dollars per landing: shown with no range until use with no price is under 10% (the owner's core metric set)",
    "Free disk: an alert of its own (disk_low, objective 204)",
  ]);
  // A line without readings in the answer has none on the card.
  assert.deepEqual(coreLinesView(answer()).rows[0].readings, []);
});

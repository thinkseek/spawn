import assert from "node:assert/strict";
import { test } from "node:test";

import { localDateTime } from "../src/format.ts";
import {
  formatCost,
  formatDecimal,
  modelCostText,
  ownerRunsNote,
  ownerRunsTileNote,
  reportToLiveText,
  tileNote,
  undecidedText,
} from "../src/scorecardFormat.ts";

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

// Objective 067: how much gate time decided nothing.
const MIN = 60_000;
const HOUR = 60 * MIN;

test("undecidedText says how much of the gate time decided nothing, and of how much", () => {
  // rsi/scenarios/067.sh: 65 min over six runs, 35 of them undecided.
  assert.equal(
    undecidedText({ total_ms: 65 * MIN, undecided_ms: 35 * MIN }),
    "35m of 1.1 h gate time decided nothing (54%)",
  );
  assert.equal(
    undecidedText({ total_ms: 10 * HOUR, undecided_ms: 1.5 * HOUR }),
    "1.5 h of 10.0 h gate time decided nothing (15%)",
  );
  // Minutes under an hour, whole; hours from one.
  assert.equal(undecidedText({ total_ms: 40 * MIN, undecided_ms: 0 }), "0m of 40m gate time decided nothing (0%)");
  assert.equal(undecidedText({ total_ms: HOUR, undecided_ms: HOUR }), "1.0 h of 1.0 h gate time decided nothing (100%)");
  assert.equal(
    undecidedText({ total_ms: 3 * HOUR, undecided_ms: MIN / 3 }),
    "0m of 3.0 h gate time decided nothing (0%)",
  );
  // No gate time: nothing to say.
  assert.equal(undecidedText({ total_ms: 0, undecided_ms: 0 }), null);
  assert.equal(undecidedText({}), null);
  assert.equal(undecidedText(null), null);
  assert.equal(undecidedText(undefined), null);
  // An older server's answer has no parts.
  assert.equal(undecidedText({ total_ms: "x", undecided_ms: 5 }), null);
});

test("the Scorecard page's gate time tile, the fleet's and each mission's, shows the undecided line", () => {
  // The fleet's and mission 1's gate_time as /api/scorecard gives them.
  const fleet = { value: 600_000, n: 7, runs: 7, total_ms: 70 * MIN, undecided_ms: 35 * MIN, undecided_runs: 3 };
  const mission = { value: 600_000, n: 6, runs: 6, total_ms: 65 * MIN, undecided_ms: 35 * MIN, undecided_runs: 3 };
  assert.equal(tileNote("gate_time", fleet), "35m of 1.2 h gate time decided nothing (50%)");
  assert.equal(tileNote("gate_time", mission), "35m of 1.1 h gate time decided nothing (54%)");
  // A mission with no gate run shows no line; no other tile has one.
  assert.equal(tileNote("gate_time", { ...mission, runs: 0, total_ms: 0, undecided_ms: 0 }), null);
  assert.equal(tileNote("cost", { ...fleet, gate_ms: 70 * MIN } as never), null);
  assert.equal(tileNote("throughput", fleet), null);
});

// Objective 076: from when the owner's judgings are counted.
test("ownerRunsNote says from when the owner's judgings count, and nothing once the window starts after", () => {
  const from = Date.UTC(2026, 10, 11, 19, 48, 0);
  const weekBefore = from - 7 * 24 * HOUR;
  assert.equal(
    ownerRunsNote(weekBefore, from),
    `Owner judgings counted from ${localDateTime(from)} only: earlier ones carry no gate time.`,
  );
  assert.equal(ownerRunsNote(from - 1, from), `Owner judgings counted from ${localDateTime(from)} only: earlier ones carry no gate time.`);
  assert.equal(ownerRunsNote(from, from), null, "a window that starts exactly at it");
  assert.equal(ownerRunsNote(from + HOUR, from), null, "a later window");
  assert.equal(ownerRunsNote(weekBefore, null), "Owner judgings not counted yet: none carries a gate time.");
  assert.equal(ownerRunsNote(from + HOUR, null), "Owner judgings not counted yet: none carries a gate time.");
  // A card with no window start reads as one that starts before.
  assert.equal(ownerRunsNote(null, from), `Owner judgings counted from ${localDateTime(from)} only: earlier ones carry no gate time.`);
});

test("the Scorecard page's gate time tile, the fleet's and each mission's, shows the owner-runs note until the window starts after", () => {
  const from = Date.UTC(2026, 10, 11, 19, 48, 0);
  const note = `Owner judgings counted from ${localDateTime(from)} only: earlier ones carry no gate time.`;
  // Every scope's tile reads the card's window and time alike.
  for (const scope of ["fleet", "mission 1", "mission 2"]) {
    assert.equal(ownerRunsTileNote("gate_time", from - 7 * 24 * HOUR, from), note, scope);
  }
  assert.equal(ownerRunsTileNote("gate_time", from + HOUR, from), null, "nothing once the window starts after");
  assert.equal(ownerRunsTileNote("gate_time", from, null), "Owner judgings not counted yet: none carries a gate time.");
  // No other tile has it, and a server that does not say shows none.
  assert.equal(ownerRunsTileNote("cost", from - HOUR, from), null);
  assert.equal(ownerRunsTileNote("throughput", from - HOUR, null), null);
  assert.equal(ownerRunsTileNote("gate_time", from - HOUR, undefined), null);
});

// Objective 081: from a landing's report to the deploy that puts it live.
test("reportToLiveText says the median in hours, the live landings and the ones not live yet", () => {
  // rsi/scenarios/081.sh: 1.0 h over four live landings, one waiting.
  assert.equal(
    reportToLiveText({ value: 3_600_000, p90: 7_200_000, n: 4, not_live: 1 } as never),
    "1.0 h from report to live (4 landings; 1 not live yet)",
  );
  assert.equal(reportToLiveText({ value: 5_400_000, n: 1, not_live: 0 }), "1.5 h from report to live (1 landing)");
  assert.equal(reportToLiveText({ value: 2_880_000, n: 4, not_live: 3 }), "0.8 h from report to live (4 landings; 3 not live yet)");
  assert.equal(reportToLiveText({ value: 36 * 3_600_000, n: 12 }), "36.0 h from report to live (12 landings)");
  // No value (below the minimum), or no dimension (an older server): null.
  assert.equal(reportToLiveText({ value: null, n: 2, not_live: 1 }), null);
  assert.equal(reportToLiveText({}), null);
  assert.equal(reportToLiveText(null), null);
  assert.equal(reportToLiveText(undefined), null);
});

test("the Scorecard page's report-to-live tile shows that line; no other tile does", () => {
  const dimension = { value: 3_600_000, n: 4, not_live: 1 };
  assert.equal(tileNote("report_to_live", dimension as never), "1.0 h from report to live (4 landings; 1 not live yet)");
  assert.equal(tileNote("report_to_live", { value: null, n: 1 } as never), null);
  assert.equal(tileNote("lead_time", dimension as never), null);
});

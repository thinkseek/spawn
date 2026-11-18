// The scorecard's number formats. Pure, and loadable with node alone (its
// one import, format.ts, imports nothing), so node can test them
// (web/test/scorecardFormat.test.ts); scorecard.ts, which also fetches,
// cannot be loaded there.

import { compactCount, localDateTime } from "./format.ts";

/** A plain number with at most `digits` decimals, grouped: 2,033.333. */
export function formatDecimal(value: number | null, digits = 3): string {
  if (value === null || !Number.isFinite(value)) return "—";
  return value.toLocaleString("en-US", { maximumFractionDigits: digits });
}

/** Tokens per landing: whole tokens from one token up. Below one, the
 * three decimals the server rounds to (0.333), so a cost that is not zero
 * never reads "0"; one too small for those reads "<0.001". */
export function formatCost(value: number | null): string {
  if (value === null || !Number.isFinite(value)) return "—";
  const shown = formatDecimal(value, Math.abs(value) < 1 ? 3 : 0);
  return value !== 0 && shown === formatDecimal(0) ? `${value < 0 ? "-" : ""}<0.001` : shown;
}

/** A model's line of the cost breakdown, its counts compact (t505u P4):
 * "6.1K tokens · 2.0K per landing"; under 1,000 per landing as
 * [`formatCost`] says it ("0.333"), "—" below the minimum. The exact
 * figures are in the line's title (scorecard.ts). */
export function modelCostText(tokens: number, perLanded: number | null): string {
  const per = perLanded !== null && Number.isFinite(perLanded) && Math.abs(perLanded) >= 1000 ? compactCount(perLanded) : formatCost(perLanded);
  return `${compactCount(tokens)} tokens · ${per} per landing`;
}

/** A gate time's spending in words: whole minutes under an hour ("35m"),
 * hours to one decimal from one ("1.1 h", "10.0 h"). */
function gateSpan(ms: number): string {
  const minutes = ms / 60_000;
  if (Math.round(minutes) < 60) return `${Math.round(minutes)}m`;
  return `${(minutes / 60).toFixed(1)} h`;
}

/** What `undecidedText` reads of a scope's `gate_time`. */
export interface GateSpending {
  total_ms?: unknown;
  undecided_ms?: unknown;
}

/** How much of a scope's gate time decided nothing (objective 067): "35m
 * of 1.1 h gate time decided nothing (54%)", the percent whole. Only judged
 * runs count, each by its record's gate time: a run whose verdict decided
 * nothing (aborted, no_room, host_fault or stale). Null with no gate time. */
export function undecidedText(gate: GateSpending | null | undefined): string | null {
  const total = typeof gate?.total_ms === "number" && Number.isFinite(gate.total_ms) ? gate.total_ms : 0;
  if (total <= 0) return null;
  const raw = typeof gate?.undecided_ms === "number" && Number.isFinite(gate.undecided_ms) ? gate.undecided_ms : 0;
  const undecided = Math.min(Math.max(raw, 0), total);
  const percent = Math.round((undecided / total) * 100);
  return `${gateSpan(undecided)} of ${gateSpan(total)} gate time decided nothing (${percent}%)`;
}

/** The line a dimension's tile shows under its value, for the fleet and
 * each mission alike: gate time's `undecidedText`; null for any other
 * dimension, or with no gate time. */
export function tileNote(key: string, dimension: GateSpending | null | undefined): string | null {
  if (key === "gate_time") return undecidedText(dimension);
  if (key === "report_to_live") return reportToLiveText(dimension as ReportToLiveFields | null | undefined);
  return null;
}

/** What `reportToLiveText` reads of a scope's `report_to_live`. */
export interface ReportToLiveFields {
  value?: unknown;
  n?: unknown;
  not_live?: unknown;
}

/** The performance north star in words (objective 081): "1.0 h from report
 * to live (4 landings; 1 not live yet)", the median in hours to one
 * decimal, the live landings' count, and the ones not live yet when there
 * are any; null when the dimension or its value is missing. */
export function reportToLiveText(dimension: ReportToLiveFields | null | undefined): string | null {
  const value = dimension?.value;
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  const n = typeof dimension?.n === "number" && Number.isFinite(dimension.n) ? dimension.n : 0;
  const notLive = typeof dimension?.not_live === "number" && Number.isFinite(dimension.not_live) ? dimension.not_live : 0;
  const landings = `${formatDecimal(n, 0)} ${n === 1 ? "landing" : "landings"}`;
  const waiting = notLive > 0 ? `; ${formatDecimal(notLive, 0)} not live yet` : "";
  return `${(value / 3_600_000).toFixed(1)} h from report to live (${landings}${waiting})`;
}

/** From when the owner's judgings are counted, in words (objective 076):
 * an owner judging counts in gate time only when its record names the
 * gate's wall clock, which the integrator has written only since a deploy.
 * "Owner judgings counted from <time> only: earlier ones carry no gate
 * time." when the window starts before `owner_runs_from_ms`; "Owner
 * judgings not counted yet: none carries a gate time." when it is null;
 * null when the window starts at or after it. */
export function ownerRunsNote(since_ms: number | null, owner_runs_from_ms: number | null): string | null {
  if (owner_runs_from_ms === null || !Number.isFinite(owner_runs_from_ms)) {
    return "Owner judgings not counted yet: none carries a gate time.";
  }
  if (since_ms !== null && Number.isFinite(since_ms) && since_ms >= owner_runs_from_ms) return null;
  return `Owner judgings counted from ${localDateTime(owner_runs_from_ms)} only: earlier ones carry no gate time.`;
}

/** The owner-runs note a dimension's tile shows, the fleet's and each
 * mission's alike: gate time's `ownerRunsNote`; null for any other
 * dimension, and for a server that does not say (`undefined`). */
export function ownerRunsTileNote(
  key: string,
  since_ms: number | null,
  owner_runs_from_ms: number | null | undefined,
): string | null {
  if (key !== "gate_time" || owner_runs_from_ms === undefined) return null;
  return ownerRunsNote(since_ms, owner_runs_from_ms);
}

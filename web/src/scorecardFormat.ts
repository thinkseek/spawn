// The scorecard's number formats. Pure, with no import, so node can test
// them (web/test/scorecardFormat.test.ts); scorecard.ts, which also
// fetches, cannot be loaded there.

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

/** A model's line of the cost breakdown: "6,100 tokens · 2,033 per
 * landing", "—" per landing below the minimum. */
export function modelCostText(tokens: number, perLanded: number | null): string {
  return `${formatDecimal(tokens, 0)} tokens · ${formatCost(perLanded)} per landing`;
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
  return key === "gate_time" ? undecidedText(dimension) : null;
}

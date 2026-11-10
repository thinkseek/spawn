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

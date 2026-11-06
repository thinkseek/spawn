import type { Phase } from "../../missionTimeline";

/** Phase tones. They reuse the trace's slots so the two time views read as
 * one product: work is blue (as an execution), being judged is orange (as a
 * turn), landing is aqua (as a tool call), waiting is the neutral grey.
 * Review and gate are both "being judged", so they share a tone; their
 * labels tell them apart. Sent back is not a colour but a hatch. */
export const PHASE_COLOR: Record<Phase, string> = {
  queued: "var(--trace-command)",
  working: "var(--trace-execution)",
  review: "var(--trace-turn)",
  gate: "var(--trace-turn)",
  landing: "var(--trace-tool)",
  revise: "var(--warning)",
};

/** The legend: one entry per tone. */
export const PHASE_LEGEND: ReadonlyArray<{ label: string; color: string; hatch?: boolean }> = [
  { label: "assigned", color: PHASE_COLOR.queued },
  { label: "working", color: PHASE_COLOR.working },
  { label: "review · gated", color: PHASE_COLOR.gate },
  { label: "landing", color: PHASE_COLOR.landing },
  { label: "sent back", color: PHASE_COLOR.revise, hatch: true },
];

/** Verdicts are states, so they wear the status colours, never a phase's. */
export function verdictColor(verdict: string): string {
  switch (verdict) {
    case "accepted":
      return "var(--success)";
    case "rejected":
    case "aborted":
      return "var(--destructive)";
    case "conflict":
    case "incompatible":
      return "var(--warning)";
    default:
      return "var(--muted-foreground)";
  }
}

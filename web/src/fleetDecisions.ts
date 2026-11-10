// How the fleet decides, for the scorecard (design-ui-information §2.8: the
// two health cards moved from home). One window's figures in one shape, so
// a longer window (objective 043: 7 d and 30 d from the experience views)
// drops into the same cards: today it is read from /api/fleet's `health`,
// the last hour's judgements and the last 24 h's decision ladder.
// Pure, with no runtime import, so node can test it.

/** The ladder's steps, cheapest first, then the liveness restarts. */
export const LADDER_STEPS = ["rules", "model", "dispatcher", "supervisor", "human"] as const;
export type LadderStepId = (typeof LADDER_STEPS)[number] | "liveness";

/** One window's judgements: who judged, and how. */
export interface JudgementFigures {
  /** The window in words: "last hour". */
  window: string;
  total: number;
  rules: number;
  jev: number;
  other: number;
  /** Jev's part of the total, 0..1; null with no judgement. */
  jevShare: number | null;
  /** Judgements where Jev was not reached and the rules' guess stood. */
  unavailable: number;
  /** Jev's state differed from the rules' guess. */
  disagreed: number;
  jevP50Ms: number | null;
}

/** One window's decision ladder: who decided, and how often a person had to. */
export interface LadderFigures {
  /** The window in words: "24 h". */
  window: string;
  counts: Record<LadderStepId, number>;
  /** Tiers the server reports beyond the known ones, so none are hidden. */
  extra: Array<[string, number]>;
  total: number;
  humanPerAgentHour: number | null;
  agentHours: number | null;
  /** The last hour's tool calls and failures (rates the ladder card shows). */
  toolCallsPerHour: number | null;
  failuresPerHour: number | null;
}

/** What the scorecard's "How the fleet decides" section shows. A longer
 * window fills the same two parts; figures it adds (landed per day, idle
 * hours per mission) go beside them as optional fields. */
export interface DecisionWindow {
  judgements: JudgementFigures;
  ladder: LadderFigures;
}

/** What these rules read of /api/fleet's `health` (fleet.ts FleetHealth). */
export interface HealthLike {
  last_hour: {
    judgements: number;
    by_judge: Record<string, number>;
    judge_unavailable: number;
    model_latency_p50_ms: number | null;
    disagreements: number;
    tool_calls: number;
    failures: number;
  };
  ladder_24h: Record<string, number>;
  ladder_extra: Array<[string, number]>;
  agent_hours_24h: number | null;
  human_per_agent_hour: number | null;
}

/** Today's figures, from the fleet's health. */
export function decisionsFromHealth(health: HealthLike): DecisionWindow {
  const hour = health.last_hour;
  const jev = hour.by_judge.jev ?? 0;
  const rules = hour.by_judge.rules ?? 0;
  const counted = Object.values(hour.by_judge).reduce((sum, n) => sum + n, 0);
  const total = Math.max(hour.judgements, counted);
  const counts = {} as Record<LadderStepId, number>;
  for (const step of [...LADDER_STEPS, "liveness"] as const) counts[step] = health.ladder_24h[step] ?? 0;
  return {
    judgements: {
      window: "last hour",
      total,
      rules,
      jev,
      other: Math.max(0, total - jev - rules),
      jevShare: total > 0 ? jev / total : null,
      unavailable: hour.judge_unavailable,
      disagreed: hour.disagreements,
      jevP50Ms: hour.model_latency_p50_ms,
    },
    ladder: {
      window: "24 h",
      counts,
      extra: health.ladder_extra,
      total: Object.values(counts).reduce((sum, n) => sum + n, 0),
      humanPerAgentHour: health.human_per_agent_hour,
      agentHours: health.agent_hours_24h,
      toolCallsPerHour: hour.tool_calls,
      failuresPerHour: hour.failures,
    },
  };
}

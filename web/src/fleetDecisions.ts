// How the fleet decides, for the scorecard (design-ui-information §2.8: the
// two health cards moved from home). One window's figures in one shape:
// the page reads the Scorecard's window from /api/scorecard/fleet (objective
// 043: the experience views over 24 h, 7 d, 30 d or 90 d; decisionsFromApi),
// and home's health (/api/fleet: the last hour's judgements and the last
// 24 h's ladder; decisionsFromHealth) fills the same cards.
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

/** One mission's idle time over the window: its open span (from its
 * `mission.added` to its close, clipped) less the hours a task of it was in
 * flight. Idle when that is more than half the span, unless the mission is
 * owner work (no objective backlog: its work comes from the owner, so an hour
 * without a task there is no loss), which is never idle. */
export interface MissionIdle {
  mission: number;
  text: string;
  hours: number;
  idleHours: number;
  /** Hours the owner had it paused (objective 099): not idle, and a mission
   * paused at the window's end is never flagged idle. 0 from an older
   * server. */
  pausedHours: number;
  idle: boolean;
  ownerWork: boolean;
}

/** The window's interventions (design §15 autonomy (a)-(d)), by kind. */
export interface InterventionFigures {
  total: number;
  byKind: Record<string, number>;
}

/** What the scorecard's "How the fleet decides" section shows. A longer
 * window fills the same two parts; figures it adds (landed per day,
 * escalations, interventions, idle hours per mission) go beside them as
 * optional fields. */
export interface DecisionWindow {
  judgements: JudgementFigures;
  ladder: LadderFigures;
  /** Tasks landed per day (landed, or accepted by review without a gate). */
  landedPerDay?: number | null;
  /** Distinct escalations the dispatcher sent into the supervisor. */
  escalations?: number;
  interventions?: InterventionFigures;
  idleHoursByMission?: MissionIdle[];
}

/** The windows /api/scorecard/fleet answers, in words. */
const WINDOW_WORDS: Record<string, string> = { "24h": "24 h", "7d": "7 d", "30d": "30 d", "90d": "90 d" };

/** A window's name in words: "7d" is "7 d". */
export function windowWords(window: string): string {
  return WINDOW_WORDS[window] ?? window;
}

function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function num(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function maybeNum(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/** A window's figures, from /api/scorecard/fleet's answer (read leniently:
 * a missing figure counts 0, a missing ratio is null). */
export function decisionsFromApi(answer: unknown): DecisionWindow {
  const raw = record(answer);
  const words = windowWords(typeof raw.window === "string" ? raw.window : "");
  const ladderRaw = record(raw.ladder);
  const counts = {} as Record<LadderStepId, number>;
  for (const step of [...LADDER_STEPS, "liveness"] as const) counts[step] = num(ladderRaw[step]);
  const extra: Array<[string, number]> = Array.isArray(raw.ladder_extra)
    ? raw.ladder_extra
        .filter((entry): entry is [string, number] => Array.isArray(entry) && typeof entry[0] === "string")
        .map(([tier, n]) => [tier, num(n)])
    : [];
  const judged = record(raw.judgements);
  const byJudge = record(judged.by_judge);
  const jev = num(byJudge.jev);
  const rules = num(byJudge.rules);
  const counted = Object.values(byJudge).reduce<number>((sum, n) => sum + num(n), 0);
  const total = Math.max(num(judged.total), counted);
  const interventions = record(raw.interventions);
  const byKind: Record<string, number> = {};
  for (const [kind, n] of Object.entries(record(interventions.by_kind))) byKind[kind] = num(n);
  const missions = Array.isArray(raw.missions) ? raw.missions : [];
  return {
    judgements: {
      window: words,
      total,
      rules,
      jev,
      other: Math.max(0, total - jev - rules),
      jevShare: total > 0 ? jev / total : null,
      unavailable: num(judged.judge_unavailable),
      disagreed: num(judged.disagreements),
      jevP50Ms: maybeNum(judged.model_latency_p50_ms),
    },
    ladder: {
      window: words,
      counts,
      extra,
      total: Object.values(counts).reduce((sum, n) => sum + n, 0),
      humanPerAgentHour: maybeNum(raw.human_per_agent_hour),
      agentHours: maybeNum(raw.agent_hours),
      toolCallsPerHour: null,
      failuresPerHour: null,
    },
    landedPerDay: maybeNum(raw.landed_per_day),
    escalations: num(raw.escalations),
    interventions: { total: num(interventions.total), byKind },
    idleHoursByMission: missions.map((entry) => {
      const mission = record(entry);
      return {
        mission: num(mission.mission),
        text: typeof mission.text === "string" ? mission.text : "",
        hours: num(mission.hours),
        idleHours: num(mission.idle_hours),
        pausedHours: num(mission.paused_hours),
        idle: mission.idle === true && mission.owner_work !== true,
        ownerWork: mission.owner_work === true,
      };
    }),
  };
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

// How the fleet decides, for the scorecard (design-ui-information §2.8: the
// two health cards moved from home). One window's figures in one shape:
// the Scorecard's window (24 h, 7 d, 30 d, 90 d) reads them from
// `GET /api/scorecard/fleet` (`decisionsFromApi`), counted over the
// experience views; /api/fleet's `health` gives the last hour's judgements
// and the last 24 h's ladder (`decisionsFromHealth`).
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

/** One mission's idle hours over a window: its hours open in the window,
 * the part of them no task of it was open, and whether that is more than
 * half (`idle`). */
export interface MissionIdle {
  mission: number;
  text: string;
  hours: number;
  idleHours: number;
  idle: boolean;
}

/** The owner's interventions over a window (design §15's autonomy). */
export interface InterventionFigures {
  total: number;
  byKind: { command: number; resume: number; owner_task: number; cancel_reassign: number };
}

/** What the scorecard's "How the fleet decides" section shows. A window
 * read from /api/scorecard/fleet also fills the optional figures beside the
 * two cards; the health's 24 h has none of them. */
export interface DecisionWindow {
  judgements: JudgementFigures;
  ladder: LadderFigures;
  /** Landings (landed, or accepted by review without a gate) per day. */
  landedPerDay?: number | null;
  /** Distinct escalations the dispatcher sent the supervisor. */
  escalations?: number;
  interventions?: InterventionFigures;
  /** Each mission open in the window, by id. */
  idleHoursByMission?: MissionIdle[];
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

/** The windows /api/scorecard/fleet answers, in words. */
const WINDOW_WORDS: Record<string, string> = { "24h": "24 h", "7d": "7 d", "30d": "30 d", "90d": "90 d" };

/** A window's name in words: "7d" is "7 d". */
export function windowWords(window: string): string {
  return WINDOW_WORDS[window] ?? window;
}

type Raw = Record<string, unknown>;

function record(value: unknown): Raw {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Raw) : {};
}

function optionalCount(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function count(value: unknown): number {
  return optionalCount(value) ?? 0;
}

/** One window's figures, from /api/scorecard/fleet's answer (serve.rs
 * `scorecard_fleet`): the ladder and the judgements labelled with the
 * window in words, and the figures only a window answer has. A field the
 * answer lacks counts 0 (null where the answer may say "none"). */
export function decisionsFromApi(answer: unknown): DecisionWindow {
  const raw = record(answer);
  const window = windowWords(typeof raw.window === "string" ? raw.window : "");
  const ladderRaw = record(raw.ladder);
  const counts = {} as Record<LadderStepId, number>;
  for (const step of [...LADDER_STEPS, "liveness"] as const) counts[step] = count(ladderRaw[step]);
  const extra: Array<[string, number]> = [];
  for (const entry of Array.isArray(raw.ladder_extra) ? raw.ladder_extra : []) {
    if (Array.isArray(entry) && typeof entry[0] === "string") extra.push([entry[0], count(entry[1])]);
  }
  const judged = record(raw.judgements);
  const byJudge = record(judged.by_judge);
  const jev = count(byJudge.jev);
  const rules = count(byJudge.rules);
  const counted = Object.values(byJudge).reduce((sum: number, n) => sum + count(n), 0);
  const total = Math.max(count(judged.total), counted);
  const interventions = record(raw.interventions);
  const byKind = record(interventions.by_kind);
  const missions = Array.isArray(raw.missions) ? raw.missions : [];
  return {
    judgements: {
      window,
      total,
      rules,
      jev,
      other: Math.max(0, total - jev - rules),
      jevShare: total > 0 ? jev / total : null,
      unavailable: count(judged.judge_unavailable),
      disagreed: count(judged.disagreements),
      jevP50Ms: optionalCount(judged.model_latency_p50_ms),
    },
    ladder: {
      window,
      counts,
      extra,
      total: Object.values(counts).reduce((sum, n) => sum + n, 0),
      humanPerAgentHour: optionalCount(raw.human_per_agent_hour),
      agentHours: optionalCount(raw.agent_hours),
      toolCallsPerHour: null,
      failuresPerHour: null,
    },
    landedPerDay: optionalCount(raw.landed_per_day),
    escalations: count(raw.escalations),
    interventions: {
      total: count(interventions.total),
      byKind: {
        command: count(byKind.command),
        resume: count(byKind.resume),
        owner_task: count(byKind.owner_task),
        cancel_reassign: count(byKind.cancel_reassign),
      },
    },
    idleHoursByMission: missions.map((entry) => {
      const mission = record(entry);
      return {
        mission: count(mission.mission),
        text: typeof mission.text === "string" ? mission.text : "",
        hours: count(mission.hours),
        idleHours: count(mission.idle_hours),
        idle: mission.idle === true,
      };
    }),
  };
}

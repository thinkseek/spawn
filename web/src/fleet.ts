// Read model of the fleet: `/api/fleet`, `/api/fleet/<id>/judgements`,
// `/api/fleet/<id>/commands` and `/api/pane/<id>`. Every field is normalized,
// so a server that omits one renders an empty value instead of crashing.

import type { SessionState } from "./api";
import { apiError, apiHeaders } from "./api";
import type { GateRoom, Memory } from "./resources";
import { normalizeGateRoom, normalizeMemory } from "./resources";
import type { RsiPane } from "./rsi";
import { normalizePane } from "./rsi";

export const FLEET_STATES = [
  "WORKING",
  "WAITING",
  "BLOCKED",
  "COMPLETED",
  "UNKNOWN",
] as const;
export type FleetState = (typeof FLEET_STATES)[number];
export type Role = "supervisor" | "worker" | "rsi";
export type JudgeKind = "rules" | "jev" | "other";
export type Probabilities = Record<FleetState, number>;

export interface Evidence {
  screen_tail: string | null;
  task: string | null;
  /** Every other scalar the judge saw, in server order, for a facts grid. */
  facts: Array<[string, string]>;
}

export interface Judgement {
  state: FleetState;
  confidence: number | null;
  judge: string;
  reason: string | null;
  based_on_seq: number | null;
  ambiguity: string | null;
  rules_state: FleetState | null;
  probabilities: Probabilities | null;
  needs_human: number | null;
  stuck: number | null;
  latency_ms: number | null;
  /** The judge's own opinion that someone should look, and why. Not who
   * needs a human: that is the dispatcher's view (attention.ts). */
  attention: boolean | null;
  attention_reason: string | null;
  evidence: Evidence | null;
  fingerprint: string | null;
}

export interface Activity {
  current_tool: string | null;
  last_event_name: string | null;
  last_event_at_ms: number | null;
  event_count: number | null;
  tool_calls: number | null;
  executions: number | null;
}

export interface FleetAgent {
  session_id: string;
  agent_id: string;
  role: Role;
  role_rank: number;
  engine: string | null;
  live: boolean;
  execution_id: string | null;
  started_at_ms: number | null;
  ended_at_ms: number | null;
  state: FleetState;
  state_since_ms: number | null;
  /** Needs a human, by the dispatcher's view (`/api/attention`, set by
   * attention.ts `markNeedsHuman`); false until that view is read. Never
   * the judge's flag, which is `judgement.attention`. */
  attention: boolean;
  judged_at_ms: number | null;
  task: string | null;
  activity: Activity;
  judgement: Judgement | null;
}

export const LADDER_STEPS = [
  "rules",
  "model",
  "dispatcher",
  "supervisor",
  "human",
] as const;
export type LadderStep = (typeof LADDER_STEPS)[number];

export interface FleetHealth {
  generated_at_ms: number | null;
  agents_total: number;
  live: number;
  by_state: Record<FleetState, number>;
  last_hour: {
    judgements: number;
    by_judge: Record<string, number>;
    judge_unavailable: number;
    model_latency_p50_ms: number | null;
    disagreements: number;
    tool_calls: number;
    failures: number;
  };
  ladder_24h: Record<LadderStep | "liveness", number>;
  /** Tiers the server reports beyond the known ones, so none are hidden. */
  ladder_extra: Array<[string, number]>;
  agent_hours_24h: number | null;
  human_per_agent_hour: number | null;
  /** Free disk on the paths the fleet writes to, read when the page loaded. */
  disk: DiskPlace[];
  /** Memory on this host, read with the disk; null when not reported. */
  memory: Memory | null;
  /** Each gated mission's waits for gate room; null when not reported. */
  gate_room: GateRoom[] | null;
}

export interface DiskPlace {
  /** `workspace` or `tmpdir`. */
  label: string;
  path: string;
  /** Null when the server could not read it. */
  free_bytes: number | null;
  /** Under this the loop records a warning. */
  warn_bytes: number;
  /** Under this no new execution starts. */
  floor_bytes: number;
}

export interface Fleet {
  agents: FleetAgent[];
  health: FleetHealth;
}

export interface JudgementRecord {
  session_seq: number | null;
  recorded_at_ms: number | null;
  observation: Judgement;
}

export interface CommandRecord {
  command_id: number | null;
  principal: string;
  action: string;
  text: string | null;
  state: string | null;
  succeeded: boolean | null;
  error: string | null;
  created_at_ms: number | null;
  finished_at_ms: number | null;
}

type JsonObject = Record<string, unknown>;

function isObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function object(value: unknown): JsonObject {
  return isObject(value) ? value : {};
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function text(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function optionalText(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function number(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function optionalBool(value: unknown): boolean | null {
  return typeof value === "boolean" ? value : null;
}

export function fleetState(value: unknown): FleetState {
  if (typeof value !== "string") return "UNKNOWN";
  const key = value.toUpperCase();
  return (FLEET_STATES as readonly string[]).includes(key)
    ? (key as FleetState)
    : "UNKNOWN";
}

function optionalState(value: unknown): FleetState | null {
  return typeof value === "string" && value ? fleetState(value) : null;
}

/** The lowercase state the shared StateDot/StateBadge components take. */
export function sessionState(state: FleetState): SessionState {
  return state.toLowerCase() as SessionState;
}

function role(value: unknown): Role {
  return value === "supervisor" || value === "rsi" ? value : "worker";
}

const ROLE_RANK: Record<Role, number> = { supervisor: 0, worker: 1, rsi: 2 };

export function judgeKind(judge: string | null | undefined): JudgeKind {
  const key = (judge ?? "").toLowerCase();
  if (key === "rules" || key === "jev") return key;
  return "other";
}

function probabilities(value: unknown): Probabilities | null {
  if (!isObject(value)) return null;
  const out = {} as Probabilities;
  let any = false;
  for (const state of FLEET_STATES) {
    const raw = optionalNumber(value[state] ?? value[state.toLowerCase()]);
    out[state] = raw ?? 0;
    if (raw !== null) any = true;
  }
  return any ? out : null;
}

// Shown elsewhere in the panel, or identity fields that repeat the header.
const EVIDENCE_HIDDEN = new Set([
  "screen_tail",
  "screen",
  "task",
  "agent_id",
  "session_id",
  "engine",
  "ambiguity",
]);

function factValue(value: unknown): string | null {
  if (value === null || value === undefined) return null;
  if (typeof value === "string") return value === "" ? null : value;
  if (typeof value === "number") {
    return Number.isFinite(value) ? String(value) : null;
  }
  if (typeof value === "boolean") return value ? "yes" : "no";
  try {
    return JSON.stringify(value);
  } catch {
    return null;
  }
}

function evidence(value: unknown): Evidence | null {
  if (!isObject(value)) return null;
  const facts: Array<[string, string]> = [];
  for (const [key, raw] of Object.entries(value)) {
    if (EVIDENCE_HIDDEN.has(key)) continue;
    const shown = factValue(raw);
    if (shown !== null) facts.push([key, shown]);
  }
  return {
    screen_tail: optionalText(value.screen_tail) ?? optionalText(value.screen),
    task: optionalText(value.task),
    facts,
  };
}

export function normalizeJudgement(value: unknown): Judgement | null {
  if (!isObject(value)) return null;
  return {
    state: fleetState(value.state),
    confidence: optionalNumber(value.confidence),
    judge: text(value.judge, "unknown"),
    reason: optionalText(value.reason),
    based_on_seq: optionalNumber(value.based_on_seq),
    ambiguity: optionalText(value.ambiguity),
    rules_state: optionalState(value.rules_state),
    probabilities: probabilities(value.probabilities),
    needs_human: optionalNumber(value.needs_human),
    stuck: optionalNumber(value.stuck),
    latency_ms: optionalNumber(value.latency_ms),
    attention: optionalBool(value.attention),
    attention_reason: optionalText(value.attention_reason),
    evidence: evidence(value.evidence),
    fingerprint: optionalText(value.fingerprint),
  };
}

function normalizeAgent(value: unknown): FleetAgent | null {
  const raw = object(value);
  const sessionId = text(raw.session_id);
  if (!sessionId) return null;
  const activity = object(raw.activity);
  const agentRole = role(raw.role);
  const judgement = normalizeJudgement(raw.judgement);
  return {
    session_id: sessionId,
    agent_id: text(raw.agent_id),
    role: agentRole,
    role_rank: number(raw.role_rank, ROLE_RANK[agentRole]),
    engine: optionalText(raw.engine),
    live: raw.live === true,
    execution_id: optionalText(raw.execution_id),
    started_at_ms: optionalNumber(raw.started_at_ms),
    ended_at_ms: optionalNumber(raw.ended_at_ms),
    state: fleetState(raw.state ?? judgement?.state),
    state_since_ms: optionalNumber(raw.state_since_ms),
    // The server's `attention` is the judge's flag; who needs a human comes
    // from /api/attention (attention.ts `markNeedsHuman`).
    attention: false,
    judged_at_ms: optionalNumber(raw.judged_at_ms),
    task: optionalText(raw.task),
    activity: {
      current_tool: optionalText(activity.current_tool),
      last_event_name: optionalText(activity.last_event_name),
      last_event_at_ms: optionalNumber(activity.last_event_at_ms),
      event_count: optionalNumber(activity.event_count),
      tool_calls: optionalNumber(activity.tool_calls),
      executions: optionalNumber(activity.executions),
    },
    judgement,
  };
}

function countByState(agents: FleetAgent[]): Record<FleetState, number> {
  const counts = Object.fromEntries(
    FLEET_STATES.map((state) => [state, 0]),
  ) as Record<FleetState, number>;
  for (const agent of agents) counts[agent.state] += 1;
  return counts;
}

function normalizeHealth(value: unknown, agents: FleetAgent[]): FleetHealth {
  const health = object(value);
  const byStateRaw = object(health.by_state);
  const hasByState = Object.keys(byStateRaw).length > 0;
  const derived = countByState(agents);
  const byState = Object.fromEntries(
    FLEET_STATES.map((state) => [
      state,
      hasByState
        ? number(byStateRaw[state] ?? byStateRaw[state.toLowerCase()])
        : derived[state],
    ]),
  ) as Record<FleetState, number>;

  const lastHour = object(health.last_hour);
  const byJudge: Record<string, number> = {};
  for (const [judge, count] of Object.entries(object(lastHour.by_judge))) {
    const n = optionalNumber(count);
    if (n !== null) byJudge[judge] = n;
  }

  const ladderRaw = object(health.ladder_24h);
  const known = new Set<string>([...LADDER_STEPS, "liveness"]);
  const ladder = Object.fromEntries(
    [...LADDER_STEPS, "liveness"].map((step) => [
      step,
      number(ladderRaw[step]),
    ]),
  ) as Record<LadderStep | "liveness", number>;
  const ladderExtra = Object.entries(ladderRaw)
    .filter(([key, count]) => !known.has(key) && optionalNumber(count) !== null)
    .map(([key, count]) => [key, number(count)] as [string, number]);

  return {
    generated_at_ms: optionalNumber(health.generated_at_ms),
    agents_total: number(health.agents_total, agents.length),
    live: number(health.live, agents.filter((agent) => agent.live).length),
    by_state: byState,
    last_hour: {
      judgements: number(lastHour.judgements),
      by_judge: byJudge,
      judge_unavailable: number(lastHour.judge_unavailable),
      model_latency_p50_ms: optionalNumber(lastHour.model_latency_p50_ms),
      disagreements: number(lastHour.disagreements),
      tool_calls: number(lastHour.tool_calls),
      failures: number(lastHour.failures),
    },
    ladder_24h: ladder,
    ladder_extra: ladderExtra,
    agent_hours_24h: optionalNumber(health.agent_hours_24h),
    human_per_agent_hour: optionalNumber(health.human_per_agent_hour),
    disk: list(health.disk).map((raw) => {
      const place = object(raw);
      return {
        label: text(place.label),
        path: text(place.path),
        free_bytes: optionalNumber(place.free_bytes),
        warn_bytes: number(place.warn_bytes),
        floor_bytes: number(place.floor_bytes),
      };
    }),
    memory: normalizeMemory(health.memory),
    gate_room: normalizeGateRoom(health.gate_room),
  };
}

export function normalizeFleet(value: unknown): Fleet {
  const raw = object(value);
  const agents = list(raw.agents)
    .map(normalizeAgent)
    .filter((agent): agent is FleetAgent => agent !== null)
    .sort(
      (left, right) =>
        left.role_rank - right.role_rank ||
        left.agent_id.localeCompare(right.agent_id) ||
        left.session_id.localeCompare(right.session_id),
    );
  return { agents, health: normalizeHealth(raw.health, agents) };
}

export function normalizeJudgements(value: unknown): JudgementRecord[] {
  return list(value).flatMap((entry) => {
    const raw = object(entry);
    const observation = normalizeJudgement(raw.observation);
    if (!observation) return [];
    return [
      {
        session_seq: optionalNumber(raw.session_seq),
        recorded_at_ms: optionalNumber(raw.recorded_at_ms),
        observation,
      },
    ];
  });
}

export function normalizeCommands(value: unknown): CommandRecord[] {
  return list(value).map((entry) => {
    const raw = object(entry);
    return {
      command_id: optionalNumber(raw.command_id),
      principal: text(raw.principal, "unknown"),
      action: text(raw.action, "unknown"),
      text: optionalText(raw.text),
      state: optionalText(raw.state),
      succeeded: optionalBool(raw.succeeded),
      error: optionalText(raw.error),
      created_at_ms: optionalNumber(raw.created_at_ms),
      finished_at_ms: optionalNumber(raw.finished_at_ms),
    };
  });
}

const sessionPath = (id: string) => encodeURIComponent(id);

export async function fetchFleet(signal: AbortSignal): Promise<Fleet> {
  return readApi("/api/fleet", signal, normalizeFleet);
}

export async function fetchJudgements(
  sessionId: string,
  signal: AbortSignal,
  limit = 60,
): Promise<JudgementRecord[]> {
  return readApi(
    `/api/fleet/${sessionPath(sessionId)}/judgements?limit=${limit}`,
    signal,
    normalizeJudgements,
  );
}

export async function fetchCommands(
  sessionId: string,
  signal: AbortSignal,
  limit = 40,
): Promise<CommandRecord[]> {
  return readApi(
    `/api/fleet/${sessionPath(sessionId)}/commands?limit=${limit}`,
    signal,
    normalizeCommands,
  );
}

export async function fetchAgentPane(
  sessionId: string,
  signal: AbortSignal,
): Promise<RsiPane> {
  return readApi(`/api/pane/${sessionPath(sessionId)}`, signal, normalizePane);
}

// ---------------------------------------------------------------------------
// Meaning: what the observer's codes say in plain words.

/** States the facts still allow once the rules hand a case to Jev. */
export const ALLOWED_BY_AMBIGUITY: Record<string, FleetState[]> = {
  turn_stopped_idle: ["WAITING", "BLOCKED", "COMPLETED"],
  quiet_live: ["WORKING", "WAITING", "BLOCKED"],
  engine_error: ["WORKING", "WAITING", "BLOCKED"],
  exited_on_its_own: ["COMPLETED", "BLOCKED"],
  lost: ["UNKNOWN", "BLOCKED", "COMPLETED"],
};

const AMBIGUITY_TEXT: Record<string, string> = {
  turn_stopped_idle: "turn ended and the agent went idle",
  quiet_live: "engine alive but quiet",
  engine_error: "engine reported an error",
  exited_on_its_own: "engine exited on its own",
  lost: "engine lost without an exit report",
};

const AMBIGUITY_QUESTION: Record<string, string> = {
  turn_stopped_idle: "Is it waiting for input, blocked, or done?",
  quiet_live: "Still working, waiting, or stuck?",
  engine_error: "Will it recover, or is it waiting or blocked?",
  exited_on_its_own: "Did it finish, or fail and block?",
  lost: "Unknown, blocked, or actually done?",
};

export function allowedStates(ambiguity: string | null): FleetState[] | null {
  return ambiguity ? (ALLOWED_BY_AMBIGUITY[ambiguity] ?? null) : null;
}

function humanize(code: string): string {
  const words = code.replace(/[_-]+/g, " ").trim().toLowerCase();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

export function ambiguityText(ambiguity: string | null): string | null {
  if (!ambiguity) return null;
  const known = AMBIGUITY_TEXT[ambiguity];
  return known ? known.charAt(0).toUpperCase() + known.slice(1) : humanize(ambiguity);
}

export function ambiguityQuestion(ambiguity: string | null): string | null {
  return ambiguity ? (AMBIGUITY_QUESTION[ambiguity] ?? null) : null;
}

const REASON_TEXT: Record<string, string> = {
  JUDGE_UNAVAILABLE: "No judge answered (Jev unreachable or not configured), showing the rules' guess",
  WAITING_FOR_USER_INPUT: "Waiting for user input",
};

export function reasonText(reason: string | null): string | null {
  if (!reason) return null;
  return REASON_TEXT[reason] ?? humanize(reason);
}

export const ROLE_LABEL: Record<Role, string> = {
  supervisor: "Supervisor",
  worker: "Worker",
  rsi: "RSI",
};

export function stateLabelUpper(state: FleetState): string {
  return state.charAt(0) + state.slice(1).toLowerCase();
}

/** The latest line where the agent asked a person something. */
export function humanQuestion(screen: string | null | undefined): string | null {
  if (!screen) return null;
  const lines = screen.split("\n");
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const match = /HUMAN:\s*(.+)$/.exec(lines[index]);
    if (match) return match[1].trim();
  }
  return null;
}

/** The rules' guess when it differs from the final state; null when they agree. */
export function rulesDisagree(judgement: Judgement | null): FleetState | null {
  if (!judgement?.rules_state) return null;
  return judgement.rules_state !== judgement.state ? judgement.rules_state : null;
}

export function percent(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—";
  return `${Math.round(value * 100)}%`;
}

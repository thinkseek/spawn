// The Show view's model, kept pure so `node --test` checks it without a
// browser: which scene path each logged event travels, which compute nodes
// are on stage, and the few live numbers. The view (components/show/)
// only draws what this module decides. Node strips the types itself, so
// this file uses erasable syntax only and imports nothing.

/** One event of `/api/events`, as the server sends it. */
export interface LogEvent {
  id: number;
  agent_id: string;
  session_id: string;
  execution_id: string | null;
  source: string;
  event_name: string;
  detail: unknown;
  recorded_at_ms: number;
}

/** The fixed places of the scene. Agents are `agent:<agent_id>`. */
export const PLACES = [
  "supervisor",
  "gateway",
  "dispatcher",
  "bridge",
  "integrator",
  "gate",
  "log",
  "transcripts",
  "workspace",
  "sources",
  "github",
] as const;
export type Place = (typeof PLACES)[number];

/** A node key: a place, or an agent's node. */
export type NodeKey = Place | `agent:${string}`;

/** What a moving mark carries; each has its own colour and legend entry. */
export type Tone = "trace" | "commit" | "command" | "task" | "landing";

/** One visible consequence of an event. */
export type Effect =
  | { kind: "travel"; route: NodeKey[]; tone: Tone }
  | { kind: "spark"; node: NodeKey }
  | { kind: "verdict"; node: "gate"; verdict: string; accepted: boolean };

export type Role = "supervisor" | "resident" | "worker";

export const SUPERVISOR_AGENT = "agent_supervisor";
export const RESIDENT_PREFIX = "agent_resident_";
/** The residents the scene always shows, in their order on stage. */
export const STANDING_RESIDENTS = ["rsi", "supply"] as const;

export function agentNode(agentId: string): NodeKey {
  return `agent:${agentId}`;
}

export function roleOf(agentId: string): Role {
  if (agentId === SUPERVISOR_AGENT) return "supervisor";
  if (agentId.startsWith(RESIDENT_PREFIX)) return "resident";
  return "worker";
}

/** Short name of an agent for its label: `rsi`, `worker_a1`, `supervisor`. */
export function agentLabel(agentId: string): string {
  if (agentId === SUPERVISOR_AGENT) return "supervisor";
  if (agentId.startsWith(RESIDENT_PREFIX)) return agentId.slice(RESIDENT_PREFIX.length);
  return agentId.replace(/^agent_(worker_)?/, "");
}

/** The control record of a `control.recorded` event, or null. */
export function controlRecord(event: LogEvent): Record<string, unknown> | null {
  if (event.event_name !== "control.recorded") return null;
  const detail = event.detail;
  if (!isObject(detail) || !isObject(detail.record)) return null;
  return detail.record;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function text(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

/** The node an event comes from: the agent that did it, or the control
 * place that recorded it (the dispatcher hands tasks out, the integrator
 * gates and lands them, anything else is the supervisor's decision). */
export function emitter(event: LogEvent): NodeKey {
  const record = controlRecord(event);
  if (event.source !== "control" && record === null) return agentNode(event.agent_id);
  const type = record ? text(record.type) : null;
  if (type === "task.assigned" || type === "task.delivered") return "dispatcher";
  if (
    type === "task.gated" ||
    type === "task.landed" ||
    type === "task.aborted" ||
    type === "mission.halted"
  ) {
    return "integrator";
  }
  return "supervisor";
}

/**
 * What one event shows, in order. Every event is appended to the log, so
 * every event sends a faint mark to it; the events with a path of their own
 * add it:
 * - `artifact.committed`: the agent's work to the workspace store
 * - `command.accepted`: supervisor → gateway → agent
 * - `execution.started`: the Bridge starts the agent's engine
 * - `turn.stopped`: the turn's transcript to the archive
 * - `tool.*`: a spark on the agent
 * - `task.assigned`: dispatcher → the assigned agent
 * - `task.gated`: integrator → gate, and the verdict at the gate
 * - `task.landed`: workspace → mission source → GitHub
 */
export function effectsOf(event: LogEvent): Effect[] {
  const from = emitter(event);
  const effects: Effect[] = [{ kind: "travel", route: [from, "log"], tone: "trace" }];
  const agent = agentNode(event.agent_id);
  switch (event.event_name) {
    case "artifact.committed":
      effects.push({ kind: "travel", route: [agent, "workspace"], tone: "commit" });
      break;
    case "command.accepted":
      effects.push({ kind: "travel", route: ["supervisor", "gateway", agent], tone: "command" });
      break;
    case "execution.started":
      effects.push({ kind: "travel", route: ["bridge", agent], tone: "command" });
      break;
    case "turn.stopped":
      effects.push({ kind: "travel", route: [agent, "transcripts"], tone: "trace" });
      break;
    case "tool.started":
    case "tool.completed":
    case "tool.failed":
      effects.push({ kind: "spark", node: agent });
      break;
    case "control.recorded":
      effects.push(...controlEffects(controlRecord(event)));
      break;
  }
  return effects;
}

function controlEffects(record: Record<string, unknown> | null): Effect[] {
  if (record === null) return [];
  switch (text(record.type)) {
    case "task.assigned": {
      const agent = text(record.agent_id);
      return agent ? [{ kind: "travel", route: ["dispatcher", agentNode(agent)], tone: "task" }] : [];
    }
    case "task.gated": {
      const verdict = text(record.verdict) ?? "unknown";
      return [
        { kind: "travel", route: ["integrator", "gate"], tone: "task" },
        { kind: "verdict", node: "gate", verdict, accepted: verdict === "accepted" },
      ];
    }
    case "task.landed":
      return [{ kind: "travel", route: ["workspace", "sources", "github"], tone: "landing" }];
    default:
      return [];
  }
}

// --- Compute nodes on stage -------------------------------------------------

/** One agent on stage. Standing nodes (supervisor, residents) stay; a
 * worker comes with its first event and leaves `LINGER_MS` after its
 * execution ends. */
export interface StageAgent {
  key: NodeKey;
  agentId: string;
  role: Role;
  label: string;
  /** An execution is running: the node breathes. */
  running: boolean;
  executionId: string | null;
  /** Scene time the last execution ended, while not running. */
  endedAtMs: number | null;
  /** Place among the workers; -1 for standing nodes. */
  slot: number;
}

export interface Stage {
  agents: Record<string, StageAgent>;
}

/** How long a finished worker stays on stage, fading. */
export const LINGER_MS = 12_000;

/** The standing nodes, none running. */
export function emptyStage(): Stage {
  const agents: Record<string, StageAgent> = {};
  for (const agentId of [SUPERVISOR_AGENT, ...STANDING_RESIDENTS.map((name) => RESIDENT_PREFIX + name)]) {
    agents[agentId] = standing(agentId);
  }
  return { agents };
}

function standing(agentId: string): StageAgent {
  return {
    key: agentNode(agentId),
    agentId,
    role: roleOf(agentId),
    label: agentLabel(agentId),
    running: false,
    executionId: null,
    endedAtMs: null,
    slot: -1,
  };
}

/** The lowest worker slot no worker on stage holds. */
function freeSlot(stage: Stage): number {
  const taken = new Set(Object.values(stage.agents).map((agent) => agent.slot));
  let slot = 0;
  while (taken.has(slot)) slot += 1;
  return slot;
}

/** The agent's node, put on stage if it is not there yet. Control events
 * belong to no agent node. */
function withAgent(stage: Stage, agentId: string): Stage {
  if (stage.agents[agentId]) return stage;
  const agent = standing(agentId);
  if (agent.role === "worker") agent.slot = freeSlot(stage);
  return { agents: { ...stage.agents, [agentId]: agent } };
}

/** The stage after one event. Returns the same object when nothing on
 * stage changed, so React skips the render. */
export function applyEvent(stage: Stage, event: LogEvent): Stage {
  if (event.source === "control" || controlRecord(event) !== null) return stage;
  let next = withAgent(stage, event.agent_id);
  const agent = next.agents[event.agent_id];
  if (event.event_name === "execution.started") {
    next = put(next, { ...agent, running: true, executionId: event.execution_id, endedAtMs: null });
  } else if (event.event_name === "execution.ended") {
    if (agent.executionId === null || agent.executionId === event.execution_id) {
      next = put(next, { ...agent, running: false, executionId: null, endedAtMs: event.recorded_at_ms });
    }
  } else if (!agent.running && event.execution_id !== null && isEngineEvent(event)) {
    // An engine reporting without a start this read saw (the start fell
    // before the window): it is running.
    next = put(next, { ...agent, running: true, executionId: event.execution_id, endedAtMs: null });
  } else if (!agent.running && agent.role === "worker") {
    // A worker with no engine running is on its way out: it leaves
    // `LINGER_MS` after the last thing it did.
    next = put(next, { ...agent, endedAtMs: Math.max(agent.endedAtMs ?? 0, event.recorded_at_ms) });
  }
  return next;
}

/** Events only a running engine writes. */
function isEngineEvent(event: LogEvent): boolean {
  return event.event_name.startsWith("tool.") || event.event_name === "turn.stopped";
}

function put(stage: Stage, agent: StageAgent): Stage {
  return { agents: { ...stage.agents, [agent.agentId]: agent } };
}

/** The stage without the workers that finished more than `LINGER_MS` ago. */
export function prune(stage: Stage, nowMs: number): Stage {
  const gone = Object.values(stage.agents).filter(
    (agent) =>
      agent.role === "worker" &&
      !agent.running &&
      agent.endedAtMs !== null &&
      nowMs - agent.endedAtMs > LINGER_MS,
  );
  if (gone.length === 0) return stage;
  const agents = { ...stage.agents };
  for (const agent of gone) delete agents[agent.agentId];
  return { agents };
}

/** The stage as `/api/fleet` reports it now: its live agents running. */
export function stageFromFleet(
  fleet: { agent_id: string; live: boolean; execution_id: string | null }[],
): Stage {
  let stage = emptyStage();
  for (const row of fleet) {
    if (!row.live && roleOf(row.agent_id) === "worker") continue;
    stage = withAgent(stage, row.agent_id);
    if (row.live) {
      const agent = stage.agents[row.agent_id];
      stage = put(stage, { ...agent, running: true, executionId: row.execution_id });
    }
  }
  return stage;
}

export function runningCount(stage: Stage): number {
  return Object.values(stage.agents).filter((agent) => agent.running).length;
}

// --- Live numbers -----------------------------------------------------------

const MINUTE_MS = 60_000;

/** Events recorded in the minute up to `nowMs`. */
export function eventsPerMinute(events: readonly LogEvent[], nowMs: number): number {
  return events.filter((event) => event.recorded_at_ms > nowMs - MINUTE_MS && event.recorded_at_ms <= nowMs)
    .length;
}

/** Local midnight before `ms`. */
export function startOfDay(ms: number): number {
  const day = new Date(ms);
  day.setHours(0, 0, 0, 0);
  return day.getTime();
}

/** Tasks landed from local midnight up to `nowMs`. */
export function landedToday(events: readonly LogEvent[], nowMs: number): number {
  const from = startOfDay(nowMs);
  return events.filter(
    (event) =>
      event.recorded_at_ms >= from &&
      event.recorded_at_ms <= nowMs &&
      controlRecord(event)?.type === "task.landed",
  ).length;
}

/** Verdicts that judge the work. `stale` (main moved on), `aborted` (the
 * gate itself did not finish), `no_room` (TMPDIR had no room to start) and
 * `host_fault` (the host failed it: the test database) say nothing about
 * the candidate. */
const JUDGING = new Set(["accepted", "rejected", "conflict", "incompatible"]);

/** Share of judging gate verdicts that accepted, up to `nowMs`; null with
 * no verdict yet. */
export function gatePassRate(events: readonly LogEvent[], nowMs: number): number | null {
  let judged = 0;
  let accepted = 0;
  for (const event of events) {
    if (event.recorded_at_ms > nowMs) continue;
    const record = controlRecord(event);
    if (record?.type !== "task.gated" || typeof record.verdict !== "string") continue;
    if (!JUDGING.has(record.verdict)) continue;
    judged += 1;
    if (record.verdict === "accepted") accepted += 1;
  }
  return judged === 0 ? null : accepted / judged;
}

/** When each task last landed, up to `nowMs`, by task id. */
export function landings(events: readonly LogEvent[], nowMs: number): Map<number, number> {
  const landed = new Map<number, number>();
  for (const event of events) {
    if (event.recorded_at_ms > nowMs) continue;
    const record = controlRecord(event);
    if (record?.type !== "task.landed" || typeof record.id !== "number") continue;
    landed.set(record.id, Math.max(landed.get(record.id) ?? 0, event.recorded_at_ms));
  }
  return landed;
}

// --- Time -------------------------------------------------------------------

export type Mode = "live" | "replay";
export const REPLAY_SPEEDS = [10, 30, 60] as const;
export const REPLAY_HOURS = [1, 3, 6] as const;

/** Scene time of a replay `elapsedMs` of wall time after it started. */
export function replayTime(fromMs: number, elapsedMs: number, speed: number): number {
  return fromMs + elapsedMs * speed;
}

/** The events recorded in `(afterMs, untilMs]`, in log order. */
export function dueEvents(events: readonly LogEvent[], afterMs: number, untilMs: number): LogEvent[] {
  return events.filter((event) => event.recorded_at_ms > afterMs && event.recorded_at_ms <= untilMs);
}

/** The scene drops marks past this many in flight, oldest kept: a burst
 * thins out instead of piling up. */
export const MAX_MARKS = 160;

/** `marks` with `added` appended, at most `MAX_MARKS` of them: what does
 * not fit is dropped, trace marks first. */
export function admitMarks<T extends { tone: Tone }>(marks: readonly T[], added: readonly T[]): T[] {
  const room = MAX_MARKS - marks.length;
  if (added.length <= room) return [...marks, ...added];
  const meaningful = added.filter((mark) => mark.tone !== "trace");
  const traces = added.filter((mark) => mark.tone === "trace");
  const kept = [...meaningful, ...traces].slice(0, Math.max(room, 0));
  return [...marks, ...kept];
}

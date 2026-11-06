/* One ordering and grouping model for every list, so the Fleet board, the
 * sidebar's session list, the Missions timeline and cards, and the
 * scorecard agree. Pure (type-only imports), tested in
 * web/test/ordering.test.ts.
 *
 * Agents fall into four groups, in this order: always on (the supervisor,
 * then the residents), on a task, idle, ended. Missions into three classes:
 * active with open tasks, active without, closed. A mission's tasks into
 * four sections: in flight, landed or accepted, sent back, cancelled; the
 * retries of one objective are one group, its latest attempt the row. */

import type { FleetAgent } from "./fleet";
import type { Mission, MissionTask, Missions, TaskState } from "./missions";
import type { ScorecardScope } from "./scorecard";

// ---------------------------------------------------------------------------
// Times

/** When a task started: its first record, else its creation. */
export function taskStart(task: Pick<MissionTask, "stages" | "created_at_ms">): number {
  const first = task.stages.find((stage) => stage.at_ms !== null)?.at_ms;
  return first ?? task.created_at_ms ?? 0;
}

/** The latest moment anything happened to a task. */
export function taskActivity(
  task: Pick<MissionTask, "stages" | "created_at_ms" | "updated_at_ms">,
): number {
  let latest = Math.max(task.created_at_ms ?? 0, task.updated_at_ms ?? 0);
  for (const stage of task.stages) {
    if (stage.at_ms !== null && stage.at_ms > latest) latest = stage.at_ms;
  }
  return latest;
}

// ---------------------------------------------------------------------------
// Tasks: sections, titles, retries

export type TaskSection = "in_flight" | "landed" | "sent_back" | "cancelled";

/** Display order of a mission's sections. */
export const TASK_SECTIONS: readonly TaskSection[] = [
  "in_flight",
  "landed",
  "sent_back",
  "cancelled",
];

export const SECTION_LABEL: Record<TaskSection, string> = {
  in_flight: "In flight",
  landed: "Landed or accepted",
  sent_back: "Sent back",
  cancelled: "Cancelled",
};

type Gated = Pick<Mission, "gate" | "accept">;

/** Where a task's row goes. `accepted` in a mission with a gate waits for
 * the gate, so it is in flight; without a gate it is the end. A state this
 * build does not know stays in flight, on top, rather than hidden. */
export function taskSection(task: Pick<MissionTask, "state">, mission: Pick<Mission, "gate">): TaskSection {
  switch (task.state) {
    case "landed":
      return "landed";
    case "accepted":
      return mission.gate ? "in_flight" : "landed";
    case "revise":
      return "sent_back";
    case "cancelled":
      return "cancelled";
    default:
      return "in_flight";
  }
}

/** A task its worker still holds: in flight, or sent back to be redone. */
export function isOpenTask(task: Pick<MissionTask, "state">, mission: Pick<Mission, "gate">): boolean {
  const section = taskSection(task, mission);
  return section === "in_flight" || section === "sent_back";
}

function aborted(task: Pick<MissionTask, "stages">): boolean {
  return task.stages.some((stage) => stage.stage === "aborted");
}

/** The task's state in an observer's words: `gating` for a task that waits
 * for the gate, `aborted at budget` for the integrator's over-budget abort. */
export function taskStateLabel(task: Pick<MissionTask, "state" | "raw_state" | "stages">, mission: Gated): string {
  switch (task.state) {
    case "reported":
      return mission.gate && mission.accept === "gate" ? "gating" : "in review";
    case "accepted":
      return mission.gate ? "gating" : "accepted";
    case "revise":
      return "sent back";
    case "cancelled":
      return aborted(task) ? "aborted at budget" : "cancelled";
    case "unknown":
      return task.raw_state ?? "unknown";
    default:
      return task.state;
  }
}

/** The last verdict that sent the work back, if the last verdict did. */
function lastSendBack(task: Pick<MissionTask, "stages">): string | null {
  for (let index = task.stages.length - 1; index >= 0; index -= 1) {
    const stage = task.stages[index];
    if (stage.stage === "reviewed") return stage.verdict === "accepted" ? null : "rejected";
    if (stage.stage === "gated") {
      const verdict = stage.verdict ?? "";
      return verdict === "rejected" || verdict === "conflict" || verdict === "incompatible"
        ? verdict
        : null;
    }
  }
  return null;
}

/** How an earlier attempt of an objective ended, for its nested row. */
export function attemptOutcome(task: Pick<MissionTask, "state" | "raw_state" | "stages">, mission: Gated): string {
  if (aborted(task)) return "aborted at budget";
  const back = lastSendBack(task);
  switch (task.state) {
    case "cancelled":
      return back ? `${back}, then cancelled` : "cancelled";
    case "revise":
      return back ?? "sent back";
    default:
      return taskStateLabel(task, mission);
  }
}

const OBJECTIVE_LINE = /^objective\s+\d+\s*[:.—-]\s*(.+)$/i;
/** The integrator's stand-in when an objective has no title of its own. */
const PLACEHOLDER = /^task\s+\d+\.?$/i;

function clip(text: string, max: number): string {
  const flat = text.replace(/\s+/g, " ").trim();
  if (flat.length <= max) return flat;
  const cut = flat.slice(0, max - 1);
  const space = cut.lastIndexOf(" ");
  return `${(space > max * 0.6 ? cut.slice(0, space) : cut).replace(/[\s,;:.—-]+$/, "")}…`;
}

function capitalize(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/**
 * A task row's title: its objective's title when it has one (the
 * integrator's brief starts `Objective N: <title>`; a bare `Task N` is a
 * stand-in, not a title), otherwise the first line of its text, without a
 * lead-in that repeats the mission (`<mission name>:`, `Owner work on X
 * (…).`) or says `Goal:`, since the row already sits under the mission.
 * Truncated to `max` characters; null when the task has no text.
 */
export function taskTitle(
  task: Pick<MissionTask, "brief">,
  options: { objectiveTitle?: string | null; missionName?: string | null; max?: number } = {},
): string | null {
  const max = options.max ?? 120;
  const given = options.objectiveTitle?.trim();
  if (given && !PLACEHOLDER.test(given)) return clip(given, max);
  const lines = (task.brief ?? "")
    .split("\n")
    .map((line) => line.trim().replace(/^#+\s*/, ""))
    .filter((line) => line !== "");
  if (lines.length === 0) return null;
  const objective = OBJECTIVE_LINE.exec(lines[0]);
  if (objective) {
    const title = objective[1].trim();
    if (!PLACEHOLDER.test(title)) return clip(title, max);
    return lines[1] ? clip(lines[1], max) : clip(lines[0], max);
  }
  return clip(withoutLead(lines[0], options.missionName ?? null), max);
}

/** `Owner work on Spawn (protected files: …). ` and the like: who the work
 * is for, which the mission's lane already says. */
const OWNER_LEAD = /^owner[\w-]*\s+work\s+on\s+[^():.]+?\s*(?:\([^)]*\))?\s*[:.]\s+/i;
const GOAL_LEAD = /^goal:\s*/i;

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** A first line without the lead-in that repeats its mission (`<mission
 * name>:`, `Owner work on X (…).`) or announces the goal (`Goal:`). */
function withoutLead(line: string, missionName: string | null): string {
  let rest = line;
  const name = missionName?.trim();
  if (name) {
    const lead = new RegExp(`^${escapeRegExp(name)}\\s*(?:\\([^)]*\\))?\\s*[:.]\\s+`, "i");
    rest = rest.replace(lead, "");
  }
  rest = rest.replace(OWNER_LEAD, "").replace(GOAL_LEAD, "");
  return rest.trim() ? capitalize(rest.trim()) : line;
}

export interface Attempt<T> {
  item: T;
  task: MissionTask;
  outcome: string;
}

/** One row of a mission's list: a task, or the latest attempt at an
 * objective with the earlier attempts nested under it (newest first). */
export interface TaskGroup<T> {
  key: string;
  item: T;
  task: MissionTask;
  section: TaskSection;
  earlier: Attempt<T>[];
}

export interface SectionGroups<T> {
  section: TaskSection;
  groups: TaskGroup<T>[];
}

function byStart(left: MissionTask, right: MissionTask): number {
  return taskStart(left) - taskStart(right) || (left.id ?? 0) - (right.id ?? 0);
}

/**
 * A mission's tasks in sections (only the non-empty ones, in
 * TASK_SECTIONS order). Tasks that share an objective are one group whose
 * row is the latest attempt, or the latest open one when attempts overlap
 * (an open attempt is never nested under a closed one); the row's section
 * decides the group's. In flight
 * runs oldest first, so the longest-running is on top; the rest newest
 * first. `items` may be tasks or anything that carries one (a timeline bar).
 */
export function groupTasks<T>(
  items: readonly T[],
  taskOf: (item: T) => MissionTask,
  mission: Gated,
): SectionGroups<T>[] {
  const buckets = new Map<string, T[]>();
  items.forEach((item, index) => {
    const task = taskOf(item);
    const key =
      task.objective !== null ? `objective:${task.objective}` : `task:${task.id ?? `index-${index}`}`;
    const bucket = buckets.get(key);
    if (bucket) bucket.push(item);
    else buckets.set(key, [item]);
  });

  const sections = new Map<TaskSection, TaskGroup<T>[]>();
  for (const [key, bucket] of buckets) {
    const ordered = [...bucket].sort((left, right) => byStart(taskOf(left), taskOf(right)));
    const open = ordered.filter((entry) => isOpenTask(taskOf(entry), mission));
    const item = open.length > 0 ? open[open.length - 1] : ordered[ordered.length - 1];
    const task = taskOf(item);
    const section = taskSection(task, mission);
    const earlier = ordered
      .filter((entry) => entry !== item)
      .reverse()
      .map((entry) => ({
        item: entry,
        task: taskOf(entry),
        outcome: attemptOutcome(taskOf(entry), mission),
      }));
    const list = sections.get(section);
    const group = { key, item, task, section, earlier };
    if (list) list.push(group);
    else sections.set(section, [group]);
  }

  const out: SectionGroups<T>[] = [];
  for (const section of TASK_SECTIONS) {
    const groups = sections.get(section);
    if (!groups) continue;
    groups.sort((left, right) =>
      section === "in_flight"
        ? byStart(left.task, right.task)
        : taskActivity(right.task) - taskActivity(left.task) ||
          (right.task.id ?? 0) - (left.task.id ?? 0),
    );
    out.push({ section, groups });
  }
  return out;
}

// ---------------------------------------------------------------------------
// Missions

/** A mission's short name: its text up to a colon, else its first words. */
export function missionName(mission: Pick<Mission, "id" | "text">): string {
  const text = (mission.text ?? "").trim().replace(/\s+/g, " ");
  if (!text) return mission.id !== null ? `Mission ${mission.id}` : "Mission";
  const colon = text.indexOf(":");
  if (colon > 0 && colon <= 40) return text.slice(0, colon);
  const sentence = text.split(/(?<=[.!?])\s/)[0];
  if (sentence.length <= 48) return sentence.replace(/[.!?]$/, "");
  const cut = sentence.slice(0, 47);
  const space = cut.lastIndexOf(" ");
  return `${(space > 24 ? cut.slice(0, space) : cut).trimEnd()}…`;
}


/** `open`: active with tasks in flight or sent back; `idle`: active with
 * none; `closed`: done, cancelled or any other end. A halted mission is
 * active: it waits for a human, it has not ended. */
export type MissionClass = "open" | "idle" | "closed";

const CLASS_RANK: Record<MissionClass, number> = { open: 0, idle: 1, closed: 2 };

export function isActiveMission(mission: Pick<Mission, "state" | "halted">): boolean {
  if (mission.halted) return true;
  return mission.state === null || mission.state === "active" || mission.state === "open";
}

export function missionClass(mission: Pick<Mission, "state" | "halted" | "gate" | "tasks">): MissionClass {
  if (!isActiveMission(mission)) return "closed";
  return mission.tasks.some((task) => isOpenTask(task, mission)) ? "open" : "idle";
}

/** The latest recorded moment of a mission or any of its tasks. */
export function missionActivity(mission: Pick<Mission, "created_at_ms" | "updated_at_ms" | "tasks">): number {
  let latest = Math.max(mission.created_at_ms ?? 0, mission.updated_at_ms ?? 0);
  for (const task of mission.tasks) latest = Math.max(latest, taskActivity(task));
  return latest;
}

type Ordered = Pick<Mission, "id" | "state" | "halted" | "gate" | "tasks" | "created_at_ms" | "updated_at_ms">;

/** Open missions first, then idle active ones, then closed ones; within a
 * class, the most recent activity first. */
export function compareMissions(left: Ordered, right: Ordered): number {
  return (
    CLASS_RANK[missionClass(left)] - CLASS_RANK[missionClass(right)] ||
    missionActivity(right) - missionActivity(left) ||
    (right.id ?? 0) - (left.id ?? 0)
  );
}

export function orderMissions<T extends Ordered>(missions: readonly T[]): T[] {
  return [...missions].sort(compareMissions);
}

// ---------------------------------------------------------------------------
// Agents

export type AgentGroupId = "always_on" | "on_task" | "idle" | "ended";

export const AGENT_GROUPS: readonly AgentGroupId[] = ["always_on", "on_task", "idle", "ended"];

export const AGENT_GROUP_LABEL: Record<AgentGroupId, string> = {
  always_on: "Always on",
  on_task: "On a task",
  idle: "Idle",
  ended: "Ended",
};

/** A task as an agent row shows it. */
export interface TaskRef {
  mission_id: number | null;
  mission_name: string;
  task_id: number | null;
  state: TaskState;
  /** The state in an observer's words (`gating`, `sent back`, …). */
  label: string;
  title: string | null;
  open: boolean;
  /** The task's latest record. */
  at_ms: number;
}

export interface FleetContext {
  /** Whether /api/missions has answered: before it has, no agent is known
   * to hold a task. */
  known: boolean;
  /** Resident name by session id and by agent id; null before /api/missions
   * answers, when an `agent_resident_` id is taken as a resident's. */
  residents: Map<string, string> | null;
  /** The approved residents, which are expected to run. */
  approved: Array<{ name: string; agent_id: string | null; session_id: string | null }>;
  /** The open task each session (or agent) holds. */
  openTasks: Map<string, TaskRef>;
  /** The latest task of each session (or agent), open or not. */
  lastTasks: Map<string, TaskRef>;
}

/** Delivered work outranks a task still queued for delivery. */
function holdRank(task: TaskRef): number {
  return task.state === "assigned" ? 1 : 0;
}

export function fleetContext(missions: Missions | null): FleetContext {
  const openTasks = new Map<string, TaskRef>();
  const lastTasks = new Map<string, TaskRef>();
  if (!missions) return { known: false, residents: null, approved: [], openTasks, lastTasks };

  const residents = new Map<string, string>();
  const approved: FleetContext["approved"] = [];
  for (const resident of missions.residents) {
    if (resident.state === "retired") continue;
    const name = resident.name ?? resident.agent_id ?? resident.session_id ?? "resident";
    if (resident.session_id) residents.set(resident.session_id, name);
    if (resident.agent_id) residents.set(resident.agent_id, name);
    if (resident.state === "approved") {
      approved.push({ name, agent_id: resident.agent_id, session_id: resident.session_id });
    }
  }

  for (const mission of missions.missions) {
    const name = missionName(mission);
    for (const task of mission.tasks) {
      const ref: TaskRef = {
        mission_id: mission.id,
        mission_name: name,
        task_id: task.id,
        state: task.state,
        label: taskStateLabel(task, mission),
        title: taskTitle(task, { missionName: name }),
        open: isOpenTask(task, mission),
        at_ms: taskActivity(task),
      };
      for (const key of [task.session_id, task.agent_id]) {
        if (!key) continue;
        const last = lastTasks.get(key);
        if (!last || ref.at_ms > last.at_ms) lastTasks.set(key, ref);
        if (!ref.open) continue;
        const held = openTasks.get(key);
        if (
          !held ||
          holdRank(ref) < holdRank(held) ||
          (holdRank(ref) === holdRank(held) && ref.at_ms > held.at_ms)
        ) {
          openTasks.set(key, ref);
        }
      }
    }
  }
  return { known: true, residents, approved, openTasks, lastTasks };
}

/** An always-on agent that should run but has no session in the fleet: the
 * supervisor, or an approved resident never started (or gone). */
export interface MissingAgent {
  role: "supervisor" | "resident";
  name: string;
}

export function missingAlwaysOn(
  agents: ReadonlyArray<Pick<AgentFacts, "session_id" | "agent_id" | "role">>,
  context: FleetContext,
): MissingAgent[] {
  const missing: MissingAgent[] = [];
  if (!agents.some((agent) => agent.role === "supervisor")) {
    missing.push({ role: "supervisor", name: "supervisor" });
  }
  const present = new Set(agents.flatMap((agent) => [agent.session_id, agent.agent_id]));
  for (const resident of context.approved) {
    const ids = [resident.session_id, resident.agent_id].filter((id): id is string => id !== null);
    if (!ids.some((id) => present.has(id))) missing.push({ role: "resident", name: resident.name });
  }
  return missing;
}

export type AgentFacts = Pick<
  FleetAgent,
  "session_id" | "agent_id" | "role" | "live" | "attention" | "ended_at_ms" | "started_at_ms"
> & { activity: Pick<FleetAgent["activity"], "last_event_at_ms"> };

/** The resident an agent is, if it is one. */
export function residentName(agent: Pick<AgentFacts, "session_id" | "agent_id">, context: FleetContext): string | null {
  if (context.residents === null) {
    const match = /^agent_resident_(.+)$/.exec(agent.agent_id);
    return match ? match[1] : null;
  }
  return context.residents.get(agent.session_id) ?? context.residents.get(agent.agent_id) ?? null;
}

export function isAlwaysOn(agent: AgentFacts, context: FleetContext): boolean {
  return agent.role === "supervisor" || residentName(agent, context) !== null;
}

export function heldTask(agent: Pick<AgentFacts, "session_id" | "agent_id">, context: FleetContext): TaskRef | null {
  return context.openTasks.get(agent.session_id) ?? context.openTasks.get(agent.agent_id) ?? null;
}

export function lastTask(agent: Pick<AgentFacts, "session_id" | "agent_id">, context: FleetContext): TaskRef | null {
  return context.lastTasks.get(agent.session_id) ?? context.lastTasks.get(agent.agent_id) ?? null;
}

export interface AgentGroup<T> {
  id: AgentGroupId;
  agents: T[];
}

const lastEvent = (agent: AgentFacts) => agent.activity.last_event_at_ms ?? 0;

/** Attention first, then the stalest (longest since its last event). */
function attentionThenStalest(left: AgentFacts, right: AgentFacts): number {
  return (
    Number(right.attention) - Number(left.attention) ||
    lastEvent(left) - lastEvent(right) ||
    left.agent_id.localeCompare(right.agent_id)
  );
}

/**
 * Every agent in its group, all four groups in order (an empty one too):
 * - always on: the supervisor, then the residents (by name), live or not;
 * - on a task: workers holding an open task, attention first, then the
 *   stalest first. A worker whose engine ended while it holds an open task
 *   stays here: its task is stuck, which is what an observer must see;
 * - idle: live workers with no open task, in the same order;
 * - ended: the rest, the most recent end first.
 */
export function groupAgents<T extends AgentFacts>(agents: readonly T[], context: FleetContext): AgentGroup<T>[] {
  const groups: Record<AgentGroupId, T[]> = { always_on: [], on_task: [], idle: [], ended: [] };
  for (const agent of agents) {
    if (isAlwaysOn(agent, context)) groups.always_on.push(agent);
    else if (heldTask(agent, context)) groups.on_task.push(agent);
    else if (agent.live) groups.idle.push(agent);
    else groups.ended.push(agent);
  }
  const alwaysRank = (agent: T) => (agent.role === "supervisor" ? 0 : 1);
  groups.always_on.sort(
    (left, right) =>
      alwaysRank(left) - alwaysRank(right) ||
      Number(right.live) - Number(left.live) ||
      (residentName(left, context) ?? "").localeCompare(residentName(right, context) ?? "") ||
      (right.started_at_ms ?? 0) - (left.started_at_ms ?? 0) ||
      left.agent_id.localeCompare(right.agent_id),
  );
  groups.on_task.sort(attentionThenStalest);
  groups.idle.sort(attentionThenStalest);
  const ended = (agent: T) => agent.ended_at_ms ?? lastEvent(agent);
  groups.ended.sort(
    (left, right) => ended(right) - ended(left) || left.agent_id.localeCompare(right.agent_id),
  );
  return AGENT_GROUPS.map((id) => ({ id, agents: groups[id] }));
}

/** The engine most agents run on; a row names its engine only when it
 * differs. Null when no agent names one. */
export function usualEngine(agents: ReadonlyArray<Pick<FleetAgent, "engine">>): string | null {
  const counts = new Map<string, number>();
  for (const agent of agents) {
    if (agent.engine) counts.set(agent.engine, (counts.get(agent.engine) ?? 0) + 1);
  }
  let best: string | null = null;
  let most = 0;
  for (const [engine, count] of [...counts].sort(([left], [right]) => left.localeCompare(right))) {
    if (count > most) {
      best = engine;
      most = count;
    }
  }
  return best;
}

// ---------------------------------------------------------------------------
// Ids

/**
 * An id split for truncation in the middle: the head may lose its end to
 * an ellipsis, the tail always shows, so `agent_worker_…_96` keeps the
 * suffix that tells agents apart. The tail is the last `_` segment when it
 * is a number or at most four characters; otherwise there is none.
 */
export function splitId(id: string): { head: string; tail: string } {
  const at = id.lastIndexOf("_");
  if (at > 0 && at < id.length - 1) {
    const segment = id.slice(at + 1);
    if (/^\d+$/.test(segment) || segment.length <= 4) {
      return { head: id.slice(0, at), tail: id.slice(at) };
    }
  }
  return { head: id, tail: "" };
}

// ---------------------------------------------------------------------------
// Scorecard

/** A mission scope where nothing happened in the window: no landing, no
 * gate run, no send-back, intervention or reliability event, and none
 * landed in the previous window either (a drop to zero is news). One line,
 * not a grid of "not enough data". */
export function isQuietScope(scope: Pick<ScorecardScope, "scope" | "dimensions">): boolean {
  const d = scope.dimensions;
  return (
    scope.scope === "mission" &&
    d.throughput.n === 0 &&
    d.throughput.landed === 0 &&
    !((d.throughput.previous ?? 0) > 0) &&
    (d.gate_time?.n ?? 0) === 0 &&
    (d.gate_time?.runs ?? 0) === 0 &&
    (d.rework?.send_backs ?? 0) === 0 &&
    (d.autonomy?.interventions ?? 0) === 0 &&
    (d.reliability?.events ?? 0) === 0
  );
}

/** The fleet first, then missions in the missions' order (unknown ones
 * after, newest id first), the quiet ones last. */
export function orderScopes<T extends Pick<ScorecardScope, "scope" | "mission" | "dimensions">>(
  scopes: readonly T[],
  missions: readonly Ordered[],
): T[] {
  const rank = new Map<number, number>();
  orderMissions(missions).forEach((mission, index) => {
    if (mission.id !== null) rank.set(mission.id, index);
  });
  const position = (scope: T) =>
    scope.mission === null ? -1 : (rank.get(scope.mission) ?? missions.length + 1e6 - scope.mission);
  return [...scopes].sort(
    (left, right) =>
      Number(left.scope !== "fleet") - Number(right.scope !== "fleet") ||
      Number(isQuietScope(left)) - Number(isQuietScope(right)) ||
      position(left) - position(right),
  );
}

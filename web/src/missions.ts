// Read model of missions: `/api/missions`. Every field is normalized, so a
// server that omits one (or sends null) renders an empty value instead of
// crashing. Read-only: this module only ever issues GET requests.

import { readApi } from "./api";
import type { Labels } from "./label.ts";
import { labels } from "./label.ts";
import { normalizeActivity } from "./taskActivity.ts";
import type { TaskActivity } from "./taskActivity.ts";
import type { RsiSummary } from "./api";
import { normalizeHolds } from "./attention";
import type { SourceHold } from "./attention";

export type { SourceHold } from "./attention";

export const TASK_STATES = [
  "assigned",
  "delivered",
  "reported",
  "revise",
  "accepted",
  "cancelled",
  "landing",
  "landed",
] as const;
export type TaskState = (typeof TASK_STATES)[number] | "unknown";

/** The happy path a task walks; `revise` and `cancelled` branch off it. */
export const PIPELINE: ReadonlyArray<TaskState> = [
  "assigned",
  "delivered",
  "reported",
  "accepted",
];

/** The path of a task in a mission with a gate (docs/fleet-architecture.md
 * §10.3): after the supervisor's review (`accept` = review) or on the gate
 * alone (`gate`), the integrator lands it. */
export function pipelineOf(mission: Pick<Mission, "gate" | "accept">): ReadonlyArray<TaskState> {
  if (!mission.gate) return PIPELINE;
  return mission.accept === "gate"
    ? ["assigned", "delivered", "reported", "landing", "landed"]
    : ["assigned", "delivered", "reported", "accepted", "landing", "landed"];
}

/** The last `task.gated` record of a task. */
export interface LastGate {
  verdict: string;
  reason: string | null;
  metric: string | null;
  candidate: string | null;
  at_ms: number | null;
}

/** A task the integrator's load-sensitive bound parked (/api/missions'
 * task `parked`): its pushed commit was found load-sensitive twice, on
 * `scenarios`, and it is not judged again until a new commit is pushed or a
 * person releases it (`spawn task release`). */
export interface TaskParked {
  pushed: string;
  scenarios: string[];
}

/** One of a task's records in the control stream (`spawn task stages`). */
export interface TaskStage {
  /** The record type without `task.`: assigned, delivered, reported,
   * reviewed, gated, held (waits behind the owner's hold), landed, aborted
   * or cancelled. */
  stage: string;
  /** A gate's verdict, or a review's (`accepted`, `rejected`). */
  verdict: string | null;
  at_ms: number | null;
  /** The time since the task's previous record; null on the first. */
  took_ms: number | null;
}

/** A stage is slow when it took over ten minutes and over three times the
 * median of the task's other stages: a delivery that waited for an hour
 * while the rest took minutes stands out, a long piece of work among long
 * ones does not. */
export const SLOW_STAGE_MS = 10 * 60_000;

function median(values: number[]): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 1
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2;
}

export function slowStages(stages: ReadonlyArray<TaskStage>): boolean[] {
  return stages.map((stage, index) => {
    if (stage.took_ms === null || stage.took_ms <= SLOW_STAGE_MS) return false;
    const others = stages
      .filter((_, other) => other !== index)
      .map((other) => other.took_ms)
      .filter((ms): ms is number => ms !== null);
    return stage.took_ms > 3 * median(others);
  });
}

/** What a mission's tasks are budgeted, when it has a budget. */
export interface Budget {
  minutes: number | null;
  tool_calls: number | null;
}

export type ResidentState = "proposed" | "approved" | "retired" | "unknown";

export interface MissionTask extends Labels {
  id: number | null;
  kind: string | null;
  agent_id: string | null;
  session_id: string | null;
  brief: string | null;
  state: TaskState;
  /** The raw state string, for display when it is not one we know. */
  raw_state: string | null;
  revisions: number;
  review_note: string | null;
  report: string | null;
  /** Each text's whole length in bytes: the board sends a text's first
   * lines (`brief`, `review_note`, `report`), and of a folded task (a
   * closed mission's, a cancelled one) no report at all; the whole is
   * `fetchTaskTexts`. Null from an older server, which sends them whole. */
  brief_bytes?: number | null;
  review_note_bytes?: number | null;
  report_bytes?: number | null;
  /** The commit the integrator built for this task. */
  candidate: string | null;
  /** The backlog objective of an objective mission's task. */
  objective: number | null;
  /** The gate's metric line of the accepted candidate. */
  metric: string | null;
  last_gate: LastGate | null;
  /** What its worker did since a delivered task reached it (objective
   * 211); null for every other task, and from an older server. */
  activity: TaskActivity | null;
  /** Every record of the task, oldest first; empty when the server has none. */
  stages: TaskStage[];
  /** The models the worker runs on: "default", or "cheap" (all DeepSeek). */
  model: string | null;
  /** The owner's hold its landing last waited behind, if any. */
  held_by: number | null;
  /** Parked by the load-sensitive bound; null for any other task, and
   * absent from an older server. */
  parked?: TaskParked | null;
  created_by: string | null;
  created_at_ms: number | null;
  updated_at_ms: number | null;
}

/** The halt a mission is in (/api/missions' `halt`): its `mission.halted`
 * record's event (what a resume from the page names), the reason, when,
 * and for a halt on the scenario floor the scenario files gone since the
 * gating that set it (null for any other halt). */
export interface MissionHalt {
  event: number;
  reason: string | null;
  at_ms: number | null;
  gone: string[] | null;
}

/** The owner's pause a mission is in (/api/missions' `paused`, objective
 * 099): its `mission.paused` record's event, when, and the owner's reason.
 * While it lasts nothing is assigned or delivered to the mission and its
 * resident is stopped; `spawn mission resume` ends it. */
export interface MissionPause {
  at_ms: number | null;
  event_id: number;
  reason: string | null;
}

export interface Mission {
  id: number | null;
  text: string | null;
  /** Its name, from its text and source (the server's labels). */
  mission_name: string | null;
  origin: string | null;
  sources: string[];
  state: string | null;
  summary: string | null;
  /** Scenarios directory of the mission's gate; null without a gate. */
  gate: string | null;
  /** `review` (the supervisor accepts, then the gate runs) or `gate`. */
  accept: string | null;
  budget: Budget | null;
  /** Where the last landing left the source's main. */
  baseline: string | null;
  /** Why a halt rule fired, until a human resumes the mission. */
  halted: string | null;
  /** The halt it is in; null when it is not halted (or the server is older). */
  halt: MissionHalt | null;
  /** The owner's pause it is in; null when it is not paused (or the server
   * is older). Its `state` stays as it was. */
  paused: MissionPause | null;
  /** Objective missions: the backlog and the resident that works it. */
  objectives: { backlog: string | null; resident: string | null } | null;
  /** The owner's active hold on its source (its event id): no task lands
   * there while it lasts. */
  hold: number | null;
  created_at_ms: number | null;
  updated_at_ms: number | null;
  tasks: MissionTask[];
}

export interface Resident extends Labels {
  name: string | null;
  agent_id: string | null;
  session_id: string | null;
  charter: string | null;
  state: ResidentState;
  raw_state: string | null;
  proposed_by: string | null;
  approved_by: string | null;
  created_at_ms: number | null;
  updated_at_ms: number | null;
}

export interface Missions {
  missions: Mission[];
  residents: Resident[];
  /** Every active owner's hold, with the tasks waiting behind it. */
  holds: SourceHold[];
}

type JsonObject = Record<string, unknown>;

function isObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function optionalText(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function taskState(value: unknown): TaskState {
  if (typeof value !== "string") return "unknown";
  const key = value.toLowerCase();
  return (TASK_STATES as readonly string[]).includes(key)
    ? (key as TaskState)
    : "unknown";
}

function residentState(value: unknown): ResidentState {
  if (typeof value !== "string") return "unknown";
  const key = value.toLowerCase();
  return key === "proposed" || key === "approved" || key === "retired"
    ? key
    : "unknown";
}

function normalizeLastGate(value: unknown): LastGate | null {
  if (!isObject(value)) return null;
  return {
    verdict: optionalText(value.verdict) ?? "unknown",
    reason: optionalText(value.reason),
    metric: optionalText(value.metric),
    candidate: optionalText(value.candidate),
    at_ms: optionalNumber(value.at_ms),
  };
}

function normalizeParked(value: unknown): TaskParked | null {
  if (!isObject(value)) return null;
  const pushed = optionalText(value.pushed);
  if (pushed === null) return null;
  const scenarios = list(value.scenarios).filter((name): name is string => typeof name === "string");
  return { pushed, scenarios };
}

function normalizeStage(value: unknown): TaskStage | null {
  if (!isObject(value)) return null;
  const stage = optionalText(value.stage);
  if (stage === null) return null;
  return {
    stage,
    verdict: optionalText(value.verdict),
    at_ms: optionalNumber(value.at_ms),
    took_ms: optionalNumber(value.took_ms),
  };
}

function normalizeTask(value: unknown): MissionTask | null {
  if (!isObject(value)) return null;
  return {
    ...labels(value),
    id: optionalNumber(value.id),
    kind: optionalText(value.kind),
    agent_id: optionalText(value.agent_id),
    session_id: optionalText(value.session_id),
    brief: optionalText(value.brief),
    state: taskState(value.state),
    raw_state: optionalText(value.state),
    revisions: Math.max(0, optionalNumber(value.revisions) ?? 0),
    review_note: optionalText(value.review_note),
    report: optionalText(value.report),
    brief_bytes: optionalNumber(value.brief_bytes),
    review_note_bytes: optionalNumber(value.review_note_bytes),
    report_bytes: optionalNumber(value.report_bytes),
    candidate: optionalText(value.candidate),
    objective: optionalNumber(value.objective),
    metric: optionalText(value.metric),
    last_gate: normalizeLastGate(value.last_gate),
    activity: normalizeActivity(value.activity),
    stages: list(value.stages)
      .map(normalizeStage)
      .filter((stage): stage is TaskStage => stage !== null),
    model: optionalText(value.model),
    held_by: optionalNumber(value.held_by),
    parked: normalizeParked(value.parked),
    created_by: optionalText(value.created_by),
    created_at_ms: optionalNumber(value.created_at_ms),
    updated_at_ms: optionalNumber(value.updated_at_ms),
  };
}

export function normalizeHalt(value: unknown): MissionHalt | null {
  if (!isObject(value)) return null;
  const event = optionalNumber(value.event);
  if (event === null) return null;
  return {
    event,
    reason: optionalText(value.reason),
    at_ms: optionalNumber(value.at_ms),
    gone: Array.isArray(value.gone)
      ? value.gone.filter((name): name is string => typeof name === "string")
      : null,
  };
}

export function normalizePause(value: unknown): MissionPause | null {
  if (!isObject(value)) return null;
  const event_id = optionalNumber(value.event_id);
  if (event_id === null) return null;
  return { at_ms: optionalNumber(value.at_ms), event_id, reason: optionalText(value.reason) };
}

function normalizeMission(value: unknown): Mission | null {
  if (!isObject(value)) return null;
  const sources = list(value.sources)
    .map((source) => optionalText(source))
    .filter((source): source is string => source !== null);
  const tasks = list(value.tasks)
    .map(normalizeTask)
    .filter((task): task is MissionTask => task !== null)
    .sort(
      (left, right) =>
        (left.created_at_ms ?? 0) - (right.created_at_ms ?? 0) ||
        (left.id ?? 0) - (right.id ?? 0),
    );
  return {
    id: optionalNumber(value.id),
    text: optionalText(value.text),
    mission_name: labels(value).mission_name,
    origin: optionalText(value.origin),
    sources,
    state: optionalText(value.state),
    summary: optionalText(value.summary),
    gate: optionalText(value.gate),
    accept: optionalText(value.accept),
    budget: isObject(value.budget)
      ? {
          minutes: optionalNumber(value.budget.minutes),
          tool_calls: optionalNumber(value.budget.tool_calls),
        }
      : null,
    baseline: optionalText(value.baseline),
    halted: optionalText(value.halted),
    halt: optionalText(value.halted) === null ? null : normalizeHalt(value.halt),
    paused: normalizePause(value.paused),
    objectives: isObject(value.objectives)
      ? {
          backlog: optionalText(value.objectives.backlog),
          resident: optionalText(value.objectives.resident),
        }
      : null,
    hold: optionalNumber(value.hold),
    created_at_ms: optionalNumber(value.created_at_ms),
    updated_at_ms: optionalNumber(value.updated_at_ms),
    tasks,
  };
}

function normalizeResident(value: unknown): Resident | null {
  if (!isObject(value)) return null;
  return {
    ...labels(value),
    name: optionalText(value.name),
    agent_id: optionalText(value.agent_id),
    session_id: optionalText(value.session_id),
    charter: optionalText(value.charter),
    state: residentState(value.state),
    raw_state: optionalText(value.state),
    proposed_by: optionalText(value.proposed_by),
    approved_by: optionalText(value.approved_by),
    created_at_ms: optionalNumber(value.created_at_ms),
    updated_at_ms: optionalNumber(value.updated_at_ms),
  };
}

const RESIDENT_RANK: Record<ResidentState, number> = {
  proposed: 0,
  approved: 1,
  unknown: 2,
  retired: 3,
};

export function normalizeMissions(value: unknown): Missions {
  const raw = isObject(value) ? value : {};
  const missions = list(raw.missions)
    .map(normalizeMission)
    .filter((mission): mission is Mission => mission !== null)
    // Newest first; missions without a time sink to the bottom.
    .sort(
      (left, right) =>
        (right.created_at_ms ?? 0) - (left.created_at_ms ?? 0) ||
        (right.id ?? 0) - (left.id ?? 0),
    );
  const residents = list(raw.residents)
    .map(normalizeResident)
    .filter((resident): resident is Resident => resident !== null)
    .sort(
      (left, right) =>
        RESIDENT_RANK[left.state] - RESIDENT_RANK[right.state] ||
        (left.name ?? "").localeCompare(right.name ?? ""),
    );
  return { missions, residents, holds: normalizeHolds(raw.holds) };
}

export async function fetchMissions(signal: AbortSignal): Promise<Missions> {
  return readApi("/api/missions", signal, normalizeMissions);
}

/** A task's texts in full, as `/api/missions/task/<id>` answers them. */
export interface TaskTexts {
  id: number | null;
  brief: string | null;
  report: string | null;
  review_note: string | null;
  updated_at_ms: number | null;
}

export function normalizeTaskTexts(value: unknown): TaskTexts {
  const raw = isObject(value) ? value : {};
  return {
    id: optionalNumber(raw.id),
    brief: optionalText(raw.brief),
    report: optionalText(raw.report),
    review_note: optionalText(raw.review_note),
    updated_at_ms: optionalNumber(raw.updated_at_ms),
  };
}

/** GET /api/missions/task/<id>: a task's brief, report and review note in
 * full, read when the task is opened. */
export async function fetchTaskTexts(task: number, signal: AbortSignal): Promise<TaskTexts> {
  return readApi(`/api/missions/task/${task}`, signal, normalizeTaskTexts);
}

/** How a task's record shows as a step: what it moved the task to.
 * `back`: sent back for another round; `closed`: the task ended there. */
export interface StageStep {
  label: string;
  tone: "forward" | "back" | "closed";
}

export function stageStep(stage: TaskStage): StageStep {
  switch (stage.stage) {
    case "reviewed":
      return stage.verdict === "accepted"
        ? { label: "accepted", tone: "forward" }
        : { label: "sent back", tone: "back" };
    case "gated":
      if (stage.verdict === "accepted") return { label: "landing", tone: "forward" };
      // The host failed (the test database, or a base that disagreed with
      // itself), not the work: no verdict on it, judged again after a
      // backoff.
      if (stage.verdict === "host_fault")
        return { label: "gate host fault (the host's, not the work's)", tone: "forward" };
      return {
        label: `gate ${stage.verdict ?? "verdict"}`,
        tone:
          stage.verdict === "rejected" ||
          stage.verdict === "conflict" ||
          stage.verdict === "incompatible"
            ? "back"
            : "forward",
      };
    case "held":
      // Its source is held by the owner: nothing was pushed, it waits.
      return { label: "waits behind the owner's hold", tone: "forward" };
    case "aborted":
    case "cancelled":
      return { label: stage.stage, tone: "closed" };
    default:
      return { label: stage.stage, tone: "forward" };
  }
}

/** Whether the task's current step shows as a success: it landed, or was
 * accepted. A cancelled task never does. */
export function endedWell(state: TaskState): boolean {
  return state === "landed" || state === "accepted";
}

/** States of a task in a gated mission whose worker idles while its work is
 * judged or lands: the worker is not idle, the task is not done. */
const AT_THE_GATE: ReadonlyArray<string> = ["accepted", "landing"];

/** The sidebar's RSI line: halted first, then the state of the task in
 * flight (accepted and landing included), else idle. */
export function rsiPhase(
  rsi: Pick<RsiSummary, "halted" | "open_task"> | null,
): string | null {
  if (!rsi) return null;
  if (rsi.halted) return "halted";
  return rsi.open_task?.state ?? "idle";
}

/** What a session row says instead of its engine's state, when the
 * session's task waits for the gate or lands: `task accepted`, `task
 * landing`. Null when the row shows the observed state. */
export function sessionTaskLabel(
  sessionId: string,
  rsi: Pick<RsiSummary, "open_task"> | null,
): string | null {
  const task = rsi?.open_task;
  if (!task || task.session_id !== sessionId) return null;
  return AT_THE_GATE.includes(task.state) ? `task ${task.state}` : null;
}

/** The exact CLI command a human runs to approve a proposed resident. */
export function approveCommand(name: string | null): string {
  return `spawn resident approve --name ${name ?? "<name>"}`;
}

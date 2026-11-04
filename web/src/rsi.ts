// Read model of the RSI page: the open objective missions
// (docs/fleet-architecture.md §10.4) from `/api/rsi`, one task from
// `/api/rsi/task/<id>` and the resident's pane from `/api/rsi/pane`. Every
// field is normalized, so a server that omits one renders an empty value
// instead of crashing the page.

import { apiHeaders } from "./api";

export type Direction = "lower" | "higher";

/** `open` until solved (one-shot) or met (recurring, metric at target). */
export type ObjectiveStatus = "open" | "solved" | "met";

/** One `task.gated` record of the control stream. */
export interface GateVerdict {
  verdict: string;
  reason: string | null;
  metric: string | null;
  candidate: string | null;
  at_ms: number;
}

/** A gate verdict with the task it judged, for the page's verdict list. */
export interface TaskVerdict extends GateVerdict {
  task: number;
  objective: number | null;
  /** Position of this verdict among the task's verdicts, from 1. */
  round: number;
}

export interface RsiTask {
  id: number;
  kind: string | null;
  agent_id: string | null;
  session_id: string | null;
  state: string;
  revisions: number;
  objective: number | null;
  candidate: string | null;
  base: string | null;
  metric: string | null;
  scenario: string | null;
  brief: string | null;
  review_note: string | null;
  created_by: string | null;
  created_at_ms: number;
  delivered_at_ms: number | null;
  updated_at_ms: number;
  /** Oldest first. */
  gates: GateVerdict[];
}

export interface RsiLanding {
  task: number;
  at_ms: number;
  metric: string | null;
  baseline: number | null;
  candidate: number | null;
}

export interface RsiObjective {
  id: number;
  title: string;
  prompt: string;
  recurring: boolean;
  metric: string | null;
  unit: string;
  direction: Direction;
  /** Value the objective aims for; `null` when it names none. */
  target: number | null;
  status: ObjectiveStatus;
  /** Latest landed metric value. */
  current: number | null;
  tasks: number[];
  /** Oldest first. */
  landings: RsiLanding[];
}

export interface RsiResident {
  name: string;
  agent_id: string | null;
  session_id: string | null;
  state: string | null;
}

export interface RsiMission {
  id: number;
  text: string | null;
  state: string | null;
  sources: string[];
  gate: string | null;
  accept: string | null;
  budget: { minutes: number | null; tool_calls: number | null } | null;
  baseline: string | null;
  halted: string | null;
  scenario_floor: number;
  gate_rejections: number;
  gate_aborts: number;
  backlog: string | null;
  backlog_error: string | null;
  resident: RsiResident | null;
  next_objective: number | null;
  objectives: RsiObjective[];
  /** Newest first. */
  tasks: RsiTask[];
}

export interface RsiDashboard {
  generated_at_ms: number;
  halt_after: number;
  missions: RsiMission[];
}

export interface RsiTaskDetail extends RsiTask {
  mission_id: number | null;
  report: string | null;
  gate_log: string | null;
  /** Oldest first, each with its log. */
  gates: (GateVerdict & { log: string | null })[];
}

export interface RsiPane {
  live: boolean;
  terminal: string | null;
  session_id: string | null;
  captured_at_ms: number;
  text: string;
}

/** One landed value of a metric, for the chart. */
export interface MetricPoint {
  task: number;
  at_ms: number;
  baseline: number | null;
  candidate: number | null;
  accepted: boolean;
}

/** Halt limit of both counters (docs/fleet-architecture.md §10.3). */
export const HALT_LIMIT = 3;

/** Task states after which nothing more happens to a task. */
const SETTLED = new Set(["landed", "cancelled"]);

export function isSettled(state: string): boolean {
  return SETTLED.has(state);
}

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : {};
}

function isObject(value: unknown): boolean {
  return value !== null && typeof value === "object" && !Array.isArray(value);
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

function normalizeGate(value: unknown): GateVerdict {
  const raw = object(value);
  return {
    verdict: text(raw.verdict, "unknown"),
    reason: optionalText(raw.reason),
    metric: optionalText(raw.metric),
    candidate: optionalText(raw.candidate),
    at_ms: number(raw.at_ms),
  };
}

function normalizeTask(value: unknown): RsiTask {
  const raw = object(value);
  return {
    id: number(raw.id),
    kind: optionalText(raw.kind),
    agent_id: optionalText(raw.agent_id),
    session_id: optionalText(raw.session_id),
    state: text(raw.state, "unknown"),
    revisions: Math.max(0, number(raw.revisions)),
    objective: optionalNumber(raw.objective),
    candidate: optionalText(raw.candidate),
    base: optionalText(raw.base),
    metric: optionalText(raw.metric),
    scenario: optionalText(raw.scenario),
    brief: optionalText(raw.brief),
    review_note: optionalText(raw.review_note),
    created_by: optionalText(raw.created_by),
    created_at_ms: number(raw.created_at_ms),
    delivered_at_ms: optionalNumber(raw.delivered_at_ms),
    updated_at_ms: number(raw.updated_at_ms),
    gates: list(raw.gates)
      .map(normalizeGate)
      .sort((left, right) => left.at_ms - right.at_ms),
  };
}

function normalizeLanding(value: unknown): RsiLanding {
  const raw = object(value);
  return {
    task: number(raw.task),
    at_ms: number(raw.at_ms),
    metric: optionalText(raw.metric),
    baseline: optionalNumber(raw.baseline),
    candidate: optionalNumber(raw.candidate),
  };
}

function objectiveStatus(value: unknown): ObjectiveStatus {
  return value === "solved" || value === "met" ? value : "open";
}

function normalizeObjective(value: unknown): RsiObjective {
  const raw = object(value);
  const id = number(raw.id);
  return {
    id,
    title: text(raw.title, `Objective ${id}`),
    prompt: text(raw.prompt),
    recurring: raw.recurring === true,
    metric: optionalText(raw.metric),
    unit: text(raw.unit),
    direction: raw.direction === "higher" ? "higher" : "lower",
    target: optionalNumber(raw.target),
    status: objectiveStatus(raw.status),
    current: optionalNumber(raw.current),
    tasks: list(raw.tasks).filter(
      (task): task is number => typeof task === "number",
    ),
    landings: list(raw.landings)
      .map(normalizeLanding)
      .sort((left, right) => left.at_ms - right.at_ms || left.task - right.task),
  };
}

function normalizeResident(value: unknown): RsiResident | null {
  if (!isObject(value)) return null;
  const raw = object(value);
  return {
    name: text(raw.name, "resident"),
    agent_id: optionalText(raw.agent_id),
    session_id: optionalText(raw.session_id),
    state: optionalText(raw.state),
  };
}

function normalizeBudget(value: unknown): RsiMission["budget"] {
  if (!isObject(value)) return null;
  const raw = object(value);
  return {
    minutes: optionalNumber(raw.minutes),
    tool_calls: optionalNumber(raw.tool_calls),
  };
}

function normalizeMission(value: unknown): RsiMission {
  const raw = object(value);
  return {
    id: number(raw.id),
    text: optionalText(raw.text),
    state: optionalText(raw.state),
    sources: list(raw.sources).filter(
      (source): source is string => typeof source === "string",
    ),
    gate: optionalText(raw.gate),
    accept: optionalText(raw.accept),
    budget: normalizeBudget(raw.budget),
    baseline: optionalText(raw.baseline),
    halted: optionalText(raw.halted),
    scenario_floor: number(raw.scenario_floor),
    gate_rejections: number(raw.gate_rejections),
    gate_aborts: number(raw.gate_aborts),
    backlog: optionalText(raw.backlog),
    backlog_error: optionalText(raw.backlog_error),
    resident: normalizeResident(raw.resident),
    next_objective: optionalNumber(raw.next_objective),
    objectives: list(raw.objectives)
      .map(normalizeObjective)
      .sort((left, right) => left.id - right.id),
    tasks: list(raw.tasks)
      .map(normalizeTask)
      .sort((left, right) => right.id - left.id),
  };
}

export function normalizeDashboard(value: unknown): RsiDashboard {
  const raw = object(value);
  return {
    generated_at_ms: number(raw.generated_at_ms, Date.now()),
    halt_after: number(raw.halt_after, HALT_LIMIT) || HALT_LIMIT,
    missions: list(raw.missions)
      .map(normalizeMission)
      .sort((left, right) => left.id - right.id),
  };
}

export function normalizeTaskDetail(value: unknown): RsiTaskDetail {
  const raw = object(value);
  const task = normalizeTask(raw);
  return {
    ...task,
    mission_id: optionalNumber(raw.mission_id),
    // The detail carries the full brief.
    brief: optionalText(raw.brief),
    report: optionalText(raw.report),
    gate_log: optionalText(raw.gate_log),
    gates: list(raw.gates)
      .map((entry) => ({
        ...normalizeGate(entry),
        log: optionalText(object(entry).log),
      }))
      .sort((left, right) => left.at_ms - right.at_ms),
  };
}

export function normalizePane(value: unknown): RsiPane {
  const raw = object(value);
  return {
    live: raw.live === true,
    terminal: optionalText(raw.terminal),
    session_id: optionalText(raw.session_id),
    captured_at_ms: number(raw.captured_at_ms),
    text: text(raw.text),
  };
}

/** Every gate verdict of a mission, newest first. */
export function verdictsOf(mission: RsiMission): TaskVerdict[] {
  return mission.tasks
    .flatMap((task) =>
      task.gates.map((gate, index) => ({
        ...gate,
        task: task.id,
        objective: task.objective,
        round: index + 1,
      })),
    )
    .sort((left, right) => right.at_ms - left.at_ms || right.task - left.task);
}

/** The mission's task still in flight, if any (one at a time). */
export function openTask(mission: RsiMission): RsiTask | null {
  return mission.tasks.find((task) => !isSettled(task.state)) ?? null;
}

/** The objective the page leads with: the one being worked on, else the
 * next the integrator will assign, else the last one. */
export function focusObjective(mission: RsiMission): RsiObjective | null {
  const open = openTask(mission);
  const id = open?.objective ?? mission.next_objective;
  return (
    mission.objectives.find((objective) => objective.id === id) ??
    mission.objectives[mission.objectives.length - 1] ??
    null
  );
}

/** The objective the metric chart shows by default: the focus objective
 * when it has a metric, else the latest objective with a measured landing. */
export function chartObjective(mission: RsiMission): RsiObjective | null {
  const focus = focusObjective(mission);
  if (focus?.metric) return focus;
  const measured = mission.objectives.filter(
    (objective) => objective.metric && objective.landings.length > 0,
  );
  return (
    measured[measured.length - 1] ??
    mission.objectives.find((objective) => objective.metric) ??
    null
  );
}

/** The chart's points: an objective's landings that carry a value. */
export function metricPoints(objective: RsiObjective | null): MetricPoint[] {
  if (!objective) return [];
  return objective.landings.map((landing) => ({
    task: landing.task,
    at_ms: landing.at_ms,
    baseline: landing.baseline,
    candidate: landing.candidate,
    accepted: true,
  }));
}

async function getJson(url: string, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(url, {
    signal,
    cache: "no-store",
    headers: apiHeaders(),
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

export async function fetchDashboard(
  signal: AbortSignal,
): Promise<RsiDashboard> {
  return normalizeDashboard(await getJson("/api/rsi", signal));
}

export async function fetchTask(
  task: number,
  signal: AbortSignal,
): Promise<RsiTaskDetail> {
  return normalizeTaskDetail(await getJson(`/api/rsi/task/${task}`, signal));
}

export async function fetchPane(
  mission: number | null,
  signal: AbortSignal,
): Promise<RsiPane> {
  const query = mission === null ? "" : `?mission=${mission}`;
  return normalizePane(await getJson(`/api/rsi/pane${query}`, signal));
}

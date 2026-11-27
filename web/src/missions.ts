// Read model of missions: `/api/missions`. Every field is normalized, so a
// server that omits one (or sends null) renders an empty value instead of
// crashing. Read-only: this module only ever issues GET requests.
//
// Pure at module load (no static React/DOM imports) so `node --test` executes
// this module directly and `cap-web-missions-view` avoids cross-capability
// static edges into `cap-agent-grading`.

import type { RsiSummary } from "./api.ts";
import type { Labels } from "./label.ts";
import { labels } from "./label.ts";
import { normalizeActivity, normalizeTaskLivenessState } from "./taskActivity.ts";
import type { TaskActivity, TaskLivenessState } from "./taskActivity.ts";
import { normalizeHolds } from "./attention.ts";
import type { SourceHold } from "./attention.ts";

export type { SourceHold } from "./attention.ts";
export type { TaskLivenessState } from "./taskActivity.ts";

export type TaskDifficulty = "l1" | "l2" | "l3";
export type TaskTierRole = "top-decision" | "mainline" | "mechanical-offload";
export type GradingBadgeTone = "neutral" | "info" | "success" | "warning" | "danger" | "purple";

/** Rework escalation and preset reroute metadata projected by `MISSION_BOARD`. */
export interface TaskReworkStepUp {
  quality_rejections: number;
  from_difficulty: TaskDifficulty | null;
  routed_difficulty: TaskDifficulty | null;
  stepped_up: boolean;
  first_preset_id: string | null;
  preset_id: string | null;
}

/** Input shape accepted by `formatTaskGrading` for live or legacy task rows. */
export interface TaskGradingInput {
  difficulty?: TaskDifficulty | null;
  routed_difficulty?: TaskDifficulty | null;
  preset_id?: string | null;
  first_preset_id?: string | null;
  tier_role?: TaskTierRole | null;
  quality_rejections?: number | null;
  engine_label?: string | null;
  model_label?: string | null;
  execution_profile?: string | null;
  rework_step_up?: TaskReworkStepUp | null;
}

/** Formatted display view of a task's grading, preset routing, and step-up state. */
export interface TaskGradingView {
  difficulty: TaskDifficulty | null;
  routedDifficulty: TaskDifficulty | null;
  difficultyLabel: "L1" | "L2" | "L3" | null;
  routedDifficultyLabel: "L1" | "L2" | "L3" | null;
  initialDifficultyTone: GradingBadgeTone;
  difficultyTone: GradingBadgeTone;
  /** Uppercase difficulty display: "L1", "L2", "L3", or "L1 → L2" when stepped up. */
  difficultyBadge: string | null;
  /** Tight badge for timeline rows: "L1", "L2", "L3", or "L1→L2". */
  compactDifficultyBadge: string | null;
  /** Non-null when stepped up with both grades known: "L1 → L2". */
  stepUpLabel: string | null;
  steppedUp: boolean;
  tierRole: TaskTierRole | null;
  presetId: string | null;
  firstPresetId: string | null;
  rerouted: boolean;
  /** Non-null when `first_preset_id !== preset_id` (both non-null): "first → current". */
  presetTrajectory: string | null;
  /** For list/card preset pill: `presetTrajectory` when rerouted, else `presetId`. */
  presetBadge: string | null;
  engineLabel: string | null;
  modelLabel: string | null;
  /** `"engine/model"`, or whichever one is present, else null. */
  engineModelLabel: string | null;
  executionProfile: string | null;
  qualityRejections: number;
  hasReworkHistory: boolean;
  /** Compact rework/step-up indicator badge for cards/lists. */
  reworkBadge: string | null;
  /** Ultra-compact marker for timeline rows ("↑", "↑1", "1r", "⇄", or null). */
  compactReworkBadge: string | null;
  /** Short context note next to `quality_rejections` in `TaskPanel`. */
  reworkNote: string | null;
  /** Tooltip descriptions for each badge group. */
  difficultyTitle: string;
  presetTitle: string;
  reworkTitle: string;
  /** Single-line summary for row and hover tooltips. */
  tooltipLine: string;
  reworkStepUp: TaskReworkStepUp | null;
}

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
  /** Optional grading metadata, projected by `MISSION_BOARD` and omitted gracefully by older servers. */
  difficulty?: TaskDifficulty | null;
  routed_difficulty?: TaskDifficulty | null;
  preset_id?: string | null;
  first_preset_id?: string | null;
  tier_role?: TaskTierRole | null;
  quality_rejections?: number | null;
  engine_label?: string | null;
  model_label?: string | null;
  execution_profile?: string | null;
  rework_step_up?: TaskReworkStepUp | null;
  /** The owner's hold its landing last waited behind, if any. */
  held_by: number | null;
  /** Parked by the load-sensitive bound; null for any other task, and
   * absent from an older server. */
  parked?: TaskParked | null;
  created_by: string | null;
  created_at_ms: number | null;
  updated_at_ms: number | null;
  last_event_at_ms?: number | null;
  silence_ms?: number | null;
  liveness_state?: TaskLivenessState | null;
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
  generated_at_ms?: number | null;
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

function optionalNonNegativeInt(value: unknown): number | null {
  const raw = optionalNumber(value);
  return raw === null ? null : Math.max(0, Math.trunc(raw));
}

/** Normalize a task skill difficulty string (`"l1" | "l2" | "l3"`). */
export function normalizeTaskDifficulty(value: unknown): TaskDifficulty | null {
  if (typeof value !== "string") return null;
  const key = value.toLowerCase();
  return key === "l1" || key === "l2" || key === "l3" ? key : null;
}

/** Normalize a task tier role string (`"top-decision" | "mainline" | "mechanical-offload"`). */
export function normalizeTaskTierRole(value: unknown): TaskTierRole | null {
  if (typeof value !== "string") return null;
  const key = value.toLowerCase();
  return key === "top-decision" || key === "mainline" || key === "mechanical-offload" ? key : null;
}

/** Normalize `rework_step_up` payload from `MISSION_BOARD` / `MISSION_BY_ID`. */
export function normalizeTaskReworkStepUp(value: unknown): TaskReworkStepUp | null {
  if (!isObject(value)) return null;
  const from_difficulty = normalizeTaskDifficulty(value.from_difficulty);
  const routed_difficulty = normalizeTaskDifficulty(value.routed_difficulty);
  const first_preset_id = optionalText(value.first_preset_id);
  const preset_id = optionalText(value.preset_id);
  const quality_rejections = optionalNonNegativeInt(value.quality_rejections) ?? 0;
  const stepped_up =
    value.stepped_up === true ||
    (from_difficulty !== null && routed_difficulty !== null && from_difficulty !== routed_difficulty);
  return {
    quality_rejections,
    from_difficulty,
    routed_difficulty,
    stepped_up,
    first_preset_id,
    preset_id,
  };
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
    difficulty: normalizeTaskDifficulty(value.difficulty),
    routed_difficulty: normalizeTaskDifficulty(value.routed_difficulty),
    preset_id: optionalText(value.preset_id),
    first_preset_id: optionalText(value.first_preset_id),
    tier_role: normalizeTaskTierRole(value.tier_role),
    quality_rejections: optionalNonNegativeInt(value.quality_rejections),
    engine_label: optionalText(value.engine_label),
    model_label: optionalText(value.model_label),
    execution_profile: optionalText(value.execution_profile),
    rework_step_up: normalizeTaskReworkStepUp(value.rework_step_up),
    held_by: optionalNumber(value.held_by),
    parked: normalizeParked(value.parked),
    created_by: optionalText(value.created_by),
    created_at_ms: optionalNumber(value.created_at_ms),
    updated_at_ms: optionalNumber(value.updated_at_ms),
    last_event_at_ms: optionalNumber(value.last_event_at_ms),
    silence_ms: optionalNumber(value.silence_ms),
    liveness_state: normalizeTaskLivenessState(value.liveness_state),
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
  return {
    generated_at_ms: optionalNumber(raw.generated_at_ms),
    missions,
    residents,
    holds: normalizeHolds(raw.holds),
  };
}

export async function fetchMissions(signal: AbortSignal): Promise<Missions> {
  const { readApi } = await import("./api.ts");
  return readApi("/api/missions", signal, normalizeMissions);
}

/** A task's texts in full, as `/api/tasks/<n>` answers them. */
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

/** GET /api/tasks/<n>: a task's brief, report and review note in
 * full, read when the task is opened. */
export async function fetchTaskTexts(task: number, signal: AbortSignal): Promise<TaskTexts> {
  const { readApi } = await import("./api.ts");
  return readApi(`/api/tasks/${task}`, signal, normalizeTaskTexts);
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

/** Format a canonical task difficulty (`"l1" | "l2" | "l3"`) as `"L1" | "L2" | "L3"`. */
export function formatDifficultyLabel(
  grade: TaskDifficulty | null | undefined,
): "L1" | "L2" | "L3" | null {
  switch (grade) {
    case "l1":
      return "L1";
    case "l2":
      return "L2";
    case "l3":
      return "L3";
    default:
      return null;
  }
}

/** Badge tone for a skill difficulty level (`l3` purple, `l2` info, `l1` neutral). */
export function taskDifficultyTone(
  grade: TaskDifficulty | null | undefined,
): GradingBadgeTone {
  switch (grade) {
    case "l3":
      return "purple";
    case "l2":
      return "info";
    default:
      return "neutral";
  }
}

/** Map a canonical difficulty grade to its top-down stack tier role. */
export function tierRoleForDifficulty(
  grade: TaskDifficulty | null | undefined,
): TaskTierRole | null {
  switch (grade) {
    case "l3":
      return "top-decision";
    case "l2":
      return "mainline";
    case "l1":
      return "mechanical-offload";
    default:
      return null;
  }
}

/** Normalize and format a task's native grading, preset, and rework step-up history.
 * Returns `null` when a task has no grading metadata or carries only the historical
 * unrouted database default (`difficulty='l2'`, zero rejections, no preset/profile). */
export function formatTaskGrading(task: TaskGradingInput): TaskGradingView | null {
  const reworkStepUp = normalizeTaskReworkStepUp(task.rework_step_up);
  const difficulty =
    normalizeTaskDifficulty(task.difficulty) ?? reworkStepUp?.from_difficulty ?? null;
  const routedDifficulty =
    normalizeTaskDifficulty(task.routed_difficulty) ??
    reworkStepUp?.routed_difficulty ??
    difficulty;
  const presetId = optionalText(task.preset_id) ?? reworkStepUp?.preset_id ?? null;
  const firstPresetId =
    optionalText(task.first_preset_id) ?? reworkStepUp?.first_preset_id ?? null;
  const qualityRejections =
    optionalNonNegativeInt(task.quality_rejections) ??
    reworkStepUp?.quality_rejections ??
    0;
  const engineLabel = optionalText(task.engine_label);
  const modelLabel = optionalText(task.model_label);
  const executionProfile = optionalText(task.execution_profile);
  const rawTierRole = normalizeTaskTierRole(task.tier_role);

  // `mission_tasks.difficulty` defaults to 2 ('l2') and `rework_step_up` is
  // `null` for historical or unrouted default rows. Suppress badges for pure
  // default unrouted rows so legacy tasks do not show synthetic `L2 · mainline` pills.
  const isPlainDefaultUnrouted =
    presetId === null &&
    firstPresetId === null &&
    qualityRejections === 0 &&
    difficulty === "l2" &&
    (routedDifficulty === null || routedDifficulty === "l2") &&
    reworkStepUp === null &&
    executionProfile === null &&
    engineLabel === null &&
    modelLabel === null;
  if (isPlainDefaultUnrouted) {
    return null;
  }

  const activeDifficulty = routedDifficulty ?? difficulty;
  const tierRole = rawTierRole ?? tierRoleForDifficulty(activeDifficulty);
  if (
    difficulty === null &&
    routedDifficulty === null &&
    presetId === null &&
    firstPresetId === null &&
    tierRole === null &&
    engineLabel === null &&
    modelLabel === null &&
    executionProfile === null &&
    qualityRejections === 0 &&
    reworkStepUp === null
  ) {
    return null;
  }

  const steppedUp =
    reworkStepUp?.stepped_up === true ||
    (difficulty !== null && routedDifficulty !== null && difficulty !== routedDifficulty);
  const rerouted = firstPresetId !== null && presetId !== null && firstPresetId !== presetId;

  const difficultyLabel = formatDifficultyLabel(difficulty);
  const routedDifficultyLabel = formatDifficultyLabel(routedDifficulty);
  const initialDifficultyTone = taskDifficultyTone(difficulty ?? routedDifficulty);
  const difficultyTone: GradingBadgeTone = steppedUp
    ? "warning"
    : taskDifficultyTone(activeDifficulty);
  const stepUpLabel =
    steppedUp && difficultyLabel && routedDifficultyLabel
      ? `${difficultyLabel} → ${routedDifficultyLabel}`
      : null;
  const difficultyBadge = stepUpLabel ?? formatDifficultyLabel(activeDifficulty);
  const compactDifficultyBadge =
    steppedUp && difficultyLabel && routedDifficultyLabel
      ? `${difficultyLabel}→${routedDifficultyLabel}`
      : formatDifficultyLabel(activeDifficulty);

  const presetTrajectory = rerouted ? `${firstPresetId} → ${presetId}` : null;
  const presetBadge = presetTrajectory ?? presetId;
  const engineModelLabel =
    engineLabel && modelLabel ? `${engineLabel}/${modelLabel}` : (engineLabel ?? modelLabel ?? null);
  const hasReworkHistory =
    steppedUp || rerouted || qualityRejections > 0 || reworkStepUp !== null;

  let reworkBadge: string | null = null;
  if (steppedUp && qualityRejections > 0) reworkBadge = `step-up · ${qualityRejections} rej`;
  else if (steppedUp) reworkBadge = "step-up";
  else if (rerouted && qualityRejections > 0) reworkBadge = `rerouted · ${qualityRejections} rej`;
  else if (rerouted) reworkBadge = "rerouted";
  else if (qualityRejections > 0) reworkBadge = `${qualityRejections} quality rej`;

  let compactReworkBadge: string | null = null;
  if (steppedUp) compactReworkBadge = qualityRejections > 0 ? `↑${qualityRejections}` : "↑";
  else if (qualityRejections > 0) compactReworkBadge = `${qualityRejections}r`;
  else if (rerouted) compactReworkBadge = "⇄";

  let reworkNote: string | null = null;
  if (stepUpLabel) reworkNote = `stepped up ${stepUpLabel}`;
  else if (steppedUp) reworkNote = "stepped up";
  else if (qualityRejections > 0 && routedDifficultyLabel) {
    reworkNote = `same tier (${routedDifficultyLabel})`;
  } else if (rerouted && routedDifficultyLabel) {
    reworkNote = `rerouted (${routedDifficultyLabel})`;
  }

  const difficultyTitle =
    steppedUp && difficultyLabel && routedDifficultyLabel
      ? `Stepped up ${difficultyLabel} → ${routedDifficultyLabel}${tierRole ? ` · tier ${tierRole}` : ""}`
      : difficultyBadge
        ? `Difficulty ${difficultyBadge}${tierRole ? ` · tier ${tierRole}` : ""}`
        : tierRole
          ? `Tier ${tierRole}`
          : "";

  const presetTitle = [
    rerouted
      ? `Rerouted ${firstPresetId} → ${presetId}`
      : presetId
        ? `Preset ${presetId}`
        : null,
    executionProfile ? `profile ${executionProfile}` : null,
    engineModelLabel,
  ]
    .filter((part): part is string => part !== null && part !== "")
    .join(" · ");

  const reworkTitle = [
    qualityRejections > 0
      ? `${qualityRejections} quality rejection${qualityRejections === 1 ? "" : "s"}`
      : null,
    stepUpLabel ? `stepped up ${stepUpLabel}` : steppedUp ? "stepped up" : null,
    rerouted ? `rerouted ${firstPresetId} → ${presetId}` : null,
  ]
    .filter((part): part is string => part !== null && part !== "")
    .join(" · ");

  const tooltipLine = [
    difficultyBadge,
    tierRole,
    presetBadge,
    engineModelLabel,
    reworkBadge,
  ]
    .filter((part): part is string => part !== null && part !== "")
    .join(" · ");

  return {
    difficulty,
    routedDifficulty,
    difficultyLabel,
    routedDifficultyLabel,
    initialDifficultyTone,
    difficultyTone,
    difficultyBadge,
    compactDifficultyBadge,
    stepUpLabel,
    steppedUp,
    tierRole,
    presetId,
    firstPresetId,
    rerouted,
    presetTrajectory,
    presetBadge,
    engineLabel,
    modelLabel,
    engineModelLabel,
    executionProfile,
    qualityRejections,
    hasReworkHistory,
    reworkBadge,
    compactReworkBadge,
    reworkNote,
    difficultyTitle,
    presetTitle,
    reworkTitle,
    tooltipLine,
    reworkStepUp,
  };
}

export interface TaskReviewCommand {
  /** Short operator-facing purpose label for the command. */
  label: string;
  /** Single-line Spawn CLI command string. */
  command: string;
}

/** Operator review and dry-run land CLI commands for a task (PR #172).
 * Validates strictly that `taskId` is a positive safe integer so invalid
 * values never produce malformed or injected command strings. */
export function taskReviewCommands(taskId: number | null | undefined): TaskReviewCommand[] {
  if (typeof taskId !== "number" || !Number.isSafeInteger(taskId) || taskId <= 0) {
    return [];
  }
  const id = String(taskId);
  return [
    { label: "Commits ahead of main", command: `spawn task commits ${id}` },
    { label: "Diff summary", command: `spawn task diff ${id} --stat` },
    { label: "Land dry-run (ff-only)", command: `spawn task land ${id} --dry-run` },
    { label: "Land dry-run (squash)", command: `spawn task land ${id} --squash --dry-run` },
  ];
}

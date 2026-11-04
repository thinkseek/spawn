// Read model of missions: `/api/missions`. Every field is normalized, so a
// server that omits one (or sends null) renders an empty value instead of
// crashing. Read-only: this module only ever issues GET requests.

export const TASK_STATES = [
  "assigned",
  "delivered",
  "reported",
  "revise",
  "accepted",
  "cancelled",
] as const;
export type TaskState = (typeof TASK_STATES)[number] | "unknown";

/** The happy path a task walks; `revise` and `cancelled` branch off it. */
export const PIPELINE: ReadonlyArray<TaskState> = [
  "assigned",
  "delivered",
  "reported",
  "accepted",
];

export type ResidentState = "proposed" | "approved" | "retired" | "unknown";

export interface MissionTask {
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
  created_by: string | null;
  created_at_ms: number | null;
  updated_at_ms: number | null;
}

export interface Mission {
  id: number | null;
  text: string | null;
  origin: string | null;
  sources: string[];
  state: string | null;
  summary: string | null;
  created_at_ms: number | null;
  updated_at_ms: number | null;
  tasks: MissionTask[];
}

export interface Resident {
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

function normalizeTask(value: unknown): MissionTask | null {
  if (!isObject(value)) return null;
  return {
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
    created_by: optionalText(value.created_by),
    created_at_ms: optionalNumber(value.created_at_ms),
    updated_at_ms: optionalNumber(value.updated_at_ms),
  };
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
    origin: optionalText(value.origin),
    sources,
    state: optionalText(value.state),
    summary: optionalText(value.summary),
    created_at_ms: optionalNumber(value.created_at_ms),
    updated_at_ms: optionalNumber(value.updated_at_ms),
    tasks,
  };
}

function normalizeResident(value: unknown): Resident | null {
  if (!isObject(value)) return null;
  return {
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
  return { missions, residents };
}

export async function fetchMissions(signal: AbortSignal): Promise<Missions> {
  const response = await fetch("/api/missions", {
    signal,
    cache: "no-store",
    headers: { Accept: "application/json" },
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return normalizeMissions(await response.json());
}

/** The exact CLI command a human runs to approve a proposed resident. */
export function approveCommand(name: string | null): string {
  return `spawn resident approve --name ${name ?? "<name>"}`;
}

// Read model of the RSI dashboard: `/api/rsi`, `/api/rsi/iteration/<n>` and
// `/api/rsi/pane`. Every field is normalized, so a server that omits one
// renders an empty value instead of crashing the page.

export type Direction = "lower" | "higher";

export interface RsiState {
  baseline: string;
  iteration: number;
  consecutive_rejected: number;
  infra_failures: number;
  scenario_count: number;
  halted: boolean;
  halt_reason: string | null;
}

export interface RsiObjective {
  task_id: number;
  title: string;
  prompt: string;
  metric: string;
  unit: string;
  direction: Direction;
  /** Value the objective aims for; `null` when the task names none. */
  target: number | null;
}

export interface RsiIteration {
  iteration: number;
  task_id: number;
  phase: string;
  started_at_ms: number;
  updated_at_ms: number;
  note: string | null;
  candidate: string | null;
  patch_bytes: number;
}

export interface MetricPair {
  baseline: number | null;
  candidate: number | null;
  unit: string;
}

export interface RsiAttempt {
  task_id: number;
  attempt: number;
  iteration: number;
  accepted: boolean;
  created_at_ms: number;
  metrics: Record<string, MetricPair>;
}

export interface MetricPoint {
  iteration: number;
  created_at_ms: number;
  metric: string;
  baseline: number | null;
  candidate: number | null;
  accepted: boolean;
}

export interface RsiDashboard {
  generated_at_ms: number;
  state: RsiState | null;
  objective: RsiObjective | null;
  iterations: RsiIteration[];
  attempts: RsiAttempt[];
  metric_history: MetricPoint[];
}

export interface RsiIterationDetail {
  iteration: number;
  task_id: number;
  phase: string;
  note: string | null;
  candidate: string | null;
  patch: string;
  gate_log: string;
  prompt: string | null;
}

export interface RsiPane {
  live: boolean;
  terminal: string | null;
  captured_at_ms: number;
  text: string;
}

/** Halt limit of both counters, from `docs/rsi.md` §9. */
export const HALT_LIMIT = 3;

/** The loop's one long-lived session. */
export const RSI_SESSION = "session_rsi";

const TERMINAL_PHASES = new Set(["Landed", "Rejected", "Aborted"]);

export function isTerminalPhase(phase: string): boolean {
  return TERMINAL_PHASES.has(phase);
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
  return typeof value === "string" ? value : null;
}

function number(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function normalizeState(value: unknown): RsiState | null {
  if (!isObject(value)) return null;
  const raw = object(value);
  return {
    baseline: text(raw.baseline),
    iteration: number(raw.iteration),
    consecutive_rejected: number(raw.consecutive_rejected),
    infra_failures: number(raw.infra_failures),
    scenario_count: number(raw.scenario_count),
    halted: raw.halted === true,
    halt_reason: optionalText(raw.halt_reason),
  };
}

function normalizeObjective(value: unknown): RsiObjective | null {
  if (!isObject(value)) return null;
  const raw = object(value);
  return {
    task_id: number(raw.task_id),
    title: text(raw.title, "Untitled objective"),
    prompt: text(raw.prompt),
    metric: text(raw.metric),
    unit: text(raw.unit),
    direction: raw.direction === "higher" ? "higher" : "lower",
    target: optionalNumber(raw.target),
  };
}

function normalizeIteration(value: unknown): RsiIteration {
  const raw = object(value);
  return {
    iteration: number(raw.iteration),
    task_id: number(raw.task_id),
    phase: text(raw.phase, "unknown"),
    started_at_ms: number(raw.started_at_ms),
    updated_at_ms: number(raw.updated_at_ms),
    note: optionalText(raw.note),
    candidate: optionalText(raw.candidate),
    patch_bytes: number(raw.patch_bytes),
  };
}

function normalizeAttempt(value: unknown): RsiAttempt {
  const raw = object(value);
  const metrics: Record<string, MetricPair> = {};
  for (const [name, entry] of Object.entries(object(raw.metrics))) {
    const pair = object(entry);
    metrics[name] = {
      baseline: optionalNumber(pair.baseline),
      candidate: optionalNumber(pair.candidate),
      unit: text(pair.unit),
    };
  }
  return {
    task_id: number(raw.task_id),
    attempt: number(raw.attempt),
    iteration: number(raw.iteration),
    accepted: raw.accepted === true,
    created_at_ms: number(raw.created_at_ms),
    metrics,
  };
}

function normalizePoint(value: unknown): MetricPoint {
  const raw = object(value);
  return {
    iteration: number(raw.iteration),
    created_at_ms: number(raw.created_at_ms),
    metric: text(raw.metric),
    baseline: optionalNumber(raw.baseline),
    candidate: optionalNumber(raw.candidate),
    accepted: raw.accepted === true,
  };
}

export function normalizeDashboard(value: unknown): RsiDashboard {
  const raw = object(value);
  return {
    generated_at_ms: number(raw.generated_at_ms, Date.now()),
    state: normalizeState(raw.state),
    objective: normalizeObjective(raw.objective),
    iterations: list(raw.iterations)
      .map(normalizeIteration)
      .sort((left, right) => right.iteration - left.iteration),
    attempts: list(raw.attempts).map(normalizeAttempt),
    metric_history: list(raw.metric_history)
      .map(normalizePoint)
      .sort(
        (left, right) =>
          left.created_at_ms - right.created_at_ms ||
          left.iteration - right.iteration,
      ),
  };
}

export function normalizeIterationDetail(value: unknown): RsiIterationDetail {
  const raw = object(value);
  return {
    iteration: number(raw.iteration),
    task_id: number(raw.task_id),
    phase: text(raw.phase, "unknown"),
    note: optionalText(raw.note),
    candidate: optionalText(raw.candidate),
    patch: text(raw.patch),
    gate_log: text(raw.gate_log),
    prompt: optionalText(raw.prompt),
  };
}

export function normalizePane(value: unknown): RsiPane {
  const raw = object(value);
  return {
    live: raw.live === true,
    terminal: optionalText(raw.terminal),
    captured_at_ms: number(raw.captured_at_ms),
    text: text(raw.text),
  };
}

async function getJson(url: string, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(url, {
    signal,
    cache: "no-store",
    headers: { Accept: "application/json" },
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

export async function fetchDashboard(
  signal: AbortSignal,
): Promise<RsiDashboard> {
  return normalizeDashboard(await getJson("/api/rsi", signal));
}

export async function fetchIteration(
  iteration: number,
  signal: AbortSignal,
): Promise<RsiIterationDetail> {
  return normalizeIterationDetail(
    await getJson(`/api/rsi/iteration/${iteration}`, signal),
  );
}

export async function fetchPane(signal: AbortSignal): Promise<RsiPane> {
  return normalizePane(await getJson("/api/rsi/pane", signal));
}

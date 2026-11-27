// What a delivered task's worker did since the task reached it (objective
// 211, GitHub #63 part 1): `/api/missions` gives each delivered task its
// `activity`, its worker's latest engine output since the delivery and the
// tool calls it started since; this module says it in words.

/** A delivered task's activity, as `/api/missions` gives it: null for
 * every task not delivered. */
export interface TaskActivity {
  /** When the task was last delivered. */
  since_ms: number;
  /** The worker's latest engine output since then, and its name; both null
   * when there is none. */
  at_ms: number | null;
  event: string | null;
  /** The tool calls the worker started since then. */
  tool_calls: number;
}

/** No engine output for this long reads as quiet / warning. */
export const QUIET_MS = 10 * 60_000;

/** No engine output for this long reads as stalled (`疑似停滞`). */
export const STALLED_MS = 20 * 60_000;

export type TaskLivenessState =
  | "active_working"
  | "briefed_quiet"
  | "stalled"
  | "waiting_review"
  | "terminal";

export interface TaskLivenessView {
  state: TaskLivenessState;
  label: string;
  tone: "success" | "info" | "warning" | "muted";
  pulse: boolean;
  lastEventAtMs: number | null;
  silenceMs: number | null;
}

const TASK_LIVENESS_STATES: ReadonlySet<TaskLivenessState> = new Set([
  "active_working",
  "briefed_quiet",
  "stalled",
  "waiting_review",
  "terminal",
]);

export function normalizeTaskLivenessState(value: unknown): TaskLivenessState | null {
  if (typeof value !== "string") return null;
  return TASK_LIVENESS_STATES.has(value as TaskLivenessState)
    ? (value as TaskLivenessState)
    : null;
}

function finite(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/** A task's `activity` from the answer, or null (none, or not readable). */
export function normalizeActivity(value: unknown): TaskActivity | null {
  if (typeof value !== "object" || value === null) return null;
  const raw = value as Record<string, unknown>;
  const since = finite(raw.since_ms);
  if (since === null) return null;
  const at = finite(raw.at_ms);
  return {
    since_ms: since,
    at_ms: at,
    event: at !== null && typeof raw.event === "string" ? raw.event : null,
    tool_calls: Math.max(0, finite(raw.tool_calls) ?? 0),
  };
}

/** A span in short words: "40s", "4m", "1h 5m". */
export function spanWords(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

/** The line under a delivered task's stepper at `now`: "last output 4m ago
 * (tool.completed) · 12 tool calls since delivery", or "no output in the
 * 31m since delivery"; in the warning tone once no engine output came for
 * [`QUIET_MS`] or more. */
export function activityLine(activity: TaskActivity, now: number): { text: string; warning: boolean } {
  const calls = `${activity.tool_calls} tool call${activity.tool_calls === 1 ? "" : "s"} since delivery`;
  if (activity.at_ms === null) {
    const since = now - activity.since_ms;
    return {
      text: `no output in the ${spanWords(since)} since delivery`,
      warning: since >= QUIET_MS,
    };
  }
  const quiet = now - activity.at_ms;
  const event = activity.event !== null ? ` (${activity.event})` : "";
  return {
    text: `last output ${spanWords(quiet)} ago${event} · ${calls}`,
    warning: quiet >= QUIET_MS,
  };
}

/** Classify a task's execution liveness at `now`, aligned with
 * `spawn-android` `FleetLivenessModel.kt` (`实时推进 / 已下发·静默 / 静默运行 / 疑似停滞`). */
export function taskLiveness(
  task: {
    state: string;
    activity: TaskActivity | null;
    stages?: ReadonlyArray<{ at_ms: number | null }>;
    updated_at_ms?: number | null;
    created_at_ms?: number | null;
    last_event_at_ms?: number | null;
    silence_ms?: number | null;
    liveness_state?: TaskLivenessState | null;
  },
  gated = false,
  now = Date.now(),
): TaskLivenessView {
  const stageMax = (task.stages ?? []).reduce<number | null>(
    (max, stage) => (stage.at_ms !== null && (max === null || stage.at_ms > max) ? stage.at_ms : max),
    null,
  );
  const fallbackTs = task.last_event_at_ms ?? stageMax ?? task.updated_at_ms ?? task.created_at_ms ?? null;

  if (task.state === "landed" || task.state === "cancelled" || (task.state === "accepted" && !gated)) {
    return {
      state: "terminal",
      label: "已结束",
      tone: "muted",
      pulse: false,
      lastEventAtMs: fallbackTs,
      silenceMs: null,
    };
  }
  if (task.state === "reported" || task.state === "revise" || task.state === "landing" || task.state === "accepted") {
    return {
      state: "waiting_review",
      label: "待审核",
      tone: "muted",
      pulse: false,
      lastEventAtMs: fallbackTs,
      silenceMs: null,
    };
  }
  if (task.state !== "delivered") {
    return {
      state: "briefed_quiet",
      label: "待下发",
      tone: "info",
      pulse: false,
      lastEventAtMs: fallbackTs,
      silenceMs: null,
    };
  }

  const engineAtMs = task.activity?.at_ms ?? null;
  const lastEventAtMs = engineAtMs ?? task.activity?.since_ms ?? fallbackTs;
  const silenceMs =
    lastEventAtMs !== null ? Math.max(0, now - lastEventAtMs) : (task.silence_ms ?? null);

  let state: TaskLivenessState;
  if (silenceMs !== null && silenceMs >= STALLED_MS) {
    state = "stalled";
  } else if (
    (task.activity !== null && engineAtMs === null) ||
    (silenceMs !== null && silenceMs >= QUIET_MS)
  ) {
    state = "briefed_quiet";
  } else if (engineAtMs !== null) {
    state = "active_working";
  } else {
    state = task.liveness_state ?? "briefed_quiet";
  }

  if (state === "active_working") {
    return { state, label: "实时推进", tone: "success", pulse: true, lastEventAtMs, silenceMs };
  }
  if (state === "stalled") {
    return { state, label: "疑似停滞", tone: "warning", pulse: false, lastEventAtMs, silenceMs };
  }
  if (state === "waiting_review") {
    return { state, label: "待审核", tone: "muted", pulse: false, lastEventAtMs, silenceMs };
  }
  if (state === "terminal") {
    return { state, label: "已结束", tone: "muted", pulse: false, lastEventAtMs, silenceMs };
  }
  return {
    state: "briefed_quiet",
    label: task.activity && engineAtMs === null ? "已下发·静默" : "静默运行",
    tone: "info",
    pulse: false,
    lastEventAtMs,
    silenceMs,
  };
}

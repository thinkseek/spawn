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

/** No engine output for this long reads as a warning. */
export const QUIET_MS = 10 * 60_000;

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

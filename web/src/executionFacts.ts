/* What the session's latest execution says, as text: one line for the
 * header and labelled fields for the phone drawer, whose readers have no
 * hover, so the id and the start are fields there, never a tooltip only
 * (rev-f F3 on 54a632c9). Pure, tested in web/test/executionFacts.test.ts. */

import type { Execution } from "./api.ts";
import { clockTime, formatDuration, relativeTime } from "./format.ts";

/** The header's line: "claude · running 12m", or "claude · exited 0 · 3m ·
 * stopped". `now` is the clock a running one is measured to. */
export function executionLine(execution: Execution, now: number): string[] {
  const parts = [execution.engine || "execution"];
  if (execution.ended_at_ms === null) {
    parts.push(`running ${formatDuration(now - execution.started_at_ms)}`);
  } else {
    parts.push(execution.exit_code === null ? "no exit code" : `exited ${execution.exit_code}`);
    parts.push(formatDuration(execution.duration_ms));
    if (execution.cause) parts.push(execution.cause);
  }
  return parts;
}

/** The drawer's fields: the line's facts plus the id and the start, which
 * the header keeps on hover. */
export function executionFields(execution: Execution, now: number): Array<[string, string]> {
  const running = execution.ended_at_ms === null;
  const fields: Array<[string, string]> = [
    ["Execution", execution.execution_id],
    ["Engine", execution.engine || "—"],
    ["Started", `${relativeTime(execution.started_at_ms)} · ${clockTime(execution.started_at_ms)}`],
    [
      running ? "Running" : "Duration",
      running ? formatDuration(now - execution.started_at_ms) : formatDuration(execution.duration_ms),
    ],
  ];
  if (!running) {
    fields.push(["Exit", execution.exit_code === null ? "no exit code" : String(execution.exit_code)]);
    if (execution.cause) fields.push(["Cause", execution.cause]);
  }
  return fields;
}

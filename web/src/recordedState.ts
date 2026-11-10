/* A session's state as the log records it, for the sidebar (the lead,
 * 2026-11-10): running, waiting or ended, with its age, never the judge's
 * word, which can lag the record ("Completed" beside a running execution),
 * so the same session never reads one way in the list and another in its
 * header. The judgement stays in the Observation card. Pure, tested in
 * web/test/recordedState.test.ts. */

import type { SessionState } from "./api";
import type { FleetAgent } from "./fleet";

export interface Recorded {
  kind: "running" | "waiting" | "ended" | "none";
  label: string;
  /** The dot's tone, from the one state palette. */
  dot: SessionState;
  /** When it became so, as far as the record says: the last event while an
   * execution runs, its end once it ended. */
  since: number | null;
}

/** A live execution whose last event is one of these waits for input: its
 * turn ended (or none has begun), and no tool is open. */
const TURN_OVER = new Set(["turn.stopped", "turn.failed", "execution.started"]);

export function recordedState(
  agent: Pick<FleetAgent, "live" | "execution_id" | "started_at_ms" | "ended_at_ms" | "activity">,
): Recorded {
  const last = agent.activity.last_event_at_ms;
  if (!agent.live) {
    if (agent.ended_at_ms !== null) {
      return { kind: "ended", label: "Ended", dot: "completed", since: agent.ended_at_ms };
    }
    return { kind: "none", label: "No execution", dot: "unknown", since: last };
  }
  if (
    !agent.activity.current_tool &&
    agent.activity.last_event_name !== null &&
    TURN_OVER.has(agent.activity.last_event_name)
  ) {
    return { kind: "waiting", label: "Waiting", dot: "waiting", since: last ?? agent.started_at_ms };
  }
  return { kind: "running", label: "Running", dot: "working", since: last ?? agent.started_at_ms };
}

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
  /** When it became so, as far as the record says: the current turn's
   * start or end while an execution runs (its start before any turn), its
   * end once it ended. */
  since: number | null;
}

/** Running or waiting is the server's own reading of the live execution's
 * turn (fleet.rs `turn_open`, from the turn's start and end records), never
 * a list of event names kept here: the Bridge records a checkpoint, a
 * screen read or a notification after a turn ends (rev-f F5), and any such
 * list goes stale with the next kind of record. An open tool call is
 * running too. */
export function recordedState(
  agent: Pick<FleetAgent, "live" | "execution_id" | "started_at_ms" | "ended_at_ms" | "activity">,
): Recorded {
  const { activity } = agent;
  if (!agent.live) {
    if (agent.ended_at_ms !== null) {
      return { kind: "ended", label: "Ended", dot: "completed", since: agent.ended_at_ms };
    }
    return { kind: "none", label: "No execution", dot: "unknown", since: activity.last_event_at_ms };
  }
  // Since the turn began or ended; before any turn, since the execution
  // started. A server without the field (null) reads as running, as before.
  const since = activity.turn_at_ms ?? agent.started_at_ms ?? activity.last_event_at_ms;
  if (activity.turn_open === false && !activity.current_tool) {
    return { kind: "waiting", label: "Waiting", dot: "waiting", since };
  }
  return { kind: "running", label: "Running", dot: "working", since };
}

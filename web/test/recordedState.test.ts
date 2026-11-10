import assert from "node:assert/strict";
import { test } from "node:test";

import { recordedState } from "../src/recordedState.ts";

const T = 1_780_000_000_000;

function agent(over: {
  live?: boolean;
  ended?: number | null;
  tool?: string | null;
  last?: string | null;
  at?: number | null;
}) {
  return {
    live: over.live ?? true,
    execution_id: "exec_1",
    started_at_ms: T,
    ended_at_ms: over.ended ?? null,
    activity: {
      current_tool: over.tool ?? null,
      current_tool_summary: null,
      jobs: [],
      last_event_name: over.last === undefined ? "tool.completed" : over.last,
      last_event_at_ms: over.at === undefined ? T + 5_000 : over.at,
      event_count: null,
      tool_calls: null,
      executions: null,
    },
  };
}

test("an execution with a tool open, or mid-turn, is running since its last event", () => {
  assert.deepEqual(recordedState(agent({ tool: "Bash", last: "tool.started" })), {
    kind: "running",
    label: "Running",
    dot: "working",
    since: T + 5_000,
  });
  assert.equal(recordedState(agent({ last: "message.received" })).kind, "running");
});

test("a live execution whose turn ended waits, whatever the judge said", () => {
  for (const last of ["turn.stopped", "turn.failed", "execution.started"]) {
    const state = recordedState(agent({ last }));
    assert.equal(state.kind, "waiting", last);
    assert.equal(state.dot, "waiting");
  }
  // A tool still open is running, even after a turn event.
  assert.equal(recordedState(agent({ last: "turn.stopped", tool: "Bash" })).kind, "running");
});

test("an ended execution is ended since its end; none at all says so", () => {
  assert.deepEqual(recordedState(agent({ live: false, ended: T + 9_000 })), {
    kind: "ended",
    label: "Ended",
    dot: "completed",
    since: T + 9_000,
  });
  const none = recordedState({ ...agent({ live: false }), execution_id: null, started_at_ms: null });
  assert.equal(none.kind, "none");
  assert.equal(none.label, "No execution");
});

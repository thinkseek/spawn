import assert from "node:assert/strict";
import { test } from "node:test";

import { recordedState } from "../src/recordedState.ts";

const T = 1_780_000_000_000;

function agent(over: {
  live?: boolean;
  ended?: number | null;
  tool?: string | null;
  last?: string | null;
  open?: boolean | null;
  turnAt?: number | null;
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
      last_event_at_ms: T + 9_000,
      turn_open: over.open === undefined ? true : over.open,
      turn_at_ms: over.turnAt === undefined ? T + 5_000 : over.turnAt,
      event_count: null,
      tool_calls: null,
      executions: null,
    },
  };
}

test("a live execution whose turn is open, or with a tool open, runs since the turn began", () => {
  assert.deepEqual(recordedState(agent({ last: "tool.completed" })), {
    kind: "running",
    label: "Running",
    dot: "working",
    since: T + 5_000,
  });
  assert.equal(recordedState(agent({ open: false, tool: "Bash" })).kind, "running");
});

test("a live execution whose turn ended waits, whatever it recorded since (rev-f F5)", () => {
  // The server's turn_open, not the last event's name: a checkpoint, a
  // notice or a screen read after the stop, or a name no list here knows.
  for (const last of [
    "turn.stopped",
    "artifact.committed",
    "notification.received",
    "screen.observed",
    "some.future_record",
  ]) {
    const state = recordedState(agent({ open: false, last }));
    assert.equal(state.kind, "waiting", last);
    assert.equal(state.dot, "waiting");
    assert.equal(state.since, T + 5_000, "since the turn ended");
  }
  // And the reverse: a turn-end name as the last event says nothing if the
  // server reads the turn open.
  assert.equal(recordedState(agent({ open: true, last: "turn.stopped" })).kind, "running");
  // No turn began yet: waiting since the execution started.
  const fresh = recordedState(agent({ open: false, turnAt: null, last: "execution.started" }));
  assert.equal(fresh.kind, "waiting");
  assert.equal(fresh.since, T);
  // A server without the field reads as running.
  assert.equal(recordedState(agent({ open: null })).kind, "running");
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

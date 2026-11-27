import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { normalizeMissions, taskReviewCommands } from "../src/missions.ts";
import {
  QUIET_MS,
  STALLED_MS,
  activityLine,
  normalizeActivity,
  spanWords,
  taskLiveness,
} from "../src/taskActivity.ts";

const MIN = 60_000;

test("a delivered task's activity says its last output and its calls since delivery (objective 211)", () => {
  const now = 1_000 * MIN;
  const working = normalizeActivity({ since_ms: now - 60 * MIN, at_ms: now - 4 * MIN, event: "tool.completed", tool_calls: 12 });
  assert.ok(working);
  assert.deepEqual(activityLine(working, now), {
    text: "last output 4m ago (tool.completed) · 12 tool calls since delivery",
    warning: false,
  });
  // One call is said in the singular.
  const one = normalizeActivity({ since_ms: now - 5 * MIN, at_ms: now - 30_000, event: "tool.started", tool_calls: 1 });
  assert.equal(one && activityLine(one, now).text, "last output 30s ago (tool.started) · 1 tool call since delivery");
  // No output since delivery.
  const silent = normalizeActivity({ since_ms: now - 31 * MIN, at_ms: null, event: null, tool_calls: 0 });
  assert.ok(silent);
  assert.deepEqual(activityLine(silent, now), { text: "no output in the 31m since delivery", warning: true });
});

test("the line turns to a warning once no engine output came for 10 minutes", () => {
  const now = 1_000 * MIN;
  const at = (quiet: number) => normalizeActivity({ since_ms: now - 120 * MIN, at_ms: now - quiet, event: "model.usage", tool_calls: 3 });
  assert.equal(activityLine(at(QUIET_MS - 1)!, now).warning, false);
  assert.equal(activityLine(at(QUIET_MS)!, now).warning, true);
  const fresh = normalizeActivity({ since_ms: now - 9 * MIN, at_ms: null, event: null, tool_calls: 0 })!;
  assert.deepEqual(activityLine(fresh, now), { text: "no output in the 9m since delivery", warning: false });
  assert.equal(activityLine(at(65 * MIN)!, now).text, "last output 1h 5m ago (model.usage) · 3 tool calls since delivery");
  assert.equal(spanWords(-5), "0s");
});

test("an activity that is null or unreadable is none", () => {
  assert.equal(normalizeActivity(null), null);
  assert.equal(normalizeActivity(undefined), null);
  assert.equal(normalizeActivity({ at_ms: 5 }), null, "no delivery time: none");
  assert.deepEqual(normalizeActivity({ since_ms: 1, at_ms: null, event: "tool.started", tool_calls: -2 }), {
    since_ms: 1,
    at_ms: null,
    event: null,
    tool_calls: 0,
  });
});

test("taskReviewCommands returns exact PR #172 operator CLI commands for positive safe integer task ids", () => {
  assert.deepEqual(
    taskReviewCommands(42).map((item) => item.command),
    [
      "spawn task commits 42",
      "spawn task diff 42 --stat",
      "spawn task land 42 --dry-run",
      "spawn task land 42 --squash --dry-run",
    ],
  );

  for (const id of [1, 42, Number.MAX_SAFE_INTEGER]) {
    const commands = taskReviewCommands(id);
    assert.equal(commands.length, 4);
    for (const item of commands) {
      assert.ok(item.label.length > 0);
      assert.ok(!/[\r\n]/.test(item.command));
      assert.ok(item.command.includes(String(id)));
    }
  }
});

test("taskReviewCommands returns an empty list for non-positive, non-integer, or malformed task ids", () => {
  const invalid: unknown[] = [
    0,
    -1,
    -99,
    NaN,
    Infinity,
    -Infinity,
    1.5,
    Number.MAX_SAFE_INTEGER + 1,
    null,
    undefined,
    "42; rm -rf /",
  ];
  for (const id of invalid) {
    assert.deepEqual(taskReviewCommands(id as number | null | undefined), [], `expected [] for ${String(id)}`);
  }
});

test("TaskPanel wires Review & land CLI helper with copy action and zero extra API fetches", () => {
  const panel = readFileSync(new URL("../src/components/missions/TaskPanel.tsx", import.meta.url), "utf8");
  assert.ok(panel.includes("taskReviewCommands(task.id)"));
  assert.ok(panel.includes("TaskReviewCliPanel"));
  assert.ok(panel.includes('label="Review & land CLI"'));
  assert.ok(panel.includes("navigator.clipboard"));
  assert.equal(panel.includes("fetch("), false);
  assert.equal(panel.includes("/api/"), false);
});

test("taskLiveness distinguishes 实时推进, 已下发·静默, 静默运行, 疑似停滞, 待审核, and 已结束", () => {
  const now = 2_000 * MIN;
  const active = taskLiveness(
    {
      state: "delivered",
      activity: { since_ms: now - 3 * MIN, at_ms: now - 30_000, event: "tool.completed", tool_calls: 4 },
    },
    true,
    now,
  );
  assert.equal(active.state, "active_working");
  assert.equal(active.label, "实时推进");
  assert.equal(active.tone, "success");
  assert.equal(active.pulse, true);

  const briefedSilent = taskLiveness(
    {
      state: "delivered",
      activity: { since_ms: now - 2 * MIN, at_ms: null, event: null, tool_calls: 0 },
    },
    true,
    now,
  );
  assert.equal(briefedSilent.state, "briefed_quiet");
  assert.equal(briefedSilent.label, "已下发·静默");
  assert.equal(briefedSilent.tone, "info");
  assert.equal(briefedSilent.pulse, false);

  const quietRunning = taskLiveness(
    {
      state: "delivered",
      activity: { since_ms: now - 30 * MIN, at_ms: now - QUIET_MS, event: "model.usage", tool_calls: 2 },
    },
    true,
    now,
  );
  assert.equal(quietRunning.state, "briefed_quiet");
  assert.equal(quietRunning.label, "静默运行");
  assert.equal(quietRunning.tone, "info");
  assert.equal(quietRunning.pulse, false);

  const stalled = taskLiveness(
    {
      state: "delivered",
      activity: { since_ms: now - 40 * MIN, at_ms: now - STALLED_MS, event: "tool.completed", tool_calls: 5 },
    },
    true,
    now,
  );
  assert.equal(stalled.state, "stalled");
  assert.equal(stalled.label, "疑似停滞");
  assert.equal(stalled.tone, "warning");
  assert.equal(stalled.pulse, false);

  const waitingReview = taskLiveness({ state: "reported", activity: null }, true, now);
  assert.equal(waitingReview.state, "waiting_review");
  assert.equal(waitingReview.label, "待审核");
  assert.equal(waitingReview.pulse, false);

  const acceptedNoGate = taskLiveness({ state: "accepted", activity: null }, false, now);
  assert.equal(acceptedNoGate.state, "terminal");
  assert.equal(acceptedNoGate.label, "已结束");
  assert.equal(acceptedNoGate.pulse, false);
});

test("normalizeMissions reads top-level generated_at_ms and task liveness fields while tolerating older payloads", () => {
  const now = 5_000 * MIN;
  const parsed = normalizeMissions({
    generated_at_ms: now,
    missions: [
      {
        id: 1,
        text: "Track liveness",
        tasks: [
          {
            id: 10,
            state: "delivered",
            last_event_at_ms: now - 15 * MIN,
            silence_ms: 15 * MIN,
            liveness_state: "briefed_quiet",
            created_at_ms: now - 20 * MIN,
          },
          {
            id: 11,
            state: "assigned",
            created_at_ms: now - 5 * MIN,
          },
        ],
      },
    ],
  });

  assert.equal(parsed.generated_at_ms, now);
  const [t10, t11] = parsed.missions[0].tasks;
  assert.equal(t10.last_event_at_ms, now - 15 * MIN);
  assert.equal(t10.silence_ms, 15 * MIN);
  assert.equal(t10.liveness_state, "briefed_quiet");
  assert.equal(t11.last_event_at_ms, null);
  assert.equal(t11.silence_ms, null);
  assert.equal(t11.liveness_state, null);

  const legacy = normalizeMissions({ missions: [] });
  assert.equal(legacy.generated_at_ms, null);
});

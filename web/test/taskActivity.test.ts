import assert from "node:assert/strict";
import { test } from "node:test";

import { QUIET_MS, activityLine, normalizeActivity, spanWords } from "../src/taskActivity.ts";

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

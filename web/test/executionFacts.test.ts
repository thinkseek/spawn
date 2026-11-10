import assert from "node:assert/strict";
import { test } from "node:test";

import type { Execution } from "../src/api.ts";
import { executionFields, executionLine } from "../src/executionFacts.ts";

const NOW = Date.now();
const M = 60_000;

function execution(over: Partial<Execution> = {}): Execution {
  return {
    execution_id: "exec_1521",
    engine: "claude",
    started_at_ms: NOW - 90 * M,
    ended_at_ms: null,
    cause: null,
    exit_code: null,
    exit_reported: false,
    duration_ms: null,
    ...over,
  };
}

test("the header line: running, or how it ended", () => {
  assert.deepEqual(executionLine(execution(), NOW), ["claude", "running 1h 30m"]);
  assert.deepEqual(
    executionLine(execution({ ended_at_ms: NOW - M, exit_code: 0, duration_ms: 3 * M, cause: "stopped" }), NOW),
    ["claude", "exited 0", "3m 0s", "stopped"],
  );
});

test("the drawer shows the id and the start as text (rev-f F3), not only on hover", () => {
  const fields = new Map(executionFields(execution(), NOW));
  assert.equal(fields.get("Execution"), "exec_1521");
  assert.match(fields.get("Started") ?? "", /^1h ago · \d\d:\d\d:\d\d$/);
  assert.equal(fields.get("Running"), "1h 30m");
  const ended = new Map(
    executionFields(execution({ ended_at_ms: NOW, exit_code: 2, duration_ms: 90 * M, cause: "lost" }), NOW),
  );
  assert.equal(ended.get("Execution"), "exec_1521");
  assert.equal(ended.get("Duration"), "1h 30m");
  assert.equal(ended.get("Exit"), "2");
  assert.equal(ended.get("Cause"), "lost");
});

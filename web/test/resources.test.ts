import assert from "node:assert/strict";
import { test } from "node:test";

import {
  MEMORY_WARN_BYTES,
  memoryBar,
  normalizeGateRoom,
  normalizeMemory,
  waitingGates,
} from "../src/resources.ts";

const GIB = 1024 ** 3;

test("memory is read as the server reports it, a missing value stays null", () => {
  assert.deepEqual(
    normalizeMemory({ available_bytes: 9 * GIB, total_bytes: 64 * GIB, warn_bytes: 4 * GIB }),
    { available_bytes: 9 * GIB, total_bytes: 64 * GIB, warn_bytes: 4 * GIB },
  );
  assert.deepEqual(normalizeMemory({ available_bytes: null, total_bytes: "lots" }), {
    available_bytes: null,
    total_bytes: null,
    warn_bytes: MEMORY_WARN_BYTES,
  });
  assert.equal(MEMORY_WARN_BYTES, 4 * GIB);
  assert.equal(normalizeMemory(undefined), null, "an older server reports none");
});

test("memory under the 4 GiB line is short; unread is unread, not zero", () => {
  const memory = (available: number | null, total: number | null) => ({
    available_bytes: available,
    total_bytes: total,
    warn_bytes: 4 * GIB,
  });
  assert.deepEqual(memoryBar(memory(3 * GIB, 64 * GIB)), { tone: "short", scale: 64 * GIB });
  assert.deepEqual(memoryBar(memory(20 * GIB, 64 * GIB)), { tone: "fine", scale: 64 * GIB });
  assert.equal(memoryBar(memory(null, 64 * GIB)).tone, "unread");
  // Without a total the bar runs to twice the line, or past what is free.
  assert.equal(memoryBar(memory(2 * GIB, null)).scale, 8 * GIB);
  assert.equal(memoryBar(memory(20 * GIB, null)).scale, 23 * GIB);
});

test("gate room: one row per gated mission, the waiting gates longest first", () => {
  const rows = normalizeGateRoom([
    { mission: 7, text: "Owner work on Spawn: changes", task: null, since_ms: null, last_ms: null, waits: 0, escalated: false },
    { mission: 4, text: "RSI: improve Spawn", task: 87, since_ms: 2_000, last_ms: 9_000, waits: 5, escalated: true },
    { mission: 9, text: null, task: 91, since_ms: 1_000, last_ms: 1_000, waits: 1, escalated: false },
    { text: "no id" },
    "nonsense",
  ]);
  assert.ok(rows);
  assert.deepEqual(
    rows.map((row) => [row.mission, row.name, row.task, row.waits, row.escalated]),
    [
      [7, "Owner work on Spawn", null, 0, false],
      [4, "RSI", 87, 5, true],
      [9, "Mission 9", 91, 1, false],
    ],
  );
  assert.deepEqual(
    waitingGates(rows).map((row) => row.mission),
    [9, 4],
  );
  assert.equal(normalizeGateRoom(undefined), null, "an older server reports none");
  assert.deepEqual(normalizeGateRoom([]), []);
});

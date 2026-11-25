import assert from "node:assert/strict";
import { test } from "node:test";

import {
  INTERRUPT_EXPIRY_MS,
  MASTER_MODES,
  MODE_HELP,
  expired,
  isMode,
  masterBody,
  masterVerdict,
} from "../src/master.ts";

test("the four modes, each explained", () => {
  assert.deepEqual(MASTER_MODES, ["auto", "steer", "queue", "restart"]);
  for (const mode of MASTER_MODES) assert.ok(MODE_HELP[mode].length > 0);
  assert.ok(isMode("steer"));
  assert.ok(!isMode("STEER"));
  assert.ok(!isMode(null));
  assert.deepEqual(masterBody("hi", "queue", "k"), { text: "hi", mode: "queue", key: "k" });
});

test("a message's answer: sent (first time or replayed), kept for a retry, or refused for good", () => {
  assert.deepEqual(masterVerdict(202, { command_id: 9, woke: true, replayed: false }), {
    state: "sent",
    command_id: 9,
    woke: true,
    replayed: false,
  });
  assert.deepEqual(masterVerdict(202, { command_id: 9, replayed: true }), {
    state: "sent",
    command_id: 9,
    woke: false,
    replayed: true,
  });
  assert.equal(masterVerdict(null, null).state, "retry");
  assert.equal(masterVerdict(502, null).state, "retry");
  assert.equal(masterVerdict(429, null).state, "retry");
  assert.deepEqual(masterVerdict(409, { error: "the key was used" }), {
    state: "refused",
    error: "the key was used",
    status: 409,
  });
  assert.equal(masterVerdict(400, {}).state, "refused");
  assert.equal(masterVerdict(403, {}).state, "refused");
});

/** Review S2: sent only as the server says it, 202 with a numeric
 * command id; a bare 200 (a proxy's page) or a 202 without one stays in
 * the outbox, sent again with its key. */
test("a message is sent only on 202 with a command id", () => {
  for (const [status, body] of [
    [200, null],
    [200, { command_id: 9 }],
    [202, null],
    [202, { command_id: "9" }],
    [204, null],
  ] as const) {
    assert.equal(masterVerdict(status, body).state, "retry", `${status} ${JSON.stringify(body)}`);
  }
  assert.equal(masterVerdict(202, { command_id: 9 }).state, "sent");
});

test("a queued steer or restart expires after five minutes; auto and queue never do", () => {
  assert.equal(INTERRUPT_EXPIRY_MS, 300_000);
  for (const mode of ["steer", "restart"]) {
    assert.equal(expired(mode, 0, INTERRUPT_EXPIRY_MS), false, mode);
    assert.equal(expired(mode, 0, INTERRUPT_EXPIRY_MS + 1), true, mode);
  }
  for (const mode of ["auto", "queue"]) assert.equal(expired(mode, 0, 10 * INTERRUPT_EXPIRY_MS), false, mode);
});

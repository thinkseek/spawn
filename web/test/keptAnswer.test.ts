import assert from "node:assert/strict";
import { test } from "node:test";

import { keptAnswer } from "../src/keptAnswer.ts";

const week = { window: "7d", at_ms: 1 };
const month = { window: "30d", at_ms: 2 };

test("the current answer is shown as it is (GitHub #61)", () => {
  assert.deepEqual(keptAnswer(month, null, week), { shown: month, refreshing: false });
  // A refresh of the same key that failed keeps its own answer.
  assert.deepEqual(keptAnswer(month, "502", month), { shown: month, refreshing: false });
});

test("while a new window loads, the last answer stays on screen, marked refreshing", () => {
  assert.deepEqual(keptAnswer(null, null, week), { shown: week, refreshing: true });
  // The first load: nothing to keep.
  assert.deepEqual(keptAnswer(null, null, null), { shown: null, refreshing: false });
});

test("a failed read of the new window never shows the previous window's answer", () => {
  assert.deepEqual(keptAnswer(null, "502 Bad Gateway", week), { shown: null, refreshing: false });
  assert.deepEqual(keptAnswer(null, "502 Bad Gateway", null), { shown: null, refreshing: false });
});

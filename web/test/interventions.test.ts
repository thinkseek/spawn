import assert from "node:assert/strict";
import { test } from "node:test";

import { figureState } from "../src/interventions.ts";

const answer = {
  window: "7d",
  since_ms: 0,
  until_ms: 1,
  agent_hours: 10,
  total: 2,
  by_kind: { command: 2 },
  retries: 0,
  per_agent_hour: 0.2,
  items: [],
};

test("a failed read with no answer shows its reason, not Loading for ever (049 live, 2026-11-11)", () => {
  const state = figureState(null, "storage backend failure: canceling statement due to statement timeout (57014) ");
  assert.deepEqual(state, {
    kind: "failed",
    reason: "storage backend failure: canceling statement due to statement timeout (57014)",
  });
});

test("before any answer or failure the figure is loading", () => {
  assert.deepEqual(figureState(null, null), { kind: "loading" });
  assert.deepEqual(figureState(undefined, "  "), { kind: "loading" });
});

test("an answer is shown, and a later failure keeps it and says why", () => {
  const fresh = figureState(answer, null);
  assert.equal(fresh.kind, "view");
  assert.equal(fresh.kind === "view" && fresh.view.figure, "0.200");
  assert.equal(fresh.kind === "view" && fresh.stale, null);
  const stale = figureState(answer, "request failed");
  assert.equal(stale.kind === "view" && stale.stale, "request failed");
  assert.equal(stale.kind === "view" && stale.view.count, "2 interventions");
});

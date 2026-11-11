import assert from "node:assert/strict";
import { test } from "node:test";

import { decisionsFromHealth } from "../src/fleetDecisions.ts";

const health = {
  last_hour: {
    judgements: 9,
    by_judge: { rules: 7, jev: 1, other: 1 },
    judge_unavailable: 1,
    model_latency_p50_ms: 1200,
    disagreements: 2,
    tool_calls: 140,
    failures: 3,
  },
  ladder_24h: { rules: 300, model: 20, dispatcher: 40, supervisor: 5, human: 2, liveness: 1 },
  ladder_extra: [["owner_gate", 4]] as Array<[string, number]>,
  agent_hours_24h: 40,
  human_per_agent_hour: 0.05,
};

test("the last hour's judgements: who judged, Jev's share, its misses", () => {
  const { judgements } = decisionsFromHealth(health);
  assert.equal(judgements.window, "last hour");
  assert.deepEqual([judgements.total, judgements.rules, judgements.jev, judgements.other], [9, 7, 1, 1]);
  assert.equal(judgements.jevShare, 1 / 9);
  assert.deepEqual([judgements.unavailable, judgements.disagreed, judgements.jevP50Ms], [1, 2, 1200]);
});

test("a judge count above the total counts, as the strip did", () => {
  const { judgements } = decisionsFromHealth({ ...health, last_hour: { ...health.last_hour, judgements: 5 } });
  assert.equal(judgements.total, 9);
  assert.equal(decisionsFromHealth({ ...health, last_hour: { ...health.last_hour, judgements: 0, by_judge: {} } }).judgements.jevShare, null);
});

test("the 24 h ladder: every step, liveness, the extra tiers and the human rate", () => {
  const { ladder } = decisionsFromHealth(health);
  assert.equal(ladder.window, "24 h");
  assert.deepEqual(ladder.counts, { rules: 300, model: 20, dispatcher: 40, supervisor: 5, human: 2, liveness: 1 });
  assert.equal(ladder.total, 368);
  assert.deepEqual(ladder.extra, [["owner_gate", 4]]);
  assert.deepEqual([ladder.humanPerAgentHour, ladder.agentHours, ladder.toolCallsPerHour, ladder.failuresPerHour], [0.05, 40, 140, 3]);
  assert.equal(decisionsFromHealth({ ...health, ladder_24h: {} }).ladder.counts.human, 0, "a step the server leaves out counts 0");
});

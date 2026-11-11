import assert from "node:assert/strict";
import { test } from "node:test";

import { decisionsFromApi, decisionsFromHealth } from "../src/fleetDecisions.ts";

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

// /api/scorecard/fleet's answer for a 7 d window (objective 043).
const week = {
  window: "7d",
  since_ms: 1792800000000,
  until_ms: 1793404800000,
  ladder: { rules: 1, model: 3, dispatcher: 4, supervisor: 1, human: 1, liveness: 1 },
  ladder_extra: [["spawn-integrate", 1]],
  agent_hours: 26.0,
  human_per_agent_hour: 0.038,
  escalations: 3,
  interventions: { total: 3, by_kind: { command: 1, resume: 1, owner_task: 1, cancel_reassign: 0 } },
  outcomes: { landed: 2, accepted: 1, cancelled: 0, aborted: 1, over_budget: 0 },
  landed_per_day: 0.429,
  judgements: { total: 4, by_judge: { jev: 3, rules: 1 }, judge_unavailable: 1, disagreements: 1, model_latency_p50_ms: 500 },
  missions: [
    { mission: 1, text: "busy", hours: 168.0, idle_hours: 133.0, idle: true },
    { mission: 2, text: "quiet", hours: 48.0, idle_hours: 20.0, idle: false },
  ],
};

test("a window from the experience views fills the same cards, labelled in words", () => {
  const window = decisionsFromApi(week);
  assert.equal(window.ladder.window, "7 d");
  assert.equal(window.judgements.window, "7 d");
  assert.deepEqual(window.ladder.counts, { rules: 1, model: 3, dispatcher: 4, supervisor: 1, human: 1, liveness: 1 });
  assert.deepEqual(window.ladder.extra, [["spawn-integrate", 1]]);
  assert.equal(window.ladder.total, 11, "the extra tiers are shown beside the ladder, not in its total");
  assert.deepEqual([window.ladder.humanPerAgentHour, window.ladder.agentHours], [0.038, 26]);
  assert.deepEqual([window.ladder.toolCallsPerHour, window.ladder.failuresPerHour], [null, null]);
  assert.deepEqual([window.judgements.total, window.judgements.jev, window.judgements.rules], [4, 3, 1]);
  assert.deepEqual([window.judgements.unavailable, window.judgements.disagreed, window.judgements.jevP50Ms], [1, 1, 500]);
  assert.equal(window.landedPerDay, 0.429);
  assert.equal(window.escalations, 3);
  assert.deepEqual(window.interventions, { total: 3, byKind: { command: 1, resume: 1, owner_task: 1, cancel_reassign: 0 } });
  assert.deepEqual(window.idleHoursByMission, [
    { mission: 1, text: "busy", hours: 168, idleHours: 133, idle: true },
    { mission: 2, text: "quiet", hours: 48, idleHours: 20, idle: false },
  ]);
});

test("every window has its words, and a thin answer reads as nothing counted", () => {
  for (const [name, words] of [["24h", "24 h"], ["30d", "30 d"], ["90d", "90 d"]]) {
    assert.equal(decisionsFromApi({ ...week, window: name }).ladder.window, words);
  }
  const thin = decisionsFromApi({ window: "24h" });
  assert.equal(thin.ladder.total, 0);
  assert.equal(thin.ladder.humanPerAgentHour, null, "no agent hours, no rate");
  assert.equal(thin.judgements.jevShare, null);
  assert.equal(thin.landedPerDay, null);
  assert.deepEqual(thin.idleHoursByMission, []);
  assert.deepEqual(decisionsFromApi(null).ladder.extra, []);
});

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import type { FleetAgent } from "../src/fleet.ts";
import { agentLiveness, snapshotFreshness } from "../src/ordering.ts";
import { decisionsFromApi, decisionsFromHealth } from "../src/fleetDecisions.ts";

function makeAgent(overrides: Partial<FleetAgent> & Pick<FleetAgent, "agent_id">): FleetAgent {
  const { activity, ...rest } = overrides;
  return {
    label: null,
    task_title: null,
    mission_name: null,
    session_id: `sess_${overrides.agent_id}`,
    role: "worker",
    role_rank: 1,
    engine: "claude",
    live: true,
    execution_id: null,
    started_at_ms: null,
    ended_at_ms: null,
    state: "WORKING",
    state_since_ms: null,
    attention: false,
    judged_at_ms: null,
    task: null,
    judgement: null,
    ...rest,
    activity: {
      current_tool: null,
      current_tool_summary: null,
      jobs: [],
      last_event_name: null,
      last_event_at_ms: null,
      turn_open: null,
      turn_at_ms: null,
      event_count: null,
      tool_calls: null,
      executions: null,
      ...activity,
    },
  };
}

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

test("idle hours by mission: owner work is said, never flagged idle, and keeps its hours", () => {
  const answer = {
    missions: [
      { mission: 1, text: "backlog", hours: 24, idle_hours: 24, idle: true, owner_work: false },
      { mission: 2, text: "owner", hours: 24, idle_hours: 20, idle: false, owner_work: true },
      { mission: 3, text: "busy", hours: 24, idle_hours: 0, idle: false, owner_work: false },
      { mission: 4, text: "older server", hours: 24, idle_hours: 20, idle: true, owner_work: true },
      { mission: 5, text: "no field", hours: 24, idle_hours: 20, idle: true },
    ],
  };
  const rows = decisionsFromApi(answer).idleHoursByMission ?? [];
  assert.deepEqual(
    rows.map((row) => [row.mission, row.idle, row.ownerWork, row.idleHours]),
    [
      [1, true, false, 24],
      [2, false, true, 20],
      [3, false, false, 0],
      [4, false, true, 20],
      [5, true, false, 20],
    ],
  );
});

test("idle hours by mission: paused hours are read, 0 from an older server (objective 099)", () => {
  const answer = {
    missions: [
      { mission: 6, text: "paused", hours: 24, idle_hours: 4, paused_hours: 20, idle: false, owner_work: false },
      { mission: 7, text: "older server", hours: 24, idle_hours: 24, idle: true, owner_work: false },
    ],
  };
  const rows = decisionsFromApi(answer).idleHoursByMission ?? [];
  assert.deepEqual(
    rows.map((row) => [row.mission, row.idle, row.idleHours, row.pausedHours]),
    [
      [6, false, 4, 20],
      [7, true, 24, 0],
    ],
  );
});

test("agentLiveness classifies all 6 states and never treats judged_at_ms as execution heartbeat (#63)", () => {
  const MIN = 60_000;
  const NOW = 10_000 * MIN;

  const active = agentLiveness(
    makeAgent({
      agent_id: "a_active",
      live: true,
      state: "WORKING",
      judged_at_ms: NOW - 1_000,
      last_event_at_ms: NOW - 2 * MIN,
      activity: { last_event_at_ms: NOW - 2 * MIN, last_event_name: "tool.completed" } as FleetAgent["activity"],
    }),
    NOW,
  );
  assert.equal(active.state, "active_heartbeat");
  assert.equal(active.label, "实时推进");
  assert.equal(active.pulse, true);

  // Recent judge ping must NEVER mask 7m of execution silence.
  const quiet = agentLiveness(
    makeAgent({
      agent_id: "a_quiet",
      live: true,
      state: "WORKING",
      judged_at_ms: NOW - 500,
      last_event_at_ms: NOW - 7 * MIN,
      activity: { last_event_at_ms: NOW - 7 * MIN, last_event_name: "model.usage" } as FleetAgent["activity"],
    }),
    NOW,
  );
  assert.equal(quiet.state, "quiet_working");
  assert.equal(quiet.label, "静默运行");
  assert.equal(quiet.pulse, false);
  assert.equal(quiet.silenceMs, 7 * MIN);

  // Even when only judged_at_ms is fresh, 16m since last execution event classifies as loop_stalled.
  const stalled = agentLiveness(
    makeAgent({
      agent_id: "a_stalled",
      live: true,
      state: "WORKING",
      judged_at_ms: NOW - 500,
      last_event_at_ms: NOW - 16 * MIN,
      activity: { last_event_at_ms: NOW - 16 * MIN, last_event_name: "tool.started" } as FleetAgent["activity"],
    }),
    NOW,
  );
  assert.equal(stalled.state, "loop_stalled");
  assert.equal(stalled.label, "疑似停滞");
  assert.equal(stalled.pulse, false);
  assert.equal(stalled.silenceMs, 16 * MIN);

  assert.equal(agentLiveness(makeAgent({ agent_id: "a_wait", state: "WAITING" }), NOW).state, "waiting_idle");
  assert.equal(agentLiveness(makeAgent({ agent_id: "a_blocked", state: "BLOCKED" }), NOW).state, "blocked");
  assert.equal(
    agentLiveness(makeAgent({ agent_id: "a_offline", live: false, state: "COMPLETED" }), NOW).state,
    "offline",
  );
});

test("snapshotFreshness detects fresh, aging (>=3m), frozen (>=10m), and node-heartbeat drift (#63)", () => {
  const MIN = 60_000;
  const NOW = 20_000 * MIN;

  assert.equal(snapshotFreshness(NOW - 2 * MIN, NOW, NOW - 2 * MIN).level, "fresh");
  assert.equal(snapshotFreshness(NOW - 3 * MIN, NOW, NOW - 3 * MIN).level, "aging");
  assert.equal(snapshotFreshness(NOW - 10 * MIN, NOW, NOW - 10 * MIN).level, "frozen");
  // Snapshot only 1m old, but a worker node heartbeat is 4m newer (>=3m drift) -> frozen.
  assert.equal(snapshotFreshness(NOW - 4 * MIN, NOW, NOW).level, "frozen");
});

test("fleet.ts normalizes top-level generated_at_ms with health fallback and per-agent liveness fields (#63)", () => {
  const fleetSrc = readFileSync(new URL("../src/fleet.ts", import.meta.url), "utf8");
  assert.ok(fleetSrc.includes("generated_at_ms: optionalNumber(raw.generated_at_ms) ?? health.generated_at_ms") || fleetSrc.includes("const generated_at_ms = optionalNumber(raw.generated_at_ms) ?? health.generated_at_ms;"));
  assert.ok(fleetSrc.includes("last_event_at_ms:"));
  assert.ok(fleetSrc.includes("silence_ms: optionalNumber(raw.silence_ms)"));
  assert.ok(fleetSrc.includes("liveness_state: normalizeAgentLivenessState(raw.liveness_state)"));
});

import assert from "node:assert/strict";
import { test } from "node:test";

import type { RsiLanding, RsiMission, RsiObjective } from "../src/rsi.ts";
import {
  chartsProgress,
  latestProgressLine,
  progressTitle,
  settledProgress,
} from "../src/rsiProgress.ts";

const HOUR = 3_600_000;
const NOW = 1_793_800_000_000;

function landing(task: number, at_ms: number): RsiLanding {
  return { task, at_ms, metric: null, baseline: null, candidate: null };
}

function objective(id: number, fields: Partial<RsiObjective> = {}): RsiObjective {
  return {
    id,
    title: `Objective ${id}`,
    prompt: "",
    recurring: false,
    metric: null,
    unit: "",
    direction: "lower",
    target: null,
    status: "open",
    current: null,
    tasks: [],
    landings: [],
    solved_on_base: null,
    ...fields,
  };
}

function mission(objectives: RsiObjective[]): RsiMission {
  return {
    id: 1,
    text: null,
    mission_name: null,
    state: "open",
    sources: [],
    gate: null,
    accept: "gate",
    budget: null,
    baseline: null,
    halted: null,
    scenario_floor: 0,
    gate_rejections: 0,
    gate_aborts: 0,
    backlog: null,
    backlog_error: null,
    resident: null,
    next_objective: null,
    objectives,
    tasks: [],
  };
}

// The scenario's mission (rsi/scenarios/063.sh): objective 4 lands first,
// 1 and 2 after, 3 is solved on the base, 5 is in flight.
const seeded = mission([
  objective(1, { status: "solved", tasks: [1], landings: [landing(1, NOW - 7 * HOUR)] }),
  objective(2, { status: "solved", tasks: [2], landings: [landing(2, NOW - 5 * HOUR)] }),
  objective(3, {
    status: "solved",
    tasks: [3],
    solved_on_base: { event: 77, task: 3, base: "b", at_ms: NOW - 4.5 * HOUR },
  }),
  objective(4, { status: "solved", tasks: [4], landings: [landing(4, NOW - 8 * HOUR)] }),
  objective(5, { tasks: [5] }),
]);

test("settledProgress places each settled objective at its first landing or its record, oldest first", () => {
  const progress = settledProgress(seeded);
  assert.equal(progress.total, 5);
  assert.equal(progress.settled, 4);
  assert.deepEqual(progress.points, [
    { objective: 4, task: 4, at_ms: NOW - 8 * HOUR, settled: 1, on_base: false },
    { objective: 1, task: 1, at_ms: NOW - 7 * HOUR, settled: 2, on_base: false },
    { objective: 2, task: 2, at_ms: NOW - 5 * HOUR, settled: 3, on_base: false },
    { objective: 3, task: 3, at_ms: NOW - 4.5 * HOUR, settled: 4, on_base: true },
  ]);
});

test("settledProgress takes an objective's first landing, counts met ones, breaks ties by id", () => {
  const progress = settledProgress(
    mission([
      objective(2, { status: "solved", landings: [landing(9, NOW - HOUR), landing(7, NOW - 3 * HOUR)] }),
      objective(1, { status: "solved", landings: [landing(8, NOW - HOUR)] }),
      objective(3, { status: "met", recurring: true, landings: [landing(6, NOW - HOUR)] }),
      objective(4, { status: "owner" }),
    ]),
  );
  assert.equal(progress.total, 4);
  assert.equal(progress.settled, 3);
  assert.deepEqual(
    progress.points.map((point) => [point.objective, point.task, point.settled]),
    [
      [2, 7, 1],
      [1, 8, 2],
      [3, 6, 3],
    ],
  );
});

test("a landing wins over a record solved on the base", () => {
  const progress = settledProgress(
    mission([
      objective(1, {
        status: "solved",
        landings: [landing(2, NOW - HOUR)],
        solved_on_base: { event: 1, task: 1, base: null, at_ms: NOW - 2 * HOUR },
      }),
    ]),
  );
  assert.deepEqual(progress.points, [
    { objective: 1, task: 2, at_ms: NOW - HOUR, settled: 1, on_base: false },
  ]);
});

test("progressTitle, and a mission with nothing settled", () => {
  assert.equal(progressTitle(settledProgress(seeded)), "4 of 5 objectives settled");
  const none = settledProgress(
    mission(seeded.objectives.map((entry) => ({ ...entry, status: "open", landings: [], solved_on_base: null }))),
  );
  assert.deepEqual(none, { total: 5, settled: 0, points: [] });
  assert.equal(progressTitle(none), "0 of 5 objectives settled");
  assert.equal(latestProgressLine(none, NOW), null);
  assert.equal(progressTitle(settledProgress(mission([]))), "0 of 0 objectives settled");
});

test("the card charts progress only when no objective names a metric", () => {
  assert.equal(chartsProgress(seeded), true);
  const measured = mission([
    ...seeded.objectives,
    objective(6, { metric: "ingest_total_ns", landings: [landing(10, NOW - HOUR)] }),
  ]);
  // A mission with a metric objective keeps the ratchet chart.
  assert.equal(chartsProgress(measured), false);
});

test("the card's last line names the latest point's objective, task and age", () => {
  assert.equal(
    latestProgressLine(settledProgress(seeded), NOW),
    "Latest: objective 3 solved on the base, task #3, 4h ago",
  );
  const landed = mission([
    objective(7, { status: "solved", landings: [landing(12, NOW - 90_000)] }),
  ]);
  assert.equal(latestProgressLine(settledProgress(landed), NOW), "Latest: objective 7 landed, task #12, 1m ago");
});

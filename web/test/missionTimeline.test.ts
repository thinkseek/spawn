import assert from "node:assert/strict";
import { test } from "node:test";

import type { Mission, MissionTask, TaskStage } from "../src/missions.ts";
import {
  deriveMissionTimeline,
  filterLanes,
  hasBars,
  isLive,
  laneState,
  missionName,
  phaseAt,
  taskBar,
} from "../src/missionTimeline.ts";

const T0 = 1_780_000_000_000;
const MIN = 60_000;

/** Stages from [stage, minutes after T0, verdict?]. */
function stages(rows: Array<[string, number, string?]>): TaskStage[] {
  return rows.map(([stage, at, verdict], index) => ({
    stage,
    verdict: verdict ?? null,
    at_ms: T0 + at * MIN,
    took_ms: index === 0 ? null : (at - rows[index - 1][1]) * MIN,
  }));
}

function task(id: number, state: string, rows: Array<[string, number, string?]>, extra: Partial<MissionTask> = {}): MissionTask {
  const list = stages(rows);
  return {
    id,
    kind: "resident",
    agent_id: "agent_resident_supply",
    session_id: "session_resident_supply",
    brief: "Do the thing",
    state: state as MissionTask["state"],
    raw_state: state,
    revisions: 0,
    review_note: null,
    report: null,
    candidate: null,
    objective: null,
    metric: null,
    last_gate: null,
    stages: list,
    model: "default",
    created_by: "spawn-integrate",
    created_at_ms: list[0]?.at_ms ?? T0,
    updated_at_ms: list.at(-1)?.at_ms ?? T0,
    ...extra,
  };
}

function mission(id: number, tasks: MissionTask[], extra: Partial<Mission> = {}): Mission {
  return {
    id,
    text: "Supply PA: give every provider an AI business agent",
    origin: "human",
    sources: [],
    state: "active",
    summary: null,
    gate: "/gate/scenarios",
    accept: "gate",
    budget: null,
    baseline: null,
    halted: null,
    objectives: null,
    created_at_ms: T0,
    updated_at_ms: T0,
    tasks,
    ...extra,
  };
}

const phases = (bar: ReturnType<typeof taskBar>) =>
  bar!.segments.map((s) => [s.phase, (s.start - T0) / MIN, s.end === null ? null : (s.end - T0) / MIN, s.attempt]);

test("a landed task's records become one segment per phase", () => {
  const m = mission(6, []);
  const bar = taskBar(
    task(16, "landed", [
      ["assigned", 0],
      ["delivered", 1],
      ["reported", 7],
      ["gated", 9, "accepted"],
      ["landed", 9.5],
    ]),
    m,
  );
  assert.deepEqual(phases(bar), [
    ["queued", 0, 1, 1],
    ["working", 1, 7, 1],
    ["gate", 7, 9, 1],
    ["landing", 9, 9.5, 1],
  ]);
  assert.equal(bar!.outcome, "landed");
  assert.equal(bar!.end, T0 + 9.5 * MIN);
  assert.equal(bar!.label, "T16");
  assert.deepEqual(bar!.verdicts.map((v) => [v.verdict, v.sentBack]), [["accepted", false]]);
  assert.equal(bar!.stalled, false);
});

test("with accept = review, the supervisor reviews before the gate", () => {
  const bar = taskBar(
    task(3, "landed", [
      ["assigned", 0],
      ["delivered", 1],
      ["reported", 5],
      ["reviewed", 6, "accepted"],
      ["gated", 8, "accepted"],
      ["landed", 9],
    ]),
    mission(1, [], { accept: "review" }),
  );
  assert.deepEqual(
    bar!.segments.map((s) => s.phase),
    ["queued", "working", "review", "gate", "landing"],
  );
  assert.deepEqual(bar!.verdicts.map((v) => [v.by, v.verdict]), [
    ["reviewed", "accepted"],
    ["gated", "accepted"],
  ]);
});

test("a task without a gate ends when the supervisor accepts it", () => {
  const bar = taskBar(
    task(2, "accepted", [
      ["assigned", 0],
      ["delivered", 1],
      ["reported", 4],
      ["reviewed", 5, "accepted"],
    ]),
    mission(1, [], { gate: null, accept: null }),
  );
  assert.equal(bar!.outcome, "accepted");
  assert.equal(bar!.end, T0 + 5 * MIN);
  assert.deepEqual(bar!.segments.map((s) => s.phase), ["queued", "working", "review"]);
});

test("rejected, then conflict, then accepted: three attempts with two returns", () => {
  const reasons = ["regression: check-e2e.sh fails", "conflicts with main", "landing e8dabad"];
  const bar = taskBar(
    task(13, "landed", [
      ["assigned", 0],
      ["delivered", 1],
      ["reported", 20],
      ["gated", 25, "conflict"],
      ["delivered", 30],
      ["reported", 50],
      ["gated", 55, "rejected"],
      ["delivered", 60],
      ["reported", 80],
      ["gated", 84, "accepted"],
      ["landed", 85],
    ]),
    mission(4, []),
    reasons,
  );
  assert.equal(bar!.attempts, 3);
  assert.deepEqual(phases(bar), [
    ["queued", 0, 1, 1],
    ["working", 1, 20, 1],
    ["gate", 20, 25, 1],
    ["revise", 25, 30, 1],
    ["working", 30, 50, 2],
    ["gate", 50, 55, 2],
    ["revise", 55, 60, 2],
    ["working", 60, 80, 3],
    ["gate", 80, 84, 3],
    ["landing", 84, 85, 3],
  ]);
  assert.deepEqual(
    bar!.verdicts.map((v) => [v.verdict, v.attempt, v.sentBack, v.reason]),
    [
      ["conflict", 1, true, reasons[0]],
      ["rejected", 2, true, reasons[1]],
      ["accepted", 3, false, reasons[2]],
    ],
  );
  // Each return goes from the verdict back to the start of its attempt.
  assert.deepEqual(
    bar!.returns.map((r) => [(r.from - T0) / MIN, (r.to - T0) / MIN, r.attempt]),
    [
      [25, 1, 1],
      [55, 30, 2],
    ],
  );
});

test("sent back by the supervisor's review", () => {
  const bar = taskBar(
    task(5, "reported", [
      ["assigned", 0],
      ["delivered", 1],
      ["reported", 3],
      ["reviewed", 4, "rejected"],
      ["delivered", 6],
      ["reported", 9],
    ]),
    mission(2, [], { gate: null, accept: null }),
  );
  assert.equal(bar!.attempts, 2);
  assert.deepEqual(bar!.verdicts.map((v) => [v.by, v.verdict, v.sentBack]), [["reviewed", "rejected", true]]);
  assert.equal(bar!.returns.length, 1);
  assert.equal(bar!.outcome, "open");
  assert.deepEqual(bar!.segments.at(-1), { phase: "review", start: T0 + 9 * MIN, end: null, attempt: 2 });
});

test("stale and aborted verdicts keep the task at the gate", () => {
  const bar = taskBar(
    task(7, "landing", [
      ["assigned", 0],
      ["delivered", 1],
      ["reported", 2],
      ["gated", 3, "stale"],
      ["gated", 4, "aborted"],
      ["gated", 5, "accepted"],
    ]),
    mission(1, []),
  );
  assert.deepEqual(bar!.segments.map((s) => s.phase), ["queued", "working", "gate", "gate", "gate", "landing"]);
  assert.equal(bar!.returns.length, 0);
  assert.equal(bar!.attempts, 1);
  assert.equal(bar!.outcome, "open");
  assert.equal(bar!.end, null);
});

test("the clock and the now line run only while a task is live", () => {
  const working = task(20, "delivered", [
    ["assigned", 0],
    ["delivered", 2],
  ]);
  const now = T0 + 30 * MIN;
  const running = deriveMissionTimeline([mission(1, [working])], now);
  assert.equal(running.openCount, 1);
  assert.equal(isLive(running), true);

  // A halted mission counts its open task for the axis, but nothing moves:
  // the marker would otherwise stay where the page loaded.
  const halted = deriveMissionTimeline(
    [mission(1, [working], { halted: "3 gate rejections in a row" })],
    now,
  );
  assert.equal(halted.openCount, 1);
  assert.equal(halted.lanes[0].bars[0].stalled, true);
  assert.equal(isLive(halted), false);
});

test("a mission without tasks has no bar to draw", () => {
  const taskless = deriveMissionTimeline([mission(1, [])], T0);
  assert.equal(hasBars(taskless), false);
  assert.equal(taskless.openCount, 0);

  const working = deriveMissionTimeline(
    [mission(1, [task(20, "delivered", [["assigned", 0], ["delivered", 2]])])],
    T0 + 10 * MIN,
  );
  assert.equal(hasBars(working), true);
});

test("an open task runs to now; a halted mission's open task is stalled", () => {
  const working = task(20, "delivered", [
    ["assigned", 0],
    ["delivered", 2],
  ]);
  const live = taskBar(working, mission(1, []));
  assert.equal(live!.end, null);
  assert.equal(live!.stalled, false);
  assert.deepEqual(live!.segments.at(-1), { phase: "working", start: T0 + 2 * MIN, end: null, attempt: 1 });
  const halted = taskBar(working, mission(1, [], { halted: "3 gate rejections in a row" }));
  assert.equal(halted!.stalled, true);
  assert.equal(halted!.end, null);

  const now = T0 + 30 * MIN;
  assert.equal(phaseAt(live!, T0 + 10 * MIN, now)?.phase, "working");
  assert.equal(phaseAt(live!, T0 + 40 * MIN, now), null);
});

test("cancelled tasks: by a record and by over-budget abort", () => {
  const byRecord = taskBar(
    task(8, "cancelled", [
      ["assigned", 0],
      ["delivered", 1],
      ["cancelled", 3],
    ]),
    mission(1, []),
  );
  assert.equal(byRecord!.outcome, "cancelled");
  assert.equal(byRecord!.end, T0 + 3 * MIN);
  assert.deepEqual(byRecord!.segments.map((s) => s.phase), ["queued", "working"]);

  const aborted = taskBar(
    task(9, "cancelled", [
      ["assigned", 0],
      ["delivered", 1],
      ["aborted", 46],
    ]),
    mission(1, []),
  );
  assert.equal(aborted!.outcome, "aborted");
});

test("records after the end are ignored, and clocks never run backwards", () => {
  const bar = taskBar(
    task(11, "landed", [
      ["assigned", 5],
      ["delivered", 4],
      ["landed", 6],
      ["gated", 7, "accepted"],
    ]),
    mission(1, []),
  );
  assert.deepEqual(phases(bar), [
    ["queued", 5, 5, 1],
    ["working", 5, 6, 1],
  ]);
  assert.equal(bar!.verdicts.length, 0);
});

test("a task from before the control stream is one segment by its state", () => {
  const legacy = taskBar(
    task(12, "landed", [], { created_at_ms: T0, updated_at_ms: T0 + 9 * MIN, revisions: 1 }),
    mission(1, []),
  );
  assert.equal(legacy!.outcome, "landed");
  assert.equal(legacy!.attempts, 2);
  assert.deepEqual(phases(legacy), [["working", 0, 9, 1]]);
  assert.equal(taskBar(task(13, "assigned", [], { created_at_ms: null }), mission(1, [])), null);
});

test("lanes: newest activity first, counts, state, and the halted mission", () => {
  const supply = mission(6, [
    task(16, "landed", [["assigned", 0], ["delivered", 1], ["reported", 5], ["gated", 6, "accepted"], ["landed", 7]]),
    task(17, "landed", [["assigned", 10], ["delivered", 11], ["reported", 15], ["gated", 16, "rejected"], ["delivered", 17], ["reported", 19], ["gated", 20, "accepted"], ["landed", 21]]),
  ]);
  const rsi = mission(4, [
    task(13, "revise", [["assigned", 30], ["delivered", 31], ["reported", 40], ["gated", 41, "rejected"]]),
  ], { text: "RSI", halted: "3 gate rejections in a row" });
  const old = mission(1, [
    task(2, "cancelled", [["assigned", -100], ["cancelled", -90]]),
  ], { state: "done", text: "An older mission that ran long ago and has a much longer description than fits" });
  const now = T0 + 50 * MIN;
  const timeline = deriveMissionTimeline([old, supply, rsi], now);
  assert.deepEqual(timeline.lanes.map((l) => [l.name, l.state]), [
    ["RSI", "halted"],
    ["Supply PA", "active"],
    ["An older mission that ran long ago and has a…", "done"],
  ]);
  assert.deepEqual(timeline.lanes[1].counts, { total: 2, landed: 2, open: 0, sentBack: 1, cancelled: 0 });
  assert.deepEqual(timeline.lanes[0].counts, { total: 1, landed: 0, open: 1, sentBack: 1, cancelled: 0 });
  assert.equal(timeline.lanes[0].halted, "3 gate rejections in a row");
  assert.equal(timeline.lanes[0].bars[0].stalled, true);
  assert.equal(timeline.openCount, 1);
  assert.equal(timeline.start, T0 - 100 * MIN);
  assert.equal(timeline.end, now);
});

test("filters: by mission keeps the lane; by state drops lanes left empty", () => {
  const a = mission(6, [
    task(16, "landed", [["assigned", 0], ["landed", 7]]),
    task(17, "delivered", [["assigned", 8], ["delivered", 9]]),
  ]);
  const b = mission(4, [], { text: "Empty" });
  const { lanes } = deriveMissionTimeline([a, b], T0 + 20 * MIN);
  assert.deepEqual(filterLanes(lanes, null, "all").map((l) => l.bars.length), [2, 0]);
  assert.deepEqual(filterLanes(lanes, 4, "all").map((l) => l.name), ["Empty"]);
  const open = filterLanes(lanes, null, "open");
  assert.deepEqual(open.map((l) => l.bars.map((bar) => bar.label)), [["T17"]]);
  assert.deepEqual(filterLanes(lanes, null, "cancelled"), []);
});

test("names and states", () => {
  assert.equal(missionName({ id: 6, text: "Supply PA: give every provider…" }), "Supply PA");
  assert.equal(missionName({ id: 4, text: "Make the gate faster. Then more." }), "Make the gate faster");
  assert.equal(missionName({ id: 9, text: null }), "Mission 9");
  assert.equal(laneState({ state: "dropped", halted: null }), "closed");
  assert.equal(laneState({ state: "done", halted: "x" }), "halted");
});

// `node --test test/` (Node strips the types itself). The Show view's
// model: which path each event travels, who is on stage, the live numbers.
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  LINGER_MS,
  MAX_MARKS,
  admitMarks,
  applyEvent,
  dueEvents,
  effectsOf,
  emptyStage,
  eventsPerMinute,
  gatePassRate,
  landedToday,
  landings,
  prune,
  replayTime,
  runningCount,
  stageFromFleet,
  startOfDay,
  type LogEvent,
} from "../src/show.ts";

let next = 1;
function event(name: string, fields: Partial<LogEvent> = {}): LogEvent {
  return {
    id: next++,
    agent_id: "agent_worker_a1",
    session_id: "session_worker_a1",
    execution_id: "exe_1",
    source: "bridge",
    event_name: name,
    detail: null,
    recorded_at_ms: 1_000_000,
    ...fields,
  };
}

function control(record: Record<string, unknown>, at = 1_000_000): LogEvent {
  return event("control.recorded", {
    agent_id: "agent_control",
    session_id: "session_control",
    execution_id: null,
    source: "control",
    detail: { record },
    recorded_at_ms: at,
  });
}

test("every event travels to the log from whoever wrote it", () => {
  for (const name of ["message.received", "screen.observed", "tool.started", "execution.ended"]) {
    assert.deepEqual(effectsOf(event(name))[0], {
      kind: "travel",
      route: ["agent:agent_worker_a1", "log"],
      tone: "trace",
    });
  }
  assert.deepEqual(effectsOf(control({ type: "task.assigned", agent_id: "agent_worker_b" }))[0].route, [
    "dispatcher",
    "log",
  ]);
  assert.deepEqual(effectsOf(control({ type: "task.gated", verdict: "accepted" }))[0].route, [
    "integrator",
    "log",
  ]);
  assert.deepEqual(effectsOf(control({ type: "mission.configured" }))[0].route, ["supervisor", "log"]);
});

test("the integrator records the outcome of the owner's landing; the owner's own move is not its", () => {
  const route = (record: Record<string, unknown>) => effectsOf(control(record))[0].route;
  assert.deepEqual(route({ type: "source.moved", request: 7, from: "a", to: "b" }), ["integrator", "log"]);
  assert.deepEqual(route({ type: "source.push_refused", request: 7, reason: "conflict" }), [
    "integrator",
    "log",
  ]);
  assert.deepEqual(route({ type: "source.moved", from: "a", to: "b" }), ["supervisor", "log"]);
  assert.deepEqual(route({ type: "source.push_requested", commit: "c" }), ["supervisor", "log"]);
});

test("each kind of work takes its own path", () => {
  const rest = (e: LogEvent) => effectsOf(e).slice(1);
  assert.deepEqual(rest(event("artifact.committed")), [
    { kind: "travel", route: ["agent:agent_worker_a1", "workspace"], tone: "commit" },
  ]);
  assert.deepEqual(rest(event("command.accepted")), [
    { kind: "travel", route: ["supervisor", "gateway", "agent:agent_worker_a1"], tone: "command" },
  ]);
  assert.deepEqual(rest(event("tool.started")), [{ kind: "spark", node: "agent:agent_worker_a1" }]);
  assert.deepEqual(rest(control({ type: "task.assigned", agent_id: "agent_worker_b" })), [
    { kind: "travel", route: ["dispatcher", "agent:agent_worker_b"], tone: "task" },
  ]);
  assert.deepEqual(rest(control({ type: "task.landed", id: 7, to: "abc" })), [
    { kind: "travel", route: ["workspace", "sources", "github"], tone: "landing" },
  ]);
  assert.deepEqual(rest(event("message.received")), []);
  assert.deepEqual(rest(control({ type: "task.reported" })), []);
});

test("a gate verdict pulses at the gate, accepted or not", () => {
  for (const [verdict, accepted] of [
    ["accepted", true],
    ["rejected", false],
    ["conflict", false],
  ] as const) {
    assert.deepEqual(effectsOf(control({ type: "task.gated", verdict })).slice(1), [
      { kind: "travel", route: ["integrator", "gate"], tone: "task" },
      { kind: "verdict", node: "gate", verdict, accepted },
    ]);
  }
});

test("the supervisor and the residents always stand; a worker comes and goes", () => {
  let stage = emptyStage();
  assert.deepEqual(Object.keys(stage.agents).sort(), [
    "agent_resident_rsi",
    "agent_resident_supply",
    "agent_supervisor",
  ]);
  assert.equal(runningCount(stage), 0);

  stage = applyEvent(stage, event("execution.started", { recorded_at_ms: 1000 }));
  const worker = stage.agents.agent_worker_a1;
  assert.equal(worker.running, true);
  assert.equal(worker.slot, 0);
  assert.equal(worker.label, "a1");
  assert.equal(runningCount(stage), 1);

  const second = applyEvent(stage, event("execution.started", { agent_id: "agent_worker_b2", execution_id: "exe_2" }));
  assert.equal(second.agents.agent_worker_b2.slot, 1, "the next free slot");

  // An end of another execution of the agent leaves the running one alone.
  assert.equal(applyEvent(stage, event("execution.ended", { execution_id: "exe_0" })).agents.agent_worker_a1.running, true);

  stage = applyEvent(stage, event("execution.ended", { recorded_at_ms: 5000 }));
  assert.equal(stage.agents.agent_worker_a1.running, false);
  assert.equal(prune(stage, 5000 + LINGER_MS), stage, "it lingers");
  const after = prune(stage, 5001 + LINGER_MS);
  assert.equal(after.agents.agent_worker_a1, undefined, "then leaves");
  assert.ok(after.agents.agent_resident_rsi, "residents never leave");

  const resident = applyEvent(emptyStage(), event("execution.ended", { agent_id: "agent_resident_rsi", recorded_at_ms: 0 }));
  assert.ok(prune(resident, 10 * LINGER_MS).agents.agent_resident_rsi);
});

test("a freed slot is taken again, and control events put no one on stage", () => {
  let stage = applyEvent(emptyStage(), event("execution.started"));
  stage = applyEvent(stage, event("execution.started", { agent_id: "agent_worker_b2", execution_id: "exe_2" }));
  stage = prune(applyEvent(stage, event("execution.ended", { recorded_at_ms: 0 })), LINGER_MS + 1);
  stage = applyEvent(stage, event("execution.started", { agent_id: "agent_worker_c3", execution_id: "exe_3" }));
  assert.equal(stage.agents.agent_worker_c3.slot, 0);
  assert.equal(applyEvent(stage, control({ type: "task.gated", verdict: "accepted" })), stage);
});

test("an engine seen working without its start is running", () => {
  const stage = applyEvent(emptyStage(), event("tool.started", { agent_id: "agent_resident_rsi" }));
  assert.equal(stage.agents.agent_resident_rsi.running, true);
  assert.equal(stage.agents.agent_resident_rsi.slot, -1);
});

test("the fleet's live agents start the live stage running", () => {
  const stage = stageFromFleet([
    { agent_id: "agent_supervisor", live: true, execution_id: "exe_s" },
    { agent_id: "agent_worker_x", live: true, execution_id: "exe_x" },
    { agent_id: "agent_worker_gone", live: false, execution_id: null },
    { agent_id: "agent_resident_core", live: false, execution_id: null },
  ]);
  assert.equal(runningCount(stage), 2);
  assert.equal(stage.agents.agent_worker_gone, undefined);
  assert.equal(stage.agents.agent_resident_core.running, false, "a resident stands even when idle");
});

test("events per minute counts the last minute only", () => {
  const now = 10 * 60_000;
  const events = [now - 60_000, now - 59_999, now - 1, now, now + 1].map((at) => event("tool.started", { recorded_at_ms: at }));
  assert.equal(eventsPerMinute(events, now), 3);
});

test("landed today counts landings since local midnight", () => {
  const now = new Date(2026, 10, 5, 15, 0).getTime();
  const midnight = startOfDay(now);
  assert.equal(midnight, new Date(2026, 10, 5).getTime());
  const events = [
    control({ type: "task.landed" }, midnight - 1),
    control({ type: "task.landed" }, midnight),
    control({ type: "task.landed" }, now - 1),
    control({ type: "task.gated", verdict: "accepted" }, now - 1),
    control({ type: "task.landed" }, now + 1),
  ];
  assert.equal(landedToday(events, now), 2);
});

test("landings are known by task, up to now", () => {
  const events = [
    control({ type: "task.landed", id: 3 }, 100),
    control({ type: "task.landed", id: 4 }, 200),
    control({ type: "task.gated", id: 5, verdict: "accepted" }, 150),
  ];
  assert.deepEqual([...landings(events, 150)], [[3, 100]]);
  assert.deepEqual([...landings(events, 300)], [[3, 100], [4, 200]]);
});

test("the gate pass rate counts only verdicts that judge the work", () => {
  assert.equal(gatePassRate([], 0), null);
  const verdicts = ["accepted", "rejected", "accepted", "conflict", "stale", "aborted"];
  const events = verdicts.map((verdict, i) => control({ type: "task.gated", verdict }, i));
  assert.equal(gatePassRate(events, 100), 2 / 4);
  assert.equal(gatePassRate(events, 0), 1, "up to now only");
});

test("replay runs scene time faster and hands over each event once", () => {
  assert.equal(replayTime(1000, 500, 60), 31_000);
  const events = [1, 2, 3, 4].map((at) => event("tool.started", { recorded_at_ms: at * 1000 }));
  const first = dueEvents(events, 0, 2000);
  const second = dueEvents(events, 2000, 4000);
  assert.deepEqual([...first, ...second].map((e) => e.recorded_at_ms), [1000, 2000, 3000, 4000]);
});

test("a burst of marks thins out, traces first", () => {
  const full = Array.from({ length: MAX_MARKS - 2 }, () => ({ tone: "trace" as const }));
  const added = [{ tone: "trace" as const }, { tone: "commit" as const }, { tone: "landing" as const }];
  const admitted = admitMarks(full, added);
  assert.equal(admitted.length, MAX_MARKS);
  assert.deepEqual(admitted.slice(-2).map((m) => m.tone), ["commit", "landing"]);
  assert.equal(admitMarks(admitted, added).length, MAX_MARKS);
});

import assert from "node:assert/strict";
import { test } from "node:test";

import type { FleetAgent } from "../src/fleet.ts";
import type { Mission, MissionTask, Missions, Resident, TaskStage } from "../src/missions.ts";
import type { ScorecardScope } from "../src/scorecard.ts";
import { absoluteTitle, localDateTime, utcDateTime } from "../src/format.ts";
import {
  attemptOutcome,
  fleetContext,
  groupAgents,
  groupTasks,
  heldTask,
  isQuietScope,
  lastTask,
  missingAlwaysOn,
  missionClass,
  orderMissions,
  orderScopes,
  splitId,
  taskSection,
  taskStateLabel,
  taskTitle,
  usualEngine,
} from "../src/ordering.ts";

const T0 = 1_793_900_000_000;
const MIN = 60_000;

function stages(rows: Array<[string, number, string?]>): TaskStage[] {
  return rows.map(([stage, at, verdict], index) => ({
    stage,
    verdict: verdict ?? null,
    at_ms: T0 + at * MIN,
    took_ms: index === 0 ? null : (at - rows[index - 1][1]) * MIN,
  }));
}

function task(
  id: number,
  state: string,
  rows: Array<[string, number, string?]>,
  extra: Partial<MissionTask> = {},
): MissionTask {
  const list = stages(rows);
  return {
    id,
    kind: "ephemeral",
    agent_id: `agent_worker_objective_${id}`,
    session_id: `session_worker_objective_${id}`,
    brief: `Objective ${extra.objective ?? id}: Task ${extra.objective ?? id}\n\nDo the thing for ${id}.`,
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
    text: `Mission ${id}: something`,
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
    created_at_ms: T0 - 1000 * MIN,
    updated_at_ms: T0 - 1000 * MIN,
    tasks,
    ...extra,
  };
}

const ids = <T>(sections: ReturnType<typeof groupTasks<T>>, taskOf: (item: T) => MissionTask) =>
  sections.map((section) => [
    section.section,
    section.groups.map((group) => [
      taskOf(group.item).id,
      ...group.earlier.map((attempt) => `${attempt.task.id}: ${attempt.outcome}`),
    ]),
  ]);

// ---------------------------------------------------------------------------
// Tasks

test("a task's section follows its state; accepted waits for the gate only in a gated mission", () => {
  const gated = { gate: "/g" };
  const ungated = { gate: null };
  const state = (value: string) => ({ state: value as MissionTask["state"] });
  assert.equal(taskSection(state("assigned"), gated), "in_flight");
  assert.equal(taskSection(state("delivered"), gated), "in_flight");
  assert.equal(taskSection(state("reported"), gated), "in_flight");
  assert.equal(taskSection(state("landing"), gated), "in_flight");
  assert.equal(taskSection(state("accepted"), gated), "in_flight");
  assert.equal(taskSection(state("accepted"), ungated), "landed");
  assert.equal(taskSection(state("landed"), gated), "landed");
  assert.equal(taskSection(state("revise"), gated), "sent_back");
  assert.equal(taskSection(state("cancelled"), gated), "cancelled");
  assert.equal(taskSection(state("unknown"), gated), "in_flight");
});

test("state labels and attempt outcomes speak an observer's words", () => {
  const gate = { gate: "/g", accept: "gate" };
  const review = { gate: "/g", accept: "review" };
  const plain = { gate: null, accept: "review" };
  const budget = task(84, "cancelled", [["assigned", 0], ["delivered", 1], ["aborted", 150]]);
  assert.equal(taskStateLabel(budget, gate), "aborted at budget");
  assert.equal(attemptOutcome(budget, gate), "aborted at budget");
  const reported = task(85, "reported", [["assigned", 0], ["delivered", 1], ["reported", 9]]);
  assert.equal(taskStateLabel(reported, gate), "gating");
  assert.equal(taskStateLabel(reported, review), "in review");
  assert.equal(taskStateLabel({ ...reported, state: "accepted" }, review), "gating");
  assert.equal(taskStateLabel({ ...reported, state: "accepted" }, plain), "accepted");
  const rejected = task(24, "cancelled", [
    ["assigned", 0], ["delivered", 1], ["reported", 5], ["gated", 6, "rejected"], ["delivered", 7], ["cancelled", 9],
  ]);
  assert.equal(attemptOutcome(rejected, gate), "rejected, then cancelled");
  const conflict = task(30, "revise", [["assigned", 0], ["delivered", 1], ["reported", 5], ["gated", 6, "conflict"]]);
  assert.equal(attemptOutcome(conflict, gate), "conflict");
  const sentBack = task(31, "revise", [["assigned", 0], ["delivered", 1], ["reported", 5], ["reviewed", 6, "rejected"]]);
  assert.equal(attemptOutcome(sentBack, plain), "rejected");
  assert.equal(attemptOutcome(task(32, "cancelled", [["assigned", 0], ["cancelled", 1]]), gate), "cancelled");
});

test("a task's title: the objective's title, else the first line of its text", () => {
  assert.equal(
    taskTitle({ brief: "Objective 17: A scorecard of every dimension at once, from the record\n\nRead…" }),
    "A scorecard of every dimension at once, from the record",
  );
  // `Task 42` is the integrator's stand-in, not a title: the text's first line is.
  assert.equal(
    taskTitle({ brief: "Objective 42: Task 42\n\nImplement the protocol's version and its conformance suite." }),
    "Implement the protocol's version and its conformance suite.",
  );
  assert.equal(taskTitle({ brief: "Objective 42: Task 42" }), "Objective 42: Task 42");
  assert.equal(
    taskTitle({ brief: "Owner work on Spawn: fix four open GitHub issues" }, { missionName: "Owner work on Spawn" }),
    "Fix four open GitHub issues",
  );
  assert.equal(taskTitle({ brief: "## Goal: add a read-only Missions view" }), "Add a read-only Missions view");
  assert.equal(
    taskTitle(
      { brief: "Owner work on Spawn (protected files: you may edit rsi/backlog/028.json only). Goal: record the owner's rulings." },
      { missionName: "Owner work on Spawn" },
    ),
    "Record the owner's rulings.",
  );
  assert.equal(
    taskTitle({ brief: "Owner-protected work on Supply PA (you may edit vitest.config.ts). The gate rejected task 38." }),
    "The gate rejected task 38.",
  );
  assert.equal(taskTitle({ brief: "Owner work on Spawn:" }, { missionName: "Owner work on Spawn" }), "Owner work on Spawn:");
  assert.equal(taskTitle({ brief: "x" }, { objectiveTitle: "Land an agent's work from its pushed branch" }), "Land an agent's work from its pushed branch");
  assert.equal(taskTitle({ brief: "Objective 3: Real title" }, { objectiveTitle: "Task 3" }), "Real title");
  assert.equal(taskTitle({ brief: null }), null);
  assert.equal(taskTitle({ brief: "\n\n" }), null);
  const long = taskTitle({ brief: "word ".repeat(60) }, { max: 40 })!;
  assert.ok(long.length <= 40, long);
  assert.ok(long.endsWith("…"));
  assert.ok(!long.includes("  "));
});

test("retries of one objective are one group: the latest attempt is the row (mission 4's objectives 6 and 17)", () => {
  const m = mission(4, [
    task(13, "landed", [["assigned", 0], ["delivered", 1], ["reported", 30], ["gated", 31, "accepted"], ["landed", 32]], { objective: 3 }),
    task(22, "cancelled", [["assigned", 40], ["delivered", 41], ["reported", 50], ["gated", 51, "conflict"], ["delivered", 52], ["aborted", 90]], { objective: 6 }),
    task(23, "cancelled", [["assigned", 95], ["delivered", 96], ["cancelled", 99]], { objective: 6 }),
    task(84, "cancelled", [["assigned", 100], ["delivered", 101], ["aborted", 250]], { objective: 17 }),
    task(85, "reported", [["assigned", 110], ["delivered", 111], ["reported", 200]], { objective: 18 }),
    task(87, "landed", [["assigned", 260], ["delivered", 261], ["reported", 300], ["gated", 301, "accepted"], ["landed", 302]], { objective: 17 }),
    task(88, "reported", [["assigned", 120], ["delivered", 121], ["reported", 280]], { objective: 19 }),
    task(94, "reported", [["assigned", 270], ["delivered", 271], ["reported", 310]], { objective: 20 }),
  ]);
  const sections = groupTasks(m.tasks, (t) => t, m);
  assert.deepEqual(ids(sections, (t) => t), [
    // In flight: oldest first, so the longest-running is on top.
    ["in_flight", [[85], [88], [94]]],
    // Landed: newest first; the budget abort nests under the redo.
    ["landed", [[87, "84: aborted at budget"], [13]]],
    ["cancelled", [[23, "22: aborted at budget"]]],
  ]);
});

test("a sent-back task that was redone and landed nests under the redo, not under Sent back (mission 6)", () => {
  const m = mission(6, [
    task(24, "revise", [["assigned", 0], ["delivered", 1], ["reported", 5], ["gated", 6, "rejected"]], { objective: 7 }),
    task(26, "landed", [["assigned", 10], ["delivered", 11], ["reported", 15], ["gated", 16, "accepted"], ["landed", 17]], { objective: 7 }),
    task(31, "revise", [["assigned", 20], ["delivered", 21], ["reported", 25], ["gated", 26, "conflict"]], { objective: 12 }),
    task(33, "revise", [["assigned", 30], ["delivered", 31], ["reported", 35], ["gated", 36, "rejected"]], { objective: 13 }),
    task(99, "cancelled", [["assigned", 40], ["delivered", 41], ["aborted", 86]], { objective: 42 }),
    task(101, "delivered", [["assigned", 90], ["delivered", 91]], { objective: 42 }),
  ]);
  assert.deepEqual(ids(groupTasks(m.tasks, (t) => t, m), (t) => t), [
    ["in_flight", [[101, "99: aborted at budget"]]],
    ["landed", [[26, "24: rejected"]]],
    // Sent back: newest first.
    ["sent_back", [[33], [31]]],
  ]);
});

test("overlapping attempts: an open attempt is the row, never nested under a closed one", () => {
  const m = mission(6, [
    task(1, "delivered", [["assigned", 1], ["delivered", 2]], { objective: 5 }),
    task(2, "cancelled", [["assigned", 3], ["cancelled", 4]], { objective: 5 }),
  ]);
  assert.deepEqual(ids(groupTasks(m.tasks, (t) => t, m), (t) => t), [
    ["in_flight", [[1, "2: cancelled"]]],
  ]);
});

test("tasks without an objective are their own rows; items may be anything that carries a task", () => {
  const m = mission(7, [
    task(92, "delivered", [["assigned", 50], ["delivered", 51]]),
    task(97, "delivered", [["assigned", 10], ["delivered", 11]]),
    task(96, "accepted", [["assigned", 0], ["delivered", 1], ["reported", 5], ["reviewed", 6, "accepted"]]),
  ], { gate: null, accept: "review" });
  const bars = m.tasks.map((t) => ({ key: `bar:${t.id}`, task: t }));
  const sections = groupTasks(bars, (bar) => bar.task, m);
  assert.deepEqual(ids(sections, (bar) => bar.task), [
    ["in_flight", [[97], [92]]],
    ["landed", [[96]]],
  ]);
  assert.equal(sections[0].groups[0].item.key, "bar:97");
});

// ---------------------------------------------------------------------------
// Missions

test("missions: open ones first, then idle active ones, then closed; each by its latest activity", () => {
  const openOld = mission(4, [task(85, "reported", [["assigned", 0], ["delivered", 1], ["reported", 10]])]);
  const openNew = mission(6, [task(101, "delivered", [["assigned", 50], ["delivered", 51]])]);
  const sentBackOnly = mission(9, [task(40, "revise", [["assigned", 5], ["gated", 6, "rejected"]])], { halted: "3 rejections" });
  const idle = mission(8, [task(83, "accepted", [["assigned", 300], ["reviewed", 301, "accepted"]])], { gate: null });
  const done = mission(5, [task(12, "landed", [["assigned", 400], ["landed", 401]])], { state: "done" });
  const cancelled = mission(2, [], { state: "cancelled", updated_at_ms: T0 + 500 * MIN });
  assert.equal(missionClass(openOld), "open");
  assert.equal(missionClass(sentBackOnly), "open");
  assert.equal(missionClass(idle), "idle");
  assert.equal(missionClass(done), "closed");
  assert.deepEqual(
    orderMissions([done, idle, openOld, cancelled, sentBackOnly, openNew]).map((m) => m.id),
    [6, 4, 9, 8, 2, 5],
  );
});

// ---------------------------------------------------------------------------
// Agents

function agent(agentId: string, extra: Partial<FleetAgent> = {}, lastEvent: number | null = null): FleetAgent {
  return {
    session_id: agentId.replace(/^agent_/, "session_"),
    agent_id: agentId,
    role: "worker",
    role_rank: 1,
    engine: "claude",
    live: true,
    execution_id: null,
    started_at_ms: T0,
    ended_at_ms: null,
    state: "WORKING",
    state_since_ms: T0,
    attention: false,
    judged_at_ms: null,
    task: null,
    activity: {
      current_tool: null,
      current_tool_summary: null,
      last_event_name: null,
      turn_open: null,
      turn_at_ms: null,
      last_event_at_ms: lastEvent === null ? null : T0 + lastEvent * MIN,
      event_count: null,
      tool_calls: null,
      executions: null,
    },
    judgement: null,
    ...extra,
  };
}

function resident(name: string, state: Resident["state"] = "approved"): Resident {
  return {
    name,
    agent_id: `agent_resident_${name}`,
    session_id: `session_resident_${name}`,
    charter: null,
    state,
    raw_state: state,
    proposed_by: "human",
    approved_by: "human",
    created_at_ms: T0,
    updated_at_ms: T0,
  };
}

function fleetMissions(): Missions {
  const supply = mission(6, [
    task(99, "cancelled", [["assigned", 0], ["delivered", 1], ["aborted", 40]], { objective: 42 }),
    task(101, "delivered", [["assigned", 50], ["delivered", 51]], {
      objective: 42,
      agent_id: "agent_resident_supply",
      session_id: "session_resident_supply",
    }),
    task(94, "reported", [["assigned", 10], ["delivered", 11], ["reported", 30]], { objective: 20 }),
    task(85, "reported", [["assigned", 5], ["delivered", 6], ["reported", 20]], { objective: 18 }),
    task(82, "landed", [["assigned", 2], ["landed", 9]], { objective: 36 }),
    task(102, "assigned", [["assigned", 60]], { session_id: "session_worker_objective_100", agent_id: "agent_worker_objective_100" }),
    task(100, "delivered", [["assigned", 20], ["delivered", 21]]),
  ]);
  return { missions: [supply], residents: [resident("supply"), resident("rsi"), resident("old", "retired")] };
}

test("agents group as always on, on a task, idle, ended — every list uses this order", () => {
  const context = fleetContext(fleetMissions());
  const agents = [
    agent("agent_worker_objective_82", { live: false, ended_at_ms: T0 + 9 * MIN, state: "UNKNOWN" }, 9),
    agent("agent_worker_objective_85", { attention: true, state: "BLOCKED" }, 20),
    agent("agent_worker_objective_94", {}, 30),
    agent("agent_worker_objective_100", {}, 10),
    agent("agent_resident_supply", {}, 52),
    agent("agent_resident_rsi", { state: "COMPLETED" }, 40),
    agent("agent_resident_old", { live: false, ended_at_ms: T0 }, 0),
    agent("agent_supervisor", { role: "supervisor", role_rank: 0 }, 55),
    agent("agent_rsi", { role: "rsi", live: false, ended_at_ms: T0 - 3000 * MIN }),
    agent("agent_worker_idle_7", {}, 45),
    agent("agent_worker_idle_8", { attention: true }, 50),
    agent("agent_worker_objective_99", { live: false, ended_at_ms: T0 + 40 * MIN }, 40),
  ];
  const groups = groupAgents(agents, context);
  assert.deepEqual(
    groups.map((group) => [group.id, group.agents.map((a) => a.agent_id)]),
    [
      ["always_on", ["agent_supervisor", "agent_resident_rsi", "agent_resident_supply"]],
      // Attention first, then the stalest (the longest since its last event).
      ["on_task", ["agent_worker_objective_85", "agent_worker_objective_100", "agent_worker_objective_94"]],
      ["idle", ["agent_worker_idle_8", "agent_worker_idle_7"]],
      // Newest end first; a retired resident is an ordinary agent.
      ["ended", ["agent_worker_objective_99", "agent_worker_objective_82", "agent_resident_old", "agent_rsi"]],
    ],
  );
  // The resident holds T101; worker 100 holds its delivered T100, not the queued T102.
  assert.equal(heldTask(agents[4], context)?.task_id, 101);
  assert.equal(heldTask(agents[3], context)?.task_id, 100);
  assert.equal(heldTask(agents[2], context)?.label, "gating");
  assert.equal(heldTask(agents[2], context)?.title, "Do the thing for 94.");
  assert.equal(lastTask(agents[0], context)?.label, "landed");
  assert.equal(lastTask(agents[11], context)?.label, "aborted at budget");
});

test("a worker whose engine ended while it holds an open task stays on a task", () => {
  const context = fleetContext(fleetMissions());
  const lost = agent("agent_worker_objective_94", { live: false, ended_at_ms: T0 + 31 * MIN, state: "UNKNOWN" }, 31);
  const [, onTask, , ended] = groupAgents([lost], context);
  assert.deepEqual(onTask.agents.map((a) => a.agent_id), ["agent_worker_objective_94"]);
  assert.equal(ended.agents.length, 0);
});

test("approved residents and the supervisor without a session are missing, not silently absent", () => {
  const context = fleetContext(fleetMissions());
  assert.equal(context.known, true);
  assert.deepEqual(
    missingAlwaysOn([agent("agent_resident_supply"), agent("agent_worker_x_1")], context),
    [
      { role: "supervisor", name: "supervisor" },
      { role: "resident", name: "rsi" },
    ],
  );
  assert.deepEqual(
    missingAlwaysOn([agent("agent_supervisor", { role: "supervisor" }), agent("agent_resident_rsi"), agent("agent_resident_supply")], context),
    [],
  );
});

test("before /api/missions answers, an agent_resident_ id is a resident", () => {
  const context = fleetContext(null);
  assert.equal(context.known, false);
  const groups = groupAgents(
    [agent("agent_worker_x_1"), agent("agent_resident_supply"), agent("agent_supervisor", { role: "supervisor" })],
    context,
  );
  assert.deepEqual(groups[0].agents.map((a) => a.agent_id), ["agent_supervisor", "agent_resident_supply"]);
  assert.deepEqual(groups[2].agents.map((a) => a.agent_id), ["agent_worker_x_1"]);
});

test("the usual engine is the most common one; ids split so their suffix stays visible", () => {
  assert.equal(usualEngine([{ engine: "claude" }, { engine: "codex" }, { engine: "claude" }, { engine: null }]), "claude");
  assert.equal(usualEngine([{ engine: null }]), null);
  assert.equal(usualEngine([{ engine: "b" }, { engine: "a" }]), "a");
  assert.deepEqual(splitId("agent_worker_forever_objectives_96"), { head: "agent_worker_forever_objectives", tail: "_96" });
  assert.deepEqual(splitId("session_worker_slice8_contract_100"), { head: "session_worker_slice8_contract", tail: "_100" });
  assert.deepEqual(splitId("agent_supervisor"), { head: "agent_supervisor", tail: "" });
  assert.deepEqual(splitId("agent_resident_rsi"), { head: "agent_resident", tail: "_rsi" });
  assert.deepEqual(splitId("plain"), { head: "plain", tail: "" });
  assert.deepEqual(splitId("trailing_"), { head: "trailing_", tail: "" });
});

// ---------------------------------------------------------------------------
// Scorecard and times

function scope(missionId: number | null, landed: number): ScorecardScope {
  const base = { value: null, n: landed, enough: false, better: "higher" as const, previous: null, trend: null };
  return {
    scope: missionId === null ? "fleet" : "mission",
    mission: missionId,
    days: [],
    dimensions: { throughput: { ...base, landed, open_hours: 1 } } as unknown as ScorecardScope["dimensions"],
  };
}

test("a mission scope where nothing happened is quiet; scopes follow the missions' order, quiet ones last", () => {
  assert.equal(isQuietScope(scope(1, 0)), true);
  assert.equal(isQuietScope(scope(4, 3)), false);
  assert.equal(isQuietScope(scope(null, 0)), false);
  // No landing, but the gate ran: its time and rework are data, not quiet.
  const rejected = scope(7, 0);
  (rejected.dimensions as unknown as Record<string, unknown>).gate_time = { n: 6, runs: 6 };
  assert.equal(isQuietScope(rejected), false);
  // A drop from landings in the previous window to none is news.
  const dropped = scope(8, 0);
  dropped.dimensions.throughput.previous = 1.5;
  assert.equal(isQuietScope(dropped), false);
  const missions = [
    mission(4, [task(85, "reported", [["assigned", 0]])]),
    mission(6, [task(101, "delivered", [["assigned", 50]])]),
    mission(1, [], { state: "done" }),
  ];
  const ordered = orderScopes([scope(1, 0), scope(4, 5), scope(null, 9), scope(6, 2), scope(3, 0)], missions);
  assert.deepEqual(ordered.map((s) => s.mission), [null, 6, 4, 1, 3]);
});

test("absolute times: local to the second, then UTC", () => {
  const at = Date.UTC(2026, 10, 6, 9, 37, 22, 513);
  assert.equal(utcDateTime(at), "2026-11-06 09:37:22 UTC");
  assert.match(localDateTime(at), /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  assert.equal(absoluteTitle(at), `${localDateTime(at, true)} local · 2026-11-06 09:37:22 UTC`);
  assert.equal(absoluteTitle(null), undefined);
  assert.equal(localDateTime(0), "—");
});

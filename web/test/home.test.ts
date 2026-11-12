import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import type { Alert } from "../src/api.ts";
import type { Attention } from "../src/attention.ts";
import type { FleetAgent, FleetHealth } from "../src/fleet.ts";
import {
  currentToolText,
  actionableAlerts,
  agentMatches,
  agentName,
  agentNames,
  agentSearchFields,
  agentsNow,
  alertMoot,
  alertStale,
  alertTitle,
  alertsSpanMs,
  awayFrom,
  digestBand,
  digestSince,
  digestToday,
  digestWindow,
  doingWords,
  latestNotes,
  allNotes,
  liveAlerts,
  missionsToday,
  conflictText,
  earlierConflictCount,
  needsSummary,
  needsYou,
  oneLine,
} from "../src/home.ts";
import type { Mission, Missions, MissionTask } from "../src/missions.ts";
import { fleetContext, heldTask } from "../src/ordering.ts";

const NOW = 1_794_224_000_000;
const MIN = 60_000;
const HOUR = 60 * MIN;
const GIB = 1024 ** 3;

function agent(session: string, fields: Partial<FleetAgent> = {}): FleetAgent {
  return {
    session_id: session,
    agent_id: session.replace("session_", "agent_"),
    role: "worker",
    role_rank: 2,
    engine: "claude",
    live: true,
    execution_id: "exec_1",
    started_at_ms: NOW - 2 * HOUR,
    ended_at_ms: null,
    state: "WAITING",
    state_since_ms: NOW - 10 * MIN,
    attention: false,
    judged_at_ms: null,
    task: null,
    activity: {
      current_tool: null,
      current_tool_summary: null,
      jobs: [],
      last_event_name: "screen.observed",
      turn_open: null,
      turn_at_ms: null,
      last_event_at_ms: NOW - MIN,
      event_count: 10,
      tool_calls: 3,
      executions: 1,
    },
    judgement: null,
    label: null,
    task_title: null,
    mission_name: null,
    ...fields,
  } as FleetAgent;
}

function task(id: number, state: MissionTask["state"], fields: Partial<MissionTask> = {}): MissionTask {
  return {
    id,
    kind: "objective",
    agent_id: null,
    session_id: null,
    brief: null,
    state,
    raw_state: state,
    revisions: 0,
    review_note: null,
    report: null,
    candidate: null,
    objective: null,
    metric: null,
    last_gate: null,
    stages: [],
    model: null,
    created_at_ms: NOW - 3 * HOUR,
    updated_at_ms: NOW - HOUR,
    label: null,
    task_title: `Task ${id}`,
    mission_name: null,
    ...fields,
  } as MissionTask;
}

function mission(id: number, name: string, tasks: MissionTask[], fields: Partial<Mission> = {}): Mission {
  return {
    id,
    text: name,
    mission_name: name,
    origin: null,
    sources: [],
    state: "active",
    summary: null,
    gate: "scripts/rsi/gate.sh",
    accept: "gate",
    budget: null,
    baseline: null,
    halted: null,
    objectives: null,
    hold: null,
    created_at_ms: NOW - 48 * HOUR,
    updated_at_ms: NOW - HOUR,
    tasks,
    ...fields,
  } as Mission;
}

function missions(list: Mission[], holds: Missions["holds"] = []): Missions {
  return { missions: list, residents: [], holds };
}

function alert(key: string, kind: string, subject: string, fields: Partial<Alert> = {}): Alert {
  return {
    key,
    kind,
    subject,
    state: "open",
    fired_event_id: 1,
    fired_at_ms: NOW - HOUR,
    evidence: {},
    cleared_event_id: null,
    cleared_at_ms: null,
    cleared_evidence: null,
    text: `${subject}: ${kind}`,
    ...fields,
  };
}

function attention(fields: Partial<Attention> = {}): Attention {
  return {
    generated_at_ms: NOW,
    pending: [],
    told: [],
    supervisor: null,
    on_own_jobs: [],
    holds: [],
    warnings: [],
    ...fields,
  } as Attention;
}

const HEALTHY = {
  disk: [{ label: "workspace", path: "/w", free_bytes: 150 * GIB, warn_bytes: 20 * GIB, floor_bytes: 2 * GIB }],
  memory: { available_bytes: 10 * GIB, total_bytes: 17 * GIB, warn_bytes: 4 * GIB },
} as unknown as FleetHealth;

const base = {
  attention: attention(),
  missions: missions([]),
  alerts: [] as Alert[],
  agents: [] as FleetAgent[],
  health: HEALTHY,
  missingAlwaysOn: 0,
  now: NOW,
};

test("a stuck alert about a session that has ended is moot; a live one or an unknown one is not", () => {
  const agents = [agent("session_a", { live: false }), agent("session_b")];
  assert.equal(alertMoot(alert("k1", "agent_stuck", "session_a"), agents), true);
  assert.equal(alertMoot(alert("k2", "agent_stuck", "session_b"), agents), false);
  assert.equal(alertMoot(alert("k3", "agent_stuck", "session_gone"), agents), false, "not guessed");
  assert.equal(alertMoot(alert("k4", "agent_stuck", "session_a"), null), false, "no fleet yet");
  assert.equal(alertMoot(alert("k5", "judge_down", "session_a"), agents), false, "only agent_stuck");
  const open = liveAlerts(
    [alert("k1", "agent_stuck", "session_a"), alert("k2", "agent_stuck", "session_b"), alert("k6", "x", "y", { state: "cleared" })],
    agents,
  );
  assert.deepEqual(open.map((entry) => entry.key), ["k2"]);
});

test("nothing needs you on a quiet day", () => {
  assert.deepEqual(needsYou(base), []);
});

test("a halted mission is one item to act on, with its resume command, and its halt alert is not said twice", () => {
  const items = needsYou({
    ...base,
    missions: missions([mission(6, "Supply PA", [], { halted: "3 gate rejections in a row" })]),
    alerts: [alert("mission_halted:9", "mission_halted", "mission #6")],
  });
  assert.equal(items.length, 1);
  assert.equal(items[0].tone, "act");
  assert.equal(items[0].command, "spawn mission resume --id 6");
  assert.match(items[0].title, /#6 Supply PA is halted/);
});

test("a held source asks for the owner when it expires soon or a task waits on it, with the release command", () => {
  const hold = (expires: number | null, waiting: number[]) => ({
    source: "/src/supply.git",
    hold: 7,
    main: null,
    by: "human",
    reason: null,
    held_at_ms: NOW - HOUR,
    expires_ms: expires,
    waiting,
  });
  const far = needsYou({ ...base, missions: missions([], [hold(NOW + 2 * HOUR, [])]) });
  assert.deepEqual(far, [], "far from expiry, nothing waiting");
  const soon = needsYou({ ...base, missions: missions([], [hold(NOW + 10 * MIN, [])]) });
  assert.equal(soon.length, 1);
  assert.equal(soon[0].command, "spawn mission release --source /src/supply.git");
  assert.match(soon[0].title, /expires in 10 min/);
  const waiting = needsYou({ ...base, missions: missions([], [hold(NOW + 2 * HOUR, [221])]) });
  assert.match(waiting[0].detail ?? "", /1 task waiting/);
});

test("actions come before what is only worth knowing; told escalations and a short disk are to know", () => {
  const item = (session: string, why: string, told: boolean, since: number) => ({
    session_id: session,
    agent_id: session.replace("session_", "agent_"),
    why,
    line: `${why} line`,
    tag: `${session} 1`,
    since_ms: since,
    told,
    told_at_ms: told ? since : null,
    label: null,
    task_title: null,
    mission_name: null,
  });
  const items = needsYou({
    ...base,
    attention: attention({
      pending: [item("session_w1", "attention", false, NOW - 5 * MIN)],
      told: [item("session_w2", "stalled", true, NOW - 50 * MIN)],
    }),
    health: {
      ...HEALTHY,
      disk: [{ label: "tmpdir", path: "/t", free_bytes: 5 * GIB, warn_bytes: 20 * GIB, floor_bytes: 2 * GIB }],
    } as unknown as FleetHealth,
  });
  assert.deepEqual(
    items.map((entry) => [entry.tone, entry.title]),
    [
      ["act", "Attention"],
      ["know", "Stalled"],
      ["know", "Disk tmpdir: 5.0 GiB free"],
    ],
  );
  assert.equal(items[0].session, "session_w1", "an agent's escalation opens it");
});

test("an alert about a live session names the agent and opens it; a moot one is absent", () => {
  const agents = [agent("session_supervisor", { role: "supervisor" }), agent("session_old", { live: false })];
  const context = fleetContext(missions([]));
  const items = needsYou({
    ...base,
    agents,
    alerts: [alert("s1", "agent_stuck", "session_supervisor"), alert("s2", "agent_stuck", "session_old")],
    nameOf: (session) => {
      const found = agents.find((entry) => entry.session_id === session);
      return found ? agentName(found, context) : null;
    },
  });
  assert.equal(items.length, 1);
  assert.equal(items[0].title, "An agent looks stuck · supervisor");
  assert.equal(items[0].session, "session_supervisor");
});

test("a mission_idle item says how long the mission has been without work as of now, from its since_ms (objective 071)", () => {
  const since = NOW - 31 * HOUR;
  const fired = (how: string) =>
    `mission #6 has had no task in work for ${how} since its last task ended: its objective backlog has nothing open to assign`;
  const idle = alert("mission_idle:6:40", "mission_idle", "mission #6", {
    fired_at_ms: since + 15 * MIN,
    evidence: { mission: 6, since_ms: since, event_id: 40, idle_ms: 15 * MIN, reading_ms: since + 15 * MIN },
    text: fired("15 min"),
  });
  const detail = (now: number, list: Alert[] = [idle]) =>
    needsYou({ ...base, now, alerts: list }).find((item) => item.key === "alert:mission_idle:6:40")?.detail;
  assert.equal(detail(NOW), fired("31 h"));
  assert.equal(detail(since + 72 * HOUR), fired("3 d"));
  assert.equal(detail(since + 25 * MIN), fired("25 min"));
  assert.equal(detail(since + 21 * HOUR + 25 * MIN), fired("21 h"));
});

test("an alert without a numeric since_ms keeps its text, and other alerts are as today (objective 071)", () => {
  const text = "mission #6 has had no task in work for 15 min since its last task ended: its objective backlog has nothing open to assign";
  for (const since_ms of [null, "1000", undefined]) {
    const idle = alert("mission_idle:6:40", "mission_idle", "mission #6", { evidence: { mission: 6, since_ms }, text });
    assert.equal(needsYou({ ...base, alerts: [idle] })[0].detail, text);
  }
  const bare = alert("mission_idle:6:41", "mission_idle", "mission #6", { evidence: null, text });
  assert.equal(needsYou({ ...base, alerts: [bare] })[0].detail, text);
  const other = alert("disk:/", "disk_low", "/", { evidence: { since_ms: NOW - 31 * HOUR }, text: "disk is low" });
  assert.equal(needsYou({ ...base, alerts: [other] })[0].detail, "disk is low");
});

test("missions today: open work as a row, today's landings and the latest send-back; nothing open folds; closed is counted", () => {
  const rejected = { stage: "gated", verdict: "rejected", at_ms: NOW - 4 * HOUR, took_ms: null } as MissionTask["stages"][number];
  const oldReject = { stage: "gated", verdict: "rejected", at_ms: NOW - 30 * HOUR, took_ms: null } as MissionTask["stages"][number];
  const landedStage = (at: number) => ({ stage: "landed", verdict: null, at_ms: at, took_ms: null }) as MissionTask["stages"][number];
  const model = missionsToday(
    missions([
      mission(6, "Supply PA", [
        task(221, "reported", { updated_at_ms: NOW - 90 * MIN }),
        task(219, "landed", { stages: [rejected, landedStage(NOW - 3 * HOUR)] }),
        task(210, "landed", { stages: [oldReject, landedStage(NOW - 26 * HOUR)] }),
      ]),
      mission(8, "Supply PA · owner work", [task(300, "accepted", { stages: [] })], { gate: null }),
      mission(2, "Done", [], { state: "done" }),
    ]),
    NOW,
  );
  assert.equal(model.rows.length, 1);
  const row = model.rows[0];
  assert.equal(row.name, "Supply PA");
  assert.deepEqual(row.inFlight.map((entry) => [entry.task_id, entry.state]), [[221, "gating"]]);
  assert.equal(row.landedToday, 1, "only the landing in the last 24 h");
  assert.deepEqual(row.lastRejection && [row.lastRejection.task_id, row.lastRejection.verdict], [219, "rejected"]);
  assert.deepEqual(model.quiet.map((entry) => entry.id), [8], "an active mission with nothing open folds");
  assert.equal(model.closed, 1);
});

test("working now: live agents with a task or a running turn, each by its one name; idle ones fold; ended ones are absent", () => {
  const supply = mission(6, "Supply PA", [
    task(222, "reported", { session_id: "session_worker_objective_222", agent_id: "agent_worker_objective_222" }),
  ]);
  const context = fleetContext(missions([supply]));
  const model = agentsNow(
    [
      agent("session_supervisor", { role: "supervisor", state: "WAITING" }),
      agent("session_worker_objective_222", { state: "WAITING" }),
      agent("session_worker_objective_300", { state: "WORKING", activity: { ...agent("x").activity, current_tool: "Bash", current_tool_summary: "cargo test -p spawn-cli" } }),
      agent("session_worker_objective_100", { live: false, state: "WORKING" }),
    ],
    context,
  );
  assert.deepEqual(
    model.working.map((entry) => [entry.name, entry.doing]),
    [
      ["agent_worker_objective_300", "running Bash · cargo test -p spawn-cli"],
      ["worker · Supply PA T222", "gating"],
    ],
  );
  assert.deepEqual(model.idle.map((entry) => entry.name), ["supervisor"]);
});

test("working now is unchanged: an agent waiting at its prompt on an assigned or a delivered task says so, never in work", () => {
  const supply = mission(6, "Supply PA", [
    task(268, "assigned", { session_id: "session_worker_objective_268", agent_id: "agent_worker_objective_268" }),
    task(269, "delivered", { session_id: "session_worker_objective_269", agent_id: "agent_worker_objective_269" }),
    task(270, "delivered", { session_id: "session_worker_objective_270", agent_id: "agent_worker_objective_270" }),
    task(271, "assigned", { session_id: "session_worker_objective_271", agent_id: "agent_worker_objective_271" }),
  ]);
  const context = fleetContext(missions([supply]));
  const model = agentsNow(
    [
      agent("session_worker_objective_268", { state: "WAITING" }),
      agent("session_worker_objective_269", { state: "WAITING" }),
      agent("session_worker_objective_270", { state: "WORKING" }),
      agent("session_worker_objective_271", { state: "WORKING" }),
    ],
    context,
  );
  assert.deepEqual(
    model.working.map((entry) => [entry.name, entry.doing]),
    [
      ["worker · Supply PA T268", "waiting at its prompt"],
      ["worker · Supply PA T269", "waiting at its prompt"],
      ["worker · Supply PA T270", "working"],
      ["worker · Supply PA T271", "working"],
    ],
  );
  for (const id of [268, 269]) {
    const held = heldTask({ session_id: `session_worker_objective_${id}`, agent_id: `agent_worker_objective_${id}` }, context);
    assert.equal(doingWords(agent(`session_worker_objective_${id}`), held), "waiting at its prompt");
  }
});

test("the Missions card says an assigned task waits for its worker and a delivered one is in work", () => {
  const model = missionsToday(
    missions([
      mission(4, "RSI", [
        task(268, "delivered", { stages: [{ stage: "delivered", verdict: null, at_ms: NOW - 5 * MIN, took_ms: null }] as MissionTask["stages"] }),
        task(272, "assigned"),
        task(273, "revise"),
        task(274, "reported"),
      ]),
    ]),
    NOW,
  );
  assert.deepEqual(
    model.rows[0].inFlight.map((entry) => [entry.task_id, entry.state]),
    [[268, "in work"], [272, "waiting for its worker"], [273, "sent back"], [274, "gating"]],
  );
});

test("the supervisor's notes: at most two from the last 6 hours, the rest counted", () => {
  const note = (hours: number) => ({ text: `${hours} h`, at_ms: NOW - hours * HOUR });
  const view = (notes: Array<{ text: string; at_ms: number }>) =>
    attention({
      supervisor: {
        session_id: "session_supervisor",
        agent_id: "agent_supervisor",
        live: true,
        execution_id: null,
        end_cause: null,
        busy: false,
        turn_ended: true,
        judgement: null,
        question: null,
        for_owner: notes,
        label: null,
        task_title: null,
        mission_name: null,
      },
    });
  const three = latestNotes(view([note(3), note(1), note(2)]), NOW);
  assert.deepEqual(three.shown.map((entry) => entry.text), ["1 h", "2 h"]);
  assert.equal(three.more, 1);
  const old = latestNotes(view([note(8)]), NOW);
  assert.deepEqual(old.shown, []);
  assert.equal(old.more, 1);
  assert.deepEqual(latestNotes(null, NOW), { shown: [], more: 0, earlier: 0, settled: 0 });
});

// The supervisor's five lines of rsi/scenarios/064.sh, oldest first, as
// /api/attention lists them (newest first, with `tasks` and `settled`).
function fiveLines() {
  const line = (minutes: number, text: string, tasks: Array<{ id: number; state: string }>, settled: boolean) => ({
    text,
    at_ms: NOW - minutes * 60_000,
    tasks,
    settled,
  });
  return attention({
    supervisor: {
      session_id: "session_supervisor",
      agent_id: "agent_supervisor",
      live: true,
      execution_id: null,
      end_cause: null,
      busy: false,
      turn_ended: true,
      judgement: null,
      question: null,
      for_owner: [
        line(1, "reports 2, 3 and 4 are still waiting for your review.", [
          { id: 2, state: "accepted" },
          { id: 3, state: "accepted" },
          { id: 4, state: "cancelled" },
        ], true),
        line(2, "task 2 landed; deploying it needs spawn init-db.", [{ id: 2, state: "accepted" }], false),
        line(3, "task #5 waits for your review.", [{ id: 5, state: "reported" }], false),
        line(4, "mission #3 has no backlog yet.", [], false),
        line(5, "deploy main to pick up the new cost page.", [], false),
      ],
      label: null,
      task_title: null,
      mission_name: null,
    },
  });
}

test("Home's card leaves a settled line out: neither shown nor counted in more (objective 064)", () => {
  const notes = latestNotes(fiveLines(), NOW);
  assert.deepEqual(
    notes.shown.map((note) => note.text),
    ["task 2 landed; deploying it needs spawn init-db.", "task #5 waits for your review."],
  );
  assert.equal(notes.more, 2, "the mission #3 and deploy main lines");
  assert.equal(notes.settled, 1);
  // Only a settled line besides those shown: the card still opens onto it.
  const only = latestNotes(
    attention({ supervisor: { ...fiveLines().supervisor!, for_owner: fiveLines().supervisor!.for_owner.slice(0, 2) } }),
    NOW,
  );
  assert.deepEqual(only.shown.map((note) => note.text), ["task 2 landed; deploying it needs spawn init-db."]);
  assert.equal(only.more, 0);
  assert.equal(only.settled, 1);
  // An older server sends no `settled`: every line counts as before.
  const older = latestNotes(
    attention({
      supervisor: {
        ...fiveLines().supervisor!,
        for_owner: fiveLines().supervisor!.for_owner.map(({ text, at_ms }) => ({ text, at_ms }) as never),
      },
    }),
    NOW,
  );
  assert.deepEqual(older.shown.map((note) => note.text), ["reports 2, 3 and 4 are still waiting for your review.", "task 2 landed; deploying it needs spawn init-db."]);
  assert.equal(older.more, 3);
  assert.equal(older.settled, 0);
});

test("the supervisor's full list still shows a settled line, marked settled, with each named task's state (objective 064)", () => {
  assert.deepEqual(allNotes(fiveLines()), [
    {
      text: "reports 2, 3 and 4 are still waiting for your review.",
      at_ms: NOW - 60_000,
      settled: true,
      tasks: "T2 accepted, T3 accepted, T4 cancelled",
      current: true,
    },
    { text: "task 2 landed; deploying it needs spawn init-db.", at_ms: NOW - 120_000, settled: false, tasks: "T2 accepted", current: true },
    { text: "task #5 waits for your review.", at_ms: NOW - 180_000, settled: false, tasks: "T5 reported", current: true },
    { text: "mission #3 has no backlog yet.", at_ms: NOW - 240_000, settled: false, tasks: "", current: true },
    { text: "deploy main to pick up the new cost page.", at_ms: NOW - 300_000, settled: false, tasks: "", current: true },
  ]);
  assert.deepEqual(allNotes(null), []);
});

// rsi/scenarios/078.sh's two blocks as /api/attention lists them: the
// newest (two lines) current, the earlier turn's three lines not.
function twoBlocks() {
  const line = (minutes: number, text: string, tasks: Array<{ id: number; state: string }>, current: boolean, settled = false) => ({
    text,
    at_ms: NOW - minutes * 60_000,
    tasks,
    settled,
    current,
  });
  return attention({
    supervisor: {
      ...fiveLines().supervisor!,
      for_owner: [
        line(1, "My engine needs SPAWN_WORKSPACE set, so I can answer messages #11, #12 and #13.", [], true),
        line(1, "Scenario bb.sh exits 2 on the base (see the gate output for task 3).", [{ id: 3, state: "accepted" }], true),
        line(2, "My engine needs SPAWN_WORKSPACE set, so I can answer messages #11 and #12.", [], false),
        line(2, "If task 5 stays idle, restart it or reassign it.", [{ id: 5, state: "reported" }], false),
        line(2, "The relay names another model for worker 5.", [], false),
        line(3, "reports 2 and 4 wait for your review.", [{ id: 2, state: "accepted" }, { id: 4, state: "cancelled" }], false, true),
      ],
    },
  });
}

test("Home's card shows the newest block alone and counts the earlier turns' lines as earlier (objective 078)", () => {
  const notes = latestNotes(twoBlocks(), NOW);
  assert.deepEqual(
    notes.shown.map((note) => note.text),
    [
      "My engine needs SPAWN_WORKSPACE set, so I can answer messages #11, #12 and #13.",
      "Scenario bb.sh exits 2 on the base (see the gate output for task 3).",
    ],
  );
  assert.equal(notes.more, 0);
  assert.equal(notes.earlier, 3, "the earlier turn's unsettled lines; the settled one is not among them");
  assert.equal(notes.settled, 1);
  // A current line past NOTES_SHOWN or stale is in `more`, never `earlier`.
  const stale = latestNotes(twoBlocks(), NOW + 7 * HOUR);
  assert.deepEqual(stale.shown, []);
  assert.equal(stale.more, 2);
  assert.equal(stale.earlier, 3);
});

test("each line Home's card shows says what became of the tasks it names (objective 078)", () => {
  const shown = latestNotes(twoBlocks(), NOW).shown;
  assert.deepEqual(
    shown.map((note) => note.taskStates),
    ["", "T3 accepted"],
  );
  const many = latestNotes(
    attention({
      supervisor: {
        ...fiveLines().supervisor!,
        for_owner: [{ text: "review T2 and T4", at_ms: NOW, tasks: [{ id: 2, state: "accepted" }, { id: 4, state: "cancelled" }], settled: false, current: true }],
      },
    }),
    NOW,
  );
  assert.equal(many.shown[0]?.taskStates, "T2 accepted, T4 cancelled");
});

test("the full list keeps every line, an earlier turn's marked not current (objective 078)", () => {
  const rows = allNotes(twoBlocks());
  assert.equal(rows.length, 6);
  assert.deepEqual(rows.map((row) => row.current), [true, true, false, false, false, false]);
  assert.equal(rows[1]?.tasks, "T3 accepted");
});

test("today's digest: one line per task, and alerts, of the last 24 h, newest first; task notices are left out", () => {
  const landedStage = (at: number) => ({ stage: "landed", verdict: null, at_ms: at, took_ms: null }) as MissionTask["stages"][number];
  const sentBack = { stage: "gated", verdict: "conflict", at_ms: NOW - 2 * HOUR, took_ms: null } as MissionTask["stages"][number];
  const digest = digestToday(
    missions([
      mission(6, "Supply PA", [
        task(219, "landed", { stages: [sentBack, landedStage(NOW - HOUR)] }),
        task(200, "landed", { stages: [landedStage(NOW - 30 * HOUR)] }),
      ]),
    ]),
    [
      alert("a1", "judge_down", "jev", { fired_at_ms: NOW - 3 * HOUR, state: "cleared" }),
      alert("n1", "task_state", "task #219", { state: "notice", fired_at_ms: NOW - HOUR }),
    ],
    NOW,
  );
  assert.deepEqual([digest.landed, digest.sentBack, digest.alerts], [1, 1, 1]);
  // One line per task: T219's send-back is part of its landing's story.
  assert.deepEqual(
    digest.entries.map((entry) => entry.kind),
    ["landed", "alert"],
  );
  assert.match(digest.entries[0].text, /^Supply PA · T219 .*: landed after 1 gate, sent back 1× on the way$/);
  assert.match(digest.entries[1].text, /\(cleared\)$/);
});

test("a dispatcher stall reads as one line; another line keeps its first clause; the whole text is kept for expanding", () => {
  assert.equal(
    oneLine(
      "task #221 (mission #6) has not moved from step judgement for 1h31m (bound 1h00m): the gate of mission #6 judges it next; ready since task.reported",
    ),
    "T221 waiting for its judgement 1h31m (bound 1h)",
  );
  assert.equal(oneLine("disk space on this host: 1.2 GiB free"), "disk space on this host");
  assert.equal(oneLine("no colon here"), "no colon here");
  const items = needsYou({
    ...base,
    attention: attention({
      told: [
        {
          session_id: "session_resident_supply",
          agent_id: "agent_resident_supply",
          why: "stalled",
          line: "task #221 (mission #6) has not moved from step judgement for 1h31m (bound 1h00m): the gate judges it next",
          tag: "t 1",
          since_ms: NOW - 50 * MIN,
          told: true,
          told_at_ms: NOW - 50 * MIN,
          label: null,
          task_title: null,
          mission_name: null,
        },
      ],
    }),
  });
  assert.equal(items[0].detail, "T221 waiting for its judgement 1h31m (bound 1h) · the supervisor was told");
  assert.match(items[0].full ?? "", /the gate judges it next \(the supervisor was told\)$/);
});

test("a stuck alert not scored since it fired is stale: to know, not to act, and out of the band; a fresh score keeps it", () => {
  const fired = NOW - 9 * HOUR;
  const stuck = alert("agent_stuck:session_supervisor:1", "agent_stuck", "session_supervisor", {
    fired_at_ms: fired,
    evidence: { stuck: 0.52 },
  });
  const judged = (stuckScore: number | null, at: number) =>
    agent("session_supervisor", {
      role: "supervisor",
      judged_at_ms: at,
      judgement: { state: "WAITING", stuck: stuckScore, judge: "jev", reason: stuckScore === null ? "JUDGE_UNAVAILABLE" : null } as FleetAgent["judgement"],
    });
  const unscored = [judged(null, NOW - 52 * MIN)];
  assert.notEqual(alertStale(stuck, unscored), null);
  assert.deepEqual(actionableAlerts([stuck], unscored), [], "not in the band");
  const items = needsYou({ ...base, agents: unscored, alerts: [stuck] });
  assert.equal(items[0].tone, "know");
  assert.equal(
    items[0].detail,
    "scored stuck 0.52 9 h ago; not scored since (its latest judgement, 52 min ago, has no score: JUDGE_UNAVAILABLE)",
  );
  const rescored = [judged(0.7, NOW - 52 * MIN)];
  assert.equal(alertStale(stuck, rescored), null, "a later score of 0.7: still stuck");
  assert.equal(needsYou({ ...base, agents: rescored, alerts: [stuck] })[0].tone, "act");
  const before = [judged(null, fired - MIN)];
  assert.equal(alertStale(stuck, before), null, "judged before it fired: the alert's own score");
});

test("the header and the card say the same", () => {
  assert.equal(needsSummary([]), "Nothing needs you");
  const item = (tone: "act" | "know") => ({ key: tone, tone, title: "t", detail: null, since_ms: null, command: null, session: null });
  assert.equal(needsSummary([item("act"), item("know")]), "1 to act on · 1 to know");
  assert.equal(needsSummary([item("know"), item("know")]), "2 to know");
});

test("a resident on a task is named by its mission, as a worker is; without one, by its own name", () => {
  const supply = mission(6, "Supply PA", [
    task(221, "reported", { session_id: "session_resident_supply", agent_id: "agent_resident_supply" }),
  ]);
  const raw = missions([supply]);
  raw.residents = [{ name: "supply", agent_id: "agent_resident_supply", session_id: "session_resident_supply" } as Missions["residents"][number]];
  const context = fleetContext(raw);
  assert.equal(agentName(agent("session_resident_supply", { role: "rsi" }), context), "resident · Supply PA T221");
  const idle = fleetContext({ ...raw, missions: [] });
  assert.equal(agentName(agent("session_resident_supply", { role: "rsi" }), idle), "resident · supply");
});

test("the digest leaves out a moot alert (one about a session that has ended)", () => {
  const agents = [agent("session_old", { live: false })];
  const digest = digestToday(missions([]), [alert("s", "agent_stuck", "session_old", { fired_at_ms: NOW - HOUR })], NOW, agents);
  assert.equal(digest.alerts, 0);
});

test("an open tool reads as its key input; a call recorded without one reads as the tool alone", () => {
  const base = agent("x").activity;
  assert.equal(currentToolText({ ...base, current_tool: "Read", current_tool_summary: "web/src/home.ts:1-80" }), "Read · web/src/home.ts:1-80");
  assert.equal(currentToolText({ ...base, current_tool: "Read", current_tool_summary: null }), "Read");
  assert.equal(currentToolText({ ...base, current_tool: null, current_tool_summary: "stale" }), null);
});

test("every agent has one name, its worker's from its open task, else its latest", () => {
  const supply = mission(6, "Supply PA", [
    task(222, "working", { session_id: "session_worker_objective_239", agent_id: "agent_worker_objective_239", task_title: "holds on a cart" } as Partial<MissionTask>),
    task(220, "landed", { session_id: "session_worker_objective_101", agent_id: "agent_worker_objective_101" }),
  ]);
  const context = fleetContext(missions([supply]));
  const names = agentNames(
    [
      agent("session_supervisor", { role: "supervisor" }),
      agent("session_worker_objective_239"),
      agent("session_worker_objective_101", { live: false }),
      agent("session_smoke_transcript", { live: false }),
    ],
    context,
  );
  assert.equal(names.get("session_supervisor"), "supervisor");
  assert.equal(names.get("session_worker_objective_239"), "worker · Supply PA T222");
  assert.equal(names.get("session_worker_objective_101"), "worker · Supply PA T220", "an ended worker, by its latest task");
  assert.equal(names.get("session_smoke_transcript"), "agent_smoke_transcript", "no task: its id");
});

test("two workers on one task (a reassign) keep apart: the T-number and a short suffix from the session id", () => {
  const supply = mission(6, "Supply PA", [
    task(222, "working", { session_id: "session_worker_objective_244", agent_id: "agent_worker_objective_244" }),
    task(222, "cancelled", { session_id: "session_worker_objective_239", agent_id: "agent_worker_objective_239" }),
  ]);
  const context = fleetContext(missions([supply]));
  const pair = [agent("session_worker_objective_244"), agent("session_worker_objective_239", { live: false })];
  const names = agentNames(pair, context);
  assert.equal(names.get("session_worker_objective_244"), "worker · Supply PA T222 · 244");
  assert.equal(names.get("session_worker_objective_239"), "worker · Supply PA T222 · 239");
  // Last words alike: the last six characters.
  const alike = agentNames([agent("session_a_x", { agent_id: "agent_same" }), agent("session_b_x", { agent_id: "agent_same" })], fleetContext(missions([])));
  assert.equal(alike.get("session_a_x"), "agent_same · on_a_x");
  assert.equal(alike.get("session_b_x"), "agent_same · on_b_x");
  // Unique across the board, whatever collides.
  const many = agentNames([...pair, agent("session_worker_objective_239b", { live: false, agent_id: "agent_worker_objective_239" })], context);
  assert.equal(new Set(many.values()).size, 3, [...many.values()].join(" / "));
});

test("home's agents now use the board's unique names", () => {
  const supply = mission(6, "Supply PA", [
    task(222, "working", { session_id: "session_worker_objective_244", agent_id: "agent_worker_objective_244" }),
    task(222, "working", { session_id: "session_worker_objective_239", agent_id: "agent_worker_objective_239" }),
  ]);
  const model = agentsNow([agent("session_worker_objective_244"), agent("session_worker_objective_239")], fleetContext(missions([supply])));
  assert.deepEqual(model.working.map((row) => row.name), ["worker · Supply PA T222 · 239", "worker · Supply PA T222 · 244"]);
});

test("the search matches every word, case aside, in the name, the ids, the T-number, the title or the mission", () => {
  const supply = mission(6, "Supply PA", [
    task(220, "landed", { session_id: "session_worker_objective_101", agent_id: "agent_worker_objective_101", task_title: "Holds on a cart" } as Partial<MissionTask>),
  ]);
  const context = fleetContext(missions([supply]));
  const ended = agent("session_worker_objective_101", { live: false });
  const fields = agentSearchFields(ended, context, "worker · Supply PA T220");
  for (const query of ["t220", "T220", "supply cart", "objective_101", "agent_worker", "  worker   holds ", ""]) {
    assert.equal(agentMatches(query, fields), true, query);
  }
  for (const query of ["T221", "supply truck", "rsi"]) assert.equal(agentMatches(query, fields), false, query);
});

test("the reader was away from the last time the page was seen, if 30 min or more ago", () => {
  assert.equal(awayFrom(null, NOW), null, "a first visit");
  assert.equal(awayFrom(NOW - 10 * MIN, NOW), null, "a reload, a short break");
  assert.equal(awayFrom(NOW - 30 * MIN, NOW), NOW - 30 * MIN);
  assert.equal(awayFrom(NOW - 3 * HOUR, NOW), NOW - 3 * HOUR);
  assert.equal(awayFrom(NOW + MIN, NOW), null, "a clock that went back");
  assert.equal(awayFrom(Number.NaN, NOW), null);
});

test("the digest window is since the reader went away, at least 24 h, at most 7 days", () => {
  assert.deepEqual(digestWindow(NOW, null), { since: NOW - 24 * HOUR, away: false });
  assert.deepEqual(digestWindow(NOW, NOW - 3 * HOUR), { since: NOW - 24 * HOUR, away: false }, "under the floor: today");
  assert.deepEqual(digestWindow(NOW, NOW - 24 * HOUR), { since: NOW - 24 * HOUR, away: false }, "at the floor");
  assert.deepEqual(digestWindow(NOW, NOW - 50 * HOUR), { since: NOW - 50 * HOUR, away: true });
  assert.deepEqual(digestWindow(NOW, NOW - 30 * 24 * HOUR), { since: NOW - 7 * 24 * HOUR, away: true }, "capped");
});

test("each task's day is one line: landed after its gates, sent back and where it is now, stopped, or moved", () => {
  const stage = (name: string, at: number, verdict: string | null = null) => ({ stage: name, verdict, at_ms: at, took_ms: null }) as MissionTask["stages"][number];
  const since = NOW - 24 * HOUR;
  const supply = mission(6, "Supply PA", [
    task(1, "landed", { task_title: "two gates", stages: [stage("assigned", NOW - 40 * HOUR), stage("gated", NOW - 30 * HOUR, "rejected"), stage("gated", NOW - 2 * HOUR, "accepted"), stage("landed", NOW - HOUR)] } as Partial<MissionTask>),
    task(2, "reported", { task_title: "sent back twice", stages: [stage("gated", NOW - 5 * HOUR, "rejected"), stage("gated", NOW - 3 * HOUR, "conflict"), stage("reported", NOW - 2 * HOUR)] } as Partial<MissionTask>),
    task(3, "cancelled", { task_title: "stopped", stages: [stage("assigned", NOW - 6 * HOUR), stage("cancelled", NOW - 4 * HOUR)] } as Partial<MissionTask>),
    task(4, "delivered", { task_title: "new", stages: [stage("assigned", NOW - 20 * MIN), stage("delivered", NOW - 19 * MIN)] } as Partial<MissionTask>),
    task(5, "landed", { task_title: "old", stages: [stage("landed", NOW - 30 * HOUR)] } as Partial<MissionTask>),
  ]);
  const digest = digestSince(missions([supply]), [], NOW, since);
  const byTask = new Map(digest.entries.map((entry) => [entry.text.split(" ")[3], entry]));
  assert.equal(byTask.get("T1")?.text, "Supply PA · T1 two gates: landed after 2 gates");
  assert.equal(byTask.get("T1")?.kind, "landed");
  assert.equal(byTask.get("T2")?.text, "Supply PA · T2 sent back twice: sent back 2× (rejected, conflict); now gating");
  assert.equal(byTask.get("T3")?.text, "Supply PA · T3 stopped: cancelled");
  assert.equal(byTask.get("T3")?.kind, "stopped");
  assert.equal(byTask.get("T4")?.text, "Supply PA · T4 new: started; now in work");
  assert.equal(byTask.has("T5"), false, "nothing in the window");
  assert.deepEqual([digest.landed, digest.sentBack], [1, 2]);
  assert.equal(digest.entries[0].text.includes("T4"), true, "newest first");
});

test("a mission's halt, resume or end is a line; a counter moving is not", () => {
  const rsi = mission(4, "Spawn RSI", []);
  const change = (key: string, at: number, before: object, after: object) =>
    alert(key, "mission_state", "mission #4", { state: "notice", fired_at_ms: at, evidence: { mission: 4, before, after } });
  const base = { state: "active", halted: null, gate_rejections: 0 };
  const digest = digestSince(
    missions([rsi]),
    [
      change("c", NOW - 4 * HOUR, base, { ...base, gate_rejections: 1 }),
      change("h", NOW - 3 * HOUR, base, { ...base, halted: "3 rejections" }),
      change("r", NOW - 2 * HOUR, { ...base, halted: "3 rejections" }, base),
      change("d", NOW - HOUR + MIN, base, { ...base, state: "done" }),
      alert("t", "task_state", "task #9", { state: "notice", fired_at_ms: NOW - HOUR }),
      // A task notice is never read, whatever its evidence holds: the missions tell the task.
      alert("t2", "task_state", "task #9", { state: "notice", fired_at_ms: NOW - HOUR, evidence: { mission: 4, before: { state: "reported" }, after: { state: "landing" } } }),
    ],
    NOW,
    NOW - 24 * HOUR,
  );
  assert.deepEqual(digest.entries.map((entry) => entry.text), ["Spawn RSI: done", "Spawn RSI: resumed", "Spawn RSI: halted (3 rejections)"]);
  assert.ok(digest.entries.every((entry) => entry.kind === "mission"));
  assert.equal(digest.alerts, 0, "a notice is not an alert");
});

test("lines are grouped by when: last hour, earlier today, yesterday, then the day", () => {
  const now = new Date(2026, 10, 10, 15, 0).getTime();
  assert.equal(digestBand(now - 20 * MIN, now), "Last hour");
  assert.equal(digestBand(new Date(2026, 10, 10, 9, 0).getTime(), now), "Earlier today");
  assert.equal(digestBand(new Date(2026, 10, 9, 23, 0).getTime(), now), "Yesterday");
  assert.equal(digestBand(new Date(2026, 10, 7, 12, 0).getTime(), now), "Sat, Nov 7");
  const stage = (at: number) => ({ stage: "landed", verdict: null, at_ms: at, took_ms: null }) as MissionTask["stages"][number];
  const digest = digestSince(
    missions([mission(6, "Supply PA", [task(1, "landed", { stages: [stage(now - 10 * MIN)] }), task(2, "landed", { stages: [stage(now - 5 * HOUR)] }), task(3, "landed", { stages: [stage(now - 6 * HOUR)] })])]),
    [],
    now,
    now - 24 * HOUR,
  );
  assert.deepEqual(digest.bands.map((band) => [band.label, band.entries.length]), [["Last hour", 1], ["Earlier today", 2]]);
});

test("the digest asks for alerts as far back as it reads, in whole hours, never under 24 h", () => {
  assert.equal(alertsSpanMs(NOW, NOW - 24 * HOUR), 24 * HOUR);
  assert.equal(alertsSpanMs(NOW, NOW - 3 * HOUR), 24 * HOUR, "the server's own window at least");
  assert.equal(alertsSpanMs(NOW, NOW - 50 * HOUR - MIN), 51 * HOUR, "rounded up");
  assert.equal(alertsSpanMs(NOW, NOW - 7 * 24 * HOUR), 7 * 24 * HOUR);
});

test("a 3-day digest holds an alert that fired and cleared 2 days ago (rev-f F1)", () => {
  const cleared = alert("j", "judge_down", "jev", { state: "cleared", fired_at_ms: NOW - 48 * HOUR, cleared_at_ms: NOW - 47 * HOUR });
  const away = digestSince(missions([]), [cleared], NOW, NOW - 72 * HOUR, null, true);
  assert.equal(away.alerts, 1);
  assert.equal(away.entries[0].text, "The model judge answers no call · jev (cleared)");
  assert.deepEqual(away.bands.map((band) => band.entries.length), [1]);
  // The last 24 h does not.
  assert.equal(digestSince(missions([]), [cleared], NOW, NOW - 24 * HOUR).alerts, 0);
});

test("a source away from this host has its own title in Needs you (P12)", () => {
  assert.equal(
    alertTitle({ kind: "source_away", subject: "mission #4" }),
    "A mission's source is not a directory on this host · mission #4",
  );
});

test("a controller without the owner's landing has its own title in Needs you (P22)", () => {
  assert.equal(
    alertTitle({ kind: "controller_behind", subject: "mission #4" }),
    "A landing waits for its deploy · mission #4",
  );
});

test("a deploy that failed has its own title in Needs you (#54)", () => {
  assert.equal(
    alertTitle({ kind: "deploy_failed", subject: "deploy" }),
    "A deploy failed · deploy",
  );
});

test("a validator that fails on main too has its own title in Needs you", () => {
  assert.equal(
    alertTitle({ kind: "validator_defect", subject: "mission #4" }),
    "A validator fails on main too · mission #4",
  );
});

test("an owner landing's old per-mission validator alerts are one item in Needs you, naming every mission; once cleared, none", () => {
  // The live shape of 2026-11-11 23:33: two alerts of request #129675's
  // landing (event 130068) about 003.sh, one per mission, from before
  // objective 056 made them one per source.
  const old = (mission: number) =>
    alert(`validator_defect:${mission}:003.sh:130068`, "validator_defect", `mission #${mission}`, {
      evidence: { event_id: 130068, mission, owner: true, request: 129675, scenario: "003.sh" },
      text: `mission #${mission}: the owner's landing (request #129675) was accepted without 003.sh`,
    });
  const perSource = alert("validator_defect:/s:v1.sh:20", "validator_defect", "/s (mission #4)", {
    evidence: { event_id: 20, source: "/s", missions: [4], owner: true, scenario: "v1.sh" },
  });
  const task = (mission: number) =>
    alert(`validator_defect:${mission}:028.sh:11`, "validator_defect", `mission #${mission}`, {
      evidence: { event_id: 11, mission, owner: false, scenario: "028.sh" },
    });
  const items = needsYou({ ...base, alerts: [old(7), old(4), perSource, task(4), task(5)] });
  const titles = items.filter((item) => item.key.startsWith("alert:validator_defect")).map((item) => item.title);
  assert.deepEqual(titles, [
    "A validator fails on main too · mission #4, mission #7",
    "A validator fails on main too · /s (mission #4)",
    "A validator fails on main too · mission #4",
    "A validator fails on main too · mission #5",
  ]);
  const folded = items.find((item) => item.title.endsWith("mission #4, mission #7"));
  assert.equal(folded?.session, null, "a mission list is no session");
  const cleared = [old(4), old(7)].map((entry) => ({ ...entry, state: "cleared" }));
  assert.deepEqual(
    needsYou({ ...base, alerts: cleared }).filter((item) => item.key.startsWith("alert:")),
    [],
    "cleared, Needs you holds no validator item",
  );
});

test("missions today: each task in flight counts its conflict verdicts and says so from two (objective 069)", () => {
  const gated = (verdict: string, hoursAgo: number) =>
    ({ stage: "gated", verdict, at_ms: NOW - hoursAgo * HOUR, took_ms: null }) as MissionTask["stages"][number];
  const reviewed = (verdict: string, hoursAgo: number) =>
    ({ stage: "reviewed", verdict, at_ms: NOW - hoursAgo * HOUR, took_ms: null }) as MissionTask["stages"][number];
  const model = missionsToday(
    missions([
      mission(4, "RSI", [
        task(1, "reported"),
        task(2, "revise", { stages: [gated("conflict", 5)] }),
        task(3, "reported", { stages: [gated("conflict", 6), gated("conflict", 4)] }),
        task(4, "revise", { stages: [gated("conflict", 8), gated("conflict", 6), gated("conflict", 4)] }),
        task(5, "revise", { stages: [gated("rejected", 6), gated("incompatible", 4), gated("conflict", 3)] }),
        task(6, "revise", { stages: [gated("conflict", 6), reviewed("conflict", 4), reviewed("rejected", 3)] }),
        task(7, "landed", { stages: [gated("conflict", 6), gated("conflict", 4), gated("accepted", 3)] }),
      ]),
    ]),
    NOW,
  );
  assert.deepEqual(
    model.rows[0].inFlight.map(({ task_id, conflicts, conflictText }) => [task_id, conflicts, conflictText]),
    [
      [1, 0, null],
      [2, 1, null],
      [3, 2, "sent back 2 times as a conflict"],
      [4, 3, "sent back 3 times as a conflict"],
      [5, 1, null],
      [6, 1, null],
    ],
    "rejected, incompatible and reviewed send-backs do not count; a landed task is not in flight",
  );
});

test("missions today: a task counts its objective's earlier attempts' conflicts too (objective 073)", () => {
  const gated = (verdict: string, hoursAgo: number) =>
    ({ stage: "gated", verdict, at_ms: NOW - hoursAgo * HOUR, took_ms: null }) as MissionTask["stages"][number];
  const made = (hoursAgo: number) => NOW - hoursAgo * HOUR;
  const model = missionsToday(
    missions([
      mission(1, "Backlog", [
        // Objective 1: two earlier attempts with a conflict each, then T3.
        task(1, "cancelled", { objective: 1, created_at_ms: made(30), stages: [gated("conflict", 29)] }),
        task(2, "cancelled", { objective: 1, created_at_ms: made(20), stages: [gated("conflict", 19)] }),
        task(3, "revise", { objective: 1, created_at_ms: made(10), stages: [gated("conflict", 9)] }),
        // Objective 2: an earlier attempt with two; T5 has none of its own.
        task(4, "cancelled", { objective: 2, created_at_ms: made(30), stages: [gated("conflict", 29), gated("conflict", 28)] }),
        task(5, "delivered", { objective: 2, created_at_ms: made(10) }),
        // Objective 4: an earlier attempt without a conflict (rejected).
        task(7, "cancelled", { objective: 4, created_at_ms: made(30), stages: [gated("rejected", 29)] }),
        task(8, "revise", { objective: 4, created_at_ms: made(10), stages: [gated("conflict", 9)] }),
        // Objective 5: T12 is earlier than T13, whose conflicts T12 does not count.
        task(12, "revise", { objective: 5, created_at_ms: made(20), stages: [gated("conflict", 19)] }),
        task(13, "revise", { objective: 5, created_at_ms: made(10), stages: [gated("conflict", 9), gated("conflict", 8)] }),
        // Objective 3 here; mission 2's objective 3 is another objective.
        task(6, "revise", { objective: 3, created_at_ms: made(10), stages: [gated("conflict", 9), gated("conflict", 8)] }),
        // No objective: nothing earlier, whatever other such tasks had.
        task(10, "cancelled", { created_at_ms: made(30), stages: [gated("conflict", 29), gated("conflict", 28)] }),
        task(11, "delivered", { created_at_ms: made(10) }),
      ]),
      mission(2, "Other backlog", [
        task(9, "cancelled", { objective: 3, created_at_ms: made(40), stages: [gated("conflict", 39), gated("conflict", 38)] }),
        task(14, "delivered", { objective: 3, created_at_ms: made(5) }),
      ]),
    ]),
    NOW,
  );
  const rows = (id: number) =>
    model.rows
      .find((row) => row.id === id)!
      .inFlight.map(({ task_id, conflicts, earlierConflicts, conflictText }) => [task_id, conflicts, earlierConflicts, conflictText]);
  assert.deepEqual(rows(1), [
    [3, 1, 2, "sent back 3 times as a conflict, 2 on earlier attempts"],
    [5, 0, 2, "sent back 2 times as a conflict, 2 on earlier attempts"],
    [6, 2, 0, "sent back 2 times as a conflict"],
    [8, 1, 0, null],
    [11, 0, 0, null],
    [12, 1, 0, null],
    [13, 2, 1, "sent back 3 times as a conflict, 1 on earlier attempts"],
  ]);
  // Mission 2's T14 counts its own mission's T9 only.
  assert.deepEqual(rows(2), [[14, 0, 2, "sent back 2 times as a conflict, 2 on earlier attempts"]]);
});

test("conflictText counts the task's own conflicts and its earlier attempts' together, from two (objective 073)", () => {
  assert.equal(conflictText(0), null);
  assert.equal(conflictText(1), null);
  assert.equal(conflictText(2), "sent back 2 times as a conflict");
  assert.equal(conflictText(1, 0), null);
  assert.equal(conflictText(0, 1), null);
  assert.equal(conflictText(1, 1), "sent back 2 times as a conflict, 1 on earlier attempts");
  assert.equal(conflictText(0, 2), "sent back 2 times as a conflict, 2 on earlier attempts");
  assert.equal(conflictText(3, 0), "sent back 3 times as a conflict");
});

test("tasks made at the same time order by id for their earlier attempts (objective 073)", () => {
  const gated = { stage: "gated", verdict: "conflict", at_ms: NOW - HOUR, took_ms: null } as MissionTask["stages"][number];
  const first = task(20, "cancelled", { objective: 9, stages: [gated] });
  const second = task(21, "revise", { objective: 9, stages: [gated] });
  const backlog = mission(5, "Backlog", [second, first]);
  assert.equal(earlierConflictCount(second, backlog), 1);
  assert.equal(earlierConflictCount(first, backlog), 0);
  const unmade = task(22, "revise", { objective: 9, created_at_ms: null });
  assert.equal(earlierConflictCount(unmade, mission(5, "Backlog", [first, second, unmade])), 0, "no time: before every timed task");
});

test("Home's in-flight line shows the conflict text beside the task's state, in the warning tone", () => {
  const home = readFileSync(new URL("../src/components/home/Home.tsx", import.meta.url), "utf8");
  const line = home.slice(home.indexOf("row.inFlight.map"), home.indexOf("row.lastRejection &&"));
  assert.match(line, /\{task\.conflictText && \(\s*<span className="[^"]*text-warning[^"]*"[^>]*>\s*\{task\.conflictText\}/);
  assert.ok(line.indexOf("task.conflictText") < line.indexOf("{task.state}"), "the text sits beside the state");
});

test("an owner's landing request waiting for its verdict has its own title, and the withdrawal as its command (S24)", () => {
  const waiting = alert("owner_request_waiting:135555", "owner_request_waiting", "/live/missions/spawn.git", {
    evidence: {
      request: 135555,
      commit: "a9e3c0c7",
      withdraw: "spawn mission push --withdraw 135555",
      // A command field it does not have must not be read instead.
    },
    text: "the owner's landing request #135555 (a9e3c0c7) on /live/missions/spawn.git has had no outcome for 1 h",
  });
  const items = needsYou({ ...base, alerts: [waiting] });
  assert.equal(items.length, 1);
  assert.equal(items[0].title, "An owner's landing waits for its verdict · /live/missions/spawn.git");
  assert.equal(items[0].command, "spawn mission push --withdraw 135555");
  assert.equal(items[0].tone, "act");
  assert.equal(items[0].detail, waiting.text);
  // Another kind's `withdraw` is no command of its own.
  const other = alert("x:1", "deploy_failed", "deploy", { evidence: { withdraw: "spawn mission push --withdraw 1" } });
  assert.equal(needsYou({ ...base, alerts: [other] })[0].command, null);
});

test("a message not taken says whether Spawn read the transcript since it was typed, from the alert's own evidence (S17)", () => {
  const unread = alert("message_not_taken:41", "message_not_taken", "session_resident", {
    evidence: { command: 41, typed: true, read_since_typed: 0 },
  });
  const heard = alert("message_not_taken:42", "message_not_taken", "session_worker", {
    evidence: { command: 42, typed: true, read_since_typed: 3 },
  });
  const older = alert("message_not_taken:43", "message_not_taken", "session_old", {
    evidence: { command: 43, typed: true },
  });
  assert.equal(alertTitle(unread), "Spawn has read nothing of a transcript since a message was typed · session_resident");
  assert.equal(alertTitle(heard), "A message was not taken · session_worker");
  assert.equal(alertTitle(older), "A message was not taken · session_old", "no evidence: said as before, never guessed");
  assert.equal(alertTitle({ kind: "message_not_taken", subject: "s" }), "A message was not taken · s");
  const titles = needsYou({ ...base, alerts: [unread, heard] }).map((item) => item.title);
  assert.deepEqual(titles, [
    "Spawn has read nothing of a transcript since a message was typed · session_resident",
    "A message was not taken · session_worker",
  ]);
});

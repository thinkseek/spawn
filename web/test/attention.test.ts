import assert from "node:assert/strict";
import { test } from "node:test";

import {
  agentNeedsHuman,
  attentionGroups,
  itemSince,
  lastParagraph,
  markNeedsHuman,
  needsHuman,
  normalizeAttention,
  questionSummary,
  toldFolded,
} from "../src/attention.ts";
import { groupAgents, fleetContext } from "../src/ordering.ts";

const NOW = 1_793_957_840_000;
const HOUR = 3_600_000;

/** The answer of `/api/attention` for the live situation of 2026-11-06,
 * in the shape the server's own test produces. */
function answer(): unknown {
  return {
    generated_at_ms: NOW,
    pending: [
      {
        session_id: "session_worker_objective_87",
        agent_id: "agent_worker_objective_87",
        why: "attention",
        line: "BLOCKED by jev (90%); needs_human 90% stuck 10%",
        tag: "session_worker_objective_87 300",
        since_ms: NOW - 2 * 60_000,
        told: false,
        told_at_ms: null,
      },
      {
        session_id: "host",
        agent_id: null,
        why: "disk space on this host",
        line: "tmpdir /var/tmp has 5.0 GiB free",
        tag: "host disk-7",
        since_ms: NOW - 10 * 60_000,
        told: false,
        told_at_ms: null,
      },
      {
        session_id: "missions",
        agent_id: null,
        why: "new mission, plan it and assign tasks",
        line: "#9 from human: ship it",
        tag: "missions mission-9",
        since_ms: null,
        told: false,
        told_at_ms: null,
      },
    ],
    told: [
      {
        session_id: "session_worker_objective_90",
        agent_id: "agent_worker_objective_90",
        why: "resume failed",
        line: "engine was lost again within a minute of its automatic resume",
        tag: "session_worker_objective_90 resume-exec_9",
        since_ms: NOW - 3 * HOUR,
        told: true,
        told_at_ms: NOW - HOUR,
      },
      {
        session_id: "session_worker_objective_86",
        agent_id: "agent_worker_objective_86",
        why: "stalled",
        line: "task #86 (mission #7) has not moved from step activity for 2h04m",
        tag: "session_worker_objective_86 stall-86-activity-41",
        since_ms: NOW - 2 * HOUR,
        told: true,
        told_at_ms: NOW - 90 * 60_000,
      },
    ],
    supervisor: {
      session_id: "session_supervisor",
      agent_id: "agent_supervisor",
      live: true,
      execution_id: "exec_379",
      end_cause: null,
      busy: false,
      turn_ended: true,
      judgement: {
        state: "BLOCKED",
        needs_human: 0.87,
        stuck: 0.1,
        ambiguity: "turn_stopped_idle",
        confidence: 0.9,
        judge: "jev",
        attention: false,
        attention_reason: null,
        judged_at_ms: NOW - 4 * HOUR,
        current: true,
      },
      question: {
        text: "Another failed delivery.\nMy question about stopping `objective_87` is still open.",
        source: "transcript",
        asked_at_ms: NOW - 4 * HOUR,
        human: {
          text: "Should I stop `objective_87` now?",
          at_ms: NOW - 4 * HOUR - 10_000,
        },
      },
    },
    warnings: [],
  };
}

test("normalizes the dispatcher's view field by field", () => {
  const view = normalizeAttention(answer());
  assert.equal(view.generated_at_ms, NOW);
  assert.equal(view.pending.length, 3);
  assert.deepEqual(view.pending[1], {
    session_id: "host",
    agent_id: null,
    why: "disk space on this host",
    line: "tmpdir /var/tmp has 5.0 GiB free",
    tag: "host disk-7",
    since_ms: NOW - 10 * 60_000,
    told: false,
    told_at_ms: null,
  });
  assert.equal(view.told[0].told, true);
  assert.equal(view.told[0].told_at_ms, NOW - HOUR);
  const supervisor = view.supervisor;
  assert.ok(supervisor);
  assert.equal(supervisor.live, true);
  assert.equal(supervisor.judgement?.state, "BLOCKED");
  assert.equal(supervisor.judgement?.needs_human, 0.87);
  assert.equal(supervisor.question?.source, "transcript");
  assert.equal(
    supervisor.question?.human?.text,
    "Should I stop `objective_87` now?",
  );
});

test("the list an item comes in decides whether it was told", () => {
  const raw = answer() as { pending: Array<Record<string, unknown>> };
  raw.pending[0].told = true;
  raw.pending[0].told_at_ms = NOW;
  const view = normalizeAttention(raw);
  assert.equal(view.pending[0].told, false);
  assert.equal(view.pending[0].told_at_ms, null);
});

test("a sparse or malformed answer gives empty values, not a crash", () => {
  const view = normalizeAttention({
    pending: [
      null,
      "nonsense",
      { session_id: "session_worker_a", since_ms: "soon", agent_id: 4 },
    ],
    told: "not a list",
    supervisor: { question: { text: "", human: { text: "" }, source: "tmux" } },
    warnings: ["dispatch: stalls: timeout", 7],
  });
  assert.equal(view.pending.length, 1);
  assert.deepEqual(view.pending[0], {
    session_id: "session_worker_a",
    agent_id: null,
    why: "escalation",
    line: "",
    tag: "",
    since_ms: null,
    told: false,
    told_at_ms: null,
  });
  assert.deepEqual(view.told, []);
  assert.equal(view.supervisor?.live, false);
  assert.equal(view.supervisor?.judgement, null);
  assert.deepEqual(view.supervisor?.question, {
    text: null,
    source: null,
    asked_at_ms: null,
    human: null,
  });
  assert.deepEqual(view.warnings, ["dispatch: stalls: timeout"]);
  assert.equal(normalizeAttention({ pending: [] }).supervisor, null);
});

test("an answer that is not the dispatcher's view throws, so the queue falls back", () => {
  assert.throws(() => normalizeAttention(null));
  assert.throws(() => normalizeAttention({ error: "not in fixture" }));
  assert.throws(() => normalizeAttention([]));
});

test("groups the question, then untold, then told, each oldest first", () => {
  const groups = attentionGroups(normalizeAttention(answer()));
  assert.equal(groups.question?.human?.text, "Should I stop `objective_87` now?");
  assert.deepEqual(
    groups.untold.map((item) => item.tag),
    // The disk record is older than the attention; the mission has no time.
    ["host disk-7", "session_worker_objective_87 300", "missions mission-9"],
  );
  assert.deepEqual(
    groups.told.map((item) => item.tag),
    // Told 90 minutes ago before told an hour ago, whatever their records' age.
    [
      "session_worker_objective_86 stall-86-activity-41",
      "session_worker_objective_90 resume-exec_9",
    ],
  );
  assert.equal(groups.count, 6);
});

test("a told item's age is from its digest, else from its record", () => {
  const view = normalizeAttention(answer());
  assert.equal(itemSince(view.told[0]), NOW - HOUR);
  assert.equal(itemSince(view.pending[0]), NOW - 2 * 60_000);
  assert.equal(itemSince({ ...view.told[0], told_at_ms: null }), NOW - 3 * HOUR);
});

test("nothing held and nothing asked is an empty queue", () => {
  const raw = answer() as { pending: unknown[]; told: unknown[]; supervisor: { question: unknown } };
  raw.pending = [];
  raw.told = [];
  raw.supervisor.question = null;
  const groups = attentionGroups(normalizeAttention(raw));
  assert.equal(groups.question, null);
  assert.equal(groups.count, 0);
});

/** A board row, as fleet.ts normalizes it: `attention` false until marked. */
function agent(agent_id: string, judgedAttention = false) {
  return {
    session_id: agent_id.replace(/^agent_/, "session_"),
    agent_id,
    role: agent_id === "agent_supervisor" ? ("supervisor" as const) : ("worker" as const),
    live: true,
    attention: false,
    ended_at_ms: null,
    started_at_ms: NOW - HOUR,
    activity: { last_event_at_ms: NOW - 60_000 },
    judgement: { attention: judgedAttention },
  };
}

test("needs a human: one definition, from the dispatcher's view alone", () => {
  const view = normalizeAttention(answer());
  const needs = needsHuman(view);
  // Three pending, two told and unresolved, and the supervisor's question.
  assert.equal(needs.count, 6);
  assert.equal(needs.count, attentionGroups(view).count, "the queue's count is the sidebar's");
  for (const id of [
    "agent_worker_objective_87",
    "session_worker_objective_87",
    "agent_worker_objective_90",
    "agent_worker_objective_86",
    "agent_supervisor",
    "session_supervisor",
  ]) {
    assert.ok(needs.ids.has(id), id);
  }
});

test("the live page of 2026-11-06 20:30: a judge's flag is no mark, an item is", () => {
  // Worker 106's task is reported and waits for the gate: a managed worker
  // between tasks, which the dispatcher does not escalate, though its judge
  // flags attention. Worker 97 has a told, unresolved item. The supervisor
  // waits at its prompt and asks nothing.
  const raw = answer() as Record<string, unknown> & { supervisor: { question: unknown } };
  raw.pending = [];
  raw.told = [
    {
      session_id: "session_worker_objective_97",
      agent_id: "agent_worker_objective_97",
      why: "nudge did not help",
      line: "stopped again after `continue`",
      tag: "session_worker_objective_97 nudge-412",
      since_ms: NOW - HOUR,
      told: true,
      told_at_ms: NOW - 48 * 60_000,
    },
  ];
  raw.supervisor.question = null;
  const needs = needsHuman(normalizeAttention(raw));
  const agents = markNeedsHuman(
    [
      agent("agent_supervisor"),
      agent("agent_worker_objective_106", true),
      agent("agent_worker_objective_97"),
    ],
    needs,
  );
  assert.deepEqual(
    agents.map((row) => [row.agent_id, row.attention]),
    [
      ["agent_supervisor", false],
      ["agent_worker_objective_106", false],
      ["agent_worker_objective_97", true],
    ],
  );
  assert.equal(needs.count, 1, "the sidebar says what the queue says");
  // The board's "needs a human first" reads the same mark.
  const groups = groupAgents(agents, fleetContext(null));
  const idle = groups.find((group) => group.id === "idle");
  assert.deepEqual(
    idle?.agents.map((row) => row.agent_id),
    ["agent_worker_objective_97", "agent_worker_objective_106"],
  );
});

test("a host or mission item counts but marks no agent; an unknown view marks none", () => {
  const raw = answer() as Record<string, unknown> & { supervisor: { question: unknown } };
  raw.pending = [
    {
      session_id: "host",
      agent_id: null,
      why: "disk space on this host",
      line: "tmpdir /var/tmp has 5.0 GiB free",
      tag: "host disk-7",
      since_ms: NOW,
      told: false,
      told_at_ms: null,
    },
  ];
  raw.told = [];
  raw.supervisor.question = null;
  const needs = needsHuman(normalizeAttention(raw));
  assert.equal(needs.count, 1);
  const supervisor = agent("agent_supervisor");
  assert.equal(agentNeedsHuman(needs, supervisor), false);
  assert.equal(agentNeedsHuman(null, { ...supervisor, attention: true }), false);
  const marked = markNeedsHuman([{ ...supervisor, attention: true }], null);
  assert.equal(marked[0].attention, false, "no view, no mark: none is guessed");
  const same = [supervisor];
  assert.equal(markNeedsHuman(same, needs)[0], supervisor, "an unchanged row is kept as it is");
});

test("the supervisor's lines for the owner are read, not counted", () => {
  const raw = answer() as {
    pending: unknown[];
    told: unknown[];
    supervisor: { question: unknown; for_owner?: unknown };
  };
  raw.supervisor.for_owner = [
    { text: "deploy ffc9fe7; it touches deploy/", at_ms: NOW - 60_000 },
    { text: "approve resident `docs`", at_ms: "soon" },
    { text: "" },
    null,
  ];
  const view = normalizeAttention(raw);
  assert.deepEqual(view.supervisor?.for_owner, [
    { text: "deploy ffc9fe7; it touches deploy/", at_ms: NOW - 60_000 },
    { text: "approve resident `docs`", at_ms: null },
  ]);
  assert.equal(attentionGroups(view).count, 6, "no more than without them");

  // A turn of the new charter asks nothing: the queue is empty, the lines
  // are still there to read.
  raw.pending = [];
  raw.told = [];
  raw.supervisor.question = null;
  const groups = attentionGroups(normalizeAttention(raw));
  assert.equal(groups.count, 0);
  assert.equal(groups.question, null);
  assert.equal(groups.forOwner.length, 2);
  assert.equal(groups.forOwner[0].text, "deploy ffc9fe7; it touches deploy/");

  // An older server sends none.
  delete raw.supervisor.for_owner;
  assert.deepEqual(normalizeAttention(raw).supervisor?.for_owner, []);
});

test("the supervisor asks in at most its HUMAN: line, else its reply's last paragraph", () => {
  const view = normalizeAttention(answer());
  const question = view.supervisor?.question;
  assert.ok(question);
  assert.deepEqual(questionSummary(question), {
    text: "Should I stop `objective_87` now?",
    more: true,
  });
  // No HUMAN: line: the last paragraph of the reply, not the whole reply.
  const reply = "Status at 11:37.\n\nWorkers:\n- task 92 passed\n\n  \nYour decisions:\n1. Accept 92?\n2. Allow 97?\n";
  assert.deepEqual(questionSummary({ ...question, text: reply, human: null }), {
    text: "Your decisions:\n1. Accept 92?\n2. Allow 97?",
    more: true,
  });
  // A reply of one paragraph is all there is: nothing more to show.
  assert.deepEqual(questionSummary({ ...question, text: "Shall I go on?", human: null }), {
    text: "Shall I go on?",
    more: false,
  });
  assert.deepEqual(questionSummary({ ...question, text: null, human: null }), {
    text: null,
    more: false,
  });
  assert.equal(lastParagraph(" \n\n "), null);
  assert.equal(lastParagraph("one\ntwo"), "one\ntwo", "a single line break is no paragraph");
});

test("told and unresolved items fold to a count when nothing is pending", () => {
  const view = normalizeAttention(answer());
  assert.equal(toldFolded(attentionGroups(view)), false, "three pending: shown");
  const calm = attentionGroups({ ...view, pending: [] });
  assert.equal(toldFolded(calm), true);
  assert.equal(calm.told.length, 2, "folded, still counted");
  assert.equal(calm.count, needsHuman({ ...view, pending: [] }).count);
});

test("workers on their own background jobs are listed, never counted", () => {
  const raw = answer() as Record<string, unknown>;
  raw.on_own_jobs = [
    {
      session_id: "session_worker_gate_cache_92",
      agent_id: "agent_worker_gate_cache_92",
      judged_at_ms: NOW - 30_000,
      jobs: [
        { id: "bys3gpa46", kind: "monitor", started_at_ms: NOW - 15 * 60_000, age_ms: 15 * 60_000 },
        { id: "a2adacaabc9f9b048", kind: "agent", started_at_ms: NOW - 5 * 60_000, age_ms: 5 * 60_000 },
        { kind: "bash" },
      ],
    },
    { agent_id: "no session" },
  ];
  const attention = normalizeAttention(raw);
  assert.deepEqual(attention.on_own_jobs, [
    {
      session_id: "session_worker_gate_cache_92",
      agent_id: "agent_worker_gate_cache_92",
      judged_at_ms: NOW - 30_000,
      jobs: [
        { id: "bys3gpa46", kind: "monitor", started_at_ms: NOW - 15 * 60_000, age_ms: 15 * 60_000 },
        { id: "a2adacaabc9f9b048", kind: "agent", started_at_ms: NOW - 5 * 60_000, age_ms: 5 * 60_000 },
      ],
    },
  ]);
  const without = normalizeAttention(answer());
  assert.deepEqual(without.on_own_jobs, [], "an older server lists none");
  const groups = attentionGroups(attention);
  assert.equal(groups.onOwnJobs.length, 1);
  assert.equal(groups.count, attentionGroups(without).count, "working, so not counted");
  assert.equal(
    needsHuman(attention).ids.has("session_worker_gate_cache_92"),
    false,
    "and not marked as needing a human",
  );
});

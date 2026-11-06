import assert from "node:assert/strict";
import { test } from "node:test";

import {
  attentionGroups,
  itemSince,
  normalizeAttention,
} from "../src/attention.ts";

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

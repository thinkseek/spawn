import assert from "node:assert/strict";
import { test } from "node:test";

import {
  actBody,
  actUrl,
  actVerdict,
  kindText,
  normalizeApprovals,
  probeWritable,
  resumeVerdict,
} from "../src/approvals.ts";

/** `/api/approvals` as the server answers it (serve/approvals.rs `items`). */
const ANSWER = {
  generated_at_ms: 1_000,
  items: [
    {
      id: "session_worker_a:3f2c00000000beef",
      kind: "choice",
      session_id: "session_worker_a",
      agent_id: "agent_worker_a",
      backend: "herdr",
      label: "Do you want to run this command?",
      summary: "rm -rf target",
      excerpt: "❯ 1. Yes\n  2. Yes, and don't ask again\n  3. No",
      options: [
        { id: "1", label: "Yes", keys: ["digit1"], text: null, danger: false, action: "allow" },
        { id: "2", label: "Yes, and don't ask again", keys: ["digit2"], text: null, danger: false, action: "allow_always" },
        { id: "3", label: "No", keys: ["digit3"], text: null, danger: false, action: "deny" },
        { id: "4", label: "Something else", keys: ["digit4"], text: null, danger: true, action: "keys" },
        { id: "bad", label: "?", keys: [], action: "explode" },
      ],
      judgement: { state: "BLOCKED", needs_human: 0.9, attention: true, reason: "asks" },
      detected_at_ms: 990,
    },
    {
      id: "session_worker_b:00000000000000aa",
      kind: "attention",
      session_id: "session_worker_b",
      agent_id: "agent_worker_b",
      backend: "herdr",
      label: "waits for the owner",
      summary: "waits for the owner",
      excerpt: "",
      options: [],
      judgement: null,
      detected_at_ms: 980,
    },
    {
      id: "mission:7:4412",
      kind: "mission_halted",
      session_id: null,
      agent_id: null,
      backend: null,
      label: "mission 7 is halted",
      summary: "gate failed twice",
      excerpt: "gate failed twice",
      options: [{ id: "resume", label: "Resume the mission", keys: [], text: null, danger: false, action: "resume" }],
      resume: { endpoint: "/api/missions/7/resume", halt: 4412 },
      judgement: null,
      detected_at_ms: 970,
    },
    { id: "x", kind: "mystery" },
    { kind: "choice" },
  ],
  warnings: ["approvals: a pane: gone", 3],
};

test("the queue reads each known item, its options and its resume", () => {
  const read = normalizeApprovals(ANSWER);
  assert.equal(read.items.length, 3);
  const [choice, attention, mission] = read.items;
  assert.equal(choice.id, "session_worker_a:3f2c00000000beef");
  assert.equal(choice.backend, "herdr");
  assert.deepEqual(
    choice.options.map((option) => [option.id, option.action, option.keys]),
    [
      ["1", "allow", ["digit1"]],
      ["2", "allow_always", ["digit2"]],
      ["3", "deny", ["digit3"]],
      ["4", "keys", ["digit4"]],
    ],
  );
  assert.deepEqual(choice.judgement, { state: "BLOCKED", needs_human: 0.9, reason: "asks" });
  assert.equal(attention.backend, "herdr");
  assert.equal(attention.options.length, 0);
  assert.equal(attention.resume, null);
  assert.equal(mission.backend, null);
  assert.deepEqual(mission.resume, { mission: 7, halt: 4412 });
  assert.deepEqual(read.warnings, ["approvals: a pane: gone"]);
  assert.equal(kindText("yes_no"), "yes / no");
});

test("active approval items carry the herdr backend while legacy backend strings remain defensively readable", () => {
  const read = normalizeApprovals({
    generated_at_ms: 2_000,
    items: [
      {
        id: "session_live:1",
        kind: "confirm",
        session_id: "session_live",
        agent_id: "agent_live",
        backend: "herdr",
        label: "Continue?",
      },
      {
        id: "session_legacy:2",
        kind: "attention",
        session_id: "session_legacy",
        agent_id: "agent_legacy",
        backend: "tmux",
        label: "historical approval record",
      },
      {
        id: "session_missing:3",
        kind: "attention",
        session_id: "session_missing",
        agent_id: "agent_missing",
        label: "missing backend",
      },
      {
        id: "session_non_string:4",
        kind: "attention",
        session_id: "session_non_string",
        agent_id: "agent_non_string",
        backend: 42,
        label: "malformed backend",
      },
    ],
  });
  assert.deepEqual(
    read.items.map((item) => [item.id, item.backend]),
    [
      ["session_live:1", "herdr"],
      ["session_legacy:2", "tmux"],
      ["session_missing:3", null],
      ["session_non_string:4", null],
    ],
  );
});

test("an answer that is not a queue is a failed read", () => {
  for (const raw of [null, [], { items: "no" }, "<html>"]) {
    assert.throws(() => normalizeApprovals(raw), /not a list of approvals/);
  }
});

test("every option's act names its approval and its option (choice_id), a keys answer its keys", () => {
  const [choice, attention, mission] = normalizeApprovals(ANSWER).items;
  for (const option of choice.options.filter((entry) => entry.action !== "keys")) {
    assert.deepEqual(actBody(choice, option, "k1"), {
      action: option.action,
      approval_id: choice.id,
      choice_id: option.id,
      key: "k1",
    });
  }
  assert.deepEqual(actBody(choice, choice.options[3], "k2"), {
    action: "keys",
    approval_id: choice.id,
    choice_id: "4",
    key: "k2",
    keys: ["digit4"],
  });
  assert.equal(actBody(choice, { ...choice.options[3], keys: [] }, "k4"), null);
  assert.equal(actBody(mission, mission.options[0], "k5"), null, "a resume is no act");
  assert.equal(actBody({ ...attention, session_id: null }, choice.options[0], "k6"), null);
  assert.equal(actUrl("session a/b"), "/api/approvals/session%20a%2Fb/act");
});

/** Review B2: three answers that are all "Yes" share one action; the
 * button for the third names the third, never the first. */
test("of three Yes answers, the third's act names the third", () => {
  const three = normalizeApprovals({
    items: [
      {
        id: "s:three",
        kind: "choice",
        session_id: "s",
        label: "Proceed?",
        options: ["1", "2", "3"].map((digit) => ({
          id: `choice${digit}`,
          label: "Yes",
          keys: [`digit${digit}`],
          text: null,
          danger: false,
          action: "allow",
        })),
      },
    ],
  }).items[0];
  assert.deepEqual(actBody(three, three.options[2], "k"), {
    action: "allow",
    approval_id: "s:three",
    choice_id: "choice3",
    key: "k",
  });
});

test("an act's answer: taken only as 202 with command ids, a changed prompt, refused, or worth a retry", () => {
  assert.deepEqual(actVerdict(202, { command_ids: [4, 5], session_id: "s" }), { ok: true, command_ids: [4, 5] });
  assert.deepEqual(actVerdict(null, null), {
    ok: false,
    error: "Cannot reach the server.",
    retry: true,
    stale: false,
    refused: false,
  });
  // A bare 200, or a 202 naming no command, is not an answer taken.
  for (const [status, body] of [
    [200, { command_ids: [4] }],
    [200, null],
    [202, {}],
    [202, { command_ids: [] }],
    [202, { command_ids: ["4"] }],
  ] as const) {
    assert.equal(actVerdict(status, body).ok, false, `${status} ${JSON.stringify(body)}`);
  }
  const stale = actVerdict(409, { error: "the prompt on s is not approval x any more" });
  assert.ok(!stale.ok && stale.stale && !stale.retry);
  const refused = actVerdict(403, { error: "no" });
  assert.ok(!refused.ok && refused.refused);
  const failed = actVerdict(500, "oops");
  assert.ok(!failed.ok && failed.retry && failed.error === "HTTP 500");
});

test("a resume is taken only as 202 with its event", () => {
  assert.deepEqual(resumeVerdict(202, { event: { id: 9 }, mission: 7 }), { ok: true, command_ids: [] });
  assert.equal(resumeVerdict(200, { event: { id: 9 } }).ok, false);
  assert.equal(resumeVerdict(202, {}).ok, false);
  const stale = resumeVerdict(409, { error: "not halted" });
  assert.ok(!stale.ok && stale.stale);
  assert.ok(!resumeVerdict(null, null).ok);
});

test("the probe says whether this page may answer", () => {
  assert.deepEqual(probeWritable(200, { writable: true }), { writable: true, reason: null });
  assert.deepEqual(probeWritable(403, { error: "a write needs the token" }), {
    writable: false,
    reason: "a write needs the token",
  });
  assert.deepEqual(probeWritable(null, null), { writable: false, reason: "Cannot reach the server." });
  assert.equal(probeWritable(200, { writable: false }).writable, false);
});

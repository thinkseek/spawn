// Run by rsi/scenarios/check-web.sh: `node --test test/`. The input box's
// rules (web/src/input.ts): what may be sent, what the probe and the answer
// of `POST /api/session/<id>/input` say, and what became of a message, from
// the session's commands.
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  MAX_INPUT_TEXT,
  commandState,
  keepsKey,
  newKey,
  probeVerdict,
  progress,
  sendAnswer,
  serialPoll,
  settled,
  textProblem,
  withOutcomes,
  withSent,
} from "../src/input.ts";
import type { CommandState, Sent } from "../src/input.ts";

test("a key is a UUID v4, fresh each time, from getRandomValues", () => {
  const key = newKey();
  assert.match(key, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.notEqual(newKey(), key);
  const fixed = newKey((bytes) => bytes.fill(0xff));
  assert.equal(fixed, "ffffffff-ffff-4fff-bfff-ffffffffffff");
});

test("only what a person types may be sent", () => {
  assert.equal(textProblem("Accept 92.\n\tThanks\r\n"), null);
  assert.equal(textProblem("字".repeat(Math.floor(MAX_INPUT_TEXT / 3))), null, "bytes, not characters");
  assert.match(textProblem("") ?? "", /Nothing/);
  assert.match(textProblem(" \n\t ") ?? "", /Nothing/);
  assert.match(textProblem("字".repeat(Math.floor(MAX_INPUT_TEXT / 3) + 1)) ?? "", /Too long/);
  assert.match(textProblem("a\u001b[201~b") ?? "", /control/);
  assert.match(textProblem("a\u009bb") ?? "", /control/);
});

test("the probe says yes on its 200 only, and why not otherwise", () => {
  assert.deepEqual(probeVerdict(200, { writable: true }), { state: "yes" });
  const reason = (status: number | null, body: unknown = {}) => {
    const verdict = probeVerdict(status, body);
    assert.equal(verdict.state, "no", String(status));
    return verdict.state === "no" ? verdict.reason : "";
  };
  assert.match(reason(401), /no token/);
  assert.match(reason(403), /Read-only from this address/);
  // An older server: no such path (its session reader refuses the id).
  for (const status of [400, 404, 405]) assert.match(reason(status), /older spawn serve/);
  assert.match(reason(200, { sessions: [] }), /older spawn serve/);
  assert.match(
    reason(500, { error: "spawn_human: this database has no such login yet; run spawn init-db" }),
    /Input unavailable: spawn_human: .*init-db/,
  );
  assert.match(reason(null), /Cannot reach/);
});

test("an answer is the command made, or the refusal in a line", () => {
  assert.deepEqual(sendAnswer(202, { command_id: 1950, principal: "human" }), {
    ok: true,
    command_id: 1950,
  });
  const refused = sendAnswer(400, { error: "bad request: the text is empty" });
  assert.deepEqual(refused, { ok: false, error: "bad request: the text is empty", status: 400 });
  assert.equal(sendAnswer(404, { error: "no agent has used session_x" }).ok, false);
  const offline = sendAnswer(null, null);
  assert.ok(!offline.ok && offline.status === null && /Cannot reach/.test(offline.error));
  assert.equal(sendAnswer(202, {}).ok, false, "no command, no success");
});

const sent = (over: Partial<Sent> = {}): Sent => ({
  key: "k-1",
  text: "Accept 92.",
  at_ms: 1,
  command_id: 1950,
  error: null,
  ...over,
});

const command = (over: Partial<CommandState> = {}): CommandState => ({
  command_id: 1950,
  state: "pending",
  succeeded: null,
  error: null,
  finished_at_ms: null,
  waiting: false,
  ...over,
});

test("a message is sending, queued, waiting for the turn, then typed or failed", () => {
  assert.deepEqual(progress(sent({ command_id: null }), null), { stage: "sending" });
  assert.deepEqual(progress(sent({ command_id: null, error: "HTTP 403" }), null), {
    stage: "refused",
    error: "HTTP 403",
  });
  assert.deepEqual(progress(sent(), null), { stage: "queued", command_id: 1950 }, "not read yet");
  assert.deepEqual(progress(sent(), [command({ command_id: 7 })]), { stage: "queued", command_id: 1950 });
  assert.deepEqual(progress(sent(), [command()]), { stage: "queued", command_id: 1950 });
  assert.deepEqual(progress(sent(), [command({ waiting: true })]), { stage: "waiting", command_id: 1950 });
  assert.deepEqual(
    progress(sent(), [command({ waiting: true, state: "finished", succeeded: true, finished_at_ms: 9 })]),
    { stage: "typed", command_id: 1950, at_ms: 9 },
  );
  assert.deepEqual(
    progress(sent(), [
      command({
        waiting: true,
        state: "finished",
        succeeded: false,
        error: "the session was stopped before the input was typed",
      }),
    ]),
    { stage: "failed", command_id: 1950, error: "the session was stopped before the input was typed" },
  );
  assert.equal(settled({ stage: "waiting", command_id: 1 }), false);
  assert.equal(settled({ stage: "queued", command_id: 1 }), false);
  assert.equal(settled({ stage: "sending" }), false);
  assert.equal(settled({ stage: "typed", command_id: 1, at_ms: null }), true);
  assert.equal(settled({ stage: "failed", command_id: 1, error: "x" }), true);
  assert.equal(settled({ stage: "refused", error: "x" }), true);
});

test("a resend replaces its first try, and the list stays short", () => {
  const first = sent({ key: "a", command_id: null, error: "Cannot reach the server." });
  const list = withSent([sent({ key: "b" }), sent({ key: "c" })], first);
  assert.deepEqual(list.map((entry) => entry.key), ["a", "b", "c"]);
  const again = withSent(list, sent({ key: "a" }));
  assert.deepEqual(again.map((entry) => [entry.key, entry.error]), [
    ["a", null],
    ["b", null],
    ["c", null],
  ]);
  assert.equal(withSent(again, sent({ key: "d" })).length, 3);
});

test("a command listed by an older server reads as not waiting", () => {
  assert.deepEqual(commandState({ command_id: 3, state: "pending", succeeded: null }), {
    command_id: 3,
    state: "pending",
    succeeded: null,
    error: null,
    finished_at_ms: null,
    waiting: false,
  });
  assert.equal(commandState({ command_id: 3, waiting: true }).waiting, true);
  assert.equal(commandState(null).command_id, null);
});

// GitHub #16 item 8. The commands read are the session's latest few; a
// message typed long ago is no longer among them.
test("a typed message stays typed when newer commands push its own off the list", () => {
  const typed = command({ state: "finished", succeeded: true, finished_at_ms: 9 });
  const newer = Array.from({ length: 20 }, (_, index) => command({ command_id: 2000 + index }));
  let list = withOutcomes([sent()], [typed]);
  assert.deepEqual(progress(list[0], [typed]), { stage: "typed", command_id: 1950, at_ms: 9 });
  const later = progress(list[0], newer);
  assert.deepEqual(later, { stage: "typed", command_id: 1950, at_ms: 9 }, "not queued again");
  assert.equal(settled(later), true, "the box stops reading");

  const failed = command({ state: "finished", succeeded: false, error: "no execution" });
  list = withOutcomes([sent()], [failed]);
  assert.deepEqual(progress(list[0], newer), { stage: "failed", command_id: 1950, error: "no execution" });

  // Nothing ended: the same array (no new render), and nothing recorded.
  const open = [sent(), sent({ key: "b", command_id: null })];
  assert.equal(withOutcomes(open, [command({ waiting: true })]), open);
  assert.equal(withOutcomes(open, null), open);
  assert.equal(withOutcomes(list, newer), list, "recorded once");
  // A command never seen to end and not listed is still queued, and a
  // listing wins over what was recorded.
  assert.deepEqual(progress(sent(), newer), { stage: "queued", command_id: 1950 });
  assert.equal(progress(list[0], [typed]).stage, "typed");
  // A resend (the same key) starts over.
  assert.equal(withSent(list, sent({ command_id: null }))[0].outcome, undefined);
});

test("readings never overlap, and one that was superseded is dropped", async () => {
  const taken: string[] = [];
  const asked: { answer: (value: string) => void; fail: () => void; signal: AbortSignal }[] = [];
  const read = (signal: AbortSignal) =>
    new Promise<string>((resolve, reject) => {
      asked.push({ answer: resolve, fail: () => reject(new Error("HTTP 500")), signal });
    });
  const poll = serialPoll(read, (value: string) => taken.push(value));
  const first = poll.tick();
  // The server is slow: five more ticks pass before it answers.
  await Promise.all([poll.tick(), poll.tick(), poll.tick(), poll.tick(), poll.tick()]);
  assert.equal(asked.length, 1, "one request out at a time");
  asked[0].answer("first");
  await first;
  assert.deepEqual(taken, ["first"]);

  // A failed reading keeps the last one, and the next tick reads again.
  const second = poll.tick();
  asked[1].fail();
  await second;
  assert.deepEqual(taken, ["first"]);
  const third = poll.tick();
  asked[2].answer("third");
  await third;
  assert.deepEqual(taken, ["first", "third"]);

  // Stopped while a request is out (the box reads another session now):
  // its answer is not taken, its request is aborted, and it reads no more.
  const late = poll.tick();
  poll.stop();
  assert.equal(asked[3].signal.aborted, true);
  asked[3].answer("late");
  await late;
  await poll.tick();
  assert.deepEqual(taken, ["first", "third"]);
  assert.equal(asked.length, 4);
});

test("a key the server holds for another message is not sent again", () => {
  assert.equal(keepsKey(null), true, "not reached: the same key, so it types once");
  assert.equal(keepsKey(500), true);
  assert.equal(keepsKey(403), true);
  assert.equal(keepsKey(409), false);
  assert.match(
    (() => {
      const answer = sendAnswer(409, {
        error: "idempotency key 'k' was already used for another input or key: send this one with a new key",
      });
      return answer.ok ? "" : answer.error;
    })(),
    /already used for another input/,
  );
});

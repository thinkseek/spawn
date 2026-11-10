import assert from "node:assert/strict";
import { test } from "node:test";

import {
  forHowLong,
  judgementLine,
  judgementRuns,
  judgementSpeaks,
  runLabel,
  type RecordLike,
} from "../src/agentPage.ts";

const MIN = 60_000;
const T0 = 1_780_000_000_000;
const words = (reason: string | null) => (reason === "IDLE_PROMPT" ? "Idle at its prompt" : reason);
const clock = (ms: number | null) => (ms === null ? "—" : `t${(ms - T0) / MIN}`);

const judged = (judge: string, state: string, more: Partial<RecordLike["observation"]> = {}) => ({
  judge,
  state,
  reason: null,
  rules_state: null,
  attention: null,
  ...more,
});
const record = (at: number, observation: RecordLike["observation"], seq = at): RecordLike => ({
  session_seq: seq,
  recorded_at_ms: T0 + at * MIN,
  observation,
});

test("a judgement reads as one line", () => {
  assert.equal(judgementLine(judged("jev", "WAITING", { reason: "JUDGE_UNAVAILABLE" }), words), "Jev: waiting · judge unavailable");
  assert.equal(judgementLine(judged("rules", "WAITING", { reason: "IDLE_PROMPT" }), words), "Rules: waiting · idle at its prompt");
  assert.equal(judgementLine(judged("rules", "WORKING"), words), "Rules: working");
});

test("the full card is for a judgement that says what the rules did not", () => {
  assert.equal(judgementSpeaks(judged("jev", "WAITING", { rules_state: "WAITING" })), false, "agrees");
  assert.equal(judgementSpeaks(judged("rules", "WORKING")), false, "the rules alone");
  assert.equal(judgementSpeaks(judged("jev", "BLOCKED", { rules_state: "WAITING" })), true, "disagrees");
  assert.equal(judgementSpeaks(judged("jev", "WAITING", { attention: true })), true, "asks for attention");
});

test("the history folds into runs of records that say the same thing", () => {
  const history = [
    record(45, judged("rules", "WORKING")),
    record(44, judged("rules", "WORKING")),
    record(37, judged("rules", "WORKING")),
    record(30, judged("jev", "WAITING", { rules_state: "WORKING" })),
    record(29, judged("jev", "WAITING", { rules_state: "WORKING" })),
    record(20, judged("rules", "WAITING", { reason: "IDLE_PROMPT" })),
    record(19, judged("rules", "WAITING")),
  ];
  const runs = judgementRuns(history);
  assert.deepEqual(runs.map((run) => run.records.length), [3, 1, 1, 1, 1], "a disagreement stands alone; another reason starts a run");
  assert.equal(runLabel(runs[0], clock), "Rules · Working ×3, t37–t45");
  assert.equal(runLabel(runs[1], clock), "Jev · Waiting, t30");
  assert.equal(runs.flatMap((run) => run.records).length, history.length, "no record is lost");
});

test("the summary says for how long", () => {
  const now = T0 + 100 * MIN;
  assert.equal(forHowLong(now - 12 * MIN, now), "for 12 min");
  assert.equal(forHowLong(now - 30_000, now), "for under a minute");
  assert.equal(forHowLong(now - 3 * 60 * MIN, now), "for 3 h");
  assert.equal(forHowLong(now - 50 * 60 * MIN, now), "for 2 d");
  assert.equal(forHowLong(null, now), null);
  assert.equal(forHowLong(now + MIN, now), null, "a clock ahead says nothing");
});

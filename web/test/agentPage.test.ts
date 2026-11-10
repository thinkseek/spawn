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

test("a worker's summary names its task: live's shape has no input, only task_title", async () => {
  const { summaryTask, taskLine } = await import("../src/agentPage.ts");
  const held = { task_id: 247, title: "Cost you can see" };
  // /api/fleet for worker T247 on 99565e2a: task null, task_title set.
  assert.deepEqual(summaryTask({ task: null, task_title: "Cost you can see" }, held), { task: "T247 · Cost you can see" });
  assert.deepEqual(summaryTask({ task: "", task_title: "Cost you can see" }, held), { task: "T247 · Cost you can see" });
  assert.deepEqual(summaryTask({ task: "  ", task_title: "Cost you can see" }, null), { task: "Cost you can see" }, "missions not read yet");
  assert.deepEqual(summaryTask({ task: "Owner note: act on it", task_title: "Talk to the supervisor" }, held), { input: "Owner note: act on it" }, "a person's input leads");
  assert.equal(summaryTask({ task: null, task_title: null }, null), null, "No task recorded.");
  assert.equal(taskLine({ task_id: 12, title: null }, null), "T12");
  assert.equal(taskLine({ task_id: 12, title: "  " }, "From fleet"), "T12", "a blank title is no title");
  assert.equal(taskLine({ task_id: null, title: null }, "From fleet"), "From fleet");
});

test("a history time carries its day when it is not today's", async () => {
  const { historyClock } = await import("../src/agentPage.ts");
  const hms = (ms: number | null) => (ms === null ? "—" : new Date(ms).toTimeString().slice(0, 8));
  const now = new Date(2026, 10, 10, 21, 0, 0).getTime(); // Tue Nov 10, local
  assert.equal(historyClock(new Date(2026, 10, 10, 0, 0, 1).getTime(), now, hms), "00:00:01", "today: the clock alone");
  assert.equal(historyClock(new Date(2026, 10, 9, 23, 59, 59).getTime(), now, hms), "Mon 23:59:59", "yesterday");
  assert.equal(historyClock(new Date(2026, 10, 4, 18, 43, 20).getTime(), now, hms), "Wed 18:43:20", "six days back");
  assert.equal(historyClock(new Date(2026, 10, 3, 18, 43, 20).getTime(), now, hms), "Nov 3 18:43:20", "a week back: its date");
  assert.equal(historyClock(null, now, hms), "—");
  const run = (at: number) => ({ session_seq: at, recorded_at_ms: at, observation: { judge: "rules", state: "WORKING", reason: null, rules_state: null, attention: null } });
  const { judgementRuns, runLabel } = await import("../src/agentPage.ts");
  const across = judgementRuns([run(new Date(2026, 10, 10, 0, 5).getTime()), run(new Date(2026, 10, 9, 23, 55).getTime())]);
  assert.equal(runLabel(across[0], (ms) => historyClock(ms, now, hms)), "Rules · Working ×2, Mon 23:55:00–00:05:00");
});

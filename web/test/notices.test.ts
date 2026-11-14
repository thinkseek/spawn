import assert from "node:assert/strict";
import { test } from "node:test";

import type { Alert } from "../src/api.ts";
import { currentNotices, earlierWords } from "../src/notices.ts";

function row(key: string, state: Alert["state"], superseded_by: string | null = null): Alert {
  return {
    key,
    kind: key.split(":")[0],
    subject: key,
    state,
    fired_event_id: 0,
    fired_at_ms: 0,
    evidence: null,
    cleared_event_id: null,
    cleared_at_ms: null,
    cleared_evidence: null,
    text: key,
    superseded_by,
  };
}

// Objective 151: the Alerts view's notices are each subject's newest, with
// how many came before it.
test("each subject's newest notice is current, with how many came before", () => {
  const alerts = [
    row("task_state:7:10", "notice", "task_state:7:11"),
    row("memory_low:1", "open"),
    row("task_state:7:12", "notice"),
    row("task_state:8:15", "notice"),
    row("task_state:7:11", "notice", "task_state:7:12"),
    row("mission_state:4:20", "notice", "mission_state:4:99"),
    row("task_state:9:1", "cleared"),
  ];
  const got = currentNotices(alerts).map(({ alert, earlier }) => [alert.key, earlierWords(earlier)]);
  assert.deepEqual(got, [
    ["task_state:7:12", "2 earlier"],
    ["task_state:8:15", null],
    // Its next notice is not in the answer: it stands, nothing is hidden.
    ["mission_state:4:20", null],
  ]);
});

test("no notice, or only open and cleared rows, lists nothing", () => {
  assert.deepEqual(currentNotices([]), []);
  assert.deepEqual(currentNotices([row("memory_low:1", "open"), row("task_state:9:1", "cleared")]), []);
});

test("earlier notices are said by number, none by nothing", () => {
  assert.equal(earlierWords(0), null);
  assert.equal(earlierWords(1), "1 earlier");
  assert.equal(earlierWords(10), "10 earlier");
});

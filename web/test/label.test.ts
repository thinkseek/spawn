import assert from "node:assert/strict";
import { test } from "node:test";

import { labels } from "../src/label.ts";

test("a server object's label fields are read, one line each", () => {
  assert.deepEqual(
    labels({
      label: "Supply PA owner work · task 156 · Write the slice 12\n owner contract · cancelled",
      task_title: "Write the slice 12 owner contract",
      mission_name: "Supply PA owner work",
      agent_id: "agent_worker_slice12_156",
    }),
    {
      label: "Supply PA owner work · task 156 · Write the slice 12 owner contract · cancelled",
      task_title: "Write the slice 12 owner contract",
      mission_name: "Supply PA owner work",
    },
  );
});

test("an older server's object has no label: nulls", () => {
  const none = labels({ agent_id: "agent_x" });
  assert.deepEqual(none, { label: null, task_title: null, mission_name: null });
  assert.deepEqual(labels(null), none);
  assert.deepEqual(labels({ label: "  ", task_title: 3 }), none);
});

test("the page's titles and mission names are the server's when it sends them", async () => {
  const { missionName, taskTitle } = await import("../src/ordering.ts");
  const brief = "Owner work on Supply PA: write the slice 12 owner contract the same way slice 11 was written.";
  assert.equal(
    taskTitle({ brief, task_title: "Write the slice 12 owner contract the same way slice 11 was written" }),
    "Write the slice 12 owner contract the same way slice 11 was written",
  );
  // An older server sends none: the page derives one from the brief.
  assert.equal(
    taskTitle({ brief }),
    "Write the slice 12 owner contract the same way slice 11 was written.",
  );
  const text = "Owner work on Supply PA: changes the owner reviews before merging.";
  assert.equal(missionName({ id: 8, text, mission_name: "Supply PA owner work" }), "Supply PA owner work");
  assert.equal(missionName({ id: 8, text }), "Owner work on Supply PA");
});

import assert from "node:assert/strict";
import { test } from "node:test";

import { settlingNote } from "../src/settling.ts";

// Objective 47: the scorecard's cost reads the transcript archive, which a
// running turn's calls reach within a pass; a window whose archive may still
// lack a call made before its end says so under the window.
test("a settled window has no note", () => {
  assert.equal(settlingNote({ settling: false }), null);
});

test("a settling window says its cost may still grow", () => {
  assert.equal(
    settlingNote({ settling: true }),
    "Still settling: a turn that was running at the window's end may add to its cost.",
  );
});

test("a card from a server that says nothing of settling has no note", () => {
  const card = JSON.parse('{"since_ms":0,"until_ms":1,"scopes":[]}');
  assert.equal(settlingNote(card), null);
});

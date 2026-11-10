import assert from "node:assert/strict";
import { test } from "node:test";

import { commandSummary, commandWords } from "../src/commands.ts";
import { KEY_NAMES, keyHint, keyLabel, keyName } from "../src/keys.ts";
import type { KeyPress } from "../src/keys.ts";

const press = (key: string, over: Partial<KeyPress> = {}): KeyPress => ({
  key,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  repeat: false,
  onPanel: true,
  ...over,
});

test("the panel sends Esc, the arrows, Enter, Y and N by name", () => {
  const sent = ["Escape", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Enter", "y", "n"].map((key) =>
    keyName(press(key)),
  );
  assert.deepEqual(sent, KEY_NAMES);
  assert.equal(keyName(press("Y")), "y", "with Shift, still the key");
  assert.equal(keyName(press("N")), "n");
  for (const key of ["a", "Tab", " ", "Backspace", "F1", "Home", "1", "Delete"]) {
    assert.equal(keyName(press(key)), null, key);
  }
});

test("a key goes only from the panel itself, with no modifier, and never on auto-repeat", () => {
  assert.equal(keyName(press("Escape", { onPanel: false })), null, "a text box or a control in the panel");
  assert.equal(keyName(press("ArrowDown", { repeat: true })), null, "a held arrow");
  assert.equal(keyName(press("y", { ctrlKey: true })), null);
  assert.equal(keyName(press("n", { metaKey: true })), null);
  assert.equal(keyName(press("ArrowLeft", { altKey: true })), null, "the browser's back");
  assert.equal(keyName(press("Enter", { isComposing: true })), null, "an input method's Enter");
});

test("keys read as the page shows them, in the hint and in command rows", () => {
  assert.deepEqual(KEY_NAMES.map(keyLabel), ["Esc", "↑", "↓", "←", "→", "Enter", "Y", "N"]);
  assert.equal(keyHint("agent_worker_a"), "Keys go to agent_worker_a: Esc ↑ ↓ ← → Enter Y N");
  assert.equal(commandSummary("key", "escape"), "Esc");
  assert.equal(commandSummary("key", "f9"), "f9", "a name the page does not know shows as it is");
  assert.equal(commandSummary("input", "escape"), "escape", "text is text");
  assert.equal(commandWords("human", "key", "enter"), "You → key · Enter");
  assert.equal(commandWords("spawn_supervisor", "key", "y"), "Supervisor → key · Y");
});

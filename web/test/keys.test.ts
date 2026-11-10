import assert from "node:assert/strict";
import { test } from "node:test";

import { commandSummary, commandWords } from "../src/commands.ts";
import { KEY_NAMES, armHint, clickArms, keyHint, keyLabel, keyName, panelKey, slashFocusesSearch } from "../src/keys.ts";
import type { KeyPress } from "../src/keys.ts";

const press = (key: string, over: Partial<KeyPress> = {}): KeyPress => ({
  key,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  repeat: false,
  onPanel: true,
  armed: true,
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

test("focus alone sends nothing: Esc and the arrows stay the page's until the panel is armed", () => {
  for (const key of ["Escape", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "y", "n"]) {
    assert.equal(panelKey(press(key, { armed: false })), null, key);
    assert.equal(keyName(press(key, { armed: false })), null, key);
  }
  // Enter on the focused panel is the keyboard's click: it arms, and is not sent.
  assert.deepEqual(panelKey(press("Enter", { armed: false })), { arm: true });
  assert.equal(keyName(press("Enter", { armed: false })), null);
  // Armed, Enter is the agent's.
  assert.deepEqual(panelKey(press("Enter")), { send: "enter" });
  assert.deepEqual(panelKey(press("Escape")), { send: "escape" });
  assert.equal(panelKey(press("a")), null, "armed, a key outside the set is still the page's");
});

test("Enter arms only as a plain press on the panel itself", () => {
  assert.equal(panelKey(press("Enter", { armed: false, onPanel: false })), null, "a text box's Enter");
  assert.equal(panelKey(press("Enter", { armed: false, repeat: true })), null);
  assert.equal(panelKey(press("Enter", { armed: false, ctrlKey: true })), null);
  assert.equal(panelKey(press("Enter", { armed: false, isComposing: true })), null);
});

test("only a plain click inside the text that selected nothing arms the panel", () => {
  const click = { button: 0, modified: false, selected: false, inside: true };
  assert.equal(clickArms(click), true);
  assert.equal(clickArms({ ...click, selected: true }), false, "a drag that selected text to copy");
  assert.equal(clickArms({ ...click, inside: false }), false, "on the scrollbar");
  assert.equal(clickArms({ ...click, button: 1 }), false, "the middle button");
  assert.equal(clickArms({ ...click, button: 2 }), false, "the context menu");
  assert.equal(clickArms({ ...click, modified: true }), false, "Shift extends a selection; Ctrl, Alt, Meta are shortcuts");
});

test("keys read as the page shows them, in the hint and in command rows", () => {
  assert.deepEqual(KEY_NAMES.map(keyLabel), ["Esc", "↑", "↓", "←", "→", "Enter", "Y", "N"]);
  assert.equal(keyHint("agent_worker_a"), "Keys go to agent_worker_a: Esc ↑ ↓ ← → Enter Y N");
  assert.equal(armHint("agent_worker_a"), "Click the screen or press Enter to send keys to agent_worker_a");
  assert.equal(commandSummary("key", "escape"), "Esc");
  assert.equal(commandSummary("key", "f9"), "f9", "a name the page does not know shows as it is");
  assert.equal(commandSummary("input", "escape"), "escape", "text is text");
  assert.equal(commandWords("human", "key", "enter"), "You → key · Enter");
  assert.equal(commandWords("spawn_supervisor", "key", "y"), "Supervisor → key · Y");
});

test("/ focuses the search, unless the reader is typing or the terminal panel is armed", () => {
  const press = { key: "/", ctrlKey: false, altKey: false, metaKey: false, inText: false, armed: false, modal: false };
  assert.equal(slashFocusesSearch(press), true);
  assert.equal(slashFocusesSearch({ ...press, inText: true }), false, "in a text box, / is a character");
  assert.equal(slashFocusesSearch({ ...press, armed: true }), false, "armed, the keys are the agent's (P11)");
  assert.equal(slashFocusesSearch({ ...press, modal: true }), false, "a modal drawer is open: focus stays in it");
  assert.equal(slashFocusesSearch({ ...press, ctrlKey: true }), false);
  assert.equal(slashFocusesSearch({ ...press, metaKey: true }), false);
  assert.equal(slashFocusesSearch({ ...press, altKey: true }), false);
  assert.equal(slashFocusesSearch({ ...press, key: "?" }), false);
});

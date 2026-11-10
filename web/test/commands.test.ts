import assert from "node:assert/strict";
import { test } from "node:test";

import { commandGist, commandWords, foldCommands } from "../src/commands.ts";

const DIGEST =
  "Spawn dispatcher: 1 update for you. Check a worker with `/x/spawn tail --session <session>` before acting.\n" +
  "- session_resident_supply — stalled: task #221 (mission #6) has not moved\n";

test("a digest reads as its items, not its header; other text as its first line", () => {
  assert.equal(commandGist(DIGEST), "session_resident_supply — stalled: task #221 (mission #6) has not moved");
  assert.equal(
    commandGist("Spawn dispatcher: 2 updates for you. Check.\n- a — one\n- b — two\n"),
    "a — one (+1 more)",
  );
  assert.equal(commandGist("\n  keep the branch, rebase it\nmore"), "keep the branch, rebase it");
  assert.equal(commandGist(""), null);
  assert.equal(commandGist(null), null);
  // A digest header with no items reads as itself.
  assert.equal(commandGist("Spawn dispatcher: 1 update for you."), "Spawn dispatcher: 1 update for you.");
});

test("a command's row says who sent what: You for the owner, the dispatcher by name", () => {
  assert.equal(commandWords("human", "input", "keep the branch"), "You → input · keep the branch");
  assert.equal(
    commandWords("spawn-dispatch", "input", DIGEST),
    "Dispatcher → input · session_resident_supply — stalled: task #221 (mission #6) has not moved",
  );
  assert.equal(commandWords("spawn-liveness", "resume", null), "Liveness → resume");
  assert.equal(commandWords("someone-new", "stop", null), "someone-new → stop");
});

test("consecutive commands from one sender with one action fold; their texts may differ", () => {
  const record = (principal: string, action: string, text: string) => ({ principal, action, text });
  const runs = foldCommands([
    record("spawn-dispatch", "input", "a"),
    record("spawn-dispatch", "input", "b"),
    record("spawn-dispatch", "input", "c"),
    record("spawn-liveness", "resume", "r"),
    record("spawn-dispatch", "input", "d"),
    record("human", "input", "e"),
  ]);
  assert.deepEqual(
    runs.map((run) => [run.first.principal, run.items.map((item) => item.text).join("")]),
    [
      ["spawn-dispatch", "abc"],
      ["spawn-liveness", "r"],
      ["spawn-dispatch", "d"],
      ["human", "e"],
    ],
  );
});

test("a standing agent's input leads its page for 6 hours; a worker's always; one not among the commands read is old", async () => {
  const { inputAt, inputLeads, INPUT_FRESH_MS } = await import("../src/commands.ts");
  const now = 1_794_200_000_000;
  const records = [
    { text: "newest digest", created_at_ms: now - 60_000 },
    { text: "Owner note: act on it now", created_at_ms: now - 2 * 86_400_000 },
  ];
  const at = inputAt("Owner note: act on it now", records);
  assert.equal(at, now - 2 * 86_400_000);
  assert.equal(inputLeads("supervisor", at, now), false, "two days old");
  assert.equal(inputLeads("supervisor", now - INPUT_FRESH_MS, now), true);
  assert.equal(inputLeads("rsi", inputAt("not in the list", records), now), false);
  assert.equal(inputLeads("worker", at, now), true, "a worker's assignment leads however old");
  // The board cuts the text at 600 characters.
  const long = "x".repeat(900);
  assert.equal(inputAt(long.slice(0, 600), [{ text: long, created_at_ms: 5 }]), 5);
});

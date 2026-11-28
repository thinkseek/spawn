import assert from "node:assert/strict";
import { test } from "node:test";

import type { TimelineEvent } from "../src/api.ts";
import {
  buildSessionStream,
  classifyToolCategory,
  filterStreamSteps,
} from "../src/sessionStream.ts";

function ev(
  id: number,
  atMs: number,
  name: string,
  detail: unknown = {},
  executionId: string | null = "exec_1",
  source = "claude",
): TimelineEvent {
  return {
    event_id: id,
    session_seq: id,
    recorded_at_ms: atMs,
    source,
    execution_id: executionId,
    event_name: name,
    detail,
  };
}

test("classifyToolCategory maps Claude, Codex, and Agy tool names uniformly", () => {
  assert.equal(classifyToolCategory("Edit"), "file_edit");
  assert.equal(classifyToolCategory("Write"), "file_edit");
  assert.equal(classifyToolCategory("apply_patch"), "file_edit");
  assert.equal(classifyToolCategory("replace_file_content"), "file_edit");

  assert.equal(classifyToolCategory("Read"), "file_read");
  assert.equal(classifyToolCategory("Grep"), "file_read");
  assert.equal(classifyToolCategory("read_file"), "file_read");

  assert.equal(classifyToolCategory("Bash"), "shell");
  assert.equal(classifyToolCategory("run_command"), "shell");
  assert.equal(classifyToolCategory("local_shell_call"), "shell");

  assert.equal(classifyToolCategory("web_search"), "tool");
});

test("buildSessionStream builds ordered turns, durations, repeat loops, slow flags, and coarse-turn detection", () => {
  const t0 = 1_794_224_000_000;
  const events: TimelineEvent[] = [
    // Turn 1: rich sequence with file edit, shell slow run, repeated read, thinking, checkpoint
    ev(1, t0 + 1_000, "message.received", { prompt: "Fix bug in Sidebar.tsx" }),
    ev(2, t0 + 2_000, "agent.thinking", { thinking: "Inspect Sidebar drawer state first" }),
    ev(3, t0 + 3_000, "tool.started", { tool_name: "Read", tool_use_id: "t_r1", summary: "web/src/components/Sidebar.tsx" }),
    ev(4, t0 + 4_000, "tool.completed", { tool_use_id: "t_r1" }),
    ev(5, t0 + 5_000, "tool.started", { tool_name: "Read", tool_use_id: "t_r2", summary: "web/src/components/Sidebar.tsx" }),
    ev(6, t0 + 6_000, "tool.completed", { tool_use_id: "t_r2" }),
    ev(7, t0 + 7_000, "tool.started", { tool_name: "Edit", tool_use_id: "t_e1", summary: "web/src/components/Sidebar.tsx" }),
    ev(8, t0 + 8_000, "tool.completed", { tool_use_id: "t_e1" }),
    ev(9, t0 + 9_000, "tool.started", { tool_name: "Bash", tool_use_id: "t_b1", summary: "node --test" }),
    ev(10, t0 + 45_000, "tool.failed", { tool_use_id: "t_b1", error: "exit 1" }),
    ev(11, t0 + 46_000, "artifact.committed", { branch: "feat/ui", commit: "abcdef1234567890", reason: "turn_stopped" }, "exec_1", "bridge"),
    ev(12, t0 + 47_000, "model.usage", { input_tokens: 1200, output_tokens: 350, cache_read_tokens: 500 }),
    ev(13, t0 + 48_000, "turn.stopped", { input_tokens: 100, output_tokens: 50 }),

    // Turn 2: coarse-only turn (only start & end without fine-grained steps)
    ev(14, t0 + 60_000, "message.received", {}),
    ev(15, t0 + 80_000, "turn.stopped", {}),
  ];

  const stream = buildSessionStream(events, t0 + 90_000);
  assert.equal(stream.summary.totalTurns, 2);
  assert.equal(stream.summary.coarseTurns, 1);
  assert.equal(stream.summary.fileReads, 2);
  assert.equal(stream.summary.fileEdits, 1);
  assert.equal(stream.summary.shellRuns, 1);
  assert.equal(stream.summary.failedSteps, 1);
  assert.equal(stream.summary.slowSteps, 1);
  assert.equal(stream.summary.repeatedSteps, 2);
  assert.equal(stream.summary.inputTokens, 1300);
  assert.equal(stream.summary.outputTokens, 400);

  const turn1 = stream.turns[0];
  assert.equal(turn1.coarseOnly, false);
  assert.equal(turn1.prompt, "Fix bug in Sidebar.tsx");
  assert.equal(turn1.steps.map((s) => s.kind).join(","), "thinking,file_read,file_read,file_edit,shell,checkpoint");
  assert.equal(turn1.steps[1].repeatedCount, 2);
  assert.equal(turn1.steps[2].repeatedCount, 2);
  assert.equal(turn1.steps[4].slow, true);
  assert.equal(turn1.steps[4].status, "failed");
  assert.equal(turn1.steps[4].durationMs, 36_000);

  const turn2 = stream.turns[1];
  assert.equal(turn2.coarseOnly, true);
  assert.equal(turn2.durationMs, 20_000);

  assert.equal(
    filterStreamSteps(turn1.steps, "key").map((s) => s.kind).join(","),
    "thinking,file_edit,shell,checkpoint",
  );
  assert.equal(filterStreamSteps(turn1.steps, "issues").length, 3);
});

test("buildSessionStream orders steps and turns by authoritative session_seq even under clock skew", () => {
  const t0 = 1_794_224_000_000;
  // Earlier seq has a later recorded_at_ms timestamp (e.g., host NTP drift or skew).
  const events: TimelineEvent[] = [
    ev(10, t0 + 20_000, "message.received", { prompt: "Step seq test" }),
    ev(11, t0 + 25_000, "tool.started", { tool_name: "Read", tool_use_id: "r_first", summary: "first.ts" }),
    ev(12, t0 + 26_000, "tool.completed", { tool_use_id: "r_first" }),
    ev(13, t0 + 18_000, "tool.started", { tool_name: "Edit", tool_use_id: "e_second", summary: "second.ts" }),
    ev(14, t0 + 19_000, "tool.completed", { tool_use_id: "e_second" }),
    ev(15, t0 + 27_000, "turn.stopped", {}),
  ];

  const stream = buildSessionStream(events, t0 + 30_000);
  assert.equal(stream.turns.length, 1);
  assert.deepEqual(
    stream.turns[0].steps.map((s) => s.summary),
    ["first.ts", "second.ts"],
  );
  assert.deepEqual(
    stream.turns[0].steps.map((s) => s.index),
    [1, 2],
  );
});

test("buildSessionStream detects A -> B -> A -> B alternating loops without false positives on A -> B -> A or A -> B -> C -> A -> B", () => {
  const t0 = 1_794_224_000_000;
  const mkPair = (seqBase: number, id: string, tool: string, summary: string): TimelineEvent[] => [
    ev(seqBase, t0 + seqBase * 1_000, "tool.started", { tool_name: tool, tool_use_id: id, summary }),
    ev(seqBase + 1, t0 + seqBase * 1_000 + 500, "tool.completed", { tool_use_id: id }),
  ];

  // 1) Pure A -> B -> A -> B alternating loop should flag all 4 steps with cycles=2 and repeatPattern='alternating'
  const altLoopEvents: TimelineEvent[] = [
    ev(1, t0 + 500, "message.received", { prompt: "Inspect loop" }),
    ...mkPair(2, "t1", "Read", "src/a.ts"),
    ...mkPair(4, "t2", "Edit", "src/a.ts"),
    ...mkPair(6, "t3", "Read", "src/a.ts"),
    ...mkPair(8, "t4", "Edit", "src/a.ts"),
    ev(10, t0 + 12_000, "turn.stopped", {}),
  ];
  const altStream = buildSessionStream(altLoopEvents, t0 + 15_000);
  assert.equal(altStream.summary.repeatedSteps, 4);
  assert.deepEqual(
    altStream.turns[0].steps.map((s) => [s.repeatedCount, s.repeatPattern]),
    [
      [2, "alternating"],
      [2, "alternating"],
      [2, "alternating"],
      [2, "alternating"],
    ],
  );
  assert.equal(filterStreamSteps(altStream.turns[0].steps, "issues").length, 4);

  // 2) Interrupted A -> B -> C -> A -> B and short A -> B -> A must NOT flag alternating loops
  const nonLoopEvents: TimelineEvent[] = [
    ev(1, t0 + 500, "message.received", { prompt: "Non-loop" }),
    ...mkPair(2, "n1", "Read", "src/a.ts"),
    ...mkPair(4, "n2", "Edit", "src/a.ts"),
    ...mkPair(6, "n3", "Bash", "pnpm test"),
    ...mkPair(8, "n4", "Read", "src/a.ts"),
    ...mkPair(10, "n5", "Edit", "src/a.ts"),
    ev(12, t0 + 20_000, "turn.stopped", {}),
  ];
  const nonLoop = buildSessionStream(nonLoopEvents, t0 + 25_000);
  assert.equal(nonLoop.summary.repeatedSteps, 0);
  assert.ok(nonLoop.turns[0].steps.every((s) => s.repeatedCount === 1 && s.repeatPattern === null));
});

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

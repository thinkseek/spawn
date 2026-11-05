import assert from "node:assert/strict";
import { test } from "node:test";

import type { TimelineEvent } from "../src/api.ts";
import { chainOf, deriveTrace, spanEnd, spanEvents, visibleSpans } from "../src/trace.ts";

const T0 = 1_780_000_000_000;

/** Build a timeline: each entry is [event_name, seconds after T0, detail?,
 * source?, execution_id?]; session_seq follows array order. */
function timeline(
  rows: Array<[string, number, unknown?, string?, (string | null)?]>,
): TimelineEvent[] {
  return rows.map(([name, at, detail, source, execution], index) => ({
    session_seq: index + 1,
    event_id: 1000 + index,
    execution_id: execution === undefined ? "ex_1" : execution,
    source: source ?? (name.startsWith("execution.") ? "bridge" : "claude"),
    event_name: name,
    detail: detail ?? {},
    recorded_at_ms: T0 + at * 1000,
  }));
}

const shape = (trace: ReturnType<typeof deriveTrace>) =>
  trace.spans.map((span) => `${"  ".repeat(span.depth)}${span.label} ${span.state}`);

test("pairs every span kind and nests command → execution → turn → tool", () => {
  const trace = deriveTrace(
    timeline([
      ["command.accepted", 0, { command_id: 7 }, "cli", null],
      ["execution.started", 1, { engine: "claude", command_id: 7 }],
      ["message.received", 2],
      ["tool.started", 3, { tool_name: "Bash", tool_use_id: "toolu_a" }],
      ["tool.started", 3.5, { tool_name: "Read", tool_use_id: "toolu_b" }],
      ["tool.completed", 4, { tool_use_id: "toolu_b", duration_ms: 500 }],
      ["tool.completed", 6, { tool_use_id: "toolu_a", duration_ms: 3000 }],
      ["turn.stopped", 7, { input_tokens: 10, output_tokens: 20 }],
      ["execution.ended", 8, { cause: "exited", exit_code: 0 }],
      ["command.finished", 9, { command_id: 7, succeeded: true }, "cli", null],
    ]),
    T0 + 60_000,
  );
  assert.deepEqual(shape(trace), [
    "command 7 closed",
    "  execution · claude closed",
    "    turn 1 closed",
    "      Bash closed",
    "      Read closed",
  ]);
  const [command, execution, turn, bash, read] = trace.spans;
  assert.equal(command.end, T0 + 9000);
  assert.equal(execution.parentId, command.id);
  assert.equal(execution.outcome, "exited");
  assert.equal(turn.end, T0 + 7000);
  // Interleaved tools pair by tool_use_id, not by order.
  assert.equal(bash.end, T0 + 6000);
  assert.equal(read.end, T0 + 4000);
  assert.equal(trace.openCount, 0);
  assert.equal(trace.start, T0);
  assert.equal(trace.end, T0 + 9000);
});

test("a reused tool_use_id pairs first start with first end", () => {
  const trace = deriveTrace(
    timeline([
      ["tool.started", 0, { tool_name: "A", tool_use_id: "dup" }],
      ["tool.started", 1, { tool_name: "B", tool_use_id: "dup" }],
      ["tool.completed", 2, { tool_use_id: "dup" }],
      ["tool.completed", 3, { tool_use_id: "dup" }],
    ]),
    T0,
  );
  const ends = Object.fromEntries(trace.spans.map((s) => [s.label, s.end]));
  assert.deepEqual(ends, { A: T0 + 2000, B: T0 + 3000 });
});

test("failures: tool.failed, turn.failed, a lost execution, a failed command", () => {
  const trace = deriveTrace(
    timeline([
      ["command.accepted", 0, { command_id: 1 }, "cli", null],
      ["execution.started", 1, { command_id: 1 }],
      ["message.received", 2],
      ["tool.started", 3, { tool_name: "Bash", tool_use_id: "t" }],
      ["tool.failed", 4, { tool_use_id: "t", error: "exit 1" }],
      ["turn.failed", 5, { error: "api" }],
      ["execution.ended", 6, { cause: "lost", exit_code: null }],
      ["command.finished", 7, { command_id: 1, succeeded: false, error: "boom" }, "cli", null],
    ]),
    T0,
  );
  const failed = Object.fromEntries(
    trace.spans.map((s) => [s.kind, [s.failed, s.outcome]]),
  );
  assert.deepEqual(failed, {
    command: [true, "boom"],
    execution: [true, "lost"],
    turn: [true, "api"],
    tool: [true, "exit 1"],
  });
});

test("open spans run to now and push the trace end out", () => {
  const now = T0 + 120_000;
  const trace = deriveTrace(
    timeline([
      ["execution.started", 0],
      ["message.received", 1],
      ["tool.started", 2, { tool_name: "Bash", tool_use_id: "t" }],
    ]),
    now,
  );
  assert.deepEqual(shape(trace), [
    "execution open",
    "  turn 1 open",
    "    Bash open",
  ]);
  assert.equal(trace.openCount, 3);
  assert.equal(trace.end, now);
  assert.equal(spanEnd(trace.spans[2], now), now);
  // A clock behind the span's start never draws it backwards.
  assert.equal(spanEnd(trace.spans[2], T0), T0 + 2000);
});

test("the bridge's execution end closes it; the agent's exit report is a marker", () => {
  const trace = deriveTrace(
    timeline([
      ["execution.started", 0],
      ["message.received", 1],
      ["tool.started", 1.5, { tool_name: "Bash", tool_use_id: "t" }],
      ["execution.ended", 2, { cause: "exited", exit_code: 0 }, "claude"],
      ["execution.ended", 3, { cause: "stopped", exit_code: null }, "bridge"],
    ]),
    T0 + 10_000,
  );
  const [execution, turn] = trace.spans;
  assert.equal(execution.state, "closed");
  assert.equal(execution.outcome, "stopped");
  assert.equal(execution.end, T0 + 3000);
  // The turn was still open when the process went: it ends with it.
  assert.equal(turn.end, T0 + 3000);
  assert.equal(turn.outcome, "unfinished");
  // So does a tool call it left open.
  const tool = trace.spans[2];
  assert.deepEqual([tool.state, tool.end, tool.outcome], ["closed", T0 + 3000, "unfinished"]);
  assert.equal(trace.openCount, 0);
  assert.deepEqual(
    turn.markers.map((m) => [m.event.event_name, m.event.source]),
    [["execution.ended", "claude"]],
  );
});

test("point events land on the innermost open span, else the session", () => {
  const trace = deriveTrace(
    timeline([
      ["notice.started", 0, {}, "bridge", null],
      ["command.accepted", 0.5, { command_id: 3 }, "cli", null],
      ["command.step", 0.6, { command_id: 3, step: "spawned" }, "cli", null],
      ["execution.started", 1, { command_id: 3 }],
      ["message.received", 2],
      ["notification.received", 3, { notification_type: "idle" }],
      ["turn.stopped", 4],
      ["artifact.committed", 5, { artifact: "a".repeat(40) }],
      ["merge.conflicted", 5.5, { files: ["x"] }],
      ["execution.ended", 6, { cause: "exited" }],
    ]),
    T0,
  );
  const markers = Object.fromEntries(
    trace.spans.map((s) => [s.kind, s.markers.map((m) => m.event.event_name)]),
  );
  assert.deepEqual(markers, {
    command: ["command.step"],
    execution: ["artifact.committed", "merge.conflicted"],
    turn: ["notification.received"],
  });
  assert.deepEqual(trace.markers.map((m) => m.event.event_name), ["notice.started"]);
  const conflict = trace.spans[1].markers[1];
  assert.equal(conflict.failed, true);
  assert.equal(trace.spans[1].markers[0].failed, false);
});

test("an end without a start is an orphan, zero length at its own time", () => {
  const trace = deriveTrace(
    timeline([
      ["execution.started", 0],
      ["message.received", 1],
      ["tool.completed", 2, { tool_use_id: "never_started" }],
      ["turn.stopped", 3],
      ["turn.stopped", 4],
      ["command.finished", 5, { command_id: 9, succeeded: true }, "cli", null],
      ["execution.ended", 6, { cause: "exited" }, "bridge", "ex_gone"],
    ]),
    T0,
  );
  const orphans = trace.spans.filter((s) => s.state === "orphan");
  assert.deepEqual(
    orphans.map((s) => [s.kind, s.start, s.end, s.startEvent]),
    [
      ["tool", T0 + 2000, T0 + 2000, null],
      ["turn", T0 + 4000, T0 + 4000, null],
      ["command", T0 + 5000, T0 + 5000, null],
      ["execution", T0 + 6000, T0 + 6000, null],
    ],
  );
  // The orphan tool still nests under the turn that covered its time, and
  // the orphan turn under its execution.
  const tool = orphans.find((s) => s.kind === "tool")!;
  assert.equal(trace.byId.get(tool.parentId!)!.label, "turn 1");
  const turn = orphans.find((s) => s.kind === "turn")!;
  assert.equal(trace.byId.get(turn.parentId!)!.kind, "execution");
});

test("out of order: an end logged before its start still pairs", () => {
  const trace = deriveTrace(
    timeline([
      ["command.finished", 0, { command_id: 4, succeeded: true }, "cli", null],
      ["command.accepted", 0, { command_id: 4 }, "cli", null],
      ["execution.ended", 9, { cause: "exited" }, "bridge", "ex_2"],
      ["execution.started", 1, {}, "bridge", "ex_2"],
      ["tool.completed", 3, { tool_use_id: "t", duration_ms: 1 }],
      ["tool.started", 2, { tool_name: "Edit", tool_use_id: "t" }],
    ]),
    T0 + 100_000,
  );
  assert.ok(trace.spans.every((s) => s.state === "closed"), shape(trace).join("\n"));
  const execution = trace.spans.find((s) => s.kind === "execution")!;
  assert.equal(execution.start, T0 + 1000);
  assert.equal(execution.end, T0 + 9000);
  const tool = trace.spans.find((s) => s.kind === "tool")!;
  assert.equal(tool.end, T0 + 3000);
  assert.equal(trace.openCount, 0);
});

test("out of order clocks: an end stamped before its start is clamped", () => {
  const trace = deriveTrace(
    timeline([
      ["tool.started", 5, { tool_name: "Bash", tool_use_id: "t" }],
      ["tool.completed", 4, { tool_use_id: "t" }],
    ]),
    T0,
  );
  assert.equal(trace.spans[0].start, T0 + 5000);
  assert.equal(trace.spans[0].end, T0 + 5000);
});

test("events are paired in session_seq order even if handed over unsorted", () => {
  const rows = timeline([
    ["message.received", 0],
    ["turn.stopped", 1],
    ["message.received", 2],
    ["turn.stopped", 3],
  ]);
  const trace = deriveTrace([...rows].reverse(), T0);
  assert.deepEqual(
    trace.spans.map((s) => [s.label, s.start, s.end]),
    [
      ["turn 1", T0, T0 + 1000],
      ["turn 2", T0 + 2000, T0 + 3000],
    ],
  );
});

test("separate executions keep separate turns and tools", () => {
  const trace = deriveTrace(
    timeline([
      ["execution.started", 0, {}, "bridge", "ex_a"],
      ["execution.started", 0, {}, "bridge", "ex_b"],
      ["message.received", 1, {}, "claude", "ex_a"],
      ["message.received", 1, {}, "claude", "ex_b"],
      ["tool.started", 2, { tool_name: "InB", tool_use_id: "b1" }, "claude", "ex_b"],
      ["turn.stopped", 3, {}, "claude", "ex_a"],
    ]),
    T0 + 5000,
  );
  assert.deepEqual(shape(trace), [
    "execution open",
    "  turn 1 closed",
    "execution open",
    "  turn 1 open",
    "    InB open",
  ]);
});

test("chain, events and collapsing", () => {
  const trace = deriveTrace(
    timeline([
      ["execution.started", 0],
      ["message.received", 1],
      ["tool.started", 2, { tool_name: "A", tool_use_id: "a" }],
      ["notification.received", 2.5],
      ["tool.completed", 3, { tool_use_id: "a" }],
      ["turn.stopped", 4],
      ["message.received", 5],
      ["turn.stopped", 6],
    ]),
    T0,
  );
  const [execution, turn1, tool, turn2] = trace.spans;
  assert.deepEqual(
    [...chainOf(trace, turn1.id)].sort(),
    [execution.id, turn1.id, tool.id].sort(),
  );
  assert.deepEqual(
    spanEvents(turn1).map((e) => e.event_name),
    ["message.received", "notification.received", "turn.stopped"],
  );
  assert.deepEqual(
    visibleSpans(trace, new Set([turn1.id])).map((s) => s.id),
    [execution.id, turn1.id, turn2.id],
  );
  assert.deepEqual(visibleSpans(trace, new Set([execution.id])).map((s) => s.id), [
    execution.id,
  ]);
});

test("an empty timeline is an empty trace at now", () => {
  const trace = deriveTrace([], T0);
  assert.equal(trace.spans.length, 0);
  assert.equal(trace.start, T0);
  assert.equal(trace.end, T0);
});

test("span ids survive new events, so a selection stays on its span", () => {
  const rows = timeline([
    ["execution.started", 0],
    ["tool.completed", 1, { tool_use_id: "lost_start" }],
    ["message.received", 2],
  ]);
  const before = deriveTrace(rows, T0).spans.map((s) => s.id);
  const more = timeline([
    ["execution.started", 0],
    ["tool.completed", 1, { tool_use_id: "lost_start" }],
    ["message.received", 2],
    ["tool.started", 3, { tool_name: "A", tool_use_id: "a" }],
    ["turn.stopped", 4],
  ]);
  const after = new Set(deriveTrace(more, T0).spans.map((s) => s.id));
  for (const id of before) assert.ok(after.has(id), id);
});

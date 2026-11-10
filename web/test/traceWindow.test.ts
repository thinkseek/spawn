import assert from "node:assert/strict";
import { test } from "node:test";

import type { TimelineEvent } from "../src/api.ts";
import { foldIdle, LINEAR, sameFoldMap } from "../src/timeFold.ts";
import { refollow } from "../src/timeWindow.ts";
import type { Follow } from "../src/timeWindow.ts";
import { deriveTrace } from "../src/trace.ts";
import {
  BREAK_PX,
  TRACE_MIN_GAP_MS,
  fitExecution,
  runWindow,
  spanCounts,
  traceFold,
} from "../src/traceWindow.ts";

const T0 = 1_780_000_000_000;
const S = 1000;
const M = 60 * S;
const H = 60 * M;

/** [event_name, ms after T0, detail?, execution_id?]; session_seq follows order. */
function timeline(rows: Array<[string, number, unknown?, string?]>): TimelineEvent[] {
  return rows.map(([name, at, detail, execution], index) => ({
    session_seq: index + 1,
    event_id: 1000 + index,
    execution_id: execution ?? "ex_1",
    source: name.startsWith("execution.") ? "bridge" : "claude",
    event_name: name,
    detail: detail ?? {},
    recorded_at_ms: T0 + at,
  }));
}

/** Two executions five hours apart: ex_1 closed, ex_2 open (or closed). */
function twoRuns(secondOpen: boolean) {
  const rows: Array<[string, number, unknown?, string?]> = [
    ["execution.started", 0, { engine: "claude" }, "ex_1"],
    ["message.received", 1 * S, {}, "ex_1"],
    ["tool.started", 2 * S, { tool_name: "Bash", tool_use_id: "a" }, "ex_1"],
    ["tool.completed", 5 * S, { tool_use_id: "a" }, "ex_1"],
    ["turn.stopped", 6 * S, {}, "ex_1"],
    ["execution.ended", 7 * S, { cause: "exited", exit_code: 0 }, "ex_1"],
    ["execution.started", 5 * H, { engine: "claude" }, "ex_2"],
    ["message.received", 5 * H + S, {}, "ex_2"],
    ["tool.started", 5 * H + 2 * S, { tool_name: "Read", tool_use_id: "b" }, "ex_2"],
  ];
  if (!secondOpen) {
    rows.push(["tool.completed", 5 * H + 3 * S, { tool_use_id: "b" }, "ex_2"]);
    rows.push(["turn.stopped", 5 * H + 4 * S, {}, "ex_2"]);
    rows.push(["execution.ended", 5 * H + 5 * S, { cause: "exited", exit_code: 0 }, "ex_2"]);
  }
  return deriveTrace(timeline(rows), T0 + 5 * H + 10 * S);
}

test("the window fits the open execution, else the last one, else the selected span's", () => {
  const live = twoRuns(true);
  const open = fitExecution(live);
  assert.equal(open?.kind, "execution");
  assert.equal(open?.start, T0 + 5 * H);
  assert.equal(open?.end, null);

  const done = twoRuns(false);
  assert.equal(fitExecution(done)?.start, T0 + 5 * H, "the last execution when none is open");

  // A tool selected in the first execution: that execution, not the current one.
  const bash = done.spans.find((span) => span.label === "Bash");
  assert.ok(bash);
  const first = fitExecution(done, bash.id);
  assert.equal(first?.start, T0);
  // A selected execution is its own; a selected id that is gone falls back.
  assert.equal(fitExecution(done, first?.id ?? null), first);
  assert.equal(fitExecution(done, "nothing")?.start, T0 + 5 * H);
  // An execution still open beats a later one that ended (the open one is
  // the current one).
  const stale = deriveTrace(
    timeline([
      ["execution.started", 0, { engine: "claude" }, "ex_1"],
      ["execution.started", H, { engine: "claude" }, "ex_2"],
      ["execution.ended", H + S, { cause: "exited", exit_code: 0 }, "ex_2"],
    ]),
    T0 + 2 * H,
  );
  assert.equal(fitExecution(stale)?.start, T0);
  // No executions at all.
  const bare = deriveTrace(timeline([["tool.started", 0, { tool_name: "Bash", tool_use_id: "x" }]]), T0 + S);
  assert.equal(fitExecution(bare), null);
});

test("the five idle hours between the runs fold; the runs keep their scale", () => {
  const trace = twoRuns(false);
  const fold = traceFold(trace, trace.end);
  assert.equal(fold.breaks.length, 1);
  assert.equal(fold.breaks[0].from, T0 + 7 * S);
  assert.equal(fold.breaks[0].to, T0 + 5 * H);
  // Inside a run, durations are true.
  assert.equal(fold.fold(T0 + 5 * S) - fold.fold(T0 + 2 * S), 3 * S);
  assert.equal(fold.fold(T0 + 5 * H + 3 * S) - fold.fold(T0 + 5 * H + 2 * S), S);
  // The gap is far narrower than five hours.
  assert.ok(fold.fold(T0 + 5 * H) - fold.fold(T0 + 7 * S) < 30 * M);
});

test("a gap under ten minutes stays; a loose marker counts as activity", () => {
  const near = deriveTrace(
    timeline([
      ["tool.started", 0, { tool_name: "A", tool_use_id: "a" }],
      ["tool.completed", S, { tool_use_id: "a" }],
      ["tool.started", TRACE_MIN_GAP_MS - S, { tool_name: "B", tool_use_id: "b" }],
      ["tool.completed", TRACE_MIN_GAP_MS, { tool_use_id: "b" }],
    ]),
    T0 + H,
  );
  assert.equal(traceFold(near, near.end).breaks.length, 0);

  // Two tools an hour apart fold, unless an event with no span sits between.
  const rows: Array<[string, number, unknown?, string?]> = [
    ["tool.started", 0, { tool_name: "A", tool_use_id: "a" }, ""],
    ["tool.completed", S, { tool_use_id: "a" }, ""],
    ["tool.started", H, { tool_name: "B", tool_use_id: "b" }, ""],
    ["tool.completed", H + S, { tool_use_id: "b" }, ""],
  ];
  const apart = deriveTrace(timeline(rows), T0 + 2 * H);
  assert.equal(traceFold(apart, apart.end).breaks.length, 1);
  const marked = deriveTrace(
    timeline([...rows.slice(0, 2), ["supervisor.digest", 30 * M, {}, ""], ...rows.slice(2)]),
    T0 + 2 * H,
  );
  assert.ok(marked.markers.some((marker) => marker.at === T0 + 30 * M), "the digest is a loose marker");
  assert.equal(traceFold(marked, marked.end).breaks.length, 2, "two half-hour gaps, not one hour");
});

test("an open span runs to the end it is given, so the live end never folds away", () => {
  const live = twoRuns(true);
  const fold = traceFold(live, T0 + 6 * H);
  // The open execution covers 5 h to 6 h: no break after it.
  assert.ok(fold.breaks.every((item) => item.to <= T0 + 5 * H));
});

test("an execution and a turn left open for hours fold their idle middle", () => {
  // The supervisor's shape: one execution open for days, a turn waiting
  // between digests; the open spans alone would leave nothing to fold.
  const trace = deriveTrace(
    timeline([
      ["execution.started", 0, { engine: "claude" }],
      ["message.received", S],
      ["tool.started", 2 * S, { tool_name: "Bash", tool_use_id: "a" }],
      ["tool.completed", 3 * S, { tool_use_id: "a" }],
      ["turn.stopped", 4 * S],
      ["message.received", 6 * H],
      ["tool.started", 6 * H + S, { tool_name: "Read", tool_use_id: "b" }],
      ["tool.completed", 6 * H + 2 * S, { tool_use_id: "b" }],
    ]),
    T0 + 20 * H,
  );
  const fold = traceFold(trace, T0 + 20 * H);
  // 4 s to 6 h idle; then the open turn waits from 6 h to now (20 h).
  assert.equal(fold.breaks.length, 2);
  assert.equal(fold.breaks[0].from, T0 + 4 * S);
  assert.equal(fold.breaks[0].to, T0 + 6 * H);
  assert.equal(fold.breaks[1].from, T0 + 6 * H + 2 * S);
  assert.equal(fold.breaks[1].to, T0 + 20 * H, "the live end is kept");

  // An event on the waiting turn (a digest arriving) is activity too.
  const digested = deriveTrace(
    timeline([
      ["execution.started", 0, { engine: "claude" }],
      ["message.received", S],
      ["tool.started", 2 * S, { tool_name: "Bash", tool_use_id: "a" }],
      ["tool.completed", 3 * S, { tool_use_id: "a" }],
      ["supervisor.digest", 10 * H],
    ]),
    T0 + 20 * H,
  );
  const turn = digested.spans.find((span) => span.kind === "turn");
  assert.ok(turn?.markers.some((marker) => marker.at === T0 + 10 * H), "the digest sits on the open turn");
  assert.deepEqual(
    traceFold(digested, T0 + 20 * H).breaks.map((item) => [item.from - T0, item.to - T0]),
    [[3 * S, 10 * H], [10 * H, 20 * H]],
  );
});

test("with a plot width each break is a band of 12 to 24 px, all of them at most 30% of the plot", () => {
  const trace = twoRuns(false);
  const plotW = 800;
  const fold = traceFold(trace, trace.end, { plotW });
  const width = fold.fold(trace.end) - fold.fold(trace.start);
  assert.equal(fold.breaks.length, 1);
  const band = (fold.keep / width) * plotW;
  assert.ok(band > BREAK_PX / 2 - 0.01 && band <= BREAK_PX, `one break: 12 to 24 px (${band})`);

  // Forty short bursts an hour apart: forty breaks share 30% of the plot.
  const rows: Array<[string, number, unknown?, string?]> = [];
  for (let index = 0; index < 40; index += 1) {
    rows.push(["tool.started", index * H, { tool_name: "Bash", tool_use_id: `t${index}` }, ""]);
    rows.push(["tool.completed", index * H + 5 * S, { tool_use_id: `t${index}` }, ""]);
  }
  const busy = deriveTrace(timeline(rows), T0 + 40 * H);
  const many = traceFold(busy, busy.end, { plotW });
  const total = many.fold(busy.end) - many.fold(busy.start);
  assert.equal(many.breaks.length, 39);
  const share = (many.breaks.length * many.keep) / total;
  assert.ok(share > 0.15 && share <= 0.3, `breaks take ${share}`);
  // A live session that grows a little keeps the same width.
  const more = deriveTrace(
    timeline([...rows, ["tool.started", 40 * H, { tool_name: "Bash", tool_use_id: "z" }, ""],
      ["tool.completed", 40 * H + 2 * S, { tool_use_id: "z" }, ""]]),
    T0 + 41 * H,
  );
  assert.equal(traceFold(more, more.end, { plotW }).keep, many.keep);
  // Without a width, the fold is the missions one (keep at least 2 min).
  assert.ok(traceFold(busy, busy.end).keep >= 2 * M);
});

test("a window keeps its real times when the fold changes (rev-f F1)", () => {
  const trace = twoRuns(true);
  const run = fitExecution(trace);
  assert.ok(run);
  const fold = traceFold(trace, T0 + 6 * H, { plotW: 800 });
  // The default fit of the open run, then the Linear toggle: still from
  // just before the run (at most a minute), not hours before it (the
  // folded number read as a real time).
  const fitted = runWindow(run, fold, T0 + 6 * H);
  assert.equal(fitted.mode, "since");
  const linear = refollow(fitted, (time) => LINEAR.fold(fold.unfold(time)));
  assert.equal(linear.mode, "since");
  const from = linear.mode === "since" ? linear.from : 0;
  assert.ok(from <= run.start && from >= run.start - 60 * S - 1, `${(run.start - from) / S} s before the run`);
  // A finished run fits whole, with the same real air on both sides.
  const done = twoRuns(false);
  const last = fitExecution(done);
  assert.ok(last && last.end !== null);
  const doneFold = traceFold(done, done.end, { plotW: 800 });
  const whole = refollow(runWindow(last, doneFold, done.end), (time) => LINEAR.fold(doneFold.unfold(time)));
  assert.equal(whole.mode, "fixed");
  if (whole.mode === "fixed") {
    assert.ok(whole.domain.from <= last.start && whole.domain.from >= last.start - 60 * S - 1);
    assert.ok(whole.domain.to >= last.end && whole.domain.to <= last.end + 60 * S + 1);
  }

  // A zoomed window on a live session whose band width changes (here a
  // narrower plot): the same real moments before and after.
  const other = traceFold(trace, T0 + 6 * H, { plotW: 300 });
  assert.notEqual(other.keep, fold.keep);
  const zoomed: Follow = {
    mode: "fixed",
    domain: { from: fold.fold(T0 + 2 * S), to: fold.fold(T0 + 5 * H + 2 * S) },
  };
  const moved = refollow(zoomed, (time) => other.fold(fold.unfold(time)));
  assert.equal(moved.mode, "fixed");
  if (moved.mode === "fixed") {
    assert.ok(Math.abs(other.unfold(moved.domain.from) - (T0 + 2 * S)) < 1);
    assert.ok(Math.abs(other.unfold(moved.domain.to) - (T0 + 5 * H + 2 * S)) < 1);
  }
  // Fit and tail follow the extent and keep as they are.
  assert.deepEqual(refollow({ mode: "fit" }, () => 0), { mode: "fit" });
  assert.deepEqual(refollow({ mode: "tail", width: 5 }, () => 0), { mode: "tail", width: 5 });
});

test("a break is never wider than the shortest gap that folds (rev-f F2)", () => {
  // Three hours of activity (a call every 5 min), a 12 min gap, then a
  // 20 min tool call, on a 200 px phone plot.
  const rows: Array<[string, number, unknown?, string?]> = [];
  for (let index = 0; index <= 36; index += 1) {
    rows.push(["tool.started", index * 5 * M, { tool_name: "Bash", tool_use_id: `t${index}` }, ""]);
    rows.push(["tool.completed", index * 5 * M + S, { tool_use_id: `t${index}` }, ""]);
  }
  const after = 3 * H + S + 12 * M;
  rows.push(["tool.started", after, { tool_name: "Bash", tool_use_id: "long" }, ""]);
  rows.push(["tool.completed", after + 20 * M, { tool_use_id: "long" }, ""]);
  const trace = deriveTrace(timeline(rows), T0 + after + 20 * M);
  const fold = traceFold(trace, trace.end, { plotW: 200 });
  assert.equal(fold.breaks.length, 1);
  assert.ok(fold.keep <= TRACE_MIN_GAP_MS, `keep ${fold.keep / M} min`);
  // The band ends at or before the long call's start.
  const item = fold.breaks[0];
  assert.ok(item.at + Math.min(fold.keep, item.to - item.from) <= fold.fold(T0 + after) + 1);
});

test("counts say what is running first, then what is done", () => {
  assert.equal(spanCounts(twoRuns(true)), "3 running · 3 done");
  assert.equal(spanCounts(twoRuns(false)), "6 done");
});

test("a live fold rebuilt a tick later is the same map; a new break or band width is not (rev-f F4)", () => {
  // Two bursts of tool work an hour apart, the last one still running.
  const intervals = (end: number) => [
    [T0, T0 + M],
    [T0 + H, end],
  ] as const;
  const tick = foldIdle(intervals(T0 + H + 5 * S), { minGap: TRACE_MIN_GAP_MS, keep: 2 ** 16 });
  const next = foldIdle(intervals(T0 + H + 6_000), { minGap: TRACE_MIN_GAP_MS, keep: 2 ** 16 });
  assert.notEqual(tick, next);
  assert.ok(sameFoldMap(tick, next), "only the live end moved");
  for (const time of [T0 - 1, T0 + 30_000, T0 + 30 * M, T0 + H + 5_500]) {
    assert.equal(next.fold(time), tick.fold(time), "and it maps every time alike");
  }
  const halved = foldIdle(intervals(T0 + H + 6_000), { minGap: TRACE_MIN_GAP_MS, keep: 2 ** 15 });
  assert.ok(!sameFoldMap(tick, halved), "a new band width moves the times after the break");
  const another = foldIdle([...intervals(T0 + 3 * H), [T0 + 3 * H + 20 * M, T0 + 3 * H + 21 * M]], {
    minGap: TRACE_MIN_GAP_MS,
    keep: 2 ** 16,
  });
  assert.ok(!sameFoldMap(tick, another), "a new break moves them");
  // One break, the same keep, but moved: its end (the second burst later),
  // or its start (the first burst longer).
  const later = foldIdle([[T0, T0 + M], [T0 + 2 * H, T0 + 2 * H + 6 * S]], { minGap: TRACE_MIN_GAP_MS, keep: 2 ** 16 });
  assert.ok(!sameFoldMap(tick, later), "a break that ends later");
  const longer = foldIdle([[T0, T0 + 2 * M], [T0 + H, T0 + H + 6 * S]], { minGap: TRACE_MIN_GAP_MS, keep: 2 ** 16 });
  assert.ok(!sameFoldMap(tick, longer), "a break that starts later");
  // The trace's own fold, as TraceView builds it each second while a tool
  // runs: same map tick to tick, so the window is not refolded.
  const live = twoRuns(true);
  const at = (end: number) => traceFold(live, end, { plotW: 800 });
  assert.ok(sameFoldMap(at(T0 + 5 * H + 10 * S), at(T0 + 5 * H + 11 * S)));
  assert.equal(at(T0 + 5 * H + 10 * S).breaks.length, 1);
  // With no break every fold is the identity, LINEAR included.
  assert.ok(sameFoldMap(LINEAR, foldIdle([[T0, T0 + 1_000]])));
  assert.ok(sameFoldMap(LINEAR, { ...LINEAR, keep: 2 ** 16 }), "whatever its keep");
  assert.ok(!sameFoldMap(LINEAR, tick));
});

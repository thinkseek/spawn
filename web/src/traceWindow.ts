/* What the session trace shows first (design-ui-information.md §4.4): its
 * axis fits the current execution, or the selected span's, and idle gaps
 * fold away, so a supervisor's 137-hour session no longer puts its live work
 * in a sliver at the right. Pure, tested in web/test/traceWindow.test.ts. */

import { foldIdle } from "./timeFold.ts";
import type { TimeFold } from "./timeFold.ts";
import type { Follow } from "./timeWindow.ts";
import { spanEnd } from "./trace.ts";
import type { Span, Trace } from "./trace.ts";

/** A gap with no span and no event of at least this long folds (§4.4). */
export const TRACE_MIN_GAP_MS = 10 * 60_000;

/** The idle fold of a trace. Activity is what happened: a tool call for
 * its whole run (an open one to `end`), every other span's start and end
 * (an open one's end being `end`, now), and every event on a span or loose.
 * A command, an execution or a turn that stays open is not activity in
 * itself: a supervisor's execution runs for days and its turn waits for
 * hours between digests, and folding only where no span is open would fold
 * nothing. A gap of TRACE_MIN_GAP_MS or more with none of these becomes a
 * marked break; the spans that cross it are drawn through it. */
export function traceFold(
  trace: Trace,
  end: number,
  options: { minGap?: number; plotW?: number } = {},
): TimeFold {
  const minGap = options.minGap ?? TRACE_MIN_GAP_MS;
  const intervals: Array<readonly [number, number]> = [];
  for (const span of trace.spans) {
    const until = spanEnd(span, end);
    if (span.kind === "tool") intervals.push([span.start, until]);
    else {
      intervals.push([span.start, span.start]);
      intervals.push([until, until]);
    }
    for (const marker of span.markers) intervals.push([marker.at, marker.at]);
  }
  for (const marker of trace.markers) intervals.push([marker.at, marker.at]);
  const first = foldIdle(intervals, { minGap });
  if (!options.plotW || first.breaks.length === 0) return first;
  return foldIdle(intervals, { minGap, keep: breakKeep(first, options.plotW, minGap) });
}

/** Each break's folded width (§4.4: a fixed band, at most 24 px), in ms
 * of folded time, for a plot `plotW` px wide showing the whole fold: a
 * session's work is seconds of tool calls between hours of waiting, so a
 * width in ms of the active time (the missions timeline's 3%) would let the
 * breaks take the axis. All the breaks together take at most 30% of it.
 * Rounded down to a power of two, so the width holds while a live session
 * grows (each change of it moves every folded time; the window follows
 * them, refollow): a band is 12 to 24 px wide. And never wider than
 * `minGap`, the shortest gap that folds (rev-f F2): a gap is squeezed to
 * min(gap, keep), so a keep past it would draw a band wider than its gap,
 * over the activity after it (a 200 px phone plot, hours of activity). */
export const BREAK_PX = 24;
export function breakKeep(fold: TimeFold, plotW: number, minGap = TRACE_MIN_GAP_MS): number {
  const count = fold.breaks.length;
  const active = Math.max(
    fold.blocks.reduce((sum, block) => sum + (block.to - block.from), 0),
    1_000,
  );
  const px = Math.min(BREAK_PX, (plotW * 0.3) / count);
  // keep / (active + count * keep) = px / plotW
  const exact = (px * active) / Math.max(plotW - count * px, 1);
  return 2 ** Math.floor(Math.log2(Math.max(Math.min(exact, minGap), 1)));
}

/** The execution span a window fits: the selected span's own execution
 * (itself, or its nearest execution ancestor) when one is selected and has
 * one; else the open execution that started last; else the execution that
 * started last; null when the trace has none. */
export function fitExecution(trace: Trace, selectedId: string | null = null): Span | null {
  let at = selectedId ? trace.byId.get(selectedId) : undefined;
  while (at) {
    if (at.kind === "execution") return at;
    at = at.parentId ? trace.byId.get(at.parentId) : undefined;
  }
  let open: Span | null = null;
  let last: Span | null = null;
  for (const span of trace.spans) {
    if (span.kind !== "execution") continue;
    if (!last || span.start >= last.start) last = span;
    if (span.end === null && (!open || span.start >= open.start)) open = span;
  }
  return open ?? last;
}

/** The air before and after a fitted run, in real time: 5% of it, at most
 * a minute. Real, not folded: a pad taken on the folded axis lands inside
 * the break before the run and stands for hours of it, which a linear axis
 * (the toggle, refollow) would then show. */
export function runPad(from: number, to: number): number {
  return Math.min(Math.max((to - from) * 0.05, 0), 60_000);
}

/** The window that fits `run` on the axis of `fold` (§4.4): a running one
 * from just before its start to the live end, followed; a finished one
 * whole, with a little air. `end` is the live end (now while it runs). */
export function runWindow(run: Span, fold: TimeFold, end: number): Follow {
  const until = spanEnd(run, end);
  const pad = runPad(run.start, until);
  if (run.end === null) return { mode: "since", from: fold.fold(run.start - pad) };
  return { mode: "fixed", domain: { from: fold.fold(run.start - pad), to: fold.fold(until + pad) } };
}

/** "2 running · 125 done": the spans still open first, the rest after. */
export function spanCounts(trace: Trace): string {
  const running = trace.spans.filter((span) => span.end === null).length;
  const done = trace.spans.length - running;
  return running > 0 ? `${running} running · ${done} done` : `${done} done`;
}

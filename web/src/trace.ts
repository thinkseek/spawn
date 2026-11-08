/* Span derivation for the trace view.
 *
 * The timeline is a flat event list ordered by session_seq. This module pairs
 * start and end events into spans, nests them (command → execution → turn →
 * tool) and pins every remaining event as a point marker on the innermost
 * span that was open when it happened. It is pure — no React, no DOM — so the
 * pairing rules are unit-tested in web/test/trace.test.ts.
 *
 * Pairing runs in session_seq order, which is the order the log committed the
 * events in. Clocks are only used for placement: a span whose end is stamped
 * before its start is drawn zero-length at its start rather than backwards.
 * An end that arrives before its start (two hook processes racing) is held
 * and paired when the start shows up; an end whose start never shows up
 * becomes an orphan span, drawn as a zero-length mark at the end's time. */

import type { TimelineEvent } from "./api";

export type SpanKind = "command" | "execution" | "turn" | "tool";

/** open: no end yet, runs to "now". orphan: an end without a start. */
export type SpanState = "closed" | "open" | "orphan";

export interface TraceMarker {
  at: number;
  event: TimelineEvent;
  failed: boolean;
}

export interface Span {
  id: string;
  kind: SpanKind;
  label: string;
  start: number;
  /** null while the span is open. */
  end: number | null;
  state: SpanState;
  failed: boolean;
  /** One-word reason shown beside a failed or ended span (cause, error). */
  outcome: string | null;
  parentId: string | null;
  depth: number;
  childIds: string[];
  executionId: string | null;
  startEvent: TimelineEvent | null;
  endEvent: TimelineEvent | null;
  markers: TraceMarker[];
}

export interface Trace {
  /** Spans in display order: depth-first, siblings by start time. */
  spans: Span[];
  byId: Map<string, Span>;
  /** Point events with no span to sit on. */
  markers: TraceMarker[];
  /** Earliest event time, and the latest of the last event and (when any
   * span is open) now. */
  start: number;
  end: number;
  openCount: number;
}

const FAILURE = /fail|error|lost|crash|conflict/i;

function field(detail: unknown, key: string): unknown {
  if (detail && typeof detail === "object" && !Array.isArray(detail)) {
    return (detail as Record<string, unknown>)[key];
  }
  return undefined;
}

function text(detail: unknown, key: string): string | null {
  const value = field(detail, key);
  if (typeof value === "string" && value !== "") return value;
  if (typeof value === "number" && Number.isFinite(value)) return String(value);
  return null;
}

/** Does this point event mean something went wrong? */
export function isFailureEvent(event: TimelineEvent): boolean {
  if (FAILURE.test(event.event_name)) return true;
  if (event.event_name === "execution.ended") {
    return text(event.detail, "cause") === "lost";
  }
  return false;
}

function compareEvents(a: TimelineEvent, b: TimelineEvent): number {
  return a.session_seq - b.session_seq || a.event_id - b.event_id;
}

/** A FIFO of unmatched starts or ends per pairing key. */
class Pending<T> {
  private queues = new Map<string, T[]>();
  push(key: string, value: T) {
    const queue = this.queues.get(key);
    if (queue) queue.push(value);
    else this.queues.set(key, [value]);
  }
  shift(key: string): T | undefined {
    const queue = this.queues.get(key);
    const value = queue?.shift();
    if (queue && queue.length === 0) this.queues.delete(key);
    return value;
  }
  /** Remove and return every queued value that matches. */
  drain(match: (value: T) => boolean): T[] {
    const out: T[] = [];
    for (const [key, queue] of this.queues) {
      const keep = queue.filter((value) => !match(value));
      out.push(...queue.filter(match));
      if (keep.length === 0) this.queues.delete(key);
      else this.queues.set(key, keep);
    }
    return out;
  }
  values(): T[] {
    return [...this.queues.values()].flat();
  }
}

export function deriveTrace(input: TimelineEvent[], now: number): Trace {
  const events = [...input].sort(compareEvents);
  const spans: Span[] = [];
  const byId = new Map<string, Span>();
  const sessionMarkers: TraceMarker[] = [];

  const open = (
    kind: SpanKind,
    label: string,
    event: TimelineEvent,
    parentId: string | null,
  ): Span => {
    const span: Span = {
      // Keyed on the event that opened it (the end, for an orphan), so the
      // id survives new events arriving and a selection stays put.
      id: `${kind}:${event.event_id}`,
      kind,
      label,
      start: event.recorded_at_ms,
      end: null,
      state: "open",
      failed: false,
      outcome: null,
      parentId,
      depth: 0,
      childIds: [],
      executionId: event.execution_id,
      startEvent: event,
      endEvent: null,
      markers: [],
    };
    spans.push(span);
    byId.set(span.id, span);
    return span;
  };

  const close = (span: Span, event: TimelineEvent) => {
    span.end = Math.max(span.start, event.recorded_at_ms);
    span.endEvent = event;
    span.state = "closed";
    applyEnd(span, event);
  };

  const orphan = (
    kind: SpanKind,
    label: string,
    event: TimelineEvent,
    parentId: string | null,
  ): Span => {
    const span = open(kind, label, event, parentId);
    span.startEvent = null;
    span.end = span.start;
    span.endEvent = event;
    span.state = "orphan";
    applyEnd(span, event);
    return span;
  };

  // Per-execution context, as of the current position in the log.
  const executionSpan = new Map<string, Span>();
  const turnCount = new Map<string, number>();
  const openTurns = new Map<string, Span[]>();
  const execKey = (event: TimelineEvent) => event.execution_id ?? "";

  const commandSpan = new Map<string, Span>();
  const pendingTools = new Pending<Span>();
  const earlyToolEnds = new Pending<TimelineEvent>();
  const pendingCommands = new Pending<Span>();
  const earlyCommandEnds = new Pending<TimelineEvent>();
  const earlyExecutionEnds = new Map<string, TimelineEvent>();

  const currentTurn = (key: string) => openTurns.get(key)?.at(-1) ?? null;

  /** The innermost span open in this event's execution, for a marker. */
  const innermost = (event: TimelineEvent): Span | null => {
    const key = execKey(event);
    const turn = currentTurn(key);
    if (turn) return turn;
    if (event.execution_id) {
      const execution = executionSpan.get(event.execution_id);
      if (execution) return execution;
    }
    const commandId = text(event.detail, "command_id");
    if (commandId) return commandSpan.get(commandId) ?? null;
    return null;
  };

  const mark = (event: TimelineEvent, on: Span | null) => {
    const marker: TraceMarker = {
      at: event.recorded_at_ms,
      event,
      failed: isFailureEvent(event),
    };
    if (on) on.markers.push(marker);
    else sessionMarkers.push(marker);
  };

  const executionParent = (event: TimelineEvent): string | null => {
    const commandId = text(event.detail, "command_id");
    return (commandId && commandSpan.get(commandId)?.id) || null;
  };

  const turnParent = (event: TimelineEvent): string | null =>
    (event.execution_id && executionSpan.get(event.execution_id)?.id) || null;

  const toolParent = (event: TimelineEvent): string | null =>
    currentTurn(execKey(event))?.id ?? turnParent(event);

  for (const event of events) {
    const key = execKey(event);
    switch (event.event_name) {
      case "command.accepted": {
        const id = text(event.detail, "command_id");
        if (!id) break;
        const span = open("command", `command ${id}`, event, null);
        commandSpan.set(id, span);
        const early = earlyCommandEnds.shift(id);
        if (early) close(span, early);
        else pendingCommands.push(id, span);
        continue;
      }
      case "command.finished": {
        const id = text(event.detail, "command_id");
        if (!id) break;
        const span = pendingCommands.shift(id);
        if (span) close(span, event);
        else earlyCommandEnds.push(id, event);
        continue;
      }
      case "execution.started": {
        if (!event.execution_id) break;
        const engine = text(event.detail, "engine");
        const span = open(
          "execution",
          engine ? `execution · ${engine}` : "execution",
          event,
          executionParent(event),
        );
        executionSpan.set(event.execution_id, span);
        const early = earlyExecutionEnds.get(event.execution_id);
        if (early) {
          earlyExecutionEnds.delete(event.execution_id);
          close(span, early);
        }
        continue;
      }
      case "execution.ended": {
        // Only the bridge's end is authoritative; the agent's own report of
        // its exit is a point event on the span.
        if (event.source !== "bridge" || !event.execution_id) break;
        const span = executionSpan.get(event.execution_id);
        if (span && span.state === "open") {
          close(span, event);
          for (const turn of openTurns.get(key) ?? []) {
            // A turn cannot outlive its process: it ended with it, unfinished.
            turn.end = Math.max(turn.start, event.recorded_at_ms);
            turn.state = "closed";
            turn.outcome = "unfinished";
          }
          openTurns.delete(key);
          // Nor can a tool call: one still open went down with the process.
          for (const tool of pendingTools.drain(
            (pending) => pending.executionId === event.execution_id,
          )) {
            tool.end = Math.max(tool.start, event.recorded_at_ms);
            tool.state = "closed";
            tool.outcome = "unfinished";
          }
        } else if (!span) {
          earlyExecutionEnds.set(event.execution_id, event);
        } else {
          mark(event, span);
        }
        continue;
      }
      case "message.received": {
        const index = (turnCount.get(key) ?? 0) + 1;
        turnCount.set(key, index);
        const span = open("turn", `turn ${index}`, event, turnParent(event));
        const turns = openTurns.get(key);
        if (turns) turns.push(span);
        else openTurns.set(key, [span]);
        continue;
      }
      case "turn.stopped":
      case "turn.failed": {
        const turns = openTurns.get(key);
        const span = turns?.pop();
        if (turns && turns.length === 0) openTurns.delete(key);
        if (span) close(span, event);
        else orphan("turn", "turn", event, turnParent(event));
        continue;
      }
      case "tool.started": {
        const id = text(event.detail, "tool_use_id");
        const name = text(event.detail, "tool_name") ?? "tool";
        const span = open("tool", name, event, toolParent(event));
        if (!id) continue;
        const early = earlyToolEnds.shift(id);
        if (early) close(span, early);
        else pendingTools.push(id, span);
        continue;
      }
      case "tool.completed":
      case "tool.failed": {
        const id = text(event.detail, "tool_use_id");
        if (!id) break;
        const span = pendingTools.shift(id);
        if (span) close(span, event);
        else earlyToolEnds.push(id, event);
        continue;
      }
    }
    mark(event, innermost(event));
  }

  // Ends whose start never arrived.
  for (const event of earlyToolEnds.values()) {
    orphan("tool", "tool", event, toolParentAt(event, spans));
  }
  for (const event of earlyCommandEnds.values()) {
    const id = text(event.detail, "command_id") ?? "?";
    orphan("command", `command ${id}`, event, null);
  }
  for (const event of earlyExecutionEnds.values()) {
    orphan("execution", "execution", event, null);
  }

  // Tie children to parents, depth, display order.
  const roots: Span[] = [];
  for (const span of spans) {
    const parent = span.parentId ? byId.get(span.parentId) : undefined;
    if (parent) parent.childIds.push(span.id);
    else {
      span.parentId = null;
      roots.push(span);
    }
  }
  const byStart = (a: Span, b: Span) =>
    a.start - b.start || logOrder(a) - logOrder(b);
  const ordered: Span[] = [];
  const visit = (span: Span, depth: number) => {
    span.depth = depth;
    ordered.push(span);
    const children = span.childIds.map((id) => byId.get(id)!).sort(byStart);
    span.childIds = children.map((child) => child.id);
    for (const child of children) visit(child, depth + 1);
  };
  for (const root of roots.sort(byStart)) visit(root, 0);

  let start = Infinity;
  let end = -Infinity;
  for (const event of events) {
    start = Math.min(start, event.recorded_at_ms);
    end = Math.max(end, event.recorded_at_ms);
  }
  const openCount = ordered.filter((span) => span.state === "open").length;
  if (openCount > 0) end = Math.max(end, now);
  if (!Number.isFinite(start)) {
    start = now;
    end = now;
  }

  return { spans: ordered, byId, markers: sessionMarkers, start, end, openCount };
}

function logOrder(span: Span): number {
  return (span.startEvent ?? span.endEvent)?.session_seq ?? 0;
}

/** For an orphan tool end: the turn of its execution that covers its time. */
function toolParentAt(event: TimelineEvent, spans: Span[]): string | null {
  let best: Span | null = null;
  for (const span of spans) {
    if (span.executionId !== event.execution_id) continue;
    if (span.kind !== "turn" && span.kind !== "execution") continue;
    if (span.start > event.recorded_at_ms) continue;
    if (span.end !== null && span.end < event.recorded_at_ms) continue;
    if (!best || (best.kind === "execution" && span.kind === "turn")) best = span;
  }
  return best?.id ?? null;
}

function applyEnd(span: Span, event: TimelineEvent) {
  switch (event.event_name) {
    case "tool.failed":
      span.failed = true;
      span.outcome = text(event.detail, "error") ?? "failed";
      break;
    case "turn.failed":
      span.failed = true;
      span.outcome = text(event.detail, "error") ?? "failed";
      break;
    case "execution.ended": {
      const cause = text(event.detail, "cause");
      span.outcome = cause;
      span.failed = cause === "lost";
      break;
    }
    case "command.finished":
      if (field(event.detail, "succeeded") === false) {
        span.failed = true;
        span.outcome = text(event.detail, "error") ?? "failed";
      }
      break;
  }
}

/** The span's end for drawing: open spans run to now. */
export function spanEnd(span: Span, now: number): number {
  return span.end ?? Math.max(span.start, now);
}

/** Ancestors and descendants of a span, the chain a hover highlights. */
export function chainOf(trace: Trace, id: string): Set<string> {
  const chain = new Set<string>();
  let cursor = trace.byId.get(id);
  while (cursor) {
    chain.add(cursor.id);
    cursor = cursor.parentId ? trace.byId.get(cursor.parentId) : undefined;
  }
  const stack = [id];
  while (stack.length > 0) {
    const span = trace.byId.get(stack.pop()!);
    if (!span) continue;
    chain.add(span.id);
    stack.push(...span.childIds);
  }
  return chain;
}

/** Every event a span owns: its start, its markers, its end, in log order. */
export function spanEvents(span: Span): TimelineEvent[] {
  const events = span.markers.map((marker) => marker.event);
  if (span.startEvent) events.push(span.startEvent);
  if (span.endEvent) events.push(span.endEvent);
  return events.sort(compareEvents);
}

/** Spans visible once collapsed subtrees are hidden. */
export function visibleSpans(trace: Trace, collapsed: Set<string>): Span[] {
  const out: Span[] = [];
  let hiddenBelow: number | null = null;
  for (const span of trace.spans) {
    if (hiddenBelow !== null && span.depth > hiddenBelow) continue;
    hiddenBelow = null;
    out.push(span);
    if (collapsed.has(span.id) && span.childIds.length > 0) {
      hiddenBelow = span.depth;
    }
  }
  return out;
}

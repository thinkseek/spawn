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
import { commandHead, commandSummary } from "./commands.ts";

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
  /** One-word reason shown beside a failed or ended span (cause, error):
   * a failed tool's is `exit N` or its error's first line. */
  outcome: string | null;
  /** A tool call's key input, as recorded on its `tool.started` (its
   * command's first line, its file, its pattern); null on other spans and
   * on calls recorded before calls carried one. */
  summary: string | null;
  /** The summary is the call's whole input (`summary_whole` on its
   * `tool.started`): only such calls fold into a ×N run. */
  summaryWhole: boolean;
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

/** An execution as the session's record knows it (api.ts `Execution`):
 * what the trace needs when its window starts after the execution did. */
export interface KnownExecution {
  execution_id: string;
  engine: string | null;
  started_at_ms: number;
  ended_at_ms: number | null;
  cause: string | null;
}

/** Spans from a window of a session's events. `executions` (the session's
 * record of them) seeds the span of an execution whose start is before the
 * window, so its turns and tools still sit under it; such an execution's
 * turns are not numbered, as its earlier turns are not in the window. */
/** A command as the commands endpoint knows it (fleet.ts
 * `CommandRecord`): who sent it, what it is, its text. */
export interface KnownCommand {
  principal: string;
  action: string;
  text: string | null;
}

export function deriveTrace(
  input: TimelineEvent[],
  now: number,
  executions: readonly KnownExecution[] = [],
  commands: ReadonlyMap<string, KnownCommand> = new Map(),
  /** The name of the session's agent (`supervisor`): a person's input
   * reads `You → supervisor` ([`commandHead`]). None: `You → input`. */
  to: string | null = null,
): Trace {
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
      summary: null,
      summaryWhole: false,
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
  // Each command span's id, for what a key went ahead of.
  const pendingIds = new Map<Span, string>();
  const earlyCommandEnds = new Pending<TimelineEvent>();
  const earlyExecutionEnds = new Map<string, TimelineEvent>();

  const currentTurn = (key: string) => openTurns.get(key)?.at(-1) ?? null;

  // Executions that began before the window: seeded from the record.
  const seeded = new Map<string, KnownExecution>();
  const started = new Set(
    events.filter((event) => event.event_name === "execution.started").map((event) => event.execution_id),
  );
  const inWindow = new Set(events.map((event) => event.execution_id));
  for (const execution of executions) {
    const id = execution.execution_id;
    if (started.has(id) || !inWindow.has(id)) continue;
    const span: Span = {
      id: `execution:${id}`,
      kind: "execution",
      label: execution.engine ? `execution · ${execution.engine}` : "execution",
      start: execution.started_at_ms,
      end: null,
      state: "open",
      failed: false,
      outcome: null,
      summary: null,
      summaryWhole: false,
      parentId: null,
      depth: 0,
      childIds: [],
      executionId: id,
      startEvent: null,
      endEvent: null,
      markers: [],
    };
    spans.push(span);
    byId.set(span.id, span);
    executionSpan.set(id, span);
    seeded.set(id, execution);
  }

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
        // In words when the commands endpoint knows it (§4.3).
        const known = commands.get(id);
        const span = open("command", known ? commandHead(known.principal, known.action, to) : `command ${id}`, event, null);
        if (known) span.summary = commandSummary(known.action, known.text);
        // A key never waits for the turn (backlog P1): one sent while inputs
        // still wait goes ahead of them, and the row says so.
        if (known?.action === "key") {
          const ahead = pendingCommands
            .values()
            .filter((pending) => commands.get(pendingIds.get(pending) ?? "")?.action === "input").length;
          if (ahead > 0) {
            span.summary = `${span.summary ?? "key"} · ahead of ${ahead} waiting input${ahead === 1 ? "" : "s"}`;
          }
        }
        commandSpan.set(id, span);
        pendingIds.set(span, id);
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
        const label = seeded.has(key) ? "turn" : `turn ${index}`;
        const span = open("turn", label, event, turnParent(event));
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
        span.summary = text(event.detail, "summary");
        span.summaryWhole = span.summary !== null && field(event.detail, "summary_whole") === true;
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

  // A seeded execution the record says ended, with its end outside the
  // window: closed where the record puts it.
  for (const [id, execution] of seeded) {
    const span = executionSpan.get(id);
    if (span && span.state === "open" && execution.ended_at_ms !== null) {
      span.end = Math.max(span.start, execution.ended_at_ms);
      span.state = "closed";
      span.outcome = execution.cause;
      span.failed = execution.cause === "lost";
    }
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
      span.outcome = toolOutcome(text(event.detail, "error"));
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

/** A failed tool call's outcome in a few words: Bash's "Exit code N" as
 * `exit N`, else the first line of its error. The whole error stays on the
 * event. */
export function toolOutcome(error: string | null): string {
  const first = error
    ?.split("\n")
    .map((line) => line.trim())
    .find((line) => line !== "");
  if (!first) return "failed";
  const exit = /^(?:Error:\s*)?Exit code (\d+)\b/i.exec(first);
  return exit ? `exit ${exit[1]}` : first;
}

const BASH_CHARS = 80;
const PATH_TOOLS = new Set(["Edit", "MultiEdit", "Write", "Read", "NotebookEdit"]);

/** A long path as its first directory, an ellipsis and its file:
 * `crates/…/integrate.rs`. A Read's `:120-180` stays on the file. */
export function shortPath(path: string): string {
  const parts = path.split("/");
  const named = parts.filter((part) => part !== "");
  if (named.length <= 3) return path;
  const lead = path.startsWith("/") ? `/${named[0]}` : named[0];
  return `${lead}/…/${named[named.length - 1]}`;
}

/** A tool call's key input as its row shows it: a command without what
 * only sets up for it (a leading `cd <dir> &&`, `NAME=value;` and
 * `NAME=value cmd` assignments, a variable that names a program read as
 * that program's name), to its first 80 characters; a file path
 * shortened. The full text is the row's title. */
export function shortSummary(tool: string, summary: string): string {
  if (PATH_TOOLS.has(tool)) return shortPath(summary);
  if (tool !== "Bash") return summary;
  const command = commandProper(summary);
  return command.length > BASH_CHARS ? `${command.slice(0, BASH_CHARS - 1)}…` : command;
}

/** `cd <dir>` followed by `&&` or `;`. */
const LEADING_CD = /^cd\s+(?:"[^"]*"|'[^']*'|\S+)\s*(?:&&|;)\s*/;
const NAME = /^[A-Za-z_][A-Za-z0-9_]*=/;

/** The length of a shell word's value at the start of `text`: a quoted
 * string, a `$(…)` (balanced), or a run of non-blanks. */
function valueLength(text: string): number {
  if (text.startsWith("$(")) {
    let depth = 0;
    for (let index = 0; index < text.length; index += 1) {
      if (text[index] === "(") depth += 1;
      else if (text[index] === ")" && (depth -= 1) === 0) return index + 1;
    }
    return text.length;
  }
  const quote = text[0];
  if (quote === '"' || quote === "'") {
    const close = text.indexOf(quote, 1);
    return close < 0 ? text.length : close + 1;
  }
  return /^[^\s;&|]*/.exec(text)?.[0].length ?? 0;
}

/** A command with its set-up taken off the front (see [`shortSummary`]). */
export function commandProper(summary: string): string {
  let rest = summary;
  const programs = new Map<string, string>();
  for (;;) {
    const cd = LEADING_CD.exec(rest);
    if (cd) {
      rest = rest.slice(cd[0].length);
      continue;
    }
    const name = NAME.exec(rest);
    if (!name) break;
    const value = rest.slice(name[0].length);
    const length = valueLength(value);
    const after = value.slice(length);
    const separator = /^\s*(?:&&|;)\s*|^\s+/.exec(after);
    if (!separator || after.slice(separator[0].length) === "") break;
    const word = value.slice(0, length).replace(/^["']|["']$/g, "");
    // `B=/path/to/spawn`: `$B` below reads as `spawn`.
    if (/^[^\s$`]*\/[^\s$`/]+$/.test(word)) programs.set(name[0].slice(0, -1), word.slice(word.lastIndexOf("/") + 1));
    rest = after.slice(separator[0].length);
  }
  if (rest === "") return summary;
  return rest.replace(/\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?/g, (whole, variable: string) => programs.get(variable) ?? whole);
}

/** A span's row text: `Bash · cargo test -p spawn-cli` for a tool call
 * with a summary, its label otherwise. */
export function rowText(span: Span, trace?: Trace): string {
  switch (span.kind) {
    case "tool":
      return span.summary ? `${span.label} · ${shortSummary(span.label, span.summary)}` : span.label;
    case "command":
      return span.summary ? `${span.label} · ${span.summary}` : span.label;
    case "turn": {
      // `turn 3 · 14 tools`: the tools are its children.
      if (!trace) return span.label;
      const tools = span.childIds.filter((id) => trace.byId.get(id)?.kind === "tool").length;
      return tools > 0 ? `${span.label} · ${tools} tool${tools === 1 ? "" : "s"}` : span.label;
    }
    case "execution":
      return span.state === "open" ? `${span.label} · running` : span.label;
  }
}

/** Two tool calls one row can stand for: the same tool with the same
 * whole input, under the same parent, ending the same way. A summary that
 * leaves input out (an Edit's path, a heredoc's first line, a redacted or
 * cut command) does not say two calls are the same, and neither does no
 * summary at all. */
function sameCall(a: Span, b: Span): boolean {
  if (a.kind === "command" || b.kind === "command") return sameSender(a, b);
  return (
    a.kind === "tool" &&
    b.kind === "tool" &&
    a.parentId === b.parentId &&
    a.label === b.label &&
    a.summaryWhole &&
    b.summaryWhole &&
    a.summary === b.summary &&
    a.failed === b.failed &&
    a.outcome === b.outcome &&
    a.state !== "open" &&
    b.state !== "open"
  );
}

/** Two commands one row can stand for: the same sender and action (their
 * texts differ, and the run opens to list them), neither running nor
 * having started anything under it, ending the same way. */
function sameSender(a: Span, b: Span): boolean {
  return (
    a.kind === "command" &&
    b.kind === "command" &&
    a.label === b.label &&
    !a.label.startsWith("command ") &&
    a.childIds.length === 0 &&
    b.childIds.length === 0 &&
    a.state === "closed" &&
    b.state === "closed" &&
    a.failed === b.failed
  );
}

/** One visible row: a span, and the run of identical calls it stands for
 * (`run[0]` is the span; a run of one is a plain row). */
export interface SpanRow {
  span: Span;
  run: Span[];
  /** The run is opened: its calls follow as their own rows. */
  unfolded: boolean;
}

/** The visible spans with consecutive identical tool calls folded into one
 * row (`Bash · git status ×6`). A run whose first span id is in `unfolded`
 * keeps its row, then lists each call as its own row. An open call is never
 * folded: it is the one running now. */
export function foldRuns(spans: Span[], unfolded: Set<string>): SpanRow[] {
  const rows: SpanRow[] = [];
  for (let index = 0; index < spans.length; ) {
    const span = spans[index];
    let next = index + 1;
    while (next < spans.length && sameCall(span, spans[next])) next += 1;
    const run = spans.slice(index, next);
    const open = run.length > 1 && unfolded.has(span.id);
    rows.push({ span, run, unfolded: open });
    if (open) for (const call of run.slice(1)) rows.push({ span: call, run: [call], unfolded: false });
    index = next;
  }
  return rows;
}

/** A run's extent: its first start to its last end. */
export function runEnd(run: Span[], now: number): number {
  return Math.max(...run.map((span) => spanEnd(span, now)));
}

/** The last `count` tool rows of a stretch of a session's events, oldest
 * first, identical calls folded (×N): what the agent page lists as its
 * latest tool calls. */
export function recentToolRows(events: TimelineEvent[], now: number, count = 8): SpanRow[] {
  const tools = deriveTrace(events, now)
    .spans.filter((span) => span.kind === "tool")
    .sort((a, b) => a.start - b.start || logOrder(a) - logOrder(b));
  return foldRuns(tools, new Set()).slice(-count);
}

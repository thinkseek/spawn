/**
 * Sequential Step-Stream derivation for Agent Sessions.
 *
 * Groups raw `/api/sessions/<id>` timeline events into chronological Turns and
 * ordered, numbered evaluation Steps (file edits, file reads, shell commands,
 * thinking/assistant blocks when collected, checkpoints, background jobs, and
 * failures) with per-step durations, slow-step highlights, and consecutive-loop
 * detection so long-horizon execution bottlenecks are easy to inspect.
 *
 * Pure module (no React/DOM imports) so `node --test` tests it directly.
 */

import type { TimelineEvent } from "./api.ts";
import { commandSummary } from "./commands.ts";
import {
  deriveTrace,
  type KnownCommand,
  type KnownExecution,
  type Span,
} from "./trace.ts";

export type StreamStepKind =
  | "prompt"
  | "thinking"
  | "assistant"
  | "file_edit"
  | "file_read"
  | "shell"
  | "tool"
  | "checkpoint"
  | "job"
  | "error"
  | "system";

export type StreamFilter = "all" | "key" | "issues";

export interface StreamStep {
  id: string;
  index: number;
  kind: StreamStepKind;
  title: string;
  summary: string | null;
  body: string | null;
  startMs: number;
  endMs: number | null;
  durationMs: number | null;
  status: "ok" | "running" | "failed";
  outcome: string | null;
  slow: boolean;
  repeatedCount: number;
  events: TimelineEvent[];
}

export interface StreamTurn {
  id: string;
  turnNumber: number;
  executionId: string | null;
  startMs: number;
  endMs: number | null;
  durationMs: number | null;
  status: "ok" | "running" | "failed";
  prompt: string | null;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  steps: StreamStep[];
  /** True when the turn only captured coarse boundaries (no tool/thinking/message/checkpoint steps). */
  coarseOnly: boolean;
}

export interface StreamSummary {
  totalTurns: number;
  totalSteps: number;
  fileEdits: number;
  fileReads: number;
  shellRuns: number;
  toolCalls: number;
  failedSteps: number;
  slowSteps: number;
  repeatedSteps: number;
  coarseTurns: number;
  totalDurationMs: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
}

export interface SessionStreamModel {
  turns: StreamTurn[];
  summary: StreamSummary;
}

export const SLOW_STEP_MS = 30_000;

const FILE_EDIT_TOOLS = new Set([
  "edit",
  "write",
  "multiedit",
  "notebookedit",
  "edit_file",
  "write_file",
  "replace_file_content",
  "multi_replace_file_content",
  "write_to_file",
  "apply_patch",
]);

const FILE_READ_TOOLS = new Set([
  "read",
  "glob",
  "grep",
  "ls",
  "read_file",
  "view_file",
  "view_file_outline",
  "view_code_item",
  "grep_search",
  "find_by_name",
  "list_dir",
]);

const SHELL_TOOLS = new Set([
  "bash",
  "run_command",
  "local_shell_call",
  "exec_command",
  "shell",
  "shell_command",
  "command_status",
]);

/** Classify a normalized/native tool name into high-level engineering actions. */
export function classifyToolCategory(
  toolName: string,
): "file_edit" | "file_read" | "shell" | "tool" {
  const key = toolName.trim().toLowerCase();
  if (FILE_EDIT_TOOLS.has(key)) return "file_edit";
  if (FILE_READ_TOOLS.has(key)) return "file_read";
  if (SHELL_TOOLS.has(key)) return "shell";
  return "tool";
}

function detailObj(detail: unknown): Record<string, unknown> | null {
  return detail && typeof detail === "object" && !Array.isArray(detail)
    ? (detail as Record<string, unknown>)
    : null;
}

function strField(detail: unknown, ...keys: string[]): string | null {
  const obj = detailObj(detail);
  if (!obj) return null;
  for (const key of keys) {
    const val = obj[key];
    if (typeof val === "string" && val.trim() !== "") return val.trim();
  }
  return null;
}

function numField(detail: unknown, key: string): number {
  const obj = detailObj(detail);
  if (!obj) return 0;
  const val = obj[key];
  return typeof val === "number" && Number.isFinite(val) ? val : 0;
}

function firstLine(text: string | null, maxLen = 140): string | null {
  if (!text) return null;
  const trimmed = text.trim();
  if (!trimmed) return null;
  const line = trimmed.split(/\r?\n/, 1)[0] ?? trimmed;
  return line.length > maxLen ? `${line.slice(0, maxLen - 1)}…` : line;
}

function extractBody(event: TimelineEvent | null): string | null {
  if (!event) return null;
  return strField(
    event.detail,
    "output_preview",
    "thinking",
    "reasoning",
    "content",
    "text",
    "prompt",
    "message",
    "error",
    "reason",
  );
}

interface RawStep {
  id: string;
  kind: StreamStepKind;
  title: string;
  summary: string | null;
  body: string | null;
  startMs: number;
  endMs: number | null;
  durationMs: number | null;
  status: "ok" | "running" | "failed";
  outcome: string | null;
  events: TimelineEvent[];
  seq: number;
}

function markerToStep(
  event: TimelineEvent,
  commands: ReadonlyMap<string, KnownCommand>,
): RawStep | null {
  switch (event.event_name) {
    case "model.usage":
    case "screen.observed":
    case "message.typed":
    case "message.taken":
      // Token accounting goes to turn totals; typing/observation telemetry doesn't clutter steps.
      return null;
    case "agent.thinking":
    case "thinking":
    case "reasoning": {
      const body = extractBody(event);
      return {
        id: `ev:${event.event_id}`,
        kind: "thinking",
        title: "Thinking",
        summary: firstLine(body),
        body,
        startMs: event.recorded_at_ms,
        endMs: event.recorded_at_ms,
        durationMs: null,
        status: "ok",
        outcome: null,
        events: [event],
        seq: event.session_seq,
      };
    }
    case "agent.message":
    case "assistant.message": {
      const body = extractBody(event);
      return {
        id: `ev:${event.event_id}`,
        kind: "assistant",
        title: "Assistant",
        summary: firstLine(body),
        body,
        startMs: event.recorded_at_ms,
        endMs: event.recorded_at_ms,
        durationMs: null,
        status: "ok",
        outcome: null,
        events: [event],
        seq: event.session_seq,
      };
    }
    case "artifact.committed": {
      const reason = strField(event.detail, "reason");
      const branch = strField(event.detail, "branch");
      const commit = strField(event.detail, "commit");
      const shortSha = commit ? commit.slice(0, 7) : null;
      const summary = [branch, shortSha, reason].filter(Boolean).join(" · ") || null;
      return {
        id: `ev:${event.event_id}`,
        kind: "checkpoint",
        title: "Checkpoint",
        summary,
        body: null,
        startMs: event.recorded_at_ms,
        endMs: event.recorded_at_ms,
        durationMs: null,
        status: "ok",
        outcome: reason,
        events: [event],
        seq: event.session_seq,
      };
    }
    case "job.started":
    case "job.ended": {
      const jobKind = strField(event.detail, "kind") ?? "job";
      const jobId = strField(event.detail, "job_id") ?? "";
      const how = strField(event.detail, "how");
      const failed = how === "failed" || how === "killed";
      return {
        id: `ev:${event.event_id}`,
        kind: "job",
        title: event.event_name === "job.started" ? `Background ${jobKind}` : `Job ${how ?? "ended"}`,
        summary: jobId || null,
        body: strField(event.detail, "summary", "error"),
        startMs: event.recorded_at_ms,
        endMs: event.recorded_at_ms,
        durationMs: null,
        status: failed ? "failed" : "ok",
        outcome: how,
        events: [event],
        seq: event.session_seq,
      };
    }
    case "command.accepted": {
      const id = strField(event.detail, "command_id");
      const known = id ? commands.get(id) : undefined;
      const body = known?.text ?? strField(event.detail, "text", "prompt");
      return {
        id: `ev:${event.event_id}`,
        kind: "prompt",
        title: known ? `${known.principal} · ${known.action}` : `Command ${id ?? ""}`.trim(),
        summary: known ? commandSummary(known.action, known.text) : firstLine(body),
        body,
        startMs: event.recorded_at_ms,
        endMs: event.recorded_at_ms,
        durationMs: null,
        status: "ok",
        outcome: null,
        events: [event],
        seq: event.session_seq,
      };
    }
    default: {
      const failed = /fail|error|lost|crash|conflict/i.test(event.event_name)
        || (event.event_name === "execution.ended" && strField(event.detail, "cause") === "lost");
      if (failed) {
        const errBody = strField(event.detail, "error", "reason", "cause", "message");
        return {
          id: `ev:${event.event_id}`,
          kind: "error",
          title: event.event_name,
          summary: firstLine(errBody),
          body: errBody,
          startMs: event.recorded_at_ms,
          endMs: event.recorded_at_ms,
          durationMs: null,
          status: "failed",
          outcome: strField(event.detail, "cause", "error_kind") ?? "failed",
          events: [event],
          seq: event.session_seq,
        };
      }
      return null;
    }
  }
}

function toolSpanToRawStep(span: Span, now: number): RawStep {
  const events: TimelineEvent[] = [];
  if (span.startEvent) events.push(span.startEvent);
  for (const m of span.markers) events.push(m.event);
  if (span.endEvent) events.push(span.endEvent);
  events.sort((a, b) => a.session_seq - b.session_seq || a.event_id - b.event_id);

  const endMs = span.end;
  const computedDuration =
    numField(span.endEvent?.detail, "duration_ms") ||
    (endMs !== null ? Math.max(0, endMs - span.start) : Math.max(0, now - span.start));

  const body =
    extractBody(span.endEvent) ??
    extractBody(span.startEvent) ??
    (span.summary && span.summary.length > 0 ? span.summary : null);

  return {
    id: span.id,
    kind: classifyToolCategory(span.label),
    title: span.label,
    summary: span.summary,
    body: body !== span.summary ? body : null,
    startMs: span.start,
    endMs,
    durationMs: span.state === "orphan" && endMs === span.start ? null : computedDuration,
    status: span.failed ? "failed" : span.state === "open" ? "running" : "ok",
    outcome: span.outcome,
    events,
    seq: (span.startEvent ?? span.endEvent)?.session_seq ?? 0,
  };
}

function isFineStepKind(kind: StreamStepKind): boolean {
  return (
    kind === "thinking" ||
    kind === "assistant" ||
    kind === "file_edit" ||
    kind === "file_read" ||
    kind === "shell" ||
    kind === "tool" ||
    kind === "checkpoint" ||
    kind === "job"
  );
}

/**
 * Transform session timeline events, executions, and commands into sequential Turns and Steps.
 */
export function buildSessionStream(
  events: TimelineEvent[],
  now: number,
  executions: readonly KnownExecution[] = [],
  commands: ReadonlyMap<string, KnownCommand> = new Map(),
  to: string | null = null,
): SessionStreamModel {
  const trace = deriveTrace(events, now, executions, commands, to);
  const byId = trace.byId;

  const turnSpans = trace.spans.filter((span) => span.kind === "turn");
  const orphanTools = trace.spans.filter(
    (span) => span.kind === "tool" && (!span.parentId || byId.get(span.parentId)?.kind !== "turn"),
  );

  // Collect raw turns (one per turn span, plus one synthetic fallback turn if orphan tool calls exist).
  interface RawTurn {
    id: string;
    executionId: string | null;
    startMs: number;
    endMs: number | null;
    status: "ok" | "running" | "failed";
    prompt: string | null;
    inputTokens: number;
    outputTokens: number;
    cacheReadTokens: number;
    rawSteps: RawStep[];
    seq: number;
  }

  const rawTurns: RawTurn[] = [];

  for (const turn of turnSpans) {
    const prompt = strField(turn.startEvent?.detail, "text", "prompt", "content");
    let inputTokens = numField(turn.endEvent?.detail, "input_tokens");
    let outputTokens = numField(turn.endEvent?.detail, "output_tokens");
    let cacheReadTokens = numField(turn.endEvent?.detail, "cache_read_tokens");

    const rawSteps: RawStep[] = [];

    for (const childId of turn.childIds) {
      const child = byId.get(childId);
      if (child && child.kind === "tool") {
        rawSteps.push(toolSpanToRawStep(child, now));
      }
    }

    for (const marker of turn.markers) {
      if (marker.event.event_name === "model.usage") {
        inputTokens += numField(marker.event.detail, "input_tokens");
        outputTokens += numField(marker.event.detail, "output_tokens");
        cacheReadTokens += numField(marker.event.detail, "cache_read_tokens");
        continue;
      }
      const step = markerToStep(marker.event, commands);
      if (step) rawSteps.push(step);
    }

    if (turn.failed && turn.endEvent) {
      const errMsg = strField(turn.endEvent.detail, "error", "reason");
      rawSteps.push({
        id: `turn-err:${turn.endEvent.event_id}`,
        kind: "error",
        title: turn.endEvent.event_name,
        summary: firstLine(errMsg) ?? turn.outcome,
        body: errMsg,
        startMs: turn.endEvent.recorded_at_ms,
        endMs: turn.endEvent.recorded_at_ms,
        durationMs: null,
        status: "failed",
        outcome: turn.outcome,
        events: [turn.endEvent],
        seq: turn.endEvent.session_seq,
      });
    }

    rawSteps.sort((a, b) => a.startMs - b.startMs || a.seq - b.seq);

    rawTurns.push({
      id: turn.id,
      executionId: turn.executionId,
      startMs: turn.start,
      endMs: turn.end,
      status: turn.failed ? "failed" : turn.state === "open" ? "running" : "ok",
      prompt,
      inputTokens,
      outputTokens,
      cacheReadTokens,
      rawSteps,
      seq: (turn.startEvent ?? turn.endEvent)?.session_seq ?? 0,
    });
  }

  // Group non-turn execution usage/commands/tool spans if any exist outside turns.
  const outsideSteps: RawStep[] = orphanTools.map((tool) => toolSpanToRawStep(tool, now));
  let extraInput = 0;
  let extraOutput = 0;
  let extraCache = 0;

  for (const span of trace.spans) {
    if (span.kind !== "execution" && span.kind !== "command") continue;
    if (span.kind === "command") {
      const cmdStep: RawStep = {
        id: span.id,
        kind: "prompt",
        title: span.label,
        summary: span.summary,
        body: strField(span.startEvent?.detail, "text", "prompt") ?? span.summary,
        startMs: span.start,
        endMs: span.end,
        durationMs: span.end !== null ? Math.max(0, span.end - span.start) : null,
        status: span.failed ? "failed" : span.state === "open" ? "running" : "ok",
        outcome: span.outcome,
        events: [span.startEvent, span.endEvent].filter((e): e is TimelineEvent => e !== null),
        seq: (span.startEvent ?? span.endEvent)?.session_seq ?? 0,
      };
      // Attach command prompt to the matching turn if this command was its parent, else keep as outside step.
      const childTurn = rawTurns.find((t) => {
        const turnSpan = byId.get(t.id);
        return turnSpan?.parentId && byId.get(turnSpan.parentId)?.parentId === span.id;
      });
      if (childTurn && !childTurn.prompt && cmdStep.body) {
        childTurn.prompt = cmdStep.body;
      } else if (!childTurn && cmdStep.summary) {
        outsideSteps.push(cmdStep);
      }
    }

    for (const marker of span.markers) {
      if (marker.event.event_name === "model.usage") {
        extraInput += numField(marker.event.detail, "input_tokens");
        extraOutput += numField(marker.event.detail, "output_tokens");
        extraCache += numField(marker.event.detail, "cache_read_tokens");
        continue;
      }
      const step = markerToStep(marker.event, commands);
      if (step) outsideSteps.push(step);
    }
  }

  for (const marker of trace.markers) {
    if (marker.event.event_name === "model.usage") {
      extraInput += numField(marker.event.detail, "input_tokens");
      extraOutput += numField(marker.event.detail, "output_tokens");
      extraCache += numField(marker.event.detail, "cache_read_tokens");
      continue;
    }
    const step = markerToStep(marker.event, commands);
    if (step) outsideSteps.push(step);
  }

  if (outsideSteps.length > 0 && rawTurns.length === 0) {
    outsideSteps.sort((a, b) => a.startMs - b.startMs || a.seq - b.seq);
    const first = outsideSteps[0];
    const last = outsideSteps[outsideSteps.length - 1];
    const hasFailed = outsideSteps.some((s) => s.status === "failed");
    const hasRunning = outsideSteps.some((s) => s.status === "running");
    rawTurns.push({
      id: "turn:session",
      executionId: first.events[0]?.execution_id ?? null,
      startMs: first.startMs,
      endMs: hasRunning ? null : (last.endMs ?? last.startMs),
      status: hasFailed ? "failed" : hasRunning ? "running" : "ok",
      prompt: null,
      inputTokens: extraInput,
      outputTokens: extraOutput,
      cacheReadTokens: extraCache,
      rawSteps: outsideSteps,
      seq: first.seq,
    });
    extraInput = 0;
    extraOutput = 0;
    extraCache = 0;
  } else if (rawTurns.length > 0 && outsideSteps.length > 0) {
    // Merge remaining non-turn checkpoints/errors into chronologically nearest preceding turn.
    rawTurns.sort((a, b) => a.startMs - b.startMs || a.seq - b.seq);
    for (const step of outsideSteps) {
      let target = rawTurns[0];
      for (const candidate of rawTurns) {
        if (candidate.startMs <= step.startMs) target = candidate;
      }
      target.rawSteps.push(step);
      target.rawSteps.sort((a, b) => a.startMs - b.startMs || a.seq - b.seq);
    }
  }

  rawTurns.sort((a, b) => a.startMs - b.startMs || a.seq - b.seq);

  // Number steps sequentially across the session and compute repeat/slow diagnostics.
  let stepIndex = 0;
  let totalSteps = 0;
  let fileEdits = 0;
  let fileReads = 0;
  let shellRuns = 0;
  let toolCalls = 0;
  let failedSteps = 0;
  let slowSteps = 0;
  let repeatedSteps = 0;
  let coarseTurns = 0;
  let totalDurationMs = 0;
  let inputTokens = extraInput;
  let outputTokens = extraOutput;
  let cacheReadTokens = extraCache;

  const turns: StreamTurn[] = rawTurns.map((raw, turnIdx) => {
    const sigKey = (step: RawStep) =>
      step.summary ? `${step.kind}\u0000${step.title}\u0000${step.summary}` : null;

    // First compute consecutive run lengths inside the turn for loop visibility.
    const counts = new Array<number>(raw.rawSteps.length).fill(1);
    let runStart = 0;
    while (runStart < raw.rawSteps.length) {
      const k = sigKey(raw.rawSteps[runStart]);
      let runEnd = runStart + 1;
      if (k !== null) {
        while (runEnd < raw.rawSteps.length && sigKey(raw.rawSteps[runEnd]) === k) {
          runEnd += 1;
        }
      }
      const len = runEnd - runStart;
      if (len > 1) {
        for (let i = runStart; i < runEnd; i += 1) counts[i] = len;
      }
      runStart = runEnd;
    }

    const steps: StreamStep[] = raw.rawSteps.map((step, i) => {
      stepIndex += 1;
      totalSteps += 1;
      const slow = step.durationMs !== null && step.durationMs >= SLOW_STEP_MS;
      const repeatedCount = counts[i];
      if (step.kind === "file_edit") fileEdits += 1;
      else if (step.kind === "file_read") fileReads += 1;
      else if (step.kind === "shell") shellRuns += 1;
      else if (step.kind === "tool") toolCalls += 1;
      if (step.status === "failed") failedSteps += 1;
      if (slow) slowSteps += 1;
      if (repeatedCount >= 2) repeatedSteps += 1;

      return {
        id: step.id,
        index: stepIndex,
        kind: step.kind,
        title: step.title,
        summary: step.summary,
        body: step.body,
        startMs: step.startMs,
        endMs: step.endMs,
        durationMs: step.durationMs,
        status: step.status,
        outcome: step.outcome,
        slow,
        repeatedCount,
        events: step.events,
      };
    });

    const coarseOnly = !steps.some((step) => isFineStepKind(step.kind));
    if (coarseOnly) coarseTurns += 1;

    const turnDuration =
      raw.endMs !== null
        ? Math.max(0, raw.endMs - raw.startMs)
        : Math.max(0, now - raw.startMs);
    totalDurationMs += turnDuration;
    inputTokens += raw.inputTokens;
    outputTokens += raw.outputTokens;
    cacheReadTokens += raw.cacheReadTokens;

    return {
      id: raw.id,
      turnNumber: turnIdx + 1,
      executionId: raw.executionId,
      startMs: raw.startMs,
      endMs: raw.endMs,
      durationMs: turnDuration,
      status: raw.status,
      prompt: raw.prompt,
      inputTokens: raw.inputTokens,
      outputTokens: raw.outputTokens,
      cacheReadTokens: raw.cacheReadTokens,
      steps,
      coarseOnly,
    };
  });

  return {
    turns,
    summary: {
      totalTurns: turns.length,
      totalSteps,
      fileEdits,
      fileReads,
      shellRuns,
      toolCalls,
      failedSteps,
      slowSteps,
      repeatedSteps,
      coarseTurns,
      totalDurationMs,
      inputTokens,
      outputTokens,
      cacheReadTokens,
    },
  };
}

const KEY_STEP_KINDS = new Set<StreamStepKind>([
  "prompt",
  "thinking",
  "assistant",
  "file_edit",
  "shell",
  "checkpoint",
  "error",
]);

/** Filter a turn's sequential steps by view mode (`all` | `key` | `issues`). */
export function filterStreamSteps(
  steps: readonly StreamStep[],
  filter: StreamFilter,
): StreamStep[] {
  if (filter === "all") return [...steps];
  if (filter === "key") return steps.filter((step) => KEY_STEP_KINDS.has(step.kind));
  return steps.filter(
    (step) => step.status === "failed" || step.slow || step.repeatedCount >= 2,
  );
}

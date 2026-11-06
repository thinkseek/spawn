/* The missions page as a timeline: each mission a lane, each task a bar on
 * one time axis, divided into the phases its control records put it in.
 * Pure (type-only imports), tested in web/test/missionTimeline.test.ts.
 *
 * A task's records (`spawn task stages`, oldest first) are the edges
 * between phases:
 *
 *   assigned  → queued     waiting for delivery to its worker
 *   delivered → working    until the worker reports
 *   reported  → review     the supervisor reviews (accept = review, or no gate)
 *             → gate       the gate judges (accept = gate)
 *   reviewed  accepted → gate (with a gate) or done; rejected → sent back
 *   gated     accepted → landing; rejected / conflict / incompatible → sent back;
 *             stale / aborted / no_room → still at the gate (judged again)
 *   landed, aborted, cancelled: the bar ends.
 *
 * Sent back, the task waits (`revise`) until it is delivered again, which
 * starts its next attempt; the return is drawn from the verdict back to the
 * start of the attempt it sent back. A task with no end runs to now; in a
 * halted mission it is stalled rather than live. */

import type { Mission, MissionTask } from "./missions";

export type Phase = "queued" | "working" | "review" | "gate" | "landing" | "revise";

/** Display order of the phases (the legend's). */
export const PHASES: readonly Phase[] = [
  "queued",
  "working",
  "review",
  "gate",
  "landing",
  "revise",
];

export const PHASE_LABEL: Record<Phase, string> = {
  queued: "assigned",
  working: "working",
  review: "review",
  gate: "gated",
  landing: "landing",
  revise: "sent back",
};

/** How a bar ended: `accepted` is the end of an ungated mission's task. */
export type Outcome = "landed" | "accepted" | "cancelled" | "aborted" | "open";

export interface Segment {
  phase: Phase;
  start: number;
  /** null while the task is still in it. */
  end: number | null;
  attempt: number;
}

export interface VerdictMark {
  at: number;
  /** `gated` (the gate) or `reviewed` (the supervisor). */
  by: "gated" | "reviewed";
  verdict: string;
  attempt: number;
  /** The verdict sent the work back. */
  sentBack: boolean;
  reason: string | null;
}

/** A send-back: from the verdict back to the start of the attempt. */
export interface Return {
  from: number;
  to: number;
  attempt: number;
}

export interface TaskBar {
  key: string;
  task: MissionTask;
  label: string;
  start: number;
  /** null while open. */
  end: number | null;
  segments: Segment[];
  verdicts: VerdictMark[];
  returns: Return[];
  outcome: Outcome;
  attempts: number;
  /** Open, but its mission is halted: nothing moves until a human resumes. */
  stalled: boolean;
}

export type LaneState = "active" | "halted" | "done" | "closed";

export interface LaneCounts {
  total: number;
  landed: number;
  open: number;
  sentBack: number;
  cancelled: number;
}

export interface Lane {
  key: string;
  mission: Mission;
  name: string;
  state: LaneState;
  /** The halt rule's reason, while halted. */
  halted: string | null;
  counts: LaneCounts;
  /** Oldest first. */
  bars: TaskBar[];
  /** The latest moment anything happened in it (now, if a task is open). */
  lastActivity: number;
}

export interface MissionTimeline {
  /** Newest activity first. */
  lanes: Lane[];
  start: number;
  end: number;
  openCount: number;
}

/** Gate verdict reasons per task, oldest first (the RSI page has them). */
export type VerdictReasons = ReadonlyMap<number, ReadonlyArray<string | null>>;

const TERMINAL: Record<string, Outcome> = {
  landed: "landed",
  aborted: "aborted",
  cancelled: "cancelled",
};

/** A mission's short name: its text up to a colon, else its first words. */
export function missionName(mission: Pick<Mission, "id" | "text">): string {
  const text = (mission.text ?? "").trim().replace(/\s+/g, " ");
  if (!text) return mission.id !== null ? `Mission ${mission.id}` : "Mission";
  const colon = text.indexOf(":");
  if (colon > 0 && colon <= 40) return text.slice(0, colon);
  const sentence = text.split(/(?<=[.!?])\s/)[0];
  if (sentence.length <= 48) return sentence.replace(/[.!?]$/, "");
  const cut = sentence.slice(0, 47);
  const space = cut.lastIndexOf(" ");
  return `${(space > 24 ? cut.slice(0, space) : cut).trimEnd()}…`;
}

export function laneState(mission: Pick<Mission, "state" | "halted">): LaneState {
  if (mission.halted) return "halted";
  switch (mission.state) {
    case "done":
      return "done";
    case "active":
    case "open":
    case null:
      return "active";
    default:
      return "closed";
  }
}

/** One task as a bar, or null when it has no time to place it at. */
export function taskBar(
  task: MissionTask,
  mission: Pick<Mission, "gate" | "accept" | "halted">,
  reasons?: ReadonlyArray<string | null>,
): TaskBar | null {
  const gated = mission.gate !== null;
  const records = task.stages.filter(
    (stage): stage is typeof stage & { at_ms: number } => stage.at_ms !== null,
  );
  const label = `T${task.id ?? "?"}`;
  const key = `task:${task.id ?? label}`;

  if (records.length === 0) return legacyBar(task, mission, key, label);

  const segments: Segment[] = [];
  const verdicts: VerdictMark[] = [];
  const returns: Return[] = [];
  let attempt = 1;
  let attemptStart: number | null = null;
  let sentBack = false;
  let phase: Phase | null = null;
  let since = records[0].at_ms;
  let outcome: Outcome = "open";
  let end: number | null = null;
  let gateCount = 0;

  for (const record of records) {
    const at = Math.max(record.at_ms, since);
    if (phase) segments.push({ phase, start: since, end: at, attempt });
    let next: Phase | null = phase;
    switch (record.stage) {
      case "assigned":
        next = "queued";
        break;
      case "delivered":
        if (sentBack) {
          attempt += 1;
          sentBack = false;
        }
        attemptStart = at;
        next = "working";
        break;
      case "reported":
        next = gated && mission.accept === "gate" ? "gate" : "review";
        break;
      case "reviewed": {
        const accepted = record.verdict === "accepted";
        verdicts.push({
          at,
          by: "reviewed",
          verdict: accepted ? "accepted" : "rejected",
          attempt,
          sentBack: !accepted,
          reason: null,
        });
        if (accepted) {
          if (!gated) {
            outcome = "accepted";
            end = at;
          }
          next = "gate";
        } else {
          next = "revise";
          sentBack = true;
          returns.push({ from: at, to: attemptStart ?? records[0].at_ms, attempt });
        }
        break;
      }
      case "gated": {
        const verdict = record.verdict ?? "unknown";
        const back =
          verdict === "rejected" || verdict === "conflict" || verdict === "incompatible";
        const reason =
          reasons?.[gateCount] ??
          (task.last_gate && task.last_gate.at_ms === record.at_ms
            ? task.last_gate.reason
            : null);
        gateCount += 1;
        verdicts.push({ at, by: "gated", verdict, attempt, sentBack: back, reason });
        if (verdict === "accepted") next = "landing";
        else if (back) {
          next = "revise";
          sentBack = true;
          returns.push({ from: at, to: attemptStart ?? records[0].at_ms, attempt });
        } else next = "gate";
        break;
      }
      default:
        if (record.stage in TERMINAL) {
          outcome = TERMINAL[record.stage];
          end = at;
        }
    }
    since = at;
    phase = next;
    if (end !== null) break;
  }

  if (end === null) {
    // A landed task without a record of its own ends at its last update.
    // Every cancelled task has its record (the writer of a retirement or a
    // close appends one): a missing record shows as open, not hidden.
    const closedAt = Math.max(task.updated_at_ms ?? since, since);
    if (task.state === "landed") {
      outcome = "landed";
      end = closedAt;
    }
    if (phase) segments.push({ phase, start: since, end, attempt });
  }

  return {
    key,
    task,
    label,
    start: records[0].at_ms,
    end,
    segments,
    verdicts,
    returns,
    outcome,
    attempts: attempt,
    stalled: end === null && Boolean(mission.halted),
  };
}

/** A task from before the control stream: one segment from its creation to
 * its last update, in the phase its state names. */
function legacyBar(
  task: MissionTask,
  mission: Pick<Mission, "gate" | "halted">,
  key: string,
  label: string,
): TaskBar | null {
  const start = task.created_at_ms;
  if (start === null) return null;
  const done: Partial<Record<string, Outcome>> = {
    landed: "landed",
    cancelled: "cancelled",
    ...(mission.gate === null ? { accepted: "accepted" as const } : {}),
  };
  const outcome = done[task.state] ?? "open";
  const end = outcome === "open" ? null : Math.max(task.updated_at_ms ?? start, start);
  const phase: Phase =
    task.state === "assigned"
      ? "queued"
      : task.state === "delivered"
        ? "working"
        : task.state === "revise"
          ? "revise"
          : task.state === "landing"
            ? "landing"
            : task.state === "reported" || task.state === "accepted"
              ? "gate"
              : "working";
  return {
    key,
    task,
    label,
    start,
    end,
    segments: [{ phase, start, end, attempt: 1 }],
    verdicts: [],
    returns: [],
    outcome,
    attempts: 1 + task.revisions,
    stalled: end === null && Boolean(mission.halted),
  };
}

export function deriveMissionTimeline(
  missions: readonly Mission[],
  now: number,
  reasons?: VerdictReasons,
): MissionTimeline {
  const lanes: Lane[] = [];
  let start = Infinity;
  let end = -Infinity;
  let openCount = 0;
  for (const mission of missions) {
    const bars: TaskBar[] = [];
    for (const task of mission.tasks) {
      const bar = taskBar(task, mission, task.id !== null ? reasons?.get(task.id) : undefined);
      if (bar) bars.push(bar);
    }
    bars.sort((a, b) => a.start - b.start || (a.task.id ?? 0) - (b.task.id ?? 0));
    const counts: LaneCounts = { total: bars.length, landed: 0, open: 0, sentBack: 0, cancelled: 0 };
    let lastActivity = mission.updated_at_ms ?? mission.created_at_ms ?? 0;
    for (const bar of bars) {
      if (bar.outcome === "landed" || bar.outcome === "accepted") counts.landed += 1;
      else if (bar.outcome === "open") counts.open += 1;
      else counts.cancelled += 1;
      if (bar.returns.length > 0) counts.sentBack += 1;
      const barEnd = bar.end ?? now;
      lastActivity = Math.max(lastActivity, barEnd);
      start = Math.min(start, bar.start);
      end = Math.max(end, barEnd);
      if (bar.end === null) openCount += 1;
    }
    lanes.push({
      key: `mission:${mission.id ?? lanes.length}`,
      mission,
      name: missionName(mission),
      state: laneState(mission),
      halted: mission.halted,
      counts,
      bars,
      lastActivity,
    });
  }
  lanes.sort(
    (a, b) => b.lastActivity - a.lastActivity || (b.mission.id ?? 0) - (a.mission.id ?? 0),
  );
  if (!Number.isFinite(start)) {
    start = now;
    end = now;
  }
  return { lanes, start, end, openCount };
}

export type StateFilter = "all" | "open" | "landed" | "sent-back" | "cancelled";

export const STATE_FILTERS: ReadonlyArray<{ id: StateFilter; label: string }> = [
  { id: "all", label: "All" },
  { id: "open", label: "Open" },
  { id: "landed", label: "Landed" },
  { id: "sent-back", label: "Sent back" },
  { id: "cancelled", label: "Cancelled" },
];

export function matchesState(bar: TaskBar, filter: StateFilter): boolean {
  switch (filter) {
    case "all":
      return true;
    case "open":
      return bar.outcome === "open";
    case "landed":
      return bar.outcome === "landed" || bar.outcome === "accepted";
    case "sent-back":
      return bar.returns.length > 0;
    case "cancelled":
      return bar.outcome === "cancelled" || bar.outcome === "aborted";
  }
}

/** Lanes narrowed to one mission and to tasks in one state. A lane left
 * with no tasks by the state filter is dropped; the mission filter keeps
 * its lane even when it has none. */
export function filterLanes(
  lanes: readonly Lane[],
  mission: number | null,
  state: StateFilter,
): Lane[] {
  const out: Lane[] = [];
  for (const lane of lanes) {
    if (mission !== null && lane.mission.id !== mission) continue;
    const bars = lane.bars.filter((bar) => matchesState(bar, state));
    if (state !== "all" && bars.length === 0) continue;
    out.push(bars.length === lane.bars.length ? lane : { ...lane, bars });
  }
  return out;
}

/** The phase a bar is in at `at`, for the tooltip; null outside it. */
export function phaseAt(bar: TaskBar, at: number, now: number): Segment | null {
  for (const segment of bar.segments) {
    const segmentEnd = segment.end ?? now;
    if (at >= segment.start && at <= segmentEnd) return segment;
  }
  return null;
}

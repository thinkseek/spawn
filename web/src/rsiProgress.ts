// The Landings card's model for a mission whose objectives carry no metric:
// its objectives settled over time. Loads with node alone (type-only
// imports), so web/test and the black-box scenarios read it as the page does.

import type { RsiMission, RsiObjective } from "./rsi.ts";

/** One settled objective, at the time it settled. */
export interface ProgressPoint {
  objective: number;
  /** The task that settled it: its first landing's, or the
   * `objective.solved` record's (null when the record names none). */
  task: number | null;
  at_ms: number;
  /** Objectives settled up to and including this one, from 1. */
  settled: number;
  /** Settled by the integrator's `objective.solved` record (its scenario
   * passed on the base), not by a landing. */
  on_base: boolean;
}

export interface SettledProgress {
  /** The mission's objectives. */
  total: number;
  /** Those solved or met. */
  settled: number;
  /** One per settled objective with a time, oldest first. */
  points: ProgressPoint[];
}

type ProgressMission = Pick<RsiMission, "objectives">;
type ProgressObjective = Pick<RsiObjective, "id" | "status" | "metric" | "landings" | "solved_on_base">;

function finite(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isSettledStatus(status: unknown): boolean {
  return status === "solved" || status === "met";
}

/** Where a settled objective sits: its first landing, else its record
 * solved on the base; null when neither carries a time. */
function settledAt(
  objective: ProgressObjective,
): Omit<ProgressPoint, "settled"> | null {
  let first: { task: number; at_ms: number } | null = null;
  for (const landing of Array.isArray(objective.landings) ? objective.landings : []) {
    if (!landing || !finite(landing.at_ms)) continue;
    if (
      first === null ||
      landing.at_ms < first.at_ms ||
      (landing.at_ms === first.at_ms && landing.task < first.task)
    ) {
      first = { task: landing.task, at_ms: landing.at_ms };
    }
  }
  if (first !== null) {
    return { objective: objective.id, task: first.task, at_ms: first.at_ms, on_base: false };
  }
  const record = objective.solved_on_base;
  if (record && finite(record.at_ms)) {
    return {
      objective: objective.id,
      task: finite(record.task) ? record.task : null,
      at_ms: record.at_ms,
      on_base: true,
    };
  }
  return null;
}

/** A mission's objectives settled over time. */
export function settledProgress(mission: ProgressMission): SettledProgress {
  const objectives: ProgressObjective[] = Array.isArray(mission.objectives)
    ? mission.objectives
    : [];
  const settled = objectives.filter((objective) => isSettledStatus(objective.status));
  const placed = settled
    .map(settledAt)
    .filter((point): point is Omit<ProgressPoint, "settled"> => point !== null)
    .sort((left, right) => left.at_ms - right.at_ms || left.objective - right.objective);
  return {
    total: objectives.length,
    settled: settled.length,
    points: placed.map((point, index) => ({ ...point, settled: index + 1 })),
  };
}

/** "<settled> of <total> objectives settled". */
export function progressTitle(progress: Pick<SettledProgress, "settled" | "total">): string {
  return `${progress.settled} of ${progress.total} objectives settled`;
}

/** Whether the Landings card charts the mission's settled objectives
 * rather than a metric's ratchet: no objective of it names a metric. */
export function chartsProgress(mission: ProgressMission): boolean {
  const objectives = Array.isArray(mission.objectives) ? mission.objectives : [];
  return !objectives.some((objective) => Boolean(objective.metric));
}

/** A short age, as the page writes one ("3h ago"). */
function age(at_ms: number, now: number): string {
  const seconds = Math.floor(Math.max(0, now - at_ms) / 1000);
  if (seconds < 1) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

/** The card's last line: the latest point's objective and task and how
 * long ago it settled, or null with nothing settled. */
export function latestProgressLine(progress: SettledProgress, now: number): string | null {
  const latest = progress.points[progress.points.length - 1];
  if (!latest) return null;
  const how = latest.on_base ? "solved on the base" : "landed";
  const task = latest.task === null ? "" : `, task #${latest.task}`;
  return `Latest: objective ${latest.objective} ${how}${task}, ${age(latest.at_ms, now)}`;
}

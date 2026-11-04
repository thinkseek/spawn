import type { Direction, MetricPoint } from "../../rsi";

export interface Ratchet {
  /** Landings that carry a value, oldest first. */
  points: MetricPoint[];
  /** Baseline before the first landing. */
  start: number | null;
  /** Best landed value after each point; index 0 is `start`. */
  steps: number[];
  /** Current baseline value (last step). */
  best: number | null;
  landed: number;
}

/** The ratchet of one metric: where it started and every landing since. */
export function ratchet(all: MetricPoint[]): Ratchet {
  const points = all.filter(
    (point) => point.candidate !== null || point.baseline !== null,
  );
  if (points.length === 0) {
    return { points, start: null, steps: [], best: null, landed: 0 };
  }
  const start =
    points[0].baseline ??
    points.find((point) => point.accepted)?.candidate ??
    points[0].candidate ??
    0;
  const steps: number[] = [start];
  for (const point of points) {
    const previous = steps[steps.length - 1];
    steps.push(
      point.accepted && point.candidate !== null ? point.candidate : previous,
    );
  }
  return {
    points,
    start,
    steps,
    best: steps[steps.length - 1],
    landed: points.filter((point) => point.accepted).length,
  };
}

/** Signed change from `start` to `value`, in percent. */
export function percentChange(
  start: number | null,
  value: number | null,
): number | null {
  if (start === null || value === null || start === 0) return null;
  return ((value - start) / Math.abs(start)) * 100;
}

export function formatPercent(change: number | null): string {
  if (change === null || !Number.isFinite(change)) return "—";
  if (Math.abs(change) < 0.05) return "0%";
  return `${change > 0 ? "+" : "−"}${Math.abs(change).toFixed(1)}%`;
}

export function isImprovement(
  change: number | null,
  direction: Direction,
): boolean {
  if (change === null || Math.abs(change) < 0.05) return false;
  return direction === "lower" ? change < 0 : change > 0;
}

/** Share of the distance from `start` to `target` already covered, 0..1. */
export function progressToTarget(
  start: number | null,
  best: number | null,
  target: number | null,
  direction: Direction,
): number | null {
  if (start === null || best === null || target === null) return null;
  const total = direction === "lower" ? start - target : target - start;
  const done = direction === "lower" ? start - best : best - start;
  if (total <= 0) return 1;
  return Math.min(1, Math.max(0, done / total));
}

import { useEffect, useState } from "react";

const PHASE: Record<string, { dot: string; text: string }> = {
  Starting: { dot: "bg-info animate-spawn-pulse", text: "text-info" },
  Working: { dot: "bg-success animate-spawn-pulse", text: "text-success" },
  Gating: { dot: "bg-warning animate-spawn-pulse", text: "text-warning" },
  Landed: { dot: "bg-success", text: "text-success" },
  Rejected: { dot: "bg-destructive", text: "text-destructive" },
  Aborted: { dot: "bg-muted-foreground", text: "text-muted-foreground" },
};

const FALLBACK = { dot: "bg-faint-foreground", text: "text-faint-foreground" };

export function PhaseBadge({ phase }: { phase: string }) {
  const style = PHASE[phase] ?? FALLBACK;
  return (
    <span
      className={`inline-flex shrink-0 items-center gap-1.5 rounded-sm border border-surface-border bg-surface px-1.5 py-0.5 text-micro font-medium ${style.text}`}
    >
      <span
        aria-hidden="true"
        className={`inline-block size-1.5 shrink-0 rounded-full ${style.dot}`}
      />
      {phase}
    </span>
  );
}

/** Wall clock that re-renders its caller every `intervalMs`. */
export function useNow(intervalMs = 1_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [intervalMs]);
  return now;
}

/** Format a metric value with a unit, keeping three significant digits. */
export function formatMetric(value: number | null, unit: string): string {
  if (value === null || !Number.isFinite(value)) return "—";
  const magnitude = Math.abs(value);
  const digits = magnitude >= 100 ? 0 : magnitude >= 10 ? 1 : 2;
  const shown = value.toLocaleString(undefined, {
    maximumFractionDigits: digits,
  });
  return unit ? `${shown} ${unit}` : shown;
}

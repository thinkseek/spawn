import { useEffect, useState } from "react";

import { cx } from "../ui";

export interface PhaseStyle {
  /** Badge colors: tinted fill, text, inset ring. */
  badge: string;
  dot: string;
  /** Solid fill for bars and steppers. */
  bar: string;
  text: string;
  pulse: boolean;
}

const INFO: Omit<PhaseStyle, "pulse"> = {
  badge: "bg-info/12 text-info ring-info/25",
  dot: "bg-info",
  bar: "bg-info",
  text: "text-info",
};

const WARNING: Omit<PhaseStyle, "pulse"> = {
  badge: "bg-warning/12 text-warning ring-warning/25",
  dot: "bg-warning",
  bar: "bg-warning",
  text: "text-warning",
};

const SUCCESS: PhaseStyle = {
  badge: "bg-success/12 text-success ring-success/25",
  dot: "bg-success",
  bar: "bg-success",
  text: "text-success",
  pulse: false,
};

const DESTRUCTIVE: PhaseStyle = {
  badge: "bg-destructive/12 text-destructive ring-destructive/25",
  dot: "bg-destructive",
  bar: "bg-destructive",
  text: "text-destructive",
  pulse: false,
};

const MUTED: PhaseStyle = {
  badge: "bg-muted text-muted-foreground ring-surface-border",
  dot: "bg-muted-foreground",
  bar: "bg-muted-foreground",
  text: "text-muted-foreground",
  pulse: false,
};

/** Task states (mission_tasks.state) and gate verdicts (task.gated). */
const PHASE: Record<string, PhaseStyle> = {
  // Task states, in the order a gated task walks them.
  assigned: { ...INFO, pulse: true },
  delivered: { ...INFO, pulse: true },
  reported: { ...WARNING, pulse: true },
  accepted: { ...WARNING, pulse: true },
  landing: { ...WARNING, pulse: true },
  revise: { ...WARNING, pulse: false },
  landed: SUCCESS,
  cancelled: MUTED,
  // Gate verdicts. `accepted` above doubles as the verdict; a verdict is
  // settled, so the list renders it without the pulse.
  rejected: DESTRUCTIVE,
  aborted: MUTED,
  conflict: { ...WARNING, pulse: false },
  stale: MUTED,
  no_room: MUTED,
};

const FALLBACK: PhaseStyle = {
  badge: "bg-muted text-faint-foreground ring-surface-border",
  dot: "bg-faint-foreground",
  bar: "bg-faint-foreground",
  text: "text-faint-foreground",
  pulse: false,
};

export function phaseStyle(phase: string): PhaseStyle {
  return PHASE[phase] ?? FALLBACK;
}

/** Style of a gate verdict: settled, never pulsing. */
export function verdictStyle(verdict: string): PhaseStyle {
  if (verdict === "accepted") return SUCCESS;
  return { ...phaseStyle(verdict), pulse: false };
}

export function VerdictBadge({ verdict }: { verdict: string }) {
  const style = verdictStyle(verdict);
  return (
    <span
      className={cx(
        "inline-flex h-5 shrink-0 items-center gap-1.5 rounded-md px-1.5 text-caption font-medium ring-1 ring-inset",
        style.badge,
      )}
    >
      <span
        aria-hidden="true"
        className={cx("inline-block size-1.5 shrink-0 rounded-full", style.dot)}
      />
      {verdict}
    </span>
  );
}

export function PhaseBadge({
  phase,
  size = "sm",
}: {
  phase: string;
  size?: "sm" | "md";
}) {
  const style = phaseStyle(phase);
  return (
    <span
      className={cx(
        "inline-flex shrink-0 items-center rounded-md font-medium ring-1 ring-inset",
        size === "md"
          ? "h-7 gap-2 px-2.5 text-label"
          : "h-5 gap-1.5 px-1.5 text-caption",
        style.badge,
      )}
    >
      <span
        aria-hidden="true"
        className={cx(
          "inline-block shrink-0 rounded-full",
          size === "md" ? "size-2" : "size-1.5",
          style.dot,
          style.pulse && "animate-spawn-pulse",
        )}
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
  const shown = value.toLocaleString("en-US", {
    maximumFractionDigits: digits,
  });
  return unit ? `${shown} ${unit}` : shown;
}

/** Number part of a metric, without the unit. */
export function formatNumber(value: number | null): string {
  return formatMetric(value, "");
}

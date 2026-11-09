import type { SessionState } from "../api";
import { cx } from "./ui";

/**
 * One state palette for the whole app. Color carries meaning only:
 * working is healthy (green), waiting needs nothing yet (blue), blocked needs
 * attention (amber), completed and unknown are neutral.
 */
const TONE: Record<
  SessionState,
  { dot: string; badge: string; pulse: boolean; label: string }
> = {
  working: {
    dot: "bg-success",
    badge: "bg-ok-tint text-success ring-success/25",
    pulse: true,
    label: "Working",
  },
  waiting: {
    dot: "bg-info",
    badge: "bg-info-tint text-info ring-info/25",
    pulse: false,
    label: "Waiting",
  },
  blocked: {
    dot: "bg-warning",
    badge: "bg-wait-tint text-warning ring-warning/25",
    pulse: false,
    label: "Blocked",
  },
  completed: {
    dot: "bg-muted-foreground",
    badge: "bg-muted text-muted-foreground ring-surface-border",
    pulse: false,
    label: "Completed",
  },
  unknown: {
    dot: "bg-faint-foreground",
    badge: "bg-muted text-faint-foreground ring-surface-border",
    pulse: false,
    label: "Unknown",
  },
};

export function stateLabel(state: SessionState): string {
  return TONE[state].label;
}

export function StateDot({
  state,
  size = "md",
}: {
  state: SessionState;
  size?: "sm" | "md";
}) {
  const tone = TONE[state];
  return (
    <span
      aria-hidden="true"
      className={cx(
        "inline-block shrink-0 rounded-full",
        size === "sm" ? "size-1.5" : "size-2",
        tone.dot,
        tone.pulse && "animate-spawn-pulse",
      )}
    />
  );
}

export function StateBadge({
  state,
  size = "sm",
}: {
  state: SessionState;
  size?: "sm" | "md";
}) {
  const tone = TONE[state];
  return (
    <span
      className={cx(
        "inline-flex shrink-0 items-center rounded-md font-medium ring-1 ring-inset",
        size === "md"
          ? "h-7 gap-2 px-2.5 text-label"
          : "h-5 gap-1.5 px-1.5 text-caption",
        tone.badge,
      )}
    >
      <StateDot state={state} size={size === "md" ? "md" : "sm"} />
      {tone.label}
    </span>
  );
}

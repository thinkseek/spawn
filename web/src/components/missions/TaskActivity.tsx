import { Activity } from "lucide-react";

import { activityLine } from "../../taskActivity";
import type { TaskActivity as Activity_, TaskLivenessView } from "../../taskActivity";
import { cx } from "../ui";

function livenessBadgeClass(tone: TaskLivenessView["tone"]): string {
  switch (tone) {
    case "success":
      return "bg-ok-tint text-success ring-success/25";
    case "info":
      return "bg-info-tint text-info ring-info/25";
    case "warning":
      return "bg-wait-tint text-warning ring-warning/25";
    default:
      return "bg-muted text-muted-foreground ring-surface-border";
  }
}

function livenessDotClass(tone: TaskLivenessView["tone"], pulse: boolean): string {
  const pulseClass = pulse ? "animate-spawn-pulse" : "";
  switch (tone) {
    case "success":
      return cx("bg-success", pulseClass);
    case "info":
      return "bg-info";
    case "warning":
      return "bg-warning";
    default:
      return "bg-muted-foreground";
  }
}

/** What a delivered task's worker did since the task reached it
 * (objective 211): its last engine output and its tool calls since the
 * delivery, plus execution liveness classification (#63). Nothing for a
 * task with neither activity nor liveness summary. */
export function TaskActivityLine({
  activity,
  liveness = null,
  now,
}: {
  activity: Activity_ | null;
  liveness?: TaskLivenessView | null;
  now: number;
}) {
  if (!activity && !liveness) return null;
  const line = activity ? activityLine(activity, now) : null;
  const isWarning = liveness?.state === "stalled" || (line?.warning ?? false);
  const textTone =
    liveness?.state === "stalled"
      ? "text-warning"
      : liveness?.state === "briefed_quiet"
        ? "text-info"
        : isWarning
          ? "text-warning"
          : "text-muted-foreground";

  return (
    <p
      className={cx("flex min-w-0 flex-wrap items-center gap-1.5 text-caption", textTone)}
      role="status"
    >
      {liveness && (
        <span
          className={cx(
            "inline-flex shrink-0 items-center gap-1 rounded px-1.5 py-0.5 text-micro font-medium ring-1 ring-inset",
            livenessBadgeClass(liveness.tone),
          )}
        >
          <span
            aria-hidden="true"
            className={cx("size-1.5 rounded-full", livenessDotClass(liveness.tone, liveness.pulse))}
          />
          {liveness.label}
        </span>
      )}
      {line && (
        <span className="flex min-w-0 items-center gap-1">
          <Activity size={11} aria-hidden="true" className="shrink-0" />
          <span className="tnum min-w-0 truncate">{line.text}</span>
        </span>
      )}
    </p>
  );
}

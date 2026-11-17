import { Activity } from "lucide-react";

import { activityLine } from "../../taskActivity";
import type { TaskActivity as Activity_ } from "../../taskActivity";
import { cx } from "../ui";

/** What a delivered task's worker did since the task reached it
 * (objective 211): its last engine output and its tool calls since the
 * delivery, in the warning tone once no output came for 10 minutes or
 * more. Nothing for a task with no activity (not delivered). */
export function TaskActivityLine({ activity, now }: { activity: Activity_ | null; now: number }) {
  if (!activity) return null;
  const line = activityLine(activity, now);
  return (
    <p
      className={cx(
        "flex min-w-0 items-center gap-1.5 text-caption",
        line.warning ? "text-warning" : "text-muted-foreground",
      )}
      role="status"
    >
      <Activity size={11} aria-hidden="true" className="shrink-0" />
      <span className="tnum min-w-0 truncate">{line.text}</span>
    </p>
  );
}

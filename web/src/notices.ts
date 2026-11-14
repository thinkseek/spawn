import type { Alert } from "./api.ts";

/** A notice no later notice supersedes, and how many notices'
 * `superseded_by` chains end at it (objective 151). */
export interface CurrentNotice {
  alert: Alert;
  earlier: number;
}

/** The notices of `alerts` no later notice supersedes, in the answer's
 * order, each with how many came before it: the notices whose
 * `superseded_by` chain ends at it. A notice whose next one is not in the
 * answer stands as current, so nothing is hidden. Open and cleared rows are
 * not notices and are never listed. */
export function currentNotices(alerts: readonly Alert[]): CurrentNotice[] {
  const notices = alerts.filter((alert) => alert.state === "notice");
  const byKey = new Map(notices.map((alert) => [alert.key, alert]));
  const next = (alert: Alert): Alert | undefined => {
    const key = alert.superseded_by;
    return typeof key === "string" ? byKey.get(key) : undefined;
  };
  const earlier = new Map<string, number>();
  for (const alert of notices) {
    // Follow the chain to its end; a cycle (never served) stops it.
    let at = alert;
    const seen = new Set([at.key]);
    for (let step = next(at); step !== undefined && !seen.has(step.key); step = next(at)) {
      at = step;
      seen.add(at.key);
    }
    if (at !== alert) earlier.set(at.key, (earlier.get(at.key) ?? 0) + 1);
  }
  return notices
    .filter((alert) => next(alert) === undefined)
    .map((alert) => ({ alert, earlier: earlier.get(alert.key) ?? 0 }));
}

/** "N earlier", or null for none. */
export function earlierWords(earlier: number): string | null {
  return earlier > 0 ? `${earlier} earlier` : null;
}

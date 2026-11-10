import { BellRing } from "lucide-react";

import type { Alert } from "../../api";
import { fetchAlerts, usePoll } from "../../api";
import { absoluteTitle, localDateTime, relativeTime } from "../../format";
import { LiveStatus, PageHeader, cx } from "../ui";

/** What each state looks like: a condition still true, news, or done. */
function stateClass(state: Alert["state"]): string {
  switch (state) {
    case "open":
      return "bg-fail-tint text-destructive ring-destructive/25";
    case "notice":
      return "bg-wait-tint text-warning ring-warning/25";
    default:
      return "bg-muted text-muted-foreground ring-surface-border";
  }
}

function Evidence({ label, value }: { label: string; value: unknown }) {
  if (value === null || value === undefined) return null;
  return (
    <details className="mt-1 text-micro text-muted-foreground">
      <summary className="cursor-pointer select-none text-faint-foreground">
        {label}
      </summary>
      <pre className="mt-1 overflow-x-auto whitespace-pre-wrap break-words rounded bg-muted/60 p-2 font-mono text-micro">
        {JSON.stringify(value, null, 2)}
      </pre>
    </details>
  );
}

function AlertRow({ alert }: { alert: Alert }) {
  const at =
    alert.state === "cleared" ? (alert.cleared_at_ms ?? alert.fired_at_ms) : alert.fired_at_ms;
  return (
    <li className="border-b border-surface-border px-4 py-3 last:border-b-0">
      <div className="flex flex-wrap items-baseline gap-2">
        <span
          className={cx(
            "rounded px-1.5 py-px text-micro font-medium uppercase tracking-wide ring-1 ring-inset",
            stateClass(alert.state),
          )}
        >
          {alert.state}
        </span>
        <span className="font-mono text-caption text-foreground">{alert.kind}</span>
        <span className="text-caption text-muted-foreground">{alert.subject}</span>
        <span className="tnum ml-auto text-micro text-faint-foreground" title={absoluteTitle(at)}>
          {alert.state === "open" ? "since " : ""}
          {relativeTime(at)} · {localDateTime(at, true)}
        </span>
      </div>
      <p className="mt-1 text-caption text-foreground">{alert.text}</p>
      {alert.scored_at_ms != null && (
        <p className="tnum mt-0.5 text-micro text-muted-foreground" title={absoluteTitle(alert.scored_at_ms)}>
          last scored {relativeTime(alert.scored_at_ms)}
        </p>
      )}
      <Evidence label="evidence" value={alert.evidence} />
      {alert.state === "cleared" && <Evidence label="cleared by" value={alert.cleared_evidence} />}
    </li>
  );
}

function Section({
  title,
  alerts,
  empty,
}: {
  title: string;
  alerts: Alert[];
  empty: string;
}) {
  return (
    <section className="rounded-lg border border-surface-border bg-surface">
      <header className="flex h-9 items-center justify-between border-b border-surface-border px-4 text-caption font-medium text-foreground">
        {title}
        <span className="tnum text-micro text-faint-foreground">{alerts.length}</span>
      </header>
      {alerts.length === 0 ? (
        <p className="px-4 py-3 text-caption text-faint-foreground">{empty}</p>
      ) : (
        <ul>
          {alerts.map((alert) => (
            <AlertRow key={alert.key} alert={alert} />
          ))}
        </ul>
      )}
    </section>
  );
}

/** The Alerts view: open conditions, the window's news, and what cleared. */
export function AlertsView() {
  const alerts = usePoll("alerts", fetchAlerts, 3_000);
  const data = alerts.data ?? [];
  const open = data.filter((alert) => alert.state === "open");
  const notices = data.filter((alert) => alert.state === "notice");
  const cleared = data.filter((alert) => alert.state === "cleared");

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <>
            <BellRing size={15} className="text-muted-foreground" />
            <h1 className="text-body font-semibold text-foreground">Alerts</h1>
            <span className="text-caption text-muted-foreground">
              what Spawn raised on its own, with its evidence
            </span>
          </>
        }
      >
        <LiveStatus error={alerts.error} label="" />
      </PageHeader>
      <div className="min-h-0 flex-1 space-y-3 overflow-y-auto p-3 sm:p-4">
        <Section title="Open" alerts={open} empty="Nothing is open." />
        <Section title="News" alerts={notices} empty="No news in the window." />
        <Section
          title="Cleared"
          alerts={cleared}
          empty="Nothing cleared in the window."
        />
      </div>
    </main>
  );
}

/** The open alerts on every page: kind, subject, since when, the text. */
/** The open alerts that still apply, on every page but Home (whose "Needs
 * you" lists them with what to do): one line, the newest, and a count. An
 * alert about a session that has ended never reaches it (home.ts
 * liveAlerts). */
export function OpenAlerts({ alerts, onOpen }: { alerts: Alert[]; onOpen: () => void }) {
  if (alerts.length === 0) return null;
  const newest = alerts.reduce((latest, alert) => (alert.fired_at_ms > latest.fired_at_ms ? alert : latest));
  return (
    <div className="shrink-0 border-b border-surface-border bg-fail-tint px-3 py-1.5">
      <button
        type="button"
        onClick={onOpen}
        className="flex w-full min-w-0 items-center gap-2 text-left text-micro"
        title="Open the Alerts page"
      >
        <span className="shrink-0 font-medium text-destructive">
          {alerts.length} open alert{alerts.length === 1 ? "" : "s"}
        </span>
        <span className="min-w-0 truncate text-muted-foreground">
          <span className="text-foreground">{newest.text}</span>{" "}
          <span className="tnum text-faint-foreground">since {relativeTime(newest.fired_at_ms)}</span>
        </span>
        <span className="ml-auto shrink-0 text-caption font-medium text-foreground">Alerts</span>
      </button>
    </div>
  );
}

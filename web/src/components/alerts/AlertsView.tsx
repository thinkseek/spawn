import { BellRing } from "lucide-react";
import { useState } from "react";

import type { Alert, PollResult } from "../../api";
import { fetchAlerts, usePoll } from "../../api";
import { absoluteTitle, localDateTime, relativeTime } from "../../format";
import { LiveStatus, PageHeader, cx } from "../ui";

/** What each state looks like: a condition still true, news, or done. */
function stateClass(state: Alert["state"]): string {
  switch (state) {
    case "open":
      return "bg-destructive/12 text-destructive ring-destructive/25";
    case "notice":
      return "bg-warning/15 text-warning ring-warning/25";
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
export function OpenAlerts({ alerts, onOpen }: { alerts: PollResult<Alert[]>; onOpen: () => void }) {
  const [expanded, setExpanded] = useState(true);
  const open = (alerts.data ?? []).filter((alert) => alert.state === "open");
  if (open.length === 0) return null;
  const shown = expanded ? open : open.slice(0, 1);
  return (
    <div className="shrink-0 border-b border-destructive/25 bg-destructive/[0.06] px-3 py-1.5">
      <div className="flex items-center gap-2 text-micro">
        <button
          type="button"
          onClick={() => setExpanded((value) => !value)}
          className="shrink-0 rounded px-1 font-medium text-destructive"
          title={expanded ? "Show only the first" : "Show every open alert"}
        >
          {open.length} open alert{open.length === 1 ? "" : "s"}
        </button>
        <ul className="flex min-w-0 flex-wrap gap-x-3 gap-y-1">
          {shown.map((alert) => (
            <li key={alert.key} className="min-w-0 truncate text-muted-foreground">
              <span className="font-mono text-foreground">{alert.kind}</span>{" "}
              <span>{alert.subject}</span>{" "}
              <span className="tnum text-faint-foreground">
                since {relativeTime(alert.fired_at_ms)}
              </span>{" "}
              <span className="text-foreground">{alert.text}</span>
            </li>
          ))}
        </ul>
        <button
          type="button"
          onClick={onOpen}
          className="ml-auto shrink-0 rounded px-1 text-caption font-medium text-foreground underline-offset-2 hover:underline"
        >
          Alerts
        </button>
      </div>
    </div>
  );
}

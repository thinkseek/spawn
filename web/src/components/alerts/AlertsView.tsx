import { BellRing } from "lucide-react";

import type { Alert } from "../../api";
import { fetchAlerts, usePoll } from "../../api";
import type { FleetAgent } from "../../fleet";
import { alertMoot, alertStale, alertTitle } from "../../home";
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

function AlertRow({ alert, title = null, note = null }: { alert: Alert; title?: string | null; note?: string | null }) {
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
        {title ? (
          <>
            <span className="text-caption font-medium text-foreground">{title}</span>
            <span className="font-mono text-micro text-faint-foreground">{alert.kind}</span>
          </>
        ) : (
          <>
            <span className="font-mono text-caption text-foreground">{alert.kind}</span>
            <span className="text-caption text-muted-foreground">{alert.subject}</span>
          </>
        )}
        <span className="tnum ml-auto text-micro text-faint-foreground" title={absoluteTitle(at)}>
          {alert.state === "open" ? "since " : ""}
          {relativeTime(at)} · {localDateTime(at, true)}
        </span>
      </div>
      <p className="mt-1 text-caption text-foreground">{alert.text}</p>
      {note && <p className="mt-0.5 text-micro text-warning">{note}</p>}
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

/** The Alerts view (design §2.9): what is open and still applies, each in
 * the words home's "Needs you" uses; what an ended session made moot,
 * folded; what cleared. The task and mission notices are news: home's
 * digest tells them by task, and the raw list stays here, folded. */
export function AlertsView({
  agents = null,
  nameOf = () => null,
  onShowHome,
}: {
  agents?: readonly FleetAgent[] | null;
  nameOf?: (session: string) => string | null;
  onShowHome?: () => void;
}) {
  const alerts = usePoll("alerts", fetchAlerts, 3_000);
  const data = alerts.data ?? [];
  const open = data.filter((alert) => alert.state === "open");
  const applies = open.filter((alert) => !alertMoot(alert, agents));
  const moot = open.filter((alert) => alertMoot(alert, agents));
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
        <section className="rounded-lg border border-surface-border bg-surface" aria-label="Open">
          <header className="flex h-9 items-center justify-between border-b border-surface-border px-4 text-caption font-medium text-foreground">
            Open
            <span className="tnum text-micro text-faint-foreground">{applies.length}</span>
          </header>
          {applies.length === 0 ? (
            <p className="px-4 py-3 text-caption text-faint-foreground">Nothing open applies now.</p>
          ) : (
            <ul>
              {applies.map((alert) => {
                const stale = alertStale(alert, agents ?? null);
                return (
                  <AlertRow
                    key={alert.key}
                    alert={alert}
                    title={alertTitle(alert, nameOf(alert.subject))}
                    note={stale ? "Not scored since it fired: its latest judgement has no score. To know, not to act." : null}
                  />
                );
              })}
            </ul>
          )}
          {moot.length > 0 && (
            <details className="border-t border-surface-border">
              <summary className="cursor-pointer select-none px-4 py-2 text-caption text-muted-foreground hover:text-foreground">
                Resolved by the session's end <span className="tnum text-faint-foreground">{moot.length}</span>
                <span className="ml-2 text-micro text-faint-foreground">still open in the record; the server clears them</span>
              </summary>
              <ul className="opacity-70">
                {moot.map((alert) => (
                  <AlertRow key={alert.key} alert={alert} title={alertTitle(alert, nameOf(alert.subject))} />
                ))}
              </ul>
            </details>
          )}
        </section>
        {/* What cleared is history: folded, by name. */}
        <section className="rounded-lg border border-surface-border bg-surface" aria-label="Cleared">
          <details>
            <summary className="cursor-pointer select-none px-4 py-2 text-caption text-muted-foreground hover:text-foreground">
              Cleared in the window <span className="tnum text-faint-foreground">{cleared.length}</span>
            </summary>
            {cleared.length === 0 ? (
              <p className="px-4 py-3 text-caption text-faint-foreground">Nothing cleared in the window.</p>
            ) : (
              <ul>
                {cleared.map((alert) => (
                  <AlertRow key={alert.key} alert={alert} title={alertTitle(alert, nameOf(alert.subject))} />
                ))}
              </ul>
            )}
          </details>
        </section>
        <section className="rounded-lg border border-surface-border bg-surface" aria-label="News">
          <p className="px-4 py-3 text-caption text-muted-foreground">
            Task and mission changes are news, told by task in home's digest
            {onShowHome ? (
              <>
                {" "}
                (
                <button type="button" onClick={onShowHome} className="font-medium text-foreground underline-offset-2 hover:underline">
                  Home
                </button>
                )
              </>
            ) : null}
            .
          </p>
          <details className="border-t border-surface-border">
            <summary className="cursor-pointer select-none px-4 py-2 text-caption text-muted-foreground hover:text-foreground">
              All notices <span className="tnum text-faint-foreground">{notices.length}</span>
            </summary>
            {notices.length === 0 ? (
              <p className="px-4 py-3 text-caption text-faint-foreground">No notices in the window.</p>
            ) : (
              <ul>
                {notices.map((alert) => (
                  <AlertRow key={alert.key} alert={alert} />
                ))}
              </ul>
            )}
          </details>
        </section>
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

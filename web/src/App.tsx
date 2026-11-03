import { PanelRight } from "lucide-react";
import { useEffect, useState } from "react";

import { fetchOverview, fetchSession, usePoll } from "./api";
import type { SessionDetail } from "./api";
import { DetailsRail, RailContent } from "./components/DetailsRail";
import { MobileNav, Sidebar } from "./components/Sidebar";
import { StateBadge } from "./components/StateDot";
import { Timeline } from "./components/Timeline";
import { RsiView } from "./components/rsi/RsiView";
import { Button, Drawer, LiveStatus, PageHeader } from "./components/ui";
import { relativeTime } from "./format";
import { RSI_SESSION } from "./rsi";

export type View = "rsi" | "sessions";

function viewFromHash(): View {
  return window.location.hash.startsWith("#/sessions") ? "sessions" : "rsi";
}

export default function App() {
  const overview = usePoll("overview", fetchOverview);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [railOpen, setRailOpen] = useState(false);
  // The RSI view is the landing page; the hash keeps the view across reloads.
  const [view, setView] = useState<View>(viewFromHash);

  useEffect(() => {
    const next = view === "rsi" ? "#/rsi" : "#/sessions";
    if (window.location.hash !== next) {
      window.history.replaceState(null, "", next);
    }
  }, [view]);

  useEffect(() => {
    const onHash = () => setView(viewFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  useEffect(() => {
    const data = overview.data;
    if (!data) return;
    setSelectedId((current) => {
      if (
        current &&
        data.sessions.some((session) => session.session_id === current)
      ) {
        return current;
      }
      return data.sessions[0]?.session_id ?? null;
    });
  }, [overview.data]);

  // The session poll runs only in the Sessions view.
  const sessionKey = view === "sessions" ? (selectedId ?? "none") : "hidden";
  const session = usePoll<SessionDetail | null>(sessionKey, (signal) =>
    view === "sessions" && selectedId
      ? fetchSession(selectedId, signal)
      : Promise.resolve(null),
  );

  // `session.data` lags one render behind a selection change; never show the
  // previous session under the new header.
  const detail = session.data?.session_id === selectedId ? session.data : null;
  const summary =
    overview.data?.sessions.find((entry) => entry.session_id === selectedId) ??
    null;
  const state = detail?.state ?? summary?.state ?? "unknown";
  const freshness = detail?.freshness ?? null;
  const error = overview.error ?? session.error;
  const agentId = detail?.agent_id || summary?.agent_id || null;
  const sessionId = detail?.session_id ?? selectedId;
  const rsi = overview.data?.rsi ?? null;
  const observation = summary?.observation ?? null;

  return (
    <div className="flex h-dvh w-full flex-col overflow-hidden bg-app-shell text-label text-foreground md:flex-row">
      <MobileNav
        view={view}
        onShowRsi={() => setView("rsi")}
        onShowSessions={() => setView("sessions")}
      />
      <Sidebar
        sessions={overview.data?.sessions ?? []}
        selectedId={view === "sessions" ? selectedId : null}
        view={view}
        rsi={rsi}
        error={overview.error}
        onShowRsi={() => setView("rsi")}
        onShowSessions={() => setView("sessions")}
        onSelect={(id) => {
          setSelectedId(id);
          setView("sessions");
        }}
      />

      {view === "rsi" ? (
        <RsiView
          onOpenSession={() => {
            setSelectedId(RSI_SESSION);
            setView("sessions");
          }}
        />
      ) : (
        <div className="flex min-h-0 min-w-0 flex-1">
          <main className="flex min-w-0 flex-1 flex-col bg-page-canvas">
            <PageHeader
              title={
                <>
                  <h1 className="min-w-0 truncate text-body font-semibold text-foreground">
                    {agentId ?? sessionId ?? "No session selected"}
                  </h1>
                  {sessionId && agentId && (
                    <span className="hidden truncate rounded bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground sm:inline">
                      {sessionId}
                    </span>
                  )}
                  {sessionId && <StateBadge state={state} />}
                </>
              }
            >
              <span className="tnum hidden text-caption text-muted-foreground lg:inline">
                {freshness ? `${freshness.event_count} events` : "0 events"}
                <span className="mx-1.5 text-faint-foreground">·</span>
                {freshness
                  ? `last ${relativeTime(freshness.last_event_at_ms)}`
                  : "no events yet"}
              </span>
              <LiveStatus error={error} label="" />
              <Button
                onClick={() => setRailOpen(true)}
                className="lg:hidden"
                title="Session details"
              >
                <PanelRight size={13} />
                Details
              </Button>
            </PageHeader>

            {(overview.data?.sessions.length ?? 0) > 1 && (
              <div className="flex shrink-0 gap-1.5 overflow-x-auto border-b border-surface-border px-3 py-2 md:hidden">
                {overview.data?.sessions.map((entry) => (
                  <button
                    key={entry.session_id}
                    type="button"
                    onClick={() => setSelectedId(entry.session_id)}
                    className={`shrink-0 rounded-md px-2.5 py-1 text-caption ${
                      entry.session_id === selectedId
                        ? "bg-surface-selected text-foreground"
                        : "text-muted-foreground"
                    }`}
                  >
                    {entry.agent_id || entry.session_id}
                  </button>
                ))}
              </div>
            )}

            {!selectedId ? (
              <Placeholder text="Waiting for a session." />
            ) : detail ? (
              <Timeline key={selectedId} events={detail.timeline} />
            ) : (
              <Placeholder
                text={
                  session.error
                    ? "Could not load this session."
                    : "Loading session…"
                }
              />
            )}
          </main>

          <DetailsRail
            className="hidden lg:flex"
            rsi={rsi}
            detail={detail}
            observation={observation}
          />
          {railOpen && (
            <Drawer
              width="max-w-sm"
              onClose={() => setRailOpen(false)}
              header={
                <h2 className="text-title-sm font-semibold text-foreground">
                  Session details
                </h2>
              }
            >
              <div className="min-h-0 flex-1 space-y-3 overflow-y-auto bg-app-shell p-3">
                <RailContent
                  rsi={rsi}
                  detail={detail}
                  observation={observation}
                />
              </div>
            </Drawer>
          )}
        </div>
      )}
    </div>
  );
}

function Placeholder({ text }: { text: string }) {
  return (
    <div className="grid min-h-0 flex-1 place-items-center px-6">
      <p className="text-caption text-faint-foreground">{text}</p>
    </div>
  );
}

import { WifiOff } from "lucide-react";
import { useEffect, useState } from "react";

import { fetchOverview, fetchSession, usePoll } from "./api";
import type { SessionDetail } from "./api";
import { DetailsRail } from "./components/DetailsRail";
import { Sidebar } from "./components/Sidebar";
import { StateBadge } from "./components/StateDot";
import { Timeline } from "./components/Timeline";
import { RsiView } from "./components/rsi/RsiView";
import { relativeTime } from "./format";
import { RSI_SESSION } from "./rsi";

export type View = "rsi" | "sessions";

function viewFromHash(): View {
  return window.location.hash.startsWith("#/sessions") ? "sessions" : "rsi";
}

export default function App() {
  const overview = usePoll("overview", fetchOverview);
  const [selectedId, setSelectedId] = useState<string | null>(null);
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
  const sessionId = detail?.session_id ?? selectedId;

  return (
    <div className="flex h-dvh w-full overflow-hidden bg-app-shell text-foreground">
      <Sidebar
        sessions={overview.data?.sessions ?? []}
        selectedId={view === "sessions" ? selectedId : null}
        view={view}
        onShowRsi={() => setView("rsi")}
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
        <>
          <main className="flex min-w-0 flex-1 flex-col bg-page-canvas">
            <header className="sticky top-0 z-10 shrink-0 border-b border-surface-border bg-page-canvas px-5 py-3">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
                <h1 className="min-w-0 truncate font-mono text-title-sm font-medium text-foreground">
                  {sessionId ?? "No session selected"}
                </h1>
                <StateBadge state={state} />
                <div className="ml-auto flex items-center gap-3">
                  <span className="text-caption text-muted-foreground">
                    {freshness ? `${freshness.event_count} events` : "0 events"}
                    <span className="mx-1.5 text-faint-foreground">·</span>
                    {freshness
                      ? `last event ${relativeTime(freshness.last_event_at_ms)}`
                      : "no events yet"}
                  </span>
                  {error && (
                    <span
                      title={error}
                      className="inline-flex items-center gap-1 rounded-sm border border-destructive/30 bg-destructive/10 px-1.5 py-0.5 text-micro font-medium text-destructive"
                    >
                      <WifiOff size={11} />
                      disconnected
                    </span>
                  )}
                </div>
              </div>
            </header>

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

          <DetailsRail rsi={overview.data?.rsi ?? null} detail={detail} />
        </>
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

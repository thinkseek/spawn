import { PanelRight } from "lucide-react";
import { Suspense, lazy, useEffect, useMemo, useState } from "react";

import { fetchAttention, fetchOverview, fetchSession, usePoll } from "./api";
import type { PollResult, SessionDetail } from "./api";
import { markNeedsHuman, needsHuman } from "./attention";
import { DetailsRail, RailContent } from "./components/DetailsRail";
import { MobileNav, Sidebar } from "./components/Sidebar";
import { StateBadge } from "./components/StateDot";
import { Timeline } from "./components/Timeline";
import { FleetView } from "./components/fleet/FleetView";
import { MissionsView } from "./components/missions/MissionsView";
import { RsiView } from "./components/rsi/RsiView";
import { ScorecardView } from "./components/scorecard/ScorecardView";
import { Button, Drawer, LiveStatus, PageHeader } from "./components/ui";
import type { Fleet } from "./fleet";
import { fetchFleet } from "./fleet";
import { relativeTime } from "./format";
import type { Missions } from "./missions";
import { fetchMissions } from "./missions";
import { fleetContext } from "./ordering";

// Its own chunk: Motion and d3 load only when the Show view opens.
const ShowView = lazy(() =>
  import("./components/show/ShowView").then((module) => ({ default: module.ShowView })),
);

export type View = "fleet" | "missions" | "rsi" | "scorecard" | "show" | "sessions";

interface Route {
  view: View;
  /** Agent open in the fleet view's detail drawer. */
  agent: string | null;
}

function routeFromHash(): Route {
  const hash = window.location.hash;
  if (hash.startsWith("#/sessions")) return { view: "sessions", agent: null };
  if (hash.startsWith("#/rsi")) return { view: "rsi", agent: null };
  if (hash.startsWith("#/missions")) return { view: "missions", agent: null };
  if (hash.startsWith("#/scorecard")) return { view: "scorecard", agent: null };
  if (hash.startsWith("#/show")) return { view: "show", agent: null };
  const match = /^#\/fleet\/(.+)$/.exec(hash);
  let agent: string | null = null;
  if (match) {
    try {
      agent = decodeURIComponent(match[1]);
    } catch {
      agent = null;
    }
  }
  return { view: "fleet", agent };
}

function hashFor(route: Route): string {
  if (route.view === "fleet") {
    return route.agent
      ? `#/fleet/${encodeURIComponent(route.agent)}`
      : "#/fleet";
  }
  if (route.view === "missions") return "#/missions";
  if (route.view === "scorecard") return "#/scorecard";
  // The Show view keeps its options (`?present`, `?replay`) in its hash.
  if (route.view === "show") {
    return window.location.hash.startsWith("#/show") ? window.location.hash : "#/show";
  }
  return route.view === "rsi" ? "#/rsi" : "#/sessions";
}

/** `?present`, in the query or the hash: the Show view alone, no chrome,
 * for a wall screen. */
function presenting(): boolean {
  const hashQuery = window.location.hash.split("?")[1] ?? "";
  return (
    new URLSearchParams(window.location.search).has("present") ||
    new URLSearchParams(hashQuery).has("present")
  );
}

export default function App() {
  const overview = usePoll("overview", fetchOverview);
  // Both polled in every view: the sidebar shows who needs a human, which is
  // the dispatcher's view (/api/attention), never a judge's flag.
  const fleetPoll = usePoll("fleet", fetchFleet, 3_000);
  const attention = usePoll("attention", fetchAttention, 3_000);
  const needs = useMemo(
    () => (attention.data ? needsHuman(attention.data) : null),
    [attention.data],
  );
  const fleetData = useMemo<Fleet | null>(
    () =>
      fleetPoll.data && {
        ...fleetPoll.data,
        agents: markNeedsHuman(fleetPoll.data.agents, needs),
      },
    [fleetPoll.data, needs],
  );
  const fleet: PollResult<Fleet> = { data: fleetData, error: fleetPoll.error };
  const humanCount = {
    count: needs?.count ?? null,
    error: attention.data ? null : attention.error,
  };
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [railOpen, setRailOpen] = useState(false);
  // The fleet view is the landing page; the hash keeps the view (and the
  // open agent) across reloads.
  const [route, setRoute] = useState<Route>(routeFromHash);
  const view = route.view;
  // Polled in every view too: the sidebar and the fleet board group agents
  // by the tasks they hold. Faster while the missions page shows them.
  const missions = usePoll<Missions>(
    "missions",
    fetchMissions,
    view === "missions" ? 5_000 : 15_000,
  );
  const context = useMemo(() => fleetContext(missions.data), [missions.data]);
  const setView = (next: View) => setRoute({ view: next, agent: null });

  useEffect(() => {
    const next = hashFor(route);
    if (window.location.hash !== next) {
      window.history.replaceState(null, "", next);
    }
  }, [route]);

  useEffect(() => {
    const onHash = () => setRoute(routeFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  const openTimeline = (sessionId: string) => {
    setSelectedId(sessionId);
    setView("sessions");
  };

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

  if (view === "show" && presenting()) {
    return (
      <div className="flex h-dvh w-full overflow-hidden bg-page-canvas text-label text-foreground">
        <Suspense fallback={null}>
          <ShowView fleet={fleet} present />
        </Suspense>
      </div>
    );
  }

  return (
    <div className="flex h-dvh w-full flex-col overflow-hidden bg-app-shell text-label text-foreground md:flex-row">
      <MobileNav
        view={view}
        attention={humanCount}
        onShowFleet={() => setView("fleet")}
        onShowMissions={() => setView("missions")}
        onShowRsi={() => setView("rsi")}
        onShowScorecard={() => setView("scorecard")}
        onShowShow={() => setView("show")}
        onShowSessions={() => setView("sessions")}
      />
      <Sidebar
        sessions={overview.data?.sessions ?? []}
        agents={fleet.data?.agents ?? null}
        context={context}
        selectedId={view === "sessions" ? selectedId : null}
        view={view}
        rsi={rsi}
        error={overview.error}
        attention={humanCount}
        onShowFleet={() => setView("fleet")}
        onShowMissions={() => setView("missions")}
        onShowRsi={() => setView("rsi")}
        onShowScorecard={() => setView("scorecard")}
        onShowShow={() => setView("show")}
        onShowSessions={() => setView("sessions")}
        onSelect={(id) => {
          setSelectedId(id);
          setView("sessions");
        }}
      />

      {view === "fleet" ? (
        <FleetView
          fleet={fleet}
          attention={attention}
          context={context}
          selectedId={route.agent}
          onSelect={(agent) => setRoute({ view: "fleet", agent })}
          onOpenTimeline={openTimeline}
        />
      ) : view === "missions" ? (
        <MissionsView missions={missions} />
      ) : view === "rsi" ? (
        <RsiView onOpenSession={openTimeline} />
      ) : view === "scorecard" ? (
        <ScorecardView missions={missions.data} />
      ) : view === "show" ? (
        <Suspense fallback={<div className="flex-1 bg-page-canvas" />}>
          <ShowView fleet={fleet} present={false} />
        </Suspense>
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

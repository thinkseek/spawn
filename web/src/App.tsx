import { Orbit, PanelRight, ShieldAlert, WifiOff } from "lucide-react";
import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  fetchAlerts,
  fetchAttention,
  fetchOverview,
  fetchSessionTail,
  getInitialTokenProblem,
  getServeToken,
  live,
  probeAuth,
  setServeToken,
  subscribeAuthSync,
  usePoll,
} from "./api";
import { PAGE_EVENTS } from "./sessionWindow";
import type { PollResult, SessionTail } from "./api";
import { markNeedsHuman, needsHuman } from "./attention";
import { inputTarget } from "./commands";
import { DetailsRail, ExecutionSummary, RailContent } from "./components/DetailsRail";
import { MobileNav, Sidebar, ThemeSwitch } from "./components/Sidebar";
import { Timeline } from "./components/Timeline";
import { TokenGate } from "./components/TokenGate";
import {
  authStateFromProbe,
  authStateOnUnauthorized,
  classifyTokenRole,
  isAbortError,
  type AuthGateState,
  type SessionRole,
} from "./token";
import { AgentsConsoleView } from "./components/agents/AgentsConsoleView";
import { AlertsView, OpenAlerts } from "./components/alerts/AlertsView";
import { CostView } from "./components/cost/CostView";
import { CoveragePage } from "./components/features/CoveragePage";
import { FeatureMapView } from "./components/features/FeatureMapView";
import { FleetView } from "./components/fleet/FleetView";
import { GradingView } from "./components/grading/GradingView";
import { MissionsView } from "./components/missions/MissionsView";
import { RsiView } from "./components/rsi/RsiView";
import { ScorecardView } from "./components/scorecard/ScorecardView";
import { StorageTreeView } from "./components/storage/StorageTreeView";
import { Button, Drawer, LiveStatus, PageHeader } from "./components/ui";
import type { Fleet } from "./fleet";
import { fetchFleet } from "./fleet";
import { relativeTime } from "./format";
import type { Missions } from "./missions";
import { fetchMissions } from "./missions";
import { fleetContext, missingAlwaysOn } from "./ordering";
import { actionableAlerts, agentNames, needsYou } from "./home";

// Its own chunk: Motion and d3 load only when the Show view opens.
const ShowView = lazy(() =>
  import("./components/show/ShowView").then((module) => ({ default: module.ShowView })),
);

export type View =
  | "fleet"
  | "agents"
  | "missions"
  | "grading"
  | "features"
  | "coverage"
  | "rsi"
  | "alerts"
  | "scorecard"
  | "cost"
  | "storage"
  | "show"
  | "sessions";

interface Route {
  view: View;
  /** Agent open in the fleet view's detail drawer, or terminal address in Agents view. */
  agent: string | null;
}

function routeFromHash(): Route {
  const hash = window.location.hash;
  if (hash.startsWith("#/sessions")) return { view: "sessions", agent: null };
  const agentsMatch = /^#\/agents\/(.+)$/.exec(hash);
  if (agentsMatch) {
    let agent: string | null = null;
    try {
      agent = decodeURIComponent(agentsMatch[1]);
    } catch {
      agent = null;
    }
    return { view: "agents", agent };
  }
  if (hash.startsWith("#/agents")) return { view: "agents", agent: null };
  if (hash.startsWith("#/rsi")) return { view: "rsi", agent: null };
  if (hash.startsWith("#/grading")) return { view: "grading", agent: null };
  if (hash.startsWith("#/missions")) return { view: "missions", agent: null };
  if (hash.startsWith("#/features")) return { view: "features", agent: null };
  if (hash.startsWith("#/coverage")) return { view: "coverage", agent: null };
  if (hash.startsWith("#/alerts")) return { view: "alerts", agent: null };
  if (hash.startsWith("#/scorecard")) return { view: "scorecard", agent: null };
  if (hash.startsWith("#/cost")) return { view: "cost", agent: null };
  if (hash.startsWith("#/storage")) return { view: "storage", agent: null };
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
  if (route.view === "agents") {
    return route.agent
      ? `#/agents/${encodeURIComponent(route.agent)}`
      : "#/agents";
  }
  if (route.view === "missions") return "#/missions";
  if (route.view === "grading") return "#/grading";
  if (route.view === "features") return "#/features";
  if (route.view === "coverage") return "#/coverage";
  if (route.view === "alerts") return "#/alerts";
  if (route.view === "scorecard") return "#/scorecard";
  if (route.view === "cost") return "#/cost";
  if (route.view === "storage") return "#/storage";
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

/** The time, to the minute: what "Needs you" is computed at, so its
 * time-bound items (a hold's expiry) move once a minute, not each render. */
function useMinute(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 60_000);
    return () => window.clearInterval(timer);
  }, []);
  return now;
}

export default function App() {
  const [auth, setAuth] = useState<AuthGateState>({ kind: "checking" });
  const [probeSeq, setProbeSeq] = useState(0);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const dropToGate = useCallback(() => {
    live.stop();
    setAuth(authStateOnUnauthorized(getServeToken() !== null, getInitialTokenProblem()));
  }, []);

  const handleAuthenticated = useCallback(() => {
    setAuth({ kind: "authenticated", hasStoredToken: getServeToken() !== null });
  }, []);

  const handleSignOut = useCallback(() => {
    setServeToken(null);
    setAuth({
      kind: "needs_token",
      reason: "missing_token",
      problem: null,
      hasStoredToken: false,
    });
  }, []);

  const handleClearStoredAndRetry = useCallback(async (): Promise<boolean> => {
    setServeToken(null);
    const result = await probeAuth();
    if (!mounted.current) return result.kind === "needs_token";
    setAuth(authStateFromProbe(result, getServeToken() !== null, getInitialTokenProblem()));
    return result.kind === "needs_token";
  }, []);

  const retryProbe = useCallback(() => {
    setAuth({ kind: "checking" });
    setProbeSeq((seq) => seq + 1);
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    void probeAuth(controller.signal)
      .then((result) => {
        if (controller.signal.aborted || !mounted.current) return;
        setAuth(authStateFromProbe(result, getServeToken() !== null, getInitialTokenProblem()));
      })
      .catch((cause: unknown) => {
        if (isAbortError(cause) || controller.signal.aborted || !mounted.current) return;
        setAuth({
          kind: "unreachable",
          message: cause instanceof Error ? cause.message : "无法连接到服务端",
          hasStoredToken: getServeToken() !== null,
        });
      });
    return () => controller.abort();
  }, [probeSeq]);

  useEffect(() => {
    const controller = new AbortController();
    const unsubscribe = subscribeAuthSync((event) => {
      if (event.kind === "unauthorized") {
        dropToGate();
        return;
      }
      if (event.kind === "signed_out") {
        live.stop();
        setAuth({
          kind: "needs_token",
          reason: "missing_token",
          problem: null,
          hasStoredToken: false,
        });
        return;
      }
      void probeAuth(controller.signal)
        .then((result) => {
          if (controller.signal.aborted || !mounted.current) return;
          const next = authStateFromProbe(result, getServeToken() !== null, getInitialTokenProblem());
          setAuth((current) =>
            current.kind === "authenticated" && next.kind === "authenticated" ? current : next,
          );
        })
        .catch((cause: unknown) => {
          if (isAbortError(cause) || controller.signal.aborted || !mounted.current) return;
          setAuth({
            kind: "unreachable",
            message: cause instanceof Error ? cause.message : "无法连接到服务端",
            hasStoredToken: getServeToken() !== null,
          });
        });
    });
    return () => {
      controller.abort();
      unsubscribe();
    };
  }, [dropToGate]);

  if (auth.kind === "checking") {
    return (
      <div className="grid h-dvh w-full place-items-center bg-page-canvas text-caption text-faint-foreground">
        <div className="flex items-center gap-2.5">
          <Orbit size={16} className="animate-spin text-muted-foreground" />
          <span>正在连接 Spawn…</span>
        </div>
      </div>
    );
  }

  if (auth.kind === "needs_token") {
    return (
      <TokenGate
        reason={auth.reason}
        problem={auth.problem}
        hasStoredToken={auth.hasStoredToken}
        onAuthenticated={handleAuthenticated}
        onClearStoredAndRetry={handleClearStoredAndRetry}
      />
    );
  }

  if (auth.kind === "forbidden") {
    return (
      <div className="h-dvh w-full overflow-y-auto bg-page-canvas text-label text-foreground">
        <div className="flex min-h-full flex-col">
          <header className="flex shrink-0 items-center justify-end px-4 pt-3 sm:px-6 sm:pt-4">
            <ThemeSwitch />
          </header>
          <main className="flex flex-1 items-center justify-center px-4 py-4 sm:px-6">
            <section className="w-full max-w-[400px] rounded-card bg-surface p-6 shadow-card sm:p-7">
              <div className="flex items-center gap-2.5 text-foreground">
                <ShieldAlert size={18} className="shrink-0 text-destructive" />
                <h1 className="text-title-sm font-semibold">访问被服务端拒绝</h1>
              </div>
              <p className="mt-2.5 rounded-control bg-fail-tint px-3 py-2 text-caption text-destructive">
                {auth.message}
              </p>
              <p className="mt-3 text-caption leading-relaxed text-faint-foreground">
                服务端在只读探测（GET）阶段返回了 HTTP 403，通常源于 Host 或 Origin 安全策略拦截，而非只读 Token 限制。请核对浏览器访问域名或反向代理的 Host/Origin 转发配置后再重试。
              </p>
              <div className="mt-4">
                <button
                  type="button"
                  onClick={retryProbe}
                  className="inline-flex h-11 w-full items-center justify-center rounded-control bg-inset px-4 text-caption font-medium text-muted-foreground shadow-hairline transition-colors hover:bg-surface-hover hover:text-foreground"
                >
                  重新检查
                </button>
              </div>
            </section>
          </main>
        </div>
      </div>
    );
  }

  if (auth.kind === "unreachable") {
    return (
      <div className="h-dvh w-full overflow-y-auto bg-page-canvas text-label text-foreground">
        <div className="flex min-h-full flex-col">
          <header className="flex shrink-0 items-center justify-end px-4 pt-3 sm:px-6 sm:pt-4">
            <ThemeSwitch />
          </header>
          <main className="flex flex-1 items-center justify-center px-4 py-4 sm:px-6">
            <section className="w-full max-w-[400px] rounded-card bg-surface p-6 shadow-card sm:p-7">
              <div className="flex items-center gap-2.5 text-foreground">
                <WifiOff size={18} className="text-muted-foreground" />
                <h1 className="text-title-sm font-semibold">暂时无法连接 Spawn 服务端</h1>
              </div>
              <p className="mt-2.5 rounded-control bg-inset px-3 py-2 font-mono text-caption text-muted-foreground">
                {auth.message}
              </p>
              <div className="mt-4 space-y-2.5">
                <button
                  type="button"
                  onClick={retryProbe}
                  className="inline-flex h-11 w-full items-center justify-center rounded-control bg-primary px-4 text-body font-semibold text-primary-foreground shadow-btn transition-opacity hover:opacity-90"
                >
                  重试连接
                </button>
                {auth.hasStoredToken && (
                  <button
                    type="button"
                    onClick={() => void handleClearStoredAndRetry()}
                    className="inline-flex h-11 w-full items-center justify-center rounded-control bg-inset px-4 text-caption font-medium text-muted-foreground shadow-hairline hover:bg-surface-hover hover:text-foreground"
                  >
                    清除已存 Token 并重试
                  </button>
                )}
              </div>
            </section>
          </main>
        </div>
      </div>
    );
  }

  return (
    <DashboardShell
      hasStoredToken={auth.hasStoredToken}
      onUnauthorized={dropToGate}
      onSignOut={handleSignOut}
    />
  );
}

function DashboardShell({
  hasStoredToken,
  onUnauthorized,
  onSignOut,
}: {
  hasStoredToken: boolean;
  onUnauthorized: () => void;
  onSignOut: () => void;
}) {
  // The live log (stream.ts, shared among tabs): every poll that names a
  // topic reads again when an event of it arrives, and polls slower while
  // the stream is live.
  useEffect(() => {
    live.start();
    const unsubscribe = live.subscribe((signal) => {
      if (signal.kind === "status" && signal.status.state === "unauthorized") {
        onUnauthorized();
      }
    });
    return () => {
      unsubscribe();
      live.stop();
    };
  }, [onUnauthorized]);
  const overview = usePoll("overview", fetchOverview, 2_000, { live: ["overview"] });
  // Both polled in every view: the sidebar shows what is escalated, which is
  // the dispatcher's view (/api/attention), never a judge's flag.
  const fleetPoll = usePoll("fleet", fetchFleet, 3_000, { live: ["fleet"] });
  const attention = usePoll("attention", fetchAttention, 3_000, { live: ["attention"] });
  // The alerts Spawn raised, on every page: the open ones are a strip at the
  // top, and the Alerts view shows the window.
  const alerts = usePoll("alerts", fetchAlerts, 5_000, { live: ["alerts"] });
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
  // The open alerts to act on: one about a session that has ended is moot,
  // and a stuck one not scored since it fired is stale; neither is in the
  // band nor counted (home.ts; design-ui-information.md §4.6).
  const applying = useMemo(
    () => actionableAlerts(alerts.data, fleetPoll.data?.agents ?? null),
    [alerts.data, fleetPoll.data],
  );
  const openAlerts = applying.length;
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [railOpen, setRailOpen] = useState(false);
  // The fleet view is the landing page; the hash keeps the view (and the
  // open agent) across reloads.
  const [route, setRoute] = useState<Route>(routeFromHash);
  const view = route.view;
  const [tokenRole, setTokenRole] = useState<SessionRole>(() =>
    classifyTokenRole(getServeToken()),
  );
  useEffect(() => {
    setTokenRole(classifyTokenRole(getServeToken()));
  }, [hasStoredToken]);
  useEffect(() => {
    return subscribeAuthSync(() => {
      setTokenRole(classifyTokenRole(getServeToken()));
    });
  }, []);
  const selectAgentAddress = useCallback(
    (agent: string | null) =>
      setRoute((curr) =>
        curr.view === "agents" ? { view: "agents", agent } : curr,
      ),
    [],
  );
  // Polled in every view too: the sidebar and the fleet board group agents
  // by the tasks they hold. Faster while the missions page shows them.
  const missions = usePoll<Missions>(
    "missions",
    fetchMissions,
    view === "missions" ? 5_000 : 15_000,
    { live: ["missions"] },
  );
  const context = useMemo(() => fleetContext(missions.data), [missions.data]);
  // The Home badge counts what needs the owner's action: the home page's
  // "Needs you" items to act on, nothing it only reports.
  const missing = useMemo(
    () => missingAlwaysOn(fleetPoll.data?.agents ?? [], context),
    [fleetPoll.data, context],
  );
  // "Needs you" is computed here, once: the Home card lists these items and
  // the Home badge counts their actions, so the two never disagree (when
  // /api/attention fails, both go on with the missions, alerts and host;
  // at a hold's 30-minute edge, both cross it together on the minute).
  const minute = useMinute();
  // One name per agent (design §4.5), unique across the board.
  const pollNames = useMemo(() => agentNames(fleetPoll.data?.agents ?? [], context), [fleetPoll.data, context]);
  const needItems = useMemo(
    () =>
      needsYou({
        attention: attention.data,
        missions: missions.data,
        alerts: alerts.data,
        agents: fleetPoll.data?.agents ?? null,
        health: fleetPoll.data?.health ?? null,
        missingAlwaysOn: missing.length,
        now: minute,
        nameOf: (session) => pollNames.get(session) ?? null,
      }),
    [attention.data, missions.data, alerts.data, fleetPoll.data, missing, minute, pollNames],
  );
  const humanCount = {
    count: needItems.filter((item) => item.tone === "act").length,
    error: attention.data ? null : attention.error,
  };
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
  const session = usePoll<SessionTail | null>(sessionKey, (signal) =>
    view === "sessions" && selectedId
      ? fetchSessionTail(selectedId, PAGE_EVENTS, signal)
      : Promise.resolve(null),
  );

  // `session.data` lags one render behind a selection change; never show the
  // previous session under the new header.
  const detail = session.data?.session_id === selectedId ? session.data : null;
  const summary =
    overview.data?.sessions.find((entry) => entry.session_id === selectedId) ??
    null;
  const freshness = detail?.freshness ?? null;
  const error = overview.error ?? session.error;
  const agentId = detail?.agent_id || summary?.agent_id || null;
  const sessionId = detail?.session_id ?? selectedId;
  const sessionName = sessionId ? (pollNames.get(sessionId) ?? null) : null;
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
        openAlerts={openAlerts}
        onShowFleet={() => setView("fleet")}
        onShowAgents={() => setView("agents")}
        onShowMissions={() => setView("missions")}
        onShowGrading={() => setView("grading")}
        onShowFeatures={() => setView("features")}
        onShowCoverage={() => setView("coverage")}
        onShowRsi={() => setView("rsi")}
        onShowAlerts={() => setView("alerts")}
        onShowScorecard={() => setView("scorecard")}
        onShowCost={() => setView("cost")}
        onShowStorage={() => setView("storage")}
        onShowShow={() => setView("show")}
        onShowSessions={() => setView("sessions")}
        onSignOut={hasStoredToken ? onSignOut : undefined}
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
        onShowAgents={() => setView("agents")}
        onShowMissions={() => setView("missions")}
        onShowGrading={() => setView("grading")}
        onShowFeatures={() => setView("features")}
        onShowCoverage={() => setView("coverage")}
        onShowRsi={() => setView("rsi")}
        onShowAlerts={() => setView("alerts")}
        openAlerts={openAlerts}
        onShowScorecard={() => setView("scorecard")}
        onShowCost={() => setView("cost")}
        onShowStorage={() => setView("storage")}
        onShowShow={() => setView("show")}
        onShowSessions={() => setView("sessions")}
        onSelect={(id) => {
          setSelectedId(id);
          setView("sessions");
        }}
        onSignOut={hasStoredToken ? onSignOut : undefined}
      />

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      {view !== "fleet" && (
        <OpenAlerts alerts={applying} onOpen={() => setView("alerts")} />
      )}
      {view === "fleet" ? (
        <FleetView
          fleet={fleet}
          attention={attention}
          context={context}
          missions={missions.data}
          alerts={alerts.data}
          needs={needItems}
          onShowMissions={() => setView("missions")}
          selectedId={route.agent}
          onSelect={(agent) => setRoute({ view: "fleet", agent })}
          onOpenTimeline={openTimeline}
        />
      ) : view === "agents" ? (
        <AgentsConsoleView
          role={tokenRole}
          selectedAddress={route.agent}
          onSelectAddress={selectAgentAddress}
        />
      ) : view === "missions" ? (
        <MissionsView missions={missions} />
      ) : view === "grading" ? (
        <GradingView />
      ) : view === "features" ? (
        <FeatureMapView
          onOpenSession={openTimeline}
          onOpenCoverage={() => setView("coverage")}
        />
      ) : view === "coverage" ? (
        <CoveragePage />
      ) : view === "rsi" ? (
        <RsiView onOpenSession={openTimeline} />
      ) : view === "alerts" ? (
        <AlertsView
          agents={fleetPoll.data?.agents ?? null}
          nameOf={(session) => pollNames.get(session) ?? null}
          onShowHome={() => setView("fleet")}
        />
      ) : view === "scorecard" ? (
        <ScorecardView missions={missions.data} health={fleetPoll.data?.health ?? null} />
      ) : view === "cost" ? (
        <CostView />
      ) : view === "storage" ? (
        <StorageTreeView />
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
                  <h1
                    className="min-w-0 truncate text-body font-semibold text-foreground"
                    title={[sessionName, agentId, sessionId].filter(Boolean).join("\n") || undefined}
                  >
                    {sessionName ?? agentId ?? sessionId ?? "No session selected"}
                  </h1>
                  {sessionId && (sessionName || agentId) && (
                    <span className="hidden truncate rounded bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground select-all sm:inline">
                      {sessionId}
                    </span>
                  )}
                  {/* No judged state here: the header says what is
                      recorded (the execution, ExecutionSummary), and the
                      judgement, which can lag it ("Completed" beside a
                      running execution), is in the Observation card. */}
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
              <ExecutionSummary detail={detail} className="hidden md:inline-flex" />
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
                    {pollNames.get(entry.session_id) ?? (entry.agent_id || entry.session_id)}
                  </button>
                ))}
              </div>
            )}

            {!selectedId ? (
              <Placeholder text="Waiting for a session." />
            ) : detail ? (
              <Timeline
                key={selectedId}
                sessionId={detail.session_id}
                tail={detail.window}
                executions={detail.executions}
                to={inputTarget(detail.session_id, pollNames.get(detail.session_id) ?? null, detail.agent_id)}
                reassignedTo={detail.reassigned_to}
                reassignedFrom={detail.reassigned_from}
                onOpenSession={setSelectedId}
              />
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
                  withExecution
                />
              </div>
            </Drawer>
          )}
        </div>
      )}
      </div>
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

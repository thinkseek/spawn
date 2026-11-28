import { Orbit, ShieldAlert, WifiOff } from "lucide-react";
import { Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  fetchActiveAlerts,
  fetchAlerts,
  fetchAttention,
  fetchOverview,
  fetchSessionTail,
  getInitialTokenProblem,
  getServeToken,
  live,
  peekActiveAlerts,
  peekAlerts,
  peekApi,
  peekAttention,
  peekOverview,
  probeAuth,
  setServeToken,
  subscribeAuthSync,
  usePoll,
} from "./api";
import { PAGE_EVENTS } from "./sessionWindow";
import type { PollResult, SessionTail } from "./api";
import { markNeedsHuman, needsHuman } from "./attention";
import { RouteErrorBoundary, retryableLazy } from "./components/RouteErrorBoundary";
import { MobileNav, LocaleSwitch, Sidebar, ThemeSwitch } from "./components/Sidebar";
import { TokenGate } from "./components/TokenGate";
import {
  authStateFromProbe,
  authStateOnUnauthorized,
  classifyTokenRole,
  isAbortError,
  type AuthGateState,
  type SessionRole,
} from "./token";
import { FleetView } from "./components/fleet/FleetView";
import { tr, useLocale } from "./components/ui";
import type { Fleet } from "./fleet";
import { fetchFleet, peekFleet } from "./fleet";
import type { Missions } from "./missions";
import { fetchMissions, fetchMissionsSummary, normalizeMissions } from "./missions";
import type { NavView } from "./navigation";
import { fleetContext, missingAlwaysOn } from "./ordering";
import { actionableAlerts, actionableHomeNeeds, agentNames, needsYou } from "./home";
import { registerRouteLoaders, scheduleIdlePrefetch } from "./routePrefetch";

// Only Overview (FleetView) stays in the main entry chunk; every secondary,
// diagnostic, trace, and chart view splits into its own on-demand chunk.
const loadAgentsConsoleView = () => import("./components/agents/AgentsConsoleView");
const loadAlertsView = () => import("./components/alerts/AlertsView");
const loadCostView = () => import("./components/cost/CostView");
const loadCoveragePage = () => import("./components/features/CoveragePage");
const loadFeatureMapView = () => import("./components/features/FeatureMapView");
const loadGradingView = () => import("./components/grading/GradingView");
const loadMissionsView = () => import("./components/missions/MissionsView");
const loadRsiView = () => import("./components/rsi/RsiView");
const loadScorecardView = () => import("./components/scorecard/ScorecardView");
const loadSessionsView = () => import("./components/SessionsView");
const loadShowView = () => import("./components/show/ShowView");
const loadStorageTreeView = () => import("./components/storage/StorageTreeView");

registerRouteLoaders({
  agents: loadAgentsConsoleView,
  alerts: loadAlertsView,
  cost: loadCostView,
  coverage: loadCoveragePage,
  features: loadFeatureMapView,
  grading: loadGradingView,
  missions: loadMissionsView,
  rsi: loadRsiView,
  scorecard: loadScorecardView,
  sessions: loadSessionsView,
  show: loadShowView,
  storage: loadStorageTreeView,
});

const AgentsConsoleView = retryableLazy(() =>
  loadAgentsConsoleView().then((module) => ({ default: module.AgentsConsoleView })),
);
const AlertsView = retryableLazy(() =>
  loadAlertsView().then((module) => ({ default: module.AlertsView })),
);
const CostView = retryableLazy(() =>
  loadCostView().then((module) => ({ default: module.CostView })),
);
const CoveragePage = retryableLazy(() =>
  loadCoveragePage().then((module) => ({ default: module.CoveragePage })),
);
const FeatureMapView = retryableLazy(() =>
  loadFeatureMapView().then((module) => ({ default: module.FeatureMapView })),
);
const GradingView = retryableLazy(() =>
  loadGradingView().then((module) => ({ default: module.GradingView })),
);
const MissionsView = retryableLazy(() =>
  loadMissionsView().then((module) => ({ default: module.MissionsView })),
);
const RsiView = retryableLazy(() =>
  loadRsiView().then((module) => ({ default: module.RsiView })),
);
const ScorecardView = retryableLazy(() =>
  loadScorecardView().then((module) => ({ default: module.ScorecardView })),
);
const SessionsView = retryableLazy(() =>
  loadSessionsView().then((module) => ({ default: module.SessionsView })),
);
const ShowView = retryableLazy(() =>
  loadShowView().then((module) => ({ default: module.ShowView })),
);
const StorageTreeView = retryableLazy(() =>
  loadStorageTreeView().then((module) => ({ default: module.StorageTreeView })),
);

const peekMissions = () => peekApi("/api/missions", normalizeMissions);
const peekMissionsSummary = () => peekApi("/api/missions?summary=1", normalizeMissions);
const EMPTY_SESSIONS: NonNullable<ReturnType<typeof peekOverview>>["sessions"] = [];

export type View = NavView;

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
  const { locale } = useLocale();
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
          <span>{tr(locale, "正在连接 Spawn…", "Connecting to Spawn…")}</span>
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
          <header className="flex shrink-0 items-center justify-end gap-2 px-4 pt-3 sm:px-6 sm:pt-4">
            <LocaleSwitch compact />
            <ThemeSwitch />
          </header>
          <main className="flex flex-1 items-center justify-center px-4 py-4 sm:px-6">
            <section className="w-full max-w-[400px] rounded-card bg-surface p-6 shadow-card sm:p-7">
              <div className="flex items-center gap-2.5 text-foreground">
                <ShieldAlert size={18} className="shrink-0 text-destructive" />
                <h1 className="text-title-sm font-semibold">
                  {tr(locale, "访问被服务端拒绝", "Access denied by server")}
                </h1>
              </div>
              <p className="mt-2.5 rounded-control bg-fail-tint px-3 py-2 text-caption text-destructive">
                {auth.message}
              </p>
              <p className="mt-3 text-caption leading-relaxed text-faint-foreground">
                {tr(
                  locale,
                  "服务端在只读探测（GET）阶段返回了 HTTP 403，通常源于 Host 或 Origin 安全策略拦截，而非只读 Token 限制。请核对浏览器访问域名或反向代理的 Host/Origin 转发配置后再重试。",
                  "The server returned HTTP 403 on the read-only probe, typically due to Host or Origin policy checks. Verify the domain or reverse-proxy Host/Origin headers and retry.",
                )}
              </p>
              <div className="mt-4">
                <button
                  type="button"
                  onClick={retryProbe}
                  className="inline-flex h-11 w-full items-center justify-center rounded-control bg-inset px-4 text-caption font-medium text-muted-foreground shadow-hairline transition-colors hover:bg-surface-hover hover:text-foreground"
                >
                  {tr(locale, "重新检查", "Retry check")}
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
          <header className="flex shrink-0 items-center justify-end gap-2 px-4 pt-3 sm:px-6 sm:pt-4">
            <LocaleSwitch compact />
            <ThemeSwitch />
          </header>
          <main className="flex flex-1 items-center justify-center px-4 py-4 sm:px-6">
            <section className="w-full max-w-[400px] rounded-card bg-surface p-6 shadow-card sm:p-7">
              <div className="flex items-center gap-2.5 text-foreground">
                <WifiOff size={18} className="text-muted-foreground" />
                <h1 className="text-title-sm font-semibold">
                  {tr(locale, "暂时无法连接 Spawn 服务端", "Cannot reach Spawn server")}
                </h1>
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
                  {tr(locale, "重试连接", "Retry connection")}
                </button>
                {auth.hasStoredToken && (
                  <button
                    type="button"
                    onClick={() => void handleClearStoredAndRetry()}
                    className="inline-flex h-11 w-full items-center justify-center rounded-control bg-inset px-4 text-caption font-medium text-muted-foreground shadow-hairline hover:bg-surface-hover hover:text-foreground"
                  >
                    {tr(locale, "清除已存 Token 并重试", "Clear stored token and retry")}
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
  const { locale } = useLocale();
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
  useEffect(() => {
    return scheduleIdlePrefetch(routeFromHash().view);
  }, []);
  const [route, setRoute] = useState<Route>(routeFromHash);
  const view = route.view;
  const overview = usePoll(
    "overview",
    fetchOverview,
    view === "sessions" ? 3_000 : 15_000,
    {
      live: ["overview"],
      initial: peekOverview,
    },
  );
  // Both polled in every view: the sidebar shows what is escalated, which is
  // the dispatcher's view (/api/attention), never a judge's flag.
  const fleetPoll = usePoll("fleet", fetchFleet, 3_000, {
    live: ["fleet"],
    checked: true,
    initial: peekFleet,
  });
  const attention = usePoll("attention", fetchAttention, 3_000, {
    live: ["attention"],
    initial: peekAttention,
  });
  // The alerts Spawn raised; poll active alerts outside `/alerts` to trim payload.
  const alertsKey = view === "alerts" ? "alerts" : "alerts:active";
  const fetchAlertsForView = view === "alerts" ? fetchAlerts : fetchActiveAlerts;
  const alertsInitial = useCallback(
    () =>
      view === "alerts"
        ? (peekAlerts() ?? peekActiveAlerts())
        : (peekActiveAlerts() ?? peekAlerts()),
    [view],
  );
  const alerts = usePoll(alertsKey, fetchAlertsForView, 5_000, {
    live: ["alerts"],
    initial: alertsInitial,
  });
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
  const fleet = useMemo<PollResult<Fleet>>(
    () => ({
      data: fleetData,
      error: fleetPoll.error,
      checkedAt: fleetPoll.checkedAt,
    }),
    [fleetData, fleetPoll.error, fleetPoll.checkedAt],
  );
  const applying = useMemo(
    () => actionableAlerts(alerts.data, fleetPoll.data?.agents ?? null),
    [alerts.data, fleetPoll.data],
  );
  const openAlerts = applying.length;
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [railOpen, setRailOpen] = useState(false);
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
  // Polled in every view too: summary board outside `/missions` avoids multi-hundred-KB polling.
  const missionsKey = view === "missions" ? "missions" : "missions:summary";
  const fetchMissionsForView = view === "missions" ? fetchMissions : fetchMissionsSummary;
  const missionsInitial = useCallback(
    () =>
      view === "missions"
        ? (peekMissions() ?? peekMissionsSummary())
        : (peekMissionsSummary() ?? peekMissions()),
    [view],
  );
  const missions = usePoll<Missions>(
    missionsKey,
    fetchMissionsForView,
    view === "missions" ? 5_000 : 15_000,
    { live: ["missions"], checked: true, initial: missionsInitial },
  );
  const context = useMemo(() => fleetContext(missions.data), [missions.data]);
  // The Home badge counts what needs the owner's action: the home page's
  // "Needs you" items to act on, nothing it only reports.
  const missing = useMemo(
    () => missingAlwaysOn(fleetPoll.data?.agents ?? [], context),
    [fleetPoll.data, context],
  );
  // "Needs you" is computed here, once: the Home card lists these items and
  // the Home badge counts their actions, so the two never disagree.
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
        locale,
        nameOf: (session) => pollNames.get(session) ?? null,
      }),
    [attention.data, missions.data, alerts.data, fleetPoll.data, missing, minute, locale, pollNames],
  );
  const homeNeeds = useMemo(() => actionableHomeNeeds(needItems), [needItems]);
  const humanCount = useMemo(
    () => ({
      count: homeNeeds.filter((item) => item.tone === "act").length,
      error: attention.data ? null : attention.error,
    }),
    [homeNeeds, attention.data, attention.error],
  );
  const setView = useCallback((next: View) => setRoute({ view: next, agent: null }), []);
  const showHome = useCallback(() => setView("fleet"), [setView]);
  const showAgents = useCallback(() => setView("agents"), [setView]);
  const showMissions = useCallback(() => setView("missions"), [setView]);
  const showGrading = useCallback(() => setView("grading"), [setView]);
  const showFeatures = useCallback(() => setView("features"), [setView]);
  const showCoverage = useCallback(() => setView("coverage"), [setView]);
  const showRsi = useCallback(() => setView("rsi"), [setView]);
  const showAlerts = useCallback(() => setView("alerts"), [setView]);
  const showScorecard = useCallback(() => setView("scorecard"), [setView]);
  const showCost = useCallback(() => setView("cost"), [setView]);
  const showStorage = useCallback(() => setView("storage"), [setView]);
  const showShow = useCallback(() => setView("show"), [setView]);
  const showSessions = useCallback(() => setView("sessions"), [setView]);
  const selectSessionFromSidebar = useCallback(
    (id: string) => {
      setSelectedId(id);
      setView("sessions");
    },
    [setView],
  );
  const selectFleetAgent = useCallback((agent: string | null) => setRoute({ view: "fleet", agent }), []);

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
        <RouteErrorBoundary resetKey="show">
          <Suspense fallback={null}>
            <ShowView fleet={fleet} present />
          </Suspense>
        </RouteErrorBoundary>
      </div>
    );
  }

  return (
    <div className="flex h-dvh w-full flex-col overflow-hidden bg-app-shell text-label text-foreground md:flex-row">
      <MobileNav
        view={view}
        rsi={rsi}
        error={overview.error}
        attention={humanCount}
        openAlerts={openAlerts}
        onShowFleet={showHome}
        onShowAgents={showAgents}
        onShowMissions={showMissions}
        onShowGrading={showGrading}
        onShowFeatures={showFeatures}
        onShowCoverage={showCoverage}
        onShowRsi={showRsi}
        onShowAlerts={showAlerts}
        onShowScorecard={showScorecard}
        onShowCost={showCost}
        onShowStorage={showStorage}
        onShowShow={showShow}
        onShowSessions={showSessions}
        onSignOut={hasStoredToken ? onSignOut : undefined}
      />
      <Sidebar
        sessions={overview.data?.sessions ?? EMPTY_SESSIONS}
        agents={fleet.data?.agents ?? null}
        context={context}
        selectedId={view === "sessions" ? selectedId : null}
        view={view}
        rsi={rsi}
        error={overview.error}
        attention={humanCount}
        onShowFleet={showHome}
        onShowAgents={showAgents}
        onShowMissions={showMissions}
        onShowGrading={showGrading}
        onShowFeatures={showFeatures}
        onShowCoverage={showCoverage}
        onShowRsi={showRsi}
        onShowAlerts={showAlerts}
        openAlerts={openAlerts}
        onShowScorecard={showScorecard}
        onShowCost={showCost}
        onShowStorage={showStorage}
        onShowShow={showShow}
        onShowSessions={showSessions}
        onSelect={selectSessionFromSidebar}
        onSignOut={hasStoredToken ? onSignOut : undefined}
      />

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      {view === "fleet" ? (
        <FleetView
          fleet={fleet}
          attention={attention}
          context={context}
          missions={missions.data}
          alerts={alerts.data}
          needs={needItems}
          actionableNeeds={homeNeeds}
          onShowMissions={showMissions}
          onShowFeatures={showFeatures}
          onShowAgents={showAgents}
          selectedId={route.agent}
          onSelect={selectFleetAgent}
          onOpenTimeline={openTimeline}
        />
      ) : (
        <RouteErrorBoundary resetKey={`${view}:${route.agent ?? ""}`}>
          <Suspense fallback={<ViewLoadingFallback label={tr(locale, "正在加载视图…", "Loading view…")} />}>
          {view === "agents" ? (
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
              onOpenCoverage={showCoverage}
            />
          ) : view === "coverage" ? (
            <CoveragePage />
          ) : view === "rsi" ? (
            <RsiView onOpenSession={openTimeline} />
          ) : view === "alerts" ? (
            <AlertsView
              agents={fleetPoll.data?.agents ?? null}
              nameOf={(session) => pollNames.get(session) ?? null}
              onShowHome={showHome}
            />
          ) : view === "scorecard" ? (
            <ScorecardView missions={missions.data} health={fleetPoll.data?.health ?? null} />
          ) : view === "cost" ? (
            <CostView />
          ) : view === "storage" ? (
            <StorageTreeView />
          ) : view === "show" ? (
            <ShowView fleet={fleet} present={false} />
          ) : (
            <SessionsView
              sessions={overview.data?.sessions ?? []}
              detail={detail}
              sessionError={session.error}
              freshness={freshness}
              error={error}
              agentId={agentId}
              sessionId={sessionId}
              sessionName={sessionName}
              rsi={rsi}
              observation={observation}
              selectedId={selectedId}
              pollNames={pollNames}
              railOpen={railOpen}
              onSelectSession={setSelectedId}
              onSetRailOpen={setRailOpen}
            />
          )}
          </Suspense>
        </RouteErrorBoundary>
      )}
      </div>
    </div>
  );
}

function ViewLoadingFallback({ label }: { label: string }) {
  return (
    <div className="grid min-h-0 flex-1 place-items-center bg-page-canvas px-6">
      <div className="flex items-center gap-2 text-caption text-faint-foreground">
        <Orbit size={15} className="animate-spin text-muted-foreground" />
        <span>{label}</span>
      </div>
    </div>
  );
}

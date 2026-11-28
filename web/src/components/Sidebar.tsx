import {
  Activity,
  BellRing,
  Boxes,
  ChevronDown,
  ChevronRight,
  CircleDollarSign,
  FolderSearch,
  Gauge,
  GitBranch,
  House,
  Layers,
  LogOut,
  Menu,
  Monitor,
  Moon,
  Orbit,
  PanelLeftClose,
  PanelLeftOpen,
  Rows3,
  Search,
  SquareTerminal,
  Sun,
  Target,
  Waypoints,
  X,
} from "lucide-react";
import { memo, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";

import type { OverviewSession, RsiSummary, SessionState } from "../api";
import type { View } from "../App";
import type { FleetAgent } from "../fleet";
import { agentMatches, agentNames, agentSearchFields, nameTask } from "../home";
import { LOCALE_CHOICES, navLabel, tr, type Locale } from "../i18n";
import { focusOf, slashFocusesSearch } from "../keys";
import { rsiPhase, sessionTaskLabel } from "../missions";
import {
  DIAGNOSTIC_VIEWS,
  PRIMARY_VIEWS,
  SECONDARY_VIEWS,
  shouldExpandDiagnostics,
  type NavView,
} from "../navigation";
import type { FleetContext } from "../ordering";
import { agentGroupLabel, groupAgents, heldTask, stateWords } from "../ordering";
import { recordedState } from "../recordedState";
import { prefetchView } from "../routePrefetch";
import type { ThemeChoice } from "../theme";
import { THEME_CHOICES, THEME_KEY, useTheme } from "../theme";
import { StateDot, stateLabel } from "./StateDot";
import { useStoredChoice } from "./ViewToggle";
import { phaseStyle } from "./rsi/Phase";
import { Ago, FOCUS_RING_INSET, GroupHeader, MiddleId, cx, useLocale } from "./ui";

export interface HumanCount {
  count: number | null;
  /** Why /api/attention could not be read, when it could not. */
  error: string | null;
}

interface SidebarProps {
  sessions: OverviewSession[];
  /** The fleet's agents, grouped as the Fleet board groups them; null until
   * /api/fleet answers, when the overview's sessions show as a flat list. */
  agents: FleetAgent[] | null;
  context: FleetContext;
  selectedId: string | null;
  view: View;
  rsi: RsiSummary | null;
  error: string | null;
  /** What requires human decision on Home (never raw alert noise). */
  attention: HumanCount;
  /** Kept for caller compatibility; navigation avoids noisy alert badges. */
  openAlerts?: number;
  onShowFleet: () => void;
  onShowAgents: () => void;
  onShowMissions: () => void;
  onShowGrading: () => void;
  onShowFeatures: () => void;
  onShowCoverage: () => void;
  onShowRsi: () => void;
  onShowAlerts: () => void;
  onShowScorecard: () => void;
  onShowCost: () => void;
  onShowStorage: () => void;
  onShowShow: () => void;
  onShowSessions: () => void;
  onSelect: (sessionId: string) => void;
  onSignOut?: () => void;
}

function Brand({ compact = false }: { compact?: boolean }) {
  return (
    <div className="flex items-center gap-2.5">
      <span className="grid size-7 shrink-0 place-items-center rounded-md border border-border-strong bg-surface text-foreground">
        <Orbit size={15} strokeWidth={2} />
      </span>
      {!compact && (
        <span className="text-body font-semibold tracking-tight text-foreground">
          Spawn
        </span>
      )}
    </div>
  );
}

type ViewHandlers = Record<NavView, () => void>;

function viewHandlersOf(props: Pick<
  SidebarProps,
  | "onShowFleet"
  | "onShowAgents"
  | "onShowMissions"
  | "onShowGrading"
  | "onShowFeatures"
  | "onShowCoverage"
  | "onShowRsi"
  | "onShowAlerts"
  | "onShowScorecard"
  | "onShowCost"
  | "onShowStorage"
  | "onShowShow"
  | "onShowSessions"
>): ViewHandlers {
  return {
    fleet: props.onShowFleet,
    agents: props.onShowAgents,
    missions: props.onShowMissions,
    grading: props.onShowGrading,
    features: props.onShowFeatures,
    coverage: props.onShowCoverage,
    rsi: props.onShowRsi,
    alerts: props.onShowAlerts,
    scorecard: props.onShowScorecard,
    cost: props.onShowCost,
    storage: props.onShowStorage,
    show: props.onShowShow,
    sessions: props.onShowSessions,
  };
}

function navIcon(id: NavView): ReactNode {
  switch (id) {
    case "fleet":
      return <House size={15} />;
    case "missions":
      return <Target size={15} />;
    case "agents":
      return <SquareTerminal size={15} />;
    case "features":
      return <Boxes size={15} />;
    case "cost":
      return <CircleDollarSign size={15} />;
    case "grading":
      return <Layers size={15} />;
    case "scorecard":
      return <Gauge size={15} />;
    case "coverage":
      return <FolderSearch size={15} />;
    case "rsi":
      return <Activity size={15} />;
    case "alerts":
      return <BellRing size={15} />;
    case "storage":
      return <GitBranch size={15} />;
    case "show":
      return <Waypoints size={15} />;
    case "sessions":
      return <Rows3 size={15} />;
  }
}

function NavSectionLabel({ label }: { label: string }) {
  return (
    <div className="px-2.5 pt-3 pb-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
      {label}
    </div>
  );
}

function SideTabList({
  view,
  locale,
  compact = false,
  attention,
  rsi,
  handlers,
  diagnosticsOpen,
  onToggleDiagnostics,
  onNavigate,
}: {
  view: View;
  locale: Locale;
  compact?: boolean;
  attention: HumanCount;
  rsi: RsiSummary | null;
  handlers: ViewHandlers;
  diagnosticsOpen: boolean;
  onToggleDiagnostics: () => void;
  onNavigate?: () => void;
}) {
  const phase = rsiPhase(rsi);
  const activate = (id: NavView) => {
    handlers[id]();
    onNavigate?.();
  };

  const renderTrailing = (id: NavView): ReactNode => {
    if (compact) return undefined;
    if (id === "fleet") return <AttentionCount {...attention} />;
    if (id === "rsi" && phase) {
      return (
        <span className="flex items-center gap-1.5 text-micro text-faint-foreground">
          <span
            className={cx(
              "size-1.5 rounded-full",
              phase === "halted" ? "bg-destructive" : phaseStyle(phase).dot,
              phaseStyle(phase).pulse && "animate-spawn-pulse",
            )}
          />
          {stateWords(phase)}
        </span>
      );
    }
    return undefined;
  };

  const compactLabelFor = (id: NavView): string => {
    const base = navLabel(id, locale);
    if (id !== "fleet") return base;
    if (attention.count !== null && attention.count > 0) {
      return tr(locale, `${base}（${attention.count} 项待处理）`, `${base} (${attention.count} pending)`);
    }
    if (attention.count === null && attention.error) {
      return `${base} (/api/attention: ${attention.error})`;
    }
    return base;
  };

  return (
    <nav aria-label={tr(locale, "主导航", "Sections")} className="px-2 pt-2 pb-2">
      {!compact && <NavSectionLabel label={tr(locale, "核心", "Core")} />}
      <div className="space-y-px">
        {PRIMARY_VIEWS.map((id) => (
          <NavItem
            key={id}
            active={view === id}
            onClick={() => activate(id)}
            onPrefetch={() => void prefetchView(id)}
            icon={navIcon(id)}
            label={compact ? compactLabelFor(id) : navLabel(id, locale)}
            compact={compact}
            trailing={renderTrailing(id)}
            compactBadge={
              compact && id === "fleet" ? <AttentionCount {...attention} badgeOnly /> : undefined
            }
          />
        ))}
      </div>

      {!compact && <NavSectionLabel label={tr(locale, "费用与分析", "Spend")} />}
      {compact && <div aria-hidden="true" className="my-2 border-t border-sidebar-border" />}
      <div className="space-y-px">
        {SECONDARY_VIEWS.map((id) => (
          <NavItem
            key={id}
            active={view === id}
            onClick={() => activate(id)}
            onPrefetch={() => void prefetchView(id)}
            icon={navIcon(id)}
            label={navLabel(id, locale)}
            compact={compact}
          />
        ))}
      </div>

      <div className="mt-2 pt-2 border-t border-sidebar-border/80">
        {compact ? (
          <button
            type="button"
            onClick={onToggleDiagnostics}
            aria-expanded={diagnosticsOpen}
            title={tr(locale, "展开/收起诊断视图", "Toggle diagnostic views")}
            className={cx(
              "flex h-9 w-full items-center justify-center rounded-control text-faint-foreground hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING_INSET,
            )}
          >
            {diagnosticsOpen ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          </button>
        ) : (
          <button
            type="button"
            onClick={onToggleDiagnostics}
            aria-expanded={diagnosticsOpen}
            className={cx(
              "flex w-full items-center gap-1.5 rounded-control px-2.5 py-1.5 text-left text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING_INSET,
            )}
          >
            {diagnosticsOpen ? (
              <ChevronDown size={12} className="shrink-0" aria-hidden="true" />
            ) : (
              <ChevronRight size={12} className="shrink-0" aria-hidden="true" />
            )}
            <span className="truncate">{tr(locale, "更多与诊断", "Diagnostics")}</span>
            <span className="tnum ml-auto font-normal tracking-normal">
              {DIAGNOSTIC_VIEWS.length}
            </span>
          </button>
        )}

        {diagnosticsOpen && (
          <div className={cx("space-y-px", compact ? "mt-1" : "mt-1 pl-0.5")}>
            {DIAGNOSTIC_VIEWS.map((id) => (
              <NavItem
                key={id}
                active={view === id}
                onClick={() => activate(id)}
                onPrefetch={() => void prefetchView(id)}
                icon={navIcon(id)}
                label={navLabel(id, locale)}
                compact={compact}
                trailing={renderTrailing(id)}
              />
            ))}
          </div>
        )}
      </div>
    </nav>
  );
}

function useDiagnosticsToggle(view: View) {
  const [diagnosticsStored, setDiagnosticsStored] = useStoredChoice(
    "spawn.sidebar.diagnostics",
    ["open", "closed"] as const,
    "closed",
  );
  const [override, setOverride] = useState<{ view: View; state: "open" | "closed" } | null>(null);
  const manualOverride = override?.view === view ? override.state : null;
  const diagnosticsOpen = shouldExpandDiagnostics(diagnosticsStored, view, manualOverride);
  const toggleDiagnostics = () => {
    const next = diagnosticsOpen ? "closed" : "open";
    setDiagnosticsStored(next);
    setOverride({ view, state: next });
  };
  return { diagnosticsOpen, toggleDiagnostics };
}

export const Sidebar = memo(function Sidebar(props: SidebarProps) {
  const {
    sessions,
    agents,
    context,
    selectedId,
    view,
    rsi,
    error,
    attention,
    onSelect,
    onSignOut,
  } = props;
  const { locale } = useLocale();
  const [railMode, setRailMode] = useStoredChoice(
    "spawn.sidebar.collapsed",
    ["expanded", "collapsed"] as const,
    "expanded",
  );
  const compact = railMode === "collapsed";
  const { diagnosticsOpen, toggleDiagnostics } = useDiagnosticsToggle(view);
  const [sessionsStored, setSessionsStored] = useStoredChoice(
    "spawn.sidebar.sessions",
    ["open", "closed"] as const,
    "closed",
  );
  const sessionsOpen = sessionsStored === "open" || view === "sessions";

  const [query, setQuery] = useState("");
  const search = useRef<HTMLInputElement>(null);
  const handlers = viewHandlersOf(props);
  const sessionCount = (agents?.length ?? sessions.length) || 0;

  // "/" focuses the search (design §4.5), unless the reader is typing or the
  // terminal panel is armed: then the key is theirs, or the agent's (P11).
  useEffect(() => {
    if (!agents || compact || !sessionsOpen) return;
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.defaultPrevented) return;
      const press = {
        key: event.key,
        ctrlKey: event.ctrlKey,
        altKey: event.altKey,
        metaKey: event.metaKey,
        ...focusOf(document.activeElement),
      };
      if (!slashFocusesSearch(press) || !search.current || search.current.offsetParent === null) return;
      event.preventDefault();
      search.current.focus();
      search.current.select();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [agents, compact, sessionsOpen]);

  return (
    <aside
      className={cx(
        "hidden shrink-0 flex-col border-r border-sidebar-border bg-sidebar transition-[width] duration-150 md:flex",
        compact ? "w-14" : "w-56 lg:w-60",
      )}
    >
      <div
        className={cx(
          "flex h-12 shrink-0 items-center border-b border-sidebar-border/60",
          compact ? "justify-center px-2" : "justify-between px-3.5",
        )}
      >
        <Brand compact={compact} />
        <button
          type="button"
          onClick={() => setRailMode(compact ? "expanded" : "collapsed")}
          title={compact ? tr(locale, "展开侧栏", "Expand sidebar") : tr(locale, "收起侧栏", "Collapse sidebar")}
          aria-label={compact ? tr(locale, "展开侧栏", "Expand sidebar") : tr(locale, "收起侧栏", "Collapse sidebar")}
          className={cx(
            "grid size-7 shrink-0 place-items-center rounded-control text-faint-foreground hover:bg-surface-hover hover:text-foreground",
            FOCUS_RING_INSET,
          )}
        >
          {compact ? <PanelLeftOpen size={14} /> : <PanelLeftClose size={14} />}
        </button>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain">
        <SideTabList
          view={view}
          locale={locale}
          compact={compact}
          attention={attention}
          rsi={rsi}
          handlers={handlers}
          diagnosticsOpen={diagnosticsOpen}
          onToggleDiagnostics={toggleDiagnostics}
        />

        <div className="mx-2 border-t border-sidebar-border/80 pt-2">
          {compact ? (
            <button
              type="button"
              onMouseEnter={() => void prefetchView("sessions")}
              onFocus={() => void prefetchView("sessions")}
              onClick={() => {
                setRailMode("expanded");
                setSessionsStored("open");
              }}
              title={tr(locale, `展开会话列表 (${sessionCount})`, `Open sessions (${sessionCount})`)}
              aria-label={tr(locale, "展开会话列表", "Open sessions")}
              className={cx(
                "flex h-9 w-full items-center justify-center rounded-control text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                view === "sessions" && "bg-surface-selected text-foreground",
                FOCUS_RING_INSET,
              )}
            >
              <Rows3 size={15} />
            </button>
          ) : (
            <button
              type="button"
              onMouseEnter={() => void prefetchView("sessions")}
              onFocus={() => void prefetchView("sessions")}
              onClick={() => setSessionsStored(sessionsOpen ? "closed" : "open")}
              aria-expanded={sessionsOpen}
              className={cx(
                "flex w-full items-center gap-1.5 rounded-control px-2.5 py-1.5 text-left text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground hover:bg-surface-hover hover:text-foreground",
                FOCUS_RING_INSET,
              )}
            >
              {sessionsOpen ? (
                <ChevronDown size={12} className="shrink-0" aria-hidden="true" />
              ) : (
                <ChevronRight size={12} className="shrink-0" aria-hidden="true" />
              )}
              <span>{tr(locale, "会话记录", "Sessions")}</span>
              {sessionCount > 0 && (
                <span className="tnum ml-auto font-normal tracking-normal">
                  {sessionCount}
                </span>
              )}
            </button>
          )}
        </div>

        {!compact && sessionsOpen && (
          <div className="pt-1.5">
            {agents && agents.length > 0 && (
              <div className="relative px-2 pb-2">
                <Search
                  size={13}
                  aria-hidden="true"
                  className="pointer-events-none absolute top-1/2 left-4.5 -translate-y-1/2 text-faint-foreground"
                />
                <input
                  ref={search}
                  type="search"
                  value={query}
                  onChange={(event) => setQuery(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key !== "Escape") return;
                    if (query) {
                      event.stopPropagation();
                      event.preventDefault();
                      setQuery("");
                    } else {
                      event.currentTarget.blur();
                    }
                  }}
                  placeholder={tr(locale, "搜索 Agent、任务或 ID", "Find agent, task or id")}
                  aria-label={tr(locale, "搜索会话", "Find a session")}
                  aria-keyshortcuts="/"
                  className={cx(
                    "h-8 w-full rounded-control border border-sidebar-border bg-surface pr-7 pl-7.5 text-label text-foreground placeholder:text-faint-foreground",
                    "[&::-webkit-search-cancel-button]:hidden",
                    FOCUS_RING_INSET,
                  )}
                />
                {query ? (
                  <button
                    type="button"
                    onClick={() => {
                      setQuery("");
                      search.current?.focus();
                    }}
                    aria-label={tr(locale, "清除搜索", "Clear search")}
                    className="absolute top-1/2 right-3.5 grid size-5 -translate-y-1/2 place-items-center rounded text-faint-foreground hover:text-foreground"
                  >
                    <X size={12} aria-hidden="true" />
                  </button>
                ) : (
                  <kbd
                    aria-hidden="true"
                    className="pointer-events-none absolute top-1/2 right-3.5 -translate-y-1/2 rounded border border-sidebar-border px-1 font-mono text-micro text-faint-foreground"
                  >
                    /
                  </kbd>
                )}
              </div>
            )}

            <nav aria-label={tr(locale, "会话列表", "Sessions")} className="px-2 pb-3">
              {agents ? (
                <GroupedSessions
                  agents={agents}
                  context={context}
                  sessions={sessions}
                  selectedId={selectedId}
                  query={query}
                  onSelect={onSelect}
                />
              ) : sessions.length === 0 ? (
                <p className="px-2 py-4 text-caption text-faint-foreground">
                  {tr(locale, "暂无会话。", "No sessions yet.")}
                </p>
              ) : (
                <ul className="space-y-px">
                  {sessions.map((session) => (
                    <li key={session.session_id}>
                      <SessionRow
                        sessionId={session.session_id}
                        agentId={session.agent_id}
                        state={session.state}
                        label={sessionTaskLabel(session.session_id, rsi) ?? stateLabel(session.state)}
                        at={session.recorded_at_ms}
                        attention={false}
                        selected={session.session_id === selectedId}
                        onSelect={onSelect}
                      />
                    </li>
                  ))}
                </ul>
              )}
            </nav>
          </div>
        )}
      </div>

      <div
        className={cx(
          "shrink-0 border-t border-sidebar-border px-2.5 py-2 text-micro text-faint-foreground",
          compact ? "flex flex-col items-center gap-1.5" : "space-y-1.5 px-3",
        )}
      >
        {compact ? (
          <>
            <span
              className={cx(
                "size-2 shrink-0 rounded-full",
                error ? "bg-destructive" : "bg-success",
              )}
              title={`${window.location.host} · ${error ? tr(locale, "离线", "offline") : tr(locale, "在线", "online")}`}
            />
            <LocaleSwitch compact />
            <ThemeSwitch compact />
            {onSignOut && <SignOutButton onSignOut={onSignOut} compact iconOnly />}
          </>
        ) : (
          <>
            <div className="flex items-center gap-1.5">
              <span
                className={cx(
                  "size-1.5 shrink-0 rounded-full",
                  error ? "bg-destructive" : "bg-success",
                )}
              />
              <span className="min-w-0 flex-1 truncate font-mono">{window.location.host}</span>
              <span className="shrink-0">
                {error ? tr(locale, "离线", "offline") : tr(locale, "在线", "online")}
              </span>
            </div>
            <div className="flex items-center gap-1.5">
              <LocaleSwitch />
              <div className="ml-auto flex items-center gap-1.5">
                {onSignOut && <SignOutButton onSignOut={onSignOut} compact />}
                <ThemeSwitch />
              </div>
            </div>
          </>
        )}
      </div>
    </aside>
  );
});

/** States whose words a held task's says better: the worker idles at its
 * prompt while its task is judged or lands. */
const AT_THE_GATE = new Set(["gating", "landing", "in review"]);

const GATE_STATE_ZH: Record<string, string> = {
  gating: "门禁校验中",
  landing: "正在合入",
  "in review": "评审中",
};

const RECORDED_STATE_ZH: Record<string, string> = {
  Running: "运行中",
  Waiting: "等待中",
  Ended: "已结束",
  "No execution": "无执行记录",
};

function GroupedSessions({
  agents,
  context,
  sessions,
  selectedId,
  query,
  onSelect,
}: {
  agents: FleetAgent[];
  context: FleetContext;
  sessions: OverviewSession[];
  selectedId: string | null;
  /** The search: only matching agents show, in every group, Ended open. */
  query: string;
  onSelect: (sessionId: string) => void;
}) {
  const { locale } = useLocale();
  const [ended, setEnded] = useStoredChoice("spawn.sidebar.ended", ["open", "closed"] as const, "closed");
  const [endedLimit, setEndedLimit] = useState(40);
  const names = useMemo(() => agentNames(agents, context), [agents, context]);
  if (agents.length === 0) {
    return (
      <p className="px-2 py-4 text-caption text-faint-foreground">
        {tr(locale, "暂无会话。", "No sessions yet.")}
      </p>
    );
  }
  const observed = new Map(sessions.map((session) => [session.session_id, session]));
  const searching = query.trim() !== "";
  const nameOf = (agent: FleetAgent) => names.get(agent.session_id) ?? (agent.agent_id || agent.session_id);
  const groups = groupAgents(agents, context).map((group) => ({
    ...group,
    agents: searching
      ? group.agents.filter((agent) => agentMatches(query, agentSearchFields(agent, context, nameOf(agent))))
      : group.agents,
  }));
  if (searching && groups.every((group) => group.agents.length === 0)) {
    return (
      <p className="px-2 py-4 text-caption text-faint-foreground">
        {tr(locale, `未找到匹配“${query.trim()}”的会话。`, `No session matches “${query.trim()}”.`)}
      </p>
    );
  }
  return (
    <div className="space-y-3">
      {groups.map((group) => {
        if (group.agents.length === 0) return null;
        const folds = group.id === "ended" && !searching;
        const open = !folds || ended === "open";
        const groupTitle = agentGroupLabel(group.id, locale);
        return (
          <section key={group.id} aria-label={groupTitle}>
            <GroupHeader
              className="px-2 pb-1"
              label={groupTitle}
              count={group.agents.length}
              expanded={folds ? open : undefined}
              onToggle={folds ? () => setEnded(open ? "closed" : "open") : undefined}
            />
            {(open || group.agents.some((agent) => agent.session_id === selectedId)) && (
              (() => {
                const visibleAgents = group.agents.filter(
                  (agent) => open || agent.session_id === selectedId,
                );
                const paginate = folds && open && !searching && visibleAgents.length > endedLimit;
                const displayedAgents = paginate
                  ? visibleAgents.filter(
                      (agent, index) => index < endedLimit || agent.session_id === selectedId,
                    )
                  : visibleAgents;
                return (
                  <>
                    <ul className="space-y-px">
                      {displayedAgents.map((agent) => {
                        const held = heldTask(agent, context);
                        const named = nameTask(agent, context);
                        const recorded = recordedState(agent);
                        const label =
                          held && AT_THE_GATE.has(held.label)
                            ? tr(locale, `任务 ${GATE_STATE_ZH[held.label] ?? held.label}`, `Task ${held.label}`)
                            : locale === "zh"
                              ? (RECORDED_STATE_ZH[recorded.label] ?? recorded.label)
                              : recorded.label;
                        const at =
                          recorded.since ?? observed.get(agent.session_id)?.recorded_at_ms ?? null;
                        return (
                          <li key={agent.session_id} className="cv-row">
                            <SessionRow
                              sessionId={agent.session_id}
                              agentId={agent.agent_id}
                              name={nameOf(agent)}
                              line={
                                named && named !== held
                                  ? named.title
                                    ? tr(locale, `上次：${named.title}`, `last: ${named.title}`)
                                    : null
                                  : (named?.title ?? agent.task_title)
                              }
                              state={recorded.dot}
                              label={label}
                              at={at}
                              attention={agent.attention}
                              task={
                                held
                                  ? `T${held.task_id ?? "?"} ${
                                      locale === "zh" ? (GATE_STATE_ZH[held.label] ?? held.label) : held.label
                                    }${held.title ? ` · ${held.title}` : ""}`
                                  : null
                              }
                              selected={agent.session_id === selectedId}
                              onSelect={onSelect}
                            />
                          </li>
                        );
                      })}
                    </ul>
                    {paginate && (
                      <div className="px-2 pt-1">
                        <button
                          type="button"
                          onClick={() => setEndedLimit((curr) => curr + 40)}
                          className={cx(
                            "w-full rounded-control px-2 py-1 text-left text-micro font-medium text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                            FOCUS_RING_INSET,
                          )}
                        >
                          {tr(
                            locale,
                            `显示更多会话（剩余 ${visibleAgents.length - displayedAgents.length} 个）`,
                            `Show more (${visibleAgents.length - displayedAgents.length} more)`,
                          )}
                        </button>
                      </div>
                    )}
                  </>
                );
              })()
            )}
          </section>
        );
      })}
    </div>
  );
}

/** A session in the list: its agent, then one line with the state dot, the
 * state and how long ago, then the session id. */
function SessionRow({
  sessionId,
  agentId,
  name = null,
  line = null,
  state,
  label,
  at,
  attention,
  task = null,
  selected,
  onSelect,
}: {
  sessionId: string;
  agentId: string;
  name?: string | null;
  line?: string | null;
  state: SessionState;
  label: string;
  at: number | null;
  attention: boolean;
  task?: string | null;
  selected: boolean;
  onSelect: (sessionId: string) => void;
}) {
  const { locale } = useLocale();
  return (
    <button
      type="button"
      onClick={() => onSelect(sessionId)}
      aria-current={selected ? "true" : undefined}
      title={[name, agentId || sessionId, sessionId, task ?? line].filter(Boolean).join("\n")}
      className={cx(
        "relative block w-full min-w-0 rounded-control px-2.5 py-1.5 text-left transition-colors",
        FOCUS_RING_INSET,
        selected ? "bg-surface-selected" : "hover:bg-surface-hover",
      )}
    >
      {attention && (
        <span
          aria-hidden="true"
          className="absolute inset-y-1.5 left-0 w-0.5 rounded-full bg-warning"
        />
      )}
      <span className="flex min-w-0 items-center gap-1.5">
        {name ? (
          <span className="min-w-0 truncate text-label font-medium text-foreground">{name}</span>
        ) : (
          <MiddleId
            id={agentId || "unknown agent"}
            title={false}
            className="text-label font-medium text-foreground"
          />
        )}
        {attention && (
          <span
            className="ml-auto flex shrink-0 text-warning"
            title={tr(locale, "已上报关注", "Escalated")}
          >
            <BellRing size={11} aria-hidden="true" />
            <span className="sr-only">{tr(locale, "已上报关注", "escalated")}</span>
          </span>
        )}
      </span>
      <span className="mt-0.5 flex min-w-0 items-center gap-1.5 overflow-hidden whitespace-nowrap text-micro text-muted-foreground">
        <StateDot state={state} size="sm" />
        <span className="min-w-0 truncate">{label}</span>
        <span aria-hidden="true" className="shrink-0 text-faint-foreground">·</span>
        <Ago ms={at} className="shrink-0" />
      </span>
      {name ? (
        line ? <span className="mt-px block truncate text-micro text-faint-foreground">{line}</span> : null
      ) : (
        <MiddleId
          id={sessionId}
          title={false}
          className="mt-px font-mono text-micro text-faint-foreground"
        />
      )}
    </button>
  );
}

function NavItem({
  active,
  onClick,
  onPrefetch,
  icon,
  label,
  compact = false,
  trailing,
  compactBadge,
}: {
  active: boolean;
  onClick: () => void;
  onPrefetch?: () => void;
  icon: ReactNode;
  label: string;
  compact?: boolean;
  trailing?: ReactNode;
  compactBadge?: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      onMouseEnter={onPrefetch}
      onFocus={onPrefetch}
      aria-current={active ? "page" : undefined}
      title={compact ? label : undefined}
      aria-label={compact ? label : undefined}
      className={cx(
        "relative flex h-8 w-full items-center rounded-control text-left text-label transition-colors pointer-coarse:h-11",
        compact ? "justify-center px-0" : "gap-2.5 px-2.5",
        FOCUS_RING_INSET,
        active
          ? "bg-surface-selected font-semibold text-foreground"
          : "font-medium text-muted-foreground hover:bg-surface-hover hover:text-foreground",
      )}
    >
      <span className={active ? "text-foreground" : "text-faint-foreground"}>
        {icon}
      </span>
      {compact && compactBadge && (
        <span className="pointer-events-none absolute top-0.5 right-0.5">{compactBadge}</span>
      )}
      {!compact && <span className="min-w-0 truncate">{label}</span>}
      {!compact && trailing && <span className="ml-auto shrink-0">{trailing}</span>}
    </button>
  );
}

/** Genuine actionable human item badge on Home (never noisy alert counts). */
function AttentionCount({
  count,
  error,
  badgeOnly = false,
}: HumanCount & { badgeOnly?: boolean }) {
  const { locale } = useLocale();
  if (count === null) {
    if (!error) return null;
    return (
      <span
        title={`/api/attention: ${error}`}
        className={cx(
          "tnum font-medium text-faint-foreground",
          badgeOnly
            ? "inline-flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-muted px-1 text-[10px] leading-none ring-1 ring-sidebar"
            : "rounded bg-muted px-1.5 py-px text-micro",
        )}
      >
        ?
      </span>
    );
  }
  if (count <= 0) return null;
  return (
    <span
      title={tr(
        locale,
        `${count} 项待处理`,
        `${count} pending action${count === 1 ? "" : "s"}`,
      )}
      className={cx(
        "tnum font-medium text-warning",
        badgeOnly
          ? "inline-flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-wait-tint px-1 text-[10px] leading-none font-semibold ring-1 ring-sidebar"
          : "rounded bg-wait-tint px-1.5 py-px text-micro",
      )}
    >
      {badgeOnly && count > 99 ? "99+" : count}
    </span>
  );
}

const DRAWER_FOCUSABLE_SELECTOR =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

/** Compact header + slide-over left Side Tab drawer below the `md` breakpoint. */
export const MobileNav = memo(function MobileNav(
  props: Pick<
    SidebarProps,
    | "view"
    | "rsi"
    | "error"
    | "attention"
    | "onShowFleet"
    | "onShowAgents"
    | "onShowMissions"
    | "onShowGrading"
    | "onShowFeatures"
    | "onShowCoverage"
    | "onShowRsi"
    | "openAlerts"
    | "onShowAlerts"
    | "onShowScorecard"
    | "onShowCost"
    | "onShowStorage"
    | "onShowShow"
    | "onShowSessions"
    | "onSignOut"
  >,
) {
  const { view, rsi = null, error = null, attention, onSignOut } = props;
  const { locale } = useLocale();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const { diagnosticsOpen, toggleDiagnostics } = useDiagnosticsToggle(view);
  const handlers = viewHandlersOf(props);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const drawerRef = useRef<HTMLElement>(null);
  const closeBtnRef = useRef<HTMLButtonElement>(null);
  const wasOpenRef = useRef(false);

  useEffect(() => {
    if (!drawerOpen) {
      if (wasOpenRef.current) {
        wasOpenRef.current = false;
        triggerRef.current?.focus();
      }
      return;
    }
    wasOpenRef.current = true;
    const target = closeBtnRef.current ?? drawerRef.current;
    target?.focus();

    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") {
        setDrawerOpen(false);
        return;
      }
      if (event.key !== "Tab" || !drawerRef.current) return;
      const focusables = Array.from(
        drawerRef.current.querySelectorAll<HTMLElement>(DRAWER_FOCUSABLE_SELECTOR),
      ).filter((el) => el.offsetParent !== null || el === document.activeElement);
      if (focusables.length === 0) {
        event.preventDefault();
        drawerRef.current.focus();
        return;
      }
      const first = focusables[0];
      const last = focusables[focusables.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (!active || !drawerRef.current.contains(active)) {
        event.preventDefault();
        (event.shiftKey ? last : first).focus();
      } else if (event.shiftKey && active === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && active === last) {
        event.preventDefault();
        first.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [drawerOpen]);

  return (
    <>
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-sidebar-border bg-sidebar px-3 md:hidden">
        <button
          ref={triggerRef}
          type="button"
          onClick={() => setDrawerOpen(true)}
          aria-expanded={drawerOpen}
          aria-label={tr(locale, "打开侧边导航", "Open side navigation")}
          className={cx(
            "inline-flex size-9 shrink-0 items-center justify-center rounded-control text-muted-foreground hover:bg-surface-hover hover:text-foreground pointer-coarse:size-10",
            FOCUS_RING_INSET,
          )}
        >
          <Menu size={18} />
        </button>
        <Brand compact />
        <span className="truncate text-body font-semibold text-foreground">
          {navLabel(view, locale)}
        </span>
        <div className="ml-auto flex shrink-0 items-center gap-1.5">
          <LocaleSwitch compact />
          {onSignOut && <SignOutButton onSignOut={onSignOut} compact />}
          <ThemeSwitch />
        </div>
      </header>

      {drawerOpen && (
        <div className="fixed inset-0 z-50 flex md:hidden">
          <div
            aria-hidden="true"
            onClick={() => setDrawerOpen(false)}
            className="animate-fade-in absolute inset-0 bg-black/55 backdrop-blur-[2px]"
          />
          <aside
            ref={drawerRef}
            tabIndex={-1}
            role="dialog"
            aria-modal="true"
            aria-label={tr(locale, "侧边导航栏", "Side navigation")}
            className="relative flex h-full w-72 max-w-[85vw] flex-col border-r border-sidebar-border bg-sidebar shadow-[var(--floating-shadow)] focus:outline-none"
          >
            <div className="flex h-12 shrink-0 items-center justify-between border-b border-sidebar-border px-3.5">
              <Brand />
              <button
                ref={closeBtnRef}
                type="button"
                onClick={() => setDrawerOpen(false)}
                aria-label={tr(locale, "关闭侧边导航", "Close navigation")}
                className={cx(
                  "grid size-8 place-items-center rounded-control text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                  FOCUS_RING_INSET,
                )}
              >
                <X size={16} />
              </button>
            </div>

            <div className="min-h-0 flex-1 overflow-y-auto overscroll-contain">
              <SideTabList
                view={view}
                locale={locale}
                attention={attention}
                rsi={rsi}
                handlers={handlers}
                diagnosticsOpen={diagnosticsOpen}
                onToggleDiagnostics={toggleDiagnostics}
                onNavigate={() => setDrawerOpen(false)}
              />

              <div className="mx-2 mt-1 border-t border-sidebar-border/80 pt-2 pb-3">
                <NavItem
                  active={view === "sessions"}
                  onClick={() => {
                    handlers.sessions();
                    setDrawerOpen(false);
                  }}
                  onPrefetch={() => void prefetchView("sessions")}
                  icon={navIcon("sessions")}
                  label={navLabel("sessions", locale)}
                />
              </div>
            </div>

            <div className="flex shrink-0 items-center gap-2 border-t border-sidebar-border px-3.5 py-2.5 text-micro text-faint-foreground">
              <span
                className={cx(
                  "size-1.5 shrink-0 rounded-full",
                  error ? "bg-destructive" : "bg-success",
                )}
              />
              <span className="min-w-0 flex-1 truncate font-mono">{window.location.host}</span>
              <span className="shrink-0">
                {error ? tr(locale, "离线", "offline") : tr(locale, "在线", "online")}
              </span>
            </div>
          </aside>
        </div>
      )}
    </>
  );
});

/** 1-click Chinese / English switcher using contrast-checked surface tokens. */
export function LocaleSwitch({ compact = false }: { compact?: boolean }) {
  const { locale, setLocale } = useLocale();
  return (
    <span
      role="radiogroup"
      aria-label={tr(locale, "界面语言", "Language")}
      className={cx(
        "inline-flex shrink-0 items-center gap-px rounded-md bg-inset p-px ring-1 ring-surface-border",
        compact && "text-micro",
      )}
    >
      {LOCALE_CHOICES.map((option) => (
        <button
          key={option}
          type="button"
          role="radio"
          aria-checked={locale === option}
          onClick={() => setLocale(option)}
          title={option === "zh" ? "切换为中文" : "Switch to English"}
          className={cx(
            "inline-flex h-5 items-center justify-center rounded-[5px] px-1.5 text-micro font-medium transition-colors",
            FOCUS_RING_INSET,
            locale === option
              ? "bg-surface text-foreground shadow-hairline"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {option === "zh" ? "中" : "EN"}
        </button>
      ))}
    </span>
  );
}

/** Two-step sign-out control so accidental taps next to ThemeSwitch never clear the stored token. */
function SignOutButton({
  onSignOut,
  compact = false,
  iconOnly = false,
}: {
  onSignOut: () => void;
  compact?: boolean;
  iconOnly?: boolean;
}) {
  const { locale } = useLocale();
  const [confirming, setConfirming] = useState(false);

  useEffect(() => {
    if (!confirming) return;
    const timer = window.setTimeout(() => setConfirming(false), 4_000);
    return () => window.clearTimeout(timer);
  }, [confirming]);

  return (
    <button
      type="button"
      onClick={() => {
        if (!confirming) {
          setConfirming(true);
          return;
        }
        setConfirming(false);
        onSignOut();
      }}
      onBlur={() => setConfirming(false)}
      onKeyDown={(event) => {
        if (event.key === "Escape") setConfirming(false);
      }}
      title={
        confirming
          ? tr(locale, "再次点击确认清除 Token 并退出", "Click again to clear token and sign out")
          : tr(locale, "退出登录 / 更换 Token", "Sign out / switch token")
      }
      aria-label={
        confirming ? tr(locale, "确认退出登录", "Confirm sign out") : tr(locale, "退出登录", "Sign out")
      }
      className={cx(
        "inline-flex shrink-0 items-center gap-1 rounded-control text-caption font-medium transition-colors",
        iconOnly ? "size-7 justify-center px-0" : compact ? "h-7 px-2 pointer-coarse:h-10" : "h-9 px-2 pointer-coarse:h-11",
        confirming
          ? "bg-fail-tint text-destructive"
          : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
        FOCUS_RING_INSET,
      )}
    >
      <LogOut size={iconOnly || compact ? 12 : 14} strokeWidth={2} aria-hidden="true" />
      {!iconOnly && <span>{confirming ? tr(locale, "确认", "Confirm") : tr(locale, "退出", "Exit")}</span>}
    </button>
  );
}

const THEME_ICON = { system: Monitor, light: Sun, dark: Moon } as const;
const THEME_EVENT = "spawn:theme-change";
const THEME_LABELS: Record<ThemeChoice, Record<Locale, string>> = {
  system: { zh: "跟随系统", en: "system" },
  light: { zh: "浅色", en: "light" },
  dark: { zh: "深色", en: "dark" },
};

/** The page's theme: the system's, light or dark (src/theme.ts). */
export function ThemeSwitch({ compact = false }: { compact?: boolean }) {
  const { locale } = useLocale();
  const [choice, choose] = useStoredChoice<ThemeChoice>(THEME_KEY, THEME_CHOICES, "system");
  useTheme(choice);

  useEffect(() => {
    const sync = () => {
      try {
        const stored = window.localStorage.getItem(THEME_KEY);
        if (THEME_CHOICES.includes(stored as ThemeChoice) && stored !== choice) {
          choose(stored as ThemeChoice);
        }
      } catch {
        // Ignore storage read errors when syncing across sibling mounts.
      }
    };
    window.addEventListener(THEME_EVENT, sync);
    return () => window.removeEventListener(THEME_EVENT, sync);
  }, [choice, choose]);

  const selectTheme = (option: ThemeChoice) => {
    choose(option);
    if (typeof window !== "undefined") window.dispatchEvent(new Event(THEME_EVENT));
  };

  if (compact) {
    const idx = THEME_CHOICES.indexOf(choice);
    const nextOption = THEME_CHOICES[(idx + 1) % THEME_CHOICES.length];
    const Icon = THEME_ICON[choice];
    const currentLabel = THEME_LABELS[choice][locale];
    const nextLabel = THEME_LABELS[nextOption][locale];
    return (
      <button
        type="button"
        onClick={() => selectTheme(nextOption)}
        title={tr(locale, `主题：${currentLabel}（点击切换为${nextLabel}）`, `Theme: ${currentLabel} (switch to ${nextLabel})`)}
        aria-label={tr(locale, `切换主题（当前：${currentLabel}）`, `Switch theme (current: ${currentLabel})`)}
        className={cx(
          "grid size-7 place-items-center rounded-control text-muted-foreground hover:bg-surface-hover hover:text-foreground",
          FOCUS_RING_INSET,
        )}
      >
        <Icon size={13} strokeWidth={2} />
      </button>
    );
  }

  return (
    <span
      role="radiogroup"
      aria-label={tr(locale, "主题模式", "Theme")}
      className="flex items-center gap-px rounded-md p-px ring-1 ring-surface-border"
    >
      {THEME_CHOICES.map((option) => {
        const Icon = THEME_ICON[option];
        const optionLabel = THEME_LABELS[option][locale];
        const tip = tr(locale, `主题：${optionLabel}`, `Theme: ${optionLabel}`);
        return (
          <button
            key={option}
            type="button"
            role="radio"
            aria-checked={choice === option}
            title={tip}
            aria-label={tip}
            onClick={() => selectTheme(option)}
            className={cx(
              "grid size-5 place-items-center rounded-[5px]",
              FOCUS_RING_INSET,
              choice === option
                ? "bg-surface-selected text-foreground"
                : "text-faint-foreground hover:text-muted-foreground",
            )}
          >
            <Icon size={12} strokeWidth={2} />
          </button>
        );
      })}
    </span>
  );
}

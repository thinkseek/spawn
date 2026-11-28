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
import { useEffect, useMemo, useRef, useState } from "react";
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
import { AGENT_GROUP_LABEL, groupAgents, heldTask, stateWords } from "../ordering";
import { recordedState } from "../recordedState";
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

  return (
    <nav aria-label={tr(locale, "主导航", "Sections")} className="px-2 pt-2 pb-2">
      {!compact && <NavSectionLabel label={tr(locale, "核心", "Core")} />}
      <div className="space-y-px">
        {PRIMARY_VIEWS.map((id) => (
          <NavItem
            key={id}
            active={view === id}
            onClick={() => activate(id)}
            icon={navIcon(id)}
            label={navLabel(id, locale)}
            compact={compact}
            trailing={renderTrailing(id)}
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

export function Sidebar(props: SidebarProps) {
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
  const [diagnosticsStored, setDiagnosticsStored] = useStoredChoice(
    "spawn.sidebar.diagnostics",
    ["open", "closed"] as const,
    "closed",
  );
  const diagnosticsOpen = shouldExpandDiagnostics(diagnosticsStored, view);
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
          onToggleDiagnostics={() =>
            setDiagnosticsStored(diagnosticsOpen ? "closed" : "open")
          }
        />

        <div className="mx-2 border-t border-sidebar-border/80 pt-2">
          {compact ? (
            <button
              type="button"
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
              title={`${window.location.host} · ${error ? "offline" : "online"}`}
            />
            <LocaleSwitch compact />
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
}

/** States whose words a held task's says better: the worker idles at its
 * prompt while its task is judged or lands. */
const AT_THE_GATE = new Set(["gating", "landing", "in review"]);

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
        return (
          <section key={group.id} aria-label={AGENT_GROUP_LABEL[group.id]}>
            <GroupHeader
              className="px-2 pb-1"
              label={AGENT_GROUP_LABEL[group.id]}
              count={group.agents.length}
              expanded={folds ? open : undefined}
              onToggle={folds ? () => setEnded(open ? "closed" : "open") : undefined}
            />
            {(open || group.agents.some((agent) => agent.session_id === selectedId)) && (
              <ul className="space-y-px">
                {group.agents
                  .filter((agent) => open || agent.session_id === selectedId)
                  .map((agent) => {
                    const held = heldTask(agent, context);
                    const named = nameTask(agent, context);
                    const recorded = recordedState(agent);
                    const label =
                      held && AT_THE_GATE.has(held.label)
                        ? `Task ${held.label}`
                        : recorded.label;
                    const at =
                      recorded.since ?? observed.get(agent.session_id)?.recorded_at_ms ?? null;
                    return (
                      <li key={agent.session_id}>
                        <SessionRow
                          sessionId={agent.session_id}
                          agentId={agent.agent_id}
                          name={nameOf(agent)}
                          line={
                            named && named !== held
                              ? named.title
                                ? `last: ${named.title}`
                                : null
                              : (named?.title ?? agent.task_title)
                          }
                          state={recorded.dot}
                          label={label}
                          at={at}
                          attention={agent.attention}
                          task={held ? `T${held.task_id ?? "?"} ${held.label}${held.title ? ` · ${held.title}` : ""}` : null}
                          selected={agent.session_id === selectedId}
                          onSelect={onSelect}
                        />
                      </li>
                    );
                  })}
              </ul>
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
          <span className="ml-auto flex shrink-0 text-warning" title="Escalated">
            <BellRing size={11} aria-hidden="true" />
            <span className="sr-only">escalated</span>
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
  icon,
  label,
  compact = false,
  trailing,
}: {
  active: boolean;
  onClick: () => void;
  icon: ReactNode;
  label: string;
  compact?: boolean;
  trailing?: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      title={compact ? label : undefined}
      aria-label={compact ? label : undefined}
      className={cx(
        "flex h-8 w-full items-center rounded-control text-left text-label transition-colors pointer-coarse:h-11",
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
      {!compact && <span className="min-w-0 truncate">{label}</span>}
      {!compact && trailing && <span className="ml-auto shrink-0">{trailing}</span>}
    </button>
  );
}

/** Genuine actionable human item badge on Home (never noisy alert counts). */
function AttentionCount({ count, error }: HumanCount) {
  if (count === null) {
    if (!error) return null;
    return (
      <span
        title={`/api/attention: ${error}`}
        className="tnum rounded bg-muted px-1.5 py-px text-micro font-medium text-faint-foreground"
      >
        ?
      </span>
    );
  }
  if (count <= 0) return null;
  return (
    <span
      title={`${count} pending action${count === 1 ? "" : "s"}`}
      className="tnum rounded bg-wait-tint px-1.5 py-px text-micro font-medium text-warning"
    >
      {count}
    </span>
  );
}

/** Compact header + slide-over left Side Tab drawer below the `md` breakpoint. */
export function MobileNav(
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
  const [diagnosticsStored, setDiagnosticsStored] = useStoredChoice(
    "spawn.sidebar.diagnostics",
    ["open", "closed"] as const,
    "closed",
  );
  const diagnosticsOpen = shouldExpandDiagnostics(diagnosticsStored, view);
  const handlers = viewHandlersOf(props);

  useEffect(() => {
    if (!drawerOpen) return;
    const onKey = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") setDrawerOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [drawerOpen]);

  return (
    <>
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-sidebar-border bg-sidebar px-3 md:hidden">
        <button
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
            role="dialog"
            aria-modal="true"
            aria-label={tr(locale, "侧边导航栏", "Side navigation")}
            className="relative flex h-full w-72 max-w-[85vw] flex-col border-r border-sidebar-border bg-sidebar shadow-[var(--floating-shadow)]"
          >
            <div className="flex h-12 shrink-0 items-center justify-between border-b border-sidebar-border px-3.5">
              <Brand />
              <button
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
                onToggleDiagnostics={() =>
                  setDiagnosticsStored(diagnosticsOpen ? "closed" : "open")
                }
                onNavigate={() => setDrawerOpen(false)}
              />

              <div className="mx-2 mt-1 border-t border-sidebar-border/80 pt-2 pb-3">
                <NavItem
                  active={view === "sessions"}
                  onClick={() => {
                    handlers.sessions();
                    setDrawerOpen(false);
                  }}
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
}

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
}: {
  onSignOut: () => void;
  compact?: boolean;
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
        "inline-flex shrink-0 items-center gap-1 rounded-control px-2 text-caption font-medium transition-colors",
        compact ? "h-7 pointer-coarse:h-10" : "h-9 pointer-coarse:h-11",
        confirming
          ? "bg-fail-tint text-destructive"
          : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
        FOCUS_RING_INSET,
      )}
    >
      <LogOut size={compact ? 12 : 14} strokeWidth={2} aria-hidden="true" />
      <span>{confirming ? tr(locale, "确认", "Confirm") : tr(locale, "退出", "Exit")}</span>
    </button>
  );
}

const THEME_ICON = { system: Monitor, light: Sun, dark: Moon } as const;
const THEME_EVENT = "spawn:theme-change";

/** The page's theme: the system's, light or dark (src/theme.ts). */
export function ThemeSwitch() {
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

  return (
    <span role="radiogroup" aria-label="Theme" className="flex items-center gap-px rounded-md p-px ring-1 ring-surface-border">
      {THEME_CHOICES.map((option) => {
        const Icon = THEME_ICON[option];
        return (
          <button
            key={option}
            type="button"
            role="radio"
            aria-checked={choice === option}
            title={`Theme: ${option}`}
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

import { Activity, BellRing, Gauge, House, Monitor, Moon, Orbit, Rows3, Sun, Target, Waypoints } from "lucide-react";
import type { ReactNode } from "react";

import type { OverviewSession, RsiSummary, SessionState } from "../api";
import type { View } from "../App";
import type { FleetAgent } from "../fleet";
import { sessionState } from "../fleet";
import { rsiPhase, sessionTaskLabel } from "../missions";
import type { ThemeChoice } from "../theme";
import { THEME_CHOICES, THEME_KEY, useTheme } from "../theme";
import type { FleetContext } from "../ordering";
import { AGENT_GROUP_LABEL, groupAgents, heldTask } from "../ordering";
import { phaseStyle } from "./rsi/Phase";
import { StateDot, stateLabel } from "./StateDot";
import { useStoredChoice } from "./ViewToggle";
import { Ago, FOCUS_RING_INSET, GroupHeader, MiddleId, cx } from "./ui";

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
  /** What is escalated, by the dispatcher's view (attention.ts
   * `needsHuman`): the queue's count, null while it is unknown, with why. */
  attention: HumanCount;
  /** How many alerts are open (the record's own count, from /api/alerts). */
  openAlerts: number;
  onShowFleet: () => void;
  onShowMissions: () => void;
  onShowRsi: () => void;
  onShowAlerts: () => void;
  onShowScorecard: () => void;
  onShowShow: () => void;
  onShowSessions: () => void;
  onSelect: (sessionId: string) => void;
}

function Brand() {
  return (
    <div className="flex items-center gap-2.5">
      <span className="grid size-7 place-items-center rounded-md border border-border-strong bg-surface text-foreground">
        <Orbit size={15} strokeWidth={2} />
      </span>
      <span className="text-body font-semibold tracking-tight text-foreground">
        Spawn
      </span>
    </div>
  );
}

export function Sidebar({
  sessions,
  agents,
  context,
  selectedId,
  view,
  rsi,
  error,
  attention,
  openAlerts,
  onShowFleet,
  onShowMissions,
  onShowRsi,
  onShowAlerts,
  onShowScorecard,
  onShowShow,
  onSelect,
}: SidebarProps) {
  const phase = rsiPhase(rsi);
  return (
    <aside className="hidden w-60 shrink-0 flex-col border-r border-sidebar-border bg-sidebar md:flex">
      <div className="flex h-12 shrink-0 items-center px-4">
        <Brand />
      </div>

      <div className="space-y-px px-2 pt-2 pb-4">
        <NavItem
          active={view === "fleet"}
          onClick={onShowFleet}
          icon={<House size={15} />}
          label="Home"
          trailing={<AttentionCount {...attention} />}
        />
        <NavItem
          active={view === "missions"}
          onClick={onShowMissions}
          icon={<Target size={15} />}
          label="Missions"
        />
        <NavItem
          active={view === "rsi"}
          onClick={onShowRsi}
          icon={<Activity size={15} />}
          label="RSI mission"
          trailing={
            phase && (
              <span className="flex items-center gap-1.5 text-micro text-faint-foreground">
                <span
                  className={cx(
                    "size-1.5 rounded-full",
                    phase === "halted" ? "bg-destructive" : phaseStyle(phase).dot,
                    phaseStyle(phase).pulse && "animate-spawn-pulse",
                  )}
                />
                {phase}
              </span>
            )
          }
        />
        <NavItem
          active={view === "alerts"}
          onClick={onShowAlerts}
          icon={<BellRing size={15} />}
          label="Alerts"
          trailing={
            openAlerts > 0 && (
              <span
                title={`${openAlerts} open alert${openAlerts === 1 ? "" : "s"}`}
                className="tnum rounded bg-fail-tint px-1.5 py-px text-micro font-medium text-destructive"
              >
                {openAlerts}
              </span>
            )
          }
        />
        <NavItem
          active={view === "scorecard"}
          onClick={onShowScorecard}
          icon={<Gauge size={15} />}
          label="Scorecard"
        />
        <NavItem
          active={view === "show"}
          onClick={onShowShow}
          icon={<Waypoints size={15} />}
          label="Show"
        />
      </div>

      <div className="flex items-center px-4 pb-1.5 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        Sessions
        <span className="tnum ml-auto normal-case tracking-normal">
          {(agents?.length ?? sessions.length) || ""}
        </span>
      </div>

      <nav
        aria-label="Sessions"
        className="min-h-0 flex-1 overflow-y-auto px-2 pb-3"
      >
        {agents ? (
          <GroupedSessions
            agents={agents}
            context={context}
            sessions={sessions}
            selectedId={selectedId}
            onSelect={onSelect}
          />
        ) : sessions.length === 0 ? (
          <p className="px-2 py-4 text-caption text-faint-foreground">
            No sessions yet.
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

      <div className="flex h-10 shrink-0 items-center gap-2 border-t border-sidebar-border px-4 text-micro text-faint-foreground">
        <span
          className={cx(
            "size-1.5 rounded-full",
            error ? "bg-destructive" : "bg-success",
          )}
        />
        <span className="truncate font-mono">{window.location.host}</span>
        <span className="ml-auto">{error ? "offline" : "online"}</span>
        <ThemeSwitch />
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
  onSelect,
}: {
  agents: FleetAgent[];
  context: FleetContext;
  sessions: OverviewSession[];
  selectedId: string | null;
  onSelect: (sessionId: string) => void;
}) {
  const [ended, setEnded] = useStoredChoice("spawn.sidebar.ended", ["open", "closed"] as const, "closed");
  if (agents.length === 0) {
    return <p className="px-2 py-4 text-caption text-faint-foreground">No sessions yet.</p>;
  }
  const observed = new Map(sessions.map((session) => [session.session_id, session]));
  return (
    <div className="space-y-3">
      {groupAgents(agents, context).map((group) => {
        if (group.agents.length === 0) return null;
        const folds = group.id === "ended";
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
            {/* Folded, the group still shows the session open in the main view. */}
            {(open || group.agents.some((agent) => agent.session_id === selectedId)) && (
              <ul className="space-y-px">
                {group.agents.filter((agent) => open || agent.session_id === selectedId).map((agent) => {
                  const held = heldTask(agent, context);
                  const state = sessionState(agent.state);
                  const label =
                    held && AT_THE_GATE.has(held.label)
                      ? `Task ${held.label}`
                      : stateLabel(state);
                  const at =
                    agent.live || agent.ended_at_ms === null
                      ? (agent.activity.last_event_at_ms ??
                        observed.get(agent.session_id)?.recorded_at_ms ??
                        null)
                      : agent.ended_at_ms;
                  return (
                    <li key={agent.session_id}>
                      <SessionRow
                        sessionId={agent.session_id}
                        agentId={agent.agent_id}
                        state={state}
                        // Ended outside the Ended group is news: say it.
                        label={agent.live || group.id === "ended" ? label : `${label}, ended`}
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
 * state and how long ago, then the session id. Nothing wraps at the
 * sidebar's 240 px. */
function SessionRow({
  sessionId,
  agentId,
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
      title={[agentId || sessionId, sessionId, task].filter(Boolean).join("\n")}
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
        <MiddleId
          id={agentId || "unknown agent"}
          title={false}
          className="text-label font-medium text-foreground"
        />
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
      <MiddleId
        id={sessionId}
        title={false}
        className="mt-px font-mono text-micro text-faint-foreground"
      />
    </button>
  );
}

function NavItem({
  active,
  onClick,
  icon,
  label,
  trailing,
}: {
  active: boolean;
  onClick: () => void;
  icon: ReactNode;
  label: string;
  trailing?: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={cx(
        // A fixed row: 32 px, 44 px under a finger.
        "flex h-8 w-full items-center gap-2.5 rounded-control px-2.5 text-left text-label transition-colors pointer-coarse:h-11",
        FOCUS_RING_INSET,
        active
          ? "bg-surface-selected font-semibold text-foreground"
          : "font-medium text-muted-foreground hover:bg-surface-hover hover:text-foreground",
      )}
    >
      <span className={active ? "text-foreground" : "text-faint-foreground"}>
        {icon}
      </span>
      {label}
      {trailing && <span className="ml-auto">{trailing}</span>}
    </button>
  );
}

/** The Escalations queue's count (the dispatcher's view); a muted "?"
 * when it could not be read, never a count from elsewhere. */
function AttentionCount({ count, error }: HumanCount) {
  if (count === null) {
    if (!error) return null;
    return (
      <span
        title={`What is escalated is unknown: /api/attention: ${error}`}
        className="tnum rounded bg-muted px-1.5 py-px text-micro font-medium text-faint-foreground"
      >
        ?
      </span>
    );
  }
  if (count <= 0) return null;
  return (
    <span
      title={`${count} thing${count === 1 ? "" : "s"} in Escalations (the dispatcher's queue)`}
      className="tnum rounded bg-wait-tint px-1.5 py-px text-micro font-medium text-warning"
    >
      {count}
    </span>
  );
}

/** Top bar that replaces the sidebar below the `md` breakpoint. */
export function MobileNav({
  view,
  attention,
  openAlerts,
  onShowFleet,
  onShowMissions,
  onShowRsi,
  onShowAlerts,
  onShowScorecard,
  onShowShow,
  onShowSessions,
}: Pick<
  SidebarProps,
  | "view"
  | "attention"
  | "onShowFleet"
  | "onShowMissions"
  | "onShowRsi"
  | "openAlerts"
  | "onShowAlerts"
  | "onShowScorecard"
  | "onShowShow"
  | "onShowSessions"
>) {
  return (
    <div className="flex h-12 shrink-0 items-center gap-3 border-b border-sidebar-border bg-sidebar px-3 md:hidden">
      <Brand />
      <div className="ml-auto flex min-w-0 overflow-x-auto rounded-lg border border-surface-border bg-surface p-0.5">
        {(
          [
            ["fleet", "Home", <House key="f" size={13} />, onShowFleet],
            ["missions", "Missions", <Target key="m" size={13} />, onShowMissions],
            ["rsi", "RSI", <Activity key="a" size={13} />, onShowRsi],
            ["alerts", "Alerts", <BellRing key="b" size={13} />, onShowAlerts],
            ["scorecard", "Score", <Gauge key="g" size={13} />, onShowScorecard],
            ["show", "Show", <Waypoints key="w" size={13} />, onShowShow],
            ["sessions", "Sessions", <Rows3 key="s" size={13} />, onShowSessions],
          ] as const
        ).map(([id, label, icon, onClick]) => (
          <button
            key={id}
            type="button"
            onClick={onClick}
            aria-current={view === id ? "page" : undefined}
            className={cx(
              "inline-flex h-7 items-center gap-1.5 rounded-md px-2.5 text-caption font-medium transition-colors",
              view === id
                ? "bg-surface-selected text-foreground"
                : "text-muted-foreground",
            )}
          >
            {icon}
            {label}
            {id === "fleet" && <AttentionCount {...attention} />}
            {id === "alerts" && openAlerts > 0 && (
              <span className="tnum text-micro font-medium text-destructive">
                {openAlerts}
              </span>
            )}
          </button>
        ))}
      </div>
    </div>
  );
}

const THEME_ICON = { system: Monitor, light: Sun, dark: Moon } as const;

/** The page's theme: the system's, light or dark (src/theme.ts). */
function ThemeSwitch() {
  const [choice, choose] = useStoredChoice<ThemeChoice>(THEME_KEY, THEME_CHOICES, "system");
  useTheme(choice);
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
            onClick={() => choose(option)}
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

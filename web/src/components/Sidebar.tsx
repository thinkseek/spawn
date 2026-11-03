import { Activity, LayoutGrid, Orbit, Rows3 } from "lucide-react";
import type { ReactNode } from "react";

import type { OverviewSession, RsiSummary } from "../api";
import type { View } from "../App";
import { relativeTime } from "../format";
import { phaseStyle } from "./rsi/Phase";
import { StateDot, stateLabel } from "./StateDot";
import { cx } from "./ui";

interface SidebarProps {
  sessions: OverviewSession[];
  selectedId: string | null;
  view: View;
  rsi: RsiSummary | null;
  error: string | null;
  /** Agents that need a human, from the fleet poll. */
  attention: number;
  onShowFleet: () => void;
  onShowRsi: () => void;
  onShowSessions: () => void;
  onSelect: (sessionId: string) => void;
}

function Brand() {
  return (
    <div className="flex items-center gap-2.5">
      <span className="grid size-7 place-items-center rounded-md border border-border-strong bg-gradient-to-b from-white/[0.10] to-white/[0.02] text-foreground">
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
  selectedId,
  view,
  rsi,
  error,
  attention,
  onShowFleet,
  onShowRsi,
  onSelect,
}: SidebarProps) {
  const phase = rsi?.open_iteration?.phase ?? null;
  return (
    <aside className="hidden w-60 shrink-0 flex-col border-r border-sidebar-border bg-sidebar md:flex">
      <div className="flex h-12 shrink-0 items-center px-4">
        <Brand />
      </div>

      <div className="space-y-px px-2 pt-2 pb-4">
        <NavItem
          active={view === "fleet"}
          onClick={onShowFleet}
          icon={<LayoutGrid size={15} />}
          label="Fleet"
          trailing={<AttentionCount count={attention} />}
        />
        <NavItem
          active={view === "rsi"}
          onClick={onShowRsi}
          icon={<Activity size={15} />}
          label="RSI loop"
          trailing={
            phase && (
              <span className="flex items-center gap-1.5 text-micro text-faint-foreground">
                <span
                  className={cx(
                    "size-1.5 rounded-full",
                    phaseStyle(phase).dot,
                    phaseStyle(phase).pulse && "animate-spawn-pulse",
                  )}
                />
                {phase}
              </span>
            )
          }
        />
      </div>

      <div className="flex items-center px-4 pb-1.5 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        Sessions
        <span className="tnum ml-auto normal-case tracking-normal">
          {sessions.length || ""}
        </span>
      </div>

      <nav
        aria-label="Sessions"
        className="min-h-0 flex-1 overflow-y-auto px-2 pb-3"
      >
        {sessions.length === 0 ? (
          <p className="px-2 py-4 text-caption text-faint-foreground">
            No sessions yet.
          </p>
        ) : (
          <ul className="space-y-px">
            {sessions.map((session) => {
              const selected = session.session_id === selectedId;
              const confidence = session.observation?.confidence;
              return (
                <li key={session.session_id}>
                  <button
                    type="button"
                    onClick={() => onSelect(session.session_id)}
                    aria-current={selected ? "true" : undefined}
                    title={`${session.agent_id} · ${session.session_id}`}
                    className={cx(
                      "w-full rounded-md px-2.5 py-2 text-left transition-colors",
                      selected
                        ? "bg-surface-selected"
                        : "hover:bg-sidebar-accent",
                    )}
                  >
                    <span className="flex items-center gap-2">
                      <StateDot state={session.state} size="sm" />
                      <span className="min-w-0 flex-1 truncate text-label font-medium text-foreground">
                        {session.agent_id || "unknown agent"}
                      </span>
                      <span className="tnum shrink-0 text-micro text-faint-foreground">
                        {relativeTime(session.recorded_at_ms)}
                      </span>
                    </span>
                    <span className="mt-0.5 flex items-center gap-1.5 pl-3.5 text-micro text-muted-foreground">
                      <span>{stateLabel(session.state)}</span>
                      {confidence != null && confidence > 0 && (
                        <span className="tnum text-faint-foreground">
                          · {Math.round(confidence * 100)}%
                        </span>
                      )}
                      <span className="ml-auto min-w-0 truncate font-mono text-faint-foreground">
                        {session.session_id}
                      </span>
                    </span>
                  </button>
                </li>
              );
            })}
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
        <span className="ml-auto">{error ? "offline" : "read-only"}</span>
      </div>
    </aside>
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
        "flex h-8 w-full items-center gap-2.5 rounded-md px-2.5 text-left text-label font-medium transition-colors",
        active
          ? "bg-surface-selected text-foreground"
          : "text-muted-foreground hover:bg-sidebar-accent hover:text-foreground",
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

function AttentionCount({ count }: { count: number }) {
  if (count <= 0) return null;
  return (
    <span
      title={`${count} agent${count === 1 ? "" : "s"} need a human`}
      className="tnum rounded bg-warning/15 px-1.5 py-px text-micro font-medium text-warning"
    >
      {count}
    </span>
  );
}

/** Top bar that replaces the sidebar below the `md` breakpoint. */
export function MobileNav({
  view,
  attention,
  onShowFleet,
  onShowRsi,
  onShowSessions,
}: Pick<
  SidebarProps,
  "view" | "attention" | "onShowFleet" | "onShowRsi" | "onShowSessions"
>) {
  return (
    <div className="flex h-12 shrink-0 items-center gap-3 border-b border-sidebar-border bg-sidebar px-3 md:hidden">
      <Brand />
      <div className="ml-auto flex rounded-lg border border-surface-border bg-surface p-0.5">
        {(
          [
            ["fleet", "Fleet", <LayoutGrid key="f" size={13} />, onShowFleet],
            ["rsi", "RSI", <Activity key="a" size={13} />, onShowRsi],
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
            {id === "fleet" && <AttentionCount count={attention} />}
          </button>
        ))}
      </div>
    </div>
  );
}

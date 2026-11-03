import { Activity } from "lucide-react";

import type { OverviewSession } from "../api";
import type { View } from "../App";
import { relativeTime } from "../format";
import { StateDot } from "./StateDot";

interface SidebarProps {
  sessions: OverviewSession[];
  selectedId: string | null;
  view: View;
  onShowRsi: () => void;
  onSelect: (sessionId: string) => void;
}

export function Sidebar({
  sessions,
  selectedId,
  view,
  onShowRsi,
  onSelect,
}: SidebarProps) {
  return (
    <aside className="flex w-60 shrink-0 flex-col border-r border-sidebar-border bg-sidebar">
      <div className="px-4 pb-5 pt-5">
        <div className="text-label font-semibold tracking-[0.3em] text-foreground">
          SPAWN
        </div>
        <div className="mt-1 text-micro text-muted-foreground">
          self-improving agent loop
        </div>
      </div>

      <div className="px-2 pb-4">
        <button
          type="button"
          onClick={onShowRsi}
          aria-current={view === "rsi" ? "page" : undefined}
          className={`flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left text-label transition-colors ${
            view === "rsi"
              ? "bg-surface-selected text-foreground"
              : "text-muted-foreground hover:bg-surface-hover hover:text-foreground"
          }`}
        >
          <Activity size={14} />
          RSI loop
        </button>
      </div>

      <div className="px-4 pb-2 text-micro font-medium uppercase tracking-[0.12em] text-faint-foreground">
        Sessions
      </div>

      <nav
        aria-label="Sessions"
        className="min-h-0 flex-1 overflow-y-auto px-2 pb-3"
      >
        {sessions.length === 0 ? (
          <p className="px-2 py-6 text-center text-caption text-faint-foreground">
            No sessions yet.
          </p>
        ) : (
          <ul className="space-y-0.5">
            {sessions.map((session) => {
              const selected = session.session_id === selectedId;
              return (
                <li key={session.session_id}>
                  <button
                    type="button"
                    onClick={() => onSelect(session.session_id)}
                    aria-current={selected ? "true" : undefined}
                    title={`${session.agent_id} · ${session.session_id}`}
                    className={`w-full rounded-md px-2.5 py-2 text-left transition-colors ${
                      selected
                        ? "bg-surface-selected"
                        : "hover:bg-surface-hover"
                    }`}
                  >
                    <span className="flex items-center gap-2">
                      <StateDot state={session.state} size="sm" />
                      <span className="min-w-0 flex-1 truncate text-label text-foreground">
                        {session.agent_id || "unknown agent"}
                      </span>
                      <span className="shrink-0 text-micro text-faint-foreground">
                        {relativeTime(session.recorded_at_ms)}
                      </span>
                    </span>
                    <span className="mt-0.5 block truncate pl-3.5 text-micro text-muted-foreground">
                      {session.session_id}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </nav>
    </aside>
  );
}

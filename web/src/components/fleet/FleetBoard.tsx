import { Bot, Wrench } from "lucide-react";

import type { FleetAgent } from "../../fleet";
import { percent, stateLabelUpper } from "../../fleet";
import { relativeTime } from "../../format";
import { LiveBadge } from "../Terminal";
import { Card, CardHeader, EmptyState, cx } from "../ui";
import { FleetStateDot, JudgeBadge, RoleBadge } from "./bits";

/** One line for "what is it doing right now". */
export function doingNow(agent: FleetAgent): {
  label: string;
  tool: boolean;
  at: number | null;
} {
  const activity = agent.activity;
  if (activity.current_tool) {
    return { label: activity.current_tool, tool: true, at: activity.last_event_at_ms };
  }
  return {
    label: activity.last_event_name ?? "—",
    tool: false,
    at: activity.last_event_at_ms,
  };
}

const COLUMNS =
  "lg:grid lg:grid-cols-[minmax(0,15rem)_minmax(0,1fr)_minmax(0,1.5fr)_8.5rem_5rem] lg:items-center lg:gap-4";

export function FleetBoard({
  agents,
  selectedId,
  onSelect,
}: {
  agents: FleetAgent[];
  selectedId: string | null;
  onSelect: (sessionId: string) => void;
}) {
  return (
    <Card className="flex shrink-0 flex-col overflow-hidden">
      <CardHeader
        icon={<Bot size={14} />}
        title="Fleet"
        meta={<span className="tnum">{agents.length} agents</span>}
      />
      {agents.length === 0 ? (
        <EmptyState
          title="No agents yet"
          hint="Agents appear here once the supervisor or a worker starts."
        />
      ) : (
        <>
          <div
            className={cx(
              "hidden border-b border-surface-border px-4 py-1.5 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground",
              COLUMNS,
            )}
          >
            <span>Agent</span>
            <span>Doing now</span>
            <span>Task</span>
            <span>Judged by</span>
            <span className="text-right">In state</span>
          </div>
          <ul className="divide-y divide-surface-border">
            {agents.map((agent) => (
              <li key={agent.session_id}>
                <AgentRow
                  agent={agent}
                  selected={agent.session_id === selectedId}
                  onSelect={onSelect}
                />
              </li>
            ))}
          </ul>
        </>
      )}
    </Card>
  );
}

function AgentRow({
  agent,
  selected,
  onSelect,
}: {
  agent: FleetAgent;
  selected: boolean;
  onSelect: (sessionId: string) => void;
}) {
  const now = doingNow(agent);
  const judgement = agent.judgement;
  return (
    <button
      type="button"
      onClick={() => onSelect(agent.session_id)}
      aria-current={selected ? "true" : undefined}
      className={cx(
        "relative flex w-full flex-wrap items-center gap-x-3 gap-y-1.5 px-4 py-2.5 text-left transition-colors",
        COLUMNS,
        selected ? "bg-surface-selected" : "hover:bg-surface-hover",
      )}
    >
      {agent.attention && (
        <span
          aria-label="Needs a human"
          className="absolute inset-y-0 left-0 w-0.5 bg-warning"
        />
      )}

      {/* Agent */}
      <div className="flex w-full min-w-0 items-center gap-2 lg:w-auto">
        <FleetStateDot state={agent.state} />
        <div className="min-w-0">
          <div className="flex min-w-0 items-center gap-1.5">
            <span className="truncate text-label font-medium text-foreground">
              {agent.agent_id || agent.session_id}
            </span>
            <RoleBadge role={agent.role} />
          </div>
          <div className="mt-0.5 flex items-center gap-1.5 text-micro text-muted-foreground">
            <span>{stateLabelUpper(agent.state)}</span>
            <LiveBadge live={agent.live} />
            {agent.engine && (
              <span className="text-faint-foreground">{agent.engine}</span>
            )}
          </div>
        </div>
      </div>

      {/* Doing now */}
      <div className="flex w-full min-w-0 items-center gap-1.5 pl-4 text-caption lg:w-auto lg:pl-0">
        {now.tool && <Wrench size={12} className="shrink-0 text-success" />}
        <span
          className={cx(
            "min-w-0 truncate font-mono",
            now.tool ? "text-foreground" : "text-muted-foreground",
          )}
        >
          {now.label}
        </span>
        <span className="tnum shrink-0 text-faint-foreground">
          {relativeTime(now.at)}
        </span>
      </div>

      {/* Task */}
      <div
        className="w-full min-w-0 truncate pl-4 text-caption text-muted-foreground lg:w-auto lg:pl-0"
        title={agent.task ?? undefined}
      >
        {agent.task ? (
          agent.task.replace(/\s+/g, " ")
        ) : (
          <span className="text-faint-foreground">No task</span>
        )}
      </div>

      {/* Judged by */}
      <div className="flex items-center gap-1.5 pl-4 lg:pl-0">
        <JudgeBadge judge={judgement?.judge} />
        <span
          className="tnum text-caption text-muted-foreground"
          title="Confidence of the latest judgement"
        >
          {percent(judgement?.confidence)}
        </span>
      </div>

      {/* In state */}
      <div className="tnum text-caption text-faint-foreground lg:text-right">
        <span className="lg:hidden">in state </span>
        {relativeTime(agent.state_since_ms).replace(" ago", "")}
      </div>
    </button>
  );
}

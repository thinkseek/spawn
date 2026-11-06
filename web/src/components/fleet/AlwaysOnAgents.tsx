import { Infinity as Forever, Wrench } from "lucide-react";

import type { FleetAgent } from "../../fleet";
import { stateLabelUpper } from "../../fleet";
import { absoluteTitle, formatDuration } from "../../format";
import type { FleetContext } from "../../ordering";
import { heldTask, residentName } from "../../ordering";
import { LiveBadge } from "../Terminal";
import { Ago, Card, CardHeader, EmptyState, FOCUS_RING_INSET, MiddleId, cx, sinceText } from "../ui";
import { FleetStateDot } from "./bits";
import { TaskCell, doingNow } from "./FleetBoard";

/** How long an agent's current execution has run, while it runs. */
export function uptimeMs(agent: Pick<FleetAgent, "live" | "started_at_ms">, now = Date.now()): number | null {
  return agent.live && agent.started_at_ms !== null ? Math.max(0, now - agent.started_at_ms) : null;
}

/**
 * The agents that run forever: the supervisor, then the residents. Pinned
 * above the fleet, live or not, each with its liveness, how long it has
 * been in its state, what it did last and the task it holds.
 */
export function AlwaysOnAgents({
  agents,
  context,
  usualEngine,
  selectedId,
  onSelect,
}: {
  agents: FleetAgent[];
  context: FleetContext;
  usualEngine: string | null;
  selectedId: string | null;
  onSelect: (sessionId: string) => void;
}) {
  const up = agents.filter((agent) => agent.live).length;
  return (
    <Card className="shrink-0 overflow-hidden">
      <CardHeader
        icon={<Forever size={14} />}
        title="Always-on agents"
        meta={
          <span className={cx("tnum", up < agents.length && "font-medium text-warning")}>
            {up} of {agents.length} live
          </span>
        }
      />
      {agents.length === 0 ? (
        <EmptyState
          title="No supervisor or resident yet"
          hint="The supervisor and approved residents appear here once they start."
        />
      ) : (
        <ul className="grid gap-px bg-surface-border md:grid-cols-2 xl:grid-cols-3">
          {agents.map((agent) => (
            <li key={agent.session_id} className="min-w-0 bg-surface">
              <AlwaysOnTile
                agent={agent}
                context={context}
                usualEngine={usualEngine}
                selected={agent.session_id === selectedId}
                onSelect={onSelect}
              />
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

function AlwaysOnTile({
  agent,
  context,
  usualEngine,
  selected,
  onSelect,
}: {
  agent: FleetAgent;
  context: FleetContext;
  usualEngine: string | null;
  selected: boolean;
  onSelect: (sessionId: string) => void;
}) {
  const resident = residentName(agent, context);
  const held = heldTask(agent, context);
  const now = doingNow(agent);
  const uptime = uptimeMs(agent);
  const engine = agent.engine && agent.engine !== usualEngine ? agent.engine : null;
  return (
    <button
      type="button"
      onClick={() => onSelect(agent.session_id)}
      aria-current={selected ? "true" : undefined}
      className={cx(
        "relative flex h-full w-full min-w-0 flex-col gap-2 px-4 py-3 text-left transition-colors",
        FOCUS_RING_INSET,
        selected ? "bg-surface-selected" : "hover:bg-surface-hover",
        !agent.live && "bg-warning/[0.05]",
      )}
    >
      {agent.attention && (
        <span aria-hidden="true" className="absolute inset-y-0 left-0 w-0.5 bg-warning" />
      )}
      <div className="flex min-w-0 items-center gap-2">
        <FleetStateDot state={agent.state} />
        <span className="shrink-0 text-label font-semibold text-foreground">
          {agent.role === "supervisor" ? "Supervisor" : `Resident · ${resident ?? "?"}`}
        </span>
        <MiddleId
          id={agent.agent_id || agent.session_id}
          className="font-mono text-micro text-faint-foreground"
        />
        <span className="ml-auto shrink-0">
          <LiveBadge live={agent.live} />
        </span>
      </div>

      <dl className="grid grid-cols-[5.5rem_minmax(0,1fr)] gap-x-3 gap-y-1 text-caption">
        <dt className="text-faint-foreground">State</dt>
        <dd className="flex min-w-0 items-center gap-1.5 whitespace-nowrap">
          <span className="text-foreground">{stateLabelUpper(agent.state)}</span>
          <span className="text-muted-foreground">
            for <span className="tnum" title={absoluteTitle(agent.state_since_ms)}>{sinceText(agent.state_since_ms)}</span>
          </span>
          {agent.attention && <span className="font-medium text-warning">· needs a human</span>}
        </dd>

        <dt className="text-faint-foreground">Last activity</dt>
        <dd className="flex min-w-0 items-center gap-1.5">
          {now.tool && <Wrench size={12} className="shrink-0 text-success" />}
          <span className={cx("min-w-0 truncate font-mono", now.tool ? "text-foreground" : "text-muted-foreground")}>
            {now.label}
          </span>
          <Ago ms={now.at} className="shrink-0 text-muted-foreground" />
        </dd>

        <dt className="text-faint-foreground">{agent.live ? "Up" : "Ended"}</dt>
        <dd className="tnum min-w-0 truncate text-muted-foreground">
          {agent.live ? (
            <span title={absoluteTitle(agent.started_at_ms)}>
              {uptime === null ? "—" : formatDuration(uptime)}
            </span>
          ) : (
            <Ago ms={agent.ended_at_ms} className="font-medium text-warning" />
          )}
          {engine && <span className="text-faint-foreground"> · {engine}</span>}
        </dd>

        <dt className="text-faint-foreground">Task</dt>
        <dd className="min-w-0">
          <TaskCell task={held} />
        </dd>
      </dl>
    </button>
  );
}

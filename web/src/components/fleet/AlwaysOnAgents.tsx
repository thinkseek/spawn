import { Infinity as Forever } from "lucide-react";

import type { FleetAgent } from "../../fleet";
import { stateLabelUpper } from "../../fleet";
import { absoluteTitle, formatDuration } from "../../format";
import type { FleetContext, MissingAgent } from "../../ordering";
import { heldTask, residentName } from "../../ordering";
import { LiveBadge } from "../Terminal";
import { Ago, Card, CardHeader, EmptyState, FOCUS_RING_INSET, LabeledId, MiddleId, cx, sinceText } from "../ui";
import { FleetStateDot } from "./bits";
import { DoingIcon, TaskCell, doingNow } from "./FleetBoard";

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
  missing,
  context,
  usualEngine,
  selectedId,
  onSelect,
}: {
  agents: FleetAgent[];
  /** Expected to run but without a session in the fleet: shown as down. */
  missing: MissingAgent[];
  context: FleetContext;
  usualEngine: string | null;
  selectedId: string | null;
  onSelect: (sessionId: string) => void;
}) {
  const up = agents.filter((agent) => agent.live).length;
  const total = agents.length + missing.length;
  return (
    <Card className="shrink-0 overflow-hidden">
      <CardHeader
        icon={<Forever size={14} />}
        title="Always-on agents"
        meta={
          <span className={cx("tnum", up < total && "font-medium text-warning")}>
            {up} of {total} live
          </span>
        }
      />
      {total === 0 ? (
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
          {missing.map((entry) => (
            <li key={`missing:${entry.role}:${entry.name}`} className="min-w-0 bg-surface">
              <div className="h-full bg-warning/[0.05] px-4 py-3">
              <div className="flex min-w-0 items-center gap-2">
                <span aria-hidden="true" className="size-2 shrink-0 rounded-full bg-warning" />
                <span className="shrink-0 text-label font-semibold text-foreground">
                  {entry.role === "supervisor" ? "Supervisor" : `Resident · ${entry.name}`}
                </span>
                <span className="ml-auto shrink-0 text-caption font-medium text-warning">No session</span>
              </div>
              <p className="mt-2 text-caption text-muted-foreground">
                {entry.role === "supervisor"
                  ? "The fleet has no supervisor session."
                  : "Approved, but the fleet has no session for it: never started, or gone."}
              </p>
              </div>
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
      <div className="flex min-w-0 items-start gap-2">
        <span className="mt-1.5 shrink-0">
          <FleetStateDot state={agent.state} />
        </span>
        {agent.label ? (
          <LabeledId
            label={agent.label}
            id={agent.agent_id || agent.session_id}
            className="text-label font-semibold text-foreground"
          />
        ) : (
          <>
            <span className="shrink-0 text-label font-semibold text-foreground">
              {agent.role === "supervisor" ? "Supervisor" : `Resident · ${resident ?? "?"}`}
            </span>
            <MiddleId
              id={agent.agent_id || agent.session_id}
              className="font-mono text-micro text-faint-foreground"
            />
          </>
        )}
        <span className="ml-auto shrink-0">
          <LiveBadge live={agent.live} />
        </span>
      </div>

      <dl className="grid grid-cols-[5.5rem_minmax(0,1fr)] gap-x-3 gap-y-1 text-caption">
        <dt className="text-faint-foreground">State</dt>
        <dd className="flex min-w-0 items-center gap-1.5 overflow-hidden whitespace-nowrap">
          <span className="text-foreground">{stateLabelUpper(agent.state)}</span>
          <span className="text-muted-foreground">
            for <span className="tnum" title={absoluteTitle(agent.state_since_ms)}>{sinceText(agent.state_since_ms)}</span>
          </span>
          <span aria-hidden="true" className="text-faint-foreground">·</span>
          {agent.live ? (
            <span className="tnum text-muted-foreground" title={`Current execution started ${absoluteTitle(agent.started_at_ms) ?? "at an unknown time"}`}>
              up {uptime === null ? "—" : formatDuration(uptime)}
            </span>
          ) : (
            <span className="font-medium text-warning">
              ended <Ago ms={agent.ended_at_ms} />
            </span>
          )}
          {engine && <span className="text-faint-foreground">· {engine}</span>}
          {agent.attention && <span className="font-medium text-warning">· escalated</span>}
        </dd>

        <dt className="text-faint-foreground">Last activity</dt>
        <dd className="flex min-w-0 items-center gap-1.5">
          <DoingIcon now={now} />
          <span className={cx("min-w-0 truncate font-mono", now.tool || now.job ? "text-foreground" : "text-muted-foreground")}>
            {now.label}
          </span>
          <Ago ms={now.at} className="shrink-0 text-muted-foreground" />
        </dd>

        <dt className="text-faint-foreground">Task</dt>
        <dd className="min-w-0">
          <TaskCell task={held} input={agent.role === "supervisor" ? agent.task : null} />
        </dd>
      </dl>
    </button>
  );
}

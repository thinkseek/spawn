import { useMemo } from "react";

import type { PollResult } from "../../api";
import type { Attention } from "../../attention";
import type { Fleet } from "../../fleet";
import { relativeTime } from "../../format";
import type { FleetContext } from "../../ordering";
import { groupAgents, missingAlwaysOn, residentName, usualEngine } from "../../ordering";
import { useNow } from "../rsi/Phase";
import { LiveStatus, PageHeader } from "../ui";
import { AgentDetail } from "./AgentDetail";
import { AlwaysOnAgents, uptimeMs } from "./AlwaysOnAgents";
import { AttentionQueue } from "./AttentionQueue";
import { FleetBoard } from "./FleetBoard";
import { InputBox } from "./InputBox";
import { HealthStrip } from "./HealthStrip";
import type { Liveness } from "./HealthStrip";

interface FleetViewProps {
  /** Its agents marked by what is escalated (App, attention.ts). */
  fleet: PollResult<Fleet>;
  /** The dispatcher's view, `/api/attention` (App polls it). */
  attention: PollResult<Attention>;
  /** Residents and held tasks, from /api/missions (App polls it). */
  context: FleetContext;
  selectedId: string | null;
  onSelect: (sessionId: string | null) => void;
  onOpenTimeline: (sessionId: string) => void;
}

/**
 * The landing page, in the order an observer asks: what is escalated (and,
 * always at the top, a box to type to the supervisor), are the always-on
 * agents alive and moving, are resources fine, then how the rest of the
 * fleet works (on a task, idle, ended). The fleet and attention polls live
 * in App so the sidebar can show the escalations' count from any view.
 */
export function FleetView({
  fleet,
  attention,
  context,
  selectedId,
  onSelect,
  onOpenTimeline,
}: FleetViewProps) {
  // Relative times ("12s ago", "waiting 4m") tick between polls.
  useNow(1_000);
  const data = fleet.data;
  const selected =
    data?.agents.find((agent) => agent.session_id === selectedId) ?? null;
  const updated = data?.health.generated_at_ms ?? null;
  const agents = data?.agents;
  const groups = useMemo(() => groupAgents(agents ?? [], context), [agents, context]);
  // Who the box at the top types to: the dispatcher's supervisor, else the
  // board's, else the one Spawn starts.
  const onBoard = agents?.find((agent) => agent.role === "supervisor") ?? null;
  const supervisor = {
    session:
      attention.data?.supervisor?.session_id ?? onBoard?.session_id ?? "session_supervisor",
    live: attention.data?.supervisor?.live ?? onBoard?.live ?? true,
  };
  const engine = useMemo(() => usualEngine(agents ?? []), [agents]);
  const alwaysOn = groups[0].agents;
  const missing = useMemo(() => missingAlwaysOn(agents ?? [], context), [agents, context]);
  const liveness: Liveness = {
    total: alwaysOn.length + missing.length,
    up: alwaysOn.filter((agent) => agent.live).length,
    shortest: alwaysOn.reduce<Liveness["shortest"]>((best, agent) => {
      const ms = uptimeMs(agent);
      if (ms === null || agent.started_at_ms === null || (best && best.ms <= ms)) return best;
      const name =
        agent.role === "supervisor" ? "supervisor" : `resident ${residentName(agent, context) ?? agent.agent_id}`;
      return { name, ms, since: agent.started_at_ms };
    }, null),
  };

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            Fleet
          </h1>
        }
      >
        <LiveStatus
          error={fleet.error}
          label={
            data
              ? updated
                ? `Updated ${relativeTime(updated)}`
                : "Live"
              : "Connecting"
          }
        />
      </PageHeader>

      {!data ? (
        <div className="grid flex-1 place-items-center">
          <p className="text-caption text-faint-foreground">
            {fleet.error
              ? `Could not reach the fleet API: ${fleet.error}`
              : "Loading the fleet…"}
          </p>
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto flex max-w-[1600px] flex-col gap-3 p-3 sm:p-4">
            <AttentionQueue
              view={attention}
              agents={data.agents}
              onSelect={onSelect}
              talk={
                <InputBox
                  key={supervisor.session}
                  sessionId={supervisor.session}
                  to="the supervisor"
                  stopped={!supervisor.live}
                />
              }
            />
            <AlwaysOnAgents
              agents={alwaysOn}
              missing={missing}
              context={context}
              usualEngine={engine}
              selectedId={selectedId}
              onSelect={onSelect}
            />
            <HealthStrip health={data.health} liveness={liveness} />
            <FleetBoard
              groups={groups}
              total={data.agents}
              context={context}
              usualEngine={engine}
              selectedId={selectedId}
              onSelect={onSelect}
            />
          </div>
        </div>
      )}

      {selected && (
        <AgentDetail
          key={selected.session_id}
          agent={selected}
          onClose={() => onSelect(null)}
          onOpenTimeline={onOpenTimeline}
        />
      )}
    </main>
  );
}

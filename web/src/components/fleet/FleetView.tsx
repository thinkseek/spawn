import { useMemo } from "react";

import type { PollResult } from "../../api";
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
import { HealthStrip } from "./HealthStrip";
import type { Liveness } from "./HealthStrip";

interface FleetViewProps {
  fleet: PollResult<Fleet>;
  /** Residents and held tasks, from /api/missions (App polls it). */
  context: FleetContext;
  selectedId: string | null;
  onSelect: (sessionId: string | null) => void;
  onOpenTimeline: (sessionId: string) => void;
}

/**
 * The landing page, in the order an observer asks: who needs a human, are
 * the always-on agents alive and moving, are resources fine, then how the
 * rest of the fleet works (on a task, idle, ended). The fleet poll lives in
 * App so the sidebar can show the attention count from any view.
 */
export function FleetView({
  fleet,
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
            <AttentionQueue agents={data.agents} onSelect={onSelect} />
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

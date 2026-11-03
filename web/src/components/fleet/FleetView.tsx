import type { PollResult } from "../../api";
import type { Fleet } from "../../fleet";
import { relativeTime } from "../../format";
import { useNow } from "../rsi/Phase";
import { LiveStatus, PageHeader } from "../ui";
import { AgentDetail } from "./AgentDetail";
import { AttentionQueue } from "./AttentionQueue";
import { FleetBoard } from "./FleetBoard";
import { HealthStrip } from "./HealthStrip";

interface FleetViewProps {
  fleet: PollResult<Fleet>;
  selectedId: string | null;
  onSelect: (sessionId: string | null) => void;
  onOpenTimeline: (sessionId: string) => void;
}

/**
 * The landing page: who needs a human, what every agent is doing, and how
 * healthy the decision ladder is. The fleet poll lives in App so the sidebar
 * can show the attention count from any view.
 */
export function FleetView({
  fleet,
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
            <HealthStrip health={data.health} />
            <FleetBoard
              agents={data.agents}
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

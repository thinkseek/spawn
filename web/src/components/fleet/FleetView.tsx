import { useMemo, useState } from "react";
import { ChevronRight } from "lucide-react";

import type { Alert, PollResult } from "../../api";
import { fetchAlertsSince, usePoll } from "../../api";
import type { Attention } from "../../attention";
import type { Fleet } from "../../fleet";
import { relativeTime } from "../../format";
import type { NeedItem } from "../../home";
import { agentNames, agentsNow, nameTask, alertsSpanMs, digestSince, digestWindow, latestNotes, missionsToday, needsSummary } from "../../home";
import type { Missions } from "../../missions";
import type { FleetContext } from "../../ordering";
import { groupAgents, missingAlwaysOn, residentName, usualEngine } from "../../ordering";
import { useNow } from "../rsi/Phase";
import { FOCUS_RING_INSET, LiveStatus, PageHeader, cx } from "../ui";
import { AgentsNowCard, DigestCard, MissionsTodayCard, NeedsYou, SupervisorNotes, useAway } from "../home/Home";
import { AgentDetail } from "./AgentDetail";
import { AlwaysOnAgents, uptimeMs } from "./AlwaysOnAgents";
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
  /** The missions themselves, for the home page's mission rows. */
  missions: Missions | null;
  /** The alert window (App polls it for the band). */
  alerts: Alert[] | null;
  /** "Needs you", computed once in App (its badge counts the same). */
  needs: NeedItem[];
  onShowMissions: () => void;
  selectedId: string | null;
  onSelect: (sessionId: string | null) => void;
  onOpenTimeline: (sessionId: string) => void;
}

/**
 * The home page, in the order the owner asks (design-ui-information.md §3,
 * layout A): does anything need me, is work moving (each mission today),
 * who is working now and on what, what the supervisor said lately (with
 * the box to type to it), and today as a digest. The always-on cards, the
 * health tiles and the whole fleet board are folded under "Every agent and
 * the host" at the bottom. The polls live in App, so the sidebar shows the
 * same counts from any view.
 */
export function FleetView({
  fleet,
  attention,
  context,
  missions,
  alerts,
  needs,
  selectedId,
  onSelect,
  onOpenTimeline,
  onShowMissions,
}: FleetViewProps) {
  const [detailsOpen, setDetailsOpen] = useState(false);
  // Relative times ("12s ago", "waiting 4m") tick between polls.
  useNow(1_000);
  const data = fleet.data;
  const selected =
    data?.agents.find((agent) => agent.session_id === selectedId) ?? null;
  const updated = data?.health.generated_at_ms ?? null;
  const agents = data?.agents;
  const groups = useMemo(() => groupAgents(agents ?? [], context), [agents, context]);
  // One name per agent (design §4.5), the sidebar's and home's.
  const names = useMemo(() => agentNames(agents ?? [], context), [agents, context]);
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

  const now = Date.now();
  // What happened while the reader was away: at least the last 24 h (design §3 Q4).
  const away = useAway();
  const digestSpan = digestWindow(now, away?.from ?? null);
  // The alerts as far back as the digest reads (the shared poll holds the
  // server's 24 h); slow, since news of days ago does not move by the second.
  const span = alertsSpanMs(now, digestSpan.since);
  const digestAlerts = usePoll(`alerts:since:${span}`, (signal) => fetchAlertsSince(span, signal), 60_000);

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            Home
          </h1>
        }
      >
        <span className="tnum hidden text-caption text-muted-foreground sm:inline">
          {needsSummary(needs)}
        </span>
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
          <div className="mx-auto flex max-w-[1200px] flex-col gap-3 p-3 sm:p-4">
            <NeedsYou items={needs} onOpenAgent={(session) => onSelect(session)} />
            <MissionsTodayCard model={missionsToday(missions, now)} onOpen={onShowMissions} />
            <AgentsNowCard model={agentsNow(agents ?? null, context)} onOpenAgent={(session) => onSelect(session)} />
            <SupervisorNotes
              model={latestNotes(attention.data, now)}
              all={[...(attention.data?.supervisor?.for_owner ?? [])].sort((left, right) => (right.at_ms ?? 0) - (left.at_ms ?? 0))}
              talk={
                <InputBox
                  key={supervisor.session}
                  sessionId={supervisor.session}
                  to="the supervisor"
                  stopped={!supervisor.live}
                />
              }
            />
            <DigestCard
              digest={digestSince(missions, digestAlerts.data ?? alerts, now, digestSpan.since, agents ?? null, digestSpan.away)}
              back={away?.at ?? null}
            />
            <button
              type="button"
              onClick={() => setDetailsOpen((value) => !value)}
              aria-expanded={detailsOpen}
              className={cx(
                "mt-2 flex items-center gap-1.5 self-start rounded-control px-2 py-1 text-caption font-medium text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                FOCUS_RING_INSET,
              )}
            >
              <ChevronRight
                size={12}
                aria-hidden="true"
                className={cx("transition-transform motion-reduce:transition-none", detailsOpen && "rotate-90")}
              />
              Every agent and the host
              <span className="tnum font-normal text-faint-foreground">
                {data.agents.length} agents · health · resources
              </span>
            </button>
            {detailsOpen && (
              <>
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
              </>
            )}
          </div>
        </div>
      )}

      {selected && (
        <AgentDetail
          key={selected.session_id}
          agent={selected}
          name={names.get(selected.session_id) ?? null}
          held={nameTask(selected, context)}
          onClose={() => onSelect(null)}
          onOpenTimeline={onOpenTimeline}
        />
      )}
    </main>
  );
}

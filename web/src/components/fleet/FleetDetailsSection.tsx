import type { Alert, PollResult } from "../../api";
import { fetchAlertsSince, usePoll } from "../../api";
import type { Attention } from "../../attention";
import { coreLinesView } from "../../coreLines";
import type { Fleet, FleetAgent } from "../../fleet";
import {
  alertsSpanMs,
  allNotes,
  digestSince,
  digestWindow,
  latestNotes,
} from "../../home";
import { figureState } from "../../interventions.ts";
import type { Missions } from "../../missions";
import type { AgentGroup, FleetContext, Liveness } from "../../ordering";
import { missingAlwaysOn, usualEngine } from "../../ordering";
import { CORE_LINES_POLL_MS, fetchCoreLines } from "../home/CoreLines";
import { HomeRuntimeDetails } from "../home/HomeRuntimeDetails";
import { AlwaysOnAgents } from "./AlwaysOnAgents";
import { DigestPanel } from "./DigestPanel";
import { FleetBoard } from "./FleetBoard";
import { HealthStrip } from "./HealthStrip";
import { InputBox } from "./InputBox";
import { INTERVENTIONS_POLL_MS, fetchInterventions } from "./Interventions";
import { NodeRows } from "./NodeRows";

export interface FleetDetailsSectionProps {
  now: number;
  data: Fleet;
  attention: PollResult<Attention>;
  context: FleetContext;
  missions: Missions | null;
  alerts: Alert[] | null;
  agents: Fleet["agents"] | undefined;
  awayFromMs: number | null;
  awayAtMs: number | null;
  supervisor: { session: string; live: boolean };
  alwaysOn: Fleet["agents"];
  missing: ReturnType<typeof missingAlwaysOn>;
  liveness: Liveness;
  groups: AgentGroup<FleetAgent>[];
  engine: ReturnType<typeof usualEngine>;
  selectedId: string | null;
  onSelect: (sessionId: string | null) => void;
}

export function FleetDetailsSection({
  now,
  data,
  attention,
  context,
  missions,
  alerts,
  agents,
  awayFromMs,
  awayAtMs,
  supervisor,
  alwaysOn,
  missing,
  liveness,
  groups,
  engine,
  selectedId,
  onSelect,
}: FleetDetailsSectionProps) {
  const digestSpan = digestWindow(now, awayFromMs);
  const span = alertsSpanMs(now, digestSpan.since);
  const digestAlerts = usePoll(`alerts:since:${span}`, (signal) => fetchAlertsSince(span, signal), 60_000);
  const day = usePoll("interventions-24h", (signal) => fetchInterventions("24h", signal), INTERVENTIONS_POLL_MS);
  const week = usePoll("interventions-7d", (signal) => fetchInterventions("7d", signal), INTERVENTIONS_POLL_MS);
  const coreLines = usePoll("core-lines", fetchCoreLines, CORE_LINES_POLL_MS);

  return (
    <>
      <DigestPanel />
      <HomeRuntimeDetails
        now={now}
        notes={latestNotes(attention.data, now)}
        allNotes={allNotes(attention.data)}
        digest={digestSince(
          missions,
          digestAlerts.data ?? alerts,
          now,
          digestSpan.since,
          agents ?? null,
          digestSpan.away,
        )}
        back={awayAtMs}
        interventions={{
          day: figureState(day.data, day.error),
          week: figureState(week.data, week.error),
        }}
        coreLines={{
          answer: coreLines.data,
          error: coreLines.data ? null : coreLines.error,
        }}
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
      <HealthStrip
        health={data.health}
        liveness={liveness}
        diagnostics={coreLines.data ? coreLinesView(coreLines.data).diagnostics : []}
      />
      <NodeRows fleetNodes={data.nodes} />
      <FleetBoard
        groups={groups}
        total={data.agents}
        context={context}
        usualEngine={engine}
        selectedId={selectedId}
        onSelect={onSelect}
      />
    </>
  );
}

export default FleetDetailsSection;

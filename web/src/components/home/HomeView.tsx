import type { ReactNode } from "react";

import type { CoreLinesAnswer } from "../../coreLines.ts";
import type {
  AgentsNow,
  Digest,
  MissionProgressSummary,
  NeedItem,
  NoteRow,
  NotesShown,
  SystemStatusModel,
} from "../../home";
import type { FigureState } from "../../interventions.ts";
import { InterventionsCard } from "../fleet/Interventions";
import { CoreLinesCard } from "./CoreLines";
import {
  AgentsNowCard,
  DigestCard,
  MissionCompletionCard,
  NeedsYou,
  SupervisorNotes,
  SystemStatusHero,
} from "./Home";

/**
 * Focused Home workspace: System Status + Mission Completion first, with
 * actionable items & approvals surfaced cleanly only when present.
 */
export interface HomeData {
  now: number;
  systemStatus: SystemStatusModel;
  missionProgress: MissionProgressSummary;
  freshnessLevel: "fresh" | "aging" | "frozen";
  freshnessLabel: string;
  actionableNeeds: NeedItem[];
  approvals?: ReactNode;
  masterControl?: ReactNode;
  agents: AgentsNow;
  onOpenAgent: (session: string) => void;
  onShowMissions: () => void;
  onShowAgents: () => void;
  onShowFeatures: () => void;
}

export interface HomeRuntimeDetailsProps {
  now: number;
  notes: NotesShown;
  allNotes: NoteRow[];
  digest: Digest;
  back: number | null;
  interventions: { day: FigureState; week: FigureState };
  coreLines: { answer: CoreLinesAnswer | null; error: string | null };
  talk: ReactNode;
}

export function HomeView(props: HomeData) {
  return (
    <>
      <SystemStatusHero
        status={props.systemStatus}
        missionProgress={props.missionProgress}
        freshnessLevel={props.freshnessLevel}
        freshnessLabel={props.freshnessLabel}
        onShowMissions={props.onShowMissions}
        onShowAgents={props.onShowAgents}
      />
      {props.actionableNeeds.length > 0 && (
        <NeedsYou
          items={props.actionableNeeds}
          onOpenAgent={props.onOpenAgent}
          showJudgeHint={false}
        />
      )}
      {props.approvals}
      <MissionCompletionCard
        model={props.missionProgress}
        onShowMissions={props.onShowMissions}
        onShowFeatures={props.onShowFeatures}
      />
      <AgentsNowCard model={props.agents} onOpenAgent={props.onOpenAgent} />
      {props.masterControl}
    </>
  );
}

/** Secondary supervisor notes, digest, interventions, and core-lines telemetry,
 * tucked inside Home's expandable details section so the primary screen stays sharp. */
export function HomeRuntimeDetails(props: HomeRuntimeDetailsProps) {
  return (
    <div className="flex flex-col gap-3">
      <SupervisorNotes model={props.notes} all={props.allNotes} talk={props.talk} />
      <DigestCard digest={props.digest} back={props.back} />
      <InterventionsCard day={props.interventions.day} week={props.interventions.week} />
      <CoreLinesCard
        answer={props.coreLines.answer}
        error={props.coreLines.error}
        now={props.now}
      />
    </div>
  );
}

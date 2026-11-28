import type { ReactNode } from "react";

import type {
  AgentsNow,
  MissionProgressSummary,
  NeedItem,
  SystemStatusModel,
} from "../../home";
import {
  AgentsNowCard,
  MissionCompletionCard,
  NeedsYou,
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

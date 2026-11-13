import type { ReactNode } from "react";

import type { CoreLinesAnswer } from "../../coreLines.ts";
import type { AgentsNow, Digest, MissionsToday, NeedItem, NoteRow, NotesShown } from "../../home";
import type { FigureState } from "../../interventions.ts";
import { InterventionsCard } from "../fleet/Interventions";
import { CoreLinesCard } from "./CoreLines";
import { AgentsNowCard, DigestCard, MissionsTodayCard, NeedsYou, SupervisorNotes } from "./Home";

/**
 * What Home shows (objective 120), each made by Spawn's pure modules
 * (home.ts, interventions.ts, coreLines.ts) from what FleetView and App
 * poll. HomeView renders from these alone.
 */
export interface HomeData {
  /** The time the models were made for (ms). */
  now: number;
  needs: NeedItem[];
  missions: MissionsToday;
  agents: AgentsNow;
  notes: NotesShown;
  allNotes: NoteRow[];
  digest: Digest;
  /** When the reader came back (useAway), or null. */
  back: number | null;
  /** figureState of /api/interventions for 24h and 7d. */
  interventions: { day: FigureState; week: FigureState };
  /** /api/core-lines: both null while the first read is out. */
  coreLines: { answer: CoreLinesAnswer | null; error: string | null };
  /** The box that types to the supervisor. */
  talk: ReactNode;
  onOpenAgent: (session: string) => void;
  onShowMissions: () => void;
}

/**
 * Home's blocks, in the order the owner asks (design-ui-information.md §3,
 * layout A), on affectUI's components: from its props only, with no fetch,
 * no poll and no effect needed for its first paint, so it renders on a
 * server as in the browser.
 */
export function HomeView(props: HomeData) {
  return (
    <>
      <NeedsYou items={props.needs} onOpenAgent={props.onOpenAgent} />
      <MissionsTodayCard model={props.missions} onOpen={props.onShowMissions} />
      <AgentsNowCard model={props.agents} onOpenAgent={props.onOpenAgent} />
      <SupervisorNotes model={props.notes} all={props.allNotes} talk={props.talk} />
      <DigestCard digest={props.digest} back={props.back} />
      <InterventionsCard day={props.interventions.day} week={props.interventions.week} />
      <CoreLinesCard answer={props.coreLines.answer} error={props.coreLines.error} now={props.now} />
    </>
  );
}

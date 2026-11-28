import type { ReactNode } from "react";

import type { CoreLinesAnswer } from "../../coreLines.ts";
import type {
  Digest,
  NoteRow,
  NotesShown,
} from "../../home";
import type { FigureState } from "../../interventions.ts";
import { InterventionsCard } from "../fleet/Interventions";
import { CoreLinesCard } from "./CoreLines";
import { DigestCard, SupervisorNotes } from "./Home";

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

/** Secondary supervisor notes, digest, interventions, and core-lines telemetry,
 * loaded only when Home's expandable details section is opened. */
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

export default HomeRuntimeDetails;

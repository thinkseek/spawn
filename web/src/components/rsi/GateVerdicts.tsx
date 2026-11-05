import { Gavel, GanttChart, Table2 } from "lucide-react";
import { Suspense, lazy, useMemo } from "react";

import { usePoll } from "../../api";
import type { VerdictReasons } from "../../missionTimeline";
import { fetchMissions } from "../../missions";
import type { Missions } from "../../missions";
import type { RsiMission } from "../../rsi";
import { verdictsOf } from "../../rsi";
import { Card, CardHeader, EmptyState, cx } from "../ui";
import { ViewToggle, useStoredChoice } from "../ViewToggle";
import type { ViewOption } from "../ViewToggle";
import { VerdictList } from "./Verdicts";

// Loaded when first shown: it carries Motion and d3.
const MissionTimeline = lazy(() =>
  import("../missions/MissionTimeline").then((module) => ({
    default: module.MissionTimeline,
  })),
);

type Mode = "timeline" | "table";
const MODES = ["timeline", "table"] as const;
const MODE_OPTIONS: ReadonlyArray<ViewOption<Mode>> = [
  { id: "timeline", label: "Timeline", icon: <GanttChart size={13} /> },
  { id: "table", label: "Table", icon: <Table2 size={13} /> },
];

/** The mission's gate verdicts: its tasks on a time axis, each verdict a
 * mark on its task's bar and each send-back a return arrow, or the table
 * of verdicts, newest first. The timeline reads the task stages from
 * /api/missions and each verdict's reason from /api/rsi. */
export function GateVerdicts({
  mission,
  selected,
  onSelect,
  className,
}: {
  mission: RsiMission;
  selected: number | null;
  onSelect: (task: number) => void;
  className?: string;
}) {
  const [mode, choose] = useStoredChoice<Mode>("spawn.rsi.verdicts", MODES, "timeline");
  const toggle = (
    <ViewToggle label="Gate verdicts view" options={MODE_OPTIONS} value={mode} onChange={choose} />
  );
  const verdicts = useMemo(() => verdictsOf(mission), [mission]);
  const missions = usePoll<Missions>("missions", fetchMissions, 5_000);
  const board = missions.data?.missions.find((entry) => entry.id === mission.id) ?? null;
  const shown = useMemo(() => (board ? [board] : []), [board]);
  const reasons = useMemo<VerdictReasons>(
    () =>
      new Map(
        mission.tasks.map((task) => [
          task.id,
          task.gates.map((gate) => gate.reason ?? gate.metric ?? null),
        ]),
      ),
    [mission],
  );

  if (mode === "table") {
    return (
      <VerdictList
        className={cx("max-h-[28rem]", className)}
        verdicts={verdicts}
        selected={selected}
        onSelect={onSelect}
        action={toggle}
      />
    );
  }

  const accepted = verdicts.filter((entry) => entry.verdict === "accepted").length;
  return (
    <Card className={cx("flex flex-col overflow-hidden", className)}>
      <CardHeader
        icon={<Gavel size={14} />}
        title="Gate verdicts"
        meta={
          <>
            {verdicts.length > 0 && (
              <span className="tnum">
                {verdicts.length} total
                <span className="mx-1.5 text-faint-foreground">·</span>
                <span className={accepted > 0 ? "text-success" : undefined}>
                  {accepted} accepted
                </span>
              </span>
            )}
            {toggle}
          </>
        }
      />
      {!board ? (
        <EmptyState
          title={missions.error ? "Could not load the task stages" : "Loading the task stages…"}
          hint={missions.error ?? undefined}
        />
      ) : board.tasks.length === 0 ? (
        <EmptyState
          title="No tasks yet"
          hint="Tasks appear here once the integrator assigns one."
        />
      ) : (
        <div className="flex h-[26rem] min-h-0 flex-col">
          <Suspense fallback={<div className="min-h-0 flex-1" />}>
            <MissionTimeline
              missions={shown}
              header={null}
              reasons={reasons}
              compact
              selectedTask={selected}
              onSelectTask={onSelect}
            />
          </Suspense>
        </div>
      )}
    </Card>
  );
}

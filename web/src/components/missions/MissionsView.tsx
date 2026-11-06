import {
  Ban,
  Check,
  ChevronRight,
  GanttChart,
  Hourglass,
  LayoutList,
  OctagonAlert,
  RotateCcw,
  Target,
  Terminal,
  Users,
} from "lucide-react";
import { Suspense, lazy, useState } from "react";
import type { ReactNode } from "react";

import type { PollResult } from "../../api";
import { absoluteTitle, relativeTime, stageDuration } from "../../format";
import type {
  Mission,
  MissionTask,
  Missions,
  Resident,
  TaskState,
} from "../../missions";
import {
  approveCommand,
  endedWell,
  pipelineOf,
  slowStages,
  stageStep,
} from "../../missions";
import type { TaskGroup, TaskSection } from "../../ordering";
import {
  SECTION_LABEL,
  groupTasks,
  missionClass,
  missionName,
  orderMissions,
  taskTitle,
} from "../../ordering";
import { Fact, SectionLabel } from "../fleet/bits";
import { VerdictBadge, useNow } from "../rsi/Phase";
import { Ago, Card, CardHeader, EmptyState, FOCUS_RING, GroupHeader, LiveStatus, PageHeader, cx } from "../ui";
import { HeldSources } from "./HeldSources";
import { Markdown } from "./Markdown";
import { AgentLink, Collapsible } from "./parts";
import { ViewToggle, useStoredChoice } from "../ViewToggle";
import type { ViewOption } from "../ViewToggle";

// The timeline carries Motion and d3: loaded when first shown, so the first
// page stays light.
const MissionTimeline = lazy(() =>
  import("./MissionTimeline").then((module) => ({ default: module.MissionTimeline })),
);

type Mode = "timeline" | "cards";
const MODES = ["timeline", "cards"] as const;
const MODE_OPTIONS: ReadonlyArray<ViewOption<Mode>> = [
  { id: "timeline", label: "Timeline", icon: <GanttChart size={13} /> },
  { id: "cards", label: "Cards", icon: <LayoutList size={13} /> },
];

/**
 * Missions and the residents that work them. Strictly read-only: the only
 * request is GET /api/missions (polled by App, which shares it with the
 * sidebar and the fleet board), and the only "actions" are links into the
 * fleet view and commands a human copies into a terminal.
 */
export function MissionsView({ missions }: { missions: PollResult<Missions> }) {
  useNow(1_000);
  const data = missions.data;

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            Missions
          </h1>
        }
      >
        <LiveStatus
          error={missions.error}
          label={
            data
              ? `${data.missions.length} mission${data.missions.length === 1 ? "" : "s"}`
              : "Connecting"
          }
        />
      </PageHeader>

      {!data ? (
        <div className="grid flex-1 place-items-center">
          <p className="text-caption text-faint-foreground">
            {missions.error
              ? `Could not reach the missions API: ${missions.error}`
              : "Loading missions…"}
          </p>
        </div>
      ) : (
        <MissionsBody data={data} />
      )}
    </main>
  );
}

/** The page body for one snapshot of /api/missions. */
export function MissionsBody({ data }: { data: Missions }) {
  const [mode, choose] = useStoredChoice<Mode>("spawn.missions.mode", MODES, "timeline");
  const toggle = (
    <ViewToggle label="Missions view" options={MODE_OPTIONS} value={mode} onChange={choose} />
  );
  if (mode === "timeline") {
    const proposed = data.residents.some((resident) => resident.state === "proposed");
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {proposed && (
          <div className="shrink-0 px-3 pt-3 sm:px-4 sm:pt-4">
            <ApprovalQueue residents={data.residents} />
          </div>
        )}
        {data.holds.length > 0 && (
          <div className="shrink-0 px-3 pt-3 sm:px-4 sm:pt-4">
            <SourceHolds data={data} />
          </div>
        )}
        {data.missions.length === 0 ? (
          <div className="flex flex-col">
            <div className="px-4 pt-2 sm:px-5">{toggle}</div>
            <EmptyState
              icon={<Target size={20} />}
              title="No missions yet"
              hint="Missions appear here once the supervisor records one."
            />
          </div>
        ) : (
          <Suspense fallback={<div className="min-h-0 flex-1" />}>
            <MissionTimeline missions={data.missions} header={toggle} />
          </Suspense>
        )}
      </div>
    );
  }
  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto flex max-w-[1600px] flex-col gap-3 p-3 sm:p-4">
        <div>{toggle}</div>
        <ApprovalQueue residents={data.residents} />
        <SourceHolds data={data} />
        <div className="grid min-w-0 gap-3 xl:grid-cols-[minmax(0,1fr)_340px]">
          <div className="flex min-w-0 flex-col gap-3">
            {data.missions.length === 0 ? (
              <Card>
                <EmptyState
                  icon={<Target size={20} />}
                  title="No missions yet"
                  hint="Missions appear here once the supervisor records one."
                />
              </Card>
            ) : (
              orderMissions(data.missions).map((mission, index) => (
                <MissionCard
                  key={mission.id ?? `index-${index}`}
                  mission={mission}
                />
              ))
            )}
          </div>
          <ResidentRoster residents={data.residents} />
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Residents

/** The owner's holds on mission sources: landings there wait. */
function SourceHolds({ data }: { data: Missions }) {
  return (
    <HeldSources
      holds={data.holds}
      title="Sources held by the owner"
      hint="no task lands there until the hold is released or expires; gates, assignment and other sources go on"
    />
  );
}

/** Proposed residents block on a human; they lead the page. */
function ApprovalQueue({ residents }: { residents: Resident[] }) {
  const proposed = residents.filter((resident) => resident.state === "proposed");
  if (proposed.length === 0) return null;
  return (
    <section
      aria-label="Residents waiting for human approval"
      className="rounded-lg border border-warning/30 bg-warning/[0.07]"
    >
      <header className="flex items-center gap-2 border-b border-warning/20 px-4 py-2.5">
        <Hourglass size={15} className="shrink-0 text-warning" />
        <h2 className="text-label font-medium text-foreground">
          {proposed.length} resident{proposed.length === 1 ? "" : "s"} waiting
          for human approval
        </h2>
      </header>
      <ul className="divide-y divide-warning/15">
        {proposed.map((resident, index) => (
          <li
            key={resident.name ?? `index-${index}`}
            className="space-y-2 px-4 py-3"
          >
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-label font-semibold text-foreground">
                {resident.name ?? "unnamed resident"}
              </span>
              <ResidentStateBadge resident={resident} />
              {resident.proposed_by && (
                <span className="text-caption text-muted-foreground">
                  proposed by {resident.proposed_by}
                </span>
              )}
              <span className="tnum ml-auto text-micro text-faint-foreground">
                <Ago ms={resident.created_at_ms} />
              </span>
            </div>
            {resident.charter && (
              <Markdown source={resident.charter} className="text-muted-foreground" />
            )}
            <CommandLine command={approveCommand(resident.name)} />
          </li>
        ))}
      </ul>
    </section>
  );
}

function CommandLine({ command }: { command: string }) {
  return (
    <div className="flex min-w-0 items-center gap-2 rounded-md border border-surface-border bg-surface px-2.5 py-1.5">
      <Terminal size={12} className="shrink-0 text-faint-foreground" />
      <code
        title="Run this in a terminal to approve"
        className="min-w-0 flex-1 overflow-x-auto whitespace-nowrap font-mono text-caption text-foreground select-all"
      >
        {command}
      </code>
    </div>
  );
}

const RESIDENT_STYLE: Record<Resident["state"], string> = {
  proposed: "bg-warning/15 text-warning ring-warning/30",
  approved: "bg-success/12 text-success ring-success/25",
  retired: "bg-muted text-faint-foreground ring-surface-border",
  unknown: "bg-transparent text-faint-foreground ring-surface-border",
};

function ResidentStateBadge({ resident }: { resident: Resident }) {
  const label =
    resident.state === "proposed"
      ? "waiting for human approval"
      : resident.state === "unknown"
        ? (resident.raw_state ?? "unknown")
        : resident.state;
  return (
    <span
      className={cx(
        "inline-flex h-5 shrink-0 items-center rounded-md px-1.5 text-micro font-medium ring-1 ring-inset",
        RESIDENT_STYLE[resident.state],
      )}
    >
      {label}
    </span>
  );
}

function ResidentRoster({ residents }: { residents: Resident[] }) {
  return (
    <Card className="h-fit min-w-0">
      <CardHeader
        icon={<Users size={14} />}
        title="Residents"
        meta={<span className="tnum">{residents.length || ""}</span>}
      />
      {residents.length === 0 ? (
        <EmptyState
          title="No residents"
          hint="Long-lived agents with a charter appear here once proposed."
        />
      ) : (
        <ul className="divide-y divide-surface-border">
          {residents.map((resident, index) => (
            <li
              key={resident.name ?? `index-${index}`}
              className={cx(
                "space-y-1.5 px-4 py-3",
                resident.state === "proposed" && "bg-warning/[0.05]",
                resident.state === "retired" && "opacity-60",
              )}
            >
              <div className="flex min-w-0 items-center gap-2">
                <span className="min-w-0 truncate text-label font-medium text-foreground">
                  {resident.name ?? "unnamed resident"}
                </span>
                <span className="ml-auto">
                  <ResidentStateBadge resident={resident} />
                </span>
              </div>
              {resident.charter ? (
                <Markdown
                  source={resident.charter}
                  className="text-muted-foreground"
                />
              ) : (
                <p className="text-caption text-faint-foreground">No charter.</p>
              )}
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-micro text-faint-foreground">
                {resident.proposed_by && (
                  <span>proposed by {resident.proposed_by}</span>
                )}
                {resident.approved_by && (
                  <span>approved by {resident.approved_by}</span>
                )}
                <AgentLink
                  sessionId={resident.session_id}
                  agentId={resident.agent_id}
                />
              </div>
              {resident.state === "proposed" && (
                <CommandLine command={approveCommand(resident.name)} />
              )}
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Missions

const MISSION_STATE_STYLE: Record<string, string> = {
  active: "bg-info/12 text-info ring-info/25",
  done: "bg-success/12 text-success ring-success/25",
  completed: "bg-success/12 text-success ring-success/25",
  accepted: "bg-success/12 text-success ring-success/25",
  failed: "bg-destructive/12 text-destructive ring-destructive/25",
  cancelled: "bg-muted text-faint-foreground ring-surface-border",
};

function Pill({
  className,
  children,
  title,
}: {
  className?: string;
  children: ReactNode;
  title?: string;
}) {
  return (
    <span
      title={title}
      className={cx(
        "inline-flex h-5 shrink-0 items-center gap-1 rounded-md px-1.5 text-micro font-medium ring-1 ring-inset",
        className ?? "bg-muted text-muted-foreground ring-surface-border",
      )}
    >
      {children}
    </span>
  );
}

function MissionCard({ mission }: { mission: Mission }) {
  const state = mission.state ?? "unknown";
  const title = firstLine(mission.text) ?? "Untitled mission";
  const counts = countStates(mission.tasks);
  // Closed missions fold to their header; the rest open.
  const [open, setOpen] = useState(() => missionClass(mission) !== "closed");
  return (
    <Card className={cx("min-w-0", !open && "bg-surface/60")}>
      <header
        className={cx(
          "flex flex-wrap items-center gap-2 px-4 py-2.5",
          open && "border-b border-surface-border",
        )}
      >
        <button
          type="button"
          aria-expanded={open}
          onClick={() => setOpen(!open)}
          className={cx("flex min-w-0 flex-1 items-center gap-2 rounded text-left", FOCUS_RING)}
        >
          <ChevronRight
            size={13}
            aria-hidden="true"
            className={cx(
              "shrink-0 text-faint-foreground transition-transform duration-150 motion-reduce:transition-none",
              open && "rotate-90",
            )}
          />
          <span className="tnum font-mono text-caption text-faint-foreground">
            #{mission.id ?? "?"}
          </span>
          <h2 className="min-w-0 flex-1 truncate text-label font-medium text-foreground">
            {title}
          </h2>
        </button>
        {!open && (
          <span className="tnum text-caption text-muted-foreground">
            {mission.tasks.length} task{mission.tasks.length === 1 ? "" : "s"}
            {counts.landed + counts.accepted > 0 && ` · ${counts.landed + counts.accepted} landed or accepted`}
          </span>
        )}
        <Pill className={MISSION_STATE_STYLE[state.toLowerCase()]}>{state}</Pill>
      </header>

      {open && (
      <>
      <div className="space-y-3 px-4 py-3">
        <div className="grid grid-cols-2 gap-x-4 gap-y-2 sm:grid-cols-4">
          <Fact label="Origin">{mission.origin ?? "—"}</Fact>
          <Fact label="Created">
            <span title={absolute(mission.created_at_ms)}>
              {relativeTime(mission.created_at_ms)}
            </span>
          </Fact>
          <Fact label="Updated">
            <span title={absolute(mission.updated_at_ms)}>
              {relativeTime(mission.updated_at_ms)}
            </span>
          </Fact>
          <Fact label="Tasks">
            <span className="tnum">
              {mission.tasks.length}
              {counts.accepted > 0 && ` · ${counts.accepted} accepted`}
              {counts.landed > 0 && ` · ${counts.landed} landed`}
              {counts.cancelled > 0 && ` · ${counts.cancelled} cancelled`}
            </span>
          </Fact>
        </div>

        {mission.halted && (
          <div className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/[0.08] px-3 py-2">
            <OctagonAlert size={14} className="mt-0.5 shrink-0 text-destructive" />
            <div className="min-w-0 text-label">
              <span className="font-medium text-destructive">Halted</span>
              <span className="text-foreground/85"> — {mission.halted}</span>
              <div className="text-caption text-muted-foreground">
                Nothing lands until a human runs{" "}
                <code className="font-mono">spawn mission resume --id {mission.id ?? "N"}</code>
              </div>
            </div>
          </div>
        )}

        {mission.hold !== null && (
          <div className="flex items-start gap-2 rounded-md border border-border bg-surface/60 px-3 py-2">
            <Hourglass size={14} className="mt-0.5 shrink-0 text-muted-foreground" />
            <div className="min-w-0 text-label">
              <span className="font-medium text-foreground">Landings held</span>
              <span className="text-foreground/85">
                {" "}
                — the owner holds its source (hold #{mission.hold}): its gates still run and an
                accepted task waits to land until the hold is released or expires
              </span>
            </div>
          </div>
        )}

        {mission.gate && (
          <div className="grid grid-cols-2 gap-x-4 gap-y-2 sm:grid-cols-4">
            <Fact label="Gate">
              <span className="font-mono" title={mission.gate}>
                {shortPath(mission.gate)}
              </span>
            </Fact>
            <Fact label="Accept">
              {mission.accept === "gate"
                ? "gate alone"
                : mission.accept === "review"
                  ? "review, then gate"
                  : (mission.accept ?? "—")}
            </Fact>
            <Fact label="Baseline">
              <span className="font-mono" title={mission.baseline ?? undefined}>
                {mission.baseline ? mission.baseline.slice(0, 10) : "—"}
              </span>
            </Fact>
            <Fact label="Budget">
              <span className="tnum">{budgetText(mission.budget)}</span>
            </Fact>
            {mission.objectives && (
              <>
                <Fact label="Backlog">
                  <span
                    className="font-mono"
                    title={mission.objectives.backlog ?? undefined}
                  >
                    {mission.objectives.backlog
                      ? shortPath(mission.objectives.backlog)
                      : "—"}
                  </span>
                </Fact>
                <Fact label="Resident">{mission.objectives.resident ?? "—"}</Fact>
              </>
            )}
          </div>
        )}

        {mission.summary && (
          <div className="rounded-md border border-success/20 bg-success/[0.05] px-3 py-2">
            <SectionLabel>Summary</SectionLabel>
            <Markdown source={mission.summary} className="mt-1" />
          </div>
        )}

        {mission.text && (
          <Collapsible label="Mission text">
            <Markdown source={mission.text} />
          </Collapsible>
        )}

        {mission.sources.length > 0 && (
          <div>
            <SectionLabel>Sources</SectionLabel>
            <ul className="mt-1 space-y-0.5">
              {mission.sources.map((source, index) => (
                <li
                  key={`${source}-${index}`}
                  className="truncate font-mono text-micro text-muted-foreground"
                  title={source}
                >
                  {source}
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>

      <div className="border-t border-surface-border">
        <div className="px-4 pt-3">
          <SectionLabel trailing={<span className="tnum">{mission.tasks.length || ""}</span>}>
            Tasks
          </SectionLabel>
        </div>
        {mission.tasks.length === 0 ? (
          <p className="px-4 py-3 text-caption text-faint-foreground">
            No tasks dispatched yet.
          </p>
        ) : (
          <TaskSections mission={mission} />
        )}
      </div>
      </>
      )}
    </Card>
  );
}

/** A mission's tasks in the shared sections (ordering.ts): in flight,
 * landed or accepted, sent back, cancelled (folded). Retries of one
 * objective show as one row, earlier attempts nested under it. */
function TaskSections({ mission }: { mission: Mission }) {
  const [toggled, setToggled] = useState<ReadonlySet<TaskSection>>(() => new Set());
  const name = missionName(mission);
  return (
    <div className="pt-1">
      {groupTasks(mission.tasks, (task) => task, mission).map(({ section, groups }) => {
        const folded = (section === "cancelled") !== toggled.has(section);
        return (
          <section key={section} aria-label={SECTION_LABEL[section]}>
            <GroupHeader
              className="border-t border-surface-border bg-page-canvas/40 px-4 py-1.5 first:border-t-0"
              label={SECTION_LABEL[section]}
              count={groups.length}
              expanded={!folded}
              onToggle={() =>
                setToggled((current) => {
                  const next = new Set(current);
                  if (next.has(section)) next.delete(section);
                  else next.add(section);
                  return next;
                })
              }
            />
            {!folded && (
              <ul className="divide-y divide-surface-border border-t border-surface-border">
                {groups.map((group) => (
                  <TaskRow key={group.key} task={group.task} mission={mission} missionName={name} group={group} />
                ))}
              </ul>
            )}
          </section>
        );
      })}
    </div>
  );
}

/** Earlier attempts at the row's objective, newest first, each with how it
 * ended. */
function EarlierAttempts({ group, missionName }: { group: TaskGroup<MissionTask>; missionName: string }) {
  if (group.earlier.length === 0) return null;
  return (
    <div className="rounded-md border border-surface-border bg-page-canvas/40 px-3 py-2">
      <SectionLabel>
        Earlier attempt{group.earlier.length === 1 ? "" : "s"} · {group.earlier.length}
      </SectionLabel>
      <ul className="mt-1 space-y-1">
        {group.earlier.map((attempt, index) => (
          <li
            key={attempt.task.id ?? `attempt-${index}`}
            className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5 border-l border-border-strong pl-2.5 text-caption"
          >
            <span className="tnum shrink-0 font-mono text-micro text-faint-foreground">
              T{attempt.task.id ?? "?"}
            </span>
            <span
              className={cx(
                "shrink-0 font-medium",
                attempt.outcome === "landed" || attempt.outcome === "accepted"
                  ? "text-success"
                  : attempt.outcome.startsWith("aborted")
                    ? "text-destructive"
                    : "text-warning",
              )}
            >
              {attempt.outcome}
            </span>
            <span className="min-w-0 flex-1 truncate text-muted-foreground">
              {taskTitle(attempt.task, { missionName }) ?? ""}
            </span>
            <AgentLink sessionId={attempt.task.session_id} agentId={attempt.task.agent_id} />
            <Ago ms={attempt.task.updated_at_ms} className="text-micro text-faint-foreground" />
          </li>
        ))}
      </ul>
    </div>
  );
}

function TaskRow({
  task,
  mission,
  missionName,
  group,
}: {
  task: MissionTask;
  mission: Mission;
  missionName: string;
  group: TaskGroup<MissionTask>;
}) {
  const title = taskTitle(task, { missionName });
  return (
    <li
      className={cx(
        "space-y-2.5 px-4 py-3",
        task.state === "cancelled" && "bg-muted/30",
      )}
    >
      <div className="flex min-w-0 items-baseline gap-2">
        <span className="tnum shrink-0 font-mono text-caption text-faint-foreground">
          T{task.id ?? "?"}
        </span>
        <h3
          className="min-w-0 flex-1 truncate text-label font-medium text-foreground"
          title={title ?? undefined}
        >
          {title ?? "Untitled task"}
        </h3>
        {group.earlier.length > 0 && (
          <span
            className="flex shrink-0 items-center gap-1 text-caption text-warning"
            title={`Attempt ${group.earlier.length + 1} at objective ${task.objective ?? "?"}`}
          >
            <RotateCcw size={11} aria-hidden="true" />
            attempt {group.earlier.length + 1}
          </span>
        )}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        {task.kind && (
          <Pill
            className={
              task.kind === "resident"
                ? "bg-jev/12 text-jev ring-jev/30"
                : undefined
            }
          >
            {task.kind}
          </Pill>
        )}
        {task.model === "cheap" && (
          <Pill className="bg-muted text-muted-foreground">cheap model</Pill>
        )}
        <AgentLink sessionId={task.session_id} agentId={task.agent_id} />
        <span className="tnum ml-auto text-micro text-faint-foreground">
          {task.created_by && <>by {task.created_by} · </>}
          updated <Ago ms={task.updated_at_ms} />
        </span>
      </div>

      {task.stages.length > 0 ? (
        <StageSteps task={task} steps={pipelineOf(mission)} />
      ) : (
        <Pipeline task={task} steps={pipelineOf(mission)} />
      )}

      {(task.objective !== null ||
        task.candidate ||
        task.metric ||
        task.last_gate) && (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 text-caption">
          {task.objective !== null && (
            <span className="text-muted-foreground">
              objective <span className="tnum text-foreground">{task.objective}</span>
            </span>
          )}
          {task.candidate && (
            <span className="text-muted-foreground">
              candidate{" "}
              <span className="font-mono text-foreground" title={task.candidate}>
                {task.candidate.slice(0, 10)}
              </span>
            </span>
          )}
          {task.metric && (
            <span className="min-w-0 truncate font-mono text-info" title={task.metric}>
              {task.metric}
            </span>
          )}
          {task.last_gate && (
            <span className="flex min-w-0 items-center gap-1.5">
              <span className="text-muted-foreground">last gate</span>
              <VerdictBadge verdict={task.last_gate.verdict} />
              {task.last_gate.reason && (
                <span
                  className="min-w-0 max-w-md truncate text-foreground/80"
                  title={task.last_gate.reason}
                >
                  {task.last_gate.reason}
                </span>
              )}
              {task.last_gate.at_ms !== null && (
                <span
                  className="tnum text-faint-foreground"
                  title={absolute(task.last_gate.at_ms)}
                >
                  {relativeTime(task.last_gate.at_ms)}
                </span>
              )}
            </span>
          )}
        </div>
      )}

      {(task.state === "revise" || task.revisions > 0) && (
        <div className="rounded-md border border-warning/25 bg-warning/[0.06] px-3 py-2">
          <div className="flex items-center gap-1.5 text-caption font-medium text-warning">
            <RotateCcw size={12} />
            {task.state === "revise" ? "Sent back for revision" : "Revised"}
            <span className="tnum">
              · {task.revisions} revision{task.revisions === 1 ? "" : "s"}
            </span>
          </div>
          {task.review_note ? (
            <Markdown source={task.review_note} className="mt-1" />
          ) : (
            <p className="mt-1 text-caption text-faint-foreground">
              No review note.
            </p>
          )}
        </div>
      )}
      {task.review_note && task.state !== "revise" && task.revisions === 0 && (
        <div className="rounded-md border border-surface-border px-3 py-2">
          <SectionLabel>Review note</SectionLabel>
          <Markdown source={task.review_note} className="mt-1" />
        </div>
      )}

      <EarlierAttempts group={group} missionName={missionName} />

      {task.brief && (
        <Collapsible label="Brief">
          <Markdown source={task.brief} />
        </Collapsible>
      )}

      {task.report ? (
        <Collapsible label="Report" defaultOpen>
          <Markdown source={task.report} />
        </Collapsible>
      ) : (
        <p className="text-caption text-faint-foreground">No report yet.</p>
      )}
    </li>
  );
}

/** How far along its path a task has got (index into `steps`). */
function reached(state: TaskState, steps: ReadonlyArray<TaskState>): number {
  const at = steps.indexOf(state);
  if (at >= 0) return at;
  // Sent back after a report; or accepted by the gate on a path without a
  // review step, about to land.
  if (state === "revise" || state === "accepted") return steps.indexOf("reported");
  return -1;
}

function Pipeline({
  task,
  steps,
}: {
  task: MissionTask;
  steps: ReadonlyArray<TaskState>;
}) {
  const cancelled = task.state === "cancelled";
  const revise = task.state === "revise";
  const at = reached(task.state, steps);
  return (
    <ol
      aria-label={`Task state: ${task.raw_state ?? "unknown"}`}
      className="flex flex-wrap items-center gap-1"
    >
      {steps.map((step, index) => {
        const done = !cancelled && index <= at;
        const current = !cancelled && !revise && index === at;
        return (
          <li key={step} className="flex items-center gap-1">
            {index > 0 && (
              <ChevronRight
                size={12}
                className={done ? "text-muted-foreground" : "text-faint-foreground/50"}
              />
            )}
            <span
              className={cx(
                "inline-flex h-6 items-center gap-1 rounded-md px-2 text-micro font-medium ring-1 ring-inset",
                cancelled
                  ? "text-faint-foreground/60 ring-surface-border line-through"
                  : current
                    ? step === "accepted" || step === "landed"
                      ? "bg-success/15 text-success ring-success/30"
                      : "bg-info/15 text-info ring-info/30"
                    : done
                      ? "bg-muted text-foreground ring-surface-border"
                      : "text-faint-foreground ring-surface-border",
              )}
            >
              {done && !current && <Check size={11} />}
              {step}
            </span>
          </li>
        );
      })}
      {revise && (
        <li className="flex items-center gap-1">
          <RotateCcw size={12} className="text-warning" />
          <span className="inline-flex h-6 items-center gap-1 rounded-md bg-warning/15 px-2 text-micro font-medium text-warning ring-1 ring-warning/30 ring-inset">
            revise
            <span className="tnum">×{task.revisions}</span>
          </span>
        </li>
      )}
      {cancelled && (
        <li className="flex items-center gap-1 pl-1">
          <span className="inline-flex h-6 items-center gap-1 rounded-md bg-destructive/12 px-2 text-micro font-medium text-destructive ring-1 ring-destructive/25 ring-inset">
            <Ban size={11} />
            cancelled
          </span>
        </li>
      )}
      {task.state === "unknown" && (
        <li className="pl-1">
          <Pill>{task.raw_state ?? "no state"}</Pill>
        </li>
      )}
    </ol>
  );
}

/** The steps of the task's path it has still to walk. */
function remainingSteps(
  state: TaskState,
  steps: ReadonlyArray<TaskState>,
): ReadonlyArray<TaskState> {
  if (state === "cancelled" || state === "landed" || state === "unknown") return [];
  // Sent back: another delivery and report, then the rest of the path.
  if (state === "revise") return steps.slice(Math.max(0, steps.indexOf("delivered")));
  const at = reached(state, steps);
  return at < 0 ? [] : steps.slice(at + 1);
}

/** The task's stepper from its records: one step per record, so a second
 * round shows as steps of its own, each with the time it took, then the
 * steps still ahead. The first of those counts the wait so far. */
function StageSteps({
  task,
  steps,
}: {
  task: MissionTask;
  steps: ReadonlyArray<TaskState>;
}) {
  const ahead = remainingSteps(task.state, steps);
  const stages = task.stages;
  const last = stages[stages.length - 1];
  const waiting =
    ahead.length > 0 && last.at_ms !== null
      ? Math.max(0, Date.now() - last.at_ms)
      : null;
  const slow = slowStages([
    ...stages,
    ...(waiting !== null
      ? [{ stage: "waiting", verdict: null, at_ms: null, took_ms: waiting }]
      : []),
  ]);
  const settled = endedWell(task.state);
  return (
    <ol
      aria-label={`Task stages, state ${task.raw_state ?? "unknown"}`}
      className="flex flex-wrap items-start gap-x-1 gap-y-2"
    >
      {stages.map((stage, index) => {
        const step = stageStep(stage);
        const current = index === stages.length - 1;
        return (
          <StepItem
            key={`${index}-${stage.stage}`}
            first={index === 0}
            title={`task.${stage.stage}${stage.verdict ? ` ${stage.verdict}` : ""} at ${absolute(stage.at_ms) ?? "an unknown time"}`}
            chip={
              step.tone === "closed"
                ? "bg-destructive/12 text-destructive ring-destructive/25"
                : current && settled
                  ? "bg-success/15 text-success ring-success/30"
                  : current
                    ? "bg-info/15 text-info ring-info/30"
                    : "bg-muted text-foreground ring-surface-border"
            }
            icon={
              step.tone === "closed" ? (
                <Ban size={11} />
              ) : step.tone === "back" ? (
                <RotateCcw size={11} className="text-warning" />
              ) : !current ? (
                <Check size={11} />
              ) : null
            }
            label={step.label}
            took={stage.took_ms}
            slow={slow[index]}
          />
        );
      })}
      {ahead.map((step, index) => (
        <StepItem
          key={`ahead-${step}`}
          first={false}
          pending
          title={
            index === 0 && waiting !== null
              ? `waiting since ${absolute(last.at_ms) ?? "an unknown time"}`
              : undefined
          }
          chip="text-faint-foreground ring-surface-border"
          label={step}
          took={index === 0 ? waiting : null}
          slow={index === 0 && waiting !== null && slow[slow.length - 1]}
        />
      ))}
    </ol>
  );
}

function StepItem({
  first,
  pending = false,
  title,
  chip,
  icon,
  label,
  took,
  slow,
}: {
  first: boolean;
  pending?: boolean;
  title?: string;
  chip: string;
  icon?: ReactNode;
  label: string;
  took: number | null;
  slow: boolean;
}) {
  const duration = stageDuration(took);
  return (
    <li className="flex items-start gap-1">
      {!first && (
        <span className="flex h-6 items-center">
          <ChevronRight
            size={12}
            className={pending ? "text-faint-foreground/50" : "text-muted-foreground"}
          />
        </span>
      )}
      <span className="flex flex-col items-center gap-0.5" title={title}>
        <span
          className={cx(
            "inline-flex h-6 items-center gap-1 rounded-md px-2 text-micro font-medium ring-1 ring-inset",
            chip,
          )}
        >
          {icon}
          {label}
        </span>
        <span
          className={cx(
            "tnum h-4 rounded px-1 text-micro leading-4",
            slow
              ? "bg-warning/15 font-semibold text-warning"
              : pending
                ? "text-faint-foreground"
                : "text-muted-foreground",
          )}
        >
          {duration && pending ? `${duration} so far` : duration}
        </span>
      </span>
    </li>
  );
}

// ---------------------------------------------------------------------------
// Shared bits

function firstLine(text: string | null): string | null {
  if (!text) return null;
  const line = text
    .split("\n")
    .map((part) => part.trim())
    .find((part) => part !== "");
  return line ? line.replace(/^#+\s*/, "") : null;
}

function countStates(tasks: MissionTask[]): Record<TaskState, number> {
  const counts: Record<TaskState, number> = {
    assigned: 0,
    delivered: 0,
    reported: 0,
    revise: 0,
    accepted: 0,
    cancelled: 0,
    landing: 0,
    landed: 0,
    unknown: 0,
  };
  for (const task of tasks) counts[task.state] += 1;
  return counts;
}

/** The last two components of a path: enough to tell gates apart. */
function shortPath(path: string): string {
  const parts = path.split("/").filter(Boolean);
  return parts.length <= 2 ? path : `…/${parts.slice(-2).join("/")}`;
}

function budgetText(budget: Mission["budget"]): string {
  if (!budget) return "—";
  const parts = [
    budget.minutes !== null ? `${budget.minutes} min` : null,
    budget.tool_calls !== null ? `${budget.tool_calls} tool calls` : null,
  ].filter((part): part is string => part !== null);
  return parts.length > 0 ? parts.join(" · ") : "—";
}

function absolute(ms: number | null): string | undefined {
  return absoluteTitle(ms);
}

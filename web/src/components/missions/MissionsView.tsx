import {
  ArrowUpRight,
  Ban,
  Check,
  ChevronRight,
  Hourglass,
  RotateCcw,
  Target,
  Terminal,
  Users,
} from "lucide-react";
import type { ReactNode } from "react";

import { usePoll } from "../../api";
import { relativeTime } from "../../format";
import type {
  Mission,
  MissionTask,
  Missions,
  Resident,
  TaskState,
} from "../../missions";
import { PIPELINE, approveCommand, fetchMissions } from "../../missions";
import { Fact, SectionLabel } from "../fleet/bits";
import { useNow } from "../rsi/Phase";
import { Card, CardHeader, EmptyState, LiveStatus, PageHeader, cx } from "../ui";
import { Markdown } from "./Markdown";

/**
 * Missions and the residents that work them. Strictly read-only: the only
 * request is GET /api/missions, and the only "actions" are links into the
 * fleet view and commands a human copies into a terminal.
 */
export function MissionsView() {
  useNow(1_000);
  const missions = usePoll<Missions>("missions", fetchMissions, 5_000);
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
  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto flex max-w-[1600px] flex-col gap-3 p-3 sm:p-4">
        <ApprovalQueue residents={data.residents} />
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
              data.missions.map((mission, index) => (
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
                {relativeTime(resident.created_at_ms)}
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
  return (
    <Card className="min-w-0">
      <header className="flex flex-wrap items-center gap-2 border-b border-surface-border px-4 py-2.5">
        <span className="tnum font-mono text-caption text-faint-foreground">
          #{mission.id ?? "?"}
        </span>
        <h2 className="min-w-0 flex-1 truncate text-label font-medium text-foreground">
          {title}
        </h2>
        <Pill className={MISSION_STATE_STYLE[state.toLowerCase()]}>{state}</Pill>
      </header>

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
              {counts.cancelled > 0 && ` · ${counts.cancelled} cancelled`}
            </span>
          </Fact>
        </div>

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
          <ul className="divide-y divide-surface-border">
            {mission.tasks.map((task, index) => (
              <TaskRow key={task.id ?? `index-${index}`} task={task} />
            ))}
          </ul>
        )}
      </div>
    </Card>
  );
}

function TaskRow({ task }: { task: MissionTask }) {
  return (
    <li
      className={cx(
        "space-y-2.5 px-4 py-3",
        task.state === "cancelled" && "bg-muted/30",
      )}
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="tnum font-mono text-caption text-faint-foreground">
          task #{task.id ?? "?"}
        </span>
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
        <AgentLink sessionId={task.session_id} agentId={task.agent_id} />
        <span className="tnum ml-auto text-micro text-faint-foreground">
          {task.created_by && <>by {task.created_by} · </>}
          <span title={absolute(task.updated_at_ms)}>
            updated {relativeTime(task.updated_at_ms)}
          </span>
        </span>
      </div>

      <Pipeline task={task} />

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

/** How far along the happy path a task has got (index into PIPELINE). */
function reached(state: TaskState): number {
  switch (state) {
    case "assigned":
      return 0;
    case "delivered":
      return 1;
    case "reported":
    case "revise":
      return 2;
    case "accepted":
      return 3;
    default:
      return -1;
  }
}

function Pipeline({ task }: { task: MissionTask }) {
  const cancelled = task.state === "cancelled";
  const revise = task.state === "revise";
  const at = reached(task.state);
  return (
    <ol
      aria-label={`Task state: ${task.raw_state ?? "unknown"}`}
      className="flex flex-wrap items-center gap-1"
    >
      {PIPELINE.map((step, index) => {
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
                    ? step === "accepted"
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

// ---------------------------------------------------------------------------
// Shared bits

function AgentLink({
  sessionId,
  agentId,
}: {
  sessionId: string | null;
  agentId: string | null;
}) {
  const label = agentId ?? sessionId;
  if (!label) return null;
  if (!sessionId) {
    return (
      <span className="truncate font-mono text-caption text-muted-foreground">
        {label}
      </span>
    );
  }
  return (
    <a
      href={`#/fleet/${encodeURIComponent(sessionId)}`}
      title={`Open ${sessionId} in the fleet view`}
      className="inline-flex min-w-0 items-center gap-0.5 truncate font-mono text-caption text-info hover:underline"
    >
      {label}
      <ArrowUpRight size={12} className="shrink-0" />
    </a>
  );
}

function Collapsible({
  label,
  children,
  defaultOpen = false,
}: {
  label: string;
  children: ReactNode;
  defaultOpen?: boolean;
}) {
  return (
    <details open={defaultOpen} className="group min-w-0">
      <summary className="flex cursor-pointer list-none items-center gap-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground select-none hover:text-muted-foreground [&::-webkit-details-marker]:hidden">
        <ChevronRight
          size={12}
          className="transition-transform group-open:rotate-90"
        />
        {label}
      </summary>
      <div className="mt-1.5 rounded-md border border-surface-border bg-page-canvas/60 px-3 py-2">
        {children}
      </div>
    </details>
  );
}

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
    unknown: 0,
  };
  for (const task of tasks) counts[task.state] += 1;
  return counts;
}

function absolute(ms: number | null): string | undefined {
  if (ms == null || ms <= 0) return undefined;
  return new Date(ms).toLocaleString();
}

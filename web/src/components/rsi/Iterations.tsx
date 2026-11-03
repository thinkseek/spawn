import { FileDiff, ListOrdered, ScrollText } from "lucide-react";
import { useState } from "react";
import type { ReactNode } from "react";

import { usePoll } from "../../api";
import { clockTime, formatBytes, formatDuration, relativeTime } from "../../format";
import type { RsiIteration } from "../../rsi";
import { fetchIteration, isTerminalPhase } from "../../rsi";
import { Card, CardHeader, Drawer, EmptyState, Tabs, cx } from "../ui";
import { DiffView, diffStats } from "../Diff";
import { GateLog, parseVerdict } from "./GateLog";
import { PhaseBadge, useNow } from "./Phase";

export function iterationDuration(
  iteration: RsiIteration,
  now: number,
): number | null {
  if (iteration.started_at_ms <= 0) return null;
  const end = isTerminalPhase(iteration.phase) ? iteration.updated_at_ms : now;
  return Math.max(0, end - iteration.started_at_ms);
}

/** What a row says when the controller left no note. */
function defaultNote(iteration: RsiIteration): string {
  switch (iteration.phase) {
    case "Starting":
      return "Preparing the agent workspace";
    case "Working":
      return "Agent is editing the tree";
    case "Gating":
      return "Running the gate scenarios";
    case "Landed":
      return iteration.candidate
        ? `Baseline moved to ${iteration.candidate.slice(0, 7)}`
        : "Baseline moved";
    case "Rejected":
      return "Gate rejected the candidate";
    case "Aborted":
      return "Aborted";
    default:
      return "";
  }
}

const COLUMNS =
  "grid-cols-[2.75rem_minmax(0,1fr)_auto] sm:grid-cols-[2.5rem_5.75rem_minmax(0,1fr)_4rem_4.25rem]";

interface IterationListProps {
  iterations: RsiIteration[];
  selected: number | null;
  onSelect: (iteration: number) => void;
  className?: string;
}

export function IterationList({
  iterations,
  selected,
  onSelect,
  className,
}: IterationListProps) {
  const now = useNow(1_000);
  const landed = iterations.filter((entry) => entry.phase === "Landed").length;
  return (
    <Card className={cx("flex flex-col overflow-hidden", className)}>
      <CardHeader
        icon={<ListOrdered size={14} />}
        title="Iterations"
        meta={
          iterations.length > 0 && (
            <span className="tnum">
              {iterations.length} total
              <span className="mx-1.5 text-faint-foreground">·</span>
              <span className={landed > 0 ? "text-success" : undefined}>
                {landed} landed
              </span>
            </span>
          )
        }
      />
      {iterations.length === 0 ? (
        <EmptyState
          title="No iterations yet"
          hint="The controller opens iteration #1 when it picks a task."
        />
      ) : (
        <>
          <div
            className={cx(
              "hidden h-8 shrink-0 items-center gap-3 border-b border-surface-border px-4 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground sm:grid",
              COLUMNS,
            )}
          >
            <span>#</span>
            <span>Phase</span>
            <span>Note</span>
            <span className="text-right">Duration</span>
            <span className="text-right">Started</span>
          </div>
          <ul className="min-h-0 flex-1 divide-y divide-surface-border overflow-y-auto">
            {iterations.map((iteration) => {
              const active = iteration.iteration === selected;
              const note = iteration.note ?? defaultNote(iteration);
              const open = !isTerminalPhase(iteration.phase);
              return (
                <li key={iteration.iteration}>
                  <button
                    type="button"
                    onClick={() => onSelect(iteration.iteration)}
                    aria-current={active ? "true" : undefined}
                    className={cx(
                      "group grid w-full items-center gap-x-3 gap-y-1 px-4 py-2.5 text-left transition-colors sm:min-h-11 sm:py-2",
                      COLUMNS,
                      active ? "bg-surface-selected" : "hover:bg-surface-hover",
                    )}
                  >
                    <span className="tnum font-mono text-label text-muted-foreground">
                      #{iteration.iteration}
                    </span>
                    <span className="flex min-w-0 items-center gap-2 sm:contents">
                      <span className="sm:flex">
                        <PhaseBadge phase={iteration.phase} />
                      </span>
                      <span
                        className={cx(
                          "min-w-0 truncate text-label",
                          iteration.note
                            ? "text-foreground/85"
                            : "text-faint-foreground",
                        )}
                        title={note}
                      >
                        <span className="mr-1.5 text-faint-foreground">
                          T{iteration.task_id}
                        </span>
                        {note}
                      </span>
                    </span>
                    <span
                      className={cx(
                        "tnum text-right font-mono text-caption",
                        open ? "text-foreground" : "text-muted-foreground",
                      )}
                    >
                      {formatDuration(iterationDuration(iteration, now))}
                    </span>
                    <span
                      className="tnum hidden text-right text-caption text-faint-foreground sm:block"
                      title={clockTime(iteration.started_at_ms)}
                    >
                      {relativeTime(iteration.started_at_ms)}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        </>
      )}
    </Card>
  );
}

type Tab = "gate" | "patch" | "prompt";

interface IterationDetailProps {
  iteration: number;
  /** Row from the dashboard poll, so polling stops once it settles. */
  summary: RsiIteration | null;
  onClose: () => void;
}

/** Slide-over with the gate log, patch and prompt of one iteration. */
export function IterationDetail({
  iteration,
  summary,
  onClose,
}: IterationDetailProps) {
  const now = useNow(1_000);
  const phase = summary?.phase ?? null;
  const settled = phase !== null && isTerminalPhase(phase);
  const detail = usePoll(
    `rsi-iteration-${iteration}`,
    (signal) => fetchIteration(iteration, signal),
    settled ? 600_000 : 3_000,
  );
  const data = detail.data;
  const [chosen, setChosen] = useState<Tab | null>(null);

  const stats = data ? diffStats(data.patch) : null;
  const verdict = data ? parseVerdict(data.gate_log) : null;
  const tab: Tab =
    chosen ??
    (data?.gate_log.trim() ? "gate" : data?.patch ? "patch" : "prompt");

  return (
    <Drawer
      onClose={onClose}
      header={
        <>
          <h2 className="tnum truncate text-title-sm font-semibold text-foreground">
            Iteration #{iteration}
          </h2>
          {(data || summary) && (
            <PhaseBadge phase={data?.phase ?? summary?.phase ?? ""} />
          )}
          {!settled && data && (
            <span className="hidden text-caption text-faint-foreground sm:inline">
              refreshing every 3s
            </span>
          )}
        </>
      }
    >
      {!data ? (
        <div className="grid flex-1 place-items-center">
          <p className="text-caption text-faint-foreground">
            {detail.error
              ? `Could not load this iteration: ${detail.error}`
              : "Loading…"}
          </p>
        </div>
      ) : (
        <>
          <dl className="grid shrink-0 grid-cols-2 gap-px border-b border-surface-border bg-surface-border sm:grid-cols-4">
            <Meta label="Task">
              <span className="tnum">#{data.task_id}</span>
            </Meta>
            <Meta label="Duration">
              <span className="tnum">
                {summary
                  ? formatDuration(iterationDuration(summary, now))
                  : "—"}
              </span>
            </Meta>
            <Meta label="Candidate">
              <span className="font-mono" title={data.candidate ?? undefined}>
                {data.candidate ? data.candidate.slice(0, 10) : "—"}
              </span>
            </Meta>
            <Meta label="Patch">
              {stats && stats.files > 0 ? (
                <span className="tnum">
                  <span className="text-success">+{stats.added}</span>{" "}
                  <span className="text-destructive">−{stats.removed}</span>
                  <span className="ml-1.5 text-muted-foreground">
                    {formatBytes(new Blob([data.patch]).size)}
                  </span>
                </span>
              ) : (
                "—"
              )}
            </Meta>
          </dl>

          {(data.note || (verdict && tab !== "gate")) && (
            <div className="flex shrink-0 flex-col gap-2 border-b border-surface-border px-4 py-3 sm:px-5">
              {verdict && tab !== "gate" && <VerdictLine verdict={verdict} />}
              {data.note && (
                <p className="text-label text-muted-foreground">
                  <span className="mr-2 text-faint-foreground">Note</span>
                  {data.note}
                </p>
              )}
            </div>
          )}

          <Tabs<Tab>
            active={tab}
            onChange={setChosen}
            tabs={[
              { id: "gate", label: "Gate log" },
              {
                id: "patch",
                label: "Patch",
                count: stats && stats.files > 0 ? `${stats.files} file${stats.files === 1 ? "" : "s"}` : undefined,
              },
              { id: "prompt", label: "Prompt" },
            ]}
          />

          <div className="min-h-0 flex-1 overflow-auto">
            {tab === "gate" &&
              (data.gate_log.trim() ? (
                <GateLog log={data.gate_log} />
              ) : (
                <EmptyState
                  icon={<ScrollText size={18} />}
                  title="No gate output yet"
                  hint="The gate runs once the agent signals that the task is done."
                />
              ))}
            {tab === "patch" &&
              (data.patch ? (
                <DiffView patch={data.patch} />
              ) : (
                <EmptyState
                  icon={<FileDiff size={18} />}
                  title="No patch captured yet"
                  hint="The candidate diff appears here when the agent finishes."
                />
              ))}
            {tab === "prompt" &&
              (data.prompt ? (
                <div className="px-4 py-4 sm:px-5">
                  <pre className="max-w-3xl whitespace-pre-wrap break-words font-sans text-label leading-6 text-foreground/85">
                    {data.prompt}
                  </pre>
                </div>
              ) : (
                <EmptyState title="No prompt recorded" />
              ))}
          </div>
        </>
      )}
    </Drawer>
  );
}

function Meta({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0 bg-page-canvas px-4 py-2.5 sm:px-5">
      <dt className="text-caption text-faint-foreground">{label}</dt>
      <dd className="mt-0.5 truncate text-label text-foreground">{children}</dd>
    </div>
  );
}

function VerdictLine({
  verdict,
}: {
  verdict: { accepted: boolean; reason: string | null };
}) {
  return (
    <div
      className={cx(
        "flex items-start gap-2 rounded-md px-3 py-2 text-label ring-1 ring-inset",
        verdict.accepted
          ? "bg-success/10 text-success ring-success/25"
          : "bg-destructive/10 text-destructive ring-destructive/25",
      )}
    >
      <span className="font-semibold">
        {verdict.accepted ? "Accepted" : "Rejected"}
      </span>
      {verdict.reason && (
        <span className="min-w-0 break-words text-foreground/80">
          {verdict.reason}
        </span>
      )}
    </div>
  );
}

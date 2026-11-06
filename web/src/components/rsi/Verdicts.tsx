import { Gavel, ScrollText } from "lucide-react";
import { useState } from "react";
import type { ReactNode } from "react";

import { usePoll } from "../../api";
import { absoluteTitle, clockTime, relativeTime } from "../../format";
import type { RsiTask, TaskVerdict } from "../../rsi";
import { fetchTask, isSettled } from "../../rsi";
import { Card, CardHeader, Drawer, EmptyState, Tabs, cx } from "../ui";
import { GateLog } from "./GateLog";
import { PhaseBadge, VerdictBadge, useNow } from "./Phase";

/** What a row says when the verdict carries no reason. */
function defaultNote(verdict: TaskVerdict): string {
  switch (verdict.verdict) {
    case "accepted":
      return verdict.candidate
        ? `Landing ${verdict.candidate.slice(0, 7)}`
        : "The gate accepted the candidate";
    case "rejected":
      return "The gate rejected the candidate";
    case "aborted":
      return "Aborted: the gate or the budget ran out";
    case "conflict":
      return "Conflicts with main; sent back";
    case "incompatible":
      return "Passes alone, fails merged with main; sent back, not counted";
    case "stale":
      return "Main moved; judged again";
    case "no_room":
      return "No room for the gate (disk, memory or the test database); judged again later";
    case "host_fault":
      return "The host failed (the test database), not the work; judged again after a backoff";
    default:
      return "";
  }
}

const COLUMNS =
  "grid-cols-[3.25rem_minmax(0,1fr)_auto] sm:grid-cols-[3.25rem_5.75rem_minmax(0,1fr)_4.25rem]";

interface VerdictListProps {
  verdicts: TaskVerdict[];
  selected: number | null;
  onSelect: (task: number) => void;
  className?: string;
  /** Beside the title: the Timeline/Table switch. */
  action?: ReactNode;
}

/** Every gate verdict of the mission, newest first: what the gate said
 * about each task, each time it judged it. */
export function VerdictList({
  verdicts,
  selected,
  onSelect,
  className,
  action,
}: VerdictListProps) {
  useNow(1_000);
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
          {action}
          </>
        }
      />
      {verdicts.length === 0 ? (
        <EmptyState
          title="No gate verdicts yet"
          hint="The integrator records one each time the gate judges a task."
        />
      ) : (
        <>
          <div
            className={cx(
              "hidden h-8 shrink-0 items-center gap-3 border-b border-surface-border px-4 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground sm:grid",
              COLUMNS,
            )}
          >
            <span>Task</span>
            <span>Verdict</span>
            <span>Reason</span>
            <span className="text-right">When</span>
          </div>
          <ul className="min-h-0 flex-1 divide-y divide-surface-border overflow-y-auto">
            {verdicts.map((verdict, index) => {
              const active = verdict.task === selected;
              const note = verdict.reason ?? verdict.metric ?? defaultNote(verdict);
              return (
                <li key={`${verdict.task}-${verdict.round}-${index}`}>
                  <button
                    type="button"
                    onClick={() => onSelect(verdict.task)}
                    aria-current={active ? "true" : undefined}
                    className={cx(
                      "group grid w-full items-center gap-x-3 gap-y-1 px-4 py-2.5 text-left transition-colors sm:min-h-11 sm:py-2",
                      COLUMNS,
                      active ? "bg-surface-selected" : "hover:bg-surface-hover",
                    )}
                  >
                    <span className="tnum font-mono text-label text-muted-foreground">
                      T{verdict.task}
                    </span>
                    <span className="flex min-w-0 items-center gap-2 sm:contents">
                      <span className="sm:flex">
                        <VerdictBadge verdict={verdict.verdict} />
                      </span>
                      <span
                        className={cx(
                          "min-w-0 truncate text-label",
                          verdict.reason || verdict.metric
                            ? "text-foreground/85"
                            : "text-faint-foreground",
                        )}
                        title={note}
                      >
                        {verdict.objective !== null && (
                          <span className="mr-1.5 text-faint-foreground">
                            O{verdict.objective}
                            {verdict.round > 1 && ` · try ${verdict.round}`}
                          </span>
                        )}
                        {note}
                      </span>
                    </span>
                    <span
                      className="tnum text-right text-caption text-faint-foreground"
                      title={absoluteTitle(verdict.at_ms)}
                    >
                      {relativeTime(verdict.at_ms)}
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

type Tab = "gate" | "brief" | "report";

interface TaskDetailProps {
  task: number;
  /** Row from the dashboard poll, so polling slows once the task settles. */
  summary: RsiTask | null;
  onClose: () => void;
}

/** Slide-over with one task's gate verdicts (with their logs), brief and
 * report. */
export function TaskDetail({ task, summary, onClose }: TaskDetailProps) {
  const settled = summary !== null && isSettled(summary.state);
  const detail = usePoll(
    `rsi-task-${task}`,
    (signal) => fetchTask(task, signal),
    settled ? 600_000 : 3_000,
  );
  const data = detail.data;
  const [chosen, setChosen] = useState<Tab | null>(null);
  const tab: Tab =
    chosen ?? (data && data.gates.length > 0 ? "gate" : "brief");
  const state = data?.state ?? summary?.state ?? null;

  return (
    <Drawer
      onClose={onClose}
      header={
        <>
          <h2 className="tnum truncate text-title-sm font-semibold text-foreground">
            Task #{task}
          </h2>
          {state && <PhaseBadge phase={state} />}
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
            {detail.error ? `Could not load this task: ${detail.error}` : "Loading…"}
          </p>
        </div>
      ) : (
        <>
          <dl className="grid shrink-0 grid-cols-2 gap-px border-b border-surface-border bg-surface-border sm:grid-cols-4">
            <Meta label="Objective">
              <span className="tnum">
                {data.objective !== null ? `#${data.objective}` : "—"}
              </span>
            </Meta>
            <Meta label="Revisions">
              <span className="tnum">{data.revisions}</span>
            </Meta>
            <Meta label="Candidate">
              <span className="font-mono" title={data.candidate ?? undefined}>
                {data.candidate ? data.candidate.slice(0, 10) : "—"}
              </span>
            </Meta>
            <Meta label="Updated">
              <span title={clockTime(data.updated_at_ms)}>
                {relativeTime(data.updated_at_ms)}
              </span>
            </Meta>
          </dl>

          {(data.metric || data.review_note) && (
            <div className="flex shrink-0 flex-col gap-2 border-b border-surface-border px-4 py-3 sm:px-5">
              {data.metric && (
                <p className="font-mono text-caption text-foreground/85">
                  {data.metric}
                </p>
              )}
              {data.review_note && (
                <p className="line-clamp-3 whitespace-pre-wrap text-label text-muted-foreground">
                  <span className="mr-2 text-faint-foreground">Note</span>
                  {data.review_note}
                </p>
              )}
            </div>
          )}

          <Tabs<Tab>
            active={tab}
            onChange={setChosen}
            tabs={[
              {
                id: "gate",
                label: "Gate verdicts",
                count: data.gates.length > 0 ? String(data.gates.length) : undefined,
              },
              { id: "brief", label: "Brief" },
              { id: "report", label: "Report" },
            ]}
          />

          <div className="min-h-0 flex-1 overflow-auto">
            {tab === "gate" &&
              (data.gates.length > 0 ? (
                <ol className="divide-y divide-surface-border">
                  {[...data.gates].reverse().map((gate, index) => (
                    <li key={`${gate.at_ms}-${index}`} className="py-2">
                      <div className="flex flex-wrap items-center gap-2 px-4 py-1 sm:px-5">
                        <VerdictBadge verdict={gate.verdict} />
                        <span className="tnum text-caption text-faint-foreground">
                          {clockTime(gate.at_ms)} · {relativeTime(gate.at_ms)}
                        </span>
                        {gate.candidate && (
                          <span className="ml-auto font-mono text-caption text-muted-foreground">
                            {gate.candidate.slice(0, 10)}
                          </span>
                        )}
                      </div>
                      {gate.reason && (
                        <p className="px-4 text-label text-foreground/85 sm:px-5">
                          {gate.reason}
                        </p>
                      )}
                      {gate.metric && (
                        <p className="px-4 font-mono text-caption text-info sm:px-5">
                          {gate.metric}
                        </p>
                      )}
                      {gate.log?.trim() && <GateLog log={gate.log} />}
                    </li>
                  ))}
                </ol>
              ) : (
                <EmptyState
                  icon={<ScrollText size={18} />}
                  title="Not gated yet"
                  hint="The gate runs once the resident reports the task done."
                />
              ))}
            {tab === "brief" && <Prose text={data.brief} empty="No brief recorded" />}
            {tab === "report" && <Prose text={data.report} empty="No report yet" />}
          </div>
        </>
      )}
    </Drawer>
  );
}

function Prose({ text, empty }: { text: string | null; empty: string }) {
  if (!text) return <EmptyState title={empty} />;
  return (
    <div className="px-4 py-4 sm:px-5">
      <pre className="max-w-3xl whitespace-pre-wrap break-words font-sans text-label leading-6 text-foreground/85">
        {text}
      </pre>
    </div>
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

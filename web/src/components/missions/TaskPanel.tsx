import { Check, Copy, Crosshair, RotateCcw, X } from "lucide-react";
import { motion } from "motion/react";
import { useEffect, useRef, useState, type ReactNode } from "react";

import { clockTime, formatDuration, relativeTime, stageDuration } from "../../format";
import { PHASE_LABEL } from "../../missionTimeline";
import type { Lane, TaskBar } from "../../missionTimeline";
import {
  formatTaskGrading,
  stageStep,
  taskReviewCommands,
  type TaskReviewCommand,
} from "../../missions";
import { taskTitle } from "../../ordering";
import { VerdictBadge } from "../rsi/Phase";
import { FOCUS_RING, cx } from "../ui";
import { TaskText, useTaskTexts } from "./TaskTexts";
import { AgentLink, Collapsible, TaskGradingDetails } from "./parts";
import { TaskActivityLine } from "./TaskActivity";
import { PHASE_COLOR, verdictColor } from "./timelineStyle";

const OUTCOME_LABEL: Record<TaskBar["outcome"], string> = {
  landed: "landed",
  accepted: "accepted",
  cancelled: "cancelled",
  aborted: "aborted (over budget)",
  open: "in flight",
};

/** The selected task: its brief, report, candidate, verdicts and every
 * record, as the cards show them, beside the timeline. */
export function TaskPanel({
  bar,
  lane,
  now,
  reduceMotion,
  onClose,
  onZoom,
}: {
  bar: TaskBar;
  lane: Lane;
  now: number;
  reduceMotion: boolean;
  onClose: () => void;
  onZoom: () => void;
}) {
  const task = bar.task;
  const grading = formatTaskGrading(task);
  const reviewCommands = taskReviewCommands(task.id);
  // Opened, the task's texts are read in full (the board sends their first
  // lines).
  const texts = useTaskTexts(task, true);
  const lastVerdict = [...bar.verdicts].reverse().find((verdict) => verdict.by === "gated") ?? null;
  const reason = lastVerdict?.reason ?? task.last_gate?.reason ?? null;
  const total = (bar.end ?? now) - bar.start;

  return (
    <motion.aside
      aria-label={`Task ${task.id ?? ""}`}
      initial={reduceMotion ? { opacity: 0 } : { opacity: 0, x: 16 }}
      animate={{ opacity: 1, x: 0 }}
      exit={reduceMotion ? { opacity: 0 } : { opacity: 0, x: 16 }}
      transition={{ duration: 0.16, ease: [0.2, 0.8, 0.2, 1] }}
      className="absolute inset-y-0 right-0 z-10 flex w-full max-w-[28rem] flex-col border-l border-surface-border bg-surface shadow-[var(--floating-shadow)] lg:static lg:shadow-none"
    >
      <header className="flex shrink-0 items-start gap-3 px-4 pt-3.5 pb-3">
        <div className="min-w-0 flex-1">
          <div className="truncate text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
            {lane.name}
          </div>
          {task.label && (
            <h3
              className="mt-1 line-clamp-2 text-title-sm font-semibold text-foreground"
              title={task.label}
            >
              {task.label}
            </h3>
          )}
          <div
            className={cx(
              "flex items-center gap-2",
              task.label
                ? "mt-0.5 select-all font-mono text-micro text-faint-foreground"
                : "mt-1 text-title-sm font-semibold text-foreground",
            )}
          >
            Task #{task.id ?? "?"}
            {task.objective !== null && (
              <span className={cx(!task.label && "text-label font-normal text-muted-foreground")}>
                objective {task.objective}
              </span>
            )}
          </div>
          {!task.label && taskTitle(task, { missionName: lane.name }) && (
            <p className="mt-1 line-clamp-2 text-label text-foreground">
              {taskTitle(task, { missionName: lane.name })}
            </p>
          )}
          <div className="mt-1 flex min-w-0 items-center gap-2">
            <AgentLink sessionId={task.session_id} agentId={task.agent_id} />
          </div>
          <div className="mt-1">
            <TaskActivityLine activity={task.activity} now={now} />
          </div>
        </div>
        <button
          type="button"
          onClick={onZoom}
          title="Zoom the axis to this task (Enter)"
          aria-label="Zoom to task"
          className="grid size-7 shrink-0 place-items-center rounded-md text-muted-foreground hover:bg-surface-hover hover:text-foreground"
        >
          <Crosshair size={14} />
        </button>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close detail"
          title="Close (Esc)"
          className="grid size-7 shrink-0 place-items-center rounded-md text-muted-foreground hover:bg-surface-hover hover:text-foreground"
        >
          <X size={15} />
        </button>
      </header>

      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-4 pb-5">
        <dl className="grid grid-cols-[6.5rem_minmax(0,1fr)] gap-x-3 gap-y-1.5 text-label">
          <Field label="Outcome">
            <span className={cx(bar.outcome === "aborted" && "text-destructive")}>
              {OUTCOME_LABEL[bar.outcome]}
              {bar.stalled && <span className="text-warning"> · mission halted</span>}
            </span>
          </Field>
          <Field label="Took">{formatDuration(total)}</Field>
          <Field label="Assigned">{clockTime(bar.start)}</Field>
          {bar.attempts > 1 && <Field label="Attempts">{bar.attempts}</Field>}
          {task.candidate && (
            <Field label="Candidate">
              <span className="font-mono text-caption" title={task.candidate}>
                {task.candidate.slice(0, 10)}
              </span>
            </Field>
          )}
          {task.metric && (
            <Field label="Metric">
              <span className="font-mono text-caption">{task.metric}</span>
            </Field>
          )}
          {task.model === "cheap" && <Field label="Model">cheap (DeepSeek)</Field>}
          {grading && <TaskGradingDetails grading={grading} />}
        </dl>

        {lastVerdict && (
          <section>
            <Label>Last verdict</Label>
            <div className="mt-1.5 flex items-center gap-2">
              <VerdictBadge verdict={lastVerdict.verdict} />
              <span className="tnum text-caption text-faint-foreground" title={clockTime(lastVerdict.at)}>
                {relativeTime(lastVerdict.at)}
              </span>
            </div>
            {reason && <p className="mt-1.5 text-label text-foreground/85">{reason}</p>}
          </section>
        )}

        {task.review_note && (
          <section className="rounded-md bg-wait-tint px-3 py-2">
            <div className="flex items-center gap-1.5 text-caption font-medium text-warning">
              <RotateCcw size={12} />
              Review note
            </div>
            <TaskText
              what="note"
              excerpt={task.review_note}
              bytes={task.review_note_bytes}
              full={texts.texts?.review_note}
              read={texts}
              className="mt-1"
            />
          </section>
        )}

        <section>
          <Label>Stages</Label>
          <ol className="mt-1.5 space-y-0.5">
            {bar.segments.map((segment, index) => (
              <li key={index} className="flex items-center gap-2 text-label">
                <span
                  aria-hidden="true"
                  className="h-2 w-3 shrink-0 rounded-full"
                  style={{ background: PHASE_COLOR[segment.phase] }}
                />
                <span className="text-foreground">{PHASE_LABEL[segment.phase]}</span>
                {bar.attempts > 1 && (
                  <span className="text-caption text-faint-foreground">try {segment.attempt}</span>
                )}
                <span className="tnum ml-auto text-caption text-muted-foreground">
                  {clockTime(segment.start)}
                </span>
                <span className="tnum w-16 text-right text-caption text-foreground/80">
                  {segment.end === null ? "now" : formatDuration(segment.end - segment.start)}
                </span>
              </li>
            ))}
          </ol>
          {bar.verdicts.length > 0 && (
            <ol className="mt-2.5 space-y-1">
              {bar.verdicts.map((verdict, index) => (
                <li key={index} className="flex min-w-0 items-start gap-2 text-label">
                  <span
                    aria-hidden="true"
                    className="mt-1.5 size-2 shrink-0 rounded-full"
                    style={{ background: verdictColor(verdict.verdict) }}
                  />
                  <span className="min-w-0 flex-1">
                    <span className="text-foreground">
                      {verdict.by === "reviewed" ? "review" : "gate"} {verdict.verdict}
                    </span>
                    {verdict.sentBack && <span className="text-warning"> · sent back</span>}
                    {verdict.verdict === "host_fault" && (
                      <span className="text-info"> · the host's, not the work's</span>
                    )}
                    {verdict.reason && (
                      <span className="block truncate text-caption text-muted-foreground" title={verdict.reason}>
                        {verdict.reason}
                      </span>
                    )}
                  </span>
                  <span className="tnum shrink-0 text-caption text-muted-foreground">
                    {clockTime(verdict.at)}
                  </span>
                </li>
              ))}
            </ol>
          )}
          {task.stages.length > 0 && (
            <div className="mt-3">
            <Collapsible label="Records">
              <ol className="space-y-0.5 font-mono text-micro">
                {task.stages.map((stage, index) => (
                  <li key={index} className="flex gap-3">
                    <span className="text-foreground/85">{stageStep(stage).label}</span>
                    <span className="ml-auto text-faint-foreground">{clockTime(stage.at_ms)}</span>
                    <span className="w-14 text-right text-muted-foreground">
                      {stageDuration(stage.took_ms)}
                    </span>
                  </li>
                ))}
              </ol>
            </Collapsible>
            </div>
          )}
        </section>

        <TaskReviewCliPanel commands={reviewCommands} />

        {task.brief && (
          <Collapsible label="Brief">
            <TaskText
              what="brief"
              excerpt={task.brief}
              bytes={task.brief_bytes}
              full={texts.texts?.brief}
              read={texts}
            />
          </Collapsible>
        )}
        {task.report !== null || (task.report_bytes ?? 0) > 0 ? (
          <Collapsible label="Report" defaultOpen>
            <TaskText
              what="report"
              excerpt={task.report}
              bytes={task.report_bytes}
              full={texts.texts?.report}
              read={texts}
            />
          </Collapsible>
        ) : (
          <p className="text-caption text-faint-foreground">No report yet.</p>
        )}
      </div>
    </motion.aside>
  );
}

function TaskReviewCliPanel({ commands }: { commands: readonly TaskReviewCommand[] }) {
  const [feedback, setFeedback] = useState<{ command: string; state: "copied" | "failed" } | null>(
    null,
  );
  const timerRef = useRef<number | null>(null);

  useEffect(() => {
    setFeedback(null);
    return () => {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, [commands]);

  if (commands.length === 0) return null;

  const copyCommand = (command: string) => {
    const show = (state: "copied" | "failed") => {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current);
      }
      setFeedback({ command, state });
      timerRef.current = window.setTimeout(() => {
        setFeedback(null);
        timerRef.current = null;
      }, 1_500);
    };
    if (navigator.clipboard) {
      void navigator.clipboard.writeText(command).then(
        () => show("copied"),
        () => show(copyCommandText(command) ? "copied" : "failed"),
      );
    } else {
      show(copyCommandText(command) ? "copied" : "failed");
    }
  };

  return (
    <Collapsible label="Review & land CLI">
      <ul className="space-y-1.5">
        {commands.map((item) => {
          const state = feedback?.command === item.command ? feedback.state : null;
          return (
            <li key={item.command} className="space-y-0.5">
              <div className="text-micro text-muted-foreground">{item.label}</div>
              <div className="flex min-w-0 items-center gap-1.5">
                <code className="min-w-0 flex-1 overflow-x-auto whitespace-nowrap rounded bg-muted/60 px-2 py-1 font-mono text-micro text-foreground select-all">
                  {item.command}
                </code>
                <button
                  type="button"
                  onClick={() => copyCommand(item.command)}
                  title={
                    state === "failed"
                      ? `Could not copy; select command: ${item.command}`
                      : `Copy command: ${item.command}`
                  }
                  aria-label={`Copy ${item.command}`}
                  className={cx(
                    "inline-flex h-6 shrink-0 items-center gap-1 rounded-chip px-1.5 text-micro text-faint-foreground hover:bg-surface-hover hover:text-foreground",
                    FOCUS_RING,
                  )}
                >
                  {state === "copied" ? (
                    <Check size={11} aria-hidden="true" />
                  ) : (
                    <Copy size={11} aria-hidden="true" />
                  )}
                  {state === "copied" ? (
                    <span>Copied</span>
                  ) : state === "failed" ? (
                    <span className="text-destructive">Not copied</span>
                  ) : (
                    <span>Copy</span>
                  )}
                </button>
              </div>
            </li>
          );
        })}
      </ul>
    </Collapsible>
  );
}

/** Plain-HTTP clipboard fallback for environments where `navigator.clipboard` is unavailable. */
function copyCommandText(text: string): boolean {
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.appendChild(area);
  area.select();
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } catch {
    copied = false;
  }
  document.body.removeChild(area);
  return copied;
}

function Label({ children }: { children: ReactNode }) {
  return (
    <div className="text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
      {children}
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="tnum min-w-0 text-foreground">{children}</dd>
    </>
  );
}

import { Eye, GitBranch, Terminal, Wrench } from "lucide-react";
import type { ReactNode } from "react";

import type { Execution, Observation, RsiSummary, SessionDetail } from "../api";
import { formatDuration, relativeTime } from "../format";
import { Card, CardHeader, cx } from "./ui";

interface DetailsRailProps {
  rsi: RsiSummary | null;
  detail: SessionDetail | null;
  observation: Observation | null;
  className?: string;
}

/** The session's latest execution, the one its header and drawer name. */
export function latestExecution(detail: SessionDetail | null): Execution | null {
  if (!detail) return null;
  return (
    [...detail.executions].sort((left, right) => right.started_at_ms - left.started_at_ms)[0] ??
    null
  );
}

/** The latest execution in one line, for the session's header
 * (design-ui-information.md §2.6: the Execution card collapsed into it):
 * "claude · running 12m" or "claude · exited 0 · 3m · stopped". The id and
 * the start time are on hover. */
export function ExecutionSummary({
  detail,
  className,
}: {
  detail: SessionDetail | null;
  className?: string;
}) {
  const execution = latestExecution(detail);
  if (!execution) return null;
  const running = execution.ended_at_ms === null;
  const parts: ReactNode[] = [execution.engine || "execution"];
  if (running) {
    parts.push(
      <span key="running" className="text-success">
        running {formatDuration(Date.now() - execution.started_at_ms)}
      </span>,
    );
  } else {
    parts.push(execution.exit_code === null ? "no exit code" : `exited ${execution.exit_code}`);
    parts.push(formatDuration(execution.duration_ms));
    if (execution.cause) parts.push(execution.cause);
  }
  return (
    <span
      className={cx("tnum inline-flex items-center gap-1.5 text-caption text-muted-foreground", className)}
      title={`${execution.execution_id} · started ${relativeTime(execution.started_at_ms)}`}
    >
      <Terminal size={12} aria-hidden="true" className="shrink-0 text-faint-foreground" />
      {parts.map((part, index) => (
        <span key={index} className="inline-flex items-center gap-1.5">
          {index > 0 && <span className="text-faint-foreground">·</span>}
          {part}
        </span>
      ))}
    </span>
  );
}

/** Side panel of the Sessions view; also rendered inside a drawer on phones. */
export function DetailsRail({
  rsi,
  detail,
  observation,
  className,
}: DetailsRailProps) {
  return (
    <aside
      className={cx(
        "flex w-80 shrink-0 flex-col gap-3 overflow-y-auto border-l border-surface-border bg-app-shell p-3",
        className,
      )}
    >
      <RailContent rsi={rsi} detail={detail} observation={observation} />
    </aside>
  );
}

/** The rail's cards (design-ui-information.md §2.6: one useful card at a
 * time): the judgment always; the open tools only while some are open; the
 * RSI card only on the resident's own trace. The execution is in the
 * header (ExecutionSummary); `withExecution` puts it here instead, for the
 * phone drawer, whose header has no room for it. */
export function RailContent({
  rsi,
  detail,
  observation,
  withExecution = false,
}: Omit<DetailsRailProps, "className"> & { withExecution?: boolean }) {
  const tools = detail?.open_tools ?? [];
  const residentRsi =
    rsi && detail && rsi.resident_session !== null && rsi.resident_session === detail.session_id
      ? rsi
      : null;

  return (
    <>
      <Card>
        <CardHeader
          icon={<Eye size={14} />}
          title="Observation"
          meta={
            observation && (
              <span className="font-mono text-micro text-faint-foreground">
                {observation.judge}
              </span>
            )
          }
        />
        <div className="space-y-2 p-4">
          {observation ? (
            <>
              <Field
                label="Judged state"
                value={
                  <span className="font-medium capitalize">
                    {observation.state.toLowerCase()}
                  </span>
                }
              />
              <div>
                <div className="flex items-baseline justify-between gap-3">
                  <span className="text-label text-muted-foreground">
                    Confidence
                  </span>
                  <span className="tnum text-label text-foreground">
                    {Math.round(observation.confidence * 100)}%
                  </span>
                </div>
                <div className="mt-1.5 h-1 overflow-hidden rounded-full bg-foreground/[0.07]">
                  <div
                    className="h-full rounded-full bg-chart-line/70"
                    style={{
                      width: `${Math.min(100, Math.max(0, observation.confidence * 100))}%`,
                    }}
                  />
                </div>
              </div>
              <Field
                label="Based on"
                value={<span className="tnum">seq {observation.based_on_seq}</span>}
              />
              {observation.reason && (
                <p className="rounded-md bg-muted px-2.5 py-2 text-caption text-foreground/80">
                  {observation.reason}
                </p>
              )}
            </>
          ) : (
            <Empty text="No judgment yet." />
          )}
        </div>
      </Card>

      {withExecution && latestExecution(detail) && (
        <Card>
          <div className="px-4 py-3">
            <ExecutionSummary detail={detail} />
          </div>
        </Card>
      )}

      {tools.length > 0 && (
        <Card>
          <CardHeader
            icon={<Wrench size={14} />}
            title="Open tools"
            meta={<span className="tnum">{tools.length}</span>}
          />
          <div className="space-y-1.5 p-3">
            {tools.map((tool) => (
              <div
                key={`${tool.tool_use_id}-${tool.session_seq}`}
                className="rounded-md border border-surface-border bg-page-canvas px-2.5 py-1.5"
              >
                <div className="flex items-center gap-2">
                  <span className="size-1.5 animate-spawn-pulse rounded-full bg-warning" />
                  <span className="text-label font-medium text-foreground">
                    {tool.tool_name}
                  </span>
                  <span className="tnum ml-auto font-mono text-micro text-faint-foreground">
                    seq {tool.session_seq}
                  </span>
                </div>
                <div
                  className="mt-0.5 truncate font-mono text-micro text-muted-foreground"
                  title={tool.tool_use_id}
                >
                  {tool.tool_use_id}
                </div>
              </div>
            ))}
          </div>
        </Card>
      )}

      {residentRsi && (
        <Card>
          <CardHeader icon={<GitBranch size={14} />} title={`RSI · mission #${residentRsi.mission}`} />
          <div className="space-y-2 p-4">
            <Field
              label="Baseline"
              value={
                <span className="font-mono">
                  {residentRsi.baseline ? residentRsi.baseline.slice(0, 8) : "—"}
                </span>
              }
            />
            <Field label="Halted" value={residentRsi.halted ?? "no"} />
            <Field
              label="Task"
              value={
                residentRsi.open_task
                  ? `#${residentRsi.open_task.id} · ${residentRsi.open_task.state}`
                  : "—"
              }
            />
            {residentRsi.open_task && (
              <Field
                label="Objective"
                value={
                  residentRsi.open_task.objective !== null
                    ? String(residentRsi.open_task.objective)
                    : "—"
                }
              />
            )}
            <Field
              label="Last verdict"
              value={residentRsi.open_task?.last_verdict ?? "—"}
            />
            <Field
              label="Objectives"
              wrap
              value={`${residentRsi.objectives.open} open · ${residentRsi.objectives.solved} solved · ${residentRsi.objectives.met} met · ${residentRsi.objectives.owner_judged} owner`}
            />
          </div>
        </Card>
      )}
    </>
  );
}

/** A label and its value on one line; `wrap` lets a long value (a list of
 * counts) take more lines instead of being cut off. */
function Field({ label, value, wrap = false }: { label: string; value: ReactNode; wrap?: boolean }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="shrink-0 text-label text-muted-foreground">{label}</span>
      <span
        className={cx(
          "min-w-0 text-right text-label text-foreground",
          wrap ? "text-pretty" : "truncate",
        )}
      >
        {value}
      </span>
    </div>
  );
}

function Empty({ text }: { text: string }) {
  return <p className="py-1 text-caption text-faint-foreground">{text}</p>;
}

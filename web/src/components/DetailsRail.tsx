import { Eye, GitBranch, Terminal, Wrench } from "lucide-react";
import type { ReactNode } from "react";

import type { Observation, RsiSummary, SessionDetail } from "../api";
import { formatDuration, relativeTime } from "../format";
import { Card, CardHeader, cx } from "./ui";

interface DetailsRailProps {
  rsi: RsiSummary | null;
  detail: SessionDetail | null;
  observation: Observation | null;
  className?: string;
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

export function RailContent({
  rsi,
  detail,
  observation,
}: Omit<DetailsRailProps, "className">) {
  const execution = detail
    ? ([...detail.executions].sort(
        (left, right) => right.started_at_ms - left.started_at_ms,
      )[0] ?? null)
    : null;
  const tools = detail?.open_tools ?? [];

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

      <Card>
        <CardHeader
          icon={<Terminal size={14} />}
          title="Execution"
          meta={
            execution && (
              <span className="font-mono text-micro text-faint-foreground">
                {execution.execution_id}
              </span>
            )
          }
        />
        <div className="space-y-2 p-4">
          {execution ? (
            <>
              <Field label="Engine" value={execution.engine || "—"} />
              <Field
                label="Started"
                value={relativeTime(execution.started_at_ms)}
              />
              <Field
                label="Duration"
                value={
                  execution.ended_at_ms === null ? (
                    <span className="text-success">running</span>
                  ) : (
                    formatDuration(execution.duration_ms)
                  )
                }
              />
              <Field label="Cause" value={execution.cause ?? "—"} />
              <Field
                label="Exit code"
                value={
                  execution.exit_code === null
                    ? "—"
                    : String(execution.exit_code)
                }
              />
            </>
          ) : (
            <Empty text="No executions yet." />
          )}
        </div>
      </Card>

      <Card>
        <CardHeader
          icon={<Wrench size={14} />}
          title="Open tools"
          meta={tools.length > 0 && <span className="tnum">{tools.length}</span>}
        />
        <div className="p-3">
          {tools.length === 0 ? (
            <Empty text="No tools waiting." />
          ) : (
            <div className="space-y-1.5">
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
          )}
        </div>
      </Card>

      {rsi && (
        <Card>
          <CardHeader icon={<GitBranch size={14} />} title={`RSI · mission #${rsi.mission}`} />
          <div className="space-y-2 p-4">
            <Field
              label="Baseline"
              value={
                <span className="font-mono">
                  {rsi.baseline ? rsi.baseline.slice(0, 8) : "—"}
                </span>
              }
            />
            <Field label="Halted" value={rsi.halted ?? "no"} />
            <Field
              label="Task"
              value={
                rsi.open_task
                  ? `#${rsi.open_task.id} · ${rsi.open_task.state}`
                  : "—"
              }
            />
            {rsi.open_task && (
              <Field
                label="Objective"
                value={
                  rsi.open_task.objective !== null
                    ? String(rsi.open_task.objective)
                    : "—"
                }
              />
            )}
            <Field
              label="Last verdict"
              value={rsi.open_task?.last_verdict ?? "—"}
            />
            <Field
              label="Objectives"
              value={`${rsi.objectives.open} open · ${rsi.objectives.solved} solved · ${rsi.objectives.met} met · ${rsi.objectives.owner_judged} owner-judged`}
            />
          </div>
        </Card>
      )}
    </>
  );
}

function Field({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="shrink-0 text-label text-muted-foreground">{label}</span>
      <span className="min-w-0 truncate text-right text-label text-foreground">
        {value}
      </span>
    </div>
  );
}

function Empty({ text }: { text: string }) {
  return <p className="py-1 text-caption text-faint-foreground">{text}</p>;
}

import { ArrowRight, ChartLine, OctagonAlert } from "lucide-react";
import { useState } from "react";
import type { ReactNode } from "react";

import { usePoll } from "../../api";
import { clockTime, formatDuration, relativeTime } from "../../format";
import type { RsiDashboard } from "../../rsi";
import { HALT_LIMIT, fetchDashboard, isTerminalPhase } from "../../rsi";
import { Button, Card, CardHeader, Eyebrow, LiveStatus, PageHeader, cx } from "../ui";
import { AgentPane } from "./AgentPane";
import { IterationDetail, IterationList, iterationDuration } from "./Iterations";
import { ChartLegend, MetricChart } from "./MetricChart";
import { PhaseBadge, formatMetric, formatNumber, phaseStyle, useNow } from "./Phase";
import {
  formatPercent,
  isImprovement,
  percentChange,
  progressToTarget,
  ratchet,
} from "./metric";

interface RsiViewProps {
  onOpenSession: () => void;
}

export function RsiView({ onOpenSession }: RsiViewProps) {
  const dashboard = usePoll<RsiDashboard>("rsi", fetchDashboard, 2_000);
  const [selected, setSelected] = useState<number | null>(null);
  const data = dashboard.data;
  useNow(1_000);
  const summary =
    data?.iterations.find((entry) => entry.iteration === selected) ?? null;

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            RSI loop
          </h1>
        }
      >
        <LiveStatus
          error={dashboard.error}
          label={data ? `Updated ${relativeTime(data.generated_at_ms)}` : "Connecting"}
        />
        <Button onClick={onOpenSession}>
          <span className="hidden sm:inline">Session timeline</span>
          <span className="sm:hidden">Timeline</span>
          <ArrowRight size={12} />
        </Button>
      </PageHeader>

      {data?.state?.halted && (
        <div className="flex shrink-0 items-start gap-3 border-b border-destructive/30 bg-destructive/12 px-4 py-3 sm:px-5">
          <OctagonAlert size={18} className="mt-0.5 shrink-0 text-destructive" />
          <div className="min-w-0">
            <div className="text-body font-semibold text-destructive">
              Loop halted — waiting for a human
            </div>
            <div className="text-label text-foreground/80">
              {data.state.halt_reason ?? "A stop condition was reached."}
            </div>
          </div>
        </div>
      )}

      {!data ? (
        <div className="grid flex-1 place-items-center">
          <p className="text-caption text-faint-foreground">
            {dashboard.error
              ? `Could not reach the RSI API: ${dashboard.error}`
              : "Loading the loop…"}
          </p>
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto xl:flex xl:flex-col xl:overflow-hidden">
          <div className="flex flex-col gap-3 p-3 sm:p-4 xl:min-h-0 xl:flex-1">
            <div className="grid shrink-0 grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-[minmax(0,1.7fr)_minmax(0,1.1fr)_minmax(0,1fr)]">
              <ObjectiveCard data={data} />
              <PhaseCard data={data} />
              <GuardsCard data={data} />
            </div>
            <div className="grid grid-cols-1 gap-3 xl:min-h-0 xl:flex-1 xl:grid-cols-[minmax(0,5fr)_minmax(0,7fr)]">
              <div className="flex flex-col gap-3 xl:min-h-0">
                <HistoryCard data={data} />
                <IterationList
                  className="max-h-[28rem] xl:max-h-none xl:min-h-0 xl:flex-1"
                  iterations={data.iterations}
                  selected={selected}
                  onSelect={(iteration) =>
                    setSelected((current) =>
                      current === iteration ? null : iteration,
                    )
                  }
                />
              </div>
              <AgentPane className="order-first h-[26rem] sm:h-[32rem] xl:order-none xl:h-auto" />
            </div>
          </div>
        </div>
      )}

      {selected !== null && (
        <IterationDetail
          key={selected}
          iteration={selected}
          summary={summary}
          onClose={() => setSelected(null)}
        />
      )}
    </main>
  );
}

function metricPoints(data: RsiDashboard) {
  const metric = data.objective?.metric ?? "";
  return metric
    ? data.metric_history.filter((point) => point.metric === metric)
    : data.metric_history;
}

function metricUnit(data: RsiDashboard): string {
  const metric = data.objective?.metric ?? "";
  return (
    data.objective?.unit ||
    data.attempts.find((attempt) => attempt.metrics[metric])?.metrics[metric]
      ?.unit ||
    ""
  );
}

function ObjectiveCard({ data }: { data: RsiDashboard }) {
  const objective = data.objective;
  if (!objective) {
    return (
      <Card className="p-4 md:col-span-2 xl:col-span-1">
        <Eyebrow>Objective</Eyebrow>
        <p className="mt-2 text-label text-muted-foreground">
          No objective set. Add a task to the backlog.
        </p>
      </Card>
    );
  }
  const unit = metricUnit(data);
  const series = ratchet(metricPoints(data));
  const change = percentChange(series.start, series.best);
  const better = isImprovement(change, objective.direction);
  const progress = progressToTarget(
    series.start,
    series.best,
    objective.target,
    objective.direction,
  );

  return (
    <Card className="flex flex-col p-4 md:col-span-2 xl:col-span-1">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <Eyebrow>Objective · task {objective.task_id}</Eyebrow>
          <h2 className="mt-0.5 truncate text-title-sm font-semibold text-foreground">
            {objective.title}
          </h2>
        </div>
        {objective.metric && (
          <span className="mt-0.5 shrink-0 rounded-md bg-muted px-2 py-0.5 font-mono text-caption text-muted-foreground">
            {objective.metric}
          </span>
        )}
      </div>

      <div className="mt-4 grid grid-cols-3 gap-3">
        <Figure label="Start" value={formatNumber(series.start)} unit={unit} />
        <Figure
          label="Current"
          value={formatNumber(series.best)}
          unit={unit}
          note={
            change !== null ? (
              <span className={better ? "text-success" : "text-muted-foreground"}>
                {formatPercent(change)}
              </span>
            ) : null
          }
        />
        <Figure
          label="Target"
          value={formatNumber(objective.target)}
          unit={unit}
        />
      </div>

      <div className="mt-auto pt-4">
        <div className="relative h-1.5 overflow-hidden rounded-full bg-white/[0.06]">
          {progress === null ? (
            <div className="bg-hatch absolute inset-0" />
          ) : (
            <div
              className="absolute inset-y-0 left-0 rounded-full bg-success transition-[width] duration-500"
              style={{ width: `${Math.max(progress * 100, 1.5)}%` }}
            />
          )}
        </div>
        <div className="mt-1.5 flex justify-between text-caption text-faint-foreground">
          <span className="tnum">
            {progress === null
              ? "Awaiting the first gated measurement"
              : progress >= 1
                ? "Target reached"
                : `${Math.round(progress * 100)}% of the way to target`}
          </span>
          <span>{objective.direction} is better</span>
        </div>
      </div>
    </Card>
  );
}

function Figure({
  label,
  value,
  unit,
  note,
}: {
  label: string;
  value: string;
  unit: string;
  note?: ReactNode;
}) {
  return (
    <div className="min-w-0">
      <Eyebrow>{label}</Eyebrow>
      <div className="mt-0.5 flex items-baseline gap-1">
        <span className="tnum text-title-lg font-semibold text-foreground">
          {value}
        </span>
        {value !== "—" && unit && (
          <span className="text-caption text-muted-foreground">{unit}</span>
        )}
      </div>
      {note && <div className="tnum text-caption">{note}</div>}
    </div>
  );
}

const STEPS = ["Starting", "Working", "Gating", "Settled"] as const;

function stepIndex(phase: string): number {
  if (phase === "Starting") return 0;
  if (phase === "Working") return 1;
  if (phase === "Gating") return 2;
  return 3;
}

function PhaseCard({ data }: { data: RsiDashboard }) {
  const now = useNow(1_000);
  const latest = data.iterations[0] ?? null;
  const open = latest && !isTerminalPhase(latest.phase) ? latest : null;
  const shown = open ?? latest;
  const style = shown ? phaseStyle(shown.phase) : null;
  const current = shown ? stepIndex(shown.phase) : -1;

  return (
    <Card className="flex flex-col p-4">
      <div className="flex items-center gap-2">
        <Eyebrow className="flex-1">
          {open ? "Current iteration" : "Last iteration"}
        </Eyebrow>
        {shown && (
          <span className="tnum text-caption text-muted-foreground">
            #{shown.iteration} · task {shown.task_id}
          </span>
        )}
      </div>
      {shown ? (
        <>
          <div className="mt-2 flex items-center gap-3">
            <PhaseBadge phase={shown.phase} size="md" />
            <span className="tnum ml-auto font-mono text-title-lg font-medium text-foreground">
              {formatDuration(iterationDuration(shown, now))}
            </span>
          </div>
          <div className="mt-auto pt-4">
            <div className="grid grid-cols-4 gap-1">
              {STEPS.map((step, index) => (
                <div
                  key={step}
                  className={cx(
                    "h-1 rounded-full",
                    index < current
                      ? "bg-foreground/35"
                      : index === current
                        ? style?.bar
                        : "bg-white/[0.07]",
                  )}
                />
              ))}
            </div>
            <div className="mt-1.5 flex justify-between text-caption text-faint-foreground">
              <span className="tnum">
                started {clockTime(shown.started_at_ms)}
              </span>
              <span>{open ? "in progress" : relativeTime(shown.updated_at_ms)}</span>
            </div>
          </div>
        </>
      ) : (
        <p className="mt-2 text-label text-muted-foreground">
          Idle — no iteration yet.
        </p>
      )}
    </Card>
  );
}

function GuardsCard({ data }: { data: RsiDashboard }) {
  const state = data.state;
  const halted = state?.halted === true;
  return (
    <Card
      className={cx(
        "flex flex-col p-4",
        halted && "border-destructive/40 bg-destructive/[0.06]",
      )}
    >
      <div className="flex items-center gap-2">
        <Eyebrow className="flex-1">Halt guards</Eyebrow>
        <span
          className={cx(
            "inline-flex items-center gap-1.5 text-caption font-medium",
            halted ? "text-destructive" : "text-muted-foreground",
          )}
        >
          <span
            className={cx(
              "size-1.5 rounded-full",
              halted ? "bg-destructive" : "bg-success",
            )}
          />
          {halted ? "Halted" : "Running"}
        </span>
      </div>
      <div className="mt-3 space-y-2.5">
        <Guard label="Rejected in a row" value={state?.consecutive_rejected ?? 0} />
        <Guard label="Aborts in a row" value={state?.infra_failures ?? 0} />
      </div>
      <div className="mt-auto flex justify-between gap-2 pt-3 text-caption text-faint-foreground">
        <span className="truncate">
          baseline{" "}
          <span className="font-mono text-muted-foreground">
            {state?.baseline ? state.baseline.slice(0, 7) : "—"}
          </span>
        </span>
        <span className="tnum shrink-0">
          {state ? `${state.scenario_count} scenarios` : ""}
        </span>
      </div>
    </Card>
  );
}

function Guard({ label, value }: { label: string; value: number }) {
  const tone =
    value >= HALT_LIMIT ? "bg-destructive" : value > 0 ? "bg-warning" : "";
  const text =
    value >= HALT_LIMIT
      ? "text-destructive"
      : value > 0
        ? "text-warning"
        : "text-muted-foreground";
  return (
    <div className="flex items-center gap-3">
      <span className="min-w-0 flex-1 truncate text-label text-foreground/85">
        {label}
      </span>
      <span className="flex gap-1" aria-hidden="true">
        {Array.from({ length: HALT_LIMIT }, (_, index) => (
          <span
            key={index}
            className={cx(
              "h-3 w-1.5 rounded-sm",
              index < value ? tone : "bg-white/[0.08]",
            )}
          />
        ))}
      </span>
      <span className={cx("tnum w-8 text-right font-mono text-caption", text)}>
        {value}/{HALT_LIMIT}
      </span>
    </div>
  );
}

function HistoryCard({ data }: { data: RsiDashboard }) {
  const objective = data.objective;
  const unit = metricUnit(data);
  const metric = objective?.metric ?? "";
  const latest = data.attempts.find((attempt) => attempt.metrics[metric]);
  const pair = latest?.metrics[metric] ?? null;

  return (
    <Card className="shrink-0 overflow-hidden">
      <CardHeader
        icon={<ChartLine size={14} />}
        title="Metric history"
        meta={<ChartLegend hasTarget={objective?.target != null} />}
        className="[&>div]:hidden sm:[&>div]:flex"
      />
      <div className="px-2 pt-3 pb-1">
        <MetricChart
          points={metricPoints(data)}
          unit={unit}
          direction={objective?.direction ?? "lower"}
          target={objective?.target ?? null}
        />
      </div>
      {latest && pair && (
        <div className="flex items-center gap-2 border-t border-surface-border px-4 py-2 text-caption text-muted-foreground">
          <span>Last gate #{latest.iteration}</span>
          <span className="tnum font-mono">{formatMetric(pair.baseline, unit)}</span>
          <ArrowRight size={11} className="text-faint-foreground" />
          <span
            className={cx(
              "tnum font-mono",
              latest.accepted ? "text-success" : "text-destructive",
            )}
          >
            {formatMetric(pair.candidate, unit)}
          </span>
          <span className="ml-auto">{latest.accepted ? "landed" : "rejected"}</span>
        </div>
      )}
    </Card>
  );
}

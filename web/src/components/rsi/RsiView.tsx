import { ArrowRight, Gauge, OctagonAlert, Target, WifiOff } from "lucide-react";
import { useState } from "react";
import type { ReactNode } from "react";

import { usePoll } from "../../api";
import { formatDuration, relativeTime } from "../../format";
import type { RsiDashboard } from "../../rsi";
import { HALT_LIMIT, fetchDashboard, isTerminalPhase } from "../../rsi";
import { AgentPane } from "./AgentPane";
import {
  IterationDetail,
  IterationList,
  iterationDuration,
} from "./Iterations";
import { MetricChart } from "./MetricChart";
import { PhaseBadge, formatMetric, useNow } from "./Phase";

interface RsiViewProps {
  onOpenSession: () => void;
}

export function RsiView({ onOpenSession }: RsiViewProps) {
  const dashboard = usePoll<RsiDashboard>("rsi", fetchDashboard, 2_000);
  const [selected, setSelected] = useState<number | null>(null);
  const data = dashboard.data;
  const selectedPhase =
    data?.iterations.find((entry) => entry.iteration === selected)?.phase ??
    null;

  return (
    <main className="flex min-w-0 flex-1 flex-col bg-page-canvas">
      <RsiHeader
        data={data}
        error={dashboard.error}
        onOpenSession={onOpenSession}
      />

      {data?.state?.halted && (
        <div className="flex items-start gap-3 border-b border-destructive/30 bg-destructive/10 px-5 py-3 text-destructive">
          <OctagonAlert size={18} className="mt-0.5 shrink-0" />
          <div>
            <div className="text-body font-semibold tracking-wide">
              HALTED — the loop is waiting for a human
            </div>
            <div className="text-label">
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
        <div className="grid min-h-0 flex-1 grid-cols-1 gap-3 overflow-y-auto p-3 xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)] xl:overflow-hidden">
          <div className="flex min-h-0 flex-col gap-3 xl:overflow-y-auto">
            <ObjectiveCard data={data} />
            <IterationList
              iterations={data.iterations}
              selected={selected}
              onSelect={(iteration) =>
                setSelected((current) =>
                  current === iteration ? null : iteration,
                )
              }
            />
          </div>
          <div className="flex min-h-[32rem] flex-col xl:min-h-0">
            <AgentPane />
          </div>
        </div>
      )}

      {selected !== null && (
        <IterationDetail
          key={selected}
          iteration={selected}
          phase={selectedPhase}
          onClose={() => setSelected(null)}
        />
      )}
    </main>
  );
}

function RsiHeader({
  data,
  error,
  onOpenSession,
}: {
  data: RsiDashboard | null;
  error: string | null;
  onOpenSession: () => void;
}) {
  const now = useNow(1_000);
  const latest = data?.iterations[0] ?? null;
  const open = latest && !isTerminalPhase(latest.phase) ? latest : null;
  const state = data?.state ?? null;

  return (
    <header className="shrink-0 border-b border-surface-border bg-page-canvas px-5 py-3">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <Target size={16} className="text-muted-foreground" />
        <h1 className="min-w-0 truncate text-title-sm font-medium text-foreground">
          {data?.objective?.title ?? "RSI loop"}
        </h1>
        {data?.objective?.metric && (
          <span className="rounded-sm border border-surface-border bg-surface px-1.5 py-0.5 font-mono text-micro text-muted-foreground">
            {data.objective.metric}
            {data.objective.unit ? ` · ${data.objective.unit}` : ""}
          </span>
        )}
        {open ? (
          <span className="inline-flex items-center gap-2">
            <PhaseBadge phase={open.phase} />
            <span className="font-mono text-caption text-muted-foreground">
              #{open.iteration} · {formatDuration(iterationDuration(open, now))}
            </span>
          </span>
        ) : (
          <span className="text-caption text-faint-foreground">
            {latest ? "idle · no open iteration" : "idle"}
          </span>
        )}
        <div className="ml-auto flex items-center gap-3">
          <button
            type="button"
            onClick={onOpenSession}
            className="inline-flex items-center gap-1 rounded-md border border-surface-border bg-surface px-2 py-1 text-caption text-muted-foreground hover:bg-surface-hover hover:text-foreground"
          >
            Session timeline
            <ArrowRight size={12} />
          </button>
          {error && (
            <span
              title={error}
              className="inline-flex items-center gap-1 rounded-sm border border-destructive/30 bg-destructive/10 px-1.5 py-0.5 text-micro font-medium text-destructive"
            >
              <WifiOff size={11} />
              disconnected
            </span>
          )}
        </div>
      </div>

      <div className="mt-2.5 flex flex-wrap items-center gap-x-5 gap-y-1.5">
        <Metric label="Baseline">
          <span className="font-mono">
            {state?.baseline ? state.baseline.slice(0, 10) : "—"}
          </span>
        </Metric>
        <Metric label="Iteration">
          {state ? String(state.iteration) : "—"}
        </Metric>
        <Counter
          label="Rejected in a row"
          value={state?.consecutive_rejected ?? 0}
        />
        <Counter label="Aborts in a row" value={state?.infra_failures ?? 0} />
        <Metric label="Scenarios">
          {state ? String(state.scenario_count) : "—"}
        </Metric>
        <span className="ml-auto text-micro text-faint-foreground">
          updated {relativeTime(data?.generated_at_ms)}
        </span>
      </div>
    </header>
  );
}

function Metric({ label, children }: { label: string; children: ReactNode }) {
  return (
    <span className="inline-flex items-baseline gap-1.5">
      <span className="text-micro uppercase tracking-[0.12em] text-faint-foreground">
        {label}
      </span>
      <span className="text-label text-foreground">{children}</span>
    </span>
  );
}

function Counter({ label, value }: { label: string; value: number }) {
  const tone =
    value >= HALT_LIMIT
      ? "text-destructive"
      : value > 0
        ? "text-warning"
        : "text-foreground";
  return (
    <Metric label={label}>
      <span className={`font-mono ${tone}`}>
        {value} / {HALT_LIMIT}
      </span>
    </Metric>
  );
}

function ObjectiveCard({ data }: { data: RsiDashboard }) {
  const objective = data.objective;
  const metric = objective?.metric ?? "";
  const points = metric
    ? data.metric_history.filter((point) => point.metric === metric)
    : data.metric_history;
  const unit =
    objective?.unit ||
    data.attempts.find((attempt) => attempt.metrics[metric])?.metrics[metric]
      ?.unit ||
    "";
  const latest = data.attempts.find((attempt) => attempt.metrics[metric]);
  const pair = latest?.metrics[metric] ?? null;

  return (
    <section className="rounded-xl border border-surface-border bg-surface p-3.5">
      <header className="mb-2.5 flex items-center gap-1.5 text-micro font-medium uppercase tracking-[0.12em] text-faint-foreground">
        <Gauge size={12} />
        Objective
        {objective && (
          <span className="normal-case tracking-normal">
            · task {objective.task_id}
          </span>
        )}
      </header>
      {objective ? (
        <>
          {objective.prompt && (
            <details className="mb-3">
              <summary className="cursor-pointer text-caption text-muted-foreground hover:text-foreground">
                Task prompt
              </summary>
              <p className="mt-1.5 whitespace-pre-wrap text-caption text-muted-foreground">
                {objective.prompt}
              </p>
            </details>
          )}
          <MetricChart
            points={points}
            unit={unit}
            direction={objective.direction}
          />
          {latest && pair && (
            <p className="mt-2 text-caption text-muted-foreground">
              Last gate (#{latest.iteration}):{" "}
              <span className="font-mono">
                {formatMetric(pair.baseline, unit)}
              </span>{" "}
              →{" "}
              <span
                className={`font-mono ${
                  latest.accepted ? "text-success" : "text-destructive"
                }`}
              >
                {formatMetric(pair.candidate, unit)}
              </span>{" "}
              {latest.accepted ? "landed" : "rejected"}
            </p>
          )}
        </>
      ) : (
        <p className="py-2 text-center text-caption text-faint-foreground">
          No objective set. Add a task to the backlog.
        </p>
      )}
    </section>
  );
}

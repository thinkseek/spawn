import { ArrowRight, ChartLine, ListChecks, OctagonAlert } from "lucide-react";
import { useState } from "react";
import type { ReactNode } from "react";

import { usePoll } from "../../api";
import { clockTime, formatDuration, relativeTime } from "../../format";
import { freshAt } from "../../validated";
import type { RsiDashboard, RsiMission, RsiObjective, RsiTask } from "../../rsi";
import {
  chartObjective,
  fetchDashboard,
  focusObjective,
  metricPoints,
  openTask,
} from "../../rsi";
import { Button, Card, CardHeader, Eyebrow, LiveStatus, PageHeader, cx } from "../ui";
import { AgentPane } from "./AgentPane";
import { ObjectiveName } from "./ObjectiveName";
import { ChartLegend, MetricChart } from "./MetricChart";
import { PhaseBadge, VerdictBadge, formatMetric, formatNumber, phaseStyle, useNow } from "./Phase";
import { GateVerdicts } from "./GateVerdicts";
import { TaskDetail } from "./Verdicts";
import {
  formatPercent,
  isImprovement,
  percentChange,
  progressToTarget,
  ratchet,
} from "./metric";

interface RsiViewProps {
  /** Open a session's timeline (the mission resident's). */
  onOpenSession: (sessionId: string) => void;
}

/** The RSI objective mission(s): objectives from the backlog, the task in
 * flight, what the gate said, and the resident's terminal. */
export function RsiView({ onOpenSession }: RsiViewProps) {
  const dashboard = usePoll<RsiDashboard>("rsi", fetchDashboard, 2_000, { checked: true });
  const [chosenMission, setChosenMission] = useState<number | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [chosenObjective, setChosenObjective] = useState<number | null>(null);
  const data = dashboard.data;
  useNow(1_000);
  const mission =
    data?.missions.find((entry) => entry.id === chosenMission) ??
    data?.missions[0] ??
    null;
  const summary = mission?.tasks.find((task) => task.id === selected) ?? null;
  const residentSession = mission?.resident?.session_id ?? null;

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            RSI mission{mission ? ` #${mission.id}` : ""}
          </h1>
        }
      >
        {data && data.missions.length > 1 && (
          <div className="flex rounded-lg border border-surface-border bg-surface p-0.5">
            {data.missions.map((entry) => (
              <button
                key={entry.id}
                type="button"
                onClick={() => {
                  setChosenMission(entry.id);
                  setSelected(null);
                  setChosenObjective(null);
                }}
                aria-current={entry.id === mission?.id ? "true" : undefined}
                className={cx(
                  "tnum h-6 rounded-md px-2 text-caption font-medium",
                  entry.id === mission?.id
                    ? "bg-surface-selected text-foreground"
                    : "text-muted-foreground",
                )}
              >
                #{entry.id}
              </button>
            ))}
          </div>
        )}
        <LiveStatus
          error={dashboard.error}
          label={
            data
              ? `Updated ${relativeTime(freshAt(data.generated_at_ms, dashboard.checkedAt))}`
              : "Connecting"
          }
        />
        {residentSession && (
          <Button onClick={() => onOpenSession(residentSession)}>
            <span className="hidden sm:inline">Resident timeline</span>
            <span className="sm:hidden">Timeline</span>
            <ArrowRight size={12} />
          </Button>
        )}
      </PageHeader>

      {mission?.halted && (
        <div className="flex shrink-0 items-start gap-3 border-b border-destructive/30 bg-destructive/12 px-4 py-3 sm:px-5">
          <OctagonAlert size={18} className="mt-0.5 shrink-0 text-destructive" />
          <div className="min-w-0">
            <div className="text-body font-semibold text-destructive">
              Mission halted — nothing lands until a human runs{" "}
              <code className="font-mono">spawn mission resume --id {mission.id}</code>
            </div>
            <div className="text-label text-foreground/80">{mission.halted}</div>
          </div>
        </div>
      )}

      {!data ? (
        <div className="grid flex-1 place-items-center">
          <p className="text-caption text-faint-foreground">
            {dashboard.error
              ? `Could not reach the RSI API: ${dashboard.error}`
              : "Loading the mission…"}
          </p>
        </div>
      ) : !mission ? (
        <div className="grid flex-1 place-items-center px-6 text-center">
          <p className="max-w-md text-caption text-faint-foreground">
            No open objective mission. RSI runs as a mission with a backlog
            and a resident (spawn mission configure --backlog … --resident …).
          </p>
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto xl:flex xl:flex-col xl:overflow-hidden">
          <div className="flex flex-col gap-3 p-3 sm:p-4 xl:min-h-0 xl:flex-1">
            <div className="grid shrink-0 grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-[minmax(0,1.7fr)_minmax(0,1.1fr)_minmax(0,1fr)]">
              <ObjectiveCard mission={mission} />
              <TaskCard mission={mission} onSelect={setSelected} />
              <GuardsCard mission={mission} haltAfter={data.halt_after} />
            </div>
            <div className="grid grid-cols-1 gap-3 xl:min-h-0 xl:flex-1 xl:grid-cols-[minmax(0,5fr)_minmax(0,7fr)]">
              <div className="flex flex-col gap-3 xl:min-h-0 xl:overflow-y-auto">
                <HistoryCard
                  mission={mission}
                  chosen={chosenObjective}
                />
                <ObjectivesCard
                  mission={mission}
                  chosen={chosenObjective}
                  onChoose={setChosenObjective}
                  onSelectTask={setSelected}
                />
                <GateVerdicts
                  className="shrink-0"
                  mission={mission}
                  selected={selected}
                  onSelect={(task) =>
                    setSelected((current) => (current === task ? null : task))
                  }
                />
              </div>
              <AgentPane
                mission={mission.id}
                resident={mission.resident?.name ?? null}
                className="order-first h-[26rem] sm:h-[32rem] xl:order-none xl:h-auto"
              />
            </div>
          </div>
        </div>
      )}

      {selected !== null && (
        <TaskDetail
          key={selected}
          task={selected}
          summary={summary}
          onClose={() => setSelected(null)}
        />
      )}
    </main>
  );
}

function ObjectiveCard({ mission }: { mission: RsiMission }) {
  const objective = focusObjective(mission);
  if (!objective) {
    return (
      <Card className="p-4 md:col-span-2 xl:col-span-1">
        <Eyebrow>Objective</Eyebrow>
        <p className="mt-2 text-label text-muted-foreground">
          {mission.backlog_error
            ? `Could not read the backlog: ${mission.backlog_error}`
            : "The backlog is empty. Add an objective to rsi/backlog."}
        </p>
      </Card>
    );
  }
  const series = ratchet(metricPoints(objective));
  const current = objective.current ?? series.best;
  const change = percentChange(series.start, current);
  const better = isImprovement(change, objective.direction);
  const progress = objective.metric
    ? progressToTarget(series.start, current, objective.target, objective.direction)
    : null;
  const working = openTask(mission)?.objective === objective.id;

  return (
    <Card className="flex flex-col p-4 md:col-span-2 xl:col-span-1">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <Eyebrow>
            Objective {objective.id} ·{" "}
            {working
              ? "in progress"
              : objective.status === "open"
                ? mission.next_objective === objective.id
                  ? "next"
                  : "open"
                : objective.status}
          </Eyebrow>
          <ObjectiveName
            heading
            mission={mission.id}
            objective={objective}
            className="mt-0.5 truncate text-title-sm font-semibold text-foreground"
          />
        </div>
        <StatusPill status={objective.status} />
      </div>

      {objective.metric ? (
        <>
          <div className="mt-4 grid grid-cols-3 gap-3">
            <Figure label="Start" value={formatNumber(series.start)} unit={objective.unit} />
            <Figure
              label="Current"
              value={formatNumber(current)}
              unit={objective.unit}
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
              unit={objective.unit}
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
                  ? "Awaiting the first landing"
                  : progress >= 1
                    ? "Target reached"
                    : `${Math.round(progress * 100)}% of the way to target`}
              </span>
              <span className="font-mono">
                {objective.metric} · {objective.direction} is better
              </span>
            </div>
          </div>
        </>
      ) : (
        <p className="mt-3 line-clamp-4 whitespace-pre-wrap text-label text-muted-foreground">
          {objective.prompt || "No prompt."}
        </p>
      )}
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

/** The steps of a task the gate alone accepts. */
const STEPS = ["assigned", "delivered", "reported", "landing", "landed"] as const;

function stepIndex(state: string): number {
  switch (state) {
    case "assigned":
      return 0;
    case "delivered":
    case "revise":
      return 1;
    case "reported":
    case "accepted":
      return 2;
    case "landing":
      return 3;
    case "landed":
      return 4;
    default:
      return -1;
  }
}

function TaskCard({
  mission,
  onSelect,
}: {
  mission: RsiMission;
  onSelect: (task: number) => void;
}) {
  const now = useNow(1_000);
  const open = openTask(mission);
  const shown: RsiTask | null = open ?? mission.tasks[0] ?? null;
  const style = shown ? phaseStyle(shown.state) : null;
  const current = shown ? stepIndex(shown.state) : -1;
  const last = shown?.gates[shown.gates.length - 1] ?? null;

  return (
    <Card className="flex flex-col p-4">
      <div className="flex items-center gap-2">
        <Eyebrow className="flex-1">{open ? "Current task" : "Last task"}</Eyebrow>
        {shown && (
          <button
            type="button"
            onClick={() => onSelect(shown.id)}
            className="tnum text-caption text-info hover:underline"
          >
            #{shown.id}
            {shown.objective !== null && ` · objective ${shown.objective}`}
          </button>
        )}
      </div>
      {shown ? (
        <>
          <div className="mt-2 flex items-center gap-3">
            <PhaseBadge phase={shown.state} size="md" />
            {shown.revisions > 0 && (
              <span className="tnum text-caption text-warning">
                {shown.revisions} revision{shown.revisions === 1 ? "" : "s"}
              </span>
            )}
            <span className="tnum ml-auto font-mono text-title-lg font-medium text-foreground">
              {formatDuration(
                Math.max(0, (open ? now : shown.updated_at_ms) - shown.created_at_ms),
              )}
            </span>
          </div>
          {last && (
            <div className="mt-2 flex min-w-0 items-center gap-2 text-caption">
              <VerdictBadge verdict={last.verdict} />
              <span
                className="min-w-0 truncate text-muted-foreground"
                title={last.reason ?? last.metric ?? undefined}
              >
                {last.reason ?? last.metric ?? relativeTime(last.at_ms)}
              </span>
            </div>
          )}
          <div className="mt-auto pt-4">
            <div className="grid grid-cols-5 gap-1">
              {STEPS.map((step, index) => (
                <div
                  key={step}
                  title={step}
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
              <span className="tnum">assigned {clockTime(shown.created_at_ms)}</span>
              <span>{open ? "in progress" : relativeTime(shown.updated_at_ms)}</span>
            </div>
          </div>
        </>
      ) : (
        <p className="mt-2 text-label text-muted-foreground">
          Idle — the integrator assigns the next open objective.
        </p>
      )}
    </Card>
  );
}

function GuardsCard({
  mission,
  haltAfter,
}: {
  mission: RsiMission;
  haltAfter: number;
}) {
  const halted = mission.halted !== null;
  const budget = mission.budget;
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
        <Guard label="Rejected in a row" value={mission.gate_rejections} limit={haltAfter} />
        <Guard label="Aborts in a row" value={mission.gate_aborts} limit={haltAfter} />
      </div>
      <div className="mt-auto space-y-0.5 pt-3 text-caption text-faint-foreground">
        <div className="flex justify-between gap-2">
          <span className="truncate">
            baseline{" "}
            <span className="font-mono text-muted-foreground" title={mission.baseline ?? undefined}>
              {mission.baseline ? mission.baseline.slice(0, 7) : "—"}
            </span>
          </span>
          <span className="tnum shrink-0">
            {mission.scenario_floor > 0 ? `≥ ${mission.scenario_floor} scenarios` : ""}
          </span>
        </div>
        <div className="flex justify-between gap-2">
          <span>accept: {mission.accept ?? "—"}</span>
          {budget && (
            <span className="tnum shrink-0">
              budget{" "}
              {[
                budget.minutes !== null ? `${budget.minutes} min` : null,
                budget.tool_calls !== null ? `${budget.tool_calls} calls` : null,
              ]
                .filter(Boolean)
                .join(" · ") || "—"}
            </span>
          )}
        </div>
      </div>
    </Card>
  );
}

function Guard({
  label,
  value,
  limit,
}: {
  label: string;
  value: number;
  limit: number;
}) {
  const tone = value >= limit ? "bg-destructive" : value > 0 ? "bg-warning" : "";
  const text =
    value >= limit
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
        {Array.from({ length: limit }, (_, index) => (
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
        {value}/{limit}
      </span>
    </div>
  );
}

const STATUS_STYLE: Record<RsiObjective["status"], string> = {
  open: "bg-info/12 text-info ring-info/25",
  solved: "bg-success/12 text-success ring-success/25",
  met: "bg-success/12 text-success ring-success/25",
};

function StatusPill({ status }: { status: RsiObjective["status"] }) {
  return (
    <span
      className={cx(
        "inline-flex h-5 shrink-0 items-center rounded-md px-1.5 text-micro font-medium ring-1 ring-inset",
        STATUS_STYLE[status],
      )}
    >
      {status}
    </span>
  );
}

/** Every backlog objective with its status and tasks. */
function ObjectivesCard({
  mission,
  chosen,
  onChoose,
  onSelectTask,
}: {
  mission: RsiMission;
  chosen: number | null;
  onChoose: (objective: number | null) => void;
  onSelectTask: (task: number) => void;
}) {
  const charted = chosen ?? chartObjective(mission)?.id ?? null;
  return (
    <Card className="shrink-0 overflow-hidden">
      <CardHeader
        icon={<ListChecks size={14} />}
        title="Objectives"
        meta={
          <span className="tnum">
            {mission.objectives.filter((entry) => entry.status !== "open").length}/
            {mission.objectives.length} settled
          </span>
        }
      />
      {mission.backlog_error && (
        <p className="border-b border-surface-border px-4 py-2 text-caption text-destructive">
          Could not read the backlog: {mission.backlog_error}
        </p>
      )}
      {mission.objectives.length === 0 ? (
        <p className="px-4 py-3 text-caption text-faint-foreground">
          No objectives in {mission.backlog ?? "the backlog"}.
        </p>
      ) : (
        <ul className="divide-y divide-surface-border">
          {mission.objectives.map((objective) => (
            <li key={objective.id} className="flex flex-wrap items-center gap-2 px-4 py-2">
              <span className="tnum w-6 font-mono text-caption text-faint-foreground">
                {objective.id}
              </span>
              <ObjectiveName
                mission={mission.id}
                objective={objective}
                className="min-w-0 flex-1 truncate text-label text-foreground/90"
              />
              {objective.metric && (
                <button
                  type="button"
                  onClick={() => onChoose(objective.id === chosen ? null : objective.id)}
                  aria-pressed={charted === objective.id}
                  title="Show this objective's landings in the chart"
                  className={cx(
                    "tnum rounded px-1 font-mono text-caption",
                    charted === objective.id
                      ? "bg-surface-selected text-foreground"
                      : "text-muted-foreground hover:text-foreground",
                  )}
                >
                  {formatMetric(objective.current, objective.unit)}
                </button>
              )}
              <span className="flex gap-1">
                {objective.tasks.slice(0, 4).map((task) => (
                  <button
                    key={task}
                    type="button"
                    onClick={() => onSelectTask(task)}
                    className="tnum font-mono text-micro text-info hover:underline"
                  >
                    T{task}
                  </button>
                ))}
                {objective.tasks.length > 4 && (
                  <span className="text-micro text-faint-foreground">
                    +{objective.tasks.length - 4}
                  </span>
                )}
              </span>
              <StatusPill status={objective.status} />
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

function HistoryCard({
  mission,
  chosen,
}: {
  mission: RsiMission;
  chosen: number | null;
}) {
  const objective =
    mission.objectives.find((entry) => entry.id === chosen && entry.metric) ??
    chartObjective(mission);
  const latest = objective?.landings[objective.landings.length - 1] ?? null;
  const unit = objective?.unit ?? "";

  return (
    <Card className="shrink-0 overflow-hidden">
      <CardHeader
        icon={<ChartLine size={14} />}
        title={
          objective?.metric
            ? `Landings · ${objective.metric}`
            : "Landings"
        }
        meta={<ChartLegend hasTarget={objective?.target != null} />}
        className="[&>div]:hidden sm:[&>div]:flex"
      />
      <div className="px-2 pt-3 pb-1">
        <MetricChart
          points={metricPoints(objective)}
          unit={unit}
          direction={objective?.direction ?? "lower"}
          target={objective?.target ?? null}
        />
      </div>
      {latest && (latest.baseline !== null || latest.candidate !== null) && (
        <div className="flex items-center gap-2 border-t border-surface-border px-4 py-2 text-caption text-muted-foreground">
          <span>Last landing T{latest.task}</span>
          <span className="tnum font-mono">{formatMetric(latest.baseline, unit)}</span>
          <ArrowRight size={11} className="text-faint-foreground" />
          <span className="tnum font-mono text-success">
            {formatMetric(latest.candidate, unit)}
          </span>
          <span className="ml-auto">{relativeTime(latest.at_ms)}</span>
        </div>
      )}
    </Card>
  );
}

import { ChevronRight } from "lucide-react";
import type { ReactNode } from "react";

import type { DiskPlace, FleetHealth, LadderStep } from "../../fleet";
import { FLEET_STATES, stateLabelUpper } from "../../fleet";
import { formatDuration } from "../../format";
import { cx } from "../ui";
import { FleetStateDot } from "./bits";

const LADDER: Array<{
  step: LadderStep | "liveness";
  label: string;
  bar: string;
  hint: string;
}> = [
  {
    step: "rules",
    label: "Rules",
    bar: "bg-muted-foreground/70",
    hint: "Settled by deterministic rules",
  },
  {
    step: "model",
    label: "Jev",
    bar: "bg-jev",
    hint: "Ambiguous cases settled by the model judge",
  },
  {
    step: "dispatcher",
    label: "Dispatcher",
    bar: "bg-info",
    hint: "Routine cases the dispatcher handled",
  },
  {
    step: "supervisor",
    label: "Supervisor",
    bar: "bg-chart-line",
    hint: "Decisions escalated to the supervisor agent",
  },
  {
    step: "human",
    label: "Human",
    bar: "bg-warning",
    hint: "Decisions that needed a person",
  },
];

const LIVENESS = {
  label: "Liveness",
  bar: "bg-faint-foreground",
  hint: "Engines restarted automatically by the liveness check",
};

function Cell({
  label,
  children,
  className,
}: {
  label: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={cx("min-w-0 bg-surface px-4 py-3", className)}>
      <div className="mb-1.5 truncate text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        {label}
      </div>
      {children}
    </div>
  );
}

function Stat({
  label,
  value,
  tone,
  title,
}: {
  label: string;
  value: ReactNode;
  tone?: string;
  title?: string;
}) {
  return (
    <span className="inline-flex items-baseline gap-1" title={title}>
      <span className={cx("tnum font-medium text-foreground", tone)}>
        {value}
      </span>
      <span className="text-faint-foreground">{label}</span>
    </span>
  );
}

function formatGib(bytes: number | null): string {
  return bytes == null ? "—" : `${(bytes / 1024 ** 3).toFixed(1)} GiB`;
}

function DiskLine({ disk }: { disk: DiskPlace[] }) {
  if (disk.length === 0) return null;
  return (
    <div className="mt-1.5 flex flex-wrap gap-x-2.5 gap-y-1 text-caption">
      {disk.map((place) => (
        <Stat
          key={`${place.label}:${place.path}`}
          label={`free · ${place.label}`}
          value={formatGib(place.free_bytes)}
          tone={
            place.free_bytes == null
              ? "text-faint-foreground"
              : place.free_bytes < place.floor_bytes
                ? "text-destructive"
                : place.free_bytes < place.warn_bytes
                  ? "text-warning"
                  : undefined
          }
          title={`${place.path}: a warning under ${formatGib(place.warn_bytes)}, no new executions under ${formatGib(place.floor_bytes)}`}
        />
      ))}
    </div>
  );
}

function formatRate(value: number | null): string {
  if (value == null) return "—";
  if (value === 0) return "0";
  if (value < 0.01) return value.toFixed(3);
  if (value < 10) return value.toFixed(2);
  return value.toFixed(1);
}

export function HealthStrip({ health }: { health: FleetHealth }) {
  const lastHour = health.last_hour;
  const jev = lastHour.by_judge.jev ?? 0;
  const rules = lastHour.by_judge.rules ?? 0;
  const judged = Math.max(
    lastHour.judgements,
    Object.values(lastHour.by_judge).reduce((sum, n) => sum + n, 0),
  );
  const other = Math.max(0, judged - jev - rules);
  const jevShare = judged > 0 ? jev / judged : null;

  const ladderTotal =
    LADDER.reduce((sum, entry) => sum + health.ladder_24h[entry.step], 0) +
    health.ladder_24h.liveness;
  const humanRate = health.human_per_agent_hour;

  return (
    <section
      aria-label="Fleet health"
      className="grid shrink-0 grid-cols-2 gap-px overflow-hidden rounded-lg border border-surface-border bg-surface-border md:grid-cols-3 xl:grid-cols-[minmax(0,1.05fr)_minmax(0,1fr)_minmax(0,1.5fr)_minmax(0,2.1fr)]"
    >
      <Cell label="Human per agent-hour · 24h">
        <div
          className={cx(
            "tnum text-display-sm leading-none font-semibold tracking-tight",
            humanRate == null
              ? "text-faint-foreground"
              : humanRate === 0
                ? "text-success"
                : "text-warning",
          )}
          title="Human decisions divided by agent-hours over the last 24 h. Lower is better."
        >
          {formatRate(humanRate)}
        </div>
        <div className="mt-1.5 text-caption text-muted-foreground">
          <span className="tnum">{health.ladder_24h.human}</span> human ·{" "}
          <span className="tnum">
            {health.agent_hours_24h == null
              ? "—"
              : health.agent_hours_24h.toFixed(1)}
          </span>{" "}
          agent-h
        </div>
      </Cell>

      <Cell label="Agents">
        <div className="flex items-baseline gap-3 text-caption">
          <span className="tnum text-title-sm font-semibold text-foreground">
            {health.agents_total}
          </span>
          <Stat label="live" value={health.live} />
          <Stat
            label="attention"
            value={health.attention}
            tone={health.attention > 0 ? "text-warning" : undefined}
          />
        </div>
        <div className="mt-1.5 flex flex-wrap gap-x-2.5 gap-y-1 text-caption text-muted-foreground">
          {FLEET_STATES.filter((state) => health.by_state[state] > 0).map(
            (state) => (
              <span key={state} className="inline-flex items-center gap-1">
                <FleetStateDot state={state} size="sm" />
                <span className="tnum text-foreground">
                  {health.by_state[state]}
                </span>
                {stateLabelUpper(state).toLowerCase()}
              </span>
            ),
          )}
          {health.agents_total === 0 && (
            <span className="text-faint-foreground">no agents yet</span>
          )}
        </div>
        <DiskLine disk={health.disk} />
      </Cell>

      <Cell label="Judgements · last hour" className="col-span-2 md:col-span-1">
        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 text-caption">
          <span className="tnum text-title-sm font-semibold text-foreground">
            {judged}
          </span>
          <Stat
            label="Jev share"
            value={jevShare == null ? "—" : `${Math.round(jevShare * 100)}%`}
            tone="text-jev"
          />
          <Stat
            label="Jev p50"
            value={
              lastHour.model_latency_p50_ms == null
                ? "—"
                : formatDuration(lastHour.model_latency_p50_ms)
            }
          />
        </div>
        <div
          className="mt-2 flex h-1.5 overflow-hidden rounded-full bg-muted"
          title={`rules ${rules} · Jev ${jev}${other ? ` · other ${other}` : ""}`}
        >
          {judged > 0 && (
            <>
              <div
                className="h-full bg-muted-foreground/70"
                style={{ width: `${(rules / judged) * 100}%` }}
              />
              <div
                className="h-full bg-jev"
                style={{ width: `${(jev / judged) * 100}%` }}
              />
              <div
                className="h-full bg-faint-foreground/50"
                style={{ width: `${(other / judged) * 100}%` }}
              />
            </>
          )}
        </div>
        <div className="mt-1.5 flex flex-wrap gap-x-3 gap-y-0.5 text-caption">
          <Stat label="rules" value={rules} />
          <Stat label="Jev" value={jev} tone="text-jev" />
          <Stat
            label="Jev unreachable"
            value={lastHour.judge_unavailable}
            tone={lastHour.judge_unavailable > 0 ? "text-warning" : undefined}
            title="Judgements where Jev could not be reached and the rules' guess stood"
          />
          <Stat
            label="disagreed"
            value={lastHour.disagreements}
            title="Jev's final state differed from the rules' guess"
          />
        </div>
      </Cell>

      <Cell
        label={
          <span className="flex items-center gap-2">
            Decision ladder · 24h
            <span className="ml-auto flex gap-3 normal-case tracking-normal">
              <Stat label="tool calls/h" value={lastHour.tool_calls} />
              <Stat
                label="failures/h"
                value={lastHour.failures}
                tone={lastHour.failures > 0 ? "text-destructive" : undefined}
              />
            </span>
          </span>
        }
        className="col-span-2 md:col-span-3 xl:col-span-1"
      >
        <div className="flex h-1.5 gap-px overflow-hidden rounded-full bg-muted">
          {ladderTotal > 0 &&
            [...LADDER, { ...LIVENESS, step: "liveness" as const }].map(
              (entry) => {
                const count = health.ladder_24h[entry.step];
                if (count <= 0) return null;
                return (
                  <div
                    key={entry.step}
                    title={`${entry.label}: ${count}`}
                    className={cx("h-full min-w-[3px]", entry.bar)}
                    style={{ flexGrow: count, flexBasis: 0 }}
                  />
                );
              },
            )}
        </div>
        <ol className="mt-2 flex flex-wrap items-center gap-x-1 gap-y-1 text-caption">
          {LADDER.map((entry, index) => {
            const count = health.ladder_24h[entry.step];
            const isHuman = entry.step === "human";
            return (
              <li
                key={entry.step}
                className="inline-flex items-center gap-1"
                title={entry.hint}
              >
                {index > 0 && (
                  <ChevronRight
                    size={12}
                    className="shrink-0 text-faint-foreground"
                  />
                )}
                <span
                  className={cx("size-1.5 shrink-0 rounded-full", entry.bar)}
                />
                <span
                  className={cx(
                    "text-muted-foreground",
                    isHuman && count > 0 && "text-warning",
                  )}
                >
                  {entry.label}
                </span>
                <span
                  className={cx(
                    "tnum font-medium",
                    count === 0
                      ? "text-faint-foreground"
                      : isHuman
                        ? "text-warning"
                        : "text-foreground",
                  )}
                >
                  {count}
                </span>
              </li>
            );
          })}
        </ol>
        <div className="mt-1 flex flex-wrap gap-x-3 text-micro text-faint-foreground">
          <span title={LIVENESS.hint} className="inline-flex items-center gap-1">
            <span className={cx("size-1.5 rounded-full", LIVENESS.bar)} />
            <span className="tnum text-muted-foreground">
              {health.ladder_24h.liveness}
            </span>{" "}
            liveness restarts
          </span>
          {health.ladder_extra.map(([name, count]) => (
            <span key={name}>
              <span className="tnum text-muted-foreground">{count}</span>{" "}
              {name}
            </span>
          ))}
        </div>
      </Cell>
    </section>
  );
}

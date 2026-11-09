import { ChevronRight } from "lucide-react";
import type { ReactNode } from "react";

import type { DiskPlace, FleetHealth, LadderStep } from "../../fleet";
import { absoluteTitle, clockTime, formatDuration } from "../../format";
import type { GateRoom, Memory } from "../../resources";
import { memoryBar, waitingGates } from "../../resources";
import { cx } from "../ui";

const LADDER: Array<{
  step: LadderStep | "liveness";
  label: string;
  bar: string;
  hint: string;
}> = [
  {
    step: "rules",
    label: "Rules",
    bar: "bg-muted-foreground",
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
    <div className={cx("min-w-0 rounded-card bg-surface px-4 py-3 shadow-card", className)}>
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

/** Free disk at one place, as a bar against its warning and its floor:
 * the scale runs to twice the warning (or a little past the free space,
 * whichever is more), so the two marks always show. */
function DiskBar({ place }: { place: DiskPlace }) {
  const free = place.free_bytes;
  const scale = Math.max(place.warn_bytes * 2, (free ?? 0) * 1.15, 1);
  const at = (bytes: number) => `${Math.min(100, (bytes / scale) * 100)}%`;
  const tone =
    free == null
      ? { bar: "bg-faint-foreground", text: "text-faint-foreground", word: "unread" }
      : free < place.floor_bytes
        ? { bar: "bg-destructive", text: "text-destructive", word: "under the floor: no new executions" }
        : free < place.warn_bytes
          ? { bar: "bg-warning", text: "text-warning", word: "under the warning" }
          : { bar: "bg-success", text: "text-foreground", word: "fine" };
  return (
    <div
      className="min-w-0"
      title={`${place.path}: ${formatGib(free)} free (${tone.word}); a warning under ${formatGib(place.warn_bytes)}, no new executions under ${formatGib(place.floor_bytes)}`}
    >
      <div className="flex items-baseline gap-2 text-caption">
        <span className="text-muted-foreground">Disk · {place.label}</span>
        <span className={cx("tnum ml-auto font-medium", tone.text)}>{formatGib(free)}</span>
        <span className="text-faint-foreground">free</span>
      </div>
      <div className="relative mt-1 h-1.5 rounded-full bg-muted">
        {free != null && (
          <div className={cx("h-full rounded-full", tone.bar)} style={{ width: at(free) }} />
        )}
        {[
          ["floor", place.floor_bytes],
          ["warn", place.warn_bytes],
        ].map(([name, bytes]) => (
          <span
            key={name}
            aria-hidden="true"
            className="absolute -top-0.5 h-2.5 w-px bg-foreground/70"
            style={{ left: at(bytes as number) }}
          />
        ))}
      </div>
      <div className="tnum mt-0.5 flex justify-between text-micro text-faint-foreground">
        <span>floor {formatGib(place.floor_bytes)}</span>
        <span>warn {formatGib(place.warn_bytes)}</span>
      </div>
    </div>
  );
}

/** Memory available now (MemAvailable), as a bar of the total against the
 * warning line (4 GiB). A value the server could not read shows as unread,
 * and an unknown total scales the bar to the warning instead. */
function MemoryBar({ memory }: { memory: Memory | null }) {
  if (!memory) {
    return <p className="text-caption text-faint-foreground">This server reports no memory.</p>;
  }
  const available = memory.available_bytes;
  const { tone, scale } = memoryBar(memory);
  const at = (bytes: number) => `${Math.min(100, (bytes / scale) * 100)}%`;
  const style =
    tone === "unread"
      ? { bar: "bg-faint-foreground", text: "text-faint-foreground", word: "unread" }
      : tone === "short"
        ? { bar: "bg-warning", text: "text-warning", word: "under the warning" }
        : { bar: "bg-success", text: "text-foreground", word: "fine" };
  return (
    <div
      className="min-w-0"
      title={`Memory available (MemAvailable, read now): ${formatGib(available)} of ${
        memory.total_bytes == null ? "an unread total" : formatGib(memory.total_bytes)
      } (${style.word}); a warning under ${formatGib(memory.warn_bytes)}`}
    >
      <div className="flex items-baseline gap-2 text-caption">
        <span className="text-muted-foreground">Memory</span>
        <span className={cx("tnum ml-auto font-medium", style.text)}>{formatGib(available)}</span>
        <span className="text-faint-foreground">available</span>
      </div>
      <div className="relative mt-1 h-1.5 rounded-full bg-muted">
        {available != null && (
          <div className={cx("h-full rounded-full", style.bar)} style={{ width: at(available) }} />
        )}
        <span
          aria-hidden="true"
          className="absolute -top-0.5 h-2.5 w-px bg-foreground/70"
          style={{ left: at(memory.warn_bytes) }}
        />
      </div>
      <div className="tnum mt-0.5 flex justify-between text-micro text-faint-foreground">
        <span>warn {formatGib(memory.warn_bytes)}</span>
        <span>{memory.total_bytes == null ? "total unread" : `of ${formatGib(memory.total_bytes)}`}</span>
      </div>
    </div>
  );
}

function ago(ms: number | null): string {
  return ms == null ? "—" : formatDuration(Math.max(0, Date.now() - ms));
}

/** Each gated mission's waits for gate room: the gates waiting now (the
 * task, how many `no_room` since when, escalated or not), then the gated
 * missions whose gate has room. */
function GateRoomList({ rows }: { rows: GateRoom[] | null }) {
  if (rows === null) {
    return <p className="text-caption text-faint-foreground">This server reports no gate room.</p>;
  }
  const waiting = waitingGates(rows);
  const free = rows.filter((row) => row.task === null);
  return (
    <div className="min-w-0 text-caption">
      <div className="flex items-baseline gap-2">
        <span className="text-muted-foreground">Gate room</span>
        <span
          className={cx(
            "tnum ml-auto font-medium",
            waiting.length > 0 ? "text-warning" : rows.length === 0 ? "text-faint-foreground" : "text-foreground",
          )}
        >
          {rows.length === 0
            ? "no gated mission"
            : waiting.length === 0
              ? "no gate waits"
              : `${waiting.length} gate${waiting.length === 1 ? "" : "s"} waiting`}
        </span>
      </div>
      {waiting.map((row) => (
        <div
          key={row.mission}
          className="mt-0.5 flex min-w-0 items-baseline gap-1.5"
          title={`Mission #${row.mission} ${row.name}: task #${row.task}'s gate found no room in TMPDIR ${row.waits} time${
            row.waits === 1 ? "" : "s"
          } in a row, since ${clockTime(row.since_ms)}, latest ${clockTime(row.last_ms)}; ${
            row.escalated ? "escalated to the supervisor" : "not escalated yet (after two hours)"
          }`}
        >
          <span className="min-w-0 truncate text-muted-foreground">
            #{row.mission} {row.name}
          </span>
          <span className="shrink-0 font-mono text-micro text-faint-foreground">T{row.task}</span>
          <span className="tnum ml-auto shrink-0 text-warning">
            {row.waits}× for {ago(row.since_ms)}
          </span>
          {row.escalated && <span className="shrink-0 text-micro text-warning">escalated</span>}
        </div>
      ))}
      {free.length > 0 && (
        <div
          className="mt-0.5 truncate text-micro text-faint-foreground"
          title={free.map((row) => `#${row.mission} ${row.name}`).join("\n")}
        >
          room for {free.map((row) => `#${row.mission}`).join(", ")}
        </div>
      )}
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

/** Liveness of the always-on agents, as the strip's first cell reads it. */
export interface Liveness {
  /** Always-on agents (the supervisor and the residents) and how many run. */
  total: number;
  up: number;
  /** The live always-on agent whose execution started last, and how long ago. */
  shortest: { name: string; ms: number; since: number } | null;
}

const HUMAN_RATE_HINT =
  "Human per agent-hour: decisions that needed a person (the ladder's human step) in the last 24 h, divided by the hours agents ran in that time. Lower is better; 0 means the fleet ran without anyone.";

export function HealthStrip({
  health,
  liveness,
}: {
  health: FleetHealth;
  liveness: Liveness;
}) {
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
  const restarts = health.ladder_24h.liveness;
  const down = liveness.total - liveness.up;

  return (
    <section
      aria-label="Fleet health"
      className="grid shrink-0 grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-[minmax(0,1fr)_minmax(0,1.25fr)_minmax(0,1.2fr)_minmax(0,1.75fr)]"
    >
      <Cell label="Liveness">
        <div className="flex items-baseline gap-2">
          <span
            className={cx(
              "tnum text-display-sm leading-none font-semibold tracking-tight",
              liveness.total === 0
                ? "text-faint-foreground"
                : down > 0
                  ? "text-warning"
                  : "text-success",
            )}
          >
            {liveness.up}/{liveness.total}
          </span>
          <span className="text-caption text-muted-foreground">always-on agents up</span>
        </div>
        <div className="mt-2 space-y-0.5 text-caption text-muted-foreground">
          <div
            className="flex min-w-0 items-baseline gap-1"
            title={
              liveness.shortest
                ? `The ${liveness.shortest.name}'s current execution started ${absoluteTitle(liveness.shortest.since)}`
                : "No always-on agent is running"
            }
          >
            <span className="shrink-0">Shortest uptime</span>
            {liveness.shortest && (
              <span className="min-w-0 truncate text-faint-foreground">· {liveness.shortest.name}</span>
            )}
            <span className="tnum ml-auto shrink-0 font-medium text-foreground">
              {liveness.shortest ? formatDuration(liveness.shortest.ms) : "—"}
            </span>
          </div>
          <div className="flex items-baseline gap-1" title={LIVENESS.hint}>
            <span>Liveness restarts · 24h</span>
            <span
              className={cx(
                "tnum ml-auto font-medium",
                restarts > 0 ? "text-warning" : "text-foreground",
              )}
            >
              {restarts}
            </span>
          </div>
          <div className="flex items-baseline gap-1">
            <span>Agents live</span>
            <span className="tnum ml-auto font-medium text-foreground">
              {health.live}
              <span className="font-normal text-faint-foreground"> of {health.agents_total}</span>
            </span>
          </div>
        </div>
      </Cell>

      <Cell label="Resources">
        <div className="space-y-2.5">
          {health.disk.length === 0 ? (
            <p className="text-caption text-faint-foreground">This server reports no disk places.</p>
          ) : (
            health.disk.map((place) => (
              <DiskBar key={`${place.label}:${place.path}`} place={place} />
            ))
          )}
          <MemoryBar memory={health.memory} />
          <GateRoomList rows={health.gate_room} />
        </div>
      </Cell>

      <Cell label="Judgements · last hour">
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
        className="sm:col-span-2 xl:col-span-1"
      >
        <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5" title={HUMAN_RATE_HINT}>
          <span
            className={cx(
              "tnum text-title-sm leading-none font-semibold",
              humanRate == null
                ? "text-faint-foreground"
                : humanRate === 0
                  ? "text-success"
                  : "text-warning",
            )}
          >
            {formatRate(humanRate)}
          </span>
          <span className="cursor-help whitespace-nowrap text-caption text-muted-foreground underline decoration-dotted underline-offset-2">
            human per agent-hour
          </span>
          <span className="tnum ml-auto whitespace-nowrap text-caption text-faint-foreground">
            {health.ladder_24h.human} human ·{" "}
            {health.agent_hours_24h == null ? "—" : health.agent_hours_24h.toFixed(1)} agent-h
          </span>
        </div>
        <div className="mt-2 flex h-1.5 gap-px overflow-hidden rounded-full bg-muted">
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
                    aria-hidden="true"
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
        {health.ladder_extra.length > 0 && (
          <div className="mt-1 flex flex-wrap gap-x-3 text-micro text-faint-foreground">
            {health.ladder_extra.map(([name, count]) => (
              <span key={name}>
                <span className="tnum text-muted-foreground">{count}</span> {name}
              </span>
            ))}
          </div>
        )}
      </Cell>
    </section>
  );
}

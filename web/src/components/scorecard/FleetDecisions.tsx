// The scorecard's "How the fleet decides" (design-ui-information §2.8): the
// judgements and the decision ladder, moved from home's health strip. It
// reads one DecisionWindow (fleetDecisions.ts); objective 043's 7 d and 30 d
// figures fill the same shape, and each card names its own window.
import { ChevronRight, Scale } from "lucide-react";

import type { DecisionWindow } from "../../fleetDecisions";
import { formatDuration } from "../../format";
import { Cell, HUMAN_RATE_HINT, LADDER, LIVENESS, Stat, formatRate } from "../fleet/HealthStrip";
import { cx } from "../ui";

export function FleetDecisions({ decisions }: { decisions: DecisionWindow | null }) {
  return (
    <section aria-label="How the fleet decides" className="flex flex-col gap-2">
      <h2 className="flex items-center gap-2 px-1 text-label font-medium text-foreground">
        <Scale size={14} className="text-muted-foreground" />
        How the fleet decides
        <span className="text-caption font-normal text-faint-foreground">
          · each card names its window; the page's window does not apply yet
        </span>
      </h2>
      {decisions === null ? (
        <p className="px-1 text-caption text-faint-foreground">Reading the fleet's judgements…</p>
      ) : (
        <Figures decisions={decisions} />
      )}
    </section>
  );
}

function Figures({ decisions }: { decisions: DecisionWindow }) {
  const j = decisions.judgements;
  const l = decisions.ladder;
  return (
    <div className="grid grid-cols-1 gap-3 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.4fr)]">
        <Cell label={`Judgements · ${j.window}`}>
          <div className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 text-caption">
            <span className="tnum text-title-sm font-semibold text-foreground">
              {j.total}
            </span>
            <Stat
              label="Jev share"
              value={j.jevShare == null ? "—" : `${Math.round(j.jevShare * 100)}%`}
              tone="text-jev"
            />
            <Stat
              label="Jev p50"
              value={
                j.jevP50Ms == null ? "—" : formatDuration(j.jevP50Ms)
              }
            />
          </div>
          <div
            className="mt-2 flex h-1.5 overflow-hidden rounded-full bg-muted"
            title={`rules ${j.rules} · Jev ${j.jev}${j.other ? ` · other ${j.other}` : ""}`}
          >
            {j.total > 0 && (
              <>
                <div
                  className="h-full bg-muted-foreground/70"
                  style={{ width: `${(j.rules / j.total) * 100}%` }}
                />
                <div
                  className="h-full bg-jev"
                  style={{ width: `${(j.jev / j.total) * 100}%` }}
                />
                <div
                  className="h-full bg-faint-foreground/50"
                  style={{ width: `${(j.other / j.total) * 100}%` }}
                />
              </>
            )}
          </div>
          <div className="mt-1.5 flex flex-wrap gap-x-3 gap-y-0.5 text-caption">
            <Stat label="rules" value={j.rules} />
            <Stat label="Jev" value={j.jev} tone="text-jev" />
            <Stat
              label="Jev unreachable"
              value={j.unavailable}
              tone={j.unavailable > 0 ? "text-warning" : undefined}
              title="Judgements where Jev could not be reached and the rules' guess stood"
            />
            <Stat
              label="disagreed"
              value={j.disagreed}
              title="Jev's final state differed from the rules' guess"
            />
          </div>
        </Cell>

        <Cell
          label={
            <span className="flex items-center gap-2">
              Decision ladder · {l.window}
              <span className="ml-auto flex gap-3 normal-case tracking-normal">
                <Stat label="tool calls/h" value={l.toolCallsPerHour ?? "—"} />
                <Stat
                  label="failures/h"
                  value={l.failuresPerHour ?? "—"}
                  tone={(l.failuresPerHour ?? 0) > 0 ? "text-destructive" : undefined}
                />
              </span>
            </span>
          }
        >
          <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5" title={HUMAN_RATE_HINT}>
            <span
              className={cx(
                "tnum text-title-sm leading-none font-semibold",
                l.humanPerAgentHour == null
                  ? "text-faint-foreground"
                  : l.humanPerAgentHour === 0
                    ? "text-success"
                    : "text-warning",
              )}
            >
              {formatRate(l.humanPerAgentHour)}
            </span>
            <span className="cursor-help whitespace-nowrap text-caption text-muted-foreground underline decoration-dotted underline-offset-2">
              human per agent-hour
            </span>
            <span className="tnum ml-auto whitespace-nowrap text-caption text-faint-foreground">
              {l.counts.human} human ·{" "}
              {l.agentHours == null ? "—" : l.agentHours.toFixed(1)} agent-h
            </span>
          </div>
          <div className="mt-2 flex h-1.5 gap-px overflow-hidden rounded-full bg-muted">
            {l.total > 0 &&
              [...LADDER, { ...LIVENESS, step: "liveness" as const }].map(
                (entry) => {
                  const count = l.counts[entry.step];
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
              const count = l.counts[entry.step];
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
          {l.extra.length > 0 && (
            <div className="mt-1 flex flex-wrap gap-x-3 text-micro text-faint-foreground">
              {l.extra.map(([name, count]) => (
                <span key={name}>
                  <span className="tnum text-muted-foreground">{count}</span> {name}
                </span>
              ))}
            </div>
          )}
        </Cell>
    </div>
  );
}

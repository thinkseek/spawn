// The scorecard's "How the fleet decides" (design-ui-information §2.8): the
// judgements and the decision ladder, moved from home's health strip, and
// beside them a window's landings per day, escalations, interventions and
// each mission's idle hours. It reads one DecisionWindow (fleetDecisions.ts)
// for the page's window (GET /api/scorecard/fleet), and each card names its
// own window.
import { ChevronRight, Scale } from "lucide-react";

import type { DecisionWindow, MissionIdle } from "../../fleetDecisions";
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
          · over the page's window; each card names its own
        </span>
      </h2>
      {decisions === null ? (
        <p className="px-1 text-caption text-faint-foreground">Reading the fleet's judgements…</p>
      ) : (
        <>
          <Figures decisions={decisions} />
          <WindowFigures decisions={decisions} />
        </>
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
              {l.toolCallsPerHour != null && (
                <span className="ml-auto flex gap-3 normal-case tracking-normal">
                  <Stat label="tool calls/h" value={l.toolCallsPerHour} />
                  <Stat
                    label="failures/h"
                    value={l.failuresPerHour ?? "—"}
                    tone={(l.failuresPerHour ?? 0) > 0 ? "text-destructive" : undefined}
                  />
                </span>
              )}
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

/** The figures only a window's answer has, as cards beside the two above:
 * landings per day, escalations, interventions and each open mission's
 * idle hours (an idle mission, idle more than half its hours, marked). */
function WindowFigures({ decisions }: { decisions: DecisionWindow }) {
  const missions = decisions.idleHoursByMission;
  const interventions = decisions.interventions;
  if (decisions.landedPerDay === undefined && missions === undefined) return null;
  const window = decisions.ladder.window;
  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-3 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_minmax(0,1fr)_minmax(0,2fr)]">
      <Cell label={`Landed per day · ${window}`}>
        <span className="tnum text-title-sm font-semibold text-foreground">
          {decisions.landedPerDay == null ? "—" : decisions.landedPerDay}
        </span>
      </Cell>
      <Cell label={`Escalations · ${window}`}>
        <span
          className="tnum text-title-sm font-semibold text-foreground"
          title="Distinct escalations the dispatcher told the supervisor, each once"
        >
          {decisions.escalations ?? "—"}
        </span>
      </Cell>
      <Cell label={`Interventions · ${window}`}>
        <span
          className={cx(
            "tnum text-title-sm font-semibold",
            (interventions?.total ?? 0) > 0 ? "text-warning" : "text-foreground",
          )}
        >
          {interventions?.total ?? "—"}
        </span>
        {interventions && (
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-caption">
            <Stat label="commands" value={interventions.byKind.command} />
            <Stat label="resumes" value={interventions.byKind.resume} />
            <Stat label="owner tasks" value={interventions.byKind.owner_task} />
            <Stat label="cancel & reassign" value={interventions.byKind.cancel_reassign} />
          </div>
        )}
      </Cell>
      <Cell label={`Idle hours by mission · ${window}`}>
        {!missions || missions.length === 0 ? (
          <span className="text-caption text-faint-foreground">No mission open in this window.</span>
        ) : (
          <ul className="flex flex-col gap-1 text-caption">
            {missions.map((mission) => (
              <MissionIdleLine key={mission.mission} mission={mission} />
            ))}
          </ul>
        )}
      </Cell>
    </div>
  );
}

function MissionIdleLine({ mission }: { mission: MissionIdle }) {
  return (
    <li
      className="flex min-w-0 items-baseline gap-2"
      title="Hours open in the window with no task of the mission open"
    >
      <span className="tnum shrink-0 text-muted-foreground">#{mission.mission}</span>
      <span className="min-w-0 flex-1 truncate text-foreground">{mission.text}</span>
      <span className="tnum shrink-0 text-muted-foreground">
        {mission.idleHours} of {mission.hours} h idle
      </span>
      {mission.idle && (
        <span className="shrink-0 rounded-sm bg-warning/15 px-1 text-micro font-medium text-warning">
          idle
        </span>
      )}
    </li>
  );
}

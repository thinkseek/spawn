// The scorecard's "How the fleet decides" (design-ui-information §2.8): the
// judgements and the decision ladder, moved from home's health strip, and
// beside them (objective 043) tasks landed per day, escalations to the
// supervisor, interventions and each mission's idle hours. It reads one
// DecisionWindow (fleetDecisions.ts) over the page's window, from
// /api/scorecard/fleet; each card names its own window.
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
          · each card names its window
        </span>
      </h2>
      {decisions === null ? (
        <p className="px-1 text-caption text-faint-foreground">Reading the fleet's judgements…</p>
      ) : (
        <>
          <Figures decisions={decisions} />
          <Experience decisions={decisions} />
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

const KIND_LABELS: Record<string, string> = {
  command: "commands",
  resume: "resumes",
  owner_task: "owner tasks",
  cancel_reassign: "cancel & reassign",
};

/** The figures a window from the experience views adds: absent from home's
 * health, so nothing shows until the page's window answers. */
function Experience({ decisions }: { decisions: DecisionWindow }) {
  const { landedPerDay, escalations, interventions, idleHoursByMission } = decisions;
  if (landedPerDay === undefined && escalations === undefined && interventions === undefined) return null;
  const window = decisions.ladder.window;
  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-3 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_minmax(0,1.4fr)_minmax(0,1.6fr)]">
      <Cell label={`Landed per day · ${window}`}>
        <span className="tnum text-title-sm font-semibold text-foreground">
          {landedPerDay == null ? "—" : landedPerDay.toFixed(landedPerDay < 10 ? 2 : 1)}
        </span>
        <div className="text-caption text-muted-foreground">tasks landed or accepted, a day</div>
      </Cell>
      <Cell label={`Escalations · ${window}`}>
        <span className="tnum text-title-sm font-semibold text-foreground">{escalations ?? "—"}</span>
        <div className="text-caption text-muted-foreground">items the dispatcher sent the supervisor</div>
      </Cell>
      <Cell label={`Interventions · ${window}`}>
        <span
          className={cx(
            "tnum text-title-sm font-semibold",
            (interventions?.total ?? 0) > 0 ? "text-warning" : "text-success",
          )}
        >
          {interventions?.total ?? "—"}
        </span>
        <div className="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-caption">
          {Object.entries(interventions?.byKind ?? {}).map(([kind, count]) => (
            <Stat key={kind} label={KIND_LABELS[kind] ?? kind} value={count} />
          ))}
        </div>
      </Cell>
      <Cell label={`Idle hours by mission · ${window}`}>
        {(idleHoursByMission ?? []).length === 0 ? (
          <div className="text-caption text-faint-foreground">No mission was open.</div>
        ) : (
          <ul className="flex flex-col gap-1 text-caption">
            {(idleHoursByMission ?? []).map((mission) => (
              <IdleRow key={mission.mission} mission={mission} />
            ))}
          </ul>
        )}
      </Cell>
    </div>
  );
}

function IdleRow({ mission }: { mission: MissionIdle }) {
  const share = mission.hours > 0 ? mission.idleHours / mission.hours : 0;
  return (
    <li
      className="flex min-w-0 items-baseline gap-2"
      title={`${mission.idleHours} of ${mission.hours} h with no task in flight`}
    >
      <span className="tnum shrink-0 text-faint-foreground">#{mission.mission}</span>
      <span className="min-w-0 flex-1 truncate text-muted-foreground">{mission.text}</span>
      <span className={cx("tnum shrink-0", mission.idle ? "text-warning" : "text-foreground")}>
        {mission.idleHours.toFixed(0)} / {mission.hours.toFixed(0)} h
      </span>
      <span className="tnum w-9 shrink-0 text-right text-faint-foreground">{Math.round(share * 100)}%</span>
      {mission.idle && (
        <span className="shrink-0 rounded-full bg-warning/10 px-1.5 text-micro font-medium text-warning">idle</span>
      )}
      {mission.pausedHours > 0 && (
        <span
          className="tnum shrink-0 rounded-full bg-muted px-1.5 text-micro font-medium text-muted-foreground"
          title="The owner paused it: paused hours are not idle"
        >
          paused {mission.pausedHours.toFixed(0)} h
        </span>
      )}
      {mission.ownerWork && (
        <span
          className="shrink-0 rounded-full bg-muted px-1.5 text-micro font-medium text-muted-foreground"
          title="No objective backlog: its work comes from the owner, so idle hours here are no loss"
        >
          owner work
        </span>
      )}
    </li>
  );
}

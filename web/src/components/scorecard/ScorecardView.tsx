import { CalendarDays, Gauge, OctagonAlert, Target } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { usePoll } from "../../api";
import type { FleetHealth } from "../../fleet";
import { decisionsFromHealth } from "../../fleetDecisions";
import { absoluteTitle, localDateTime, relativeTime } from "../../format";
import type { Missions } from "../../missions";
import { isQuietScope, orderScopes } from "../../ordering";
import type {
  DimensionKey,
  Dimensions,
  Scorecard,
  ScorecardDay,
  ScorecardScope,
  TrendTone,
  WindowPreset,
} from "../../scorecard";
import {
  DIMENSION_INFO,
  DIMENSION_KEYS,
  WINDOW_PRESETS,
  dimensionParts,
  directionLabel,
  fetchScorecard,
  formatValue,
  nUnit,
  rawTitle,
  scopeLabel,
  scorecardQuery,
  showCell,
  showValue,
  trendMark,
} from "../../scorecard";
import { Fact, SectionLabel } from "../fleet/bits";
import { Collapsible } from "../missions/parts";
import { useNow } from "../rsi/Phase";
import { Card, CardHeader, EmptyState, LiveStatus, PageHeader, cx } from "../ui";
import { ViewToggle, useStoredChoice } from "../ViewToggle";
import type { ViewOption } from "../ViewToggle";
import { FleetDecisions } from "./FleetDecisions";

const POLL_MS = 60_000;

const WINDOW_OPTIONS: ReadonlyArray<ViewOption<WindowPreset>> = WINDOW_PRESETS.map(
  (preset) => ({ id: preset, label: preset, icon: null }),
);

const TONE_STYLE: Record<TrendTone, string> = {
  good: "bg-ok-tint text-success ring-success/25",
  bad: "bg-fail-tint text-destructive ring-destructive/25",
  flat: "bg-muted text-muted-foreground ring-surface-border",
};

interface Loaded {
  card: Scorecard;
  at_ms: number;
}

/**
 * How the fleet and each mission did over a window: throughput, lead time,
 * first-pass quality, rework, autonomy, reliability, cost and gate time,
 * each against the previous window of the same length. Strictly read-only:
 * the only requests are GET /api/scorecard (and GET /api/missions, polled by
 * App, for the missions' titles and order).
 */
export function ScorecardView({
  missions,
  health,
}: {
  missions: Missions | null;
  /** /api/fleet's health (App polls it): the judgements and the ladder. */
  health: FleetHealth | null;
}) {
  useNow(10_000);
  const [since, chooseSince] = useStoredChoice<WindowPreset>(
    "spawn.scorecard.window",
    WINDOW_PRESETS,
    "7d",
  );
  const [mission, setMission] = useState<number | null>(null);
  const query = { mission, since };
  const scorecard = usePoll<Loaded>(
    `scorecard${scorecardQuery(query)}`,
    async (signal) => ({
      card: await fetchScorecard(query, signal),
      at_ms: Date.now(),
    }),
    POLL_MS,
  );
  const data = scorecard.data?.card ?? null;

  const titles = useMemo(() => {
    const out = new Map<number, string>();
    for (const entry of missions?.missions ?? []) {
      if (entry.id !== null) out.set(entry.id, firstLine(entry.text) ?? "");
    }
    return out;
  }, [missions]);
  const scopes = useMemo(
    () => (data ? orderScopes(data.scopes, missions?.missions ?? []) : []),
    [data, missions],
  );

  // Every mission the scorecard has answered for stays a choice, so picking
  // one does not hide the others.
  const [seen, setSeen] = useState<number[]>([]);
  useEffect(() => {
    const ids = (data?.scopes ?? [])
      .map((scope) => scope.mission)
      .filter((id): id is number => id !== null);
    if (ids.length === 0) return;
    setSeen((current) => {
      const merged = new Set([...current, ...ids]);
      return merged.size === current.length ? current : [...merged];
    });
  }, [data]);

  const choices = useMemo(() => {
    const ids = new Set<number>([...seen, ...titles.keys()]);
    if (mission !== null) ids.add(mission);
    return [...ids].sort((left, right) => left - right);
  }, [seen, titles, mission]);

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            Scorecard
          </h1>
        }
      >
        <ViewToggle
          label="Window"
          options={WINDOW_OPTIONS}
          value={since}
          onChange={chooseSince}
        />
        <ScopeSelect
          value={mission}
          onChange={setMission}
          choices={choices}
          titles={titles}
          className="hidden h-7 max-w-56 sm:block"
        />
        <LiveStatus
          error={scorecard.error}
          label={
            scorecard.data
              ? `Updated ${relativeTime(scorecard.data.at_ms)}`
              : "Connecting"
          }
        />
      </PageHeader>

      {/* The scope filter, below the header on a phone. */}
      <div className="flex shrink-0 items-center gap-2 border-b border-surface-border px-3 py-2 sm:hidden">
        <span className="text-caption text-muted-foreground">Scope</span>
        <ScopeSelect
          value={mission}
          onChange={setMission}
          choices={choices}
          titles={titles}
          className="h-7 min-w-0 flex-1"
        />
      </div>

      {!data ? (
        <div className="grid flex-1 place-items-center px-6 text-center">
          <p className="max-w-xl text-caption text-faint-foreground">
            {scorecard.error
              ? `Could not load the scorecard: ${scorecard.error}`
              : "Loading the scorecard…"}
          </p>
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto flex max-w-[1600px] flex-col gap-3 p-3 sm:p-4">
            {scorecard.error && (
              <div className="flex items-start gap-2 rounded-md border border-destructive/30 bg-fail-tint px-3 py-2">
                <OctagonAlert size={14} className="mt-0.5 shrink-0 text-destructive" />
                <div className="min-w-0 text-label">
                  <span className="font-medium text-destructive">
                    The last refresh failed
                  </span>
                  <span className="text-foreground/85"> — {scorecard.error}</span>
                  <div className="text-caption text-muted-foreground">
                    Showing the answer from{" "}
                    {relativeTime(scorecard.data?.at_ms ?? null)}.
                  </div>
                </div>
              </div>
            )}
            <WindowCard card={data} />
            <FleetDecisions decisions={health ? decisionsFromHealth(health) : null} />
            {scopes.length === 0 ? (
              <Card>
                <EmptyState
                  icon={<Gauge size={20} />}
                  title="Nothing to score"
                  hint="No fleet or mission scope in this window."
                />
              </Card>
            ) : (
              <>
                {scopes
                  .filter((scope) => !isQuietScope(scope))
                  .map((scope, index) => (
                    <ScopeCard
                      key={`${scope.scope}-${scope.mission ?? "all"}-${index}`}
                      scope={scope}
                      minN={data.min_n}
                      title={scope.mission === null ? null : (titles.get(scope.mission) ?? null)}
                    />
                  ))}
                <QuietScopes
                  scopes={scopes.filter(isQuietScope)}
                  window={since}
                  titles={titles}
                />
              </>
            )}
          </div>
        </div>
      )}
    </main>
  );
}

/** All scopes, or one mission (`mission=N`). */
function ScopeSelect({
  value,
  onChange,
  choices,
  titles,
  className,
}: {
  value: number | null;
  onChange: (mission: number | null) => void;
  choices: number[];
  titles: Map<number, string>;
  className?: string;
}) {
  return (
    <select
      aria-label="Scope"
      value={value === null ? "" : String(value)}
      onChange={(event) => {
        const next = event.target.value;
        onChange(next === "" ? null : Number(next));
      }}
      className={cx(
        "truncate rounded-md border border-surface-border bg-surface px-2 text-caption font-medium text-foreground transition-colors hover:border-border-strong focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring",
        className,
      )}
    >
      <option value="">All scopes</option>
      {choices.map((id) => (
        <option key={id} value={String(id)}>
          {missionOption(id, titles.get(id))}
        </option>
      ))}
    </select>
  );
}

function missionOption(id: number, title: string | undefined): string {
  if (!title) return `Mission #${id}`;
  const short = title.length > 48 ? `${title.slice(0, 47)}…` : title;
  return `Mission #${id} — ${short}`;
}

/** The first non-empty line of a mission's text, without a heading mark. */
function firstLine(text: string | null): string | null {
  if (!text) return null;
  const line = text
    .split("\n")
    .map((part) => part.trim())
    .find((part) => part !== "");
  return line ? line.replace(/^#+\s*/, "") : null;
}

// ---------------------------------------------------------------------------
// Window

function WindowCard({ card }: { card: Scorecard }) {
  return (
    <Card className="min-w-0">
      <CardHeader
        icon={<CalendarDays size={14} />}
        title="Window"
        meta={<span>local time · hover for UTC</span>}
      />
      <div className="grid grid-cols-1 gap-x-4 gap-y-2 px-4 py-3 sm:grid-cols-2 xl:grid-cols-4">
        <Fact label="Since">
          <WindowTime ms={card.since_ms} />
        </Fact>
        <Fact label="Until">
          <WindowTime ms={card.until_ms} />
        </Fact>
        {card.previous_since_ms !== null && (
          <Fact label="Previous window since">
            <WindowTime
              ms={card.previous_since_ms}
              note="the previous window runs up to this window's start"
            />
          </Fact>
        )}
        <Fact label="Minimum n">
          <span
            className="tnum"
            title="Below this many landings (gate runs for gate time) a dimension shows no value"
          >
            {card.min_n}
          </span>
        </Fact>
      </div>
    </Card>
  );
}

/** A window bound in local time; UTC (and the raw milliseconds) in the
 * tooltip. */
function WindowTime({ ms, note }: { ms: number | null; note?: string }) {
  if (ms === null) return <span className="text-faint-foreground">—</span>;
  return (
    <time
      dateTime={new Date(ms).toISOString()}
      className="tnum"
      title={`${absoluteTitle(ms)} · ${ms} ms${note ? `; ${note}` : ""}`}
    >
      {localDateTime(ms)}
      <span className="ml-1.5 text-muted-foreground">({relativeTime(ms)})</span>
    </time>
  );
}

/** Missions with no landing in the window: one line each, not a grid of
 * "not enough data". */
function QuietScopes({
  scopes,
  window,
  titles,
}: {
  scopes: ScorecardScope[];
  window: WindowPreset;
  titles: Map<number, string>;
}) {
  if (scopes.length === 0) return null;
  return (
    <Card className="min-w-0">
      <ul className="divide-y divide-surface-border">
        {scopes.map((scope, index) => {
          const title = scope.mission === null ? null : titles.get(scope.mission);
          return (
            <li
              key={`${scope.mission ?? "all"}-${index}`}
              className="flex min-w-0 items-center gap-2 px-4 py-2 text-caption"
            >
              <Target size={13} aria-hidden="true" className="shrink-0 text-faint-foreground" />
              <span className="shrink-0 font-medium text-muted-foreground">
                {scopeLabel(scope)} · no landings in {window}
              </span>
              {title && (
                <span className="min-w-0 truncate text-faint-foreground" title={title}>
                  {title}
                </span>
              )}
            </li>
          );
        })}
      </ul>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Scopes

function ScopeCard({
  scope,
  minN,
  title,
}: {
  scope: ScorecardScope;
  minN: number;
  title: string | null;
}) {
  const landings = scope.dimensions.throughput.n;
  return (
    <Card className="min-w-0">
      <header className="flex flex-wrap items-center gap-2 border-b border-surface-border px-4 py-2.5">
        <span className="flex shrink-0 text-faint-foreground">
          {scope.scope === "fleet" ? <Gauge size={14} /> : <Target size={14} />}
        </span>
        <h2 className="text-label font-medium text-foreground">{scopeLabel(scope)}</h2>
        {title && (
          <span
            className="min-w-0 flex-1 truncate text-caption text-muted-foreground"
            title={title}
          >
            {title}
          </span>
        )}
        <span className="tnum ml-auto text-caption text-muted-foreground">
          {landings} {nUnit("throughput", landings)}
        </span>
      </header>

      <div className="grid grid-cols-1 gap-3 p-3 sm:grid-cols-2 sm:p-4 xl:grid-cols-4">
        {DIMENSION_KEYS.map((key) => (
          <DimensionTile key={key} dimKey={key} dimensions={scope.dimensions} minN={minN} />
        ))}
      </div>

      <div className="border-t border-surface-border px-4 py-3">
        {scope.days.length === 0 ? (
          <SectionLabel>Per day · no days in this window</SectionLabel>
        ) : (
          <Collapsible label={`Per day (UTC) · ${scope.days.length}`}>
            <DaysTable days={scope.days} dimensions={scope.dimensions} minN={minN} />
          </Collapsible>
        )}
      </div>
    </Card>
  );
}

function DimensionTile({
  dimKey,
  dimensions,
  minN,
}: {
  dimKey: DimensionKey;
  dimensions: Dimensions;
  minN: number;
}) {
  const dimension = dimensions[dimKey];
  const info = DIMENSION_INFO[dimKey];
  const shown = showValue(dimKey, dimension, minN);
  const mark = trendMark(dimension.trend, dimension.better);
  const previous = formatValue(dimKey, dimension.previous);
  const groups = dimensionParts(dimKey, dimensions);

  return (
    <section
      aria-label={info.label}
      className="flex min-w-0 flex-col gap-2 rounded-md border border-surface-border bg-page-canvas/40 p-3"
    >
      <div className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          <div className="text-caption text-muted-foreground">{info.label}</div>
          {shown.enough ? (
            <div
              className="tnum mt-0.5 truncate text-title-sm font-semibold text-foreground"
              title={shown.title || undefined}
            >
              {shown.text}
            </div>
          ) : (
            <div className="mt-0.5 text-label font-medium text-muted-foreground">
              {shown.text}
            </div>
          )}
          <div className="text-micro text-faint-foreground">{info.unit}</div>
        </div>
        {mark && (
          <span
            title={`${mark.label} than the previous window${
              dimension.previous === null ? "" : ` (${previous})`
            }`}
            className={cx(
              "inline-flex h-5 shrink-0 items-center gap-1 rounded-md px-1.5 text-micro font-medium ring-1 ring-inset",
              TONE_STYLE[mark.tone],
            )}
          >
            <span aria-hidden="true">{mark.arrow}</span>
            {mark.label}
          </span>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-x-3 gap-y-0.5 text-micro text-muted-foreground">
        <span className="tnum" title={`${dimension.n} ${nUnit(dimKey, dimension.n)}`}>
          n {dimension.n}
        </span>
        <span>{directionLabel(dimension.better)}</span>
        {/* No previous window, no "previous —" (design §2.8). */}
        {dimension.previous !== null && (
          <span className="tnum">
            previous{" "}
            <span
              className="text-foreground"
              title={rawTitle(dimKey, dimension.previous) || undefined}
            >
              {previous}
            </span>
          </span>
        )}
      </div>

      <div className="space-y-1.5 border-t border-surface-border pt-2">
        {groups.map((group, index) => (
          <div key={group.title ?? `own-${index}`}>
            {group.title && (
              <div className="text-micro text-faint-foreground">{group.title}</div>
            )}
            {group.parts.length === 0 ? (
              <p className="text-micro text-faint-foreground">none</p>
            ) : (
              <dl className="space-y-px">
                {group.parts.map((part) => (
                  <div
                    key={part.label}
                    className={cx(
                      // A long value ("3,123,319,774 tokens · 16,882,810 per
                      // landing") wraps under its label, which keeps its
                      // width: the model's name is the row's point (§2.8).
                      "flex min-w-0 flex-wrap items-baseline gap-x-2 text-caption",
                      group.title && "pl-2",
                    )}
                  >
                    <dt className="max-w-full shrink-0 truncate text-muted-foreground" title={part.title ?? part.label}>
                      {part.label}
                    </dt>
                    <dd
                      className="tnum ml-auto min-w-0 max-w-full truncate text-right text-foreground"
                      title={part.title}
                    >
                      {part.value}
                    </dd>
                  </div>
                ))}
              </dl>
            )}
          </div>
        ))}
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Per day

function DaysTable({
  days,
  dimensions,
  minN,
}: {
  days: ScorecardDay[];
  dimensions: Dimensions;
  minN: number;
}) {
  return (
    <div className="-mx-3 -my-2">
      <div className="overflow-x-auto">
        <table className="tnum w-full min-w-[760px] border-collapse text-caption">
          <thead>
            <tr className="border-b border-surface-border text-micro text-faint-foreground">
              <th scope="col" className="px-3 py-1.5 text-left font-medium">
                Day
              </th>
              {DIMENSION_KEYS.map((key) => (
                <th
                  key={key}
                  scope="col"
                  title={`${DIMENSION_INFO[key].label}: ${DIMENSION_INFO[key].unit}, ${directionLabel(dimensions[key].better)}`}
                  className="px-3 py-1.5 text-right font-medium whitespace-nowrap"
                >
                  {DIMENSION_INFO[key].short}
                </th>
              ))}
            </tr>
          </thead>
          <tbody className="divide-y divide-surface-border">
            {days.map((day, index) => (
              <tr key={day.day || `index-${index}`} className="hover:bg-surface-hover/50">
                <th
                  scope="row"
                  className="px-3 py-1.5 text-left font-mono font-normal whitespace-nowrap text-muted-foreground"
                >
                  {day.day || "—"}
                </th>
                {DIMENSION_KEYS.map((key) => {
                  const cell = showCell(key, day.dimensions[key], minN);
                  return (
                    <td
                      key={key}
                      title={cell.title}
                      className={cx(
                        "px-3 py-1.5 text-right whitespace-nowrap",
                        cell.enough ? "text-foreground" : "text-faint-foreground",
                      )}
                    >
                      {cell.text}
                      {!cell.enough && <span className="sr-only"> not enough data</span>}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="border-t border-surface-border px-3 py-1.5 text-micro text-faint-foreground">
        — not enough data that day (fewer than {minN}); hover a cell for its raw value and n.
      </p>
    </div>
  );
}

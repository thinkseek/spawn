import { Activity } from "lucide-react";

import { readApi } from "../../api";
import type { CoreFigure, CoreLinesAnswer, CoreMark } from "../../coreLines.ts";
import { coreLinesView, trendView } from "../../coreLines.ts";
import { HomeCard, LoadingLines } from "../aui/HomeCard";
import { NorthStarChart } from "../aui/NorthStarChart";
import { cx } from "../ui";

/** How often Home reads the core lines. */
export const CORE_LINES_POLL_MS = 60_000;

const TONE: Record<CoreMark["tone"], string> = {
  plain: "text-foreground",
  warning: "text-warning",
  muted: "text-faint-foreground",
};

export function fetchCoreLines(signal: AbortSignal): Promise<CoreLinesAnswer> {
  return readApi("/api/core-lines", signal, (raw) => raw as CoreLinesAnswer);
}

const TREND_WIDTH = 56;
const TREND_HEIGHT = 16;

/** A figure's last 7 days as a small line (objective 090), the change from
 * the first point to the last as its title. Nothing without two points. */
function Trend({ figure }: { figure: CoreFigure | undefined }) {
  const { points, change } = trendView(figure);
  if (points.length < 2) {
    return (
      <span className="shrink-0 text-micro text-faint-foreground" title="no trend yet: fewer than two days sampled">
        –
      </span>
    );
  }
  const low = Math.min(...points);
  const high = Math.max(...points);
  const span = high - low;
  const pad = 2;
  const x = (index: number) => pad + (index * (TREND_WIDTH - 2 * pad)) / (points.length - 1);
  const y = (value: number) =>
    span === 0 ? TREND_HEIGHT / 2 : pad + ((high - value) * (TREND_HEIGHT - 2 * pad)) / span;
  const path = points.map((value, index) => `${index === 0 ? "M" : "L"}${x(index).toFixed(1)},${y(value).toFixed(1)}`).join(" ");
  const last = points[points.length - 1];
  const title = `7 days: ${change ?? "no change to say"}`;
  return (
    <svg
      className="shrink-0 text-muted-foreground"
      width={TREND_WIDTH}
      height={TREND_HEIGHT}
      viewBox={`0 0 ${TREND_WIDTH} ${TREND_HEIGHT}`}
      role="img"
      aria-label={title}
    >
      <title>{title}</title>
      <path d={path} fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinejoin="round" strokeLinecap="round" />
      <circle cx={x(points.length - 1)} cy={y(last)} r={1.75} fill="currentColor" />
    </svg>
  );
}

/** A guardrail: its name and value, coloured out of range, its range under,
 * its 7-day trend beside it. */
function Guardrail({ mark, figure }: { mark: CoreMark; figure: CoreFigure | undefined }) {
  return (
    <li
      className={cx(
        "min-w-0 rounded-md border px-2 py-1",
        mark.tone === "warning" ? "border-warning/40 bg-wait-tint" : "border-surface-border",
      )}
      title={`${mark.name}: ${mark.value} (${mark.range})`}
    >
      <div className="truncate text-micro text-muted-foreground">{mark.name}</div>
      <div className="flex min-w-0 items-center justify-between gap-2">
        <div className={cx("tnum truncate text-caption font-medium", TONE[mark.tone])}>{mark.value}</div>
        <Trend figure={figure} />
      </div>
      <div className="truncate text-micro text-faint-foreground">{mark.range}</div>
    </li>
  );
}

/**
 * The Core lines on Home (objective 084): each core line's north star with
 * its value and range, then its guardrails as marks, plain when healthy and
 * coloured when out of range (`/api/core-lines`, coreLines.ts). A ranges
 * file that cannot be read is said here, so a broken file shows without
 * reading logs; a range it gave that was left out is said in words (a
 * figure not built yet, a range not read), never as its internal key and
 * reason (lead-mac 2026-11-12 18:15). Each figure
 * carries its last 7 days (objective 090): a north star on the kit's KPI
 * chart (objective 120), a guardrail as a small line with the change as its
 * title. Under the recoverability row, the recovery table's rows
 * each with its drill's last pass, "never passed" or "no drill" (objective
 * 093). Read-only, from its props: Home polls the answer
 * ([`fetchCoreLines`]); both null while the first read is out.
 */
export function CoreLinesCard({
  answer,
  error,
  now,
}: {
  answer: CoreLinesAnswer | null;
  error: string | null;
  now: number;
}) {
  const view = answer ? coreLinesView(answer, now) : null;
  return (
    <HomeCard icon={<Activity size={14} />} title="Core lines">
      {!view ? (
        error ? (
          <p className="text-caption text-faint-foreground" role="status">
            Could not read the core lines: {error}
          </p>
        ) : (
          <LoadingLines label="Loading the core lines" />
        )
      ) : (
        <>
          {view.notice && (
            <p className="break-words text-caption text-warning" role="status">
              {view.notice}
            </p>
          )}
          {view.pending.length > 0 && (
            <div className="space-y-0.5 text-caption text-faint-foreground">
              {view.pending.map((line) => (
                <p key={line} className="break-words">
                  {line}
                </p>
              ))}
            </div>
          )}
          <ul className="divide-y divide-surface-border">
            {view.rows.map((row, index) => {
              const line = answer?.lines?.[index];
              return (
                <li
                  key={row.line}
                  className={cx(
                    "min-w-0 py-3 first:pt-0 last:pb-0",
                    row.north_star.tone === "warning" && "border-l-2 border-l-warning pl-3",
                  )}
                >
                  <div className="mb-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
                    {row.line}
                  </div>
                  <div className="grid min-w-0 grid-cols-1 gap-3 lg:grid-cols-[minmax(0,460px)_minmax(0,1fr)]">
                    <NorthStarChart mark={row.north_star} figure={line?.north_star} />
                    <div className="min-w-0 space-y-2">
                      {line?.line === "recoverability" && view.drills.length > 0 && (
                        <ul className="space-y-0.5" aria-label="Restore drills">
                          {view.drills.map((drill, at) => (
                            <li
                              key={`${at}:${drill.name}`}
                              className="flex min-w-0 items-baseline gap-2 text-caption"
                              title={drill.detail !== "" ? drill.detail : undefined}
                            >
                              <span className="min-w-0 truncate text-muted-foreground">{drill.name}</span>
                              <span className={cx("tnum ml-auto shrink-0", TONE[drill.tone])}>{drill.text}</span>
                            </li>
                          ))}
                        </ul>
                      )}
                      {row.guardrails.length > 0 && (
                        <ul className="grid grid-cols-1 gap-1.5 sm:grid-cols-2">
                          {row.guardrails.map((mark, at) => (
                            <Guardrail key={mark.name} mark={mark} figure={line?.guardrails?.[at]} />
                          ))}
                        </ul>
                      )}
                    </div>
                  </div>
                </li>
              );
            })}
          </ul>
        </>
      )}
    </HomeCard>
  );
}

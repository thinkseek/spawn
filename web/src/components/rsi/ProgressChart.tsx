import { ListChecks } from "lucide-react";
import { useState } from "react";

import { clockTime, relativeTime } from "../../format";
import type { SettledProgress } from "../../rsiProgress.ts";
import { progressTitle } from "../../rsiProgress.ts";
import { cx } from "../ui";
import { useWidth } from "./MetricChart";

const PAD_LEFT = 36;
const PAD_RIGHT = 14;
const PAD_TOP = 14;
const PAD_BOTTOM = 24;

/** Whole-number ticks covering [0, high]. */
function countTicks(high: number, count: number): number[] {
  const step = Math.max(1, Math.ceil(high / count));
  const ticks: number[] = [];
  for (let tick = 0; tick <= high; tick += step) ticks.push(tick);
  if (ticks[ticks.length - 1] !== high) ticks.push(high);
  return ticks;
}

/**
 * A mission's objectives settled over time: a step line up by one at each
 * objective's first landing (or its record solved on the base), against the
 * mission's objectives as a dashed rule.
 */
export function ProgressChart({
  progress,
  now,
  height = 196,
}: {
  progress: SettledProgress;
  now: number;
  height?: number;
}) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const [hover, setHover] = useState<number | null>(null);
  const { points, total } = progress;

  if (points.length === 0) {
    return (
      <div ref={ref} className="relative" style={{ height }}>
        <div className="relative flex h-full flex-col items-center justify-center gap-1.5 px-6 text-center">
          <span className="grid size-8 place-items-center rounded-md border border-surface-border bg-surface-raised text-faint-foreground">
            <ListChecks size={15} />
          </span>
          <p className="text-label font-medium text-muted-foreground">
            {progressTitle(progress)}
          </p>
          <p className="max-w-xs text-caption text-faint-foreground">
            Each objective that lands or is solved on the base steps the line up.
          </p>
        </div>
      </div>
    );
  }

  const first = points[0].at_ms;
  const last = Math.max(now, points[points.length - 1].at_ms);
  const span = Math.max(1, last - first);
  const startMs = first - span * 0.04;
  const top = Math.max(1, total, progress.settled);

  const plotWidth = Math.max(0, width - PAD_LEFT - PAD_RIGHT);
  const right = width - PAD_RIGHT;
  const bottom = height - PAD_BOTTOM;
  const x = (ms: number) => PAD_LEFT + ((ms - startMs) / (last - startMs)) * plotWidth;
  const y = (count: number) => PAD_TOP + ((top - count) / top) * (bottom - PAD_TOP);

  let line = `M ${x(startMs)} ${y(0)}`;
  for (const point of points) {
    line += ` H ${x(point.at_ms)} V ${y(point.settled)}`;
  }
  line += ` H ${right}`;
  const area = `${line} V ${bottom} H ${x(startMs)} Z`;
  const ticks = countTicks(top, height < 170 ? 3 : 4);
  const radius = points.length > 40 ? 2.5 : points.length > 16 ? 3.25 : 4;
  const hovered = hover !== null ? points[hover] : null;

  return (
    <div ref={ref} className="relative select-none" style={{ height }}>
      {width > 0 && (
        <svg
          width={width}
          height={height}
          role="img"
          aria-label={`${progressTitle(progress)} over time`}
          onMouseLeave={() => setHover(null)}
          onMouseMove={(event) => {
            const box = event.currentTarget.getBoundingClientRect();
            const at = event.clientX - box.left;
            let nearest: number | null = null;
            let distance = 24;
            points.forEach((point, index) => {
              const gap = Math.abs(x(point.at_ms) - at);
              if (gap <= distance) {
                distance = gap;
                nearest = index;
              }
            });
            setHover(nearest);
          }}
        >
          <defs>
            <linearGradient id="progress-fill" x1="0" x2="0" y1="0" y2="1">
              <stop offset="0%" stopColor="var(--chart-line)" stopOpacity="0.14" />
              <stop offset="100%" stopColor="var(--chart-line)" stopOpacity="0" />
            </linearGradient>
          </defs>
          {ticks.map((tick) => (
            <g key={tick}>
              <line x1={PAD_LEFT} x2={right} y1={y(tick)} y2={y(tick)} className="stroke-surface-border" />
              <text
                x={PAD_LEFT - 8}
                y={y(tick) + 3.5}
                textAnchor="end"
                className="tnum fill-faint-foreground text-[11px]"
              >
                {tick}
              </text>
            </g>
          ))}
          <line
            x1={PAD_LEFT}
            x2={right}
            y1={y(total)}
            y2={y(total)}
            className="stroke-success/70"
            strokeDasharray="4 4"
          />
          <path d={area} fill="url(#progress-fill)" />
          <path d={line} fill="none" className="stroke-chart-line" strokeWidth={1.75} strokeLinejoin="round" />
          {points.map((point, index) => (
            <circle
              key={point.objective}
              cx={x(point.at_ms)}
              cy={y(point.settled)}
              r={hover === index ? radius + 1.5 : radius}
              strokeWidth={2}
              className={point.on_base ? "fill-surface stroke-success" : "fill-success stroke-page-canvas"}
            />
          ))}
          <line x1={PAD_LEFT} x2={right} y1={bottom} y2={bottom} className="stroke-border-strong" />
          <text x={PAD_LEFT} y={height - 7} textAnchor="start" className="tnum fill-faint-foreground text-[11px]">
            {clockTime(first)}
          </text>
          <text x={right} y={height - 7} textAnchor="end" className="fill-faint-foreground text-[11px]">
            now
          </text>
        </svg>
      )}
      {hovered && hover !== null && (
        <div
          className="pointer-events-none absolute z-10 w-52 rounded-md border border-border-strong bg-surface-raised px-3 py-2 text-caption shadow-[var(--floating-shadow)]"
          style={{ top: 4, left: Math.min(Math.max(0, x(hovered.at_ms) + 12), Math.max(0, width - 212)) }}
        >
          <div className="mb-1 flex items-center justify-between">
            <span className="tnum font-medium text-foreground">Objective {hovered.objective}</span>
            <span className={cx(hovered.on_base ? "text-muted-foreground" : "text-success")}>
              {hovered.on_base ? "solved on base" : "landed"}
            </span>
          </div>
          <div className="text-muted-foreground">
            {hovered.task === null ? "" : `Task #${hovered.task} · `}
            {relativeTime(hovered.at_ms)}
          </div>
          <div className="tnum text-foreground">
            {hovered.settled} of {total} settled
          </div>
        </div>
      )}
      <span className="sr-only">{progressTitle(progress)}.</span>
    </div>
  );
}

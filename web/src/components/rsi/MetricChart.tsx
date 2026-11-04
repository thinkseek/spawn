import { ChartLine } from "lucide-react";
import { useLayoutEffect, useRef, useState } from "react";

import type { Direction, MetricPoint } from "../../rsi";
import { cx } from "../ui";
import { formatMetric, formatNumber } from "./Phase";
import { formatPercent, percentChange, ratchet } from "./metric";

interface MetricChartProps {
  points: MetricPoint[];
  unit: string;
  direction: Direction;
  target: number | null;
  height?: number;
}

const PAD_LEFT = 48;
const PAD_RIGHT = 14;
const PAD_TOP = 14;
const PAD_BOTTOM = 24;

function useWidth<T extends HTMLElement>() {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    setWidth(element.clientWidth);
    const observer = new ResizeObserver((entries) => {
      setWidth(Math.round(entries[0].contentRect.width));
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return [ref, width] as const;
}

/** Round tick values covering [low, high]. */
function niceTicks(low: number, high: number, count = 4): number[] {
  const span = high - low;
  const raw = span / count;
  const power = 10 ** Math.floor(Math.log10(raw));
  const step =
    [1, 2, 2.5, 5, 10].map((m) => m * power).find((s) => s >= raw) ??
    10 * power;
  const first = Math.ceil(low / step) * step;
  const ticks: number[] = [];
  for (let tick = first; tick <= high + step * 1e-6; tick += step) {
    ticks.push(Number(tick.toPrecision(12)));
  }
  return ticks;
}

/**
 * The ratchet: a step line through the landed values, with every landing
 * (a landed task's metric line) as a point. The target is a dashed rule.
 */
export function MetricChart({
  points,
  unit,
  direction,
  target,
  height = 196,
}: MetricChartProps) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const [hover, setHover] = useState<number | null>(null);
  const series = ratchet(points);

  if (series.points.length === 0 || series.start === null) {
    return (
      <div ref={ref} className="relative" style={{ height }}>
        <svg className="absolute inset-0 size-full" aria-hidden="true">
          {[0.2, 0.4, 0.6, 0.8].map((at) => (
            <line
              key={at}
              x1="0"
              x2="100%"
              y1={`${at * 100}%`}
              y2={`${at * 100}%`}
              className="stroke-surface-border"
              strokeDasharray="2 4"
            />
          ))}
        </svg>
        <div className="relative flex h-full flex-col items-center justify-center gap-1.5 px-6 text-center">
          <span className="grid size-8 place-items-center rounded-md border border-surface-border bg-surface-raised text-faint-foreground">
            <ChartLine size={15} />
          </span>
          <p className="text-label font-medium text-muted-foreground">
            No landed measurements yet
          </p>
          <p className="max-w-xs text-caption text-faint-foreground">
            The first landing records the starting baseline
            {target !== null ? ` and plots it against the ${formatMetric(target, unit)} target` : ""}
            .
          </p>
        </div>
      </div>
    );
  }

  const { steps, start } = series;
  const usable = series.points;
  const slots = usable.length;

  const values = [
    ...steps,
    ...usable.flatMap((point) =>
      [point.baseline, point.candidate].filter(
        (value): value is number => value !== null,
      ),
    ),
    ...(target !== null ? [target] : []),
  ];
  let low = Math.min(...values);
  let high = Math.max(...values);
  if (high - low < 1e-9) {
    const pad = Math.abs(high) * 0.1 || 1;
    low -= pad;
    high += pad;
  } else {
    const pad = (high - low) * 0.12;
    low -= pad;
    high += pad;
  }

  const plotWidth = Math.max(0, width - PAD_LEFT - PAD_RIGHT);
  const band = plotWidth / (slots + 1);
  const x = (slot: number) => PAD_LEFT + band * (slot + 0.5);
  const y = (value: number) =>
    PAD_TOP + ((high - value) / (high - low)) * (height - PAD_TOP - PAD_BOTTOM);
  const bottom = height - PAD_BOTTOM;
  const right = width - PAD_RIGHT;

  let line = `M ${x(0)} ${y(steps[0])}`;
  for (let slot = 1; slot < steps.length; slot += 1) {
    line += ` H ${x(slot)}`;
    if (steps[slot] !== steps[slot - 1]) line += ` V ${y(steps[slot])}`;
  }
  line += ` H ${right}`;
  const area = `${line} V ${bottom} H ${x(0)} Z`;

  const ticks = niceTicks(low, high, height < 170 ? 3 : 4);
  const maxLabels = Math.max(1, Math.floor(plotWidth / 46));
  const every = Math.ceil(slots / maxLabels);
  const radius = slots > 40 ? 2.5 : slots > 16 ? 3.25 : 4;

  const hovered = hover !== null ? usable[hover] : null;
  const hoverChange = hovered
    ? percentChange(hovered.baseline, hovered.candidate)
    : null;

  return (
    <div ref={ref} className="relative select-none" style={{ height }}>
      {width > 0 && (
        <svg
          width={width}
          height={height}
          role="img"
          aria-label={`Landed metric values, ${direction} is better`}
          onMouseLeave={() => setHover(null)}
          onMouseMove={(event) => {
            const box = event.currentTarget.getBoundingClientRect();
            const slot = Math.round((event.clientX - box.left - PAD_LEFT) / band - 0.5);
            setHover(slot >= 1 && slot <= slots ? slot - 1 : null);
          }}
        >
          <defs>
            <linearGradient id="ratchet-fill" x1="0" x2="0" y1="0" y2="1">
              <stop offset="0%" stopColor="var(--chart-line)" stopOpacity="0.14" />
              <stop offset="100%" stopColor="var(--chart-line)" stopOpacity="0" />
            </linearGradient>
          </defs>

          {ticks.map((tick) => (
            <g key={tick}>
              <line
                x1={PAD_LEFT}
                x2={right}
                y1={y(tick)}
                y2={y(tick)}
                className="stroke-surface-border"
              />
              <text
                x={PAD_LEFT - 8}
                y={y(tick) + 3.5}
                textAnchor="end"
                className="tnum fill-faint-foreground text-[11px]"
              >
                {formatNumber(tick)}
              </text>
            </g>
          ))}

          {target !== null && (
            <g>
              <line
                x1={PAD_LEFT}
                x2={right}
                y1={y(target)}
                y2={y(target)}
                className="stroke-success/70"
                strokeDasharray="4 4"
              />
              <text
                x={right - 2}
                y={y(target) - 5}
                textAnchor="end"
                className="tnum fill-success text-[11px] font-medium"
              >
                {`target ${formatMetric(target, unit)}`}
              </text>
            </g>
          )}

          {hover !== null && (
            <rect
              x={x(hover + 1) - band / 2}
              y={PAD_TOP}
              width={band}
              height={bottom - PAD_TOP}
              className="fill-white/[0.035]"
            />
          )}

          <path d={area} fill="url(#ratchet-fill)" />
          <path
            d={line}
            fill="none"
            className="stroke-chart-line"
            strokeWidth={1.75}
            strokeLinejoin="round"
          />
          <circle
            cx={x(0)}
            cy={y(steps[0])}
            r={3}
            className="fill-chart-line"
          />

          {usable.map((point, index) => {
            const value = point.candidate ?? point.baseline;
            if (value === null) return null;
            return (
              <circle
                key={`${point.task}-${index}`}
                cx={x(index + 1)}
                cy={y(value)}
                r={hover === index ? radius + 1.5 : radius}
                strokeWidth={point.accepted ? 2 : 1.75}
                className={
                  point.accepted
                    ? "fill-success stroke-page-canvas"
                    : "fill-surface stroke-destructive"
                }
              />
            );
          })}

          <line
            x1={PAD_LEFT}
            x2={right}
            y1={bottom}
            y2={bottom}
            className="stroke-border-strong"
          />
          <text
            x={x(0)}
            y={height - 7}
            textAnchor="middle"
            className="fill-faint-foreground text-[11px]"
          >
            start
          </text>
          {usable.map((point, index) =>
            (slots - 1 - index) % every === 0 ? (
              <text
                key={`label-${point.task}-${index}`}
                x={x(index + 1)}
                y={height - 7}
                textAnchor="middle"
                className={cx(
                  "tnum text-[11px]",
                  hover === index ? "fill-foreground" : "fill-faint-foreground",
                )}
              >
                {`T${point.task}`}
              </text>
            ) : null,
          )}
        </svg>
      )}

      {hovered && hover !== null && (
        <div
          className="pointer-events-none absolute z-10 w-48 rounded-md border border-border-strong bg-surface-raised px-3 py-2 text-caption shadow-[var(--floating-shadow)]"
          style={{
            top: 4,
            left: Math.min(
              Math.max(0, x(hover + 1) + 12),
              Math.max(0, width - 196),
            ),
          }}
        >
          <div className="mb-1 flex items-center justify-between">
            <span className="tnum font-medium text-foreground">
              Task #{hovered.task}
            </span>
            <span
              className={
                hovered.accepted ? "text-success" : "text-destructive"
              }
            >
              {hovered.accepted ? "landed" : "rejected"}
            </span>
          </div>
          <Row label="Candidate" value={formatMetric(hovered.candidate, unit)} />
          <Row label="Baseline" value={formatMetric(hovered.baseline, unit)} />
          <Row label="Change" value={formatPercent(hoverChange)} />
        </div>
      )}
      <span className="sr-only">
        Started at {formatMetric(start, unit)}, best{" "}
        {formatMetric(series.best, unit)}.
      </span>
    </div>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="text-muted-foreground">{label}</span>
      <span className="tnum text-foreground">{value}</span>
    </div>
  );
}

export function ChartLegend({ hasTarget }: { hasTarget: boolean }) {
  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-caption text-muted-foreground">
      <span className="inline-flex items-center gap-1.5">
        <span className="inline-block h-0.5 w-3.5 rounded bg-chart-line" />
        baseline
      </span>
      <span className="inline-flex items-center gap-1.5">
        <span className="inline-block size-2 rounded-full bg-success" />
        landed
      </span>
      {hasTarget && (
        <span className="inline-flex items-center gap-1.5">
          <span className="inline-block w-3.5 border-t border-dashed border-success" />
          target
        </span>
      )}
    </div>
  );
}

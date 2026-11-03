import type { Direction, MetricPoint } from "../../rsi";
import { formatMetric } from "./Phase";

interface MetricChartProps {
  points: MetricPoint[];
  unit: string;
  direction: Direction;
}

const WIDTH = 640;
const HEIGHT = 220;
const PAD_LEFT = 64;
const PAD_RIGHT = 16;
const PAD_TOP = 14;
const PAD_BOTTOM = 26;

/**
 * The ratchet: a step line through the landed values, with every gated
 * attempt as a point. Landed points are filled, rejected ones hollow.
 */
export function MetricChart({ points, unit, direction }: MetricChartProps) {
  const usable = points.filter(
    (point) => point.candidate !== null || point.baseline !== null,
  );
  if (usable.length === 0) {
    return (
      <div className="grid h-40 place-items-center rounded-md border border-dashed border-surface-border">
        <p className="text-caption text-faint-foreground">
          No measurements yet. The first gated attempt draws the baseline.
        </p>
      </div>
    );
  }

  const start =
    usable[0].baseline ??
    usable.find((point) => point.accepted)?.candidate ??
    usable[0].candidate ??
    0;

  // Best landed value after each point; index 0 is the starting baseline.
  const best: number[] = [start];
  for (const point of usable) {
    const previous = best[best.length - 1];
    best.push(
      point.accepted && point.candidate !== null ? point.candidate : previous,
    );
  }
  const current = best[best.length - 1];

  const values = [
    ...best,
    ...usable.flatMap((point) =>
      [point.baseline, point.candidate].filter(
        (value): value is number => value !== null,
      ),
    ),
  ];
  let low = Math.min(...values);
  let high = Math.max(...values);
  if (high - low < 1e-9) {
    const pad = Math.abs(high) * 0.1 || 1;
    low -= pad;
    high += pad;
  } else {
    const pad = (high - low) * 0.08;
    low -= pad;
    high += pad;
  }

  const slots = usable.length;
  const x = (slot: number) =>
    PAD_LEFT + (slot / slots) * (WIDTH - PAD_LEFT - PAD_RIGHT);
  const y = (value: number) =>
    PAD_TOP + ((high - value) / (high - low)) * (HEIGHT - PAD_TOP - PAD_BOTTOM);

  let path = `M ${x(0)} ${y(best[0])}`;
  for (let slot = 1; slot < best.length; slot += 1) {
    path += ` H ${x(slot)}`;
    if (best[slot] !== best[slot - 1]) path += ` V ${y(best[slot])}`;
  }

  const ticks = [high, (high + low) / 2, low];
  const change = start !== 0 ? ((current - start) / Math.abs(start)) * 100 : 0;
  const improved = direction === "lower" ? current < start : current > start;
  const landed = usable.filter((point) => point.accepted).length;

  return (
    <div>
      <div className="mb-2 flex flex-wrap items-baseline gap-x-5 gap-y-1">
        <Stat label="Start" value={formatMetric(start, unit)} />
        <Stat label="Best" value={formatMetric(current, unit)} strong />
        <Stat
          label="Change"
          value={
            current === start
              ? "0%"
              : `${change > 0 ? "+" : ""}${change.toFixed(1)}%`
          }
          tone={
            current === start
              ? "text-muted-foreground"
              : improved
                ? "text-success"
                : "text-destructive"
          }
        />
        <Stat label="Landed" value={`${landed} / ${usable.length}`} />
        <span className="ml-auto text-micro text-faint-foreground">
          {direction === "lower" ? "lower is better" : "higher is better"}
        </span>
      </div>
      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        className="h-auto w-full"
        role="img"
        aria-label={`Metric history, ${direction} is better`}
      >
        {ticks.map((tick) => (
          <g key={tick}>
            <line
              x1={PAD_LEFT}
              x2={WIDTH - PAD_RIGHT}
              y1={y(tick)}
              y2={y(tick)}
              className="stroke-surface-border"
              strokeDasharray="3 4"
            />
            <text
              x={PAD_LEFT - 8}
              y={y(tick) + 4}
              textAnchor="end"
              className="fill-faint-foreground text-[11px]"
            >
              {formatMetric(tick, unit)}
            </text>
          </g>
        ))}
        <path
          d={path}
          fill="none"
          className="stroke-success"
          strokeWidth={2}
          strokeLinejoin="round"
        />
        {usable.map((point, index) => {
          const value = point.candidate ?? point.baseline;
          if (value === null) return null;
          return (
            <circle
              key={`${point.iteration}-${index}`}
              cx={x(index + 1)}
              cy={y(value)}
              r={4}
              strokeWidth={1.5}
              className={
                point.accepted
                  ? "fill-success stroke-success"
                  : "fill-page-canvas stroke-destructive"
              }
            >
              <title>
                {`#${point.iteration} ${point.accepted ? "landed" : "rejected"}: ${formatMetric(point.candidate, unit)} (baseline ${formatMetric(point.baseline, unit)})`}
              </title>
            </circle>
          );
        })}
        <text
          x={x(0)}
          y={HEIGHT - 8}
          textAnchor="middle"
          className="fill-faint-foreground text-[11px]"
        >
          start
        </text>
        {usable.map((point, index) =>
          slots <= 24 || index % Math.ceil(slots / 24) === 0 ? (
            <text
              key={`label-${point.iteration}-${index}`}
              x={x(index + 1)}
              y={HEIGHT - 8}
              textAnchor="middle"
              className="fill-faint-foreground text-[11px]"
            >
              {`#${point.iteration}`}
            </text>
          ) : null,
        )}
      </svg>
      <div className="mt-1 flex items-center gap-4 text-micro text-muted-foreground">
        <Legend className="fill-success stroke-success" label="landed" />
        <Legend
          className="fill-page-canvas stroke-destructive"
          label="rejected"
        />
        <span className="inline-flex items-center gap-1.5">
          <span className="inline-block h-0.5 w-4 bg-success" />
          baseline (ratchet)
        </span>
      </div>
    </div>
  );
}

function Stat({
  label,
  value,
  strong = false,
  tone = "text-foreground",
}: {
  label: string;
  value: string;
  strong?: boolean;
  tone?: string;
}) {
  return (
    <span className="inline-flex items-baseline gap-1.5">
      <span className="text-micro uppercase tracking-[0.12em] text-faint-foreground">
        {label}
      </span>
      <span
        className={`font-mono ${strong ? "text-title-sm font-medium" : "text-label"} ${tone}`}
      >
        {value}
      </span>
    </span>
  );
}

function Legend({ className, label }: { className: string; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <svg viewBox="0 0 10 10" className="size-2.5" aria-hidden="true">
        <circle cx={5} cy={5} r={3.5} strokeWidth={1.5} className={className} />
      </svg>
      {label}
    </span>
  );
}

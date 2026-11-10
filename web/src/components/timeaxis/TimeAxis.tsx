/* The drawn parts of a time axis that the session trace and the missions
 * timeline share: clock ticks and their row, hairline gridlines, the "now"
 * line, the live tail of a span still running, the zoom buttons and the
 * dark tooltip box. */

import { scaleTime } from "d3-scale";
import { timeFormat } from "d3-time-format";
import { Maximize2, Minus, Plus } from "lucide-react";
import { useMemo } from "react";
import type { ReactNode } from "react";

import { LINEAR, breakLabel } from "../../timeFold";
import type { TimeFold } from "../../timeFold";
import { tickPattern } from "../../timeWindow";
import type { Domain } from "../../timeWindow";
import { cx } from "../ui";
import type { TimeWindow } from "./useTimeWindow";

/** Width of the fading tail on a span that is still running. */
export const LIVE_TAIL_PX = 28;

export interface Tick {
  x: number;
  label: string;
  key: number;
}

/** Clock ticks for a window: about one per 110 px, at least three. With a
 * fold (`domain` in folded time) each kept stretch gets its own ticks, and
 * across days the first tick after a break names its day. */
export function useTicks(domain: Domain, plotW: number, fold: TimeFold = LINEAR): Tick[] {
  return useMemo(() => {
    if (plotW <= 0) return [];
    const k = plotW / Math.max(domain.to - domain.from, 1);
    const x = (real: number) => (fold.fold(real) - domain.from) * k;
    const realFrom = fold.unfold(domain.from);
    const realTo = fold.unfold(domain.to);
    const multiDay = realTo - realFrom > 20 * 3_600_000;
    const stretches =
      fold.blocks.length > 1
        ? fold.blocks.map((block, index) => ({
            from: index === 0 ? -Infinity : block.from,
            to: index === fold.blocks.length - 1 ? Infinity : block.to,
          }))
        : [{ from: -Infinity, to: Infinity }];
    const single = stretches.length === 1;
    const out: Tick[] = [];
    let lastX = -Infinity;
    let previousDay: string | null = null;
    const day = timeFormat("%a");
    for (const stretch of stretches) {
      const a = Math.max(stretch.from, realFrom);
      const b = Math.min(stretch.to, realTo);
      if (!(b > a)) continue;
      const width = x(b) - x(a);
      if (width < 40) continue;
      const time = scaleTime().domain([new Date(a), new Date(b)]).range([x(a), x(b)]);
      const values = time.ticks(single ? Math.max(3, Math.floor(width / 110)) : Math.max(1, Math.floor(width / 110)));
      const step = values.length > 1 ? values[1].getTime() - values[0].getTime() : 3_600_000;
      const format = timeFormat(tickPattern(step));
      for (const value of values) {
        const at = time(value);
        if (at - lastX < 64) continue;
        const name = day(value);
        const label =
          multiDay && name !== previousDay ? `${name} ${format(value)}` : format(value);
        previousDay = name;
        out.push({ x: at, label, key: value.getTime() });
        lastX = at;
      }
    }
    return out;
  }, [domain.from, domain.to, plotW, fold]);
}

/** A folded gap on screen: from x0 to x1, with what it stands for. */
export interface BreakMark {
  x0: number;
  x1: number;
  label: string;
  key: number;
}

/** The breaks of a fold in plot coordinates, those on screen. */
export function useBreaks(domain: Domain, plotW: number, fold: TimeFold): BreakMark[] {
  return useMemo(() => {
    if (plotW <= 0) return [];
    const k = plotW / Math.max(domain.to - domain.from, 1);
    return fold.breaks
      .map((item) => ({
        x0: (item.at - domain.from) * k,
        x1: (item.at + fold.keep - domain.from) * k,
        label: breakLabel(item),
        key: item.from,
      }))
      .filter((mark) => mark.x1 >= 0 && mark.x0 <= plotW);
  }, [domain.from, domain.to, plotW, fold]);
}

/** Folded gaps in the plot: a quiet band between two dashed edges. */
export function BreakBands({ breaks, height }: { breaks: BreakMark[]; height: number }) {
  return (
    <g>
      {breaks.map((mark) => (
        <g key={mark.key}>
          <rect
            x={mark.x0}
            y={0}
            width={Math.max(mark.x1 - mark.x0, 1)}
            height={height}
            fill="var(--muted)"
            opacity={0.55}
          />
          {/* Dashed edges only where the band is wide enough to read as
              one: a run of narrow breaks (a session's hours between
              bursts) stays a quiet tint, not a field of dashes. */}
          {(mark.x1 - mark.x0 >= 12 ? [mark.x0, mark.x1] : []).map((x, index) => (
            <line
              key={index}
              x1={x}
              x2={x}
              y1={0}
              y2={height}
              stroke="var(--faint-foreground)"
              strokeWidth={1}
              strokeDasharray="2 3"
              opacity={0.45}
            />
          ))}
        </g>
      ))}
    </g>
  );
}

/** The tick labels above the plot, with a caption over the label column. */
export function AxisRow({
  ticks,
  labelW,
  plotW,
  caption,
  breaks = [],
}: {
  ticks: Tick[];
  labelW: number;
  plotW: number;
  caption: string;
  breaks?: BreakMark[];
}) {
  return (
    <div className="relative mt-2 h-7 shrink-0 border-b border-surface-border">
      <span className="absolute inset-y-0 left-4 flex items-center text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground sm:left-5">
        {caption}
      </span>
      <div className="absolute inset-y-0 overflow-hidden" style={{ left: labelW, width: plotW }}>
        {ticks.map((tick, index) => (
          <span
            key={tick.key}
            className="tnum absolute top-1/2 whitespace-nowrap text-micro text-muted-foreground"
            style={{
              left: tick.x,
              transform: `translate(${index === 0 && tick.x < 30 ? "0" : "-50%"}, -50%)`,
            }}
          >
            {tick.label}
          </span>
        ))}
        {breaks.map((mark) =>
          mark.x1 - mark.x0 >= 18 ? (
            <span
              key={`break-${mark.key}`}
              title={mark.label}
              className="absolute inset-y-1 flex items-center justify-center overflow-hidden rounded-sm bg-muted text-micro whitespace-nowrap text-faint-foreground"
              style={{ left: mark.x0, width: mark.x1 - mark.x0 }}
            >
              {mark.x1 - mark.x0 >= 64 ? mark.label : "⋯"}
            </span>
          ) : null,
        )}
      </div>
    </div>
  );
}

/** Hairline gridlines at the ticks, under everything else. */
export function Gridlines({ ticks, height }: { ticks: Tick[]; height: number }) {
  return (
    <g>
      {ticks.map((tick) => (
        <line
          key={tick.key}
          x1={tick.x}
          x2={tick.x}
          y1={0}
          y2={height}
          stroke="var(--border)"
          strokeWidth={1}
          shapeRendering="crispEdges"
        />
      ))}
    </g>
  );
}

/** A dashed line at now, when anything still runs. */
export function NowLine({ x, height, plotW }: { x: number | null; height: number; plotW: number }) {
  if (x === null || x < 0 || x > plotW) return null;
  return (
    <line
      x1={x}
      x2={x}
      y1={0}
      y2={height}
      stroke="var(--faint-foreground)"
      strokeWidth={1}
      strokeDasharray="2 4"
      opacity={0.6}
    />
  );
}

/** A horizontal fade from a colour to almost nothing, for live tails. */
export function TailGradient({ id, color }: { id: string; color: string }) {
  return (
    <linearGradient id={id} x1="0" x2="1">
      <stop offset="0" stopColor={color} stopOpacity="1" />
      <stop offset="1" stopColor={color} stopOpacity="0.15" />
    </linearGradient>
  );
}

/** The end of a bar that has not ended: a fading tail and, while it is
 * live, a slowly breathing 2 px edge. A stalled one (its mission halted)
 * keeps the tail but not the breath. */
export function LiveTail({
  x0,
  x1,
  cy,
  h,
  color,
  gradient,
  stalled = false,
}: {
  x0: number;
  x1: number;
  cy: number;
  h: number;
  color: string;
  gradient: string;
  stalled?: boolean;
}) {
  const w = Math.max(x1 - x0, 0);
  const tail = Math.min(w, LIVE_TAIL_PX);
  return (
    <>
      <rect x={x1 - tail} y={cy - h / 2} width={tail} height={h} fill={`url(#${gradient})`} />
      <line
        x1={x1}
        x2={x1}
        y1={cy - h / 2 - 3}
        y2={cy + h / 2 + 3}
        stroke={color}
        strokeWidth={2}
        strokeLinecap="round"
        strokeDasharray={stalled ? "2 3" : undefined}
        className={stalled ? undefined : "animate-live-edge"}
      />
    </>
  );
}

export function IconButton({
  label,
  onClick,
  active = false,
  pressed,
  children,
}: {
  label: string;
  onClick: () => void;
  active?: boolean;
  /** For a toggle: its state, as aria-pressed. */
  pressed?: boolean;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      aria-pressed={pressed}
      title={label}
      onClick={onClick}
      className={cx(
        "grid size-7 place-items-center rounded-md transition-colors hover:bg-surface-hover hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring",
        active ? "text-foreground" : "text-muted-foreground",
      )}
    >
      {children}
    </button>
  );
}

/** −, + and fit, with the gesture hint on wide screens. */
export function ZoomControls({ view, fitLabel }: { view: TimeWindow; fitLabel: string }) {
  return (
    <div className="ml-auto flex shrink-0 items-center gap-1">
      <span className="mr-2 hidden text-caption text-faint-foreground 2xl:inline">
        ⌘/Ctrl + scroll to zoom · drag to pan
      </span>
      <IconButton label="Zoom out (−)" onClick={() => view.zoomBy(2)}>
        <Minus size={13} />
      </IconButton>
      <IconButton label="Zoom in (+)" onClick={() => view.zoomBy(0.5)}>
        <Plus size={13} />
      </IconButton>
      <IconButton label={`${fitLabel} (0)`} onClick={view.fit} active={view.follow.mode === "fit"}>
        <Maximize2 size={12} />
      </IconButton>
    </div>
  );
}

/** The dark tooltip box, beside the pointer, flipped left near the edge. */
export function FloatingTip({
  x,
  y,
  flip,
  children,
}: {
  x: number;
  y: number;
  flip: boolean;
  children: ReactNode;
}) {
  return (
    <div
      role="tooltip"
      className="pointer-events-none absolute z-20 w-max max-w-80 rounded-md bg-[var(--tooltip)] px-2.5 py-1.5 text-caption text-white shadow-[var(--tooltip-shadow)]"
      style={{
        left: x + 14,
        top: y + 16,
        transform: flip ? "translateX(calc(-100% - 28px))" : undefined,
      }}
    >
      {children}
    </div>
  );
}

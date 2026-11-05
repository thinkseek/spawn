import { useEffect, useMemo, useRef } from "react";
import type { PointerEvent } from "react";

import { clampDomain, panDomain, spanEnd, zoomDomain } from "../../trace";
import type { Domain, Trace } from "../../trace";
import { KIND_COLOR } from "./style";

const HEIGHT = 40;
const PAD_Y = 5;
/** How close to a brush edge a press grabs the edge instead of the body. */
const EDGE_PX = 6;

type Grab =
  | { mode: "move"; x: number; domain: Domain }
  | { mode: "from" | "to"; domain: Domain }
  | { mode: "select"; x: number; moved: boolean };

/** The whole session compressed into one strip, with the visible window as
 * a brush: drag it to pan, drag its edges to resize, drag elsewhere to pick
 * a new window, click to centre there, scroll to zoom. */
export function Overview({
  trace,
  extent,
  domain,
  now,
  width,
  onDomain,
  onGlide,
}: {
  trace: Trace;
  extent: Domain;
  domain: Domain;
  now: number;
  width: number;
  onDomain: (domain: Domain) => void;
  onGlide: (domain: Domain) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const grab = useRef<Grab | null>(null);
  const msPerPx = (extent.to - extent.from) / Math.max(width, 1);
  const px = (time: number) => (time - extent.from) / msPerPx;
  const timeAt = (clientX: number) => {
    const left = ref.current?.getBoundingClientRect().left ?? 0;
    return extent.from + (clientX - left) * msPerPx;
  };

  // Each span as a hairline at its row's height in the full list: the
  // strip reads as a thumbnail of the trace below.
  const lines = useMemo(() => {
    const count = Math.max(trace.spans.length - 1, 1);
    const band = HEIGHT - PAD_Y * 2;
    return trace.spans.map((span, index) => ({
      id: span.id,
      y: PAD_Y + (trace.spans.length === 1 ? band / 2 : (index / count) * band),
      from: span.start,
      to: spanEnd(span, now),
      color: span.failed && span.kind === "tool" ? "var(--destructive)" : KIND_COLOR[span.kind],
    }));
  }, [trace, now]);

  const latest = useRef({ domain, extent, msPerPx, onDomain });
  latest.current = { domain, extent, msPerPx, onDomain };
  useEffect(() => {
    const element = ref.current;
    if (!element) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      const { domain: current, extent: full, msPerPx: scale, onDomain: set } =
        latest.current;
      const left = element.getBoundingClientRect().left;
      const anchor = full.from + (event.clientX - left) * scale;
      const lines = event.deltaMode === 1 ? 16 : 1;
      if (Math.abs(event.deltaX) > Math.abs(event.deltaY)) {
        set(panDomain(current, event.deltaX * lines * scale, full));
      } else {
        const factor = Math.exp(event.deltaY * lines * (event.ctrlKey ? 0.01 : 0.002));
        set(zoomDomain(current, factor, anchor, full));
      }
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, []);

  if (width <= 0) return <div className="flex-1" style={{ height: HEIGHT }} />;

  const x0 = Math.max(0, px(domain.from));
  const x1 = Math.min(width, px(domain.to));
  const whole = x1 - x0 >= width - 1;

  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    const left = ref.current!.getBoundingClientRect().left;
    const x = event.clientX - left;
    if (!whole && Math.abs(x - x0) <= EDGE_PX) grab.current = { mode: "from", domain };
    else if (!whole && Math.abs(x - x1) <= EDGE_PX) grab.current = { mode: "to", domain };
    else if (!whole && x > x0 && x < x1) {
      grab.current = { mode: "move", x: event.clientX, domain };
    } else grab.current = { mode: "select", x: event.clientX, moved: false };
    event.currentTarget.setPointerCapture(event.pointerId);
  };
  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    const active = grab.current;
    if (!active) return;
    switch (active.mode) {
      case "move":
        onDomain(panDomain(active.domain, (event.clientX - active.x) * msPerPx, extent));
        break;
      case "from":
        onDomain(
          clampDomain(
            { from: Math.min(timeAt(event.clientX), active.domain.to - 1), to: active.domain.to },
            extent,
          ),
        );
        break;
      case "to":
        onDomain(
          clampDomain(
            { from: active.domain.from, to: Math.max(timeAt(event.clientX), active.domain.from + 1) },
            extent,
          ),
        );
        break;
      case "select": {
        if (!active.moved && Math.abs(event.clientX - active.x) < 3) return;
        active.moved = true;
        const a = timeAt(active.x);
        const b = timeAt(event.clientX);
        onDomain(clampDomain({ from: Math.min(a, b), to: Math.max(a, b) }, extent));
        break;
      }
    }
  };
  const onPointerUp = (event: PointerEvent<HTMLDivElement>) => {
    const active = grab.current;
    grab.current = null;
    if (active?.mode === "select" && !active.moved) {
      // A click: centre the current window on that moment.
      const half = (domain.to - domain.from) / 2;
      const at = timeAt(event.clientX);
      onGlide(clampDomain({ from: at - half, to: at + half }, extent));
    }
  };

  return (
    <div
      ref={ref}
      role="slider"
      aria-label="Visible time window"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round((x0 / Math.max(width, 1)) * 100)}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={() => (grab.current = null)}
      className="relative shrink-0 cursor-crosshair touch-none select-none rounded-md bg-muted/60"
      style={{ width, height: HEIGHT }}
    >
      <svg width={width} height={HEIGHT} className="absolute inset-0" aria-hidden="true">
        <g strokeLinecap="round" strokeWidth={2}>
          {lines.map((line) => {
            const a = px(line.from);
            const b = Math.max(px(line.to), a + 1);
            return (
              <line
                key={line.id}
                x1={a}
                x2={b}
                y1={line.y}
                y2={line.y}
                stroke={line.color}
                opacity={0.75}
              />
            );
          })}
        </g>
        {!whole && (
          <>
            <rect x={0} y={0} width={x0} height={HEIGHT} fill="var(--page-canvas)" opacity={0.6} />
            <rect
              x={x1}
              y={0}
              width={Math.max(0, width - x1)}
              height={HEIGHT}
              fill="var(--page-canvas)"
              opacity={0.6}
            />
            <rect
              x={x0 + 0.5}
              y={0.5}
              width={Math.max(x1 - x0 - 1, 1)}
              height={HEIGHT - 1}
              rx={5}
              fill="none"
              stroke="var(--muted-foreground)"
              strokeOpacity={0.55}
            />
            {[x0, x1].map((x, index) => (
              <line
                key={index}
                x1={x}
                x2={x}
                y1={HEIGHT / 2 - 7}
                y2={HEIGHT / 2 + 7}
                stroke="var(--foreground)"
                strokeWidth={2}
                strokeLinecap="round"
                className="cursor-ew-resize"
              />
            ))}
          </>
        )}
      </svg>
    </div>
  );
}

import { AlertTriangle, ChevronRight } from "lucide-react";
import { AnimatePresence, useReducedMotion } from "motion/react";
import { useCallback, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent, MouseEvent, PointerEvent, ReactNode } from "react";

import type { TimelineEvent } from "../../api";
import { pairs } from "../../eventFormat";
import { clockTime, formatDuration } from "../../format";
import { fitDomain } from "../../timeWindow";
import { chainOf, deriveTrace, spanEnd, visibleSpans } from "../../trace";
import type { Span, SpanKind, TraceMarker } from "../../trace";
import { Overview } from "../timeaxis/Overview";
import type { OverviewLine } from "../timeaxis/Overview";
import {
  AxisRow,
  FloatingTip,
  Gridlines,
  LIVE_TAIL_PX,
  LiveTail,
  NowLine,
  TailGradient,
  ZoomControls,
  useTicks,
} from "../timeaxis/TimeAxis";
import {
  useDragPan,
  useNowWhile,
  useTimeWindow,
  useWheelZoom,
  useWidth,
} from "../timeaxis/useTimeWindow";
import { cx } from "../ui";
import { SpanDetail } from "./SpanDetail";
import { KIND_COLOR, KIND_LABEL, KIND_ORDER, markerColor } from "./style";

const ROW_H = 28;
const BAR_H: Record<SpanKind, number> = {
  command: 12,
  execution: 12,
  turn: 10,
  tool: 10,
};
const INDENT_PX = 14;
const BOTTOM_SLACK_PX = 56;

/** A visible row: a span, or the session row for events no span owns. */
interface Row {
  key: string;
  span: Span | null;
  markers: TraceMarker[];
}

interface Hover {
  rowKey: string;
  marker: TraceMarker | null;
  x: number;
  y: number;
}

export function TraceView({
  events,
  header,
}: {
  events: TimelineEvent[];
  header: ReactNode;
}) {
  const reduceMotion = useReducedMotion() ?? false;
  const gradientId = useId();
  const trace = useMemo(() => deriveTrace(events, 0), [events]);

  // Open spans run to now; the clock only ticks while one is open.
  const now = useNowWhile(trace.openCount > 0);
  const traceEnd = trace.openCount > 0 ? Math.max(trace.end, now) : trace.end;
  const extent = useMemo(
    () => fitDomain({ start: trace.start, end: traceEnd }),
    [trace.start, traceEnd],
  );

  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [selectedEvent, setSelectedEvent] = useState<number | null>(null);
  const [hover, setHover] = useState<Hover | null>(null);

  const rows = useMemo<Row[]>(() => {
    const list: Row[] = [];
    if (trace.markers.length > 0) {
      list.push({ key: "session", span: null, markers: trace.markers });
    }
    for (const span of visibleSpans(trace, collapsed)) {
      list.push({ key: span.id, span, markers: span.markers });
    }
    return list;
  }, [trace, collapsed]);
  const rowIndex = useMemo(
    () => new Map(rows.map((row, index) => [row.key, index])),
    [rows],
  );

  // Geometry.
  const rootRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const rowsRef = useRef<HTMLDivElement>(null);
  const width = useWidth(scrollRef);
  const labelW = width < 640 ? 150 : 264;
  const plotW = Math.max(0, width - labelW - 20);
  const view = useTimeWindow(extent, plotW, reduceMotion);
  const { domain, px, glideTo, zoomBy } = view;
  useWheelZoom(scrollRef, view, labelW);
  const pan = useDragPan(view);
  const zoomToSpan = useCallback(
    (span: Span) => view.zoomTo(span.start, spanEnd(span, view.latest.current.extent.to)),
    [view],
  );

  // Stick to the newest rows while the reader is at the bottom.
  const stickToBottom = useRef(true);
  const [following, setFollowing] = useState(true);
  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element && stickToBottom.current) element.scrollTop = element.scrollHeight;
  }, [rows.length]);

  // Pointer: hover, click to select, drag to pan, all on the row stack.
  const locate = (event: PointerEvent | MouseEvent) => {
    const rect = rowsRef.current!.getBoundingClientRect();
    const index = Math.floor((event.clientY - rect.top) / ROW_H);
    const x = event.clientX - rect.left - labelW;
    const row = rows[index] ?? null;
    let marker: TraceMarker | null = null;
    if (row && x >= 0) {
      let best = 7;
      for (const candidate of row.markers) {
        const distance = Math.abs(px(candidate.at) - x);
        if (distance < best) {
          best = distance;
          marker = candidate;
        }
      }
    }
    return { row, x, marker };
  };

  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    const { x } = locate(event);
    if (x < 0) return;
    pan.begin(event.clientX);
    event.currentTarget.setPointerCapture(event.pointerId);
  };
  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    if (pan.move(event.clientX)) {
      setHover(null);
      return;
    }
    const { row, marker } = locate(event);
    const rootRect = rootRef.current!.getBoundingClientRect();
    setHover(
      row
        ? {
            rowKey: row.key,
            marker,
            x: event.clientX - rootRect.left,
            y: event.clientY - rootRect.top,
          }
        : null,
    );
  };
  const onPointerUp = (event: PointerEvent<HTMLDivElement>) => {
    if (pan.end()) return;
    const { row, marker } = locate(event);
    if (!row) return;
    setSelected(row.key);
    setSelectedEvent(marker?.event.event_id ?? null);
  };

  const select = (key: string | null) => {
    setSelected(key);
    setSelectedEvent(null);
  };
  const toggle = (id: string) =>
    setCollapsed((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const index = selected ? (rowIndex.get(selected) ?? -1) : -1;
    const span = index >= 0 ? rows[index].span : null;
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp": {
        event.preventDefault();
        const step = event.key === "ArrowDown" ? 1 : -1;
        const next = rows[Math.min(rows.length - 1, Math.max(0, index + step))];
        if (next) {
          select(next.key);
          scrollRowIntoView(scrollRef.current, rowIndex.get(next.key) ?? 0);
        }
        break;
      }
      case "ArrowLeft":
      case "ArrowRight":
        if (span && span.childIds.length > 0) {
          event.preventDefault();
          const isCollapsed = collapsed.has(span.id);
          if (isCollapsed === (event.key === "ArrowRight")) toggle(span.id);
        }
        break;
      case "Enter":
        if (span) zoomToSpan(span);
        break;
      case "Escape":
        select(null);
        break;
      case "+":
      case "=":
        zoomBy(0.5);
        break;
      case "-":
        zoomBy(2);
        break;
      case "0":
        glideTo(extent);
        break;
    }
  };

  // Highlight: the hovered span's chain, else the selected one's.
  const focusKey = hover?.rowKey ?? selected;
  const chain = useMemo(
    () =>
      focusKey && focusKey !== "session" && trace.byId.has(focusKey)
        ? chainOf(trace, focusKey)
        : null,
    [trace, focusKey],
  );

  const ticks = useTicks(domain, plotW);
  const overviewLines = useMemo<OverviewLine[]>(() => {
    const count = Math.max(trace.spans.length - 1, 1);
    return trace.spans.map((span, index) => ({
      id: span.id,
      from: span.start,
      to: spanEnd(span, traceEnd),
      y: trace.spans.length === 1 ? 0.5 : index / count,
      color: span.failed && span.kind === "tool" ? "var(--destructive)" : KIND_COLOR[span.kind],
    }));
  }, [trace, traceEnd]);

  const height = rows.length * ROW_H;
  const selectedRow = selected ? rows[rowIndex.get(selected) ?? -1] : undefined;
  const nowX = trace.openCount > 0 ? px(now) : null;

  return (
    <div ref={rootRef} className="relative flex min-h-0 flex-1 flex-col">
      <div className="flex h-11 shrink-0 items-center gap-3 px-4 sm:px-5">
        {header}
        <Legend />
        <ZoomControls view={view} fitLabel="Fit the whole session" />
      </div>

      <div className="flex shrink-0 items-stretch px-4 sm:px-5">
        <div
          className="flex shrink-0 flex-col justify-center pr-3 text-caption"
          style={{ width: labelW - 16 }}
        >
          <span className="tnum text-muted-foreground">
            {formatDuration(traceEnd - trace.start)}
          </span>
          <span className="tnum truncate text-faint-foreground">
            {trace.spans.length} spans
            {trace.openCount > 0 && ` · ${trace.openCount} running`}
          </span>
        </div>
        <Overview
          lines={overviewLines}
          extent={extent}
          domain={domain}
          width={plotW}
          onDomain={view.setDomain}
          onGlide={glideTo}
        />
      </div>

      <div className="relative flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          <AxisRow ticks={ticks} labelW={labelW} plotW={plotW} caption="Span" />

          <div
            ref={scrollRef}
            onScroll={() => {
              const element = scrollRef.current;
              if (!element) return;
              const atBottom =
                element.scrollHeight - element.scrollTop - element.clientHeight <
                BOTTOM_SLACK_PX;
              stickToBottom.current = atBottom;
              if (atBottom !== following) setFollowing(atBottom);
            }}
            className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden pb-6"
          >
            <div
              ref={rowsRef}
              role="tree"
              aria-label="Session trace"
              tabIndex={0}
              onKeyDown={onKeyDown}
              onPointerDown={onPointerDown}
              onPointerMove={onPointerMove}
              onPointerUp={onPointerUp}
              onPointerCancel={() => pan.cancel()}
              onPointerLeave={() => setHover(null)}
              onDoubleClick={(event) => {
                const { row, x } = locate(event);
                if (row?.span && x >= 0) zoomToSpan(row.span);
              }}
              className={cx(
                "relative select-none outline-none",
                pan.dragging ? "cursor-grabbing" : "cursor-default",
              )}
              style={{ height }}
            >
              {/* Row backgrounds and labels */}
              {rows.map((row, index) => (
                <RowLabel
                  key={row.key}
                  row={row}
                  top={index * ROW_H}
                  labelW={labelW}
                  hovered={hover?.rowKey === row.key}
                  selected={selected === row.key}
                  dimmed={chain !== null && !(row.span && chain.has(row.span.id))}
                  collapsed={row.span ? collapsed.has(row.span.id) : false}
                  onToggle={toggle}
                  onSelect={select}
                />
              ))}

              <svg
                aria-hidden="true"
                width={plotW}
                height={height}
                className="pointer-events-none absolute top-0 overflow-hidden"
                style={{ left: labelW }}
              >
                <defs>
                  {KIND_ORDER.map((kind) => (
                    <TailGradient key={kind} id={`${gradientId}-${kind}`} color={KIND_COLOR[kind]} />
                  ))}
                  <clipPath id={`${gradientId}-plot`}>
                    <rect x={0} y={0} width={plotW} height={height} />
                  </clipPath>
                </defs>
                <g clipPath={`url(#${gradientId}-plot)`}>
                  <Gridlines ticks={ticks} height={height} />
                  <NowLine x={nowX} height={height} plotW={plotW} />
                  <Connectors
                    rows={rows}
                    rowIndex={rowIndex}
                    trace={trace}
                    px={px}
                    plotW={plotW}
                    now={now}
                    chain={chain}
                  />
                  {rows.map((row, index) =>
                    row.span ? (
                      <SpanBar
                        key={row.key}
                        span={row.span}
                        cy={index * ROW_H + ROW_H / 2}
                        px={px}
                        plotW={plotW}
                        now={now}
                        gradientId={gradientId}
                        dimmed={chain !== null && !chain.has(row.span.id)}
                      />
                    ) : null,
                  )}
                  {rows.map((row, index) => (
                    <Markers
                      key={row.key}
                      markers={row.markers}
                      cy={index * ROW_H + ROW_H / 2}
                      px={px}
                      plotW={plotW}
                      hovered={hover?.rowKey === row.key ? hover.marker : null}
                      dimmed={
                        chain !== null && !(row.span && chain.has(row.span.id))
                      }
                    />
                  ))}
                </g>
              </svg>
            </div>
          </div>
        </div>

        <AnimatePresence>
          {selectedRow && (
            <SpanDetail
              key="detail"
              row={selectedRow}
              trace={trace}
              now={now}
              selectedEvent={selectedEvent}
              reduceMotion={reduceMotion}
              onClose={() => select(null)}
              onSelect={(id) => {
                select(id);
                const index = rowIndex.get(id);
                if (index !== undefined) scrollRowIntoView(scrollRef.current, index);
              }}
              onZoom={zoomToSpan}
            />
          )}
        </AnimatePresence>
      </div>

      {hover && !pan.dragging && (
        <Tooltip
          hover={hover}
          row={rows[rowIndex.get(hover.rowKey) ?? -1]}
          now={now}
          flip={hover.x > (rootRef.current?.clientWidth ?? 0) - 340}
        />
      )}

      {!following && (
        <button
          type="button"
          onClick={() => {
            const element = scrollRef.current;
            if (!element) return;
            stickToBottom.current = true;
            setFollowing(true);
            element.scrollTo({
              top: element.scrollHeight,
              behavior: reduceMotion ? "auto" : "smooth",
            });
          }}
          className="absolute bottom-4 left-1/2 inline-flex h-7 -translate-x-1/2 items-center gap-1.5 rounded-md border border-border-strong bg-surface-raised px-2.5 text-caption font-medium text-foreground shadow-[var(--floating-shadow)] hover:bg-surface-selected"
        >
          Jump to latest
        </button>
      )}
    </div>
  );
}

function scrollRowIntoView(element: HTMLElement | null, index: number) {
  if (!element) return;
  const top = index * ROW_H;
  if (top < element.scrollTop) element.scrollTop = top;
  else if (top + ROW_H > element.scrollTop + element.clientHeight) {
    element.scrollTop = top + ROW_H - element.clientHeight;
  }
}

function Legend() {
  return (
    <ul className="hidden items-center gap-3 md:flex" aria-label="Span kinds">
      {KIND_ORDER.map((kind) => (
        <li key={kind} className="flex items-center gap-1.5 text-caption text-muted-foreground">
          <span
            aria-hidden="true"
            className="h-1.5 w-3 rounded-full"
            style={{ background: KIND_COLOR[kind] }}
          />
          {KIND_LABEL[kind]}
        </li>
      ))}
      <li className="flex items-center gap-1.5 text-caption text-muted-foreground">
        <AlertTriangle size={11} className="text-destructive" aria-hidden="true" />
        Failed
      </li>
    </ul>
  );
}

function RowLabel({
  row,
  top,
  labelW,
  hovered,
  selected,
  dimmed,
  collapsed,
  onToggle,
  onSelect,
}: {
  row: Row;
  top: number;
  labelW: number;
  hovered: boolean;
  selected: boolean;
  dimmed: boolean;
  collapsed: boolean;
  onToggle: (id: string) => void;
  onSelect: (key: string) => void;
}) {
  const span = row.span;
  const depth = span?.depth ?? 0;
  const hasChildren = (span?.childIds.length ?? 0) > 0;
  return (
    <div
      role="treeitem"
      aria-level={depth + 1}
      aria-selected={selected}
      aria-expanded={hasChildren ? !collapsed : undefined}
      className={cx(
        "absolute inset-x-0 flex items-center transition-colors duration-150 motion-reduce:transition-none",
        selected ? "bg-surface-selected" : hovered ? "bg-surface-hover" : "",
      )}
      style={{ top, height: ROW_H }}
    >
      <div
        className={cx(
          "flex h-full shrink-0 items-center gap-1.5 pr-3 transition-opacity duration-150 motion-reduce:transition-none",
          dimmed && "opacity-50",
        )}
        style={{ width: labelW, paddingLeft: 12 + depth * INDENT_PX }}
        onPointerDown={(event) => event.stopPropagation()}
      >
        {hasChildren ? (
          <button
            type="button"
            tabIndex={-1}
            aria-label={collapsed ? "Expand" : "Collapse"}
            onClick={() => span && onToggle(span.id)}
            className="grid size-4 shrink-0 place-items-center rounded text-faint-foreground hover:text-foreground"
          >
            <ChevronRight
              size={12}
              className={cx(
                "transition-transform duration-150 motion-reduce:transition-none",
                !collapsed && "rotate-90",
              )}
            />
          </button>
        ) : (
          <span className="size-4 shrink-0" />
        )}
        <button
          type="button"
          tabIndex={-1}
          onClick={() => onSelect(row.key)}
          className="flex min-w-0 flex-1 items-center gap-2 text-left"
        >
          {span ? (
            <span
              aria-hidden="true"
              className="size-2 shrink-0 rounded-full"
              style={{ background: KIND_COLOR[span.kind] }}
            />
          ) : (
            <span aria-hidden="true" className="size-2 shrink-0 rounded-full bg-faint-foreground" />
          )}
          <span
            className={cx(
              "min-w-0 max-w-full shrink-0 truncate",
              span && span.kind !== "tool"
                ? "text-label text-foreground"
                : "font-mono text-caption text-foreground/85",
            )}
          >
            {span ? span.label : "Session"}
          </span>
          {span?.failed && (
            <span className="flex min-w-0 flex-1 items-center gap-1 text-caption text-muted-foreground">
              <AlertTriangle size={11} className="shrink-0 text-destructive" aria-label="Failed" />
              <span className="truncate">{span.outcome}</span>
            </span>
          )}
          {span && !span.failed && span.kind === "execution" && span.outcome && (
            <span className="shrink-0 text-caption text-faint-foreground">{span.outcome}</span>
          )}
          {span?.state === "orphan" && (
            <span className="shrink-0 text-caption text-faint-foreground">no start</span>
          )}
          {span?.state === "open" && span.kind !== "tool" && (
            <span className="sr-only">running</span>
          )}
        </button>
      </div>
    </div>
  );
}

function Connectors({
  rows,
  rowIndex,
  trace,
  px,
  plotW,
  now,
  chain,
}: {
  rows: Row[];
  rowIndex: Map<string, number>;
  trace: ReturnType<typeof deriveTrace>;
  px: (time: number) => number;
  plotW: number;
  now: number;
  chain: Set<string> | null;
}) {
  const quiet: string[] = [];
  const lit: Array<{ d: string; color: string }> = [];
  rows.forEach((row, index) => {
    const span = row.span;
    if (!span?.parentId) return;
    const parentIndex = rowIndex.get(span.parentId);
    const parent = trace.byId.get(span.parentId);
    if (parentIndex === undefined || !parent) return;
    const parentBottom = parentIndex * ROW_H + ROW_H / 2 + BAR_H[parent.kind] / 2;
    const childMid = index * ROW_H + ROW_H / 2;
    const childTop = childMid - BAR_H[span.kind] / 2;
    const xs = px(span.start);
    const xe = px(spanEnd(span, now));
    const r = 3;
    let d = "";
    // At the start: down from the parent's row, then a short elbow into
    // the child's leading edge. At the end: up from the trailing edge.
    if (xs >= -r && xs <= plotW) {
      d += `M${xs - r} ${parentBottom}V${childMid - r}Q${xs - r} ${childMid} ${xs} ${childMid}`;
    }
    if (span.end !== null && xe - xs > 2 * r + 2 && xe >= 0 && xe <= plotW + r) {
      d += `M${xe} ${childTop}V${parentBottom}`;
    }
    if (!d) return;
    if (chain?.has(span.id) && chain.has(parent.id)) {
      lit.push({ d, color: KIND_COLOR[span.kind] });
    } else quiet.push(d);
  });
  return (
    <g fill="none" strokeLinecap="round">
      <path
        d={quiet.join("")}
        stroke="var(--faint-foreground)"
        strokeWidth={1}
        opacity={chain ? 0.18 : 0.38}
      />
      {lit.map((connector, index) => (
        <path
          key={index}
          d={connector.d}
          stroke={connector.color}
          strokeWidth={1}
          opacity={0.75}
        />
      ))}
    </g>
  );
}

function SpanBar({
  span,
  cy,
  px,
  plotW,
  now,
  gradientId,
  dimmed,
}: {
  span: Span;
  cy: number;
  px: (time: number) => number;
  plotW: number;
  now: number;
  gradientId: string;
  dimmed: boolean;
}) {
  // A failed tool call is short: it turns red. A failed long span keeps its
  // kind and gets a red trailing end, so failure stays a small mark.
  const color = span.failed && span.kind === "tool" ? "var(--destructive)" : KIND_COLOR[span.kind];
  const failedEnd = span.failed && span.kind !== "tool";
  const h = BAR_H[span.kind];
  const start = px(span.start);
  const end = px(spanEnd(span, now));
  if (end < -8 || start > plotW + 8) return null;
  const style = {
    opacity: dimmed ? 0.28 : 1,
    transition: "opacity 150ms ease",
  };

  if (span.state === "orphan") {
    // An end with no start: a hollow diamond where the end landed.
    const s = 5;
    return (
      <path
        d={`M${start} ${cy - s}L${start + s} ${cy}L${start} ${cy + s}L${start - s} ${cy}Z`}
        fill="var(--page-canvas)"
        stroke={color}
        strokeWidth={1.5}
        style={style}
      />
    );
  }

  const x0 = Math.max(start, -8);
  const x1 = Math.min(Math.max(end, start + 2), plotW + 8);
  const w = Math.max(x1 - x0, 2);
  const radius = Math.min(4, w / 2, h / 2);
  const duration = spanEnd(span, now) - span.start;
  const label = formatDuration(duration);
  const labelW = label.length * 6.2;
  let labelX: number | null = null;
  let anchor: "start" | "end" = "start";
  if (x1 + 6 + labelW <= plotW) labelX = x1 + 6;
  else if (x0 - 6 - labelW >= 0) {
    labelX = x0 - 6;
    anchor = "end";
  }

  return (
    <g style={style}>
      {span.state === "open" ? (
        <>
          <rect
            x={x0}
            y={cy - h / 2}
            width={Math.max(w - LIVE_TAIL_PX, 0) + radius}
            height={h}
            rx={radius}
            fill={color}
          />
          <LiveTail
            x0={x0}
            x1={x1}
            cy={cy}
            h={h}
            color={color}
            gradient={`${gradientId}-${span.kind}`}
          />
        </>
      ) : (
        <rect x={x0} y={cy - h / 2} width={w} height={h} rx={radius} fill={color} />
      )}
      {failedEnd && span.state !== "open" && end <= plotW + 8 && (
        <path
          d={`M${x1 - Math.min(w, 7)} ${cy - h / 2}H${x1 - radius}Q${x1} ${cy - h / 2} ${x1} ${cy - h / 2 + radius}V${cy + h / 2 - radius}Q${x1} ${cy + h / 2} ${x1 - radius} ${cy + h / 2}H${x1 - Math.min(w, 7)}Z`}
          fill="var(--destructive)"
        />
      )}
      {labelX !== null && (
        <text
          x={labelX}
          y={cy}
          dy="0.35em"
          textAnchor={anchor}
          className="tnum"
          fontSize={11}
          fill="var(--muted-foreground)"
        >
          {label}
        </text>
      )}
    </g>
  );
}

function Markers({
  markers,
  cy,
  px,
  plotW,
  hovered,
  dimmed,
}: {
  markers: TraceMarker[];
  cy: number;
  px: (time: number) => number;
  plotW: number;
  hovered: TraceMarker | null;
  dimmed: boolean;
}) {
  if (markers.length === 0) return null;
  return (
    <g style={{ opacity: dimmed ? 0.3 : 1, transition: "opacity 150ms ease" }}>
      {markers.map((marker) => {
        const x = px(marker.at);
        if (x < -6 || x > plotW + 6) return null;
        const active = hovered === marker;
        return (
          <circle
            key={marker.event.event_id}
            cx={x}
            cy={cy}
            r={active ? 5.5 : 4}
            fill={markerColor(marker)}
            stroke="var(--page-canvas)"
            strokeWidth={2}
          />
        );
      })}
    </g>
  );
}

function Tooltip({
  hover,
  row,
  now,
  flip,
}: {
  hover: Hover;
  row: Row | undefined;
  now: number;
  flip: boolean;
}) {
  if (!row) return null;
  let body: ReactNode;
  if (hover.marker) {
    const event = hover.marker.event;
    const fields = pairs(event.detail).slice(0, 4);
    body = (
      <>
        <div className="flex items-center gap-2">
          <span
            className="size-2 shrink-0 rounded-full"
            style={{ background: markerColor(hover.marker) }}
          />
          <span className="font-medium">{event.event_name}</span>
        </div>
        <div className="tnum mt-0.5 text-white/60">
          {clockTime(event.recorded_at_ms)} · {event.source}
        </div>
        {fields.map(([key, value], index) => (
          <div key={index} className="mt-0.5 max-w-72 truncate font-mono text-micro text-white/75">
            {key && <span className="text-white/45">{key}=</span>}
            {value}
          </div>
        ))}
      </>
    );
  } else if (row.span) {
    const span = row.span;
    const end = spanEnd(span, now);
    body = (
      <>
        <div className="flex items-center gap-2">
          <span
            className="size-2 shrink-0 rounded-full"
            style={{ background: KIND_COLOR[span.kind] }}
          />
          <span className="font-medium">{span.label}</span>
          <span className="text-white/50">{KIND_LABEL[span.kind].toLowerCase()}</span>
        </div>
        <div className="tnum mt-0.5 text-white/60">
          {clockTime(span.start)}
          {span.state !== "orphan" && (
            <> → {span.end === null ? "running" : clockTime(end)}</>
          )}
          {span.state !== "orphan" && <> · {formatDuration(end - span.start)}</>}
        </div>
        {span.failed && (
          <div className="mt-0.5 flex items-center gap-1 text-white/80">
            <AlertTriangle size={11} className="text-destructive" />
            {span.outcome}
          </div>
        )}
        {span.state === "orphan" && (
          <div className="mt-0.5 text-white/60">Ended here; its start is not in the log.</div>
        )}
      </>
    );
  } else {
    body = <div>{row.markers.length} session events</div>;
  }
  return (
    <FloatingTip x={hover.x} y={hover.y} flip={flip}>
      {body}
    </FloatingTip>
  );
}

import { AlertTriangle, ChevronRight, Crosshair, FoldHorizontal } from "lucide-react";
import { AnimatePresence, useReducedMotion } from "motion/react";
import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent, MouseEvent, PointerEvent, ReactNode } from "react";

import type { TimelineEvent } from "../../api";
import { pairs } from "../../eventFormat";
import { clockTime, formatDuration } from "../../format";
import { LINEAR, sameFoldMap } from "../../timeFold";
import { fitDomain } from "../../timeWindow";
import { chainOf, deriveTrace, foldRuns, rowText, runEnd, spanEnd, visibleSpans } from "../../trace";
import type { KnownCommand, KnownExecution, Span, SpanKind, Trace, TraceMarker } from "../../trace";
import { fitExecution, runWindow, spanCounts, traceFold } from "../../traceWindow";
import { useStoredChoice } from "../ViewToggle";
import { Overview } from "../timeaxis/Overview";
import type { OverviewLine } from "../timeaxis/Overview";
import {
  AxisRow,
  BreakBands,
  FloatingTip,
  IconButton,
  Gridlines,
  LIVE_TAIL_PX,
  LiveTail,
  NowLine,
  TailGradient,
  ZoomControls,
  useBreaks,
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

/** A visible row: a span, or the session row for events no span owns. A
 * tool row can stand for a run of identical calls (`run`, ×N); opened, the
 * run's calls follow as their own rows. */
interface Row {
  key: string;
  span: Span | null;
  run: Span[];
  unfolded: boolean;
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
  executions,
  commands,
  header,
}: {
  events: TimelineEvent[];
  executions: readonly KnownExecution[];
  commands: ReadonlyMap<string, KnownCommand>;
  header: ReactNode;
}) {
  const reduceMotion = useReducedMotion() ?? false;
  const gradientId = useId();
  const trace = useMemo(
    () => deriveTrace(events, 0, executions, commands),
    [events, executions, commands],
  );

  // Open spans run to now; the clock only ticks while one is open.
  const now = useNowWhile(trace.openCount > 0);
  const traceEnd = trace.openCount > 0 ? Math.max(trace.end, now) : trace.end;
  // Geometry.
  const rootRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const rowsRef = useRef<HTMLDivElement>(null);
  const width = useWidth(scrollRef);
  const labelW = width < 640 ? 150 : 264;
  const plotW = Math.max(0, width - labelW - 20);

  // Idle gaps of 10 min or more fold into marked breaks (§4.4), as on the
  // missions timeline; the reader can switch to a linear axis.
  const [folding, setFolding] = useStoredChoice("spawn.trace.fold", ["on", "off"] as const, "on");
  const fold = useMemo(
    () => (folding === "off" ? LINEAR : traceFold(trace, traceEnd, { plotW })),
    [folding, trace, traceEnd, plotW],
  );
  const extent = useMemo(
    () => fitDomain({ start: fold.fold(trace.start), end: fold.fold(traceEnd) }),
    [fold, trace.start, traceEnd],
  );

  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set());
  const [unfoldedRuns, setUnfoldedRuns] = useState<Set<string>>(() => new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [selectedEvent, setSelectedEvent] = useState<number | null>(null);
  const [hover, setHover] = useState<Hover | null>(null);

  const rows = useMemo<Row[]>(() => {
    const list: Row[] = [];
    if (trace.markers.length > 0) {
      list.push({ key: "session", span: null, run: [], unfolded: false, markers: trace.markers });
    }
    for (const { span, run, unfolded } of foldRuns(visibleSpans(trace, collapsed), unfoldedRuns)) {
      const markers = unfolded || run.length === 1 ? span.markers : run.flatMap((call) => call.markers);
      list.push({ key: span.id, span, run, unfolded, markers });
    }
    return list;
  }, [trace, collapsed, unfoldedRuns]);
  const rowIndex = useMemo(
    () => new Map(rows.map((row, index) => [row.key, index])),
    [rows],
  );

  const view = useTimeWindow(extent, plotW, reduceMotion);
  const { domain, glideTo, zoomBy } = view;
  const viewPx = view.px;
  // Real time to plot x, through the fold.
  const px = useCallback((time: number) => viewPx(fold.fold(time)), [viewPx, fold]);
  useWheelZoom(scrollRef, view, labelW);
  const pan = useDragPan(view);
  const zoomToSpan = useCallback(
    (span: Span) => view.zoomTo(fold.fold(span.start), fold.fold(spanEnd(span, traceEnd))),
    [view, fold, traceEnd],
  );
  // Fit an execution: from its start to the live end while it runs (its
  // start stays in view as it grows), else its whole span with some air.
  const fitRun = useCallback(
    (key: string | null) => {
      const run = fitExecution(trace, key);
      if (!run) {
        view.fit();
        return;
      }
      const target = runWindow(run, fold, traceEnd);
      if (target.mode === "since") view.since(target.from);
      else if (target.mode === "fixed") view.glideTo(target.domain);
    },
    [trace, fold, traceEnd, view],
  );
  // The window is kept on the folded axis; when the fold's map changes (the
  // Linear toggle, a live session's new break or band width), move it to
  // the same real times on the new one, before the frame is drawn. Only
  // then: a live trace rebuilds its fold every second with the same map
  // (sameFoldMap), and moving nothing each tick is what it should do.
  const shownFold = useRef(fold);
  const { refold } = view;
  useLayoutEffect(() => {
    const old = shownFold.current;
    if (sameFoldMap(old, fold)) return;
    shownFold.current = fold;
    refold((time) => fold.fold(old.unfold(time)));
  }, [fold, refold]);
  // The first window fits the current execution (§4.4), not the session's
  // lifetime: once, when the trace has one and the plot has a width.
  const fitted = useRef(false);
  useEffect(() => {
    if (fitted.current || plotW <= 0 || !fitExecution(trace)) return;
    fitted.current = true;
    fitRun(null);
  }, [trace, plotW, fitRun]);

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
  const flip = (current: Set<string>, id: string) => {
    const next = new Set(current);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    return next;
  };
  const toggle = (id: string) => setCollapsed((current) => flip(current, id));
  const toggleRun = (id: string) => setUnfoldedRuns((current) => flip(current, id));

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const index = selected ? (rowIndex.get(selected) ?? -1) : -1;
    const row = index >= 0 ? rows[index] : null;
    const span = row?.span ?? null;
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
        } else if (span && row && row.run.length > 1) {
          event.preventDefault();
          if (row.unfolded === (event.key === "ArrowLeft")) toggleRun(span.id);
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
      case "e":
        fitRun(selected);
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

  const ticks = useTicks(domain, plotW, fold);
  const breaks = useBreaks(domain, plotW, fold);
  const overviewLines = useMemo<OverviewLine[]>(() => {
    const count = Math.max(trace.spans.length - 1, 1);
    return trace.spans.map((span, index) => ({
      id: span.id,
      from: fold.fold(span.start),
      to: fold.fold(spanEnd(span, traceEnd)),
      y: trace.spans.length === 1 ? 0.5 : index / count,
      color: span.failed && span.kind === "tool" ? "var(--destructive)" : KIND_COLOR[span.kind],
    }));
  }, [trace, traceEnd, fold]);

  const height = rows.length * ROW_H;
  const selectedRow = selected ? rows[rowIndex.get(selected) ?? -1] : undefined;
  const nowX = trace.openCount > 0 ? px(now) : null;

  return (
    <div ref={rootRef} className="relative flex min-h-0 flex-1 flex-col">
      <div className="flex h-11 shrink-0 items-center gap-3 px-4 sm:px-5">
        {header}
        <Legend />
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {fold !== LINEAR || folding === "off" ? (
            <IconButton
              label={
                folding === "on"
                  ? "Idle folded: stretches of 10 min or more with nothing happening are marked breaks; click for a linear axis"
                  : "Linear axis; click to fold idle stretches into breaks"
              }
              active={folding === "on"}
              pressed={folding === "on"}
              onClick={() => setFolding(folding === "on" ? "off" : "on")}
            >
              <FoldHorizontal size={13} />
            </IconButton>
          ) : null}
          {fitExecution(trace) && (
            <IconButton
              label={
                selected
                  ? "Fit the selected span's execution (e)"
                  : "Fit the current execution (e)"
              }
              onClick={() => fitRun(selected)}
            >
              <Crosshair size={13} />
            </IconButton>
          )}
          <ZoomControls view={view} fitLabel="Fit the whole session" />
        </div>
      </div>

      <div className="flex shrink-0 items-stretch px-4 sm:px-5">
        <div
          className="flex shrink-0 flex-col justify-center pr-3 text-caption"
          style={{ width: labelW - 16 }}
        >
          <span className="tnum text-muted-foreground">
            {formatDuration(traceEnd - trace.start)}
          </span>
          <span className="tnum truncate text-faint-foreground">{spanCounts(trace)}</span>
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
          <AxisRow ticks={ticks} breaks={breaks} labelW={labelW} plotW={plotW} caption="Span" />

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
                  trace={trace}
                  top={index * ROW_H}
                  labelW={labelW}
                  hovered={hover?.rowKey === row.key}
                  selected={selected === row.key}
                  dimmed={chain !== null && !(row.span && chain.has(row.span.id))}
                  collapsed={row.span ? collapsed.has(row.span.id) : false}
                  onToggle={toggle}
                  onToggleRun={toggleRun}
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
                  <BreakBands breaks={breaks} height={height} />
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
                  {rows.flatMap((row, index) =>
                    (row.unfolded ? row.run.slice(0, 1) : row.run).map((span) => (
                      <SpanBar
                        key={span.id}
                        span={span}
                        labelled={row.unfolded || row.run.length === 1}
                        cy={index * ROW_H + ROW_H / 2}
                        px={px}
                        plotW={plotW}
                        now={now}
                        gradientId={gradientId}
                        dimmed={chain !== null && !chain.has(row.key)}
                      />
                    )),
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
          trace={trace}
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
        <li key={kind} className="flex items-center gap-1.5 whitespace-nowrap text-caption text-muted-foreground">
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
  trace,
  top,
  labelW,
  hovered,
  selected,
  dimmed,
  collapsed,
  onToggle,
  onToggleRun,
  onSelect,
}: {
  row: Row;
  trace: Trace;
  top: number;
  labelW: number;
  hovered: boolean;
  selected: boolean;
  dimmed: boolean;
  collapsed: boolean;
  onToggle: (id: string) => void;
  onToggleRun: (id: string) => void;
  onSelect: (key: string) => void;
}) {
  const span = row.span;
  const depth = span?.depth ?? 0;
  const isRun = row.run.length > 1;
  const hasChildren = (span?.childIds.length ?? 0) > 0 || isRun;
  const shut = isRun ? !row.unfolded : collapsed;
  // A folded run of commands is its sender and action: their texts differ.
  const commandRun = isRun && !row.unfolded && span?.kind === "command";
  const text = span ? (commandRun ? span.label : rowText(span, trace)) : "Session";
  const last = row.run.at(-1);
  return (
    <div
      role="treeitem"
      aria-level={depth + 1}
      aria-selected={selected}
      aria-expanded={hasChildren ? !shut : undefined}
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
            aria-label={isRun ? (shut ? `List the ${row.run.length} calls` : "Fold the calls") : shut ? "Expand" : "Collapse"}
            onClick={() => span && (isRun ? onToggleRun(span.id) : onToggle(span.id))}
            className="grid size-4 shrink-0 place-items-center rounded text-faint-foreground hover:text-foreground"
          >
            <ChevronRight
              size={12}
              className={cx(
                "transition-transform duration-150 motion-reduce:transition-none",
                !shut && "rotate-90",
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
          title={span?.summary ? `${span.label} · ${span.summary}` : undefined}
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
              "min-w-0 max-w-full truncate",
              span?.kind !== "tool" && "shrink-0",
              span && span.kind !== "tool"
                ? "text-label text-foreground"
                : "font-mono text-caption text-foreground/85",
            )}
          >
            {text}
          </span>
          {isRun && (
            <span className="tnum shrink-0 text-caption font-medium text-muted-foreground">
              ×{row.run.length}
            </span>
          )}
          {commandRun && span && last && (
            <span className="tnum shrink-0 text-caption text-faint-foreground">
              {clockTime(span.start)}–{clockTime(last.start)}
            </span>
          )}
          {span?.failed && (
            <span
              className={cx(
                "flex min-w-0 items-center gap-1 text-caption text-muted-foreground",
                // A tool's outcome is short (`exit 1`); its input gives way first.
                span.kind === "tool" ? "max-w-[45%] shrink-0" : "flex-1",
              )}
            >
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
  labelled = true,
}: {
  span: Span;
  cy: number;
  px: (time: number) => number;
  plotW: number;
  now: number;
  gradientId: string;
  dimmed: boolean;
  /** Its duration beside it: not for the calls of a folded run, whose
   * labels would stack on one row. */
  labelled?: boolean;
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
  if (!labelled) labelX = null;
  else if (x1 + 6 + labelW <= plotW) labelX = x1 + 6;
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
  trace,
  now,
  flip,
}: {
  hover: Hover;
  row: Row | undefined;
  trace: Trace;
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
    const folded = row.run.length > 1 && !row.unfolded;
    const end = folded ? runEnd(row.run, now) : spanEnd(span, now);
    body = (
      <>
        <div className="flex items-center gap-2">
          <span
            className="size-2 shrink-0 rounded-full"
            style={{ background: KIND_COLOR[span.kind] }}
          />
          <span className="max-w-72 truncate font-medium">
            {folded && span.kind === "command" ? span.label : rowText(span, trace)}
          </span>
          <span className="shrink-0 text-white/50">
            {folded ? `×${row.run.length} calls` : KIND_LABEL[span.kind].toLowerCase()}
          </span>
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

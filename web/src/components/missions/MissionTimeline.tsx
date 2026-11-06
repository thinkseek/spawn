import { Ban, Check, ChevronRight, FoldHorizontal, OctagonAlert, RotateCcw } from "lucide-react";
import { AnimatePresence, useReducedMotion } from "motion/react";
import { useCallback, useId, useMemo, useRef, useState } from "react";
import type { KeyboardEvent, MouseEvent, PointerEvent, ReactNode } from "react";

import { clockTime, formatDuration } from "../../format";
import {
  PHASE_LABEL,
  STATE_FILTERS,
  deriveMissionTimeline,
  filterLanes,
  phaseAt,
} from "../../missionTimeline";
import type {
  Lane,
  Segment,
  StateFilter,
  TaskBar,
  VerdictMark,
  VerdictReasons,
} from "../../missionTimeline";
import type { Mission } from "../../missions";
import type { TaskSection } from "../../ordering";
import { SECTION_LABEL, groupTasks, missionClass, taskTitle } from "../../ordering";
import { LINEAR, foldIdle } from "../../timeFold";
import { fitDomain } from "../../timeWindow";
import { Overview } from "../timeaxis/Overview";
import type { OverviewLine } from "../timeaxis/Overview";
import {
  AxisRow,
  BreakBands,
  FloatingTip,
  Gridlines,
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
import { FOCUS_RING, FOCUS_RING_INSET, cx } from "../ui";
import { useStoredChoice } from "../ViewToggle";
import { TaskPanel } from "./TaskPanel";
import { PHASE_COLOR, PHASE_LEGEND, verdictColor } from "./timelineStyle";

const LANE_H = 40;
const SECTION_H = 24;
const ROW_H = 34;
const ATTEMPT_H = 28;
const BAR_H = 14;
const RADIUS = 4;
/** Gap between a bar's segments, in the surface colour. */
const GAP = 2;

type Row =
  | { kind: "lane"; key: string; lane: Lane; y: number; h: number }
  | { kind: "section"; key: string; lane: Lane; section: TaskSection; count: number; y: number; h: number }
  | {
      kind: "task";
      key: string;
      lane: Lane;
      bar: TaskBar;
      y: number;
      h: number;
      /** 0: a row of the list; 1: an earlier attempt nested under it. */
      depth: 0 | 1;
      title: string | null;
      /** How a nested attempt ended. */
      outcome: string | null;
      /** Attempts at the row's objective, this one included. */
      retries: number;
    };

interface Hover {
  key: string;
  verdict: VerdictMark | null;
  time: number;
  x: number;
  y: number;
}

/** Missions as lanes and their tasks as bars on one time axis: when each
 * task ran, how long every phase took, what the gate said and when work
 * was sent back. `compact` (the RSI page) shows one mission without lane
 * headers or the mission filter; `onSelectTask` there opens the page's own
 * task drawer instead of the panel. */
export function MissionTimeline({
  missions,
  header,
  reasons,
  compact = false,
  onSelectTask,
  selectedTask = null,
}: {
  missions: readonly Mission[];
  header: ReactNode;
  reasons?: VerdictReasons;
  compact?: boolean;
  onSelectTask?: (task: number) => void;
  selectedTask?: number | null;
}) {
  const reduceMotion = useReducedMotion() ?? false;
  const ids = useId();
  const all = useMemo(() => deriveMissionTimeline(missions, 0, reasons), [missions, reasons]);
  const live = all.lanes.some((lane) => lane.bars.some((bar) => bar.end === null && !bar.stalled));
  const now = useNowWhile(live);
  // Ordering and extent depend on now only through open bars.
  const timeline = useMemo(
    () => (all.openCount > 0 ? deriveMissionTimeline(missions, now, reasons) : all),
    [all, missions, now, reasons],
  );

  const [missionFilter, setMissionFilter] = useState<number | null>(null);
  const [stateFilter, setStateFilter] = useState<StateFilter>("all");
  // Folds the reader flipped: a closed mission's lane and the cancelled
  // section start folded, the rest open (see `folded`).
  const [toggled, setToggled] = useState<Set<string>>(() => new Set());
  const [moreMissions, setMoreMissions] = useState(false);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [hover, setHover] = useState<Hover | null>(null);

  const lanes = useMemo(
    () => filterLanes(timeline.lanes, missionFilter, stateFilter),
    [timeline, missionFilter, stateFilter],
  );
  // A filter shows what it picked: under one, nothing starts folded.
  const folded = useCallback(
    (key: string, byDefault: boolean) =>
      (byDefault && missionFilter === null && stateFilter === "all") !== toggled.has(key),
    [toggled, missionFilter, stateFilter],
  );
  const rows = useMemo(() => {
    const list: Row[] = [];
    let y = 0;
    for (const lane of lanes) {
      if (!compact) {
        list.push({ kind: "lane", key: lane.key, lane, y, h: LANE_H });
        y += LANE_H;
      }
      if (!compact && folded(lane.key, missionClass(lane.mission) === "closed")) continue;
      // The one task order (ordering.ts): in flight, landed or accepted,
      // sent back, cancelled; an objective's retries nest under its latest.
      for (const { section, groups } of groupTasks(lane.bars, (bar) => bar.task, lane.mission)) {
        const sectionKey = `${lane.key}:${section}`;
        list.push({ kind: "section", key: sectionKey, lane, section, count: groups.length, y, h: SECTION_H });
        y += SECTION_H;
        if (folded(sectionKey, section === "cancelled")) continue;
        for (const group of groups) {
          list.push({
            kind: "task",
            key: group.item.key,
            lane,
            bar: group.item,
            y,
            h: ROW_H,
            depth: 0,
            title: taskTitle(group.task, { missionName: lane.name }),
            outcome: null,
            retries: group.earlier.length + 1,
          });
          y += ROW_H;
          for (const attempt of group.earlier) {
            list.push({
              kind: "task",
              key: attempt.item.key,
              lane,
              bar: attempt.item,
              y,
              h: ATTEMPT_H,
              depth: 1,
              title: null,
              outcome: attempt.outcome,
              retries: 1,
            });
            y += ATTEMPT_H;
          }
        }
      }
    }
    return { list, height: y };
  }, [lanes, folded, compact]);

  // The axis spans what the filters show, so one mission fills the width.
  const shown = useMemo(() => {
    const bars = lanes.flatMap((lane) => lane.bars);
    if (bars.length === 0) return { bars, start: timeline.start, end: timeline.end };
    let start = Infinity;
    let end = -Infinity;
    for (const bar of bars) {
      start = Math.min(start, bar.start);
      end = Math.max(end, bar.end ?? now);
    }
    return { bars, start, end };
  }, [lanes, timeline, now]);

  // Idle days fold away, so bursts of work keep their width (see timeFold).
  const [folding, setFolding] = useStoredChoice("spawn.missions.fold", ["on", "off"] as const, "on");
  const fold = useMemo(() => {
    if (folding === "off") return LINEAR;
    return foldIdle(shown.bars.map((bar) => [bar.start, bar.end ?? shown.end] as const));
  }, [folding, shown]);
  const extent = useMemo(
    () => fitDomain({ start: fold.fold(shown.start), end: fold.fold(shown.end) }),
    [fold, shown],
  );
  const rootRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const rowsRef = useRef<HTMLDivElement>(null);
  const width = useWidth(scrollRef);
  // Rows carry a title beside the id, so the label column grows on wide screens.
  const labelW = width < 640 ? 132 : compact ? (width >= 900 ? 250 : 150) : width >= 1200 ? 330 : 236;
  const plotW = Math.max(0, width - labelW - 20);
  const view = useTimeWindow(extent, plotW, reduceMotion);
  const { domain } = view;
  const viewPx = view.px;
  const px = useCallback((time: number) => viewPx(fold.fold(time)), [viewPx, fold]);
  useWheelZoom(scrollRef, view, labelW);
  const pan = useDragPan(view);
  const ticks = useTicks(domain, plotW, fold);
  const breaks = useBreaks(domain, plotW, fold);

  const overviewLines = useMemo<OverviewLine[]>(() => {
    const bars = shown.bars;
    const count = Math.max(bars.length - 1, 1);
    return bars.flatMap((bar, index) =>
      bar.segments.map((segment, at) => ({
        id: `${bar.key}:${at}`,
        from: fold.fold(segment.start),
        to: fold.fold(segment.end ?? shown.end),
        y: bars.length === 1 ? 0.5 : index / count,
        color: PHASE_COLOR[segment.phase],
      })),
    );
  }, [shown, fold]);

  const selected =
    (onSelectTask
      ? rows.list.find((row) => row.kind === "task" && row.bar.task.id === selectedTask)
      : rows.list.find((row) => row.key === selectedKey)) ?? null;
  const selectedRow = selected?.kind === "task" ? selected : null;

  const zoomToBar = (bar: TaskBar) =>
    view.zoomTo(fold.fold(bar.start), fold.fold(bar.end ?? now));
  const choose = (row: Row) => {
    if (row.kind !== "task") {
      toggleFold(row.key);
      return;
    }
    if (onSelectTask) {
      if (row.bar.task.id !== null) onSelectTask(row.bar.task.id);
    } else setSelectedKey((current) => (current === row.key ? null : row.key));
  };
  const toggleFold = (key: string) =>
    setToggled((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

  const locate = (event: PointerEvent | MouseEvent) => {
    const rect = rowsRef.current!.getBoundingClientRect();
    const y = event.clientY - rect.top;
    const x = event.clientX - rect.left - labelW;
    const row = rows.list.find((entry) => y >= entry.y && y < entry.y + entry.h) ?? null;
    let verdict: VerdictMark | null = null;
    if (row?.kind === "task" && x >= 0) {
      let best = 7;
      for (const candidate of row.bar.verdicts) {
        const distance = Math.abs(px(candidate.at) - x);
        if (distance < best) {
          best = distance;
          verdict = candidate;
        }
      }
    }
    return { row, x, verdict, time: fold.unfold(domain.from + x * view.scale) };
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
    const { row, verdict, time, x } = locate(event);
    const rootRect = rootRef.current!.getBoundingClientRect();
    setHover(
      row && (row.kind === "task" || x < 0)
        ? {
            key: row.key,
            verdict,
            time,
            x: event.clientX - rootRect.left,
            y: event.clientY - rootRect.top,
          }
        : row
          ? { key: row.key, verdict: null, time, x: -1, y: -1 }
          : null,
    );
  };
  const onPointerUp = (event: PointerEvent<HTMLDivElement>) => {
    if (pan.end()) return;
    const { row, x } = locate(event);
    if (row && x >= 0) choose(row);
  };

  const taskRows = rows.list.filter((row): row is Extract<Row, { kind: "task" }> => row.kind === "task");
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const index = selected ? taskRows.findIndex((row) => row.key === selected.key) : -1;
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp": {
        event.preventDefault();
        const step = event.key === "ArrowDown" ? 1 : -1;
        const next = taskRows[Math.min(taskRows.length - 1, Math.max(0, index + step))];
        if (next) {
          choose(next);
          scrollIntoView(scrollRef.current, next.y, next.h);
        }
        break;
      }
      case "Enter":
        if (selectedRow) zoomToBar(selectedRow.bar);
        break;
      case "Escape":
        setSelectedKey(null);
        break;
      case "+":
      case "=":
        view.zoomBy(0.5);
        break;
      case "-":
        view.zoomBy(2);
        break;
      case "0":
        view.fit();
        break;
    }
  };

  const hoveredRow = hover ? rows.list.find((row) => row.key === hover.key) : undefined;
  const nowX = timeline.openCount > 0 ? px(now) : null;
  const totalTasks = timeline.lanes.reduce((sum, lane) => sum + lane.bars.length, 0);

  return (
    <div ref={rootRef} className="relative flex min-h-0 flex-1 flex-col">
      <div className="flex min-h-11 shrink-0 flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2 sm:px-5">
        {header}
        <Legend />
        <div className="ml-auto flex items-center gap-1">
          {fold !== LINEAR || folding === "off" ? (
            <button
              type="button"
              aria-pressed={folding === "on"}
              onClick={() => setFolding(folding === "on" ? "off" : "on")}
              title={
                folding === "on"
                  ? "Idle stretches of 30 min or more are folded into marked breaks; click for a linear axis"
                  : "A linear axis; click to fold idle stretches into breaks"
              }
              className={cx(
                "inline-flex h-7 items-center gap-1.5 rounded-md px-2 text-caption transition-colors hover:bg-surface-hover",
                folding === "on" ? "text-foreground" : "text-muted-foreground",
              )}
            >
              <FoldHorizontal size={13} />
              {folding === "on" ? "Idle folded" : "Linear"}
            </button>
          ) : null}
          <ZoomControls view={view} fitLabel="Fit every task" />
        </div>
      </div>

      {!compact && (
        <div className="flex shrink-0 flex-wrap items-center gap-x-4 gap-y-2 px-4 pb-2 sm:px-5">
          {timeline.lanes.length > 1 && (
            <MissionChips
              lanes={timeline.lanes}
              value={missionFilter}
              onChange={setMissionFilter}
              more={moreMissions}
              onMore={() => setMoreMissions(!moreMissions)}
            />
          )}
          <Chips
            label="Task state"
            value={stateFilter}
            onChange={setStateFilter}
            options={STATE_FILTERS.map((entry) => ({ id: entry.id, label: entry.label }))}
          />
        </div>
      )}

      {!compact && (
        <div className="flex shrink-0 items-stretch px-4 sm:px-5">
          <div
            className="flex shrink-0 flex-col justify-center pr-3 text-caption"
            style={{ width: labelW - 16 }}
          >
            <span className="tnum text-muted-foreground">
              {formatDuration(timeline.end - timeline.start)}
            </span>
            <span className="tnum truncate text-faint-foreground">
              {totalTasks} task{totalTasks === 1 ? "" : "s"}
              {timeline.openCount > 0 && ` · ${timeline.openCount} open`}
            </span>
          </div>
          <Overview
            lines={overviewLines}
            extent={extent}
            domain={domain}
            width={plotW}
            onDomain={view.setDomain}
            onGlide={view.glideTo}
          />
        </div>
      )}

      <div className="relative flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          <AxisRow
            ticks={ticks}
            breaks={breaks}
            labelW={labelW}
            plotW={plotW}
            caption={compact ? "Task" : "Mission · task"}
          />
          <div ref={scrollRef} className="min-h-0 flex-1 overflow-x-hidden overflow-y-auto pb-6">
            {rows.list.length === 0 ? (
              <p className="px-5 py-10 text-center text-caption text-faint-foreground">
                No task matches these filters.
              </p>
            ) : (
              <div
                ref={rowsRef}
                role="tree"
                aria-label="Missions timeline"
                tabIndex={0}
                onKeyDown={onKeyDown}
                onPointerDown={onPointerDown}
                onPointerMove={onPointerMove}
                onPointerUp={onPointerUp}
                onPointerCancel={() => pan.cancel()}
                onPointerLeave={() => setHover(null)}
                onDoubleClick={(event) => {
                  const { row, x } = locate(event);
                  if (row?.kind === "task" && x >= 0) zoomToBar(row.bar);
                }}
                className={cx(
                  "relative select-none",
                  FOCUS_RING_INSET,
                  pan.dragging ? "cursor-grabbing" : "cursor-default",
                )}
                style={{ height: rows.height }}
              >
                {rows.list.map((row) =>
                  row.kind === "lane" ? null : row.kind === "section" ? (
                    <SectionRowLabel
                      key={row.key}
                      row={row}
                      short={labelW < 200}
                      onSurface={compact}
                      folded={folded(row.key, row.section === "cancelled")}
                      onToggle={() => toggleFold(row.key)}
                    />
                  ) : (
                    <TaskLabel
                      key={row.key}
                      row={row}
                      labelW={labelW}
                      titled={labelW >= 200}
                      hovered={hover?.key === row.key}
                      selected={selected?.key === row.key}
                      onSelect={() => choose(row)}
                    />
                  ),
                )}

                <svg
                  aria-hidden="true"
                  width={plotW}
                  height={rows.height}
                  className="pointer-events-none absolute top-0 overflow-hidden"
                  style={{ left: labelW }}
                >
                  <defs>
                    {(Object.keys(PHASE_COLOR) as Array<keyof typeof PHASE_COLOR>).map((phase) => (
                      <TailGradient key={phase} id={`${ids}-tail-${phase}`} color={PHASE_COLOR[phase]} />
                    ))}
                    <pattern
                      id={`${ids}-hatch`}
                      width={5}
                      height={5}
                      patternUnits="userSpaceOnUse"
                      patternTransform="rotate(45)"
                    >
                      <rect width={5} height={5} fill="var(--warning)" opacity={0.14} />
                      <line x1={0} y1={0} x2={0} y2={5} stroke="var(--warning)" strokeWidth={2} opacity={0.75} />
                    </pattern>
                    <clipPath id={`${ids}-plot`}>
                      <rect x={0} y={0} width={plotW} height={rows.height} />
                    </clipPath>
                  </defs>
                  <g clipPath={`url(#${ids}-plot)`}>
                    <BreakBands breaks={breaks} height={rows.height} />
                    <Gridlines ticks={ticks} height={rows.height} />
                    <NowLine x={nowX} height={rows.height} plotW={plotW} />
                    {rows.list.map((row) =>
                      row.kind === "lane" ? (
                        <LaneBand key={row.key} row={row} px={px} now={now} />
                      ) : row.kind === "section" ? null : (
                        <Bar
                          key={row.key}
                          bar={row.bar}
                          cy={row.y + row.h / 2 + (row.depth === 0 ? 3 : 1)}
                          px={px}
                          plotW={plotW}
                          now={now}
                          ids={ids}
                          hovered={hover?.key === row.key ? hover.verdict : null}
                        />
                      ),
                    )}
                  </g>
                </svg>
                {/* Lane headers sit above the plot, on the canvas colour, so
                    their text never runs over a gridline. */}
                {rows.list.map((row) =>
                  row.kind === "lane" ? (
                    <LaneLabel
                      key={row.key}
                      row={row}
                      collapsed={folded(row.lane.key, missionClass(row.lane.mission) === "closed")}
                      onToggle={() => toggleFold(row.lane.key)}
                    />
                  ) : null,
                )}
              </div>
            )}
          </div>
        </div>

        {!onSelectTask && (
          <AnimatePresence>
            {selectedRow && (
              <TaskPanel
                key="task"
                bar={selectedRow.bar}
                lane={selectedRow.lane}
                now={now}
                reduceMotion={reduceMotion}
                onClose={() => setSelectedKey(null)}
                onZoom={() => zoomToBar(selectedRow.bar)}
              />
            )}
          </AnimatePresence>
        )}
      </div>

      {hover && hover.x >= 0 && !pan.dragging && hoveredRow?.kind === "task" && (
        <FloatingTip
          x={hover.x}
          y={hover.y}
          flip={hover.x > (rootRef.current?.clientWidth ?? 0) - 320}
        >
          <TaskTip
            bar={hoveredRow.bar}
            title={hoveredRow.title ?? taskTitle(hoveredRow.bar.task, { missionName: hoveredRow.lane.name })}
            verdict={hover.verdict}
            time={hover.time}
            now={now}
          />
        </FloatingTip>
      )}
    </div>
  );
}

function scrollIntoView(element: HTMLElement | null, top: number, height: number) {
  if (!element) return;
  if (top < element.scrollTop) element.scrollTop = top;
  else if (top + height > element.scrollTop + element.clientHeight) {
    element.scrollTop = top + height - element.clientHeight;
  }
}

function Legend() {
  return (
    <ul className="hidden min-w-0 flex-wrap items-center gap-x-3 gap-y-1 md:flex" aria-label="Phases">
      {PHASE_LEGEND.map((entry) => (
        <li key={entry.label} className="flex items-center gap-1.5 text-caption text-muted-foreground">
          <span
            aria-hidden="true"
            className="h-1.5 w-3 rounded-full"
            style={
              entry.hatch
                ? {
                    background: `repeating-linear-gradient(45deg, ${entry.color} 0 2px, transparent 2px 4px)`,
                  }
                : { background: entry.color }
            }
          />
          {entry.label}
        </li>
      ))}
      <li className="flex items-center gap-1.5 text-caption text-muted-foreground">
        <span aria-hidden="true" className="size-2 rounded-full" style={{ background: "var(--success)" }} />
        accepted
      </li>
      <li className="flex items-center gap-1.5 text-caption text-muted-foreground">
        <span aria-hidden="true" className="size-2 rounded-full" style={{ background: "var(--destructive)" }} />
        rejected
      </li>
      <li className="flex items-center gap-1.5 text-caption text-muted-foreground">
        <span aria-hidden="true" className="size-2 rounded-full" style={{ background: "var(--warning)" }} />
        conflict · incompatible
      </li>
    </ul>
  );
}

function Chips<T extends string | number | null>({
  label,
  value,
  onChange,
  options,
}: {
  label: string;
  value: T;
  onChange: (next: T) => void;
  options: ReadonlyArray<{ id: T; label: string }>;
}) {
  return (
    <div role="radiogroup" aria-label={label} className="flex min-w-0 flex-wrap items-center gap-1">
      {options.map((option) => (
        <button
          key={String(option.id)}
          type="button"
          role="radio"
          aria-checked={value === option.id}
          onClick={() => onChange(option.id)}
          className={cx(
            "h-6 max-w-56 truncate rounded-md px-2 text-caption transition-colors",
            FOCUS_RING,
            value === option.id
              ? "bg-surface-selected font-medium text-foreground"
              : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
          )}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

/** The mission filter, in the lanes' order. Closed missions sit behind a
 * "More" button, unless one of them is the one picked. */
function MissionChips({
  lanes,
  value,
  onChange,
  more,
  onMore,
}: {
  lanes: readonly Lane[];
  value: number | null;
  onChange: (next: number | null) => void;
  more: boolean;
  onMore: () => void;
}) {
  const closed = lanes.filter((lane) => missionClass(lane.mission) === "closed");
  const shown = lanes.filter((lane) => missionClass(lane.mission) !== "closed");
  const picked = closed.find((lane) => lane.mission.id === value);
  const extra = more ? closed : picked ? [picked] : [];
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-1">
      <Chips
        label="Mission"
        value={value}
        onChange={onChange}
        options={[
          { id: null, label: "All missions" },
          ...[...shown, ...extra].map((lane) => ({ id: lane.mission.id, label: lane.name })),
        ]}
      />
      {closed.length > 0 && (
        <button
          type="button"
          aria-expanded={more}
          onClick={onMore}
          title={more ? "Hide the closed missions" : "Show the closed missions"}
          className={cx(
            "inline-flex h-6 items-center gap-1 rounded-md px-2 text-caption text-muted-foreground transition-colors hover:bg-surface-hover hover:text-foreground",
            FOCUS_RING,
          )}
        >
          {more ? "Fewer" : `More · ${closed.length} closed`}
          <ChevronRight
            size={12}
            aria-hidden="true"
            className={cx("transition-transform motion-reduce:transition-none", more && "rotate-180")}
          />
        </button>
      )}
    </div>
  );
}

/** A section's header inside a lane: its name and count; it folds. */
/** Section names for a narrow label column. */
const SECTION_SHORT: Record<TaskSection, string> = {
  in_flight: "In flight",
  landed: "Landed",
  sent_back: "Sent back",
  cancelled: "Cancelled",
};

function SectionRowLabel({
  row,
  short,
  onSurface,
  folded,
  onToggle,
}: {
  row: Extract<Row, { kind: "section" }>;
  short: boolean;
  /** Drawn inside a card (the RSI page), not on the page canvas. */
  onSurface: boolean;
  folded: boolean;
  onToggle: () => void;
}) {
  return (
    <div
      role="treeitem"
      aria-level={2}
      aria-expanded={!folded}
      className="pointer-events-none absolute inset-x-0 flex items-end pl-7"
      style={{ top: row.y, height: row.h }}
      onPointerDown={(event) => event.stopPropagation()}
    >
      <button
        type="button"
        aria-expanded={!folded}
        onClick={onToggle}
        className={cx(
          "pointer-events-auto mb-0.5 inline-flex items-center gap-1.5 rounded px-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground hover:text-foreground",
          onSurface ? "bg-surface" : "bg-page-canvas",
          FOCUS_RING,
        )}
      >
        <ChevronRight
          size={11}
          aria-hidden="true"
          className={cx("transition-transform duration-150 motion-reduce:transition-none", !folded && "rotate-90")}
        />
        {(short ? SECTION_SHORT : SECTION_LABEL)[row.section]}
        <span className="tnum font-normal tracking-normal text-muted-foreground">{row.count}</span>
      </button>
    </div>
  );
}

const LANE_STATE: Record<Lane["state"], { label: string; dot: string; text: string }> = {
  active: { label: "active", dot: "bg-success", text: "text-muted-foreground" },
  halted: { label: "halted", dot: "bg-warning", text: "text-warning" },
  done: { label: "done", dot: "bg-faint-foreground", text: "text-muted-foreground" },
  closed: { label: "closed", dot: "bg-faint-foreground", text: "text-muted-foreground" },
};

function LaneLabel({
  row,
  collapsed,
  onToggle,
}: {
  row: Extract<Row, { kind: "lane" }>;
  collapsed: boolean;
  onToggle: () => void;
}) {
  const { lane } = row;
  const state = LANE_STATE[lane.state];
  const counts = lane.counts;
  return (
    <div
      role="treeitem"
      aria-level={1}
      aria-expanded={!collapsed}
      className="pointer-events-none absolute inset-x-0 flex items-end px-3 pb-1.5"
      style={{ top: row.y, height: row.h }}
      onPointerDown={(event) => event.stopPropagation()}
    >
      <button
        type="button"
        tabIndex={-1}
        onClick={onToggle}
        className="pointer-events-auto flex min-w-0 items-center gap-2 rounded-md bg-page-canvas py-0.5 pr-2 text-left outline-none"
      >
        <ChevronRight
          size={13}
          className={cx(
            "shrink-0 text-faint-foreground transition-transform duration-150 motion-reduce:transition-none",
            !collapsed && "rotate-90",
          )}
        />
        <span className="tnum shrink-0 font-mono text-caption text-faint-foreground">
          #{lane.mission.id ?? "?"}
        </span>
        <span className="min-w-0 max-w-[24rem] truncate text-label font-semibold text-foreground">
          {lane.name}
        </span>
        <span className={cx("flex min-w-0 shrink items-center gap-1.5 text-caption", state.text)}>
          <span aria-hidden="true" className={cx("size-1.5 shrink-0 rounded-full", state.dot)} />
          <span className="shrink-0">{state.label}</span>
          {lane.halted && (
            <span className="min-w-0 truncate text-warning/90" title={lane.halted}>
              · {lane.halted}
            </span>
          )}
        </span>
        <span className="tnum hidden shrink-0 text-caption text-faint-foreground sm:inline">
          {lane.mission.gate ? `accept ${lane.mission.accept === "gate" ? "gate" : "review → gate"}` : "review"}
          {" · "}
          {counts.total} task{counts.total === 1 ? "" : "s"}
          {counts.landed > 0 && ` · ${counts.landed} landed`}
          {counts.open > 0 && ` · ${counts.open} open`}
          {counts.sentBack > 0 && ` · ${counts.sentBack} sent back`}
          {counts.cancelled > 0 && ` · ${counts.cancelled} cancelled`}
        </span>
      </button>
    </div>
  );
}

const OUTCOME_ICON: Record<TaskBar["outcome"], ReactNode> = {
  landed: <Check size={12} className="text-success" aria-label="landed" />,
  accepted: <Check size={12} className="text-success" aria-label="accepted" />,
  cancelled: <Ban size={11} className="text-faint-foreground" aria-label="cancelled" />,
  aborted: <OctagonAlert size={12} className="text-destructive" aria-label="aborted" />,
  open: null,
};

function TaskLabel({
  row,
  labelW,
  titled,
  hovered,
  selected,
  onSelect,
}: {
  row: Extract<Row, { kind: "task" }>;
  labelW: number;
  /** Room for the title (and a nested attempt's outcome) beside the id. */
  titled: boolean;
  hovered: boolean;
  selected: boolean;
  onSelect: () => void;
}) {
  const { bar } = row;
  const nested = row.depth === 1;
  return (
    <div
      role="treeitem"
      aria-level={nested ? 4 : 3}
      aria-selected={selected}
      className={cx(
        "absolute inset-x-0 flex items-center transition-colors duration-150 motion-reduce:transition-none",
        selected ? "bg-surface-selected" : hovered ? "bg-surface-hover" : "",
      )}
      style={{ top: row.y, height: row.h }}
    >
      <button
        type="button"
        tabIndex={-1}
        onClick={onSelect}
        onPointerDown={(event) => event.stopPropagation()}
        title={row.title ? `${bar.label} · ${row.title}` : row.outcome ? `${bar.label} · ${row.outcome}` : undefined}
        className={cx(
          "flex h-full shrink-0 items-center gap-2 pr-3 text-left outline-none",
          nested ? "pl-12" : "pl-9",
        )}
        style={{ width: labelW }}
      >
        {nested ? (
          <>
            <span aria-hidden="true" className="h-3 w-2 shrink-0 border-b border-l border-border-strong" />
            <span className="tnum shrink-0 font-mono text-micro text-muted-foreground">{bar.label}</span>
            {titled && (
              <span
                className={cx(
                  "min-w-0 truncate text-micro",
                  row.outcome?.startsWith("aborted") ? "text-destructive" : "text-muted-foreground",
                )}
              >
                {row.outcome}
              </span>
            )}
          </>
        ) : (
          <>
            <span
              className={cx(
                "tnum shrink-0 font-mono",
                titled ? "text-micro text-faint-foreground" : "text-caption text-foreground/90",
              )}
            >
              {bar.label}
            </span>
            {titled && (
              <span className="min-w-0 flex-1 truncate text-caption text-foreground">
                {row.title ?? (bar.task.objective !== null ? `objective ${bar.task.objective}` : "")}
              </span>
            )}
          </>
        )}
        {!nested && row.retries > 1 && (
          <span
            className="tnum flex shrink-0 items-center gap-0.5 text-micro text-muted-foreground"
            title={`Attempt ${row.retries} at objective ${bar.task.objective ?? "?"}; the earlier ones are nested below`}
          >
            ×{row.retries}
          </span>
        )}
        {bar.attempts > 1 && (
          <span
            className="flex shrink-0 items-center gap-0.5 text-caption text-warning"
            title={`${bar.attempts} rounds: sent back ${bar.attempts - 1} time${bar.attempts === 2 ? "" : "s"}`}
          >
            <RotateCcw size={10} />
            {bar.attempts}
          </span>
        )}
        <span className={cx("flex shrink-0 items-center", nested ? "" : "ml-auto")}>{OUTCOME_ICON[bar.outcome]}</span>
      </button>
    </div>
  );
}

/** A thin band under a lane's header: where its work sits in time, which
 * is all that shows of a collapsed lane. */
function LaneBand({
  row,
  px,
  now,
}: {
  row: Extract<Row, { kind: "lane" }>;
  px: (time: number) => number;
  now: number;
}) {
  if (row.lane.bars.length === 0) return null;
  const y = row.y + row.h - 4;
  return (
    <g opacity={0.5}>
      {row.lane.bars.map((bar) => (
        <line
          key={bar.key}
          x1={px(bar.start)}
          x2={Math.max(px(bar.end ?? now), px(bar.start) + 2)}
          y1={y}
          y2={y}
          stroke="var(--muted-foreground)"
          strokeWidth={2}
          strokeLinecap="round"
        />
      ))}
    </g>
  );
}

/** A path for one segment: square inner edges, rounded data-ends. */
function segmentPath(x0: number, x1: number, top: number, h: number, left: boolean, right: boolean) {
  const w = Math.max(x1 - x0, 1);
  const rl = left ? Math.min(RADIUS, w / 2, h / 2) : 0;
  const rr = right ? Math.min(RADIUS, w / 2, h / 2) : 0;
  const bottom = top + h;
  return (
    `M${x0 + rl} ${top}H${x1 - rr}` +
    (rr ? `Q${x1} ${top} ${x1} ${top + rr}V${bottom - rr}Q${x1} ${bottom} ${x1 - rr} ${bottom}` : `V${bottom}`) +
    `H${x0 + rl}` +
    (rl ? `Q${x0} ${bottom} ${x0} ${bottom - rl}V${top + rl}Q${x0} ${top} ${x0 + rl} ${top}` : `V${top}`) +
    "Z"
  );
}

function Bar({
  bar,
  cy,
  px,
  plotW,
  now,
  ids,
  hovered,
}: {
  bar: TaskBar;
  cy: number;
  px: (time: number) => number;
  plotW: number;
  now: number;
  ids: string;
  hovered: VerdictMark | null;
}) {
  const start = px(bar.start);
  const end = px(bar.end ?? Math.max(now, bar.start));
  if (end < -40 || start > plotW + 40) return null;
  const top = cy - BAR_H / 2;
  const faded = bar.outcome === "cancelled" || bar.outcome === "aborted";
  const last = bar.segments.length - 1;
  const total = (bar.end ?? now) - bar.start;
  const totalLabel = formatDuration(total);
  const totalW = totalLabel.length * 6.2;
  const capRoom = bar.outcome === "open" ? 0 : 12;

  return (
    <g opacity={faded ? 0.5 : 1}>
      {bar.segments.map((segment, index) => (
        <SegmentMark
          key={index}
          segment={segment}
          first={index === 0}
          last={index === last}
          top={top}
          px={px}
          plotW={plotW}
          now={now}
          ids={ids}
          stalled={bar.stalled}
        />
      ))}
      {bar.returns.map((back, index) => {
        const from = px(back.from);
        const to = px(back.to);
        const lift = top - 2;
        const peak = top - 10;
        return (
          <g key={index} fill="none" stroke="var(--warning)" strokeWidth={1.25} strokeLinecap="round">
            <path d={`M${from} ${lift}C${from} ${peak} ${to} ${peak} ${to} ${lift}`} opacity={0.85} />
            <path d={`M${to - 3} ${lift - 4}L${to} ${lift}L${to + 3.5} ${lift - 3}`} opacity={0.85} />
          </g>
        );
      })}
      {bar.verdicts.map((verdict, index) => {
        const x = px(verdict.at);
        if (x < -6 || x > plotW + 6) return null;
        return (
          <circle
            key={index}
            cx={x}
            cy={cy}
            r={hovered === verdict ? 5.5 : 4}
            fill={verdictColor(verdict.verdict)}
            stroke="var(--page-canvas)"
            strokeWidth={2}
          />
        );
      })}
      <EndCap bar={bar} x={end} cy={cy} />
      {end + capRoom + 6 + totalW <= plotW && (
        <text
          x={end + capRoom + 6}
          y={cy}
          dy="0.35em"
          fontSize={11}
          className="tnum"
          fill="var(--muted-foreground)"
        >
          {totalLabel}
        </text>
      )}
    </g>
  );
}

function SegmentMark({
  segment,
  first,
  last,
  top,
  px,
  plotW,
  now,
  ids,
  stalled,
}: {
  segment: Segment;
  first: boolean;
  last: boolean;
  top: number;
  px: (time: number) => number;
  plotW: number;
  now: number;
  ids: string;
  stalled: boolean;
}) {
  const open = segment.end === null;
  const a = px(segment.start);
  const b = px(segment.end ?? Math.max(now, segment.start));
  if (b < -8 || a > plotW + 8) return null;
  // Clip far ends so huge zooms keep small numbers; leave a surface gap
  // before the next segment.
  const x0 = Math.max(a, -8);
  const x1 = Math.min(Math.max(b - (last ? 0 : GAP), a + 1), plotW + 8);
  const color = PHASE_COLOR[segment.phase];
  const revise = segment.phase === "revise";
  const fill = revise ? `url(#${ids}-hatch)` : color;
  const duration = (segment.end ?? now) - segment.start;
  const label = formatDuration(duration);
  const fits = !revise && segment.phase !== "queued" && x1 - x0 >= label.length * 5.8 + 10;
  const cy = top + BAR_H / 2;

  return (
    <g>
      {open ? (
        <>
          <path
            d={segmentPath(x0, Math.max(x1 - 28, x0 + 1), top, BAR_H, first, false)}
            fill={fill}
          />
          <LiveTail
            x0={x0}
            x1={x1}
            cy={cy}
            h={BAR_H}
            color={color}
            gradient={`${ids}-tail-${segment.phase}`}
            stalled={stalled}
          />
        </>
      ) : (
        <path d={segmentPath(x0, x1, top, BAR_H, first, last)} fill={fill} />
      )}
      {fits && (
        <text
          x={(x0 + x1) / 2}
          y={cy}
          dy="0.35em"
          textAnchor="middle"
          fontSize={10}
          fontWeight={500}
          className="tnum"
          fill="var(--bar-label)"
        >
          {label}
        </text>
      )}
    </g>
  );
}

/** How a bar ended: a check for landed, a cross for cancelled or aborted. */
function EndCap({ bar, x, cy }: { bar: TaskBar; x: number; cy: number }) {
  switch (bar.outcome) {
    case "landed":
    case "accepted":
      return (
        <g transform={`translate(${x + 6} ${cy})`}>
          <circle r={5} fill="var(--success)" stroke="var(--page-canvas)" strokeWidth={2} />
          <path d="M-2.2 0.1L-0.6 1.7L2.3 -1.6" fill="none" stroke="var(--page-canvas)" strokeWidth={1.5} strokeLinecap="round" strokeLinejoin="round" />
        </g>
      );
    case "cancelled":
    case "aborted": {
      const color = bar.outcome === "aborted" ? "var(--destructive)" : "var(--muted-foreground)";
      return (
        <path
          d={`M${x + 3} ${cy - 3.5}l7 7M${x + 10} ${cy - 3.5}l-7 7`}
          stroke={color}
          strokeWidth={1.75}
          strokeLinecap="round"
        />
      );
    }
    default:
      return null;
  }
}

function TaskTip({
  bar,
  title,
  verdict,
  time,
  now,
}: {
  bar: TaskBar;
  title: string | null;
  verdict: VerdictMark | null;
  time: number;
  now: number;
}) {
  if (verdict) {
    return (
      <>
        <div className="flex items-center gap-2">
          <span className="size-2 shrink-0 rounded-full" style={{ background: verdictColor(verdict.verdict) }} />
          <span className="font-medium">
            {verdict.by === "reviewed" ? "Review" : "Gate"} {verdict.verdict}
          </span>
          <span className="text-white/50">
            {bar.label}
            {bar.attempts > 1 && ` · try ${verdict.attempt}`}
          </span>
        </div>
        <div className="tnum mt-0.5 text-white/60">
          {clockTime(verdict.at)}
          {verdict.sentBack && " · sent back"}
        </div>
        {verdict.reason && <div className="mt-0.5 line-clamp-3 text-white/80">{verdict.reason}</div>}
      </>
    );
  }
  const segment = phaseAt(bar, time, now);
  const total = (bar.end ?? now) - bar.start;
  return (
    <>
      <div className="flex items-center gap-2">
        <span className="font-medium">{bar.label}</span>
        {bar.task.objective !== null && <span className="text-white/50">objective {bar.task.objective}</span>}
        <span className="text-white/50">
          {bar.outcome === "open" ? (bar.stalled ? "stalled" : "in flight") : bar.outcome}
        </span>
      </div>
      {title && <div className="mt-0.5 line-clamp-2 text-white/85">{title}</div>}
      {segment && (
        <div className="mt-0.5 flex items-center gap-1.5">
          <span className="h-1.5 w-2.5 rounded-full" style={{ background: PHASE_COLOR[segment.phase] }} />
          {PHASE_LABEL[segment.phase]}
          {bar.attempts > 1 && <span className="text-white/50">try {segment.attempt}</span>}
          <span className="tnum text-white/60">
            {formatDuration((segment.end ?? now) - segment.start)}
          </span>
        </div>
      )}
      <div className="tnum mt-0.5 text-white/60">
        {clockTime(bar.start)} → {bar.end === null ? "now" : clockTime(bar.end)} · {formatDuration(total)}
      </div>
    </>
  );
}

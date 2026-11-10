import { ArrowDown, ChevronsUp, GanttChart, List } from "lucide-react";
import { Fragment, Suspense, lazy, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";

import { sessionReader, usePoll } from "../api";
import { fetchCommands } from "../fleet";
import type { CommandRecord } from "../fleet";
import type { KnownCommand } from "../trace";
import type { Execution, TimelineEvent } from "../api";
import { hasEarlier, joinTail, mergeEvents, readBack } from "../sessionWindow";
import type { EventWindow } from "../sessionWindow";
import { eventTone, fullText, pairs } from "../eventFormat";
import { clockTime } from "../format";
import { EmptyState, cx } from "./ui";
import { ViewToggle, useStoredChoice } from "./ViewToggle";
import type { ViewOption } from "./ViewToggle";

// The trace carries Motion and d3: loaded when first shown, so the first
// page stays light.
const TraceView = lazy(() =>
  import("./trace/TraceView").then((module) => ({ default: module.TraceView })),
);

type Mode = "trace" | "list";
const MODES = ["trace", "list"] as const;
const MODE_OPTIONS: ReadonlyArray<ViewOption<Mode>> = [
  { id: "trace", label: "Trace", icon: <GanttChart size={13} /> },
  { id: "list", label: "List", icon: <List size={13} /> },
];

/** A session's events as a trace (spans on a time axis) or as the plain
 * list, for searching and copying. The choice is remembered. `tail` is the
 * newest window, polled; it joins the window held when they touch and
 * replaces it when they do not (a hidden tab), so no stretch is ever
 * missing unmarked; older events load when the reader asks
 * (sessionWindow.ts). */
export function Timeline({
  sessionId,
  tail,
  executions,
  to = null,
}: {
  sessionId: string;
  tail: EventWindow;
  executions: Execution[];
  /** Who the session's inputs go to (`supervisor`): the trace reads a
   * person's input as `You → supervisor` ([`inputTarget`]). */
  to?: string | null;
}) {
  const [mode, choose] = useStoredChoice<Mode>("spawn.timeline.mode", MODES, "trace");
  const [held, setHeld] = useState<EventWindow>(tail);
  const [loading, setLoading] = useState(false);
  const [failed, setFailed] = useState<string | null>(null);
  useEffect(() => setHeld((current) => joinTail(current, tail)), [tail]);
  const events = held.events;
  const earlier = hasEarlier(held);

  // What each command was, for the trace's command rows (UI note §4.3).
  const commandPoll = usePoll<CommandRecord[]>(
    `trace-commands:${sessionId}`,
    (signal) => fetchCommands(sessionId, signal, COMMANDS_READ),
    15_000,
  );
  const commands = useMemo(() => {
    const byId = new Map<string, KnownCommand>();
    for (const record of commandPoll.data ?? []) {
      if (record.command_id !== null) byId.set(String(record.command_id), record);
    }
    return byId;
  }, [commandPoll.data]);

  // An older page, once, on request; the component is keyed by session.
  const aborts = useRef<AbortController | null>(null);
  useEffect(() => () => aborts.current?.abort(), []);
  const loadEarlier = () => {
    if (!earlier || loading) return;
    const controller = new AbortController();
    aborts.current = controller;
    setLoading(true);
    setFailed(null);
    const from = held;
    readBack(from, sessionReader(sessionId, controller.signal))
      // Joined to what is held now, unless a new tail replaced it meanwhile.
      .then((older) => setHeld((current) => (current.after === from.after ? { ...older, through: current.through, events: mergeEvents(older.events, current.events) } : current)))
      .catch((error: unknown) => {
        if (!controller.signal.aborted) setFailed(error instanceof Error ? error.message : String(error));
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
  };

  if (events.length === 0) {
    // Nothing visible in what was read: more may lie before it (a long
    // stretch of the observer's hidden events), so "Earlier" stays.
    return (
      <div className="grid min-h-0 flex-1 place-items-center">
        <div className="flex flex-col items-center gap-3">
          <EmptyState
            title={earlier ? `No events from #${held.after + 1}` : "No events in this session"}
            hint={
              earlier
                ? "The latest stretch holds only the observer's own events."
                : "Events appear here as the agent's hooks report them."
            }
          />
          {earlier && (
            <button
              type="button"
              onClick={loadEarlier}
              disabled={loading}
              title={failed ?? undefined}
              className="inline-flex h-7 items-center gap-1.5 rounded-md px-2 text-caption text-muted-foreground hover:bg-surface-hover hover:text-foreground disabled:opacity-60"
            >
              <ChevronsUp size={13} />
              {loading ? "Loading…" : failed ? "Retry earlier" : "Earlier"}
            </button>
          )}
        </div>
      </div>
    );
  }

  const first = held.after + 1;
  const toggle = (
    <>
      <ViewToggle label="Timeline view" options={MODE_OPTIONS} value={mode} onChange={choose} />
      {earlier && (
        <button
          type="button"
          onClick={loadEarlier}
          disabled={loading}
          title={failed ?? `Events from #${first}; load the ones before them`}
          className="inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md px-2 text-caption text-muted-foreground hover:bg-surface-hover hover:text-foreground disabled:opacity-60"
        >
          <ChevronsUp size={13} />
          {loading ? "Loading…" : failed ? "Retry earlier" : `Earlier · from #${first}`}
        </button>
      )}
    </>
  );
  return mode === "trace" ? (
    <Suspense fallback={<div className="min-h-0 flex-1" />}>
      <TraceView events={events} executions={executions} commands={commands} to={to} header={toggle} />
    </Suspense>
  ) : (
    <EventList events={events} header={toggle} />
  );
}

const BOTTOM_SLACK_PX = 56;
/** The newest commands read for the trace's words: more than a page of
 * events can hold. */
const COMMANDS_READ = 300;

const COLUMNS =
  "md:grid-cols-[3rem_4.5rem_4.5rem_minmax(9rem,13rem)_minmax(0,1fr)]";

function EventList({ events, header }: { events: TimelineEvent[]; header: ReactNode }) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);
  const [following, setFollowing] = useState(true);
  const lastEventId = events.at(-1)?.event_id ?? null;

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element && stickToBottom.current) {
      element.scrollTop = element.scrollHeight;
    }
  }, [lastEventId, events.length]);

  return (
    <div className="relative flex min-h-0 flex-1 flex-col">
      <div className="flex h-11 shrink-0 items-center px-4 sm:px-5">{header}</div>
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
        className="min-h-0 flex-1 overflow-y-auto"
      >
        <div
          className={cx(
            "sticky top-0 z-[1] hidden h-8 items-center gap-3 border-b border-surface-border bg-page-canvas/95 px-4 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground backdrop-blur md:grid sm:px-5",
            COLUMNS,
          )}
        >
          <span className="text-right">Seq</span>
          <span>Time</span>
          <span>Source</span>
          <span>Event</span>
          <span>Detail</span>
        </div>
        <ol className="pb-2">
          {events.map((event, index) => {
            const previous = events[index - 1];
            const newExecution =
              event.execution_id !== null &&
              event.execution_id !== (previous?.execution_id ?? null);
            const detail = pairs(event.detail);
            return (
              <Fragment key={event.event_id}>
                {newExecution && (
                  <li
                    aria-hidden="true"
                    className="flex items-center gap-3 px-4 pt-4 pb-1.5 sm:px-5"
                  >
                    <span className="rounded bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground">
                      {event.execution_id}
                    </span>
                    <span className="h-px flex-1 bg-surface-border" />
                  </li>
                )}
                <li
                  className={cx(
                    "grid grid-cols-[auto_minmax(0,1fr)] items-baseline gap-x-3 gap-y-0.5 border-b border-surface-border/60 px-4 py-2 transition-colors hover:bg-surface-hover sm:px-5 md:min-h-9 md:items-center md:py-1.5",
                    COLUMNS,
                  )}
                >
                  <span className="tnum hidden text-right font-mono text-micro text-faint-foreground md:block">
                    {event.session_seq}
                  </span>
                  <span className="tnum font-mono text-micro text-muted-foreground">
                    {clockTime(event.recorded_at_ms)}
                  </span>
                  <span className="hidden md:block">
                    <span className="rounded bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground">
                      {event.source}
                    </span>
                  </span>
                  <span className="flex min-w-0 items-center gap-2">
                    <span
                      aria-hidden="true"
                      className={cx(
                        "size-1.5 shrink-0 rounded-full",
                        eventTone(event.event_name),
                      )}
                    />
                    <span
                      className="truncate text-label text-foreground"
                      title={event.event_name}
                    >
                      {event.event_name}
                    </span>
                    <span className="font-mono text-micro text-faint-foreground md:hidden">
                      {event.source}
                    </span>
                  </span>
                  <span
                    className="col-span-2 min-w-0 truncate font-mono text-micro md:col-span-1"
                    title={fullText(event.detail)}
                  >
                    {detail.length === 0 ? (
                      <span className="text-faint-foreground">—</span>
                    ) : (
                      detail.map(([key, value], at) => (
                        <span key={`${key}-${at}`} className="mr-3">
                          {key && (
                            <span className="text-faint-foreground">{key}=</span>
                          )}
                          <span className="text-foreground/75">{value}</span>
                        </span>
                      ))
                    )}
                  </span>
                </li>
              </Fragment>
            );
          })}
        </ol>
      </div>

      {!following && (
        <button
          type="button"
          onClick={() => {
            const element = scrollRef.current;
            if (!element) return;
            stickToBottom.current = true;
            setFollowing(true);
            element.scrollTo({ top: element.scrollHeight, behavior: "smooth" });
          }}
          className="absolute bottom-4 left-1/2 inline-flex h-7 -translate-x-1/2 items-center gap-1.5 rounded-md border border-border-strong bg-surface-raised px-2.5 text-caption font-medium text-foreground shadow-[var(--floating-shadow)] hover:bg-surface-selected"
        >
          <ArrowDown size={12} />
          Jump to latest
        </button>
      )}
    </div>
  );
}

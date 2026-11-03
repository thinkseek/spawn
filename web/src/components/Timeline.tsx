import { ArrowDown } from "lucide-react";
import { Fragment, useLayoutEffect, useRef, useState } from "react";

import type { TimelineEvent } from "../api";
import { clockTime, formatDuration } from "../format";
import { EmptyState, cx } from "./ui";

const BOTTOM_SLACK_PX = 56;

const COLUMNS =
  "md:grid-cols-[3rem_4.5rem_4.5rem_minmax(9rem,13rem)_minmax(0,1fr)]";

/** Marker color by event family; neutral unless the event means something. */
function eventTone(name: string): string {
  if (/fail|error|lost|crash/i.test(name)) return "bg-destructive";
  if (name === "artifact.committed") return "bg-success";
  if (name.startsWith("execution.") || name.startsWith("session.")) {
    return "bg-info";
  }
  if (name.startsWith("notification.") || name.startsWith("screen.")) {
    return "bg-warning";
  }
  return "bg-faint-foreground/60";
}

function short(value: unknown): string {
  if (typeof value === "string") {
    if (/^[0-9a-f]{40}$/.test(value)) return value.slice(0, 7);
    // Opaque ids (toolu_01Ab…) only need their tail to be told apart.
    const id = /^([a-z]+_)([A-Za-z0-9]{16,})$/.exec(value);
    if (id) return `${id[1]}…${id[2].slice(-6)}`;
    return value;
  }
  if (value === null || value === undefined) return "null";
  if (typeof value === "object") {
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  }
  return String(value);
}

/** Flatten one level of nesting into key/value pairs. */
function pairs(detail: unknown): Array<[string, string]> {
  if (detail === null || typeof detail !== "object" || Array.isArray(detail)) {
    return detail == null ? [] : [["", short(detail)]];
  }
  const out: Array<[string, string]> = [];
  for (const [key, value] of Object.entries(detail as Record<string, unknown>)) {
    if (value === null) continue;
    if (key.endsWith("_ms") && typeof value === "number") {
      out.push([key.replace(/_ms$/, ""), formatDuration(value)]);
    } else if (value && typeof value === "object" && !Array.isArray(value)) {
      for (const [inner, innerValue] of Object.entries(
        value as Record<string, unknown>,
      )) {
        out.push([`${key}.${inner}`, short(innerValue)]);
      }
    } else {
      out.push([key, short(value)]);
    }
  }
  return out;
}

function fullText(detail: unknown): string {
  try {
    return typeof detail === "string"
      ? detail
      : JSON.stringify(detail, null, 2);
  } catch {
    return String(detail);
  }
}

export function Timeline({ events }: { events: TimelineEvent[] }) {
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

  if (events.length === 0) {
    return (
      <div className="grid min-h-0 flex-1 place-items-center">
        <EmptyState
          title="No events in this session"
          hint="Events appear here as the agent's hooks report them."
        />
      </div>
    );
  }

  return (
    <div className="relative flex min-h-0 flex-1 flex-col">
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

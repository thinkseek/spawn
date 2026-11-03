import { useLayoutEffect, useRef } from "react";
import type { TimelineEvent } from "../api";
import { clockTime } from "../format";

const BOTTOM_SLACK_PX = 56;

export function Timeline({ events }: { events: TimelineEvent[] }) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);
  const lastEventId = events.at(-1)?.event_id ?? null;

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element && stickToBottom.current) {
      element.scrollTop = element.scrollHeight;
    }
  }, [lastEventId, events.length]);

  return (
    <div
      ref={scrollRef}
      onScroll={() => {
        const element = scrollRef.current;
        if (!element) return;
        stickToBottom.current =
          element.scrollHeight - element.scrollTop - element.clientHeight <
          BOTTOM_SLACK_PX;
      }}
      className="min-h-0 flex-1 overflow-y-auto"
    >
      {events.length === 0 ? (
        <div className="grid h-full place-items-center px-6">
          <p className="text-caption text-faint-foreground">
            No events in this session.
          </p>
        </div>
      ) : (
        <ol className="py-1">
          {events.map((event) => {
            const detail = detailText(event.detail);
            return (
              <li
                key={event.event_id}
                className="grid grid-cols-[2.25rem_4.5rem_5rem_minmax(6rem,13rem)_minmax(0,1fr)] items-center gap-3 border-b border-surface-border px-4 py-1.5 transition-colors hover:bg-surface-hover"
              >
                <span className="text-right font-mono text-micro tabular-nums text-faint-foreground">
                  {event.session_seq}
                </span>
                <span className="font-mono text-micro tabular-nums text-muted-foreground">
                  {clockTime(event.recorded_at_ms)}
                </span>
                <span className="justify-self-start rounded-xs bg-muted px-1.5 py-0.5 font-mono text-micro text-muted-foreground">
                  {event.source}
                </span>
                <span
                  className="truncate text-label text-foreground"
                  title={event.event_name}
                >
                  {event.event_name}
                </span>
                <span
                  className="truncate font-mono text-micro text-muted-foreground"
                  title={detail}
                >
                  {detail}
                </span>
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}

function detailText(detail: unknown): string {
  if (detail == null) return "null";
  try {
    return typeof detail === "string" ? detail : JSON.stringify(detail);
  } catch {
    return String(detail);
  }
}

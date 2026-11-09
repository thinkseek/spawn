import { AlertTriangle, Crosshair, X } from "lucide-react";
import { motion } from "motion/react";
import { useEffect, useRef } from "react";
import type { ReactNode } from "react";

import type { TimelineEvent } from "../../api";
import { eventTone, fullText, pairs } from "../../eventFormat";
import { clockTime, formatDuration } from "../../format";
import { rowText, runEnd, spanEnd, spanEvents } from "../../trace";
import type { Span, Trace, TraceMarker } from "../../trace";
import { cx } from "../ui";
import { KIND_COLOR, KIND_LABEL } from "./style";

/** The selected span: its timing, its place in the chain, and every event
 * it owns with the same fields the event list shows. */
export function SpanDetail({
  row,
  trace,
  now,
  selectedEvent,
  reduceMotion,
  onClose,
  onSelect,
  onZoom,
}: {
  row: { key: string; span: Span | null; run: Span[]; unfolded: boolean; markers: TraceMarker[] };
  trace: Trace;
  now: number;
  selectedEvent: number | null;
  reduceMotion: boolean;
  onClose: () => void;
  onSelect: (id: string) => void;
  onZoom: (span: Span) => void;
}) {
  const span = row.span;
  // A folded run of identical calls owns every call's events.
  const run = row.run.length > 1 && !row.unfolded ? row.run : null;
  const events: TimelineEvent[] = run
    ? run.flatMap(spanEvents)
    : span
      ? spanEvents(span)
      : row.markers.map((marker) => marker.event);

  const listRef = useRef<HTMLOListElement>(null);
  useEffect(() => {
    if (selectedEvent === null) return;
    listRef.current
      ?.querySelector(`[data-event="${selectedEvent}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [selectedEvent, row.key]);

  const ancestors: Span[] = [];
  for (let id = span?.parentId; id; id = trace.byId.get(id)?.parentId) {
    const parent = trace.byId.get(id);
    if (parent) ancestors.unshift(parent);
  }

  return (
    <motion.aside
      aria-label="Span detail"
      initial={reduceMotion ? { opacity: 0 } : { opacity: 0, x: 16 }}
      animate={{ opacity: 1, x: 0 }}
      exit={reduceMotion ? { opacity: 0 } : { opacity: 0, x: 16 }}
      transition={{ duration: 0.16, ease: [0.2, 0.8, 0.2, 1] }}
      className="absolute inset-y-0 right-0 z-10 flex w-full max-w-[24rem] flex-col border-l border-surface-border bg-surface shadow-[var(--floating-shadow)] lg:static lg:shadow-none"
    >
      <header className="flex shrink-0 items-start gap-3 px-4 pt-3.5 pb-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
            {span && (
              <span
                aria-hidden="true"
                className="size-2 rounded-full"
                style={{ background: KIND_COLOR[span.kind] }}
              />
            )}
            {span ? KIND_LABEL[span.kind] : "Session"}
          </div>
          <h3 className="mt-1 truncate text-title-sm font-semibold text-foreground">
            {span ? (run && span.kind === "command" ? span.label : rowText(span, trace)) : "Session events"}
            {run && <span className="tnum font-normal text-muted-foreground"> ×{run.length}</span>}
          </h3>
          {ancestors.length > 0 && (
            <nav aria-label="Parents" className="mt-1 flex flex-wrap items-center gap-x-1 text-caption text-muted-foreground">
              {ancestors.map((parent, index) => (
                <span key={parent.id} className="flex items-center gap-1">
                  {index > 0 && <span className="text-faint-foreground">›</span>}
                  <button
                    type="button"
                    onClick={() => onSelect(parent.id)}
                    className="rounded hover:text-foreground hover:underline"
                  >
                    {parent.label}
                  </button>
                </span>
              ))}
            </nav>
          )}
        </div>
        {span && (
          <button
            type="button"
            onClick={() => onZoom(span)}
            title="Zoom the axis to this span (Enter)"
            aria-label="Zoom to span"
            className="grid size-7 shrink-0 place-items-center rounded-md text-muted-foreground hover:bg-surface-hover hover:text-foreground"
          >
            <Crosshair size={14} />
          </button>
        )}
        <button
          type="button"
          onClick={onClose}
          aria-label="Close detail"
          title="Close (Esc)"
          className="grid size-7 shrink-0 place-items-center rounded-md text-muted-foreground hover:bg-surface-hover hover:text-foreground"
        >
          <X size={15} />
        </button>
      </header>

      {span && (
        <dl className="grid shrink-0 grid-cols-[6rem_minmax(0,1fr)] gap-x-3 gap-y-1.5 px-4 pb-4 text-label">
          <Field label="Started">
            {span.state === "orphan" ? (
              <span className="text-faint-foreground">not in the log</span>
            ) : (
              clockTime(span.start)
            )}
          </Field>
          <Field label="Ended">
            {span.end === null ? (
              <span className="inline-flex items-center gap-1.5 text-foreground">
                <span className="size-1.5 animate-spawn-pulse rounded-full bg-success" />
                running
              </span>
            ) : (
              clockTime(run ? runEnd(run, now) : span.end)
            )}
          </Field>
          {span.state !== "orphan" && (
            <Field label="Duration">
              {formatDuration((run ? runEnd(run, now) : spanEnd(span, now)) - span.start)}
              {run && <span className="text-muted-foreground"> · first start to last end</span>}
            </Field>
          )}
          {span.summary && (
            <Field label="Input">
              <span className="font-mono text-caption break-all select-text">{span.summary}</span>
            </Field>
          )}
          {run && (
            <Field label="Calls">
              <ol className="tnum space-y-0.5">
                {run.map((call) => (
                  <li key={call.id} className="text-caption">
                    {clockTime(call.start)}
                    <span className="text-muted-foreground">
                      {" · "}
                      {call.state === "orphan" ? "no start" : formatDuration(spanEnd(call, now) - call.start)}
                    </span>
                    {/* A command run's texts differ: each says its own. */}
                    {call.kind === "command" && call.summary && (
                      <span className="block break-words text-foreground">{call.summary}</span>
                    )}
                  </li>
                ))}
              </ol>
            </Field>
          )}
          {span.outcome && (
            <Field label={span.failed ? "Failed" : "Outcome"}>
              <span className="inline-flex min-w-0 items-start gap-1.5">
                {span.failed && (
                  <AlertTriangle size={13} className="mt-0.5 shrink-0 text-destructive" />
                )}
                <span className="break-words">{span.outcome}</span>
              </span>
            </Field>
          )}
          {span.executionId && (
            <Field label="Execution">
              <span className="font-mono text-caption">{span.executionId}</span>
            </Field>
          )}
          {span.childIds.length > 0 && (
            <Field label="Children">{span.childIds.length}</Field>
          )}
        </dl>
      )}

      <div className="flex shrink-0 items-center gap-2 border-t border-surface-border px-4 pt-3 pb-1.5 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        Events
        <span className="tnum font-normal normal-case tracking-normal">{events.length}</span>
      </div>
      <ol ref={listRef} className="min-h-0 flex-1 overflow-y-auto px-2 pb-4">
        {events.map((event) => {
          const fields = pairs(event.detail);
          return (
            <li
              key={event.event_id}
              data-event={event.event_id}
              className={cx(
                "rounded-md px-2 py-2",
                event.event_id === selectedEvent && "bg-surface-selected",
              )}
            >
              <div className="flex items-center gap-2">
                <span
                  aria-hidden="true"
                  className={cx("size-1.5 shrink-0 rounded-full", eventTone(event.event_name))}
                />
                <span className="min-w-0 flex-1 truncate text-label text-foreground">
                  {event.event_name}
                </span>
                <span className="rounded bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground">
                  {event.source}
                </span>
              </div>
              <div className="tnum mt-0.5 pl-3.5 font-mono text-micro text-faint-foreground">
                #{event.session_seq} · {clockTime(event.recorded_at_ms)}
              </div>
              {fields.length > 0 && (
                <div
                  className="mt-1 pl-3.5 font-mono text-micro leading-relaxed break-all select-text"
                  title={fullText(event.detail)}
                >
                  {fields.map(([key, value], index) => (
                    <div key={`${key}-${index}`}>
                      {key && <span className="text-faint-foreground">{key}=</span>}
                      <span className="text-foreground/80">{value}</span>
                    </div>
                  ))}
                </div>
              )}
            </li>
          );
        })}
      </ol>
    </motion.aside>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="tnum min-w-0 text-foreground">{children}</dd>
    </>
  );
}

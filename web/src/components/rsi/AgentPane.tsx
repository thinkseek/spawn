import { MonitorPlay } from "lucide-react";
import { useLayoutEffect, useRef } from "react";

import { usePoll } from "../../api";
import { relativeTime } from "../../format";
import { fetchPane } from "../../rsi";
import { useNow } from "./Phase";

/**
 * Tail of the agent's tmux pane. Mounted only while the RSI view is
 * visible, so the 1 s poll stops with it.
 */
export function AgentPane() {
  const pane = usePoll("rsi-pane", fetchPane, 1_000);
  useNow(1_000);
  const scroller = useRef<HTMLPreElement>(null);
  const pinned = useRef(true);
  const text = pane.data?.text ?? "";

  // Follow the bottom unless the reader scrolled up to read history.
  useLayoutEffect(() => {
    const element = scroller.current;
    if (element && pinned.current) element.scrollTop = element.scrollHeight;
  }, [text]);

  const live = pane.data?.live === true;

  return (
    <section className="flex min-h-0 flex-1 flex-col rounded-xl border border-surface-border bg-surface">
      <header className="flex items-center gap-2 border-b border-surface-border px-3.5 py-2.5">
        <MonitorPlay size={12} className="text-faint-foreground" />
        <span className="text-micro font-medium uppercase tracking-[0.12em] text-faint-foreground">
          Agent screen
        </span>
        {pane.data?.terminal && (
          <span className="truncate font-mono text-micro text-muted-foreground">
            {pane.data.terminal}
          </span>
        )}
        <span className="ml-auto flex items-center gap-2">
          <span className="text-micro text-faint-foreground">
            {pane.data?.captured_at_ms
              ? `captured ${relativeTime(pane.data.captured_at_ms)}`
              : ""}
          </span>
          <span
            className={`inline-flex items-center gap-1.5 rounded-sm border border-surface-border bg-page-canvas px-1.5 py-0.5 text-micro font-medium ${
              live ? "text-success" : "text-muted-foreground"
            }`}
          >
            <span
              aria-hidden="true"
              className={`inline-block size-1.5 rounded-full ${
                live ? "animate-spawn-pulse bg-success" : "bg-muted-foreground"
              }`}
            />
            {pane.data ? (live ? "live" : "ended") : "…"}
          </span>
        </span>
      </header>
      <pre
        ref={scroller}
        onScroll={(event) => {
          const element = event.currentTarget;
          pinned.current =
            element.scrollHeight - element.scrollTop - element.clientHeight <
            24;
        }}
        className="min-h-64 flex-1 overflow-auto whitespace-pre-wrap break-words bg-page-canvas px-3.5 py-3 font-mono text-caption text-foreground"
      >
        {text ||
          (pane.error
            ? `Could not read the pane: ${pane.error}`
            : pane.data
              ? "No agent terminal is running."
              : "Connecting…")}
      </pre>
    </section>
  );
}

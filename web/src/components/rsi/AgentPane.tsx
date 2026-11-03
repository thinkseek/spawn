import { usePoll } from "../../api";
import { relativeTime } from "../../format";
import { fetchPane } from "../../rsi";
import { LiveBadge, TerminalPanel } from "../Terminal";
import { useNow } from "./Phase";

/**
 * Tail of the RSI agent's tmux pane. Mounted only while the RSI view is
 * visible, so the 1 s poll stops with it.
 */
export function AgentPane({ className }: { className?: string }) {
  const pane = usePoll("rsi-pane", fetchPane, 1_000);
  useNow(1_000);
  const data = pane.data;

  return (
    <TerminalPanel
      className={className}
      title="Agent terminal"
      tag={data?.terminal}
      text={data?.text ?? ""}
      placeholder={
        pane.error
          ? `Could not read the pane: ${pane.error}`
          : data
            ? "No agent terminal is running."
            : "Connecting…"
      }
      status={
        <>
          {data?.captured_at_ms ? (
            <span className="tnum hidden text-caption text-faint-foreground sm:inline">
              {relativeTime(data.captured_at_ms)}
            </span>
          ) : null}
          <LiveBadge live={data?.live === true} known={data !== null} />
        </>
      }
    />
  );
}

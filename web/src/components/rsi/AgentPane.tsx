import { usePoll } from "../../api";
import { relativeTime } from "../../format";
import { fetchPane } from "../../rsi";
import { LiveBadge, TerminalPanel } from "../Terminal";
import { useNow } from "./Phase";

/**
 * Tail of the objective mission's resident's tmux pane. Mounted only while
 * the RSI view is visible, so the 1 s poll stops with it.
 */
export function AgentPane({
  mission,
  resident,
  className,
}: {
  mission: number | null;
  resident: string | null;
  className?: string;
}) {
  const pane = usePoll(
    `rsi-pane-${mission ?? "none"}`,
    (signal) => fetchPane(mission, signal),
    1_000,
    { checked: true },
  );
  useNow(1_000);
  const data = pane.data;

  return (
    <TerminalPanel
      className={className}
      title={resident ? `Resident ${resident}` : "Resident terminal"}
      tag={data?.terminal}
      text={data?.text ?? ""}
      placeholder={
        pane.error
          ? `Could not read the pane: ${pane.error}`
          : data
            ? "The resident's terminal is not running."
            : "Connecting…"
      }
      status={
        <>
          {data?.captured_at_ms ? (
            <span className="tnum hidden text-caption text-faint-foreground sm:inline">
              {relativeTime(freshAt(data.captured_at_ms, pane.checkedAt))}
            </span>
          ) : null}
          <LiveBadge live={data?.live === true} known={data !== null} />
        </>
      }
    />
  );
}

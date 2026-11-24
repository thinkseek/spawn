import { readApi, usePoll } from "../../api";
import type { NodeRow } from "../../nodes";
import { coresText, gibText, normalizeNodes } from "../../nodes";
import { absoluteTitle, relativeTime } from "../../format";
import { cx } from "../ui";

const STATE_TONE: Record<NodeRow["state"], string> = {
  live: "text-success",
  lost: "text-destructive",
  drained: "text-warning",
};

const fetchNodes = (signal: AbortSignal) => readApi("/api/nodes", signal, normalizeNodes);

/**
 * The nodes Spawn may use (multi-host P1, `/api/nodes`): one compact row
 * per node. It polls only while shown (the Fleet details open). Nothing
 * shows while no node is joined; a read that failed says so, beside the
 * last rows read.
 */
export function NodeRows() {
  const { data, error } = usePoll("nodes", fetchNodes, 30_000);
  if (!error && !data?.length) return null;
  return (
    <section aria-label="Nodes" className="rounded-card bg-surface px-4 py-3 shadow-card">
      <div className="mb-1.5 flex flex-wrap items-baseline gap-x-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        Nodes
        {error && (
          <span role="status" className="normal-case tracking-normal text-destructive">
            {data ? `not updated: ${error}` : `could not be read: ${error}`}
          </span>
        )}
      </div>
      <ul className="flex flex-col gap-1">
        {(data ?? []).map((node) => (
          <li
            key={node.name}
            className="tnum flex flex-wrap items-baseline gap-x-4 gap-y-0.5 text-caption text-muted-foreground"
          >
            <span className="min-w-[6rem] truncate font-medium text-foreground">{node.name}</span>
            <span
              className={cx("font-medium", STATE_TONE[node.state])}
              title={absoluteTitle(node.lost_since_ms)}
            >
              {node.state}
              {node.lost_since_ms !== null && ` ${relativeTime(node.lost_since_ms)}`}
            </span>
            <span>{node.arch ?? "—"}</span>
            <span title="cores (the cgroup's limit)">{coresText(node)} cores</span>
            <span title="memory">{gibText(node.mem_bytes)} mem</span>
            <span title="free disk of the node user's home">{gibText(node.disk_free_bytes)} free</span>
            <span className="truncate">{node.gpu ?? "no GPU"}</span>
            <span title={absoluteTitle(node.sampled_ms)}>
              {node.sampled_ms === null ? "not sampled yet" : `sampled ${relativeTime(node.sampled_ms)}`}
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

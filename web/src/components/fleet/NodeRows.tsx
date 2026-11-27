import { readApi, usePoll } from "../../api";
import type { NodeRow } from "../../nodes";
import {
  buildShaBadge,
  coresText,
  gibText,
  mergeNodeRows,
  nodeVersionText,
  normalizeNodes,
  powerText,
  powerTone,
  reachText,
  viaText,
} from "../../nodes";
import { absoluteTitle, relativeTime } from "../../format";
import { cx } from "../ui";

const STATE_TONE: Record<NodeRow["state"], string> = {
  live: "text-success",
  lost: "text-destructive",
  drained: "text-warning",
};

const POWER_TONE = {
  ok: "text-muted-foreground",
  draining: "text-warning",
  low: "text-destructive",
} as const;

const fetchNodes = (signal: AbortSignal) => readApi("/api/nodes", signal, normalizeNodes);

/**
 * The nodes Spawn may use (multi-host P1, `/api/nodes`): one compact row
 * per node, with its power (AC or battery, its charge, charging or not)
 * and the low-battery soft drain when it holds (issue #91). It polls only
 * while shown (the Fleet details open), and reads again on a node event of
 * the live stream. Nothing
 * shows while no node is joined; a read that failed says so, beside the
 * last rows read.
 */
export function NodeRows({ fleetNodes }: { fleetNodes?: readonly NodeRow[] } = {}) {
  const { data, error } = usePoll("nodes", fetchNodes, 30_000, { live: ["nodes"] });
  const rows = mergeNodeRows(data, fleetNodes);
  if (!error && rows.length === 0) return null;
  return (
    <section aria-label="Nodes" className="rounded-card bg-surface px-4 py-3 shadow-card">
      <div className="mb-1.5 flex flex-wrap items-baseline gap-x-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        Nodes
        {error && (
          <span role="status" className="normal-case tracking-normal text-destructive">
            {rows.length > 0 ? `not updated: ${error}` : `could not be read: ${error}`}
          </span>
        )}
      </div>
      <ul className="flex flex-col gap-1.5">
        {rows.map((node) => {
          const sha = buildShaBadge(node);
          const version = nodeVersionText(node);
          return (
            <li
              key={node.name}
              className="tnum flex flex-wrap items-center gap-x-3.5 gap-y-1 rounded-control bg-inset px-3 py-2 text-caption text-muted-foreground shadow-hairline transition-colors hover:bg-surface-hover"
            >
              <span className="min-w-[6rem] truncate font-medium text-foreground">{node.name}</span>
              <span
                className={cx("font-medium", STATE_TONE[node.state])}
                title={absoluteTitle(node.lost_since_ms)}
              >
                {node.state}
                {node.lost_since_ms !== null && ` ${relativeTime(node.lost_since_ms)}`}
              </span>
              <span
                className="rounded-chip bg-surface px-1.5 py-px text-micro font-medium uppercase tracking-[0.04em] shadow-hairline"
                title="how the primary reaches it (transport/mode)"
              >
                {reachText(node)}
              </span>
              {(version || sha) && (
                <span className="inline-flex items-baseline gap-1">
                  {version && (
                    <span
                      className="font-mono text-micro text-muted-foreground"
                      title={`node version ${node.node_version}`}
                    >
                      {version}
                    </span>
                  )}
                  {sha && (
                    <span
                      className="rounded-chip bg-surface px-1.5 py-px font-mono text-micro text-foreground shadow-hairline"
                      title={sha.full}
                    >
                      {sha.short}
                    </span>
                  )}
                </span>
              )}
              {viaText(node) && (
                <span className="truncate" title="the hops from the primary to the node, in order">
                  {viaText(node)}
                </span>
              )}
              <span>{node.arch ?? "—"}</span>
              <span title="cores (the cgroup's limit)">{coresText(node)} cores</span>
              <span title="memory">{gibText(node.mem_bytes)} mem</span>
              <span title="free disk of the node user's home">{gibText(node.disk_free_bytes)} free</span>
              <span className="truncate">{node.gpu ?? "no GPU"}</span>
              {node.power && (
                <span
                  className={cx("font-medium", POWER_TONE[powerTone(node) ?? "ok"])}
                  title="power: AC or battery, its charge, charging or not, and the time left"
                >
                  {powerText(node)}
                </span>
              )}
              {node.low_battery_drain && (
                <span
                  className="rounded-sm bg-fail-tint px-1 text-micro font-medium uppercase tracking-[0.04em] text-destructive"
                  title="on battery at or under the low-battery threshold: no work is placed on it until it is plugged in (its state and the owner's drain are untouched)"
                >
                  low-battery drain
                </span>
              )}
              {node.schedulable !== null && (
                <span
                  className={cx(node.schedulable ? "text-success" : "text-warning")}
                  title={
                    node.effective_drained
                      ? "drained (by the owner, or the low-battery soft drain)"
                      : "whether work may be placed on it"
                  }
                >
                  {node.schedulable ? "schedulable" : "not schedulable"}
                </span>
              )}
              <span title={absoluteTitle(node.sampled_ms)}>
                {node.sampled_ms === null ? "not sampled yet" : `sampled ${relativeTime(node.sampled_ms)}`}
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

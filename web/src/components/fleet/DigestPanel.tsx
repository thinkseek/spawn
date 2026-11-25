import { FileText } from "lucide-react";
import { useCallback, useState } from "react";

import { fetchDigest, usePoll, useLiveStatus } from "../../api";
import type { Digest } from "../../digest";
import { digestBlocks, digestSummary } from "../../digest";
import { relativeTime } from "../../format";
import { Drawer, cx } from "../ui";

/** The live stream's state in a few words, for the panel's header. */
function LiveBadge() {
  const status = useLiveStatus();
  const [label, tone] =
    status.state === "live"
      ? [status.leader ? "Live" : "Live (shared)", "text-success"]
      : status.state === "connecting"
        ? ["Connecting", "text-muted-foreground"]
        : status.state === "unauthorized"
          ? ["Live: token needed", "text-destructive"]
          : status.state === "retrying"
            ? [
                `Reconnecting${status.retry_at_ms ? ` in ${Math.max(0, Math.ceil((status.retry_at_ms - Date.now()) / 1000))}s` : ""}`,
                "text-warning",
              ]
            : ["Polling", "text-muted-foreground"];
  return (
    <span className={cx("text-caption font-medium", tone)} title={status.error ?? "the live log (/api/stream)"}>
      {label}
    </span>
  );
}

function DigestBody({ digest }: { digest: Digest }) {
  return (
    <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3 text-caption sm:px-5">
      <div className="mb-3 flex flex-wrap gap-x-3 gap-y-1 text-muted-foreground">
        {digest.fleet.states.map(([state, count]) => (
          <span key={state} className="tnum">
            <span className="font-medium text-foreground">{count}</span> {state.toLowerCase()}
          </span>
        ))}
        <span className="tnum">
          <span className="font-medium text-foreground">{digest.approvals.length}</span> to answer
        </span>
      </div>
      <div className="space-y-2 text-foreground">
        {digestBlocks(digest.digest_text).map((block, at) =>
          block.kind === "heading" ? (
            <p
              key={at}
              className={cx(
                "font-semibold",
                block.level <= 2 ? "text-label" : "pt-1 text-micro uppercase tracking-[0.06em] text-faint-foreground",
              )}
            >
              {block.text}
            </p>
          ) : block.kind === "bullets" ? (
            <ul key={at} className="list-disc space-y-0.5 pl-4">
              {block.items.map((item, index) => (
                <li key={index} className="break-words">
                  {item}
                </li>
              ))}
            </ul>
          ) : (
            <p key={at} className="break-words">
              {block.text}
            </p>
          ),
        )}
      </div>
      {digest.warnings.length > 0 && (
        <p className="mt-3 text-faint-foreground">Not read: {digest.warnings.join("; ")}</p>
      )}
    </div>
  );
}

/**
 * The fleet digest (issue #91 P1b, `GET /api/context/digest`): one line on
 * the home page (sessions, live ones, prompts to answer, halted missions)
 * with the live stream's state; a click opens the whole digest (its states
 * and the bounded Markdown, shown as text) in a drawer. Read again on any
 * live event, slowly polled besides.
 */
export function DigestPanel() {
  const [open, setOpen] = useState(false);
  const { data, error } = usePoll("digest", fetchDigest, 30_000, { live: ["digest"] });
  const close = useCallback(() => setOpen(false), []);
  return (
    <>
      <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 rounded-card bg-surface px-4 py-2 shadow-card">
        <LiveBadge />
        <span className="min-w-0 truncate text-caption text-muted-foreground">
          {data ? digestSummary(data) : error ? `Digest could not be read: ${error}` : "Reading the digest…"}
        </span>
        {data && (
          <button
            type="button"
            onClick={() => setOpen(true)}
            className="ml-auto inline-flex items-center gap-1 rounded-control px-2 py-0.5 text-caption font-medium text-muted-foreground hover:bg-surface-hover hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring"
          >
            <FileText size={12} aria-hidden="true" />
            Digest
          </button>
        )}
      </div>
      {open && data && (
        <Drawer
          onClose={close}
          width="max-w-xl"
          header={
            <span className="truncate text-body font-semibold text-foreground">
              Fleet digest
              {data.generated_at_ms !== null && (
                <span className="ml-2 text-caption font-normal text-faint-foreground">
                  {relativeTime(data.generated_at_ms)}
                </span>
              )}
            </span>
          }
        >
          <DigestBody digest={data} />
        </Drawer>
      )}
    </>
  );
}

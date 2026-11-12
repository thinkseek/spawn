import { Activity } from "lucide-react";

import { readApi, usePoll } from "../../api";
import type { CoreLinesAnswer, CoreMark } from "../../coreLines.ts";
import { coreLinesView } from "../../coreLines.ts";
import { Card, CardHeader, cx } from "../ui";

const POLL_MS = 60_000;

const TONE: Record<CoreMark["tone"], string> = {
  plain: "text-foreground",
  warning: "text-warning",
  muted: "text-faint-foreground",
};

function fetchCoreLines(signal: AbortSignal): Promise<CoreLinesAnswer> {
  return readApi("/api/core-lines", signal, (raw) => raw as CoreLinesAnswer);
}

/** A guardrail: its name and value, coloured out of range, its range under. */
function Guardrail({ mark }: { mark: CoreMark }) {
  return (
    <li
      className={cx(
        "min-w-0 rounded-md border px-2 py-1",
        mark.tone === "warning" ? "border-warning/40 bg-wait-tint" : "border-surface-border",
      )}
      title={`${mark.name}: ${mark.value} (${mark.range})`}
    >
      <div className="truncate text-micro text-muted-foreground">{mark.name}</div>
      <div className={cx("tnum truncate text-caption font-medium", TONE[mark.tone])}>{mark.value}</div>
      <div className="truncate text-micro text-faint-foreground">{mark.range}</div>
    </li>
  );
}

/**
 * The Core lines on Home (objective 084): each core line's north star with
 * its value and range, then its guardrails as marks, plain when healthy and
 * coloured when out of range (`/api/core-lines`, coreLines.ts). A ranges
 * file that cannot be read, and the ranges it gave that were left out, are
 * said here, so a broken file shows without reading logs. Read-only.
 */
export function CoreLinesCard() {
  const poll = usePoll("core-lines", fetchCoreLines, POLL_MS);
  const view = poll.data ? coreLinesView(poll.data) : null;
  return (
    <Card>
      <CardHeader icon={<Activity size={14} />} title="Core lines" />
      {!view ? (
        <p className="px-4 py-2.5 text-caption text-faint-foreground" role="status">
          {poll.error ? `Could not read the core lines: ${poll.error}` : "Loading…"}
        </p>
      ) : (
        <>
          {(view.notice || view.ignored.length > 0) && (
            <div className="space-y-0.5 border-b border-surface-border px-4 py-2 text-caption text-warning" role="status">
              {view.notice && <p className="break-words">{view.notice}</p>}
              {view.ignored.map((line) => (
                <p key={line} className="break-words">
                  range ignored: {line}
                </p>
              ))}
            </div>
          )}
          <ul className="divide-y divide-surface-border">
            {view.rows.map((row) => (
              <li key={row.line} className="min-w-0 px-4 py-2.5">
                <div className="flex min-w-0 flex-wrap items-baseline gap-x-2">
                  <span className="text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
                    {row.line}
                  </span>
                  <span className="min-w-0 truncate text-caption text-muted-foreground" title={row.north_star.name}>
                    {row.north_star.name}
                  </span>
                </div>
                <div className="mt-0.5 flex min-w-0 flex-wrap items-baseline gap-x-2">
                  <span className={cx("tnum text-title-sm font-semibold", TONE[row.north_star.tone])}>
                    {row.north_star.value}
                  </span>
                  <span className="text-micro text-faint-foreground">{row.north_star.range}</span>
                </div>
                {row.guardrails.length > 0 && (
                  <ul className="mt-2 grid grid-cols-1 gap-1.5 sm:grid-cols-2 lg:grid-cols-3">
                    {row.guardrails.map((mark) => (
                      <Guardrail key={mark.name} mark={mark} />
                    ))}
                  </ul>
                )}
              </li>
            ))}
          </ul>
        </>
      )}
    </Card>
  );
}

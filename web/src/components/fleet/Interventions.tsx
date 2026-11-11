import { useState } from "react";
import { ChevronRight, Hand } from "lucide-react";

import { readApi, usePoll } from "../../api";
import type { FigureState, InterventionsAnswer } from "../../interventions.ts";
import { figureState } from "../../interventions.ts";
import { Card, CardHeader, FOCUS_RING_INSET, cx } from "../ui";

const POLL_MS = 60_000;

/** The windows Home shows, side by side. */
const WINDOWS = [
  { id: "24h", label: "Last 24 h" },
  { id: "7d", label: "Last 7 days" },
] as const;

const HINT =
  "Human interventions per agent-hour: each time a person stepped in (a command, a resume, a task they assigned, a cancel and reassign), divided by the hours agents ran. A command refused and sent again is one intervention; its retries are not counted. Lower is better.";

function fetchInterventions(window: string, signal: AbortSignal): Promise<InterventionsAnswer> {
  return readApi(`/api/interventions?window=${window}`, signal, (raw) => raw as InterventionsAnswer);
}

const OUTCOME_TONE: Record<string, string> = {
  taken: "text-success",
  failed: "text-warning",
  pending: "text-muted-foreground",
};

function Figure({ label, state }: { label: string; state: FigureState }) {
  const [open, setOpen] = useState(false);
  if (state.kind !== "view") {
    return (
      <div className="min-w-0 px-4 py-3">
        <div className="text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">{label}</div>
        {state.kind === "failed" ? (
          <p className="mt-1 break-words text-caption text-warning" role="status">
            Could not read the interventions: {state.reason}
          </p>
        ) : (
          <p className="mt-1 text-caption text-faint-foreground">Loading…</p>
        )}
      </div>
    );
  }
  const { view, stale } = state;
  return (
    <div className="min-w-0 px-4 py-3">
      <div className="text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">{label}</div>
      <div className="mt-1 flex flex-wrap items-baseline gap-x-2 gap-y-0.5" title={HINT}>
        <span className="tnum text-display-sm leading-none font-semibold tracking-tight text-foreground">
          {view.figure}
        </span>
        <span className="text-caption text-muted-foreground">per agent-hour</span>
      </div>
      <div className="mt-1 text-caption text-muted-foreground">
        <span className="tnum">{view.count}</span>
        {view.retries && <span className="tnum text-faint-foreground"> · {view.retries}</span>}
      </div>
      {stale && (
        <p className="mt-1 break-words text-caption text-warning" role="status">
          The last read failed: {stale}
        </p>
      )}
      {view.rows.length > 0 && (
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          aria-expanded={open}
          className={cx(
            "mt-1.5 flex items-center gap-1 rounded-control px-1 py-0.5 text-caption text-muted-foreground hover:bg-surface-hover hover:text-foreground",
            FOCUS_RING_INSET,
          )}
        >
          <ChevronRight
            size={12}
            aria-hidden="true"
            className={cx("transition-transform motion-reduce:transition-none", open && "rotate-90")}
          />
          Which interventions
        </button>
      )}
      {open && (
        <ul className="mt-1 space-y-0.5">
          {view.rows.map((row, index) => (
            <li key={`${row.kind}:${row.commands}:${index}`} className="flex min-w-0 items-baseline gap-2 text-caption">
              <span className="shrink-0 font-mono text-faint-foreground">{row.kind}</span>
              {row.who && (
                <span className="max-w-[12rem] shrink-0 truncate font-mono text-muted-foreground" title={row.who}>
                  {row.who}
                </span>
              )}
              <span className="min-w-0 truncate text-foreground" title={row.line}>
                {row.line}
              </span>
              {row.commands && (
                <span className="tnum shrink-0 text-faint-foreground" title="The commands of this intervention">
                  {row.commands}
                </span>
              )}
              {row.outcome && (
                <span className={cx("ml-auto shrink-0", OUTCOME_TONE[row.outcome] ?? "text-muted-foreground")}>
                  {row.outcome}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/**
 * The north star on Home: human interventions per agent-hour over the last
 * 24 hours and 7 days, how many interventions and the retries they leave
 * out, and the list of interventions behind each figure
 * (`/api/interventions`, interventions.ts). Read-only.
 */
export function InterventionsCard() {
  const day = usePoll("interventions-24h", (signal) => fetchInterventions("24h", signal), POLL_MS);
  const week = usePoll("interventions-7d", (signal) => fetchInterventions("7d", signal), POLL_MS);
  const polls = { "24h": day, "7d": week } as const;
  return (
    <Card>
      <CardHeader icon={<Hand size={14} />} title="Human interventions" />
      <div className="grid grid-cols-1 sm:grid-cols-2">
        {WINDOWS.map((window) => {
          const poll = polls[window.id];
          return <Figure key={window.id} label={window.label} state={figureState(poll.data, poll.error)} />;
        })}
      </div>
    </Card>
  );
}

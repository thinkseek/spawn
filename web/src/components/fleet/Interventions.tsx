import { useState } from "react";
import { ChevronRight, Hand } from "lucide-react";

import { readApi } from "../../api";
import type { FigureState, InterventionsAnswer } from "../../interventions.ts";
import { HomeCard, LoadingLines } from "../aui/HomeCard";
import { Tooltip } from "../aui/kit";
import { FOCUS_RING_INSET, cx } from "../ui";

/** How often Home reads each window. */
export const INTERVENTIONS_POLL_MS = 60_000;

/** The windows Home shows, side by side. */
const WINDOWS = [
  { id: "24h", label: "Last 24 h" },
  { id: "7d", label: "Last 7 days" },
] as const;

const HINT =
  "Human interventions per agent-hour: each time a person stepped in (a command, a pause, a resume, a task they assigned, a cancel and reassign), divided by the hours agents ran. A command refused and sent again is one intervention; its retries are not counted. Lower is better.";

export function fetchInterventions(window: string, signal: AbortSignal): Promise<InterventionsAnswer> {
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
      <div className="min-w-0">
        <div className="text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">{label}</div>
        {state.kind === "failed" ? (
          <p className="mt-1 break-words text-caption text-warning" role="status">
            Could not read the interventions: {state.reason}
          </p>
        ) : (
          <div className="mt-1">
            <LoadingLines label={`Loading ${label.toLowerCase()}`} lines={2} />
          </div>
        )}
      </div>
    );
  }
  const { view, stale } = state;
  return (
    <div className="min-w-0">
      <div className="text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">{label}</div>
      <Tooltip content={HINT}>
        <div className="mt-1 flex flex-wrap items-baseline gap-x-2 gap-y-0.5" tabIndex={0}>
          <span className="tnum text-display-sm leading-none font-semibold tracking-tight text-foreground">
            {view.figure}
          </span>
          <span className="text-caption text-muted-foreground">per agent-hour</span>
        </div>
      </Tooltip>
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
 * (`/api/interventions`, interventions.ts). Read-only, from its props: Home
 * polls both windows ([`fetchInterventions`]) and passes each figureState.
 */
export function InterventionsCard({ day, week }: { day: FigureState; week: FigureState }) {
  const states = { "24h": day, "7d": week } as const;
  return (
    <HomeCard icon={<Hand size={14} />} title="Human interventions">
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        {WINDOWS.map((window) => (
          <Figure key={window.id} label={window.label} state={states[window.id]} />
        ))}
      </div>
    </HomeCard>
  );
}

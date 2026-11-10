import { CircleDollarSign } from "lucide-react";

import { readApi, usePoll } from "../../api";
import type { CostAnswer, CostView as Figures, Row } from "../../cost.ts";
import { costView } from "../../cost.ts";
import { Card, CardHeader, EmptyState, LiveStatus, PageHeader, cx } from "../ui";

const POLL_MS = 30_000;

/** The windows the page shows, side by side. */
const WINDOWS = [
  { id: "24h", label: "Last 24 h" },
  { id: "7d", label: "Last 7 days" },
] as const;

function fetchCost(window: string, signal: AbortSignal): Promise<CostAnswer> {
  return readApi(`/api/cost?window=${window}`, signal, (raw) => raw as CostAnswer);
}

/** The mark of a figure that leaves out use on a model with no price. */
function UnpricedMark({ on }: { on: boolean }) {
  if (!on) return null;
  return (
    <span
      title="Part of this use ran on a model SPAWN_MODEL_PRICES does not price: its tokens are counted, its cost is not"
      className="ml-1.5 rounded bg-wait-tint px-1 py-px text-micro font-medium text-warning"
    >
      unpriced use
    </span>
  );
}

function Rows({ title, rows }: { title: string; rows: Row[] }) {
  return (
    <div className="min-w-0">
      <div className="px-4 pt-3 pb-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        {title}
      </div>
      {rows.length === 0 ? (
        <p className="px-4 pb-2 text-caption text-faint-foreground">None in this window.</p>
      ) : (
        <ul className="pb-2">
          {rows.map((row) => (
            <li key={row.key} className="flex items-center gap-2 px-4 py-1 text-caption">
              <span className="min-w-0 truncate font-mono text-muted-foreground" title={row.key}>
                {row.key}
              </span>
              <UnpricedMark on={row.unpriced} />
              <span
                className={cx(
                  "tnum ml-auto shrink-0",
                  row.usd === "unpriced" ? "text-warning" : "text-foreground",
                )}
              >
                {row.usd}
              </span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function WindowCard({ label, figures }: { label: string; figures: Figures | null }) {
  return (
    <Card className="min-w-0 flex-1">
      <CardHeader
        icon={<CircleDollarSign size={14} />}
        title={label}
        meta={
          figures && (
            <span className="flex items-center">
              <span className="tnum text-body font-semibold text-foreground">{figures.total}</span>
              <UnpricedMark on={figures.unpriced} />
            </span>
          )
        }
      />
      {figures ? (
        <div className="grid grid-cols-1 lg:grid-cols-3">
          <Rows title="Missions" rows={figures.missions} />
          <Rows title="Tasks" rows={figures.tasks} />
          <Rows title="Agents" rows={figures.agents} />
        </div>
      ) : (
        <EmptyState title="Loading…" />
      )}
    </Card>
  );
}

/**
 * What the fleet's model calls cost over the last 24 hours and 7 days, in
 * US dollars: the total, each mission, task and agent, most costly first
 * (crates/spawn-cli/src/cost.rs prices each call when it is recorded).
 * Use on a model with no price is marked, and a figure of only such use
 * reads "unpriced", never $0.00. Strictly read-only.
 */
export function CostView() {
  const day = usePoll("cost-24h", (signal) => fetchCost("24h", signal), POLL_MS);
  const week = usePoll("cost-7d", (signal) => fetchCost("7d", signal), POLL_MS);
  const polls = { "24h": day, "7d": week } as const;
  const error = day.error ?? week.error;
  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader title={<h1 className="text-body font-semibold text-foreground">Cost</h1>}>
        <LiveStatus error={error} label="" />
      </PageHeader>
      <div className="min-h-0 flex-1 overflow-y-auto p-4 sm:p-5">
        <div className="flex flex-col gap-4">
          {WINDOWS.map((window) => {
            const data = polls[window.id].data;
            return (
              <WindowCard
                key={window.id}
                label={window.label}
                figures={data ? costView(data) : null}
              />
            );
          })}
        </div>
      </div>
    </main>
  );
}

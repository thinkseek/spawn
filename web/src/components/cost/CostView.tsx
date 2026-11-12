import { CircleDollarSign } from "lucide-react";

import { readApi, usePoll } from "../../api";
import type { CostAnswer, CostView as Figures, Fold, Row } from "../../cost.ts";
import { costView, rowLines } from "../../cost.ts";
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

/** The mark of a row whose figure leaves out use on a model with no
 * price: how many tokens, compact ("+485M unpriced"), the full count
 * ("+ 485,321,042 tokens unpriced") in its title. */
function UnpricedMark({ text }: { text: string }) {
  return (
    <span
      data-cost-unpriced
      title={`${text}: these tokens ran on a model SPAWN_MODEL_PRICES does not price; they are counted, their cost is not`}
      className="min-w-0 break-words rounded bg-wait-tint px-1 py-px font-medium text-warning"
    >
      {text}
    </span>
  );
}

/** A row's share of the window's dollars ("14%", "<1%"); nothing when the
 * window cost nothing priced. */
function ShareMark({ share }: { share: string | null }) {
  if (share === null) return null;
  return (
    <span
      title="Its share of this window's dollars"
      className="tnum w-9 shrink-0 text-right text-micro text-faint-foreground"
    >
      {share}
    </span>
  );
}

function Rows({ title, rows, more = null }: { title: string; rows: Row[]; more?: Fold | null }) {
  return (
    <div className="min-w-0">
      <div className="px-4 pt-3 pb-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        {title}
      </div>
      {rows.length === 0 ? (
        <p className="px-4 pb-2 text-caption text-faint-foreground">None in this window.</p>
      ) : (
        <ul className="pb-2">
          {rows.map((row) => {
            const { first, second } = rowLines(row);
            return (
              <li key={first.key} className="min-w-0 px-4 py-1 text-caption">
                {/* First line: the amount and share never shrink or leave
                    the column; the label truncates before anything else
                    gives way (objective 079). */}
                <div className="flex min-w-0 items-baseline gap-2">
                  <span
                    className={cx(
                      "font-mono text-muted-foreground",
                      first.label ? "shrink-0" : "min-w-0 truncate",
                    )}
                    title={first.key}
                  >
                    {first.key}
                  </span>
                  {first.label && (
                    <span className="min-w-0 flex-1 truncate text-foreground" title={first.label}>
                      {first.label}
                    </span>
                  )}
                  <span
                    className={cx(
                      "tnum ml-auto shrink-0 whitespace-nowrap",
                      first.usd === "unpriced" ? "text-warning" : "text-foreground",
                    )}
                  >
                    {first.usd}
                  </span>
                  <ShareMark share={first.share} />
                </div>
                {/* Second line: the unpriced mark's text and the per-landing
                    text, in full, wrapping if it must. */}
                {second.length > 0 && (
                  <div className="mt-0.5 flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5 text-micro">
                    {second.map((text, index) =>
                      // rowLines puts the unpriced mark's text first.
                      index === 0 && row.unpriced_text ? (
                        <UnpricedMark key={text} text={text} />
                      ) : (
                        <span
                          key={text}
                          title="This mission's priced dollars over its tasks that landed or were accepted in the window"
                          className="tnum min-w-0 break-words text-faint-foreground"
                        >
                          {text}
                        </span>
                      ),
                    )}
                  </div>
                )}
              </li>
            );
          })}
          {more && (
            <li
              data-cost-fold
              title="The rest of this window's rows, past the ten costliest, folded into one line"
              className="tnum px-4 py-1 text-caption text-muted-foreground"
            >
              {more.text}
            </li>
          )}
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
              <span
                title="The window's priced dollars over the tasks that landed or were accepted in it, counted as the scorecard counts landings"
                className="tnum ml-2 text-caption text-muted-foreground"
              >
                {figures.per_landing}
              </span>
            </span>
          )
        }
      />
      {figures?.unpriced_note && (
        <p
          title="SPAWN_MODEL_PRICES is a JSON object from model name to its four rates; set, it replaces the built-in prices whole. A price applies to calls recorded after it is set."
          className="px-4 pt-2 text-caption text-warning"
        >
          {figures.unpriced_note}
        </p>
      )}
      {figures?.since && (
        <p
          title="The cost record holds no message from before this time, so this window's figures may be low"
          className="px-4 pt-2 text-caption text-warning"
        >
          {figures.since}
        </p>
      )}
      {figures ? (
        <div className="grid grid-cols-1 lg:grid-cols-3 [&>*]:min-w-0">
          <Rows title="Missions" rows={figures.missions} />
          <Rows title="Tasks" rows={figures.tasks} more={figures.tasks_more} />
          <Rows title="Agents" rows={figures.agents} more={figures.agents_more} />
        </div>
      ) : (
        <EmptyState title="Loading…" />
      )}
    </Card>
  );
}

/**
 * What the fleet's model calls cost over the last 24 hours and 7 days, in
 * US dollars: the total and what a landing cost, each mission (with its
 * own dollars per landing) and the ten costliest tasks and agents, most
 * costly first, the rest of each folded into one line ("3 more tasks ·
 * $1.80"); each row says its share of the window
 * (crates/spawn-cli/src/cost.rs prices each call when it is recorded).
 * Each mission row shows its name beside its id ("#4 Spawn RSI"), each
 * task row its label; a long name truncates so the dollars stay whole. Under each window's total a
 * note says once how many tokens no price counts, on which models, and how
 * to price them; a row with such use says its count compact ("+485M
 * unpriced", the full count in its title), and a figure of only
 * such use reads "unpriced", never $0.00. A window that starts before the earliest
 * message the record holds says since when it counts. Strictly read-only.
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

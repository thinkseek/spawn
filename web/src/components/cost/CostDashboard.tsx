import type { BudgetGauge, Dashboard, RankBar, Tile, TrendBar } from "../../cost.ts";
import { cx } from "../ui";
import { Card, CardHeader } from "../aui/kit";
// CostView imports this file too; UnpricedMark is only called at render,
// so the cycle never reads it before CostView has defined it.
import { UnpricedMark } from "./CostView";

/** A small uppercase heading inside a card. */
function Caption({ children }: { children: string }) {
  return (
    <div className="text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">{children}</div>
  );
}

function TileView({ name, tile }: { name: string; tile: Tile }) {
  return (
    <div title={tile.title} className="min-w-0 px-2 py-2">
      <Caption>{name}</Caption>
      <div className="tnum mt-1 truncate text-[1.375rem] font-semibold leading-tight text-foreground">
        {tile.value}
      </div>
      <div className="mt-1 flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5 text-micro">
        <span className="tnum min-w-0 break-words text-muted-foreground">{tile.detail}</span>
        {tile.unpriced_short && (
          <UnpricedMark text={tile.unpriced_text ?? tile.unpriced_short} label={tile.unpriced_short} />
        )}
      </div>
    </div>
  );
}

/** The four tiles in one card: four across, two across when narrow. */
function Tiles({ view }: { view: Dashboard }) {
  return (
    <Card as="section" aria-label="Cost at a glance">
      <div data-cost-tiles className="grid grid-cols-2 gap-y-1 py-1 lg:grid-cols-4 [&>*]:min-w-0">
        <TileView name="Spend" tile={view.spend} />
        <TileView name="Tokens" tile={view.tokens} />
        <TileView name="Cache-read share" tile={view.cache} />
        <TileView name="Per landed task" tile={view.landing} />
      </div>
    </Card>
  );
}

/** The spend by step as a row of bars; a step with use on no price gets a
 * warning cap, so it never reads as a step that cost nothing. */
function Trend({ bars, note, unpriced }: { bars: TrendBar[]; note: string; unpriced: number }) {
  const ticks = new Set([0, Math.floor((bars.length - 1) / 2), bars.length - 1]);
  return (
    <Card as="section" className="min-w-0">
      <CardHeader titleAs="h2" title="Spend over the window" description={note} />
      <div className="px-4 pb-3">
        {bars.length === 0 ? (
          <p className="text-caption text-faint-foreground">No trend in this answer.</p>
        ) : (
          <>
            <div data-cost-trend className="flex h-24 items-end gap-0.5" role="img" aria-label={`Spend, ${note}`}>
              {bars.map((bar, index) => (
                <div key={index} title={bar.title} className="flex h-full min-w-0 flex-1 flex-col justify-end">
                  {bar.unpriced && (
                    <div data-cost-unpriced-step className="mb-px h-1 rounded-sm bg-warning" />
                  )}
                  <div
                    className={cx("rounded-t-sm", bar.height > 0 ? "bg-info" : "bg-surface-border")}
                    style={{ height: bar.height > 0 ? `${bar.height}%` : "1px" }}
                  />
                </div>
              ))}
            </div>
            <div className="mt-1 flex gap-0.5 text-micro text-faint-foreground">
              {bars.map((bar, index) => (
                <span key={index} className="tnum min-w-0 flex-1 overflow-visible whitespace-nowrap text-center">
                  {ticks.has(index) ? bar.label : ""}
                </span>
              ))}
            </div>
            {unpriced > 0 && (
              <p className="mt-2 text-micro text-warning">
                {unpriced} step{unpriced === 1 ? "" : "s"} had use on a model with no price (marked): its cost is not
                in the bars.
              </p>
            )}
          </>
        )}
      </div>
    </Card>
  );
}

function Gauge({ gauge }: { gauge: BudgetGauge }) {
  return (
    <li data-cost-budget className="min-w-0 py-1.5 text-caption">
      <div className="flex min-w-0 items-baseline gap-2">
        <span className="shrink-0 font-mono text-muted-foreground">{gauge.key}</span>
        {gauge.label && <span className="min-w-0 flex-1 truncate text-foreground">{gauge.label}</span>}
        <span className={cx("tnum ml-auto shrink-0", gauge.over ? "font-medium text-warning" : "text-foreground")}>
          {gauge.percent}
        </span>
      </div>
      <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-muted">
        <div
          className={cx("h-full rounded-full", gauge.over ? "bg-warning" : "bg-info")}
          style={{ width: `${gauge.width}%` }}
        />
      </div>
      <div
        className={cx("tnum mt-0.5 text-micro", gauge.over ? "text-warning" : "text-faint-foreground")}
        title={gauge.over ? "At or past its daily budget: the cost_budget alert is open for today" : undefined}
      >
        {gauge.text}
        {gauge.over ? " · budget reached" : ""}
      </div>
    </li>
  );
}

function Budgets({ gauges }: { gauges: BudgetGauge[] }) {
  return (
    <Card as="section" className="min-w-0">
      <CardHeader titleAs="h2" title="Daily budgets" description="each mission's spend of the UTC day so far" />
      <ul className="px-4 pb-2">
        {gauges.map((gauge) => (
          <Gauge key={gauge.key} gauge={gauge} />
        ))}
      </ul>
    </Card>
  );
}

/** One ranked row: its id and name, its bar of the window's dollars, its
 * amount and share. A row of only unpriced use draws a dashed warning bar,
 * never an empty one. */
function Bar({ bar }: { bar: RankBar }) {
  const onlyUnpriced = bar.usd === "unpriced";
  return (
    <li data-cost-bar className="min-w-0 py-1 text-caption">
      <div className="flex min-w-0 items-baseline gap-2">
        <span className={cx("font-mono text-muted-foreground", bar.label ? "shrink-0" : "min-w-0 truncate")} title={bar.key}>
          {bar.key}
        </span>
        {bar.label && (
          <span className="min-w-0 flex-1 truncate text-foreground" title={bar.label}>
            {bar.label}
          </span>
        )}
        <span className={cx("tnum ml-auto shrink-0", onlyUnpriced ? "text-warning" : "text-foreground")}>
          {bar.usd}
        </span>
        {bar.share !== null && (
          <span className="tnum w-9 shrink-0 text-right text-micro text-faint-foreground">{bar.share}</span>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2">
        <div className="h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-muted">
          {onlyUnpriced ? (
            <div className="h-full w-full rounded-full border border-dashed border-warning bg-wait-tint" />
          ) : (
            <div className="h-full rounded-full bg-info" style={{ width: `${Math.max(bar.width, 0.5)}%` }} />
          )}
        </div>
        {bar.unpriced_short && (
          <span className="shrink-0 text-micro">
            <UnpricedMark text={bar.unpriced_text ?? bar.unpriced_short} label={bar.unpriced_short} />
          </span>
        )}
      </div>
    </li>
  );
}

function Ranked({ title, bars }: { title: string; bars: RankBar[] }) {
  return (
    <Card as="section" className="min-w-0">
      <CardHeader titleAs="h2" title={title} description="share of the window's dollars" />
      {bars.length === 0 ? (
        <p className="px-4 pb-3 text-caption text-faint-foreground">None in this window.</p>
      ) : (
        <ul className="px-4 pb-2">
          {bars.map((bar) => (
            <Bar key={bar.key} bar={bar} />
          ))}
        </ul>
      )}
    </Card>
  );
}

/**
 * The cost page's dashboard for one window (GitHub #64), from `dashboard()`
 * (web/src/cost.ts) alone: the four tiles, the trend beside the daily
 * budgets (only when a mission has one), and the top missions and models.
 */
export function CostDashboard({ view }: { view: Dashboard }) {
  return (
    <div className="flex flex-col gap-4">
      <Tiles view={view} />
      <div className={cx("grid grid-cols-1 gap-4 [&>*]:min-w-0", view.budgets.length > 0 && "lg:grid-cols-[2fr_1fr]")}>
        <Trend bars={view.trend} note={view.trend_note} unpriced={view.trend_unpriced} />
        {view.budgets.length > 0 && <Budgets gauges={view.budgets} />}
      </div>
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2 [&>*]:min-w-0">
        <Ranked title="Top missions" bars={view.missions} />
        <Ranked title="Top models" bars={view.models} />
      </div>
    </div>
  );
}

// The cost page's figures (objective 041), from what `GET /api/cost?window=`
// answers (crates/spawn-cli/src/cost.rs): the total, and each mission, task
// and agent, in dollars, most costly first. Pure, with no import, so node
// loads it alone (type stripping, no bundler): the page's fetch lives in
// components/cost/CostView.tsx.
//
// The scorecard's rule holds here too (GitHub #16 item 3): a cost that is
// not zero never reads as zero. Use on a model no price names is "unpriced",
// never "$0.00": it cost something no price says.

/** Token counts of one entry: every message, priced or not. */
export interface CostTokens {
  input: number;
  output: number;
  cache_write: number;
  cache_read: number;
}

/** What one mission, task, agent or the whole window cost. */
export interface CostEntry {
  usd: number;
  tokens: CostTokens;
  /** The four counts of the messages under a model with no price. */
  unpriced_tokens: number;
}

export interface CostModel {
  model: string;
  /** Null: the model has no price. */
  usd: number | null;
  tokens: CostTokens;
}

export interface CostMission extends CostEntry {
  mission: number;
}

export interface CostTask extends CostEntry {
  task: number;
  mission: number | null;
}

export interface CostAgent extends CostEntry {
  agent: string;
}

export interface CostExecution extends CostEntry {
  execution: string;
  agent: string;
  task: number | null;
  models: CostModel[];
}

/** `GET /api/cost?window=24h|7d`. */
export interface CostAnswer {
  window: string;
  since_ms: number;
  until_ms: number;
  total: CostEntry;
  models: CostModel[];
  missions: CostMission[];
  tasks: CostTask[];
  agents: CostAgent[];
  executions: CostExecution[];
}

/** One line of the page: its label, its cost as shown, and whether any of
 * its use was on a model with no price. */
export interface Row {
  key: string;
  usd: string;
  unpriced: boolean;
}

export interface CostView {
  total: string;
  unpriced: boolean;
  missions: Row[];
  tasks: Row[];
  agents: Row[];
}

const DOLLARS = new Intl.NumberFormat("en-US", {
  style: "currency",
  currency: "USD",
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

/** An amount of US dollars to the cent, grouped ("$0.94", "$1,234.57");
 * null "—"; zero "$0.00"; one that is not zero but rounds to $0.00
 * "<$0.01", never "$0.00". */
export function formatUsd(usd: number | null): string {
  if (usd === null || !Number.isFinite(usd)) return "—";
  if (usd === 0) return "$0.00";
  if (Math.abs(usd) < 0.005) return usd < 0 ? "-<$0.01" : "<$0.01";
  return DOLLARS.format(usd);
}

function count(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function tokenSum(tokens: unknown): number {
  if (typeof tokens !== "object" || tokens === null) return 0;
  const record = tokens as Record<string, unknown>;
  return count(record.input) + count(record.output) + count(record.cache_write) + count(record.cache_read);
}

/** An entry's figure: its dollars, or "unpriced" when all its use was on
 * models with no price (it cost something no price says). */
function shown(entry: Partial<CostEntry> | null | undefined): { usd: string; unpriced: boolean; order: number; weight: number } {
  const usd = typeof entry?.usd === "number" && Number.isFinite(entry.usd) ? entry.usd : 0;
  const unpricedTokens = count(entry?.unpriced_tokens);
  const priced = tokenSum(entry?.tokens) - unpricedTokens;
  const unpriced = unpricedTokens > 0;
  const text = unpriced && priced <= 0 && usd === 0 ? "unpriced" : formatUsd(usd);
  return { usd: text, unpriced, order: usd, weight: unpricedTokens };
}

function rows<T>(entries: unknown, key: (entry: T) => string): Row[] {
  if (!Array.isArray(entries)) return [];
  return (entries as T[])
    .map((entry) => ({ key: key(entry), ...shown(entry as Partial<CostEntry>) }))
    .sort((a, b) => b.order - a.order || b.weight - a.weight || a.key.localeCompare(b.key))
    .map(({ key: label, usd, unpriced }) => ({ key: label, usd, unpriced }));
}

/** The page's figures for one window's answer: the total; each mission
 * (`#<id>`), task (`T<id>`) and agent (its id), most costly first. */
export function costView(answer: Partial<CostAnswer> | null | undefined): CostView {
  const total = shown(answer?.total);
  return {
    total: total.usd,
    unpriced: total.unpriced,
    missions: rows<CostMission>(answer?.missions, (entry) => `#${entry.mission}`),
    tasks: rows<CostTask>(answer?.tasks, (entry) => `T${entry.task}`),
    agents: rows<CostAgent>(answer?.agents, (entry) => String(entry.agent)),
  };
}

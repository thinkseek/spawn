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

/** What a landing cost (objective 060): the tasks that landed or were
 * accepted in the window, as the scorecard counts landings, and the priced
 * dollars over them (null when none landed). */
export interface CostLandings {
  landed: number;
  usd_per_landing: number | null;
}

export interface CostMission extends CostEntry, Partial<CostLandings> {
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
export interface CostAnswer extends CostLandings {
  window: string;
  since_ms: number;
  until_ms: number;
  total: CostEntry;
  models: CostModel[];
  missions: CostMission[];
  tasks: CostTask[];
  agents: CostAgent[];
  executions: CostExecution[];
  /** The time of the earliest message the cost record holds; null when it
   * holds none (objective 048). */
  counted_since_ms: number | null;
}

/** One line of the page: its label, its cost as shown, and whether any of
 * its use was on a model with no price. */
export interface Row {
  key: string;
  usd: string;
  unpriced: boolean;
  /** A mission's own dollars per landing ([`perLanding`]); absent on a
   * task's or an agent's line. */
  per_landing?: string;
}

export interface CostView {
  total: string;
  unpriced: boolean;
  /** "<dollars> per landing (<n> landed)", or "no landing in the window". */
  per_landing: string;
  missions: Row[];
  tasks: Row[];
  agents: Row[];
  /** "Counted since <YYYY-MM-DD HH:MM> UTC" when the window starts before
   * the earliest message the record holds (its figures may be low); null
   * otherwise. */
  since: string | null;
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

const COUNT = new Intl.NumberFormat("en-US");

/** What a landing cost in the window: "<dollars> per landing (<n>
 * landed)", the dollars as [`formatUsd`] says them; "no landing in the
 * window" when none landed. */
export function perLanding(entry: Partial<CostLandings> | null | undefined): string {
  const landed = count(entry?.landed);
  if (landed <= 0) return "no landing in the window";
  const per = entry?.usd_per_landing;
  const usd = typeof per === "number" && Number.isFinite(per) ? per : null;
  return `${formatUsd(usd)} per landing (${COUNT.format(landed)} landed)`;
}

function rows<T>(entries: unknown, key: (entry: T) => string, landings = false): Row[] {
  if (!Array.isArray(entries)) return [];
  return (entries as T[])
    .map((entry) => ({
      key: key(entry),
      ...shown(entry as Partial<CostEntry>),
      ...(landings ? { per_landing: perLanding(entry as Partial<CostLandings>) } : {}),
    }))
    .sort((a, b) => b.order - a.order || b.weight - a.weight || a.key.localeCompare(b.key))
    .map(({ order: _order, weight: _weight, ...row }) => row);
}

function twoDigits(value: number): string {
  return String(value).padStart(2, "0");
}

/** The note of a window that starts before the record's first message:
 * "Counted since <YYYY-MM-DD HH:MM> UTC", to the minute; null when the
 * record holds nothing or counts from the window's start. */
export function countedSince(answer: Partial<CostAnswer> | null | undefined): string | null {
  const counted = answer?.counted_since_ms;
  const since = answer?.since_ms;
  if (typeof counted !== "number" || !Number.isFinite(counted)) return null;
  if (typeof since !== "number" || !Number.isFinite(since) || since >= counted) return null;
  const at = new Date(counted);
  if (Number.isNaN(at.getTime())) return null;
  const day = `${at.getUTCFullYear()}-${twoDigits(at.getUTCMonth() + 1)}-${twoDigits(at.getUTCDate())}`;
  return `Counted since ${day} ${twoDigits(at.getUTCHours())}:${twoDigits(at.getUTCMinutes())} UTC`;
}

/** The page's figures for one window's answer: the total and what a
 * landing cost; each mission (with its own dollars per landing),
 * `#<id>`; task (`T<id>`) and agent (its id), most costly first; and
 * since when the record counts, when the window starts before it. */
export function costView(answer: Partial<CostAnswer> | null | undefined): CostView {
  const total = shown(answer?.total);
  return {
    total: total.usd,
    unpriced: total.unpriced,
    per_landing: perLanding(answer),
    missions: rows<CostMission>(answer?.missions, (entry) => `#${entry.mission}`, true),
    tasks: rows<CostTask>(answer?.tasks, (entry) => `T${entry.task}`),
    agents: rows<CostAgent>(answer?.agents, (entry) => String(entry.agent)),
    since: countedSince(answer),
  };
}

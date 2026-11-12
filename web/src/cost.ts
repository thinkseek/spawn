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
  /** Its four counts when the model has no price, 0 when it has one. */
  unpriced_tokens: number;
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
  /** The mission's name, as `/api/missions` gives its `mission_name`;
   * null for a mission the record does not hold. */
  name?: string | null;
}

export interface CostTask extends CostEntry {
  task: number;
  mission: number | null;
  /** The task's label, as `/api/missions` gives it; null for a task the
   * record does not hold. */
  label: string | null;
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

/** One line of the page: its id, its cost as shown, whether any of its use
 * was on a model with no price and how many tokens that was, and (a task's)
 * the label the API gives it. */
export interface Row {
  key: string;
  usd: string;
  unpriced: boolean;
  /** Tokens of its use on models with no price; 0 when none. */
  unpriced_tokens: number;
  /** "+ 220,000 tokens unpriced"; null when none. */
  unpriced_text: string | null;
  /** The same count made compact, for the badge: "+485M unpriced"
   * ([`unpricedShort`]); null when none. */
  unpriced_short: string | null;
  /** A task's label or a mission's name as the API gives it; null
   * otherwise. */
  label: string | null;
  /** A mission's own dollars per landing ([`perLanding`]); absent on a
   * task's or an agent's line. */
  per_landing?: string;
  /** Its dollars over the window's, a whole percent rounded to the nearest
   * ("14%"); "<1%" when above zero and under one percent; null when the
   * window cost nothing priced ([`shareText`]). */
  share: string | null;
}

/** A row as the page lays it out (objective 079): `first` is one line, its
 * key, label, amount and share, the amount and share always in full and
 * the label truncating before them; `second` is the line under it, in
 * full and wrapping if it must: its unpriced mark's text, then its
 * per-landing text, each only when the row has it ([] for neither). */
export interface RowLines {
  first: { key: string; label: string | null; usd: string; share: string | null };
  second: string[];
}

export function rowLines(row: Row): RowLines {
  const second: string[] = [];
  if (row.unpriced_text) second.push(row.unpriced_text);
  if (row.per_landing !== undefined) second.push(row.per_landing);
  return { first: { key: row.key, label: row.label, usd: row.usd, share: row.share }, second };
}

/** The rows past the ten costliest, folded into one line: how many, their
 * dollars summed to the cent, and "<n> more tasks · <dollars>". */
export interface Fold {
  count: number;
  usd: number;
  text: string;
}

export interface CostView {
  total: string;
  unpriced: boolean;
  /** Said once for the window: how many of its tokens have no price, on
   * which models, and how to price them (SPAWN_MODEL_PRICES); null when
   * every token is priced. */
  unpriced_note: string | null;
  /** "<dollars> per landing (<n> landed)", or "no landing in the window". */
  per_landing: string;
  missions: Row[];
  /** The ten costliest tasks; the rest are in `tasks_more`. */
  tasks: Row[];
  /** The tasks past the ten costliest; null with ten or fewer. */
  tasks_more: Fold | null;
  /** The ten costliest agents; the rest are in `agents_more`. */
  agents: Row[];
  /** The agents past the ten costliest; null with ten or fewer. */
  agents_more: Fold | null;
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

const TOKENS = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });

/** A token count grouped the "en-US" way ("275,000"). */
export function formatTokens(tokens: number): string {
  return TOKENS.format(Number.isFinite(tokens) ? tokens : 0);
}

/** What a row with unpriced use says of it: "+ 220,000 tokens unpriced"
 * ("1 token"); null when it has none. */
export function unpricedText(tokens: number): string | null {
  if (!Number.isFinite(tokens) || tokens <= 0) return null;
  return `+ ${formatTokens(tokens)} token${tokens === 1 ? "" : "s"} unpriced`;
}

/** A count's step: its divisor and its letter. */
const STEPS: ReadonlyArray<[number, string]> = [
  [1e3, "K"],
  [1e6, "M"],
  [1e9, "B"],
];

/** A count made compact: "950", "55K", "1.1M", "17M", "485M", "1.5B"; one
 * decimal under 10, whole from 10, K, M or B by thousands. It is rounded
 * first and the step picked after, so 9,960 is "10K" and 999,600 "1.0M". */
export function compactCount(value: number): string {
  const n = Number.isFinite(value) ? Math.abs(value) : 0;
  const sign = Number.isFinite(value) && value < 0 ? "-" : "";
  const whole = Math.round(n);
  if (whole < 1000) return `${sign}${whole}`;
  let said = "";
  for (const [size, letter] of STEPS) {
    const scaled = n / size;
    const rounded = scaled < 10 ? Math.round(scaled * 10) / 10 : Math.round(scaled);
    said = `${rounded < 10 ? rounded.toFixed(1) : String(rounded)}${letter}`;
    if (rounded < 1000) break;
  }
  return `${sign}${said}`;
}

/** A row's unpriced badge, short enough to keep its dollars on the page:
 * "+485M unpriced" ([`compactCount`]); null when it has none. */
export function unpricedShort(tokens: number): string | null {
  if (!Number.isFinite(tokens) || tokens <= 0) return null;
  return `+${compactCount(tokens)} unpriced`;
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

/** How many rows of tasks and of agents the page names; the rest fold. */
export const TOP_ROWS = 10;

/** A row's share of the window: its dollars over the window's, a whole
 * percent rounded to the nearest ("14%"); "<1%" when above zero and under
 * one percent; null when the window cost nothing priced. */
export function shareText(usd: number, windowUsd: number): string | null {
  if (!Number.isFinite(windowUsd) || windowUsd <= 0) return null;
  const percent = Number.isFinite(usd) ? (usd * 100) / windowUsd : 0;
  if (percent > 0 && percent < 1) return "<1%";
  return `${Math.round(percent) + 0}%`;
}

/** The rows past the first `top`, folded into one line: their count, their
 * dollars summed and rounded to the cent, and "<n> more <noun>s ·
 * <dollars>" ("1 more <noun>"); null when none is past them. */
export function fold(dollars: number[], noun: string, top = TOP_ROWS): Fold | null {
  const rest = dollars.slice(top);
  if (rest.length === 0) return null;
  const sum = rest.reduce((all, usd) => all + (Number.isFinite(usd) ? usd : 0), 0);
  const usd = Math.round(sum * 100) / 100 + 0;
  // Said from the sum itself, so a fold that cost under a cent reads
  // "<$0.01", never "$0.00".
  const text = `${COUNT.format(rest.length)} more ${noun}${rest.length === 1 ? "" : "s"} · ${formatUsd(sum)}`;
  return { count: rest.length, usd, text };
}

/** The rows of a list, most costly first, each with its share of the
 * window's `windowUsd`, and each one's dollars in the same order; with
 * `landings` (a mission's), each carries its own dollars per landing. */
function rows<T>(
  entries: unknown,
  windowUsd: number,
  key: (entry: T) => string,
  label: (entry: T) => string | null = () => null,
  landings = false,
): { rows: Row[]; dollars: number[] } {
  if (!Array.isArray(entries)) return { rows: [], dollars: [] };
  const sorted = (entries as T[])
    .map((entry) => ({
      key: key(entry),
      label: label(entry),
      per_landing: landings ? perLanding(entry as Partial<CostLandings>) : undefined,
      ...shown(entry as Partial<CostEntry>),
    }))
    .sort((a, b) => b.order - a.order || b.weight - a.weight || a.key.localeCompare(b.key));
  return {
    rows: sorted.map(({ key: id, label: named, per_landing, usd, unpriced, weight, order }) => {
      const row: Row = {
        key: id,
        usd,
        unpriced,
        unpriced_tokens: weight,
        unpriced_text: unpricedText(weight),
        unpriced_short: unpricedShort(weight),
        label: named,
        share: shareText(order, windowUsd),
      };
      if (per_landing !== undefined) row.per_landing = per_landing;
      return row;
    }),
    dollars: sorted.map((entry) => entry.order),
  };
}

function taskLabel(entry: CostTask): string | null {
  const label = (entry as Partial<CostTask>).label;
  return typeof label === "string" && label.trim() !== "" ? label : null;
}

function missionName(entry: CostMission): string | null {
  const name = (entry as Partial<CostMission>).name;
  return typeof name === "string" && name.trim() !== "" ? name : null;
}

/** The window's note on what no price counts, said once: "275,000 of
 * 385,000 tokens (71%) ran on models with no price (bb-model-z): their
 * cost is not in these figures. Price them in SPAWN_MODEL_PRICES." Each
 * unpriced model is named, with its count when there are several; a priced
 * model is not. Null when every token is priced. */
export function unpricedNote(answer: Partial<CostAnswer> | null | undefined): string | null {
  const unpriced = count(answer?.total?.unpriced_tokens);
  if (unpriced <= 0) return null;
  const all = tokenSum(answer?.total?.tokens);
  const models = (Array.isArray(answer?.models) ? answer.models : [])
    .map((model) => {
      const given = (model as Partial<CostModel>).unpriced_tokens;
      // An answer from before the rows carried their count: a model with
      // no price is unpriced in all its tokens.
      const tokens = typeof given === "number" ? count(given) : model?.usd === null ? tokenSum(model?.tokens) : 0;
      return { name: String(model?.model ?? ""), tokens };
    })
    .filter((model) => model.tokens > 0 && model.name !== "")
    .sort((a, b) => b.tokens - a.tokens || a.name.localeCompare(b.name));
  const share = all > 0 ? unpriced / all : 0;
  const percent = share >= 0.01 ? `${Math.round(share * 100)}%` : "under 1%";
  const of = all >= unpriced && all > 0 ? ` of ${formatTokens(all)} tokens (${percent})` : " tokens";
  const named =
    models.length === 1
      ? ` (${models[0].name})`
      : models.length > 1
        ? ` (${models.map((model) => `${model.name} ${formatTokens(model.tokens)}`).join(", ")})`
        : "";
  const noun = models.length === 1 ? "a model" : "models";
  return (
    `${formatTokens(unpriced)}${of} ran on ${noun} with no price${named}: their cost is not in ` +
    `these figures. Price ${models.length === 1 ? "it" : "them"} in SPAWN_MODEL_PRICES ` +
    `(US dollars per million tokens: input, output, cache_write, cache_read).`
  );
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

/** The page's figures for one window's answer: the total, what a landing
 * cost and, once, the note on its unpriced tokens; each mission (`#<id>`,
 * with its name and its own dollars per landing), the ten costliest tasks (`T<id>`, with
 * its label) and agents (its id) with the rest of each folded into one
 * line, most costly first, each with its unpriced count and its share of
 * the window; and since when the record counts, when the window starts
 * before it. */
export function costView(answer: Partial<CostAnswer> | null | undefined): CostView {
  const total = shown(answer?.total);
  const windowUsd = total.order;
  const missions = rows<CostMission>(answer?.missions, windowUsd, (entry) => `#${entry.mission}`, missionName, true);
  const tasks = rows<CostTask>(answer?.tasks, windowUsd, (entry) => `T${entry.task}`, taskLabel);
  const agents = rows<CostAgent>(answer?.agents, windowUsd, (entry) => String(entry.agent));
  return {
    total: total.usd,
    unpriced: total.unpriced,
    unpriced_note: unpricedNote(answer),
    per_landing: perLanding(answer),
    missions: missions.rows,
    tasks: tasks.rows.slice(0, TOP_ROWS),
    tasks_more: fold(tasks.dollars, "task"),
    agents: agents.rows.slice(0, TOP_ROWS),
    agents_more: fold(agents.dollars, "agent"),
    since: countedSince(answer),
  };
}

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

export interface CostMission extends CostEntry {
  mission: number;
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
  /** A task's label as the API gives it; null otherwise. */
  label: string | null;
}

export interface CostView {
  total: string;
  unpriced: boolean;
  /** Said once for the window: how many of its tokens have no price, on
   * which models, and how to price them (SPAWN_MODEL_PRICES); null when
   * every token is priced. */
  unpriced_note: string | null;
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

function rows<T>(
  entries: unknown,
  key: (entry: T) => string,
  label: (entry: T) => string | null = () => null,
): Row[] {
  if (!Array.isArray(entries)) return [];
  return (entries as T[])
    .map((entry) => ({ key: key(entry), label: label(entry), ...shown(entry as Partial<CostEntry>) }))
    .sort((a, b) => b.order - a.order || b.weight - a.weight || a.key.localeCompare(b.key))
    .map(({ key: id, label: named, usd, unpriced, weight }) => ({
      key: id,
      usd,
      unpriced,
      unpriced_tokens: weight,
      unpriced_text: unpricedText(weight),
      label: named,
    }));
}

function taskLabel(entry: CostTask): string | null {
  const label = (entry as Partial<CostTask>).label;
  return typeof label === "string" && label.trim() !== "" ? label : null;
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

/** The page's figures for one window's answer: the total and, once, the
 * note on its unpriced tokens; each mission (`#<id>`), task (`T<id>`, with
 * its label) and agent (its id), most costly first, each with its unpriced
 * count; and since when the record counts, when the window starts before
 * it. */
export function costView(answer: Partial<CostAnswer> | null | undefined): CostView {
  const total = shown(answer?.total);
  return {
    total: total.usd,
    unpriced: total.unpriced,
    unpriced_note: unpricedNote(answer),
    missions: rows<CostMission>(answer?.missions, (entry) => `#${entry.mission}`),
    tasks: rows<CostTask>(answer?.tasks, (entry) => `T${entry.task}`, taskLabel),
    agents: rows<CostAgent>(answer?.agents, (entry) => String(entry.agent)),
    since: countedSince(answer),
  };
}

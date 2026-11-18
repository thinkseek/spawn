// The cost page's figures (objective 041), from what `GET /api/cost?window=`
// answers (crates/spawn-cli/src/cost.rs): the total, and each mission, task
// and agent, in dollars, most costly first. Pure, importing only format.ts
// (itself import-free), so node loads it alone (type stripping, no bundler):
// the page's fetch lives in components/cost/CostView.tsx.
//
// The scorecard's rule holds here too (GitHub #16 item 3): a cost that is
// not zero never reads as zero. Use on a model no price names is "unpriced",
// never "$0.00": it cost something no price says.

import { compactCount } from "./format.ts";

export { compactCount };

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
  /** The window of equal length just before this one (GitHub #64); absent
   * in an answer from before it. */
  previous?: CostPrevious;
  /** The window in steps of `trend_step_ms`, oldest first. */
  trend?: CostStep[];
  trend_step_ms?: number;
  /** Each mission with a daily budget and its spend of the UTC day so far:
   * the reading the `cost_budget` alert takes. */
  budgets?: CostBudget[];
}

export interface CostPrevious extends CostLandings {
  total: CostEntry;
  since_ms: number;
  until_ms: number;
}

export interface CostStep {
  since_ms: number;
  usd: number;
  unpriced_tokens: number;
}

export interface CostBudget {
  mission: number;
  budget_usd: number;
  spent_usd: number;
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
  /** The same note with every count in full ("275,000"), for its title. */
  unpriced_note_exact: string | null;
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

/** The window's note on what no price counts, said once: "275K of 385K
 * tokens (71%) ran on models with no price (bb-model-z): their
 * cost is not in these figures. Price them in SPAWN_MODEL_PRICES." Each
 * unpriced model is named, with its count when there are several; a priced
 * model is not. Counts are compact (`say`, [`compactCount`]); pass
 * [`formatTokens`] for the same note in full. Null when every token is priced. */
export function unpricedNote(
  answer: Partial<CostAnswer> | null | undefined,
  say: (tokens: number) => string = compactCount,
): string | null {
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
  const of = all >= unpriced && all > 0 ? ` of ${say(all)} tokens (${percent})` : " tokens";
  const named =
    models.length === 1
      ? ` (${models[0].name})`
      : models.length > 1
        ? ` (${models.map((model) => `${model.name} ${say(model.tokens)}`).join(", ")})`
        : "";
  const noun = models.length === 1 ? "a model" : "models";
  return (
    `${say(unpriced)}${of} ran on ${noun} with no price${named}: their cost is not in ` +
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
/** The title of the cost page's unpriced note (t505u P4): the note with
 * every count in full (`unpriced_note_exact`), a blank line, then how to
 * price a model. The page shows the note compact; this keeps it exact. */
export function unpricedNoteTitle(view: Pick<CostView, "unpriced_note_exact">): string {
  const hint =
    "SPAWN_MODEL_PRICES is a JSON object from model name to its four rates; set, it replaces the built-in prices whole. A price applies to calls recorded after it is set.";
  return view.unpriced_note_exact ? `${view.unpriced_note_exact}\n\n${hint}` : hint;
}

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
    unpriced_note_exact: unpricedNote(answer, formatTokens),
    per_landing: perLanding(answer),
    missions: missions.rows,
    tasks: tasks.rows.slice(0, TOP_ROWS),
    tasks_more: fold(tasks.dollars, "task"),
    agents: agents.rows.slice(0, TOP_ROWS),
    agents_more: fold(agents.dollars, "agent"),
    since: countedSince(answer),
  };
}

// --- The dashboard (GitHub #64): four tiles, the trend, the budgets and the
// ranked bars, from the same answer. Presentation only.

/** One tile of the KPI row: its value as shown, the line under it, and the
 * unpriced mark's compact text when part of the figure has no price. */
export interface Tile {
  value: string;
  detail: string;
  title: string;
  unpriced_short: string | null;
  /** The same count in full ("+ 1,250,000 tokens unpriced"), for its title. */
  unpriced_text: string | null;
}

/** One step of the trend: its bar's height (0-100, of the costliest step),
 * whether it holds use with no price, and its words for the hover. */
export interface TrendBar {
  label: string;
  title: string;
  height: number;
  unpriced: boolean;
}

/** A mission's daily budget as a gauge: how full (0-100, capped), and
 * `over` exactly when the alert fires (spend at or past the budget). */
export interface BudgetGauge {
  key: string;
  label: string | null;
  text: string;
  percent: string;
  width: number;
  over: boolean;
}

/** One ranked bar: its row, and its width (0-100) as its share of the
 * window's priced dollars. A row with use on no price (`unpriced`) keeps
 * its mark; one of only such use has width 0 and reads "unpriced". */
export interface RankBar extends Row {
  width: number;
}

export interface Dashboard {
  spend: Tile;
  tokens: Tile;
  cache: Tile;
  landing: Tile;
  trend: TrendBar[];
  /** What the trend's steps are: "hours back from now, UTC" or "days
   * back from now, UTC". */
  trend_note: string;
  /** How many steps had use with no price (not in their bar's height). */
  trend_unpriced: number;
  budgets: BudgetGauge[];
  missions: RankBar[];
  models: RankBar[];
}

/** How many bars each ranking shows. */
export const TOP_BARS = 5;

function finiteOr(value: unknown, otherwise: number): number {
  return typeof value === "number" && Number.isFinite(value) ? value : otherwise;
}

/** A window's length in words: "24 h", "7 days". */
function lengthWords(answer: Partial<CostAnswer> | null | undefined): string {
  const length = finiteOr(answer?.until_ms, 0) - finiteOr(answer?.since_ms, 0);
  if (length > 0 && length <= 86_400_000) return `${Math.round(length / 3_600_000)} h`;
  if (length > 86_400_000) return `${Math.round(length / 86_400_000)} days`;
  return String(answer?.window ?? "window");
}

/** The spend's change against the previous window of equal length:
 * "▲ 23% vs previous 24 h", "▼ 8% …", "same as previous 24 h"; when the
 * previous window cost nothing priced, its amount ("previous 24 h:
 * $0.00"); null with no previous window in the answer. */
export function spendDelta(answer: Partial<CostAnswer> | null | undefined): string | null {
  const previous = answer?.previous;
  if (!previous || typeof previous !== "object") return null;
  const now = finiteOr(answer?.total?.usd, 0);
  const before = finiteOr(previous.total?.usd, 0);
  const span = `previous ${lengthWords(answer)}`;
  if (before <= 0) return `${span}: ${shown(previous.total).usd}`;
  const percent = Math.round(((now - before) * 100) / before);
  if (percent === 0) return `same as ${span}`;
  return `${percent > 0 ? "▲" : "▼"} ${Math.abs(percent)}% vs ${span}`;
}

function tokenParts(entry: Partial<CostEntry> | null | undefined) {
  const t = (entry?.tokens ?? {}) as Partial<CostTokens>;
  return { input: count(t.input), cache_read: count(t.cache_read), all: tokenSum(entry?.tokens) };
}

/** The cache-read share: cache_read / (cache_read + input), a whole
 * percent ("<1%" above zero, ">99%" below all); "—" when neither was read. */
export function cacheShare(entry: Partial<CostEntry> | null | undefined): string {
  const { input, cache_read } = tokenParts(entry);
  const read = cache_read + input;
  if (read <= 0) return "—";
  // Not all of it: never "100%" (99.6% rounds there), as "<1%" is never 0%.
  if (cache_read < read && (cache_read * 100) / read >= 99.5) return ">99%";
  return shareText(cache_read, read) ?? "—";
}

const HOUR_MS = 3_600_000;

/** A time to the minute, UTC: "2026-11-16 06:45"; "" when not a time. */
function utcMinute(ms: number): string {
  const at = new Date(ms);
  if (Number.isNaN(at.getTime())) return "";
  return at.toISOString().slice(0, 16).replace("T", " ");
}

/** A step's label on the axis. The steps run back from the answer's
 * `until_ms` (now), not on clock hours or calendar days (rev-j's n1), so
 * the label is the step's own start, UTC: "06:45" in a step of an hour or
 * less, "11-16" (its start's day) in a longer one, its full span in the
 * bar's title. */
function stepLabel(sinceMs: number, stepMs: number): string {
  const minute = utcMinute(sinceMs);
  if (minute === "") return "";
  return stepMs <= HOUR_MS ? minute.slice(11) : minute.slice(5, 10);
}

/** The trend's bars: each step's height of the costliest step's (a step
 * that cost anything at least 2, so it never looks empty), and its hover:
 * "<start> – <end> UTC: $1.20", to the minute, with its unpriced count
 * when it has one. */
export function trendBars(answer: Partial<CostAnswer> | null | undefined): TrendBar[] {
  const steps = Array.isArray(answer?.trend) ? answer.trend : [];
  const step = finiteOr(answer?.trend_step_ms, HOUR_MS);
  const until = finiteOr(answer?.until_ms, Number.POSITIVE_INFINITY);
  const top = steps.reduce((max, s) => Math.max(max, finiteOr(s?.usd, 0)), 0);
  return steps.map((s) => {
    const usd = finiteOr(s?.usd, 0);
    const unpricedTokens = count(s?.unpriced_tokens);
    const since = finiteOr(s?.since_ms, 0);
    const extra = unpricedText(unpricedTokens);
    return {
      label: stepLabel(since, step),
      title: `${utcMinute(since)} – ${utcMinute(Math.min(since + step, until))} UTC: ${formatUsd(usd)}${extra ? ` ${extra}` : ""}`,
      height: top > 0 && usd > 0 ? Math.max(2, Math.round((usd * 100) / top)) : 0,
      unpriced: unpricedTokens > 0,
    };
  });
}

/** Each mission's daily budget as a gauge, named as the mission rows name
 * it: "$3.20 of $5.00 today (UTC)", "64%"; `over` at or past the budget,
 * the `cost_budget` alert's rule. A budget not above zero is no gauge. */
export function budgetGauges(answer: Partial<CostAnswer> | null | undefined): BudgetGauge[] {
  const budgets = Array.isArray(answer?.budgets) ? answer.budgets : [];
  const names = new Map<number, string | null>();
  for (const m of Array.isArray(answer?.missions) ? answer.missions : []) names.set(m.mission, missionName(m));
  return budgets
    .filter((b) => finiteOr(b?.budget_usd, 0) > 0)
    .map((b) => {
      const budget = finiteOr(b.budget_usd, 0);
      const spent = finiteOr(b.spent_usd, 0);
      const ratio = spent / budget;
      return {
        key: `#${b.mission}`,
        label: names.get(b.mission) ?? null,
        text: `${formatUsd(spent)} of ${formatUsd(budget)} today (UTC)`,
        percent: ratio > 0 && ratio < 0.01 ? "<1%" : `${Math.round(ratio * 100)}%`,
        width: Math.min(100, Math.max(0, ratio * 100)),
        over: spent >= budget,
      };
    });
}

function bars(rowsOf: Row[], dollars: number[], windowUsd: number): RankBar[] {
  return rowsOf.slice(0, TOP_BARS).map((row, index) => ({
    ...row,
    // A row of only unpriced use has no share of the dollars to say: "0%"
    // would read as costing nothing.
    share: row.usd === "unpriced" ? null : row.share,
    width: windowUsd > 0 ? Math.min(100, Math.max(0, (finiteOr(dollars[index], 0) * 100) / windowUsd)) : 0,
  }));
}

/** The dashboard of one window's answer: the four tiles (spend with its
 * change, tokens priced and unpriced, the cache-read share, the cost of a
 * landed task), the trend, the budget gauges, and the top missions and
 * models as bars of their share. */
export function dashboard(answer: Partial<CostAnswer> | null | undefined): Dashboard {
  const total = shown(answer?.total);
  const windowUsd = total.order;
  const { all, input, cache_read } = tokenParts(answer?.total);
  const unpricedTokens = count(answer?.total?.unpriced_tokens);
  const landed = count(answer?.landed);
  const perLandingUsd = answer?.usd_per_landing;
  const missions = rows<CostMission>(answer?.missions, windowUsd, (entry) => `#${entry.mission}`, missionName);
  const models = rows<CostModel>(answer?.models, windowUsd, (entry) => String(entry.model));
  const trend = trendBars(answer);
  const step = finiteOr(answer?.trend_step_ms, HOUR_MS);
  return {
    spend: {
      value: total.usd,
      detail: spendDelta(answer) ?? `last ${lengthWords(answer)}`,
      title: "The window's priced dollars, against the window of equal length just before it",
      unpriced_short: unpricedShort(unpricedTokens),
      unpriced_text: unpricedText(unpricedTokens),
    },
    tokens: {
      value: compactCount(all),
      detail: `${compactCount(Math.max(0, all - unpricedTokens))} priced`,
      title: `${formatTokens(all)} tokens: input, output, cache write and cache read`,
      unpriced_short: unpricedShort(unpricedTokens),
      unpriced_text: unpricedText(unpricedTokens),
    },
    cache: {
      value: cacheShare(answer?.total),
      detail: `${compactCount(cache_read)} of ${compactCount(cache_read + input)} read from cache`,
      title: "Cache-read tokens over cache-read plus input tokens",
      unpriced_short: null,
      unpriced_text: null,
    },
    landing: {
      value: landed > 0 ? formatUsd(finiteOr(perLandingUsd, Number.NaN)) : "—",
      detail: landed > 0 ? `${COUNT.format(landed)} landed` : "no landing in the window",
      title: "The window's priced dollars over the tasks that landed or were accepted in it",
      unpriced_short: null,
      unpriced_text: null,
    },
    trend,
    trend_note: step <= HOUR_MS ? "hours back from now, UTC" : "days back from now, UTC",
    trend_unpriced: trend.filter((bar) => bar.unpriced).length,
    budgets: budgetGauges(answer),
    missions: bars(missions.rows, missions.dollars, windowUsd),
    models: bars(models.rows, models.dollars, windowUsd),
  };
}

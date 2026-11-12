// The Core lines card's model (objective 084): the four lines the owner
// steers by, each north star and its guardrails, from `GET /api/core-lines`
// (crates/spawn-cli/src/serve/core_lines.rs), in words. Pure, and loadable
// with node alone (relative `.ts` imports that load alone too).

import { storageSize } from "./resources.ts";

/** A figure's unit, as the answer names it. */
export type CoreUnit = "bytes" | "bytes_per_day" | "days" | "count" | "ratio" | "ms" | "share" | "usd";

export interface CoreRange {
  min?: number;
  max?: number;
}

/** One figure of `GET /api/core-lines`. */
export interface CoreFigure {
  key: string;
  name: string;
  unit: CoreUnit | string;
  value: number | null;
  range: CoreRange | null;
  state: "in" | "out" | "no_range" | "not_collected" | string;
  /** The last 7 UTC days (objective 090): each day's last sample carrying
   * the figure, oldest first; empty without one. */
  trend?: CoreTrendPoint[];
  /** A figure still measuring its window: when it started (objective 095). */
  measuring_since_ms?: number | null;
  /** The window a figure is read over, when it says one (objective 100:
   * interventions per agent-hour, "24h"). */
  window?: string;
}

/** One day of a figure's trend. */
export interface CoreTrendPoint {
  /** The UTC day, YYYY-MM-DD. */
  day: string;
  value: number;
}

/** A figure's trend on the card: its values, oldest first, and which way
 * it went from the first to the last. */
export interface CoreTrendView {
  points: number[];
  /** "up <n>%", "down <n>%", "steady" under half a percent, or null with
   * fewer than two points or a first point of 0. */
  change: string | null;
}

export interface CoreLine {
  line: string;
  north_star: CoreFigure;
  guardrails: CoreFigure[];
}

/** One row of the recovery table (objective 093): its drill's source
 * ("record <type>", "unit <name>", or null: no drill) and its last pass. */
export interface CoreDrill {
  name: string;
  source: string | null;
  passed_at_ms: number | null;
}

/** `GET /api/core-lines`. */
export interface CoreLinesAnswer {
  lines: CoreLine[];
  ranges_file: string | null;
  ranges_error: string | null;
  ignored_ranges: Array<{ key: string; reason: string }>;
  /** The Core lines file's recoverability rows, in its order. */
  drills?: CoreDrill[];
}

/** A drill on the card, under the recoverability row. */
export interface CoreDrillMark {
  name: string;
  /** "passed MM-DD HH:MM (<span> ago)", "never passed" or "no drill". */
  text: string;
  /** "plain" passed within 7 days, "warning" not, "muted" no drill. */
  tone: "plain" | "warning" | "muted";
}

/** A figure on the card: its words and its tone. */
export interface CoreMark {
  name: string;
  value: string;
  range: string;
  /** "plain" in its range, "warning" out of it, "muted" otherwise. */
  tone: "plain" | "warning" | "muted";
}

export interface CoreRow {
  /** "Performance", "Storage", "Observability" or "Recoverability". */
  line: string;
  north_star: CoreMark;
  guardrails: CoreMark[];
}

export interface CoreLinesView {
  rows: CoreRow[];
  /** "ranges file unreadable: <why>", or null. */
  notice: string | null;
  /** "<key>: <reason>" for each range the file gave that was left out. */
  ignored: string[];
  /** The same, as the Home card says them (never the internal key and
   * reason): "<name>: figure not built yet" for a range of a figure this
   * build does not have, "<name>: range not read (it needs a number min or
   * max)" for one without a bound. */
  pending: string[];
  /** The recovery table's rows, each with its last pass (objective 093). */
  drills: CoreDrillMark[];
}

const WEEK_MS = 7 * 86_400_000;

/** A drill row in words at `now`: its last pass and how long ago, "never
 * passed" with a drill and no pass, "no drill" without one. */
export function drillMark(raw: Partial<CoreDrill> | null | undefined, now: number): CoreDrillMark {
  const name = typeof raw?.name === "string" ? raw.name : "";
  const source = typeof raw?.source === "string" && raw.source !== "" ? raw.source : null;
  const passed = finite(raw?.passed_at_ms);
  if (source === null) return { name, text: "no drill", tone: "muted" };
  if (passed === null) return { name, text: "never passed", tone: "warning" };
  return {
    name,
    text: `passed ${sinceWords(passed)} (${spanWords(Math.max(0, now - passed))} ago)`,
    tone: now - passed <= WEEK_MS ? "plain" : "warning",
  };
}

const LINE_NAMES: Record<string, string> = {
  performance: "Performance",
  storage: "Storage",
  observability: "Observability",
  recoverability: "Recoverability",
};

const GROUPED = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });

function finite(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

const USD = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });

/** Milliseconds as a span (objective 086): under a minute "<s> s"; under an
 * hour "<m> min"; otherwise "<h> h <m> min", or "<h> h" on the hour, the
 * minutes rounded. */
export function spanWords(ms: number): string {
  if (Math.abs(ms) < 60_000) return `${Math.round(ms / 1000)} s`;
  const minutes = Math.round(ms / 60_000);
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0 ? `${GROUPED.format(hours)} h` : `${GROUPED.format(hours)} h ${rest} min`;
}

/** A number in a unit's words: bytes as storageSize ("104.0 GB"), a day's
 * growth "<size> a day", days "1,234 days", a ratio "4.7×" (below one
 * "0.13"), a count grouped ("1,214"), milliseconds as a span ("1 h 32
 * min"), a share as a percent to one decimal ("74.9%"), dollars and cents
 * ("$8.08"). */
export function unitWords(unit: string, value: number): string {
  switch (unit) {
    case "bytes":
      return storageSize(value);
    case "bytes_per_day":
      return `${storageSize(value)} a day`;
    case "days":
      return `${GROUPED.format(value)} days`;
    case "ratio":
      // Below one (interventions per agent-hour) as a plain number with
      // two significant digits, else as a multiple.
      return Math.abs(value) < 1 ? `${Number(value.toPrecision(2))}` : `${value.toFixed(1)}×`;
    case "ms":
      return spanWords(value);
    case "share":
      return `${(value * 100).toFixed(1)}%`;
    case "usd":
      return USD.format(value);
    default:
      return GROUPED.format(value);
  }
}

/** A bound as core-lines.json wrote it: a byte bound that is a whole
 * number of decimal megabytes reads in decimal units ("500 MB" for
 * 500000000, "1.5 GB" for 1500000000), not pushed through storageSize's
 * 1024s ("476.8 MB"); any other bound reads as unitWords says it. */
function boundWords(unit: string, bound: number): string {
  if ((unit === "bytes" || unit === "bytes_per_day") && bound !== 0 && bound % 1e6 === 0) {
    const size = Math.abs(bound) >= 1e9 ? `${bound / 1e9} GB` : `${bound / 1e6} MB`;
    return unit === "bytes_per_day" ? `${size} a day` : size;
  }
  return unitWords(unit, bound);
}

/** A range in its unit's words: "<min> or more", "<max> or less", "between
 * <min> and <max>", or "no range set"; each bound as the file wrote it
 * (boundWords). */
export function rangeWords(unit: string, range: CoreRange | null | undefined): string {
  const min = finite(range?.min);
  const max = finite(range?.max);
  if (min !== null && max !== null) return `between ${boundWords(unit, min)} and ${boundWords(unit, max)}`;
  if (min !== null) return `${boundWords(unit, min)} or more`;
  if (max !== null) return `${boundWords(unit, max)} or less`;
  return "no range set";
}

const two = (n: number) => String(n).padStart(2, "0");

/** A moment as "MM-DD HH:MM", local time. */
export function sinceWords(ms: number): string {
  const at = new Date(ms);
  return `${two(at.getMonth() + 1)}-${two(at.getDate())} ${two(at.getHours())}:${two(at.getMinutes())}`;
}

/** A figure's window beside its name: " (last 24 h)" for "24h", " (last 7
 * days)" for "7d", nothing without one. */
function windowWords(window: unknown): string {
  if (typeof window !== "string" || window === "") return "";
  const hours = /^(\d+)h$/.exec(window);
  if (hours) return ` (last ${hours[1]} h)`;
  const days = /^(\d+)d$/.exec(window);
  if (days) return ` (last ${days[1]} ${days[1] === "1" ? "day" : "days"})`;
  return ` (${window})`;
}

function mark(raw: unknown): CoreMark {
  const figure = (typeof raw === "object" && raw !== null ? raw : {}) as Partial<CoreFigure>;
  const unit = typeof figure.unit === "string" ? figure.unit : "count";
  const value = finite(figure.value);
  const since = finite(figure.measuring_since_ms);
  return {
    name: `${typeof figure.name === "string" ? figure.name : String(figure.key ?? "")}${windowWords(figure.window)}`,
    value:
      value !== null ? unitWords(unit, value) : since !== null ? `measuring since ${sinceWords(since)}` : "not collected yet",
    range: rangeWords(unit, figure.range),
    tone: figure.state === "in" ? "plain" : figure.state === "out" ? "warning" : "muted",
  };
}

/** An ignored range as the owner reads it: the figure's name (its own, or
 * its key's last part in words) and what is missing. */
function pendingWords(entry: { key?: unknown; reason?: unknown } | null | undefined, names: Map<string, string>): string {
  const key = String(entry?.key ?? "");
  // A test database URL file the reader refused (objective 094): its name
  // and why, never its URL (the answer holds none).
  const urlFile = "storage.test_database_url_files.";
  if (key.startsWith(urlFile)) {
    return `test database URL file ${key.slice(urlFile.length)} not used: ${String(entry?.reason ?? "refused")}`;
  }
  const known = names.get(key);
  if (known !== undefined) return `${known}: range not read (it needs a number min or max)`;
  const words = key.slice(key.lastIndexOf(".") + 1).replace(/_/g, " ");
  return `${words}: figure not built yet`;
}

/** The card's words for an answer of `GET /api/core-lines`. */
export function coreLinesView(answer: Partial<CoreLinesAnswer> | null | undefined, now: number = Date.now()): CoreLinesView {
  const lines = Array.isArray(answer?.lines) ? answer.lines : [];
  const error = typeof answer?.ranges_error === "string" && answer.ranges_error !== "" ? answer.ranges_error : null;
  const ignored = Array.isArray(answer?.ignored_ranges) ? answer.ignored_ranges : [];
  const names = new Map<string, string>();
  for (const line of lines) {
    const figures = [line?.north_star, ...(Array.isArray(line?.guardrails) ? line.guardrails : [])];
    for (const figure of figures) {
      if (typeof figure?.key === "string" && typeof figure?.name === "string") names.set(figure.key, figure.name);
    }
  }
  return {
    rows: lines.map((line) => ({
      line: LINE_NAMES[line?.line] ?? String(line?.line ?? ""),
      north_star: mark(line?.north_star),
      guardrails: Array.isArray(line?.guardrails) ? line.guardrails.map(mark) : [],
    })),
    notice: error === null ? null : `ranges file unreadable: ${error}`,
    ignored: ignored.map((entry) => `${entry?.key}: ${entry?.reason}`),
    pending: ignored.map((entry) => pendingWords(entry, names)),
    drills: (Array.isArray(answer?.drills) ? answer.drills : []).map((drill) => drillMark(drill, now)),
  };
}

/** A figure's trend in words (objective 090): its points' values, and the
 * change from the first point to the last, rounded, relative to the first
 * point's size. */
export function trendView(figure: Partial<CoreFigure> | null | undefined): CoreTrendView {
  const trend = Array.isArray(figure?.trend) ? figure.trend : [];
  const points = trend.map((point) => finite(point?.value)).filter((value): value is number => value !== null);
  if (points.length < 2 || points[0] === 0) return { points, change: null };
  const first = points[0];
  const last = points[points.length - 1];
  const percent = ((last - first) / Math.abs(first)) * 100;
  if (Math.abs(percent) < 0.5) return { points, change: "steady" };
  const rounded = Math.round(Math.abs(percent));
  return { points, change: `${percent > 0 ? "up" : "down"} ${rounded}%` };
}

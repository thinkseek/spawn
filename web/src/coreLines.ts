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

/** `GET /api/core-lines`. */
export interface CoreLinesAnswer {
  lines: CoreLine[];
  ranges_file: string | null;
  ranges_error: string | null;
  ignored_ranges: Array<{ key: string; reason: string }>;
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
 * growth "<size> a day", days "1,234 days", a ratio "4.7×", a count
 * grouped ("1,214"), milliseconds as a span ("1 h 32 min"), a share as a
 * percent to one decimal ("74.9%"), dollars and cents ("$8.08"). */
export function unitWords(unit: string, value: number): string {
  switch (unit) {
    case "bytes":
      return storageSize(value);
    case "bytes_per_day":
      return `${storageSize(value)} a day`;
    case "days":
      return `${GROUPED.format(value)} days`;
    case "ratio":
      return `${value.toFixed(1)}×`;
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

/** A range in its unit's words: "<min> or more", "<max> or less", "between
 * <min> and <max>", or "no range set". */
export function rangeWords(unit: string, range: CoreRange | null | undefined): string {
  const min = finite(range?.min);
  const max = finite(range?.max);
  if (min !== null && max !== null) return `between ${unitWords(unit, min)} and ${unitWords(unit, max)}`;
  if (min !== null) return `${unitWords(unit, min)} or more`;
  if (max !== null) return `${unitWords(unit, max)} or less`;
  return "no range set";
}

function mark(raw: unknown): CoreMark {
  const figure = (typeof raw === "object" && raw !== null ? raw : {}) as Partial<CoreFigure>;
  const unit = typeof figure.unit === "string" ? figure.unit : "count";
  const value = finite(figure.value);
  return {
    name: typeof figure.name === "string" ? figure.name : String(figure.key ?? ""),
    value: value === null ? "not collected yet" : unitWords(unit, value),
    range: rangeWords(unit, figure.range),
    tone: figure.state === "in" ? "plain" : figure.state === "out" ? "warning" : "muted",
  };
}

/** The card's words for an answer of `GET /api/core-lines`. */
export function coreLinesView(answer: Partial<CoreLinesAnswer> | null | undefined): CoreLinesView {
  const lines = Array.isArray(answer?.lines) ? answer.lines : [];
  const error = typeof answer?.ranges_error === "string" && answer.ranges_error !== "" ? answer.ranges_error : null;
  const ignored = Array.isArray(answer?.ignored_ranges) ? answer.ignored_ranges : [];
  return {
    rows: lines.map((line) => ({
      line: LINE_NAMES[line?.line] ?? String(line?.line ?? ""),
      north_star: mark(line?.north_star),
      guardrails: Array.isArray(line?.guardrails) ? line.guardrails.map(mark) : [],
    })),
    notice: error === null ? null : `ranges file unreadable: ${error}`,
    ignored: ignored.map((entry) => `${entry?.key}: ${entry?.reason}`),
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

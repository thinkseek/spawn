// Read model of the scorecard: `/api/scorecard?mission=&since=&until=`.
// Every field is normalized, so a server that omits one renders an empty
// value instead of crashing. Read-only: this module only ever issues GET
// requests. The helpers below the fetch are pure (no React), so the view
// stays a thin layer over them.

import { apiError, apiHeaders } from "./api";

// ---------------------------------------------------------------------------
// Types: exactly the JSON the server answers.

export const DIMENSION_KEYS = [
  "throughput",
  "lead_time",
  "first_pass",
  "rework",
  "autonomy",
  "reliability",
  "cost",
  "gate_time",
] as const;
export type DimensionKey = (typeof DIMENSION_KEYS)[number];

export type Better = "higher" | "lower";
export type Trend = "better" | "worse" | "same";

/** What every dimension carries. Below `min_n` (`enough` false) `value`
 * is null; `previous` and `trend` may be null at any time (always null in
 * the per-day breakdown). */
export interface DimensionBase {
  value: number | null;
  /** Landings in the window (gate runs for gate time). */
  n: number;
  enough: boolean;
  better: Better;
  previous: number | null;
  trend: Trend | null;
}

export interface Throughput extends DimensionBase {
  landed: number;
  open_hours: number;
}

export interface LeadTime extends DimensionBase {
  /** Milliseconds; null below `min_n`. */
  p90: number | null;
}

export interface FirstPass extends DimensionBase {
  first_pass: number;
}

export interface Rework extends DimensionBase {
  send_backs: number;
  by_verdict: Record<string, number>;
}

export interface Autonomy extends DimensionBase {
  interventions: number;
  by_kind: Record<string, number>;
}

export interface Reliability extends DimensionBase {
  events: number;
  by_kind: Record<string, number>;
}

export interface ModelCost {
  tokens: number;
  /** Tokens per landing; null below `min_n`. */
  per_landed: number | null;
}

export interface Cost extends DimensionBase {
  tokens: number;
  by_model: Record<string, ModelCost>;
  gate_ms: number;
  /** Null below `min_n`. */
  gate_ms_per_landed: number | null;
}

export interface GateTime extends DimensionBase {
  runs: number;
}

export interface Dimensions {
  throughput: Throughput;
  lead_time: LeadTime;
  first_pass: FirstPass;
  rework: Rework;
  autonomy: Autonomy;
  reliability: Reliability;
  cost: Cost;
  gate_time: GateTime;
}

export interface ScorecardDay {
  /** A UTC day, `YYYY-MM-DD`. */
  day: string;
  dimensions: Dimensions;
}

export interface ScorecardScope {
  scope: "fleet" | "mission";
  mission: number | null;
  dimensions: Dimensions;
  days: ScorecardDay[];
}

export interface Scorecard {
  since_ms: number | null;
  until_ms: number | null;
  previous_since_ms: number | null;
  min_n: number;
  scopes: ScorecardScope[];
}

// ---------------------------------------------------------------------------
// Normalization

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : {};
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function text(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function number(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function counts(value: unknown): Record<string, number> {
  const out: Record<string, number> = {};
  for (const [key, count] of Object.entries(object(value))) {
    out[key] = number(count);
  }
  return out;
}

/** Throughput and first-pass quality go up when things go well; the rest
 * are costs. Used only when the server does not say. */
const DEFAULT_BETTER: Record<DimensionKey, Better> = {
  throughput: "higher",
  lead_time: "lower",
  first_pass: "higher",
  rework: "lower",
  autonomy: "lower",
  reliability: "lower",
  cost: "lower",
  gate_time: "lower",
};

function normalizeBase(raw: JsonObject, key: DimensionKey): DimensionBase {
  // A missing `enough` reads as false: never a number without the server
  // saying there is enough data behind it.
  const enough = raw.enough === true;
  const trend = raw.trend;
  return {
    value: enough ? optionalNumber(raw.value) : null,
    n: number(raw.n),
    enough,
    better:
      raw.better === "higher" || raw.better === "lower"
        ? raw.better
        : DEFAULT_BETTER[key],
    previous: optionalNumber(raw.previous),
    trend:
      trend === "better" || trend === "worse" || trend === "same" ? trend : null,
  };
}

export function normalizeDimensions(value: unknown): Dimensions {
  const raw = object(value);
  const part = (key: DimensionKey) => object(raw[key]);
  const base = (key: DimensionKey) => normalizeBase(part(key), key);
  // Per-landing parts follow `value`: null below `min_n`.
  const perLanding = (key: DimensionKey, field: unknown) =>
    part(key).enough === true ? optionalNumber(field) : null;

  const throughput = part("throughput");
  const leadTime = part("lead_time");
  const firstPass = part("first_pass");
  const rework = part("rework");
  const autonomy = part("autonomy");
  const reliability = part("reliability");
  const cost = part("cost");
  const gateTime = part("gate_time");

  const byModel: Record<string, ModelCost> = {};
  for (const [model, entry] of Object.entries(object(cost.by_model))) {
    const modelCost = object(entry);
    byModel[model] = {
      tokens: number(modelCost.tokens),
      per_landed: perLanding("cost", modelCost.per_landed),
    };
  }

  return {
    throughput: {
      ...base("throughput"),
      landed: number(throughput.landed),
      open_hours: number(throughput.open_hours),
    },
    lead_time: {
      ...base("lead_time"),
      p90: perLanding("lead_time", leadTime.p90),
    },
    first_pass: {
      ...base("first_pass"),
      first_pass: number(firstPass.first_pass),
    },
    rework: {
      ...base("rework"),
      send_backs: number(rework.send_backs),
      by_verdict: counts(rework.by_verdict),
    },
    autonomy: {
      ...base("autonomy"),
      interventions: number(autonomy.interventions),
      by_kind: counts(autonomy.by_kind),
    },
    reliability: {
      ...base("reliability"),
      events: number(reliability.events),
      by_kind: counts(reliability.by_kind),
    },
    cost: {
      ...base("cost"),
      tokens: number(cost.tokens),
      by_model: byModel,
      gate_ms: number(cost.gate_ms),
      gate_ms_per_landed: perLanding("cost", cost.gate_ms_per_landed),
    },
    gate_time: {
      ...base("gate_time"),
      runs: number(gateTime.runs),
    },
  };
}

export function normalizeScorecard(value: unknown): Scorecard {
  const raw = object(value);
  return {
    since_ms: optionalNumber(raw.since_ms),
    until_ms: optionalNumber(raw.until_ms),
    previous_since_ms: optionalNumber(raw.previous_since_ms),
    min_n: number(raw.min_n),
    scopes: list(raw.scopes).map((entry) => {
      const scope = object(entry);
      return {
        scope: scope.scope === "mission" ? "mission" : "fleet",
        mission: optionalNumber(scope.mission),
        dimensions: normalizeDimensions(scope.dimensions),
        days: list(scope.days).map((day) => {
          const rawDay = object(day);
          return {
            day: text(rawDay.day),
            dimensions: normalizeDimensions(rawDay.dimensions),
          };
        }),
      };
    }),
  };
}

// ---------------------------------------------------------------------------
// Fetch

export interface ScorecardQuery {
  /** A mission id; empty for every scope. */
  mission?: number | string | null;
  /** RFC 3339, or a span back from now such as `36h` or `7d`. */
  since?: string | null;
  until?: string | null;
}

/** The query string of a scorecard read: each parameter URL-encoded, the
 * empty ones left out. "" when there are none, else starting with `?`. */
export function scorecardQuery(query: ScorecardQuery): string {
  const parts: string[] = [];
  for (const key of ["mission", "since", "until"] as const) {
    const value = query[key];
    if (value === null || value === undefined) continue;
    const shown = String(value).trim();
    if (shown === "") continue;
    parts.push(`${key}=${encodeURIComponent(shown)}`);
  }
  return parts.length === 0 ? "" : `?${parts.join("&")}`;
}

/** GET /api/scorecard. A 400 carries the server's reason
 * (`HTTP 400: bad request: …`). */
export async function fetchScorecard(
  query: ScorecardQuery,
  signal: AbortSignal,
): Promise<Scorecard> {
  return readApi(`/api/scorecard${scorecardQuery(query)}`, signal, normalizeScorecard);
}

// ---------------------------------------------------------------------------
// Meaning: labels and formatting. Pure, no React.

export interface DimensionInfo {
  label: string;
  /** Column heading of the per-day table. */
  short: string;
  /** What the value counts. */
  unit: string;
}

export const DIMENSION_INFO: Record<DimensionKey, DimensionInfo> = {
  throughput: { label: "Throughput", short: "Throughput", unit: "landings per open hour" },
  lead_time: { label: "Lead time", short: "Lead time", unit: "per landing" },
  first_pass: { label: "First-pass quality", short: "First pass", unit: "of landings, first pass" },
  rework: { label: "Rework", short: "Rework", unit: "send-backs per landing" },
  autonomy: { label: "Autonomy", short: "Autonomy", unit: "interventions per landing" },
  reliability: { label: "Reliability", short: "Reliability", unit: "events per landing" },
  cost: { label: "Cost", short: "Cost", unit: "tokens per landing" },
  gate_time: { label: "Gate time", short: "Gate time", unit: "per gate run" },
};

export function directionLabel(better: Better): string {
  return better === "higher" ? "higher is better" : "lower is better";
}

/** What `n` counts: landings, or gate runs for gate time. */
export function nUnit(key: DimensionKey, n: number): string {
  if (key === "gate_time") return n === 1 ? "gate run" : "gate runs";
  return n === 1 ? "landing" : "landings";
}

/** Exactly what a dimension below the minimum reads. */
export function notEnoughText(n: number, minN: number): string {
  return `not enough data (n ${n}, minimum ${minN})`;
}

/** A plain number with at most `digits` decimals, grouped: 2,033.333. */
export function formatDecimal(value: number | null, digits = 3): string {
  if (value === null || !Number.isFinite(value)) return "—";
  return value.toLocaleString("en-US", { maximumFractionDigits: digits });
}

/** Milliseconds as hours, the unit the titles spell out: "5.00 h". */
export function formatHours(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return "—";
  return `${(ms / 3_600_000).toFixed(2)} h`;
}

/** Milliseconds as a short span: "42s", "8m 16s", "48m", "5h", "52h 10m".
 * Hours keep counting past a day, so lead times compare at a glance. */
export function formatSpan(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return "—";
  const negative = ms < 0;
  const abs = Math.abs(ms);
  const sign = negative ? "-" : "";
  if (abs < 1_000) return `${sign}${Math.round(abs)}ms`;
  const seconds = Math.round(abs / 1_000);
  if (seconds < 60) return `${sign}${seconds}s`;
  if (seconds < 600) {
    const rest = seconds % 60;
    return `${sign}${Math.floor(seconds / 60)}m${rest ? ` ${rest}s` : ""}`;
  }
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${sign}${minutes}m`;
  const rest = minutes % 60;
  return `${sign}${Math.floor(minutes / 60)}h${rest ? ` ${rest}m` : ""}`;
}

/** Whole milliseconds with their hours, for a title: "18,000,000 ms (5.00 h)". */
export function msTitle(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return "";
  return `${formatDecimal(ms, 0)} ms (${formatHours(ms)})`;
}

/** A dimension's value in its unit. Durations as a span, first-pass as a
 * percentage, cost as whole tokens, the rest as decimals. */
export function formatValue(key: DimensionKey, value: number | null): string {
  if (value === null || !Number.isFinite(value)) return "—";
  switch (key) {
    case "lead_time":
    case "gate_time":
      return formatSpan(value);
    case "first_pass":
      return `${formatDecimal(value * 100, 1)}%`;
    case "throughput":
      return `${formatDecimal(value, 3)}/h`;
    case "cost":
      return formatDecimal(value, 0);
    case "rework":
    case "autonomy":
    case "reliability":
      return formatDecimal(value, 3);
  }
}

/** The raw number behind a formatted value, for its title. */
export function rawTitle(key: DimensionKey, value: number | null): string {
  if (value === null || !Number.isFinite(value)) return "";
  if (key === "lead_time" || key === "gate_time") return msTitle(value);
  if (key === "first_pass") return `share ${value}`;
  return String(value);
}

export interface Shown {
  text: string;
  /** The raw value (or why there is none), for a title attribute. */
  title: string;
  enough: boolean;
}

/** A dimension's headline. Below the minimum it is the words, never a
 * number. */
export function showValue(
  key: DimensionKey,
  dimension: DimensionBase,
  minN: number,
): Shown {
  if (!dimension.enough) {
    const words = notEnoughText(dimension.n, minN);
    return { text: words, title: words, enough: false };
  }
  return {
    text: formatValue(key, dimension.value),
    title: rawTitle(key, dimension.value),
    enough: true,
  };
}

/** A per-day cell: compact, so below the minimum it is "—" with the words
 * in its title. */
export function showCell(
  key: DimensionKey,
  dimension: DimensionBase,
  minN: number,
): Shown {
  if (!dimension.enough) {
    return { text: "—", title: notEnoughText(dimension.n, minN), enough: false };
  }
  const raw = rawTitle(key, dimension.value);
  const n = `n ${dimension.n} ${nUnit(key, dimension.n)}`;
  return {
    text: formatValue(key, dimension.value),
    title: raw ? `${raw} · ${n}` : n,
    enough: true,
  };
}

export type TrendTone = "good" | "bad" | "flat";

export interface TrendMark {
  /** Which way the value moved. */
  arrow: "↑" | "↓" | "→";
  tone: TrendTone;
  /** The trend in words; colour never stands alone. */
  label: string;
}

/** The arrow and colour of a trend. The arrow is the way the value moved,
 * the tone whether that was good: a throughput that got worse went down,
 * a lead time that got worse went up. Null trend, no mark. */
export function trendMark(trend: Trend | null, better: Better): TrendMark | null {
  if (trend === null) return null;
  if (trend === "same") return { arrow: "→", tone: "flat", label: "same" };
  const up = (trend === "better") === (better === "higher");
  return {
    arrow: up ? "↑" : "↓",
    tone: trend === "better" ? "good" : "bad",
    label: trend,
  };
}

/** A key of a `by_*` map in words: `owner_task` → "owner task". */
export function partLabel(key: string): string {
  return key.replace(/_/g, " ");
}

export interface Part {
  label: string;
  value: string;
  title?: string;
}

export interface PartGroup {
  /** Null for the dimension's own counts; else the map it breaks down. */
  title: string | null;
  parts: Part[];
}

function countParts(map: Record<string, number>): Part[] {
  return Object.entries(map).map(([key, count]) => ({
    label: partLabel(key),
    value: formatDecimal(count, 3),
    title: key,
  }));
}

/** What a dimension is made of, as label/value rows. */
export function dimensionParts(
  key: DimensionKey,
  dimensions: Dimensions,
): PartGroup[] {
  switch (key) {
    case "throughput": {
      const d = dimensions.throughput;
      return [
        {
          title: null,
          parts: [
            { label: "landed", value: formatDecimal(d.landed, 0) },
            {
              label: "open hours",
              value: d.open_hours.toLocaleString("en-US", {
                minimumFractionDigits: 1,
                maximumFractionDigits: 1,
              }),
              title: String(d.open_hours),
            },
          ],
        },
      ];
    }
    case "lead_time": {
      const d = dimensions.lead_time;
      return [
        {
          title: null,
          parts: [{ label: "p90", value: formatSpan(d.p90), title: msTitle(d.p90) }],
        },
      ];
    }
    case "first_pass": {
      const d = dimensions.first_pass;
      return [
        {
          title: null,
          parts: [
            {
              label: "first pass",
              value: `${formatDecimal(d.first_pass, 0)} of ${formatDecimal(d.n, 0)}`,
            },
          ],
        },
      ];
    }
    case "rework": {
      const d = dimensions.rework;
      return [
        {
          title: null,
          parts: [{ label: "send-backs", value: formatDecimal(d.send_backs, 0) }],
        },
        { title: "by verdict", parts: countParts(d.by_verdict) },
      ];
    }
    case "autonomy": {
      const d = dimensions.autonomy;
      return [
        {
          title: null,
          parts: [{ label: "interventions", value: formatDecimal(d.interventions, 0) }],
        },
        { title: "by kind", parts: countParts(d.by_kind) },
      ];
    }
    case "reliability": {
      const d = dimensions.reliability;
      return [
        {
          title: null,
          parts: [{ label: "events", value: formatDecimal(d.events, 0) }],
        },
        { title: "by kind", parts: countParts(d.by_kind) },
      ];
    }
    case "cost": {
      const d = dimensions.cost;
      return [
        {
          title: null,
          parts: [
            { label: "tokens", value: formatDecimal(d.tokens, 0) },
            { label: "gate time", value: formatSpan(d.gate_ms), title: msTitle(d.gate_ms) },
            {
              label: "gate time per landing",
              value: formatSpan(d.gate_ms_per_landed),
              title: msTitle(d.gate_ms_per_landed),
            },
          ],
        },
        {
          title: "by model",
          parts: Object.entries(d.by_model).map(([model, entry]) => ({
            label: model,
            value: `${formatDecimal(entry.tokens, 0)} tokens · ${
              entry.per_landed === null ? "—" : formatDecimal(entry.per_landed, 0)
            } per landing`,
            title:
              entry.per_landed === null
                ? `${model}: ${entry.tokens} tokens`
                : `${model}: ${entry.tokens} tokens, ${entry.per_landed} per landing`,
          })),
        },
      ];
    }
    case "gate_time": {
      const d = dimensions.gate_time;
      return [
        {
          title: null,
          parts: [{ label: "runs", value: formatDecimal(d.runs, 0) }],
        },
      ];
    }
  }
}

/** An epoch-millisecond time as a UTC ISO string, whole seconds without
 * the `.000`: "2026-10-02T00:00:00Z". */
export function isoUtc(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return "—";
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toISOString().replace(".000Z", "Z");
}

/** How a scope is named: "Fleet", or "Mission #2". */
export function scopeLabel(scope: Pick<ScorecardScope, "scope" | "mission">): string {
  if (scope.scope === "fleet") return "Fleet";
  return scope.mission === null ? "Mission" : `Mission #${scope.mission}`;
}

/** The windows the picker offers, passed to the server as `since`. */
export const WINDOW_PRESETS = ["24h", "7d", "30d", "90d"] as const;
export type WindowPreset = (typeof WINDOW_PRESETS)[number];

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
  /** `not_measured_yet` (objective 176): a figure of the owner's set no
   * code computes yet, its value null. */
  state: "in" | "out" | "no_range" | "not_collected" | "not_measured_yet" | string;
  /** The line an alert figure, or a diagnostic that still raises, is
   * under (objective 176). */
  line?: string;
  /** The last 7 UTC days (objective 090): each day's last sample carrying
   * the figure, oldest first; empty without one. */
  trend?: CoreTrendPoint[];
  /** A figure still measuring its window: when it started (objective 095). */
  measuring_since_ms?: number | null;
  /** The window a figure is read over, when it says one (objective 100:
   * interventions per agent-hour, "24h"). */
  window?: string;
  /** The store a figure names (objective 141: the store that grew most),
   * said after its name. */
  store?: string;
  /** What a judged share is read from (objective 148: "Needs you is
   * right", the needs fired in 7 days), said after its name. */
  judged?: CoreJudged;
  /** Each mission's figure beside the fleet's (objective 159: the
   * performance north star's report to live by mission), by mission;
   * absent when no mission has a live landing. */
  by_mission?: CoreMissionFigure[];
  /** The ranges file's target (objective 181): said beside the figure,
   * never judged against. */
  target?: number;
  /** What a count is out of (objective 181: U1 out of the recovery
   * table's rows): "<n> of <of>". */
  of?: number;
  /** Why the file's range is held (objective 181: U2 in its first week of
   * judged data): said where the range would be. */
  range_note?: string;
}

/** One mission's report to live (objective 159): its median (null below
 * the Scorecard's minimum), how many of its landings are live, and how many
 * of those were live when they landed (no deploy ships its source). */
export interface CoreMissionFigure {
  mission: number;
  value: number | null;
  n: number;
  live_at_landing: number;
}

/** A mission's report to live on the card. */
export interface CoreMissionMark {
  /** "mission <id>". */
  name: string;
  /** "<span> (<n> live)", or "<n> live, too few for a median" without a
   * value; then " · live when landed (no deploy ships it)" when every live
   * landing was. */
  text: string;
  /** "muted" when every live landing was live when it landed (its figure
   * is no deploy's wait), else "plain". */
  tone: "plain" | "muted";
}

/** The needs a judged share is read from: fired, judged and not. */
export interface CoreJudged {
  fired: number;
  judged: number;
  unjudged: number;
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
  /** The line's plain readings (objective 182): they raise nothing.
   * Dollars per landing takes its configured range while use with no price
   * is a fresh number under 10% (objective 207), and otherwise has no range
   * and its `range_note`. Empty for most lines. */
  readings?: CoreFigure[];
}

/** One row of the recovery table (objective 093): its drill's source
 * ("record <type>", "unit <name>", or null: no drill) and its last pass. */
export interface CoreDrill {
  name: string;
  source: string | null;
  passed_at_ms: number | null;
  /** Its newest drill's own result, pass or fail (objective 105), or null. */
  last?: CoreDrillResult | null;
}

/** A drill's own result: when it ran, whether it passed, and why not. */
export interface CoreDrillResult {
  at_ms: number | null;
  ok: boolean;
  reason: string;
}

/** `GET /api/core-lines`. */
export interface CoreLinesAnswer {
  lines: CoreLine[];
  ranges_file: string | null;
  ranges_error: string | null;
  ignored_ranges: Array<{ key: string; reason: string }>;
  /** The figures demoted to an alert only, each with its line (objective
   * 176). */
  alert_figures?: CoreFigure[];
  /** The storage page's diagnostics, in order (objective 176). */
  diagnostics?: CoreFigure[];
  /** The Core lines file's recoverability rows, in its order. */
  drills?: CoreDrill[];
  /** The scratch walk (objective 094); `claimed`, every claimed folder by
   * owner (objective 140). */
  scratch?: { claimed?: CoreClaimed[] };
}

/** A claimed scratch folder (objective 140, `spawn scratch claim`). */
export interface CoreClaimed {
  path: string;
  agent: string;
  /** "<mission>/<NNN>". */
  objective: string;
  created_ms: number;
  lease_until_ms: number;
  /** Its bytes; null while its lease runs (never walked whole, objective
   * 155). */
  bytes: number | null;
  /** Its build paths' bytes, reached through real directories only. */
  build_bytes: number;
  /** Whether its objective landed; null when the store was not read. */
  landed: boolean | null;
}

/** A claimed folder on the card's Storage row. */
export interface CoreScratchMark {
  /** "<agent> · <objective>". */
  name: string;
  /** "<size> (build <size>) · <age> old · lease until MM-DD HH:MM" or
   * "· lease ended", then "· landed", "· not landed" or "· landing not
   * read". */
  text: string;
  /** "warning" when it landed and its lease ended (build output left to
   * remove), else "plain". */
  tone: "plain" | "warning";
  /** The folder's path (the row's title). */
  detail: string;
}

/** A drill on the card, under the recoverability row. */
export interface CoreDrillMark {
  name: string;
  /** "passed MM-DD HH:MM (<span> ago)", "never passed" or "no drill";
   * "failed MM-DD HH:MM (<span> ago); last passed ..." (or "; never
   * passed") when its newest drill failed (objective 112). */
  text: string;
  /** "plain" passed within 7 days, "warning" not or newest failed, "muted"
   * no drill. */
  tone: "plain" | "warning" | "muted";
  /** A failed newest drill's reason (the row's title), else "". */
  detail: string;
}

/** A figure on the card: its words and its tone. */
export interface CoreMark {
  name: string;
  value: string;
  /** The range in words, or the note that holds it (objective 181). */
  range: string;
  /** "target <value in the unit's words>" (objective 181); absent without
   * one. */
  target?: string;
  /** "plain" in its range, "warning" out of it, "muted" otherwise. */
  tone: "plain" | "warning" | "muted";
}

export interface CoreRow {
  /** "Storage", "Performance" or "Sustainability" (objective 176). */
  line: string;
  north_star: CoreMark;
  guardrails: CoreMark[];
  /** The line's plain readings (objective 182), shown under its
   * guardrails: their range, or the note where the range would be
   * (objective 207). */
  readings: CoreMark[];
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
   * max)" for one without a bound, "<name>: <reason>" for one the set
   * ignores on purpose (objective 182). */
  pending: string[];
  /** The recovery table's rows, each with its last pass (objective 093). */
  drills: CoreDrillMark[];
  /** Scratch by owner (objective 140): each claimed folder, as the answer
   * lists them; empty when none is claimed. */
  scratch: CoreScratchMark[];
  /** Report to live by mission (objective 159): the performance line's
   * north star `by_mission`, in words; empty without it. */
  byMission: CoreMissionMark[];
  /** The storage page's diagnostics in words (objective 176), as the
   * answer lists them; on no row. */
  diagnostics: CoreMark[];
}

const WEEK_MS = 7 * 86_400_000;

/** A drill row in words at `now`: its last pass and how long ago, "never
 * passed" with a drill and no pass, "no drill" without one. */
export function drillMark(raw: Partial<CoreDrill> | null | undefined, now: number): CoreDrillMark {
  const name = typeof raw?.name === "string" ? raw.name : "";
  const source = typeof raw?.source === "string" && raw.source !== "" ? raw.source : null;
  const passed = finite(raw?.passed_at_ms);
  if (source === null) return { name, text: "no drill", tone: "muted", detail: "" };
  const ago = (at: number) => `${sinceWords(at)} (${spanWords(Math.max(0, now - at))} ago)`;
  const last = raw?.last;
  if (last && typeof last === "object" && last.ok === false) {
    const failedAt = finite(last.at_ms);
    const failed = failedAt === null ? "failed (time unknown)" : `failed ${ago(failedAt)}`;
    return {
      name,
      text: `${failed}; ${passed === null ? "never passed" : `last passed ${ago(passed)}`}`,
      tone: "warning",
      detail: typeof last.reason === "string" ? last.reason : "",
    };
  }
  if (passed === null) return { name, text: "never passed", tone: "warning", detail: "" };
  return { name, text: `passed ${ago(passed)}`, tone: now - passed <= WEEK_MS ? "plain" : "warning", detail: "" };
}

/** A claimed scratch folder in words at `now` (objective 140). A folder
 * whose lease runs is never walked whole (objective 155): its bytes are
 * null, and it says its build paths' size only. */
export function scratchMark(raw: Partial<CoreClaimed> | null | undefined, now: number): CoreScratchMark {
  const agent = typeof raw?.agent === "string" ? raw.agent : "";
  const objective = typeof raw?.objective === "string" ? raw.objective : "";
  const created = finite(raw?.created_ms);
  const lease = finite(raw?.lease_until_ms);
  const leased = lease !== null && lease > now;
  const landed = raw?.landed === true ? "landed" : raw?.landed === false ? "not landed" : "landing not read";
  const bytes = finite(raw?.bytes);
  const build = storageSize(finite(raw?.build_bytes) ?? 0);
  const parts = [
    bytes === null ? `build ${build} (not walked whole while leased)` : `${storageSize(bytes)} (build ${build})`,
    `${created === null ? "?" : spanWords(Math.max(0, now - created))} old`,
    leased ? `lease until ${sinceWords(lease)}` : "lease ended",
    landed,
  ];
  return {
    name: `${agent} · ${objective}`,
    text: parts.join(" · "),
    tone: raw?.landed === true && !leased ? "warning" : "plain",
    detail: typeof raw?.path === "string" ? raw.path : "",
  };
}

/** A mission's report to live in words (objective 159). */
export function missionMark(raw: Partial<CoreMissionFigure> | null | undefined): CoreMissionMark {
  const mission = finite(raw?.mission);
  const value = finite(raw?.value);
  const n = finite(raw?.n) ?? 0;
  const atLanding = finite(raw?.live_at_landing) ?? 0;
  const live = value === null ? `${n} live, too few for a median` : `${spanWords(value)} (${n} live)`;
  const landed = n > 0 && atLanding === n;
  return {
    name: `mission ${mission === null ? "?" : mission}`,
    text: landed ? `${live} · live when landed (no deploy ships it)` : live,
    tone: landed ? "muted" : "plain",
  };
}

const LINE_NAMES: Record<string, string> = {
  storage: "Storage",
  performance: "Performance",
  sustainability: "Sustainability",
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

/** The store a figure names, after its name: ": <store>", nothing without
 * one. */
function storeWords(store: unknown): string {
  return typeof store === "string" && store !== "" ? `: ${store}` : "";
}

/** What a judged share is read from, after its name: " (5 judged, 1
 * unjudged of 6)", nothing without it. */
function judgedWords(judged: unknown): string {
  if (typeof judged !== "object" || judged === null) return "";
  const counts = judged as Partial<CoreJudged>;
  const [fired, done, unjudged] = [finite(counts.fired), finite(counts.judged), finite(counts.unjudged)];
  if (fired === null || done === null || unjudged === null) return "";
  return ` (${done} judged, ${unjudged} unjudged of ${fired})`;
}

function mark(raw: unknown): CoreMark {
  const figure = (typeof raw === "object" && raw !== null ? raw : {}) as Partial<CoreFigure>;
  const unit = typeof figure.unit === "string" ? figure.unit : "count";
  const value = finite(figure.value);
  const since = finite(figure.measuring_since_ms);
  const of = finite(figure.of);
  const target = finite(figure.target);
  const held = typeof figure.range_note === "string" && figure.range_note !== "" ? figure.range_note : null;
  const said: CoreMark = {
    name: `${typeof figure.name === "string" ? figure.name : String(figure.key ?? "")}${windowWords(figure.window)}${storeWords(figure.store)}${judgedWords(figure.judged)}`,
    value:
      value !== null
        ? `${unitWords(unit, value)}${of !== null ? ` of ${GROUPED.format(of)}` : ""}`
        : figure.state === "not_measured_yet"
          ? "not measured yet"
          : since !== null
            ? `measuring since ${sinceWords(since)}`
            : "not collected yet",
    range: held ?? rangeWords(unit, figure.range),
    tone: figure.state === "in" ? "plain" : figure.state === "out" ? "warning" : "muted",
  };
  if (target !== null) said.target = `target ${boundWords(unit, target)}`;
  return said;
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
  const reason = typeof entry?.reason === "string" ? entry.reason : "";
  // A range the set ignores on purpose (objective 182: free disk, dollars
  // per landing): the figure's name and why.
  if (known !== undefined && reason !== "" && reason !== "needs a number min or max") return `${known}: ${reason}`;
  if (known !== undefined) return `${known}: range not read (it needs a number min or max)`;
  const words = key.slice(key.lastIndexOf(".") + 1).replace(/_/g, " ");
  // A figure the owner's core metric set dropped (objective 176).
  if (String(entry?.reason ?? "").startsWith("retired")) return `${words}: retired from the core metric set`;
  return `${words}: figure not built yet`;
}

/** The card's words for an answer of `GET /api/core-lines`. */
export function coreLinesView(answer: Partial<CoreLinesAnswer> | null | undefined, now: number = Date.now()): CoreLinesView {
  const lines = Array.isArray(answer?.lines) ? answer.lines : [];
  const error = typeof answer?.ranges_error === "string" && answer.ranges_error !== "" ? answer.ranges_error : null;
  const ignored = Array.isArray(answer?.ignored_ranges) ? answer.ignored_ranges : [];
  const names = new Map<string, string>();
  const diagnostics = Array.isArray(answer?.diagnostics) ? answer.diagnostics : [];
  const alertFigures = Array.isArray(answer?.alert_figures) ? answer.alert_figures : [];
  const readingsOf = (line: Partial<CoreLine> | null | undefined) => (Array.isArray(line?.readings) ? line.readings : []);
  const figures = [
    ...lines.flatMap((line) => [
      line?.north_star,
      ...(Array.isArray(line?.guardrails) ? line.guardrails : []),
      ...readingsOf(line),
    ]),
    ...alertFigures,
    ...diagnostics,
  ];
  for (const figure of figures) {
    if (typeof figure?.key === "string" && typeof figure?.name === "string") names.set(figure.key, figure.name);
  }
  return {
    rows: lines.map((line) => ({
      line: LINE_NAMES[line?.line] ?? String(line?.line ?? ""),
      north_star: mark(line?.north_star),
      guardrails: Array.isArray(line?.guardrails) ? line.guardrails.map(mark) : [],
      readings: readingsOf(line).map(mark),
    })),
    notice: error === null ? null : `ranges file unreadable: ${error}`,
    ignored: ignored.map((entry) => `${entry?.key}: ${entry?.reason}`),
    pending: ignored.map((entry) => pendingWords(entry, names)),
    drills: (Array.isArray(answer?.drills) ? answer.drills : []).map((drill) => drillMark(drill, now)),
    scratch: (Array.isArray(answer?.scratch?.claimed) ? answer.scratch.claimed : []).map((claimed) =>
      scratchMark(claimed, now),
    ),
    byMission: (() => {
      const performance = lines.find((line) => line?.line === "performance");
      const byMission = performance?.north_star?.by_mission;
      return Array.isArray(byMission) ? byMission.map(missionMark) : [];
    })(),
    diagnostics: diagnostics.map(mark),
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

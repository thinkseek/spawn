// Pure read-model and formatting helpers for `#/features`'s non-blocking
// Runtime Telemetry Strip (`/api/fleet`, `/api/grading`, `/api/cost?window=24h|7d`).
// Zero React, DOM, or fetch imports so `node --test` verifies every ADT and
// billing invariant directly. Imports only from `cap-web-shell-design-system`
// modules (`format.ts`, `grading.ts`) to keep feature dependency edges clean.

import { compactCount } from "./format.ts";
import { groupByTopDownTier, normalizeGrading, type SkillGrade } from "./grading.ts";
import { Validated } from "./validated.ts";

export const TELEMETRY_POLL_MS = 15_000;
export const TELEMETRY_REQUEST_TIMEOUT_MS = 6_000;

export type CostDisplayAmount =
  | { kind: "unpriced"; unpricedTokens: number; text: "unpriced" }
  | { kind: "priced"; usd: number; text: string }
  | { kind: "mixed"; usd: number; unpricedTokens: number; text: string; unpricedShort: string }
  | { kind: "unavailable"; text: "—" };

export interface TelemetryAgentsSummary {
  live: number;
  working: number;
  waiting: number;
  blocked: number;
  total: number;
}

export interface TelemetryTierSummary {
  grade: SkillGrade;
  label: "L1" | "L2" | "L3";
  activeCount: number;
  totalCount: number;
  activeModels: string[];
}

export interface TelemetryModelsSummary {
  tiers: TelemetryTierSummary[];
  activeTotal: number;
  totalPresets: number;
}

export interface TelemetryCostModel {
  model: string;
  totalTokens: number;
  unpricedTokens: number;
  amount: CostDisplayAmount;
}

export interface TelemetryCostWindow {
  window: "24h" | "7d";
  totalTokens: number;
  unpricedTokens: number;
  amount: CostDisplayAmount;
  models: TelemetryCostModel[];
  unpricedModelCount: number;
}

export type TelemetrySlice<T> =
  | { status: "loading" }
  | { status: "ready"; data: T }
  | { status: "unavailable" };

export interface RuntimeTelemetryState {
  fleet: TelemetrySlice<TelemetryAgentsSummary>;
  grading: TelemetrySlice<TelemetryModelsSummary>;
  cost24h: TelemetrySlice<TelemetryCostWindow>;
  cost7d: TelemetrySlice<TelemetryCostWindow>;
}

export const INITIAL_RUNTIME_TELEMETRY: RuntimeTelemetryState = {
  fleet: { status: "loading" },
  grading: { status: "loading" },
  cost24h: { status: "loading" },
  cost7d: { status: "loading" },
};

const DOLLARS = new Intl.NumberFormat("en-US", {
  style: "currency",
  currency: "USD",
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

type JsonObject = Record<string, unknown>;

function isObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function finiteNumberOrNull(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function nonNegInt(value: unknown): number {
  const n = finiteNumberOrNull(value);
  return n !== null && n > 0 ? Math.trunc(n) : 0;
}

function tokenSum(tokensRaw: unknown): number {
  if (!isObject(tokensRaw)) return 0;
  return (
    nonNegInt(tokensRaw.input) +
    nonNegInt(tokensRaw.output) +
    nonNegInt(tokensRaw.cache_write) +
    nonNegInt(tokensRaw.cache_read)
  );
}

export function formatTelemetryUsd(usd: number): string {
  if (!Number.isFinite(usd)) return "—";
  if (usd === 0) return "$0.00";
  if (Math.abs(usd) < 0.005) return usd < 0 ? "-<$0.01" : "<$0.01";
  return DOLLARS.format(usd);
}

/** Closed ADT classification for model or window cost. Enforces the billing red
 * line: when `usd == null && unpricedTokens > 0`, or when all tokens are
 * unpriced (`unpricedTokens > 0 && pricedTokens <= 0 && usd === 0`), returns
 * `{ kind: "unpriced", text: "unpriced" }` — NEVER `"$0.00"`. */
export function classifyCostAmount(
  usd: number | null,
  totalTokens: number,
  unpricedTokens: number,
): CostDisplayAmount {
  const unpriced = nonNegInt(unpricedTokens);
  const total = nonNegInt(totalTokens);
  const finiteUsd = finiteNumberOrNull(usd);

  if (finiteUsd === null && unpriced > 0) {
    return { kind: "unpriced", unpricedTokens: unpriced, text: "unpriced" };
  }
  const pricedTokens = Math.max(0, total - unpriced);
  if (unpriced > 0 && (finiteUsd === 0 || finiteUsd === null) && pricedTokens <= 0) {
    return { kind: "unpriced", unpricedTokens: unpriced, text: "unpriced" };
  }
  if (unpriced > 0 && finiteUsd !== null) {
    return {
      kind: "mixed",
      usd: finiteUsd,
      unpricedTokens: unpriced,
      text: formatTelemetryUsd(finiteUsd),
      unpricedShort: `+${compactCount(unpriced)} unpriced`,
    };
  }
  if (finiteUsd !== null) {
    return { kind: "priced", usd: finiteUsd, text: formatTelemetryUsd(finiteUsd) };
  }
  return { kind: "unavailable", text: "—" };
}

export function formatCostAmountText(amount: CostDisplayAmount): string {
  switch (amount.kind) {
    case "unpriced":
      return amount.text;
    case "priced":
      return amount.text;
    case "mixed":
      return `${amount.text} (${amount.unpricedShort})`;
    case "unavailable":
      return amount.text;
  }
}

export function normalizeFleetTelemetry(raw: unknown): TelemetryAgentsSummary {
  const obj = isObject(raw) ? raw : {};
  const agents = Array.isArray(obj.agents) ? obj.agents.filter(isObject) : [];
  const health = isObject(obj.health) ? obj.health : {};
  const byState = isObject(health.by_state) ? health.by_state : {};

  let countedLive = 0;
  let countedWorking = 0;
  let countedWaiting = 0;
  let countedBlocked = 0;

  for (const agent of agents) {
    if (agent.live === true) countedLive += 1;
    const state = typeof agent.state === "string" ? agent.state.trim().toUpperCase() : "";
    if (state === "WORKING") countedWorking += 1;
    else if (state === "WAITING") countedWaiting += 1;
    else if (state === "BLOCKED") countedBlocked += 1;
  }

  const readStateCount = (upper: string, fallback: number): number => {
    const val = byState[upper] ?? byState[upper.toLowerCase()];
    const num = finiteNumberOrNull(val);
    return num !== null && num >= 0 ? Math.trunc(num) : fallback;
  };

  const hasByState = Object.keys(byState).length > 0;
  const liveRaw = finiteNumberOrNull(health.live);
  const totalRaw = finiteNumberOrNull(health.agents_total);

  return {
    live: liveRaw !== null && liveRaw >= 0 ? Math.trunc(liveRaw) : countedLive,
    working: hasByState ? readStateCount("WORKING", countedWorking) : countedWorking,
    waiting: hasByState ? readStateCount("WAITING", countedWaiting) : countedWaiting,
    blocked: hasByState ? readStateCount("BLOCKED", countedBlocked) : countedBlocked,
    total: totalRaw !== null && totalRaw >= 0 ? Math.trunc(totalRaw) : agents.length,
  };
}

const TIER_SPEC: ReadonlyArray<{ tierId: "t1" | "t2" | "t3"; grade: SkillGrade; label: "L1" | "L2" | "L3" }> = [
  { tierId: "t1", grade: "l1", label: "L1" },
  { tierId: "t2", grade: "l2", label: "L2" },
  { tierId: "t3", grade: "l3", label: "L3" },
];

export function normalizeGradingTelemetry(raw: unknown): TelemetryModelsSummary {
  const catalog = normalizeGrading(raw);
  const groups = groupByTopDownTier(catalog.presets);
  const byTierId = new Map(groups.map((group) => [group.id, group]));

  const tiers: TelemetryTierSummary[] = TIER_SPEC.map(({ tierId, grade, label }) => {
    const presets = byTierId.get(tierId)?.presets ?? [];
    const activePresets = presets.filter((preset) => preset.state === "active");
    const activeModels = [
      ...new Set(
        activePresets
          .map((preset) => preset.model_label.trim() || preset.preset_id)
          .filter((name) => name !== ""),
      ),
    ].sort((a, b) => a.localeCompare(b));
    return {
      grade,
      label,
      activeCount: activePresets.length,
      totalCount: presets.length,
      activeModels,
    };
  });

  const activeTotal = tiers.reduce((sum, tier) => sum + tier.activeCount, 0);
  return {
    tiers,
    activeTotal,
    totalPresets: catalog.presets.length,
  };
}

export function normalizeCostWindowTelemetry(
  window: "24h" | "7d",
  raw: unknown,
): TelemetryCostWindow {
  const obj = isObject(raw) ? raw : {};
  const totalObj = isObject(obj.total) ? obj.total : {};
  const totalTokens = tokenSum(totalObj.tokens);
  const unpricedTokens = nonNegInt(totalObj.unpriced_tokens);
  const totalUsd = finiteNumberOrNull(totalObj.usd);

  const rawModels = Array.isArray(obj.models) ? obj.models.filter(isObject) : [];
  const models: TelemetryCostModel[] = rawModels.map((entry, idx) => {
    const name =
      typeof entry.model === "string" && entry.model.trim() !== ""
        ? entry.model.trim()
        : `model-${idx + 1}`;
    const mTokens = tokenSum(entry.tokens);
    const mUnpriced = nonNegInt(entry.unpriced_tokens);
    const mUsd = finiteNumberOrNull(entry.usd);
    return {
      model: name,
      totalTokens: mTokens,
      unpricedTokens: mUnpriced,
      amount: classifyCostAmount(mUsd, mTokens, mUnpriced),
    };
  });

  const everyModelUnpriced =
    models.length > 0 && models.every((entry) => entry.amount.kind === "unpriced");
  const baseAmount = classifyCostAmount(totalUsd, totalTokens, unpricedTokens);
  const amount: CostDisplayAmount =
    everyModelUnpriced && (totalUsd === null || totalUsd === 0) && unpricedTokens > 0
      ? { kind: "unpriced", unpricedTokens, text: "unpriced" }
      : baseAmount;

  const unpricedModelCount = models.filter(
    (entry) => entry.amount.kind === "unpriced" || entry.amount.kind === "mixed",
  ).length;

  return {
    window,
    totalTokens,
    unpricedTokens,
    amount,
    models,
    unpricedModelCount,
  };
}

export type TelemetrySliceKey = keyof RuntimeTelemetryState;

export const TELEMETRY_SLICE_KEYS: ReadonlyArray<TelemetrySliceKey> = [
  "fleet",
  "grading",
  "cost24h",
  "cost7d",
];

export type TelemetrySliceUpdate =
  | { key: "fleet"; slice: TelemetrySlice<TelemetryAgentsSummary> }
  | { key: "grading"; slice: TelemetrySlice<TelemetryModelsSummary> }
  | { key: "cost24h"; slice: TelemetrySlice<TelemetryCostWindow> }
  | { key: "cost7d"; slice: TelemetrySlice<TelemetryCostWindow> };

export function applyTelemetrySliceUpdate(
  state: RuntimeTelemetryState,
  update: TelemetrySliceUpdate,
): RuntimeTelemetryState {
  return { ...state, [update.key]: update.slice };
}

export interface OptionalSliceReadOptions {
  signal: AbortSignal;
  headers: Record<string, string>;
  timeoutMs?: number;
}

/** Local optional endpoint reader with strict isolation from global auth
 * sync: 401/403/timeout/network failures degrade locally to `"unavailable"`
 * and NEVER trigger global `unauthorized` broadcasts or logouts. */
export async function readOptionalTelemetrySlice<T>(
  reader: Validated,
  url: string,
  normalize: (raw: unknown) => T,
  options: OptionalSliceReadOptions,
): Promise<TelemetrySlice<T>> {
  if (options.signal.aborted) {
    throw new DOMException("The operation was aborted.", "AbortError");
  }

  const timeoutMs = options.timeoutMs ?? TELEMETRY_REQUEST_TIMEOUT_MS;
  const sliceController = new AbortController();
  const onOuterAbort = () => sliceController.abort();
  options.signal.addEventListener("abort", onOuterAbort, { once: true });

  let timedOut = false;
  const timer: ReturnType<typeof setTimeout> | null =
    timeoutMs > 0
      ? setTimeout(() => {
          timedOut = true;
          sliceController.abort();
        }, timeoutMs)
      : null;

  try {
    const data = await reader.read(url, normalize, {
      signal: sliceController.signal,
      headers: options.headers,
      // Strictly local error creation — never calls `reportUnauthorizedStatus`.
      fail: (status, detail) => new Error(detail ? `HTTP ${status}: ${detail}` : `HTTP ${status}`),
    });
    return { status: "ready", data };
  } catch {
    if (options.signal.aborted && !timedOut) {
      throw new DOMException("The operation was aborted.", "AbortError");
    }
    return { status: "unavailable" };
  } finally {
    if (timer !== null) clearTimeout(timer);
    options.signal.removeEventListener("abort", onOuterAbort);
  }
}

export async function readTelemetrySliceByKey(
  key: TelemetrySliceKey,
  options: OptionalSliceReadOptions & { reader: Validated },
): Promise<TelemetrySliceUpdate> {
  const { reader, signal, headers, timeoutMs } = options;
  switch (key) {
    case "fleet":
      return {
        key: "fleet",
        slice: await readOptionalTelemetrySlice(reader, "/api/fleet", normalizeFleetTelemetry, {
          signal,
          headers,
          timeoutMs,
        }),
      };
    case "grading":
      return {
        key: "grading",
        slice: await readOptionalTelemetrySlice(reader, "/api/grading", normalizeGradingTelemetry, {
          signal,
          headers,
          timeoutMs,
        }),
      };
    case "cost24h":
      return {
        key: "cost24h",
        slice: await readOptionalTelemetrySlice(
          reader,
          "/api/cost?window=24h",
          (raw) => normalizeCostWindowTelemetry("24h", raw),
          { signal, headers, timeoutMs },
        ),
      };
    case "cost7d":
      return {
        key: "cost7d",
        slice: await readOptionalTelemetrySlice(
          reader,
          "/api/cost?window=7d",
          (raw) => normalizeCostWindowTelemetry("7d", raw),
          { signal, headers, timeoutMs },
        ),
      };
  }
}

/** Fire all 4 optional telemetry slice reads concurrently with independent
 * timeouts and immediate per-slice settlement callbacks so one slow/stalled
 * endpoint never starves healthy siblings. */
export async function pollRuntimeTelemetrySlices(
  options: OptionalSliceReadOptions & {
    reader: Validated;
    onSlice?: (update: TelemetrySliceUpdate) => void;
  },
): Promise<RuntimeTelemetryState> {
  let state: RuntimeTelemetryState = { ...INITIAL_RUNTIME_TELEMETRY };
  await Promise.allSettled(
    TELEMETRY_SLICE_KEYS.map((key) =>
      readTelemetrySliceByKey(key, options).then((update) => {
        state = applyTelemetrySliceUpdate(state, update);
        if (!options.signal.aborted) {
          options.onSlice?.(update);
        }
      }),
    ),
  );
  if (options.signal.aborted) {
    throw new DOMException("The operation was aborted.", "AbortError");
  }
  return state;
}

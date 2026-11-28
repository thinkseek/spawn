// The Feature map's reads (`/api/features`, `/api/features/truth`). Its own
// module, so the read model in features.ts stays pure for node's test runner.

import { apiHeaders, readApi, subscribeAuthSync } from "./api";
import {
  parseFeatureTruthResponse,
  truthUrl,
  type FeatureTruthResponse,
} from "./featureCoverage.ts";
import { normalizeFeatureMap, type FeatureMap } from "./features.ts";
import {
  pollRuntimeTelemetrySlices,
  readTelemetrySliceByKey,
  type RuntimeTelemetryState,
  type TelemetrySliceKey,
  type TelemetrySliceUpdate,
} from "./runtimeTelemetry.ts";
import { Validated } from "./validated.ts";

/** Dedicated local reader for optional telemetry so 401/403/timeout on any
 * telemetry endpoint never flows through `readApi` or logs the session out. */
const optionalTelemetryReads = new Validated();
subscribeAuthSync(() => optionalTelemetryReads.clear());

/** GET /api/features: every open objective mission's capability map. */
export async function fetchFeatureMap(signal: AbortSignal): Promise<FeatureMap> {
  return readApi("/api/features", signal, normalizeFeatureMap);
}

/** GET /api/features/truth (optional read-only enrichment for `#/features`):
 * returns `null` if the endpoint is unavailable or unsupported so existing
 * feature map views keep rendering without interruption. */
export async function fetchOptionalFeatureTruth(
  signal: AbortSignal,
  mission: number | null = null,
): Promise<FeatureTruthResponse | null> {
  try {
    return await readApi(truthUrl({ mission }), signal, parseFeatureTruthResponse);
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return null;
  }
}

/** Read a single optional runtime telemetry slice independently with per-slice
 * timeout and zero global `unauthorized` auth side effects. */
export async function readOptionalTelemetrySliceByKey(
  key: TelemetrySliceKey,
  signal: AbortSignal,
  timeoutMs?: number,
): Promise<TelemetrySliceUpdate> {
  return readTelemetrySliceByKey(key, {
    reader: optionalTelemetryReads,
    signal,
    headers: apiHeaders(),
    timeoutMs,
  });
}

/** Stream optional runtime telemetry slices concurrently, notifying `onSlice`
 * immediately as each individual endpoint settles. */
export async function pollOptionalRuntimeTelemetry(
  signal: AbortSignal,
  onSlice: (update: TelemetrySliceUpdate) => void,
  timeoutMs?: number,
): Promise<RuntimeTelemetryState> {
  return pollRuntimeTelemetrySlices({
    reader: optionalTelemetryReads,
    signal,
    headers: apiHeaders(),
    timeoutMs,
    onSlice,
  });
}

/** Non-blocking optional runtime telemetry for `#/features` from existing
 * read-only endpoints (`/api/fleet`, `/api/grading`, `/api/cost?window=24h|7d`).
 * Individual endpoint failures degrade independently to `"unavailable"`. */
export async function fetchOptionalRuntimeTelemetry(
  signal: AbortSignal,
): Promise<RuntimeTelemetryState> {
  return pollOptionalRuntimeTelemetry(signal, () => {});
}

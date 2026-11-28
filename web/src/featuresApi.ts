// The Feature map's reads (`/api/features`, `/api/features/truth`). Its own
// module, so the read model in features.ts stays pure for node's test runner.

import { readApi } from "./api";
import {
  parseFeatureTruthResponse,
  truthUrl,
  type FeatureTruthResponse,
} from "./featureCoverage.ts";
import { normalizeFeatureMap, type FeatureMap } from "./features.ts";
import {
  INITIAL_RUNTIME_TELEMETRY,
  normalizeCostWindowTelemetry,
  normalizeFleetTelemetry,
  normalizeGradingTelemetry,
  type RuntimeTelemetryState,
  type TelemetrySlice,
} from "./runtimeTelemetry.ts";

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

async function readOptionalSlice<T>(
  url: string,
  signal: AbortSignal,
  normalize: (raw: unknown) => T,
): Promise<TelemetrySlice<T>> {
  try {
    const data = await readApi(url, signal, normalize);
    return { status: "ready", data };
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return { status: "unavailable" };
  }
}

/** Non-blocking optional runtime telemetry for `#/features` from existing
 * read-only endpoints (`/api/fleet`, `/api/grading`, `/api/cost?window=24h|7d`).
 * Individual endpoint failures degrade independently to `"unavailable"`. */
export async function fetchOptionalRuntimeTelemetry(
  signal: AbortSignal,
): Promise<RuntimeTelemetryState> {
  const [fleetRes, gradingRes, cost24hRes, cost7dRes] = await Promise.allSettled([
    readOptionalSlice("/api/fleet", signal, normalizeFleetTelemetry),
    readOptionalSlice("/api/grading", signal, normalizeGradingTelemetry),
    readOptionalSlice("/api/cost?window=24h", signal, (raw) =>
      normalizeCostWindowTelemetry("24h", raw),
    ),
    readOptionalSlice("/api/cost?window=7d", signal, (raw) =>
      normalizeCostWindowTelemetry("7d", raw),
    ),
  ]);

  if (signal.aborted) {
    throw new DOMException("The operation was aborted.", "AbortError");
  }

  const pick = <T,>(
    res: PromiseSettledResult<TelemetrySlice<T>>,
    fallback: TelemetrySlice<T>,
  ): TelemetrySlice<T> => (res.status === "fulfilled" ? res.value : fallback);

  return {
    fleet: pick(fleetRes, INITIAL_RUNTIME_TELEMETRY.fleet),
    grading: pick(gradingRes, INITIAL_RUNTIME_TELEMETRY.grading),
    cost24h: pick(cost24hRes, INITIAL_RUNTIME_TELEMETRY.cost24h),
    cost7d: pick(cost7dRes, INITIAL_RUNTIME_TELEMETRY.cost7d),
  };
}

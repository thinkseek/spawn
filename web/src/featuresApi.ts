// The Feature map's reads (`/api/features`, `/api/features/truth`). Its own
// module, so the read model in features.ts stays pure for node's test runner.

import { readApi } from "./api";
import {
  parseFeatureTruthResponse,
  truthUrl,
  type FeatureTruthResponse,
} from "./featureCoverage.ts";
import { normalizeFeatureMap, type FeatureMap } from "./features.ts";

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

// The Feature map's one read (`/api/features`). Its own module, so the read
// model in features.ts stays pure for node's test runner (no fetch, no api).

import { readApi } from "./api";
import { normalizeFeatureMap, type FeatureMap } from "./features.ts";

/** GET /api/features: every open objective mission's capability map. */
export async function fetchFeatureMap(signal: AbortSignal): Promise<FeatureMap> {
  return readApi("/api/features", signal, normalizeFeatureMap);
}

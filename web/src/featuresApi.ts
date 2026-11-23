// The Feature map's one read (`/api/features`). Its own module, so the read
// model in features.ts stays pure for node's test runner (no fetch, no api).

import { readApi } from "./api";
import { normalizeFeatureMap, type FeatureMap } from "./features.ts";

/** The most missions `/api/features` answers (`?limit=`, the server's cap). */
export const FEATURE_MAP_MAX_MISSIONS = 50;

/** GET /api/features: every open objective mission's capability map, at most
 * `limit` missions (the server's default of 10 when not given). */
export async function fetchFeatureMap(
  signal: AbortSignal,
  limit?: number,
): Promise<FeatureMap> {
  const url = limit === undefined ? "/api/features" : `/api/features?limit=${limit}`;
  return readApi(url, signal, normalizeFeatureMap);
}

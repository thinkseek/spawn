// HTTP client for Feature Map file coverage & one-click unassociated-file scan.
// Separate from featureCoverage.ts so the pure read model stays testable under
// `node --test`.

import { apiError, apiHeaders, readApi } from "./api";
import {
  normalizeFeatureCoverage,
  type FeatureCoverageResponse,
} from "./featureCoverage.ts";

/** GET /api/features/coverage: current coverage report and registered features. */
export async function fetchFeatureCoverage(
  signal: AbortSignal,
): Promise<FeatureCoverageResponse> {
  return readApi("/api/features/coverage", signal, normalizeFeatureCoverage);
}

/** POST /api/features/scan: trigger a fresh one-click unassociated-file scan. */
export async function runWildFileScan(
  signal?: AbortSignal,
): Promise<FeatureCoverageResponse> {
  const response = await fetch("/api/features/scan", {
    method: "POST",
    signal,
    cache: "no-store",
    headers: { ...apiHeaders(), "Content-Type": "application/json" },
    body: "{}",
  });
  const payload = await readJson(response);
  if (!response.ok) {
    throw apiError(response.status, errorDetail(payload));
  }
  return normalizeFeatureCoverage(payload);
}

/** POST /api/features/associate: associate one in-scope file with one or more
 * genuine feature nodes (many-to-many). Never deletes any file on disk. */
export async function associateWildFile(
  path: string,
  features: string[],
  signal?: AbortSignal,
): Promise<FeatureCoverageResponse> {
  const response = await fetch("/api/features/associate", {
    method: "POST",
    signal,
    cache: "no-store",
    headers: { ...apiHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ path, features }),
  });
  const payload = await readJson(response);
  if (!response.ok) {
    throw apiError(response.status, errorDetail(payload));
  }
  return normalizeFeatureCoverage(payload);
}

async function readJson(response: Response): Promise<unknown> {
  try {
    return await response.json();
  } catch {
    return null;
  }
}

function errorDetail(payload: unknown): string {
  if (
    payload !== null &&
    typeof payload === "object" &&
    "error" in payload &&
    typeof (payload as { error?: unknown }).error === "string"
  ) {
    return (payload as { error: string }).error;
  }
  return "";
}

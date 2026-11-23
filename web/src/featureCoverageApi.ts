// HTTP client for Feature Map file coverage & one-click unassociated-file scan.
// Separate from featureCoverage.ts so the pure read model stays testable under
// `node --test`. Each call reads the server's default project, or with
// `mission` that mission's configured `--features` project.

import { apiError, apiHeaders, readApi } from "./api";
import {
  coverageUrl,
  normalizeCoverageProjects,
  parseCoverageHttpResponse,
  parseFeatureCoverageResponse,
  type CoverageProject,
  type FeatureCoverageResponse,
} from "./featureCoverage.ts";

/** GET /api/features/coverage/projects: every open objective mission whose
 * `--features` file is a coverage registry (the project picker). */
export async function fetchCoverageProjects(signal: AbortSignal): Promise<CoverageProject[]> {
  return readApi("/api/features/coverage/projects", signal, normalizeCoverageProjects);
}

/** GET /api/features/coverage: current coverage report and registered features. */
export async function fetchFeatureCoverage(
  signal: AbortSignal,
  mission: number | null = null,
): Promise<FeatureCoverageResponse> {
  return readApi(
    coverageUrl("/api/features/coverage", mission),
    signal,
    parseFeatureCoverageResponse,
  );
}

/** POST /api/features/scan: trigger a fresh one-click unassociated-file scan. */
export async function runWildFileScan(
  signal?: AbortSignal,
  mission: number | null = null,
): Promise<FeatureCoverageResponse> {
  const response = await fetch(coverageUrl("/api/features/scan", mission), {
    method: "POST",
    signal,
    cache: "no-store",
    headers: { ...apiHeaders(), "Content-Type": "application/json" },
    body: "{}",
  });
  return parseCoverageHttpResponse(response, apiError);
}

/** POST /api/features/associate: associate one in-scope file with one or more
 * genuine feature nodes (many-to-many). Never deletes any file on disk. */
export async function associateWildFile(
  path: string,
  features: string[],
  signal?: AbortSignal,
  mission: number | null = null,
): Promise<FeatureCoverageResponse> {
  const response = await fetch(coverageUrl("/api/features/associate", mission), {
    method: "POST",
    signal,
    cache: "no-store",
    headers: { ...apiHeaders(), "Content-Type": "application/json" },
    body: JSON.stringify({ path, features }),
  });
  return parseCoverageHttpResponse(response, apiError);
}

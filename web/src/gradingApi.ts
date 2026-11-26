// Grading catalog API client (`GET /api/grading`). Isolated from grading.ts
// so node's test runner executes grading.ts without Vite/fetch globals.

import { readApi } from "./api";
import { normalizeGrading, type GradingCatalog } from "./grading.ts";

/** GET /api/grading: the grading catalog and profile names. */
export async function fetchGrading(signal: AbortSignal): Promise<GradingCatalog> {
  return readApi("/api/grading", signal, normalizeGrading);
}

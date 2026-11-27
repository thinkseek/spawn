// D6 Storage Evolution Tree & Snapshot Detail API client.
// Kept separate from storageTree.ts so node --test imports pure logic cleanly.

import { readApi } from "./api";
import {
  normalizeStorageSnapshot,
  normalizeStorageTree,
  type StorageSnapshotDetail,
  type StorageTreeResponse,
} from "./storageTree.ts";

/** GET /api/storage/tree[?project=<name>&limit=<N>] */
export async function fetchStorageTree(
  project: string | null,
  limit: number,
  signal: AbortSignal,
): Promise<StorageTreeResponse> {
  const params = new URLSearchParams();
  if (project) params.set("project", project);
  if (Number.isFinite(limit) && limit > 0) params.set("limit", String(limit));
  const suffix = params.toString();
  const path = suffix ? `/api/storage/tree?${suffix}` : "/api/storage/tree";
  return readApi(path, signal, normalizeStorageTree);
}

/** GET /api/storage/snapshot/<id> (returns null on 404). */
export async function fetchStorageSnapshot(
  snapshotId: string,
  signal: AbortSignal,
): Promise<StorageSnapshotDetail | null> {
  const path = `/api/storage/snapshot/${encodeURIComponent(snapshotId)}`;
  try {
    return await readApi(path, signal, normalizeStorageSnapshot);
  } catch (err) {
    if (err instanceof Error && /^HTTP 404(?::|$)/.test(err.message)) {
      return null;
    }
    throw err;
  }
}

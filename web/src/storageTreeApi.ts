// D6 Storage Evolution Tree & Snapshot Detail API client.
// Kept separate from storageTree.ts so node --test imports pure logic cleanly.

import { apiHeaders, readApi } from "./api";
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
  const url = `/api/storage/snapshot/${encodeURIComponent(snapshotId)}`;
  const response = await fetch(url, {
    headers: apiHeaders(),
    signal,
  });
  if (response.status === 404) {
    return null;
  }
  const text = await response.text();
  if (!response.ok) {
    let message = `HTTP ${response.status}`;
    try {
      const parsed = JSON.parse(text) as { error?: unknown };
      if (typeof parsed.error === "string" && parsed.error.length > 0) {
        message = parsed.error;
      }
    } catch {
      if (text.trim().length > 0) message = text.trim();
    }
    throw new Error(message);
  }
  return normalizeStorageSnapshot(JSON.parse(text));
}

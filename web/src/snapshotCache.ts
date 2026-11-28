// Safe browser sessionStorage persistence for read-only dashboard snapshots
// and their ETags (never stored in Service Worker Cache Storage, never stores
// raw serve tokens). Pure apart from the injected Storage interface so Node
// tests exercise every quota, expiration, and token-scope guard directly.

export const SNAPSHOT_PREFIX = "spawn.snap.v1:";

/** Read-only endpoints safe to hydrate across reloads within the same session. */
export const SNAPSHOT_PATHS: ReadonlyArray<string> = [
  "/api/overview",
  "/api/fleet",
  "/api/missions",
  "/api/attention",
  "/api/alerts",
  "/api/approvals",
];

/** Skip individual entries larger than ~900 KB to stay comfortably inside quota. */
export const MAX_ENTRY_CHARS = 900_000;

/** Discard snapshots older than 10 minutes instead of hydrating stale state. */
export const MAX_SNAPSHOT_AGE_MS = 10 * 60_000;

export interface SnapshotEntry {
  url: string;
  etag: string;
  raw: unknown;
  savedAtMs: number;
}

export interface SnapshotStorageLike {
  readonly length: number;
  key(index: number): string | null;
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

const SNAPSHOT_ALLOWED = new Set<string>(SNAPSHOT_PATHS);

export function isSnapshotAllowed(url: string): boolean {
  return SNAPSHOT_ALLOWED.has(url);
}

/** Non-reversible 64-bit hex scope fingerprint derived from the active credential
 * (separates tokenless loopback and distinct tokens without storing any secret). */
export function tokenScope(token: string | null): string {
  const input = token === null ? "open" : `tok:${token}`;
  let h1 = 0x811c9dc5;
  let h2 = 0x050c5d1f;
  for (let i = 0; i < input.length; i += 1) {
    const code = input.charCodeAt(i);
    h1 ^= code;
    h1 = Math.imul(h1, 0x01000193);
    h2 ^= (code + i) & 0xffff;
    h2 = Math.imul(h2, 0x01000193);
  }
  const hi = (h1 >>> 0).toString(16).padStart(8, "0");
  const lo = (h2 >>> 0).toString(16).padStart(8, "0");
  return `${hi}${lo}`;
}

function storageKey(scope: string, url: string): string {
  return `${SNAPSHOT_PREFIX}${scope}:${url}`;
}

function listSnapshotKeys(storage: SnapshotStorageLike): string[] {
  const keys: string[] = [];
  for (let i = 0; i < storage.length; i += 1) {
    const k = storage.key(i);
    if (k !== null && k.startsWith(SNAPSHOT_PREFIX)) keys.push(k);
  }
  return keys;
}

/** Load valid snapshots matching `scope`, removing expired or foreign-scope entries. */
export function loadSnapshots(
  storage: SnapshotStorageLike | null | undefined,
  scope: string,
  now = Date.now(),
): SnapshotEntry[] {
  if (!storage) return [];
  const prefixForScope = `${SNAPSHOT_PREFIX}${scope}:`;
  const loaded: SnapshotEntry[] = [];
  let keys: string[];
  try {
    keys = listSnapshotKeys(storage);
  } catch {
    return [];
  }
  for (const key of keys) {
    if (!key.startsWith(prefixForScope)) {
      try {
        storage.removeItem(key);
      } catch {
        // Ignore storage deletion errors.
      }
      continue;
    }
    try {
      const rawText = storage.getItem(key);
      if (!rawText) {
        storage.removeItem(key);
        continue;
      }
      const parsed: unknown = JSON.parse(rawText);
      if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
        storage.removeItem(key);
        continue;
      }
      const record = parsed as Record<string, unknown>;
      const url = typeof record.url === "string" ? record.url : null;
      const etag = typeof record.etag === "string" && record.etag.length > 0 ? record.etag : null;
      const savedAtMs =
        typeof record.savedAtMs === "number" && Number.isFinite(record.savedAtMs)
          ? record.savedAtMs
          : null;
      const age = savedAtMs === null ? Infinity : now - savedAtMs;
      if (
        url === null ||
        !isSnapshotAllowed(url) ||
        etag === null ||
        !("raw" in record) ||
        age > MAX_SNAPSHOT_AGE_MS ||
        age < -60_000
      ) {
        storage.removeItem(key);
        continue;
      }
      loaded.push({ url, etag, raw: record.raw, savedAtMs: savedAtMs as number });
    } catch {
      try {
        storage.removeItem(key);
      } catch {
        // Ignore storage deletion errors.
      }
    }
  }
  return loaded;
}

/** Persist one allowlisted snapshot under `scope`, failing closed on oversized values or quota errors. */
export function saveSnapshot(
  storage: SnapshotStorageLike | null | undefined,
  scope: string,
  url: string,
  etag: string,
  raw: unknown,
  now = Date.now(),
): boolean {
  if (!storage || !isSnapshotAllowed(url) || !etag) return false;
  try {
    const serialized = JSON.stringify({ url, etag, raw, savedAtMs: now });
    if (serialized.length > MAX_ENTRY_CHARS) return false;
    storage.setItem(storageKey(scope, url), serialized);
    return true;
  } catch {
    return false;
  }
}

/** Remove all cached Spawn snapshots from `storage` (e.g. on sign-out or 401). */
export function clearSnapshots(storage: SnapshotStorageLike | null | undefined): void {
  if (!storage) return;
  try {
    for (const key of listSnapshotKeys(storage)) {
      try {
        storage.removeItem(key);
      } catch {
        // Ignore individual removal errors.
      }
    }
  } catch {
    // Ignore storage enumeration errors.
  }
}

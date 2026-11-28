import assert from "node:assert/strict";
import { test } from "node:test";

import {
  clearSnapshots,
  isSnapshotAllowed,
  loadSnapshots,
  MAX_ENTRY_CHARS,
  MAX_SNAPSHOT_AGE_MS,
  saveSnapshot,
  SNAPSHOT_PREFIX,
  tokenScope,
  type SnapshotStorageLike,
} from "../src/snapshotCache.ts";

class MemoryStorage implements SnapshotStorageLike {
  readonly map = new Map<string, string>();
  quotaLimit = Infinity;

  get length(): number {
    return this.map.size;
  }

  key(index: number): string | null {
    return [...this.map.keys()][index] ?? null;
  }

  getItem(key: string): string | null {
    return this.map.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    if (value.length > this.quotaLimit) {
      throw new DOMException("QuotaExceededError", "QuotaExceededError");
    }
    this.map.set(key, value);
  }

  removeItem(key: string): void {
    this.map.delete(key);
  }
}

test("tokenScope derives distinct non-reversible hex fingerprints without leaking credentials", () => {
  const open = tokenScope(null);
  const a = tokenScope("secret_token_alpha");
  const b = tokenScope("secret_token_beta");
  assert.match(open, /^[0-9a-f]{16}$/);
  assert.match(a, /^[0-9a-f]{16}$/);
  assert.notEqual(open, a);
  assert.notEqual(a, b);
  assert.equal(a.includes("secret"), false);
});

test("saveSnapshot and loadSnapshots enforce allowlist, scope isolation, TTL, size cap, and clear", () => {
  const storage = new MemoryStorage();
  const scopeA = tokenScope("tok-a");
  const scopeB = tokenScope("tok-b");
  const now = 1_795_000_000_000;

  assert.equal(isSnapshotAllowed("/api/fleet"), true);
  assert.equal(isSnapshotAllowed("/api/missions?summary=1"), true);
  assert.equal(isSnapshotAllowed("/api/alerts?active=1"), true);
  assert.equal(isSnapshotAllowed("/api/sessions/session_1"), false);
  assert.equal(saveSnapshot(storage, scopeA, "/api/sessions/session_1", 'W/"x"', {}, now), false);

  const huge = "x".repeat(MAX_ENTRY_CHARS + 10);
  assert.equal(saveSnapshot(storage, scopeA, "/api/missions", 'W/"big"', { huge }, now), false);

  assert.equal(saveSnapshot(storage, scopeA, "/api/fleet", 'W/"f1"', { agents: [] }, now), true);
  assert.equal(
    saveSnapshot(storage, scopeA, "/api/overview", 'W/"o1"', { sessions: [] }, now - MAX_SNAPSHOT_AGE_MS - 1),
    true,
  );
  storage.setItem(`${SNAPSHOT_PREFIX}${scopeA}:/api/alerts`, "{bad json");
  storage.setItem("unrelated.key", "keep-me");

  // Loading with scope B drops scope A entries and keeps non-snapshot keys untouched.
  assert.deepEqual(loadSnapshots(storage, scopeB, now), []);
  assert.equal(storage.getItem("unrelated.key"), "keep-me");
  assert.equal([...storage.map.keys()].filter((k) => k.startsWith(SNAPSHOT_PREFIX)).length, 0);

  // Loading with matching scope A keeps valid entries and prunes expired/corrupt ones.
  assert.equal(saveSnapshot(storage, scopeA, "/api/fleet", 'W/"f2"', { agents: [1] }, now), true);
  assert.equal(
    saveSnapshot(storage, scopeA, "/api/missions", 'W/"old"', { missions: [] }, now - MAX_SNAPSHOT_AGE_MS - 5),
    true,
  );
  const loaded = loadSnapshots(storage, scopeA, now);
  assert.equal(loaded.length, 1);
  assert.equal(loaded[0].url, "/api/fleet");
  assert.equal(loaded[0].etag, 'W/"f2"');
  assert.deepEqual(loaded[0].raw, { agents: [1] });

  // Quota errors degrade cleanly without throwing.
  storage.quotaLimit = 10;
  assert.equal(saveSnapshot(storage, scopeA, "/api/attention", 'W/"att"', { pending: [] }, now), false);

  clearSnapshots(storage);
  assert.deepEqual([...storage.map.keys()], ["unrelated.key"]);
});

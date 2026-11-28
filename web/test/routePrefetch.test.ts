import assert from "node:assert/strict";
import { test } from "node:test";

import {
  createPrefetcher,
  prefetchPlan,
  PRIMARY_PREFETCH_ORDER,
  shouldSkipIdlePrefetch,
} from "../src/routePrefetch.ts";

test("prefetchPlan warms primary routes excluding the active landing view", () => {
  assert.deepEqual(prefetchPlan("fleet"), [...PRIMARY_PREFETCH_ORDER]);
  assert.deepEqual(prefetchPlan("missions"), ["agents", "features", "cost", "sessions"]);
  assert.deepEqual(prefetchPlan("cost"), ["missions", "agents", "features", "sessions"]);
});

test("shouldSkipIdlePrefetch respects Save-Data and 2G connections", () => {
  assert.equal(shouldSkipIdlePrefetch(undefined), false);
  assert.equal(shouldSkipIdlePrefetch(null), false);
  assert.equal(shouldSkipIdlePrefetch({ effectiveType: "4g", saveData: false }), false);
  assert.equal(shouldSkipIdlePrefetch({ effectiveType: "4g", saveData: true }), true);
  assert.equal(shouldSkipIdlePrefetch({ effectiveType: "slow-2g" }), true);
  assert.equal(shouldSkipIdlePrefetch({ effectiveType: "2g" }), true);
  assert.equal(shouldSkipIdlePrefetch({ effectiveType: "3g" }), false);
});

test("createPrefetcher deduplicates successful loads and clears failed loads for retry", async () => {
  const calls: Record<string, number> = { missions: 0, agents: 0, cost: 0 };
  let shouldFail = true;

  const prefetcher = createPrefetcher({
    missions: async () => {
      calls.missions += 1;
    },
    agents: async () => {
      calls.agents += 1;
      if (shouldFail) throw new Error("transient chunk error");
    },
  });

  await Promise.all([prefetcher.prefetch("missions"), prefetcher.prefetch("missions"), prefetcher.prefetch("fleet")]);
  assert.equal(calls.missions, 1, "concurrent prefetches share one dynamic import");

  // Registered lazily after creation (dependency inversion from composition root).
  prefetcher.register({
    cost: async () => {
      calls.cost += 1;
    },
  });
  await prefetcher.prefetch("cost");
  assert.equal(calls.cost, 1);

  // First attempt fails quietly; second attempt retries instead of caching rejection.
  await prefetcher.prefetch("agents");
  assert.equal(calls.agents, 1);
  shouldFail = false;
  await prefetcher.prefetch("agents");
  assert.equal(calls.agents, 2);
  await prefetcher.prefetch("agents");
  assert.equal(calls.agents, 2, "resolved chunk is not fetched again");
});

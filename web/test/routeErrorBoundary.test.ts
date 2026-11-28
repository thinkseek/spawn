import assert from "node:assert/strict";
import { test } from "node:test";

import {
  createRetryableLoader,
  createRouteErrorState,
  isChunkLoadError,
  retryRouteErrorState,
  routeErrorFromCaught,
  syncRouteErrorResetKey,
} from "../src/routeErrorBoundary.ts";

test("isChunkLoadError identifies dynamic-import and hashed chunk 404 failures", () => {
  assert.equal(isChunkLoadError(new TypeError("Failed to fetch dynamically imported module: https://host/assets/MissionsView-abc.js")), true);
  assert.equal(isChunkLoadError(new Error("Loading chunk 42 failed.")), true);
  const named = new Error("missing script");
  named.name = "ChunkLoadError";
  assert.equal(isChunkLoadError(named), true);
  assert.equal(isChunkLoadError(new Error("Importing a module script failed.")), true);
  assert.equal(isChunkLoadError(new Error("Cannot read properties of undefined")), false);
});

test("RouteErrorState recovers on retry (bumping nonce) and on route resetKey changes", () => {
  const initial = createRouteErrorState("missions");
  assert.equal(initial.hasError, false);
  assert.equal(initial.retryNonce, 0);

  const failed = routeErrorFromCaught(
    new TypeError("Failed to fetch dynamically imported module: /assets/MissionsView-deadbeef.js"),
    initial,
  );
  assert.equal(failed.hasError, true);
  assert.equal(failed.isChunkError, true);
  assert.match(failed.message, /dynamically imported module/);

  // Clicking Retry resets error and increments retryNonce so fresh lazy import runs.
  const retried = retryRouteErrorState(failed);
  assert.equal(retried.hasError, false);
  assert.equal(retried.message, "");
  assert.equal(retried.retryNonce, 1);
  assert.equal(retried.resetKey, "missions");

  // Navigating away (e.g. back to #/fleet) after a failure clears the error AND bumps retryNonce
  // so coming back to the failed route re-fetches the lazy chunk instead of reusing a rejected promise.
  const failedAgain = routeErrorFromCaught(new Error("ChunkLoadError"), retried);
  const switchedRoute = syncRouteErrorResetKey(failedAgain, "fleet");
  assert.equal(switchedRoute.hasError, false);
  assert.equal(switchedRoute.resetKey, "fleet");
  assert.equal(switchedRoute.retryNonce, 2);
  assert.equal(syncRouteErrorResetKey(switchedRoute, "fleet"), switchedRoute);

  // Switching routes while healthy leaves retryNonce untouched so resolved chunks do not flicker.
  const healthySwitch = syncRouteErrorResetKey(switchedRoute, "missions");
  assert.equal(healthySwitch.resetKey, "missions");
  assert.equal(healthySwitch.retryNonce, 2);
});

test("createRetryableLoader re-invokes loader after retryNonce advances following a 404 chunk failure", async () => {
  let failOnce = true;
  const controller = createRetryableLoader(async () => {
    if (failOnce) {
      failOnce = false;
      throw new TypeError("Failed to fetch dynamically imported module: /assets/chunk.js");
    }
    return { viewName: "MissionsView" };
  });

  let state = createRouteErrorState("missions");
  await assert.rejects(
    () => controller.load(state.retryNonce),
    /Failed to fetch dynamically imported module/,
  );
  assert.equal(controller.attempts(), 1);

  state = routeErrorFromCaught(new Error("Failed to fetch dynamically imported module"), state);
  assert.equal(state.hasError, true);

  state = retryRouteErrorState(state);
  const loaded = await controller.load(state.retryNonce);
  assert.deepEqual(loaded, { viewName: "MissionsView" });
  assert.equal(controller.attempts(), 2);
});

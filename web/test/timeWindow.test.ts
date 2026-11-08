import assert from "node:assert/strict";
import { test } from "node:test";

import {
  MIN_WINDOW_MS,
  clampDomain,
  fitDomain,
  panDomain,
  tickPattern,
  zoomDomain,
} from "../src/timeWindow.ts";

test("the window zooms around the pointer, pans, and stays inside the extent", () => {
  const extent = fitDomain({ start: 0, end: 100_000 });
  assert.deepEqual(extent, { from: -2000, to: 102_000 });
  const half = zoomDomain(extent, 0.5, 50_000, extent);
  assert.deepEqual(half, { from: 24_000, to: 76_000 });
  // The anchor stays put.
  const right = zoomDomain(extent, 0.5, 102_000, extent);
  assert.equal(right.to, 102_000);
  // Panning stops at the edges, keeping the width.
  assert.deepEqual(panDomain(half, 1e9, extent), { from: 50_000, to: 102_000 });
  assert.deepEqual(panDomain(half, -1e9, extent), { from: -2000, to: 50_000 });
  // Zooming out never goes past the extent, in never under the floor.
  assert.deepEqual(zoomDomain(half, 10, 50_000, extent), extent);
  const tight = zoomDomain(half, 1e-9, 30_000, extent);
  assert.equal(tight.to - tight.from, MIN_WINDOW_MS);
  assert.deepEqual(clampDomain({ from: 5, to: 5 }, extent).to - 5, MIN_WINDOW_MS);
  assert.equal(tickPattern(500), "%H:%M:%S.%L");
  assert.equal(tickPattern(5_000), "%H:%M:%S");
  assert.equal(tickPattern(300_000), "%H:%M");
});

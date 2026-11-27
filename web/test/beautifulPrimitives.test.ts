import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import { PIXEL_GRID_DELAYS_MS, pixelCellDelayMs } from "../src/beautifulPrimitives.ts";

test("pixel grid delays form a 3x3 monotonic diagonal shimmer wave", () => {
  assert.equal(PIXEL_GRID_DELAYS_MS.length, 9);
  for (const delay of PIXEL_GRID_DELAYS_MS) {
    assert.ok(Number.isFinite(delay) && delay >= 0);
  }
  assert.ok(pixelCellDelayMs(0) < pixelCellDelayMs(4));
  assert.ok(pixelCellDelayMs(4) < pixelCellDelayMs(8));
  assert.equal(pixelCellDelayMs(-1), PIXEL_GRID_DELAYS_MS[8]);
  assert.equal(pixelCellDelayMs(9), PIXEL_GRID_DELAYS_MS[0]);
});

test("spawn.css exposes Beautiful UI button and inset field shadow tokens in both themes", () => {
  const css = readFileSync(new URL("../src/styles/spawn.css", import.meta.url), "utf8");
  const darkBlock = css.slice(css.indexOf('[data-theme="dark"] {'), css.indexOf('[data-theme="light"] {'));
  const lightBlock = css.slice(css.indexOf('[data-theme="light"] {'), css.indexOf("/* Spawn's and Multica's names"));
  for (const token of ["--shadow-btn:", "--shadow-inset-field:", "--shadow-card:", "--shadow-raised:"]) {
    assert.ok(darkBlock.includes(token), `dark theme missing ${token}`);
    assert.ok(lightBlock.includes(token), `light theme missing ${token}`);
  }
});

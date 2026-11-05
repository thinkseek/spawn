import assert from "node:assert/strict";
import { test } from "node:test";

import { LINEAR, breakLabel, foldIdle } from "../src/timeFold.ts";

const M = 60_000;
const H = 60 * M;

test("one burst, or gaps under the threshold, fold nothing", () => {
  const single = foldIdle([[0, 10 * M], [5 * M, 20 * M]]);
  assert.equal(single.breaks.length, 0);
  assert.equal(single.fold(15 * M), 15 * M);
  const near = foldIdle([[0, 10 * M], [30 * M, 40 * M]]);
  assert.equal(near.breaks.length, 0);
  assert.deepEqual(near.blocks, [{ from: 0, to: 40 * M }]);
  assert.equal(foldIdle([]), LINEAR);
});

test("a long gap shrinks to `keep`; times inside bursts keep their scale", () => {
  const fold = foldIdle([[0, H], [6 * H, 7 * H]], { keep: 10 * M });
  assert.equal(fold.breaks.length, 1);
  assert.deepEqual(fold.breaks[0], { at: H, from: H, to: 6 * H });
  // Inside the first burst: unchanged.
  assert.equal(fold.fold(30 * M), 30 * M);
  // The second burst starts right after the kept 10 minutes.
  assert.equal(fold.fold(6 * H), H + 10 * M);
  assert.equal(fold.fold(6 * H + 30 * M) - fold.fold(6 * H), 30 * M);
  // Inside the gap: squeezed linearly.
  assert.equal(fold.fold(3.5 * H), H + 5 * M);
  // After everything: shifted like the last burst.
  assert.equal(fold.fold(8 * H), 8 * H - (5 * H - 10 * M));
});

test("unfold inverts fold everywhere, and fold is monotonic", () => {
  const fold = foldIdle([[0, H], [5 * H, 5.5 * H], [30 * H, 31 * H]], { keep: 6 * M });
  let previous = -Infinity;
  for (let t = -H; t <= 33 * H; t += 7 * M) {
    const folded = fold.fold(t);
    assert.ok(folded >= previous, `monotonic at ${t}`);
    previous = folded;
    assert.ok(Math.abs(fold.unfold(folded) - t) < 1e-6, `round trip at ${t}`);
  }
  assert.equal(fold.breaks.length, 2);
  assert.equal(fold.breaks[1].at, fold.fold(5.5 * H));
});

test("keep defaults to 3% of the active time, within 2..30 minutes", () => {
  const small = foldIdle([[0, 10 * M], [10 * H, 10 * H + 10 * M]]);
  assert.equal(small.keep, 2 * M);
  const big = foldIdle([[0, 10 * H], [30 * H, 40 * H]]);
  assert.equal(big.keep, 30 * M);
  const mid = foldIdle([[0, 5 * H], [20 * H, 25 * H]]);
  assert.equal(mid.keep, 18 * M);
});

test("break labels", () => {
  assert.equal(breakLabel({ at: 0, from: 0, to: 45 * M }), "45m idle");
  assert.equal(breakLabel({ at: 0, from: 0, to: 5 * H + 12 * M }), "5h 12m idle");
  assert.equal(breakLabel({ at: 0, from: 0, to: 26 * H }), "26h idle");
  assert.equal(breakLabel({ at: 0, from: 0, to: 72 * H }), "3d idle");
});

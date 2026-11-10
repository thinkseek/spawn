import assert from "node:assert/strict";
import { test } from "node:test";

import { foldIdle, foldIdleSteady, sameFoldMap } from "../src/timeFold.ts";
import { todayWindow } from "../src/timeWindow.ts";

const T0 = 1_780_000_000_000;
const M = 60_000;
const H = 60 * M;
const DAY = 24 * H;
const identity = (time: number) => time;

test("the missions open on the last 24 h, followed to the live end (design §4.4)", () => {
  // Six days of tasks, one open now.
  const now = T0 + 6 * DAY;
  assert.deepEqual(todayWindow(identity, { start: T0, end: now }, now, DAY), {
    mode: "since",
    from: now - DAY,
  });
  // Through the fold: the same real time, on the folded axis.
  const shift = (time: number) => time - 5 * H;
  assert.deepEqual(todayWindow(shift, { start: T0, end: now }, now, DAY), {
    mode: "since",
    from: now - DAY - 5 * H,
  });
  // Quiet for three hours: still the last 24 h before now.
  assert.deepEqual(todayWindow(identity, { start: T0, end: now - 3 * H }, now, DAY), {
    mode: "since",
    from: now - DAY,
  });
  // Quiet for two days: the last day with any work, not an empty plot.
  assert.deepEqual(todayWindow(identity, { start: T0, end: now - 2 * DAY }, now, DAY), {
    mode: "since",
    from: now - 3 * DAY,
  });
  // Younger than a day: the whole of it.
  assert.deepEqual(todayWindow(identity, { start: now - 5 * H, end: now }, now, DAY), { mode: "fit" });
  assert.deepEqual(todayWindow(identity, { start: now - DAY, end: now }, now, DAY), { mode: "fit" });
});

test("the missions fold's band width holds while an open bar grows", () => {
  // Two bursts a day apart, the second still open: its end is now.
  const at = (now: number) =>
    [
      [T0, T0 + 2 * H],
      [T0 + DAY, now],
    ] as const;
  const plain = [foldIdle(at(T0 + DAY + H)), foldIdle(at(T0 + DAY + H + 1_000))];
  assert.notEqual(plain[0].keep, plain[1].keep, "3% of a growing active time moves each second");
  assert.ok(!sameFoldMap(plain[0], plain[1]));
  const steady = [foldIdleSteady(at(T0 + DAY + H)), foldIdleSteady(at(T0 + DAY + H + 1_000))];
  assert.equal(steady[0].keep, steady[1].keep);
  assert.ok(sameFoldMap(steady[0], steady[1]), "so the window is not refolded each second");
  // A power of two, at most the plain width and more than half of it.
  const keep = steady[0].keep;
  assert.equal(2 ** Math.log2(keep), keep);
  assert.equal(Number.isInteger(Math.log2(keep)), true);
  assert.ok(keep <= plain[0].keep && keep > plain[0].keep / 2, `${keep} vs ${plain[0].keep}`);
  // Nothing to fold, or a keep given: as foldIdle.
  assert.equal(foldIdleSteady([[T0, T0 + H]]).breaks.length, 0);
  assert.equal(foldIdleSteady(at(T0 + DAY + H), { keep: 1234 }).keep, 1234);
});

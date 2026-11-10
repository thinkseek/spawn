/* Folding idle time out of an axis. Mission work comes in bursts (an hour
 * of tasks, then nothing for a day), and on a linear axis the bursts shrink
 * to slivers. A fold keeps every stretch of activity at its true scale and
 * shrinks each idle gap of at least `minGap` to `keep`, marked as a break,
 * so the order of events and every duration inside a burst stay true while
 * the empty days stop taking the width. Pure, tested in
 * web/test/timeFold.test.ts. */

export interface Break {
  /** Folded position where the break starts; it spans `keep`. */
  at: number;
  /** The real idle stretch it stands for. */
  from: number;
  to: number;
}

export interface TimeFold {
  /** Real time to folded time. Monotonic. */
  fold: (time: number) => number;
  /** Folded time back to real time. */
  unfold: (folded: number) => number;
  breaks: Break[];
  /** Folded width of each break. */
  keep: number;
  /** The real stretches kept at true scale, oldest first. */
  blocks: Array<{ from: number; to: number }>;
}

/** No fold: the identity. */
export const LINEAR: TimeFold = {
  fold: (time) => time,
  unfold: (folded) => folded,
  breaks: [],
  keep: 0,
  blocks: [],
};

export const MIN_GAP_MS = 30 * 60_000;

/** Fold the idle gaps between `intervals` ([start, end] in real ms). A gap
 * shorter than `minGap` stays as it is. Each break keeps `keep` ms of width:
 * by default 3% of the active time, between 2 and 30 minutes. */
export function foldIdle(
  intervals: ReadonlyArray<readonly [number, number]>,
  options: { minGap?: number; keep?: number } = {},
): TimeFold {
  const minGap = options.minGap ?? MIN_GAP_MS;
  const sorted = intervals
    .map(([from, to]) => [from, Math.max(from, to)] as const)
    .filter(([from, to]) => Number.isFinite(from) && Number.isFinite(to))
    .sort((a, b) => a[0] - b[0]);
  if (sorted.length === 0) return LINEAR;

  // Merge into blocks of activity, joining across gaps under minGap.
  const blocks: Array<{ from: number; to: number }> = [];
  for (const [from, to] of sorted) {
    const last = blocks.at(-1);
    if (last && from - last.to < minGap) last.to = Math.max(last.to, to);
    else blocks.push({ from, to });
  }
  if (blocks.length === 1) return { ...LINEAR, blocks };

  const active = blocks.reduce((sum, block) => sum + (block.to - block.from), 0);
  const keep =
    options.keep ?? Math.min(Math.max(active * 0.03, 2 * 60_000), 30 * 60_000);

  // removed[i]: real time cut out before block i.
  const removed: number[] = [0];
  const breaks: Break[] = [];
  for (let index = 1; index < blocks.length; index += 1) {
    const from = blocks[index - 1].to;
    const to = blocks[index].from;
    const cut = Math.max(0, to - from - keep);
    breaks.push({ at: from - removed[index - 1], from, to });
    removed.push(removed[index - 1] + cut);
  }

  const fold = (time: number): number => {
    // Find the last block starting at or before `time`.
    let index = 0;
    while (index + 1 < blocks.length && blocks[index + 1].from <= time) index += 1;
    const block = blocks[index];
    if (time <= block.to || index === blocks.length - 1) return time - removed[index];
    // Inside the gap after block `index`: squeeze it into `keep`.
    const gap = blocks[index + 1].from - block.to;
    const start = block.to - removed[index];
    return start + ((time - block.to) / gap) * Math.min(gap, keep);
  };

  const unfold = (folded: number): number => {
    let index = 0;
    while (
      index + 1 < blocks.length &&
      blocks[index + 1].from - removed[index + 1] <= folded
    ) {
      index += 1;
    }
    const block = blocks[index];
    const blockEnd = block.to - removed[index];
    if (folded <= blockEnd || index === blocks.length - 1) return folded + removed[index];
    const gap = blocks[index + 1].from - block.to;
    return block.to + ((folded - blockEnd) / Math.min(gap, keep)) * gap;
  };

  return { fold, unfold, breaks, keep, blocks };
}

/** foldIdle with its band width rounded down to a power of two (from its
 * own 3% of the active time): the missions timeline's fold. An open bar
 * grows the active time each second, and a keep that followed it would
 * move every folded time after the first break each second; rounded, it
 * holds until the active time doubles (the window follows when it moves,
 * refollow). A `keep` given is used as it is. */
export function foldIdleSteady(
  intervals: ReadonlyArray<readonly [number, number]>,
  options: { minGap?: number; keep?: number } = {},
): TimeFold {
  const first = foldIdle(intervals, options);
  if (first.breaks.length === 0 || options.keep !== undefined) return first;
  return foldIdle(intervals, { ...options, keep: 2 ** Math.floor(Math.log2(Math.max(first.keep, 1))) });
}

/** Whether two folds map every time alike. A fold's mapping is its breaks
 * (each idle stretch's real ends) and `keep`, nothing else: its first
 * block's start and its last block's end move no time, so a live trace's
 * fold, rebuilt each second as its last block grows toward now, is the
 * same map until a break or the band width changes (rev-f F4). With no
 * break, any fold is the identity.
 *
 * One more case is the same: a live end that is only now, after an idle
 * stretch (a waiting session's open turn, its last record an hour ago).
 * Its last block is that instant alone, and the break before it ends at
 * now, so it grows with the clock. Every gap that breaks is at least
 * minGap, never shorter than `keep`, so it folds to `keep` exactly, and
 * the times after it move by just what it grew: none moves before the
 * band, now stays where it was on the axis, and only real times inside
 * the 12–24 px band slide, which no window shows apart. */
export function sameFoldMap(a: TimeFold, b: TimeFold): boolean {
  if (a === b) return true;
  if (a.breaks.length === 0 && b.breaks.length === 0) return true;
  if (a.keep !== b.keep || a.breaks.length !== b.breaks.length) return false;
  const last = a.breaks.length - 1;
  return a.breaks.every(
    (item, index) =>
      item.from === b.breaks[index].from &&
      (item.to === b.breaks[index].to || (index === last && liveInstant(a) && liveInstant(b))),
  );
}

/** The fold's last block is one instant (so its last break ends there). */
function liveInstant(fold: TimeFold): boolean {
  const block = fold.blocks.at(-1);
  return !!block && block.from === block.to;
}

/** How long a break stands for, as a short label: "5h 12m idle". */
export function breakLabel(item: Break): string {
  const minutes = Math.round((item.to - item.from) / 60_000);
  if (minutes < 60) return `${minutes}m idle`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return minutes % 60 === 0 ? `${hours}h idle` : `${hours}h ${minutes % 60}m idle`;
  return `${Math.round(hours / 24)}d idle`;
}

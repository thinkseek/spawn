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

/** How long a break stands for, as a short label: "5h 12m idle". */
export function breakLabel(item: Break): string {
  const minutes = Math.round((item.to - item.from) / 60_000);
  if (minutes < 60) return `${minutes}m idle`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return minutes % 60 === 0 ? `${hours}h idle` : `${hours}h ${minutes % 60}m idle`;
  return `${Math.round(hours / 24)}d idle`;
}

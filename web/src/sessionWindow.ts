/* A session's events as a window that ends at its newest event.
 *
 * /api/session pages forward from `after`, and a long-running agent has
 * thousands of events: the trace reads the newest stretch first (api.ts
 * `fetchSessionTail`) and older ones only when asked. A window records the
 * span of sequence numbers it has read in full, `(after, through]`, not
 * only the events it holds: the session view hides the observer's events,
 * so a span can hold fewer events than it has numbers, or none.
 *
 * - A new tail that does not reach the window held (a hidden tab stops the
 *   poll; meanwhile more than a page was written) starts the window again
 *   from the tail: what was held is dropped, never joined across a hole.
 * - Reading back keeps going, page by page, until it has the events it
 *   wants or reaches the session's start (at most `MAX_PAGES` reads), and
 *   the window's start moves back over every span read, empty or not, so
 *   "Earlier" never offers the same empty span again.
 *
 * Pure, with the reader passed in; tested in web/test/sessionWindow.test.ts. */

import type { TimelineEvent } from "./api";

/** How many events a page holds, and how many visible events a read
 * back looks for. */
export const PAGE_EVENTS = 300;

/** The most pages one read back makes. */
export const MAX_PAGES = 8;

/** Events read in full over the sequence numbers `(after, through]`. */
export interface EventWindow {
  events: TimelineEvent[];
  after: number;
  through: number;
}

/** Read `limit` sequence numbers after `after`: the events among them. */
export type PageReader = (after: number, limit: number) => Promise<TimelineEvent[]>;

/** Two runs of events merged: one per sequence number (the later read
 * wins), in log order. */
export function mergeEvents(kept: readonly TimelineEvent[], fresh: readonly TimelineEvent[]): TimelineEvent[] {
  const bySeq = new Map<number, TimelineEvent>();
  for (const event of kept) bySeq.set(event.session_seq, event);
  for (const event of fresh) bySeq.set(event.session_seq, event);
  return [...bySeq.values()].sort((a, b) => a.session_seq - b.session_seq || a.event_id - b.event_id);
}

/** Read back from `window`'s start until it holds `want` more events or
 * reaches the session's start, at most `MAX_PAGES` pages of `size`. */
export async function readBack(
  window: EventWindow,
  read: PageReader,
  want = PAGE_EVENTS,
  size = PAGE_EVENTS,
): Promise<EventWindow> {
  let { events, after } = window;
  let found = 0;
  for (let page = 0; page < MAX_PAGES && after > 0 && found < want; page += 1) {
    const from = Math.max(0, after - size);
    const older = await read(from, after - from);
    events = mergeEvents(older, events);
    found += older.length;
    after = from;
  }
  return { events, after, through: window.through };
}

/** The newest `size` sequence numbers up to `latest`, read back further
 * until `want` events are held (the observer's hidden events do not
 * count) or the session's start is reached. */
export async function readTail(
  latest: number,
  read: PageReader,
  want = PAGE_EVENTS,
  size = PAGE_EVENTS,
): Promise<EventWindow> {
  const after = Math.max(0, latest - size);
  const events = await read(after, latest - after);
  const window = { events, after, through: latest };
  return events.length >= want ? window : readBack(window, read, want - events.length, size);
}

/** A new tail joined to the window held: merged when they touch (the tail
 * starts at or before what the window has read through), else the tail
 * alone, as what lies between was never read. */
export function joinTail(held: EventWindow | null, tail: EventWindow): EventWindow {
  if (!held || held.through < tail.after) return tail;
  return {
    events: mergeEvents(held.events, tail.events),
    after: Math.min(held.after, tail.after),
    through: Math.max(held.through, tail.through),
  };
}

/** Is there anything before the window? */
export function hasEarlier(window: EventWindow): boolean {
  return window.after > 0;
}

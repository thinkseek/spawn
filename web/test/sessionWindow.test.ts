import assert from "node:assert/strict";
import { test } from "node:test";

import type { TimelineEvent } from "../src/api.ts";
import { MAX_PAGES, hasEarlier, joinTail, mergeEvents, readBack, readTail } from "../src/sessionWindow.ts";
import type { EventWindow } from "../src/sessionWindow.ts";

const event = (seq: number, name = "message.received"): TimelineEvent => ({
  session_seq: seq,
  event_id: 1000 + seq,
  execution_id: "ex_1",
  source: "claude",
  event_name: name,
  detail: {},
  recorded_at_ms: 1_780_000_000_000 + seq * 1000,
});
const span = (from: number, to: number) => Array.from({ length: to - from + 1 }, (_, i) => event(from + i));
const window = (from: number, to: number): EventWindow => ({ events: span(from, to), after: from - 1, through: to });

/** A session of `count` sequence numbers whose visible events are those
 * `visible` keeps; the reader the server would be, counting its reads. */
function session(count: number, visible: (seq: number) => boolean = () => true) {
  const reads: Array<[number, number]> = [];
  // As the server reads: the first `limit` visible rows after `after`, so
  // past a hidden stretch a read runs on beyond the span it asked for.
  const read = async (after: number, limit: number) => {
    reads.push([after, limit]);
    return span(after + 1, count)
      .filter((entry) => visible(entry.session_seq))
      .slice(0, limit);
  };
  return { read, reads };
}

test("pages merge by sequence number: a re-read replaces", () => {
  const merged = mergeEvents([event(5), event(6), event(7)], [event(7, "turn.stopped"), event(8)]);
  assert.deepEqual(
    merged.map((entry) => [entry.session_seq, entry.event_name]),
    [
      [5, "message.received"],
      [6, "message.received"],
      [7, "turn.stopped"],
      [8, "message.received"],
    ],
  );
});

test("rev-e's case: held 1..300, then a tail of 701..1000 after a hidden tab: the window starts again, no hole", () => {
  const held = window(1, 300);
  const tail = window(701, 1000);
  const joined = joinTail(held, tail);
  assert.equal(joined.events.length, 300);
  assert.deepEqual([joined.events[0].session_seq, joined.after, joined.through], [701, 700, 1000]);
  assert.ok(hasEarlier(joined), "301..700 is offered as the earlier page");
  // Every sequence number in (after, through] is held: nothing is missing.
  const seqs = new Set(joined.events.map((entry) => entry.session_seq));
  for (let seq = joined.after + 1; seq <= joined.through; seq += 1) assert.ok(seqs.has(seq), `seq ${seq}`);
});

test("a tail that touches the window held joins it", () => {
  const joined = joinTail(window(1, 300), window(201, 500));
  assert.deepEqual([joined.events.length, joined.after, joined.through], [500, 0, 500]);
  assert.equal(hasEarlier(joined), false);
  // Exactly touching: held through 300, the tail after 300.
  assert.equal(joinTail(window(1, 300), window(301, 600)).events.length, 600);
  assert.equal(joinTail(null, window(5, 9)).events.length, 5);
});

test("the tail reads back past hidden events until it holds a page, or reaches the start", async () => {
  // Only every tenth sequence number is visible (the rest are observer
  // events): one span of 300 holds 30.
  const sparse = session(10_000, (seq) => seq % 10 === 0);
  const tail = await readTail(10_000, sparse.read, 100, 300);
  assert.ok(tail.events.length >= 100, `${tail.events.length}`);
  assert.equal(tail.through, 10_000);
  assert.equal(tail.after, 10_000 - 4 * 300, "four spans of 30 make 100");
  // A session with nothing visible at all: bounded reads, then it stops.
  const empty = session(100_000, () => false);
  const nothing = await readTail(100_000, empty.read, 300, 300);
  assert.equal(nothing.events.length, 0);
  assert.equal(empty.reads.length, 1 + MAX_PAGES);
  // A short session: read to its start.
  const short = await readTail(120, session(120).read);
  assert.deepEqual([short.events.length, short.after, hasEarlier(short)], [120, 0, false]);
});

test("an earlier read that finds nothing still moves the window's start, so it is never offered again", async () => {
  const hiddenStretch = session(5_000, (seq) => seq <= 1_000 || seq > 4_000);
  const held = window(4_001, 5_000);
  const back = await readBack(held, hiddenStretch.read, 300, 300);
  assert.ok(back.after < held.after, "the start moved back");
  assert.deepEqual(
    hiddenStretch.reads,
    [
      [3_700, 300],
      [3_400, 300],
      [3_100, 300],
      [2_800, 300],
      [2_500, 300],
      [2_200, 300],
      [1_900, 300],
      [1_600, 300],
    ],
    "eight empty spans read, each once",
  );
  assert.equal(back.after, 1_600);
  // The next read goes on from there and finds the visible events.
  const again = await readBack(back, hiddenStretch.read, 300, 300);
  assert.ok(again.events.some((entry) => entry.session_seq === 1_000));
});

test("rows a read returns past the window's start are not counted as new: one read back brings a page of new events", async () => {
  // 1..2000 visible, 2001..2290 hidden, 2291..2600 visible; held from 2301.
  const mixed = session(2_600, (seq) => seq <= 2_000 || seq > 2_290);
  const held: EventWindow = { events: span(2_301, 2_600), after: 2_300, through: 2_600 };
  const back = await readBack(held, mixed.read, 300, 300);
  const fresh = back.events.filter((entry) => entry.session_seq <= held.after);
  assert.ok(fresh.length >= 300, `only ${fresh.length} new events`);
});

import assert from "node:assert/strict";
import { test } from "node:test";

import type { ChannelLike, LiveDeps, LiveSignal, LocksLike } from "../src/stream.ts";
import {
  ALL_TOPICS,
  BACKOFF_MAX_MS,
  STALE_MS,
  controlTopics,
  recordType,
  LiveStream,
  SseParser,
  UNAUTHORIZED_RETRY_MS,
  backoffMs,
  frameSignal,
  streamHeaders,
  streamUrl,
  topicsOf,
} from "../src/stream.ts";

test("the parser follows the event-stream rules: fields, comments, ids, retry", () => {
  const parser = new SseParser();
  const frames = parser.feed(
    "retry: 3000\n\n: keepalive\n\nevent: ready\ndata: {\"latest_event_id\":7,\"replayed\":0}\n\n" +
      "id: 8\nevent: session.linked\ndata: {\"a\":1}\n\nid: 9\ndata: line one\ndata: line two\n\n",
  );
  assert.deepEqual(frames, [
    { event: "", data: "", id: null, retry: 3000 },
    { event: "ready", data: '{"latest_event_id":7,"replayed":0}', id: null, retry: null },
    { event: "session.linked", data: '{"a":1}', id: "8", retry: null },
    { event: "message", data: "line one\nline two", id: "9", retry: null },
  ]);
  assert.equal(parser.lastEventId, "9");
});

test("the parser joins frames split anywhere, CRLF and CR included, and drops a BOM", () => {
  const whole = "﻿id: 1\r\nevent: a\r\ndata: x\r\n\r\nid: 2\revent: b\rdata: y\r\r";
  for (let split = 0; split <= whole.length; split += 1) {
    const parser = new SseParser();
    const frames = [...parser.feed(whole.slice(0, split)), ...parser.feed(whole.slice(split))];
    assert.deepEqual(
      frames.map((frame) => [frame.id, frame.event, frame.data]),
      [
        ["1", "a", "x"],
        ["2", "b", "y"],
      ],
      `split at ${split}`,
    );
  }
  // One character at a time.
  const parser = new SseParser();
  const frames = [...whole].flatMap((character) => parser.feed(character));
  assert.equal(frames.length, 2);
});

test("a field without a colon, an id with a NUL and a bad retry are handled as the standard says", () => {
  const parser = new SseParser();
  const frames = parser.feed("id: 4\n\ndata\n\nid: 5\u0000x\nretry: 1s\ndata: z\n\n");
  // `data` alone is an empty data line: a frame with "" as data.
  assert.deepEqual(frames, [
    { event: "message", data: "", id: "4", retry: null },
    { event: "message", data: "z", id: "4", retry: null },
  ]);
});

test("frames become signals and move the cursor; nothing is shown twice", () => {
  const frame = (event: string, data: string, id: string | null = null) => ({ event, data, id, retry: null });
  // Ready with no cursor: the stream starts live at latest_event_id.
  assert.deepEqual(frameSignal(frame("ready", '{"latest_event_id":40,"replayed":0}'), null), {
    signal: { kind: "ready", latest_event_id: 40, replayed: 0 },
    cursor: 40,
  });
  // Ready while catching up keeps the cursor: the replayed frames move it.
  assert.equal(frameSignal(frame("ready", '{"latest_event_id":40,"replayed":3}'), 37).cursor, 37);
  const event = frameSignal(frame("task.done", '{"id":41}', "41"), 40);
  assert.deepEqual(event, { signal: { kind: "event", id: 41, name: "task.done", data: { id: 41 } }, cursor: 41 });
  // An event at or before the cursor is not shown again.
  assert.deepEqual(frameSignal(frame("task.done", "{}", "41"), 41), { signal: null, cursor: 41 });
  // A resync moves to the log's end, but a slow page keeps its cursor.
  const behind = frameSignal(frame("resync", '{"reason":"behind","after_event_id":3,"latest_event_id":900}'), 3);
  assert.deepEqual(behind, { signal: { kind: "resync", reason: "behind", latest_event_id: 900 }, cursor: 900 });
  const slow = frameSignal(frame("resync", '{"reason":"slow_consumer","after_event_id":50,"latest_event_id":null}'), 50);
  assert.equal(slow.cursor, 50);
  // A retry-only frame or an id that is no number is no signal.
  assert.equal(frameSignal(frame("", ""), 1).signal, null);
  assert.equal(frameSignal(frame("x", "{}", "abc"), 1).signal, null);
});

test("the backoff doubles from the minimum to the cap, jittered between half and all", () => {
  assert.equal(backoffMs(0, 0), 500);
  assert.equal(backoffMs(0, 0.999999), 1000);
  assert.equal(backoffMs(3, 1), 8000);
  assert.equal(backoffMs(20, 1), BACKOFF_MAX_MS);
  assert.equal(backoffMs(20, 0), BACKOFF_MAX_MS / 2);
  // The server's retry: raises the floor.
  assert.equal(backoffMs(0, 1, 3000), 3000);
  for (let attempt = 0; attempt < 12; attempt += 1) {
    assert.ok(backoffMs(attempt, 1) >= backoffMs(Math.max(0, attempt - 1), 1));
  }
});

test("a resume carries the cursor as Last-Event-ID and ?after=, never a token in the URL", () => {
  assert.equal(streamUrl("/api/stream", null), "/api/stream");
  assert.equal(streamUrl("/api/stream", 42), "/api/stream?after=42");
  assert.equal(streamUrl("/api/stream?x=1", 0), "/api/stream?x=1&after=0");
  const headers = streamHeaders({ Accept: "application/json", Authorization: "Bearer t0k" }, 42);
  assert.deepEqual(headers, {
    Accept: "text/event-stream",
    Authorization: "Bearer t0k",
    "Cache-Control": "no-cache",
    "Last-Event-ID": "42",
  });
  assert.ok(!("Last-Event-ID" in streamHeaders({}, null)));
});

test("signals name the views they make stale", () => {
  const event = (name: string): LiveSignal => ({ kind: "event", id: 1, name, data: null });
  assert.ok(topicsOf(event("mission.halted")).includes("missions"));
  assert.ok(topicsOf(event("task.verdict")).includes("missions"));
  assert.ok(topicsOf(event("node.heartbeat")).includes("nodes"));
  assert.ok(!topicsOf(event("command.accepted")).includes("nodes"));
  assert.ok(topicsOf(event("command.accepted")).includes("approvals"));
  assert.deepEqual(topicsOf({ kind: "resync", reason: "behind", latest_event_id: 1 }), ALL_TOPICS);
  assert.deepEqual(topicsOf({ kind: "ready", latest_event_id: 1, replayed: 0 }), []);
  assert.deepEqual(topicsOf({ kind: "ready", latest_event_id: 1, replayed: 2 }), ALL_TOPICS);
});

/** A response whose body yields `chunks`, then ends (or hangs: `hang`). */
function response(status: number, chunks: string[], hang = false) {
  const encoder = new TextEncoder();
  let at = 0;
  let release: ((value: { done: boolean }) => void) | null = null;
  return {
    ok: status >= 200 && status < 300,
    status,
    body: {
      getReader: () => ({
        read: async () => {
          if (at < chunks.length) return { done: false, value: encoder.encode(chunks[at++]) };
          if (hang) return new Promise<{ done: boolean; value?: Uint8Array }>((resolve) => (release = resolve));
          return { done: true };
        },
        // As a real reader: a cancel ends the read that waits.
        cancel: async () => release?.({ done: true }),
      }),
    },
  };
}

/** Timers run by hand. */
function clock() {
  let now = 1_000;
  let next = 1;
  const timers = new Map<number, { at: number; callback: () => void }>();
  return {
    now: () => now,
    setTimer: (callback: () => void, ms: number) => {
      const id = next++;
      timers.set(id, { at: now + ms, callback });
      return id;
    },
    clearTimer: (id: unknown) => void timers.delete(id as number),
    pending: () => [...timers.values()].map((timer) => timer.at - now).sort((a, b) => a - b),
    /** The clock moves with no timer run (a laptop that slept). */
    jump(ms: number) {
      now += ms;
    },
    advance(ms: number) {
      now += ms;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= now) {
          timers.delete(id);
          timer.callback();
        }
      }
    },
  };
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function deps(overrides: Partial<LiveDeps>, time = clock()): LiveDeps {
  return {
    fetch: async () => response(200, [], true),
    headers: () => ({ Authorization: "Bearer t0k" }),
    random: () => 1,
    now: time.now,
    setTimer: time.setTimer,
    clearTimer: time.clearTimer,
    ...overrides,
  };
}

test("the client reads events, then reconnects with backoff resuming after the last id", async () => {
  const time = clock();
  const calls: { url: string; headers: Record<string, string> }[] = [];
  const answers = [
    response(200, ["retry: 2000\n\nevent: ready\ndata: {\"latest_event_id\":10,\"replayed\":0}\n\n", "id: 11\nevent: task.done\ndata: {}\n\n"]),
    response(503, []),
    response(200, ["event: ready\ndata: {\"latest_event_id\":12,\"replayed\":1}\n\nid: 12\nevent: node.heartbeat\ndata: {}\n\n"], true),
  ];
  const stream = new LiveStream(
    deps(
      {
        fetch: async (url, init) => {
          calls.push({ url, headers: init.headers });
          return answers.shift() ?? response(200, [], true);
        },
      },
      time,
    ),
  );
  const seen: LiveSignal[] = [];
  stream.subscribe((signal) => seen.push(signal));
  stream.start();
  await tick();
  await tick();
  assert.equal(calls[0].url, "/api/stream");
  assert.equal(calls[0].headers.Authorization, "Bearer t0k");
  assert.ok(!calls[0].url.includes("t0k"), "the token is never in the URL");
  assert.equal(stream.lastEventId, 11);
  assert.equal(stream.status.state, "retrying");
  // The server's retry: 2000 is the floor of the first pause.
  assert.deepEqual(time.pending(), [2000]);
  time.advance(2000);
  await tick();
  await tick();
  assert.equal(calls[1].url, "/api/stream?after=11");
  assert.equal(calls[1].headers["Last-Event-ID"], "11");
  // A 503 doubles the pause.
  assert.deepEqual(time.pending(), [4000]);
  time.advance(4000);
  await tick();
  await tick();
  assert.equal(stream.status.state, "live");
  assert.equal(stream.lastEventId, 12);
  const events = seen.filter((signal) => signal.kind === "event").map((signal) => signal.kind === "event" && signal.id);
  assert.deepEqual(events, [11, 12]);
});

test("a 401 waits long, and a page shown again connects at once", async () => {
  const time = clock();
  let calls = 0;
  let listener: (() => void) | null = null;
  const page = { visibilityState: "visible", addEventListener: (_: string, fn: () => void) => void (listener = fn) };
  const stream = new LiveStream(
    deps(
      {
        fetch: async () => {
          calls += 1;
          return calls === 1 ? response(401, []) : response(200, [], true);
        },
        document: page,
      },
      time,
    ),
  );
  stream.start();
  await tick();
  assert.equal(stream.status.state, "unauthorized");
  assert.deepEqual(time.pending(), [UNAUTHORIZED_RETRY_MS]);
  // Hidden then shown again: no wait for the minute's pause.
  page.visibilityState = "hidden";
  listener!();
  page.visibilityState = "visible";
  listener!();
  await tick();
  assert.equal(calls, 2, "connected at once, not after the pause");
  assert.equal(stream.status.state, "connecting");
});

test("a stream silent past the keepalive is dropped and made again", async () => {
  const time = clock();
  let calls = 0;
  const stream = new LiveStream(
    deps(
      {
        fetch: async () => {
          calls += 1;
          return response(200, ['event: ready\ndata: {"latest_event_id":1,"replayed":0}\n\n'], true);
        },
      },
      time,
    ),
  );
  stream.start();
  await tick();
  await tick();
  assert.equal(stream.status.state, "live");
  time.advance(40_000);
  await tick();
  await tick();
  assert.equal(stream.status.state, "retrying");
  assert.match(stream.status.error ?? "", /quiet/);
  time.advance(1_000);
  await tick();
  assert.equal(calls, 2);
});

/** BroadcastChannels of one origin, in memory. */
function channels() {
  const open: { listeners: ((event: { data: unknown }) => void)[] }[] = [];
  return (): ChannelLike => {
    const self = { listeners: [] as ((event: { data: unknown }) => void)[] };
    open.push(self);
    return {
      postMessage: (message) => {
        for (const other of open) {
          if (other !== self) for (const fn of other.listeners) fn({ data: structuredClone(message) });
        }
      },
      addEventListener: (_type, fn) => void self.listeners.push(fn),
      close: () => undefined,
    };
  };
}

/** One Web Lock among tabs: the first gets it, the rest wait. */
function lock(): LocksLike {
  let held = false;
  const waiting: (() => void)[] = [];
  return {
    request: (_name, callback) => {
      if (!held) {
        held = true;
        return callback();
      }
      return new Promise((resolve) => waiting.push(() => resolve(callback())));
    },
  };
}

test("tabs share one stream: the lock's holder reads it, the others hear it over the channel", async () => {
  const channel = channels();
  const locks = lock();
  let fetches = 0;
  const make = () =>
    new LiveStream(
      deps({
        channel,
        locks,
        fetch: async () => {
          fetches += 1;
          return response(
            200,
            ['event: ready\ndata: {"latest_event_id":5,"replayed":0}\n\n', "id: 6\nevent: mission.halted\ndata: {}\n\n"],
            true,
          );
        },
      }),
    );
  const follower = make();
  const leader = make();
  // The first started takes the lock.
  const heard: LiveSignal[] = [];
  follower.subscribe((signal) => heard.push(signal));
  follower.start();
  leader.start();
  await tick();
  await tick();
  assert.equal(fetches, 1, "one stream for both tabs");
  assert.equal(follower.status.leader, true);
  assert.equal(leader.status.leader, false);
  assert.equal(leader.status.state, "live", "the follower shows the leader's state");
  assert.equal(leader.lastEventId, 6, "the follower keeps the cursor for when it leads");
  assert.ok(heard.some((signal) => signal.kind === "event" && signal.id === 6));
});

test("without BroadcastChannel or Web Locks each tab reads its own stream", async () => {
  let fetches = 0;
  const make = (extra: Partial<LiveDeps>) =>
    new LiveStream(
      deps({
        ...extra,
        fetch: async () => {
          fetches += 1;
          return response(200, [], true);
        },
      }),
    );
  make({ channel: () => null, locks: lock() }).start();
  make({ channel: () => { throw new Error("no BroadcastChannel"); }, locks: lock() }).start();
  make({ channel: channels(), locks: null }).start();
  await tick();
  assert.equal(fetches, 3);
});

/** A `control.recorded` frame as `/api/stream` sends it: the `/api/events`
 * row (crates/spawn-cli/src/serve/stream.rs), whose `detail` holds the
 * control record (schema.sql `spawn_control_record`). */
function controlFrame(id: number, record: Record<string, unknown> | null) {
  return {
    event: "control.recorded",
    id: String(id),
    retry: null,
    data: JSON.stringify({
      id,
      agent_id: "agent_control",
      session_id: "session_control",
      execution_id: null,
      source: "control",
      event_name: "control.recorded",
      detail: record === null ? {} : { record },
      recorded_at_ms: 1_700_000_000_000,
    }),
  };
}

test("control.recorded frames make the boards of their record's type stale", () => {
  const topics = (record: Record<string, unknown> | null) => {
    const { signal } = frameSignal(controlFrame(90, record), 89);
    assert.ok(signal && signal.kind === "event");
    return topicsOf(signal);
  };
  const has = (list: readonly string[], ...wanted: string[]) => wanted.every((topic) => list.includes(topic));
  for (const type of ["task.assigned", "task.gated", "task.landed", "objective.verified", "mission.configured", "mission.halted"]) {
    const list = topics({ type, id: 7 });
    assert.ok(has(list, "missions", "fleet", "approvals", "digest"), `${type}: ${list}`);
    assert.ok(!list.includes("nodes") && !list.includes("alerts"), `${type}: ${list}`);
  }
  for (const type of ["alert.fired", "alert.cleared"]) {
    const list = topics({ type, key: "agent_stuck:s" });
    assert.ok(has(list, "alerts") && !list.includes("missions"), `${type}: ${list}`);
  }
  assert.ok(has(topics({ type: "node.joined", name: "laptop" }), "nodes"));
  // A record of no type known, or none, marks all three.
  assert.ok(has(topics({ type: "something.new" }), "missions", "alerts", "nodes"));
  assert.ok(has(topics(null), "missions", "alerts", "nodes"));
  assert.equal(recordType({ detail: { record: { type: "task.gated" } } }), "task.gated");
  assert.equal(recordType({ detail: { record: { type: "" } } }), null);
  assert.equal(recordType(null), null);
  assert.deepEqual(controlTopics(null), ["missions", "alerts", "nodes"]);
  assert.deepEqual(controlTopics("resident.started"), ["missions"]);
});

test("a server that never sends its headers is a quiet stream, dropped and made again", async () => {
  const time = clock();
  let calls = 0;
  const stream = new LiveStream(
    deps(
      {
        fetch: async (_url, init) => {
          calls += 1;
          // Never answers; rejects only when aborted, as fetch does.
          return new Promise((_, reject) =>
            init.signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError"))),
          );
        },
      },
      time,
    ),
  );
  stream.start();
  await tick();
  assert.equal(stream.status.state, "connecting");
  time.advance(STALE_MS);
  await tick();
  await tick();
  assert.equal(stream.status.state, "retrying");
  assert.match(stream.status.error ?? "", /quiet/);
  assert.equal(calls, 1);
});

test("a page shown again makes a stale stream again at once, with no pause", async () => {
  const time = clock();
  let calls = 0;
  let listener: (() => void) | null = null;
  const page = { visibilityState: "visible", addEventListener: (_: string, fn: () => void) => void (listener = fn) };
  const stream = new LiveStream(
    deps(
      {
        fetch: async () => {
          calls += 1;
          return response(200, ['event: ready\ndata: {"latest_event_id":1,"replayed":0}\n\n'], true);
        },
        document: page,
        // Shared tabs: a hidden tab keeps its stream (only a lone one closes it).
        channel: channels(),
        locks: lock(),
      },
      time,
    ),
  );
  stream.start();
  await tick();
  await tick();
  assert.equal(stream.status.state, "live");
  // A sleeping laptop: the clock jumps past the keepalive before any timer runs.
  time.jump(STALE_MS + 1);
  listener!();
  await tick();
  await tick();
  assert.equal(calls, 2, "made again at once");
  assert.equal(stream.status.state, "live");
});

test("tabs that cannot share a stream close it while hidden and resume from the cursor", async () => {
  const time = clock();
  const urls: string[] = [];
  let listener: (() => void) | null = null;
  const page = { visibilityState: "visible", addEventListener: (_: string, fn: () => void) => void (listener = fn) };
  const stream = new LiveStream(
    deps(
      {
        fetch: async (url) => {
          urls.push(url);
          return response(
            200,
            ['event: ready\ndata: {"latest_event_id":20,"replayed":0}\n\n', "id: 21\nevent: task.done\ndata: {}\n\n"],
            true,
          );
        },
        document: page,
        channel: channels(),
        locks: null,
      },
      time,
    ),
  );
  stream.start();
  await tick();
  await tick();
  assert.equal(stream.lastEventId, 21);
  page.visibilityState = "hidden";
  listener!();
  await tick();
  assert.equal(stream.status.state, "paused");
  assert.deepEqual(time.pending().filter((ms) => ms < STALE_MS), [], "no reconnect while hidden");
  page.visibilityState = "visible";
  listener!();
  await tick();
  assert.deepEqual(urls, ["/api/stream", "/api/stream?after=21"]);
  // A lone tab that starts hidden does not connect until shown.
  const hidden = { visibilityState: "hidden", addEventListener: () => undefined };
  let fetched = 0;
  const later = new LiveStream(
    deps({ fetch: async () => (fetched++, response(200, [], true)), document: hidden, locks: null }),
  );
  later.start();
  await tick();
  assert.equal(fetched, 0);
  assert.equal(later.status.state, "paused");
});

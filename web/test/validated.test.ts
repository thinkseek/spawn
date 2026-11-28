// Run by rsi/scenarios/check-web.sh: `node --test test/`. The page's reads
// send the tag of what they read last and get the same object back on a
// 304; every read accepts an answer a second old; a failure carries the
// server's reason.
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  cacheControlFor,
  FRESH,
  formatBytes,
  markFresh,
  isCut,
  OnDemand,
  pageHidden,
  SHARED_FOR,
  utf8Bytes,
  Validated,
  type FetchLike,
} from "../src/validated.ts";

interface Seen {
  url: string;
  headers: Record<string, string>;
  cache: RequestCache | undefined;
}

/** A server that answers from `answers` in turn, honouring If-None-Match
 * against each answer's tag; it records what it was sent. */
function server(answers: Array<{ status?: number; body: unknown; etag?: string }>) {
  const seen: Seen[] = [];
  let index = 0;
  const fetcher: FetchLike = async (url, init) => {
    const headers = (init.headers ?? {}) as Record<string, string>;
    seen.push({ url, headers, cache: init.cache });
    const answer = answers[Math.min(index, answers.length - 1)];
    index += 1;
    const status = answer.status ?? 200;
    if (answer.etag && headers["If-None-Match"] === answer.etag && status === 200) {
      return new Response(null, { status: 304, headers: { ETag: answer.etag } });
    }
    return new Response(JSON.stringify(answer.body), {
      status,
      headers: answer.etag ? { ETag: answer.etag } : {},
    });
  };
  return { seen, fetcher };
}

const options = (fail = (status: number, detail: string) => new Error(`${status} ${detail}`)) => ({
  signal: new AbortController().signal,
  headers: { Authorization: "Bearer t" },
  fail,
});

test("a read sends the tag it holds and a 304 gives the very same object", async () => {
  const { seen, fetcher } = server([
    { body: { n: 1 }, etag: 'W/"a"' },
    { body: { n: 1 }, etag: 'W/"a"' },
    { body: { n: 2 }, etag: 'W/"b"' },
  ]);
  const reads = new Validated(fetcher);
  let normalized = 0;
  const normalize = (raw: unknown) => {
    normalized += 1;
    return { ...(raw as { n: number }) };
  };
  const first = await reads.read("/api/fleet", normalize, options());
  const second = await reads.read("/api/fleet", normalize, options());
  assert.equal(second, first, "the same object: nothing to render");
  assert.equal(normalized, 1, "a 304 is not normalized again");
  const third = await reads.read("/api/fleet", normalize, options());
  assert.deepEqual(third, { n: 2 });

  assert.equal(seen[0].headers["If-None-Match"], undefined, "nothing held yet");
  assert.equal(seen[1].headers["If-None-Match"], 'W/"a"');
  for (const request of seen) {
    assert.equal(request.headers.Authorization, "Bearer t");
    assert.equal(request.headers["Cache-Control"], SHARED_FOR);
    assert.equal(request.cache, "no-store", "the browser's cache stays out of it");
  }
});

test("each URL keeps its own answer, and an untagged answer is not kept", async () => {
  const { seen, fetcher } = server([
    { body: { n: 1 }, etag: 'W/"x"' },
    { body: { n: 2 } },
    { body: { n: 2 } },
  ]);
  const reads = new Validated(fetcher);
  await reads.read("/api/a", (raw) => raw, options());
  await reads.read("/api/b", (raw) => raw, options());
  await reads.read("/api/b", (raw) => raw, options());
  assert.equal(seen[1].headers["If-None-Match"], undefined, "another URL");
  assert.equal(seen[2].headers["If-None-Match"], undefined, "no tag was sent for /api/b");
  assert.equal(reads.size, 1);
});

test("a second reader of one URL gets the answer in its own reading", async () => {
  const { fetcher } = server([{ body: { n: 3 }, etag: 'W/"c"' }]);
  const reads = new Validated(fetcher);
  await reads.read("/api/objectives", (raw) => (raw as { n: number }).n, options());
  const other = await reads.read("/api/objectives", (raw) => `n=${(raw as { n: number }).n}`, options());
  assert.equal(other, "n=3");
});

test("a failed read throws with the server's reason and forgets nothing it held", async () => {
  const { seen, fetcher } = server([
    { body: { n: 1 }, etag: 'W/"a"' },
    { status: 500, body: { error: "the store is down" } },
    { body: { n: 1 }, etag: 'W/"a"' },
  ]);
  const reads = new Validated(fetcher);
  await reads.read("/api/x", (raw) => raw, options());
  await assert.rejects(
    reads.read("/api/x", (raw) => raw, options()),
    /500 the store is down/,
  );
  await reads.read("/api/x", (raw) => raw, options());
  assert.equal(seen[2].headers["If-None-Match"], 'W/"a"', "the tag survives a failure");
});

test("lengths and cuts are counted in UTF-8 bytes, as the server counts them", () => {
  assert.equal(utf8Bytes("abc"), 3);
  assert.equal(utf8Bytes("é"), 2);
  assert.equal(utf8Bytes("令"), 3);
  assert.equal(isCut("ab", 2), false);
  assert.equal(isCut("ab", 3), true);
  assert.equal(isCut("é", 2), false);
  assert.equal(isCut(null, 0), false);
  assert.equal(isCut(null, 5), true, "nothing sent of a five-byte text");
  assert.equal(isCut("x", null), false, "an older server sends no length");
  assert.equal(formatBytes(812), "812 B");
  assert.equal(formatBytes(4200), "4.2 KB");
  assert.equal(formatBytes(48_000), "48 KB");
  assert.equal(formatBytes(1_300_000), "1.3 MB");
});

test("a hidden page is one whose visibility says hidden", () => {
  assert.equal(pageHidden({ visibilityState: "hidden" }), true);
  assert.equal(pageHidden({ visibilityState: "visible" }), false);
  assert.equal(pageHidden(undefined), false);
});

test("a value read on demand is read once per key, and a failure is read again", async () => {
  const wanted = new OnDemand<string>(2);
  let reads = 0;
  const read = (value: string) => () => {
    reads += 1;
    return Promise.resolve(value);
  };
  const [first, second] = await Promise.all([
    wanted.get("7/4/2000", read("whole")),
    wanted.get("7/4/2000", read("other")),
  ]);
  assert.equal(first, "whole");
  assert.equal(second, "whole", "the second want shares the first read");
  assert.equal(reads, 1);

  await assert.rejects(wanted.get("7/5/10", () => Promise.reject(new Error("down"))));
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(await wanted.get("7/5/10", read("back")), "back", "a failure is not kept");

  await wanted.get("7/6/10", read("third"));
  assert.equal(wanted.size, 2, "past the limit the oldest goes");
  assert.equal(await wanted.get("7/4/2000", read("again")), "again", "the oldest was read again");
});

/** Review S1 (issue #91 P1b): a read the live stream asked for (its
 * signal marked fresh) sends `Cache-Control: no-cache`, so the server
 * computes the board after the event; a periodic poll keeps sharing the
 * second-old answer (`max-age=1`). Every read made with a marked signal is
 * fresh, a read awaited after another included. */
test("a live-triggered read asks for a fresh answer, a periodic one shares", async () => {
  const { seen, fetcher } = server([{ body: { n: 1 } }]);
  const reads = new Validated(fetcher);
  const periodic = options();
  await reads.read("/api/fleet", (raw) => raw, periodic);
  const live = { ...options(), signal: markFresh(new AbortController().signal) };
  await reads.read("/api/fleet", (raw) => raw, live);
  await reads.read("/api/attention", (raw) => raw, live);
  assert.deepEqual(
    seen.map((request) => request.headers["Cache-Control"]),
    [SHARED_FOR, FRESH, FRESH],
  );
  assert.equal(FRESH, "no-cache");
  assert.equal(SHARED_FOR, "max-age=1");
  assert.equal(cacheControlFor(undefined), SHARED_FOR);
  assert.equal(cacheControlFor(new AbortController().signal), SHARED_FOR);
});

test("clear forgets kept ETags so the next read omits If-None-Match", async () => {
  const { seen, fetcher } = server([
    { body: { n: 1 }, etag: 'W/"a"' },
    { body: { n: 1 }, etag: 'W/"a"' },
  ]);
  const reads = new Validated(fetcher);
  await reads.read("/api/overview", (raw) => raw, options());
  assert.equal(reads.size, 1);
  reads.clear();
  assert.equal(reads.size, 0);
  await reads.read("/api/overview", (raw) => raw, options());
  assert.equal(seen[1].headers["If-None-Match"], undefined);
});

test("clear bumps epoch and prevents late in-flight responses from repopulating kept cache", async () => {
  let resolveSlow!: (res: Response) => void;
  let callIndex = 0;
  const seenHeaders: Array<Record<string, string>> = [];

  const reads = new Validated(async (_url, init) => {
    seenHeaders.push((init.headers ?? {}) as Record<string, string>);
    callIndex += 1;
    if (callIndex === 1) {
      return await new Promise<Response>((resolve) => {
        resolveSlow = resolve;
      });
    }
    return new Response(JSON.stringify({ n: 2 }), {
      status: 200,
      headers: { ETag: 'W/"fresh-2"' },
    });
  });

  const epochBefore = reads.epoch;
  const pendingSlow = reads.read("/api/fleet", (raw) => raw, options());
  reads.clear();
  assert.ok(reads.epoch > epochBefore);

  resolveSlow(
    new Response(JSON.stringify({ n: 1 }), {
      status: 200,
      headers: { ETag: 'W/"stale-1"' },
    }),
  );
  await assert.rejects(pendingSlow, { name: "AbortError" });
  assert.equal(reads.size, 0, "stale response after clear() must never repopulate cache");

  await reads.read("/api/fleet", (raw) => raw, options());
  assert.equal(seenHeaders[1]["If-None-Match"], undefined);
});

test("stale in-flight 401 after clear() or isCurrent() transition aborts without invoking fail", async () => {
  let resolveClearRace!: (res: Response) => void;
  let resolveEpochRace!: (res: Response) => void;
  let callNum = 0;

  const reads = new Validated(async () => {
    callNum += 1;
    if (callNum === 1) {
      return await new Promise<Response>((resolve) => {
        resolveClearRace = resolve;
      });
    }
    return await new Promise<Response>((resolve) => {
      resolveEpochRace = resolve;
    });
  });

  let clearFailCalls = 0;
  const pendingClearRace = reads.read("/api/overview", (raw) => raw, {
    ...options(() => {
      clearFailCalls += 1;
      return new Error("must not be called after clear");
    }),
  });
  reads.clear();
  resolveClearRace(new Response(JSON.stringify({ error: "expired" }), { status: 401 }));
  await assert.rejects(pendingClearRace, { name: "AbortError" });
  assert.equal(clearFailCalls, 0);

  let epochCurrent = true;
  let epochFailCalls = 0;
  const pendingEpochRace = reads.read("/api/overview", (raw) => raw, {
    ...options(() => {
      epochFailCalls += 1;
      return new Error("must not be called after isCurrent false");
    }),
    isCurrent: () => epochCurrent,
  });
  epochCurrent = false;
  resolveEpochRace(new Response(JSON.stringify({ error: "expired" }), { status: 401 }));
  await assert.rejects(pendingEpochRace, { name: "AbortError" });
  assert.equal(epochFailCalls, 0);
});

test("seed, peek, fresh window, and persist/clear hooks preserve object identity and epoch safety", async () => {
  const persisted: Array<{ url: string; etag: string }> = [];
  let clears = 0;
  const { seen, fetcher } = server([
    { body: { n: 10 }, etag: 'W/"ten"' },
    { body: { n: 20 }, etag: 'W/"twenty"' },
  ]);
  const reads = new Validated(fetcher, {
    onPersist: (url, etag) => persisted.push({ url, etag }),
    onClear: () => {
      clears += 1;
    },
  });

  let normalizedCalls = 0;
  const normalize = (raw: unknown) => {
    normalizedCalls += 1;
    return { val: (raw as { n: number }).n };
  };

  // Seed without fresh window: peek memoizes object identity and supplies ETag, while read() still validates.
  reads.seed("/api/fleet", 'W/"ten"', { n: 10 });
  assert.equal(reads.etagOf("/api/fleet"), 'W/"ten"');
  const peeked1 = reads.peek("/api/fleet", normalize);
  const peeked2 = reads.peek("/api/fleet", normalize);
  assert.deepEqual(peeked1, { val: 10 });
  assert.equal(peeked1, peeked2, "peek returns memoized reference");
  assert.equal(normalizedCalls, 1);

  const after304 = await reads.read("/api/fleet", normalize, options());
  assert.equal(after304, peeked1, "304 keeps exact peeked object identity");
  assert.equal(seen[0].headers["If-None-Match"], 'W/"ten"');

  // Seed with freshMs: first periodic read uses seeded payload without network, second hits network.
  reads.seed("/api/overview", 'W/"seed"', { n: 42 }, 5_000);
  const freshHit = await reads.read("/api/overview", normalize, options());
  assert.deepEqual(freshHit, { val: 42 });
  assert.equal(seen.length, 1, "fresh window skipped duplicate fetch");
  assert.equal(persisted.length, 0, "seed itself does not trigger onPersist");

  const revalidated = await reads.read("/api/overview", normalize, options());
  assert.deepEqual(revalidated, { val: 20 });
  assert.equal(seen[1].headers["If-None-Match"], 'W/"seed"');
  assert.deepEqual(persisted, [{ url: "/api/overview", etag: 'W/"twenty"' }]);

  // markFreshFor skips next read if still inside window, clear notifies onClear.
  reads.markFreshFor("/api/overview", 5_000);
  const markedHit = await reads.read("/api/overview", normalize, options());
  assert.equal(markedHit, revalidated);
  assert.equal(seen.length, 2);

  reads.clear();
  assert.equal(clears, 1);
  assert.equal(reads.etagOf("/api/overview"), null);
  assert.equal(reads.peek("/api/overview", normalize), null);
});

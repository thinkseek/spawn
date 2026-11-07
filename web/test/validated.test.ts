// Run by rsi/scenarios/check-web.sh: `node --test test/`. The page's reads
// send the tag of what they read last and get the same object back on a
// 304; every read accepts an answer a second old; a failure carries the
// server's reason.
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  formatBytes,
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
  await reads.read("/api/rsi", (raw) => (raw as { n: number }).n, options());
  const other = await reads.read("/api/rsi", (raw) => `n=${(raw as { n: number }).n}`, options());
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

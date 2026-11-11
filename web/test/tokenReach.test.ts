import assert from "node:assert/strict";
import { test } from "node:test";

import { localDateTime } from "../src/format.ts";
import { tokenReachNote, tokenReachOf } from "../src/tokenReach.ts";

const from = Date.UTC(2026, 10, 5, 15, 30);

test("an archive that reaches back past both windows says nothing", () => {
  assert.equal(tokenReachNote({ tokens_short: null, tokens_from_ms: null }), null);
});

test("a window that starts before the archive says where its tokens begin", () => {
  assert.equal(
    tokenReachNote({ tokens_short: "window", tokens_from_ms: from }),
    `Tokens counted from ${localDateTime(from)} only: the transcript archive begins there, after this window's start.`,
  );
  assert.equal(
    tokenReachNote({ tokens_short: "window", tokens_from_ms: null }),
    "Tokens not counted: the transcript archive holds no model call.",
  );
});

test("a previous window that starts before the archive says so", () => {
  assert.equal(
    tokenReachNote({ tokens_short: "previous", tokens_from_ms: from }),
    `Previous window's tokens counted from ${localDateTime(from)} only: the transcript archive begins there, after that window's start.`,
  );
});

test("the server's fields are read, and anything else reads as whole", () => {
  assert.deepEqual(tokenReachOf({ tokens_short: "window", tokens_from_ms: from }), {
    tokens_short: "window",
    tokens_from_ms: from,
  });
  assert.deepEqual(tokenReachOf({ tokens_short: "previous", tokens_from_ms: from }), {
    tokens_short: "previous",
    tokens_from_ms: from,
  });
  for (const odd of [{}, { tokens_short: "yes" }, { tokens_short: true }, { tokens_short: null, tokens_from_ms: from }]) {
    assert.equal(tokenReachOf(odd).tokens_short, null, JSON.stringify(odd));
  }
  for (const odd of [{}, { tokens_from_ms: "1" }, { tokens_from_ms: Number.NaN }, { tokens_from_ms: null }]) {
    assert.equal(tokenReachOf({ tokens_short: "window", ...odd }).tokens_from_ms, null, JSON.stringify(odd));
  }
});

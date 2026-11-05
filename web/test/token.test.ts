// Run by rsi/scenarios/check-web.sh: `node --test test/` (Node strips the
// types itself). Audit L11: a token that cannot be a header value is never
// stored or sent, and a saved one is removed with a message.
import assert from "node:assert/strict";
import { test } from "node:test";

import { chooseToken, serveCredential } from "../src/token.ts";

test("only visible ASCII up to 512 characters is a token", () => {
  for (const good of ["abc", "bb-serve-token-1", "a".repeat(512), "!~#$%&'()*+,-./:;<=>?@[]^_`{|}"]) {
    assert.equal(serveCredential(good), good, good);
  }
  for (const bad of ["", "a b", "a\nb", "a\rb", "a\u0000b", "a\tb", "令牌", "café", "a".repeat(513), null, undefined]) {
    assert.equal(serveCredential(bad), null, JSON.stringify(bad));
  }
});

test("every token chosen fits an Authorization header", () => {
  for (const [fromUrl, saved] of [
    ["good", null],
    ["a\nb", null],
    [null, "a\u0000b"],
    ["令牌", "good"],
    ["a\rb", "b\nc"],
  ] as const) {
    const { token } = chooseToken(fromUrl, saved);
    assert.doesNotThrow(() => new Headers(token ? { Authorization: `Bearer ${token}` } : {}));
  }
});

test("a good ?token= is saved and sent", () => {
  assert.deepEqual(chooseToken("good", "older"), { token: "good", save: "good", clear: false, problem: null });
});

test("a bad ?token= is neither saved nor sent, and the saved one stays", () => {
  const choice = chooseToken("a\nb", "kept");
  assert.equal(choice.token, "kept");
  assert.equal(choice.save, null);
  assert.equal(choice.clear, false);
  assert.match(choice.problem ?? "", /not a token/);
});

test("a bad saved value is removed with a message", () => {
  const choice = chooseToken(null, "a\u0000b");
  assert.equal(choice.token, null);
  assert.equal(choice.clear, true);
  assert.match(choice.problem ?? "", /saved token .* removed/);
});

test("a good ?token= replaces a bad saved value", () => {
  const choice = chooseToken("good", "b\nad");
  assert.equal(choice.token, "good");
  assert.equal(choice.save, "good");
  assert.match(choice.problem ?? "", /saved token/);
});

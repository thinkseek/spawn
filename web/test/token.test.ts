// Run by rsi/scenarios/check-web.sh: `node --test test/` (Node strips the
// types itself). Audit L11: a token that cannot be a header value is never
// stored or sent, and a saved one is removed with a message.
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  FORBIDDEN_HOST_OR_ORIGIN_MESSAGE,
  TOKEN_KEY,
  authStateFromProbe,
  authStateOnUnauthorized,
  chooseToken,
  classifyStorageTokenEvent,
  isAbortError,
  probeAuthWithFetch,
  serveCredential,
  unauthorizedReason,
  validateSubmittedToken,
  verifyTokenWithFetch,
  type AuthSyncEvent,
} from "../src/token.ts";

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

test("validateSubmittedToken trims valid credentials and rejects empty or invalid input", () => {
  assert.deepEqual(validateSubmittedToken("  spw_test_123  "), {
    ok: true,
    token: "spw_test_123",
  });
  for (const empty of ["", "   \n\t "]) {
    const res = validateSubmittedToken(empty);
    assert.equal(res.ok, false);
  }
  for (const bad of ["two words", "令牌", `a${"b".repeat(512)}`]) {
    const res = validateSubmittedToken(bad);
    assert.equal(res.ok, false, JSON.stringify(bad));
  }
});

test("unauthorizedReason maps closed reasons for missing, invalid, and rejected tokens", () => {
  assert.equal(unauthorizedReason(true), "rejected_token");
  assert.equal(unauthorizedReason(true, "bad url"), "rejected_token");
  assert.equal(unauthorizedReason(false, "bad saved"), "invalid_token");
  assert.equal(unauthorizedReason(false, null), "missing_token");
});

test("probeAuthWithFetch maps 401 -> needs_token, 403 -> forbidden, 500/HTML/network -> unreachable, and 200 JSON -> authenticated", async () => {
  const headers = { Accept: "application/json" };

  assert.deepEqual(
    await probeAuthWithFetch(async () => new Response(JSON.stringify({ sessions: [] }), { status: 200 }), headers),
    { kind: "authenticated" },
  );
  assert.deepEqual(
    await probeAuthWithFetch(async () => new Response(null, { status: 304 }), headers),
    { kind: "authenticated" },
  );
  assert.deepEqual(
    await probeAuthWithFetch(async () => new Response(JSON.stringify({ error: "unauthorized" }), { status: 401 }), headers),
    { kind: "needs_token" },
  );

  // GET 403 originates from Host/Origin checks and must map to explicit forbidden state, never needs_token or unreachable.
  const forbiddenOut = await probeAuthWithFetch(
    async () => new Response(JSON.stringify({ error: "forbidden host or origin" }), { status: 403 }),
    headers,
  );
  assert.deepEqual(forbiddenOut, {
    kind: "forbidden",
    message: FORBIDDEN_HOST_OR_ORIGIN_MESSAGE,
  });
  assert.ok(!forbiddenOut.message.includes("只读"));

  // 500, HTML captive portal, malformed JSON, JSON array, and network failure MUST enter unreachable.
  for (const res of [
    new Response(JSON.stringify({ error: "internal error" }), { status: 500 }),
    new Response("<html><body>captive portal</body></html>", { status: 200 }),
    new Response("{not json", { status: 200 }),
    new Response("[]", { status: 200 }),
  ]) {
    const out = await probeAuthWithFetch(async () => res, headers);
    assert.equal(out.kind, "unreachable");
  }

  const netOut = await probeAuthWithFetch(async () => {
    throw new TypeError("Failed to fetch");
  }, headers);
  assert.equal(netOut.kind, "unreachable");

  // AbortError propagates cleanly and is recognized by isAbortError().
  await assert.rejects(
    probeAuthWithFetch(async () => {
      throw new DOMException("aborted", "AbortError");
    }, headers),
    (err: unknown) => isAbortError(err),
  );
  assert.equal(isAbortError(new DOMException("aborted", "AbortError")), true);
  assert.equal(isAbortError(new TypeError("Failed to fetch")), false);
});

test("verifyTokenWithFetch saves valid token on 200 JSON and avoids hardcoded HTTP 401 on 401/403", async () => {
  let stored: string | null = null;
  const save = (tok: string) => {
    stored = tok;
  };

  let fetched = 0;
  const invalid = await verifyTokenWithFetch("bad token with space", async () => {
    fetched += 1;
    return new Response("{}", { status: 200 });
  }, save);
  assert.equal(invalid.ok, false);
  assert.equal(fetched, 0);
  assert.equal(stored, null);

  const rejected401 = await verifyTokenWithFetch("spw_wrong", async () => {
    return new Response(JSON.stringify({ error: "unauthorized" }), { status: 401 });
  }, save);
  assert.equal(rejected401.ok, false);
  if (!rejected401.ok) assert.ok(!rejected401.message.includes("HTTP 401"));
  assert.equal(stored, null);

  const refused403 = await verifyTokenWithFetch("spw_ro", async () => {
    return new Response(JSON.stringify({ error: "forbidden" }), { status: 403 });
  }, save);
  assert.equal(refused403.ok, false);
  if (!refused403.ok) {
    assert.match(refused403.message, /403/);
    assert.match(refused403.message, /Host|Origin/);
    assert.ok(!refused403.message.includes("只读"));
  }
  assert.equal(stored, null);

  const html200 = await verifyTokenWithFetch("spw_ok", async () => {
    return new Response("<html>proxy error</html>", { status: 200 });
  }, save);
  assert.equal(html200.ok, false);
  assert.equal(stored, null);

  const accepted = await verifyTokenWithFetch("  spw_ok_123  ", async (_url, init) => {
    const auth = (init.headers as Record<string, string>).Authorization;
    assert.equal(auth, "Bearer spw_ok_123");
    return new Response(JSON.stringify({ sessions: [] }), { status: 200 });
  }, save);
  assert.deepEqual(accepted, { ok: true });
  assert.equal(stored, "spw_ok_123");
});

test("auth state machine handles stale-token escape hatch, unreachable errors, and cross-tab storage sync", async () => {
  // Stale stored token rejected (401) -> clear -> loopback no-token host returns 200 -> authenticated(hasStoredToken=false)
  assert.deepEqual(authStateFromProbe({ kind: "needs_token" }, true, null), {
    kind: "needs_token",
    reason: "rejected_token",
    problem: null,
    hasStoredToken: true,
  });
  assert.deepEqual(authStateFromProbe({ kind: "authenticated" }, false, null), {
    kind: "authenticated",
    hasStoredToken: false,
  });
  assert.deepEqual(authStateFromProbe({ kind: "forbidden", message: FORBIDDEN_HOST_OR_ORIGIN_MESSAGE }, true, null), {
    kind: "forbidden",
    message: FORBIDDEN_HOST_OR_ORIGIN_MESSAGE,
    hasStoredToken: true,
  });
  assert.deepEqual(authStateFromProbe({ kind: "unreachable", message: "HTTP 500" }, true, null), {
    kind: "unreachable",
    message: "HTTP 500",
    hasStoredToken: true,
  });
  assert.deepEqual(authStateOnUnauthorized(true, null), {
    kind: "needs_token",
    reason: "rejected_token",
    problem: null,
    hasStoredToken: true,
  });

  // Cross-tab storage sync uses unified AuthSyncEvent: sign-out affects tabs with a token, ignores unrelated keys or tokenless loopback tabs
  const signedOut: AuthSyncEvent | null = classifyStorageTokenEvent(TOKEN_KEY, null, "old-tok");
  assert.deepEqual(signedOut, { kind: "signed_out" });
  assert.deepEqual(classifyStorageTokenEvent(null, null, "old-tok"), { kind: "signed_out" });
  assert.equal(classifyStorageTokenEvent(TOKEN_KEY, null, null), null);
  assert.equal(classifyStorageTokenEvent("other.key", null, "old-tok"), null);
  const updated: AuthSyncEvent | null = classifyStorageTokenEvent(TOKEN_KEY, "new-tok", "old-tok");
  assert.deepEqual(updated, {
    kind: "token_updated",
    token: "new-tok",
  });
});

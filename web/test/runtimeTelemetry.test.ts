import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { getAuthEpoch, readApi, setServeToken, subscribeAuthSync } from "../src/api.ts";
import {
  pollOptionalRuntimeTelemetry,
  readOptionalTelemetrySliceByKey,
} from "../src/featuresApi.ts";
import {
  INITIAL_RUNTIME_TELEMETRY,
  TELEMETRY_POLL_MS,
  TELEMETRY_REQUEST_TIMEOUT_MS,
  applyTelemetrySliceUpdate,
  classifyCostAmount,
  formatCostAmountText,
  formatTelemetryUsd,
  normalizeCostWindowTelemetry,
  normalizeFleetTelemetry,
  normalizeGradingTelemetry,
  pollRuntimeTelemetrySlices,
} from "../src/runtimeTelemetry.ts";
import { Validated, type FetchLike } from "../src/validated.ts";

test("billing red line: models with usd == null && unpriced_tokens > 0 show unpriced, never $0.00", () => {
  const modelAmount = classifyCostAmount(null, 60_000, 60_000);
  assert.equal(modelAmount.kind, "unpriced");
  assert.equal(modelAmount.text, "unpriced");
  assert.equal(formatCostAmountText(modelAmount), "unpriced");
  assert.notEqual(formatCostAmountText(modelAmount), "$0.00");

  const window = normalizeCostWindowTelemetry("24h", {
    total: {
      usd: 0,
      tokens: { input: 50_000, output: 10_000, cache_write: 0, cache_read: 0 },
      unpriced_tokens: 60_000,
    },
    models: [
      {
        model: "claude-internal-unpriced",
        usd: null,
        tokens: { input: 50_000, output: 10_000, cache_write: 0, cache_read: 0 },
        unpriced_tokens: 60_000,
      },
    ],
  });

  assert.equal(window.totalTokens, 60_000);
  assert.equal(window.unpricedTokens, 60_000);
  assert.equal(window.unpricedModelCount, 1);
  assert.equal(window.amount.kind, "unpriced");
  assert.equal(formatCostAmountText(window.amount), "unpriced");
  assert.equal(window.models[0].amount.kind, "unpriced");
  assert.equal(window.models[0].amount.text, "unpriced");
});

test("mixed priced + unpriced cost window preserves USD spend alongside compact unpriced count", () => {
  const win7d = normalizeCostWindowTelemetry("7d", {
    total: {
      usd: 1.5,
      tokens: { input: 100_000, output: 20_000, cache_write: 30_000, cache_read: 50_000 },
      unpriced_tokens: 50_000,
    },
    models: [
      {
        model: "claude-sonnet-4-6",
        usd: 1.5,
        tokens: { input: 70_000, output: 15_000, cache_write: 25_000, cache_read: 40_000 },
        unpriced_tokens: 0,
      },
      {
        model: "custom-preview-model",
        usd: null,
        tokens: { input: 30_000, output: 5_000, cache_write: 5_000, cache_read: 10_000 },
        unpriced_tokens: 50_000,
      },
    ],
  });

  assert.equal(win7d.totalTokens, 200_000);
  assert.equal(win7d.unpricedModelCount, 1);
  assert.equal(win7d.amount.kind, "mixed");
  assert.equal(formatCostAmountText(win7d.amount), "$1.50 (+50K unpriced)");
  assert.equal(win7d.models[0].amount.kind, "priced");
  assert.equal(win7d.models[0].amount.text, "$1.50");
  assert.equal(win7d.models[1].amount.kind, "unpriced");
  assert.equal(win7d.models[1].amount.text, "unpriced");
});

test("sub-cent USD formats as <$0.01, exact priced zero formats as $0.00, and null priced formats as —", () => {
  assert.equal(formatTelemetryUsd(0.001), "<$0.01");
  assert.equal(formatTelemetryUsd(0), "$0.00");
  assert.equal(classifyCostAmount(0, 1000, 0).kind, "priced");
  assert.equal(classifyCostAmount(0, 1000, 0).text, "$0.00");
  assert.equal(classifyCostAmount(null, 0, 0).kind, "unavailable");
  assert.equal(classifyCostAmount(null, 0, 0).text, "—");
});

test("normalizeFleetTelemetry extracts live, working, waiting, blocked counts with fallback from agents list", () => {
  const fromHealth = normalizeFleetTelemetry({
    health: {
      agents_total: 6,
      live: 4,
      by_state: { WORKING: 3, WAITING: 1, BLOCKED: 1, COMPLETED: 1, UNKNOWN: 0 },
    },
    agents: [],
  });
  assert.deepEqual(fromHealth, { live: 4, working: 3, waiting: 1, blocked: 1, total: 6 });

  const fromAgents = normalizeFleetTelemetry({
    agents: [
      { session_id: "s1", live: true, state: "WORKING" },
      { session_id: "s2", live: true, state: "waiting" },
      { session_id: "s3", live: false, state: "BLOCKED" },
    ],
  });
  assert.deepEqual(fromAgents, { live: 2, working: 1, waiting: 1, blocked: 1, total: 3 });
});

test("normalizeGradingTelemetry groups active presets across L1, L2, and L3", () => {
  const summary = normalizeGradingTelemetry({
    presets: [
      {
        preset_id: "haiku-fast",
        model_label: "claude-haiku-4-5",
        state: "active",
        routing_grade: "l1",
      },
      {
        preset_id: "sonnet-main",
        model_label: "claude-sonnet-4-6",
        state: "active",
        routing_grade: "l2",
      },
      {
        preset_id: "sonnet-demoted",
        model_label: "claude-sonnet-4-0",
        state: "demoted",
        routing_grade: "l2",
      },
      {
        preset_id: "opus-top",
        model_label: "claude-opus-4-6",
        state: "active",
        routing_grade: "l3",
      },
      {
        preset_id: "unexamined-new",
        model_label: "model-x",
        state: "unexamined",
      },
    ],
  });

  assert.equal(summary.activeTotal, 3);
  assert.equal(summary.totalPresets, 5);
  assert.deepEqual(
    summary.tiers.map((t) => [t.label, t.activeCount, t.totalCount, t.activeModels]),
    [
      ["L1", 1, 1, ["claude-haiku-4-5"]],
      ["L2", 1, 2, ["claude-sonnet-4-6"]],
      ["L3", 1, 1, ["claude-opus-4-6"]],
    ],
  );
});

test("telemetry normalizers and INITIAL_RUNTIME_TELEMETRY degrade gracefully on empty/malformed inputs", () => {
  assert.equal(INITIAL_RUNTIME_TELEMETRY.fleet.status, "loading");
  assert.equal(INITIAL_RUNTIME_TELEMETRY.grading.status, "loading");
  assert.equal(INITIAL_RUNTIME_TELEMETRY.cost24h.status, "loading");
  assert.equal(INITIAL_RUNTIME_TELEMETRY.cost7d.status, "loading");

  assert.deepEqual(normalizeFleetTelemetry(null), {
    live: 0,
    working: 0,
    waiting: 0,
    blocked: 0,
    total: 0,
  });
  const emptyGrading = normalizeGradingTelemetry(undefined);
  assert.equal(emptyGrading.activeTotal, 0);
  assert.equal(emptyGrading.tiers.length, 3);

  const emptyCost = normalizeCostWindowTelemetry("24h", "invalid");
  assert.equal(emptyCost.totalTokens, 0);
  assert.equal(emptyCost.amount.kind, "unavailable");
  assert.equal(formatCostAmountText(emptyCost.amount), "—");
});

test("Fix 1: optional telemetry 401 degrades locally to unavailable without calling global unauthorized hooks", async () => {
  const fetcher: FetchLike = async (url) => {
    if (url === "/api/grading") {
      return new Response(JSON.stringify({ error: "unauthorized" }), { status: 401 });
    }
    if (url === "/api/fleet") {
      return new Response(JSON.stringify({ health: { live: 2, agents_total: 3, by_state: { WORKING: 2, WAITING: 1, BLOCKED: 0 } } }), { status: 200 });
    }
    return new Response(
      JSON.stringify({
        total: { usd: 0.42, tokens: { input: 1000, output: 500, cache_write: 0, cache_read: 0 }, unpriced_tokens: 0 },
        models: [],
      }),
      { status: 200 },
    );
  };

  const reader = new Validated(fetcher);
  const controller = new AbortController();
  const state = await pollRuntimeTelemetrySlices({
    reader,
    signal: controller.signal,
    headers: { Accept: "application/json" },
    timeoutMs: 250,
  });

  assert.equal(state.grading.status, "unavailable");
  assert.equal(state.fleet.status, "ready");
  assert.equal(state.fleet.data.live, 2);
  assert.equal(state.cost24h.status, "ready");
  assert.equal(state.cost7d.status, "ready");

  // Guard featuresApi source: optional telemetry uses dedicated local reader, never readApi or reportUnauthorizedStatus.
  const featuresApiSrc = readFileSync(new URL("../src/featuresApi.ts", import.meta.url), "utf8");
  assert.doesNotMatch(featuresApiSrc, /reportUnauthorizedStatus/);
  assert.match(featuresApiSrc, /optionalTelemetryReads\s*=\s*new Validated\(\)/);
  assert.match(featuresApiSrc, /fetchFeatureMap[\s\S]*readApi\("\/api\/features"/);
  const telemetrySection = featuresApiSrc.slice(featuresApiSrc.indexOf("readOptionalTelemetrySliceByKey"));
  assert.doesNotMatch(telemetrySection, /\breadApi\s*\(/);
});

test("Fix 2: stalled slice times out below poll interval while healthy sibling slices settle immediately", async () => {
  assert.ok(TELEMETRY_REQUEST_TIMEOUT_MS < TELEMETRY_POLL_MS);

  const fetcher: FetchLike = (url, init) => {
    if (url === "/api/cost?window=7d") {
      return new Promise<Response>((_resolve, reject) => {
        init.signal?.addEventListener(
          "abort",
          () => reject(new DOMException("Aborted", "AbortError")),
          { once: true },
        );
      });
    }
    if (url === "/api/fleet") {
      return Promise.resolve(
        new Response(JSON.stringify({ health: { live: 1, agents_total: 1, by_state: { WORKING: 1, WAITING: 0, BLOCKED: 0 } } }), { status: 200 }),
      );
    }
    if (url === "/api/grading") {
      return Promise.resolve(
        new Response(JSON.stringify({ presets: [{ preset_id: "p1", model_label: "sonnet", state: "active", routing_grade: "l2" }] }), { status: 200 }),
      );
    }
    return Promise.resolve(
      new Response(
        JSON.stringify({
          total: { usd: 1.25, tokens: { input: 500, output: 100, cache_write: 0, cache_read: 0 }, unpriced_tokens: 0 },
          models: [],
        }),
        { status: 200 },
      ),
    );
  };

  const settledOrder: string[] = [];
  let progressive = INITIAL_RUNTIME_TELEMETRY;
  const reader = new Validated(fetcher);
  const controller = new AbortController();

  const finalState = await pollRuntimeTelemetrySlices({
    reader,
    signal: controller.signal,
    headers: { Accept: "application/json" },
    timeoutMs: 35,
    onSlice: (update) => {
      settledOrder.push(update.key);
      progressive = applyTelemetrySliceUpdate(progressive, update);
    },
  });

  // The three healthy endpoints settle before the stalled /api/cost?window=7d hits its timeout.
  assert.equal(settledOrder.length, 4);
  assert.equal(settledOrder[3], "cost7d");
  assert.equal(finalState.fleet.status, "ready");
  assert.equal(finalState.grading.status, "ready");
  assert.equal(finalState.cost24h.status, "ready");
  assert.equal(finalState.cost7d.status, "unavailable");
  assert.deepEqual(progressive, finalState);
});

test("Blocker 1 (real API): same-tab logout and re-login clears optional telemetry ETag and advances auth epoch", async () => {
  const prevFetch = globalThis.fetch;
  const requests: Array<{ url: string; auth: string | undefined; ifNoneMatch: string | undefined }> = [];

  globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    const url = String(input);
    const headers = (init?.headers ?? {}) as Record<string, string>;
    requests.push({
      url,
      auth: headers.Authorization,
      ifNoneMatch: headers["If-None-Match"],
    });
    if (headers["If-None-Match"] === 'W/"sess-a"') {
      return new Response(null, { status: 304, headers: { ETag: 'W/"sess-a"' } });
    }
    const isB = headers.Authorization === "Bearer tok_session_b";
    return new Response(
      JSON.stringify({
        health: {
          live: isB ? 5 : 2,
          agents_total: isB ? 5 : 2,
          by_state: { WORKING: isB ? 5 : 2, WAITING: 0, BLOCKED: 0 },
        },
      }),
      {
        status: 200,
        headers: { ETag: isB ? 'W/"sess-b"' : 'W/"sess-a"' },
      },
    );
  }) as typeof fetch;

  try {
    setServeToken("tok_session_a");
    const epochA = getAuthEpoch();
    const signal = new AbortController().signal;

    const firstA = await readOptionalTelemetrySliceByKey("fleet", signal);
    assert.equal(firstA.kind, "current");
    if (firstA.kind === "current" && firstA.update.key === "fleet" && firstA.update.slice.status === "ready") {
      assert.equal(firstA.update.slice.data.live, 2);
    } else {
      assert.fail("expected ready fleet slice in session A");
    }

    // Same session hits cached ETag cleanly.
    const secondA = await readOptionalTelemetrySliceByKey("fleet", signal);
    assert.equal(secondA.kind, "current");
    assert.equal(requests[1].ifNoneMatch, 'W/"sess-a"');

    // Same-tab logout -> re-login as session B MUST flush optionalTelemetryReads ETag cache.
    setServeToken(null);
    const epochSignedOut = getAuthEpoch();
    assert.ok(epochSignedOut > epochA);

    setServeToken("tok_session_b");
    const epochB = getAuthEpoch();
    assert.ok(epochB > epochSignedOut);

    const firstB = await readOptionalTelemetrySliceByKey("fleet", signal);
    assert.equal(firstB.kind, "current");
    if (firstB.kind === "current" && firstB.update.key === "fleet" && firstB.update.slice.status === "ready") {
      assert.equal(firstB.update.slice.data.live, 5);
    } else {
      assert.fail("expected ready fleet slice in session B");
    }
    assert.equal(requests[2].auth, "Bearer tok_session_b");
    assert.equal(
      requests[2].ifNoneMatch,
      undefined,
      "same-tab re-login MUST NOT send previous session ETag",
    );
  } finally {
    setServeToken(null);
    globalThis.fetch = prevFetch;
  }
});

test("Blocker 1 (real API): late in-flight session A telemetry response is discarded as stale and never backfills session B cache", async () => {
  const prevFetch = globalThis.fetch;
  const requests: Array<{ auth: string | undefined; ifNoneMatch: string | undefined }> = [];
  let resolveSlowSessionA!: (res: Response) => void;
  let callNum = 0;

  globalThis.fetch = ((_input: string | URL | Request, init?: RequestInit) => {
    const headers = (init?.headers ?? {}) as Record<string, string>;
    requests.push({ auth: headers.Authorization, ifNoneMatch: headers["If-None-Match"] });
    callNum += 1;
    if (callNum === 1) {
      return new Promise<Response>((resolve) => {
        resolveSlowSessionA = resolve;
      });
    }
    return Promise.resolve(
      new Response(
        JSON.stringify({ health: { live: 3, agents_total: 3, by_state: { WORKING: 3, WAITING: 0, BLOCKED: 0 } } }),
        { status: 200, headers: { ETag: 'W/"sess-b-fresh"' } },
      ),
    );
  }) as typeof fetch;

  try {
    setServeToken("tok_session_a");
    const signal = new AbortController().signal;
    const pendingSessionA = readOptionalTelemetrySliceByKey("fleet", signal);

    // Mid-flight same-tab logout and login as session B before session A responds.
    setServeToken(null);
    setServeToken("tok_session_b");

    // Late session A 200 response with ETag arrives after auth transition.
    resolveSlowSessionA(
      new Response(
        JSON.stringify({ health: { live: 77, agents_total: 77, by_state: { WORKING: 77, WAITING: 0, BLOCKED: 0 } } }),
        { status: 200, headers: { ETag: 'W/"late-sess-a"' } },
      ),
    );

    const lateOutcome = await pendingSessionA;
    assert.equal(lateOutcome.kind, "stale", "stale-epoch response must never publish UI state");

    // Subsequent session B read MUST NOT carry session A's late ETag into If-None-Match.
    const sessionBOutcome = await readOptionalTelemetrySliceByKey("fleet", signal);
    assert.equal(sessionBOutcome.kind, "current");
    if (
      sessionBOutcome.kind === "current" &&
      sessionBOutcome.update.key === "fleet" &&
      sessionBOutcome.update.slice.status === "ready"
    ) {
      assert.equal(sessionBOutcome.update.slice.data.live, 3);
    } else {
      assert.fail("expected fresh ready fleet slice for session B");
    }
    assert.equal(requests[1].auth, "Bearer tok_session_b");
    assert.equal(
      requests[1].ifNoneMatch,
      undefined,
      "late session A response MUST NOT backfill optionalTelemetryReads cache",
    );
  } finally {
    setServeToken(null);
    globalThis.fetch = prevFetch;
  }
});

test("Blocker 1 (real API): pollOptionalRuntimeTelemetry 401 never triggers global subscribeAuthSync logout", async () => {
  const prevFetch = globalThis.fetch;
  const authEvents: string[] = [];
  const unsubscribe = subscribeAuthSync((ev) => authEvents.push(ev.kind));

  globalThis.fetch = (async (input: string | URL | Request) => {
    const url = String(input);
    if (url === "/api/grading") {
      return new Response(JSON.stringify({ error: "unauthorized" }), { status: 401 });
    }
    if (url === "/api/fleet") {
      return new Response(
        JSON.stringify({ health: { live: 1, agents_total: 1, by_state: { WORKING: 1, WAITING: 0, BLOCKED: 0 } } }),
        { status: 200 },
      );
    }
    return new Response(
      JSON.stringify({
        total: { usd: 0.1, tokens: { input: 100, output: 50, cache_write: 0, cache_read: 0 }, unpriced_tokens: 0 },
        models: [],
      }),
      { status: 200 },
    );
  }) as typeof fetch;

  try {
    setServeToken("tok_live");
    const streamed: string[] = [];
    const state = await pollOptionalRuntimeTelemetry(
      new AbortController().signal,
      (u) => streamed.push(u.key),
      200,
    );
    assert.deepEqual(authEvents, []);
    assert.equal(state.grading.status, "unavailable");
    assert.equal(state.fleet.status, "ready");
    assert.equal(streamed.length, 4);
  } finally {
    unsubscribe();
    setServeToken(null);
    globalThis.fetch = prevFetch;
  }
});

test("Blocker 1 (real API): stale in-flight Session A readApi(/api/overview) 401 after switching to Session B aborts with 0 unauthorized events", async () => {
  const prevFetch = globalThis.fetch;
  const authEvents: string[] = [];
  const unsubscribe = subscribeAuthSync((ev) => authEvents.push(ev.kind));
  let resolveSlowSessionA!: (res: Response) => void;
  const requests: Array<{ url: string; auth: string | undefined }> = [];

  globalThis.fetch = ((input: string | URL | Request, init?: RequestInit) => {
    const headers = (init?.headers ?? {}) as Record<string, string>;
    requests.push({ url: String(input), auth: headers.Authorization });
    return new Promise<Response>((resolve) => {
      resolveSlowSessionA = resolve;
    });
  }) as typeof fetch;

  try {
    setServeToken("tok_session_a");
    const epochA = getAuthEpoch();
    const pendingSessionA = readApi("/api/overview", new AbortController().signal, (raw) => raw);

    // Switch to Session B while Session A's /api/overview request is still in flight.
    setServeToken("tok_session_b");
    assert.ok(getAuthEpoch() > epochA);

    // Late 401 from Session A arrives after Session B is already signed in.
    resolveSlowSessionA(new Response(JSON.stringify({ error: "session expired" }), { status: 401 }));

    await assert.rejects(pendingSessionA, { name: "AbortError" });
    assert.deepEqual(authEvents, []);
    assert.equal(requests[0].url, "/api/overview");
    assert.equal(requests[0].auth, "Bearer tok_session_a");
  } finally {
    unsubscribe();
    setServeToken(null);
    globalThis.fetch = prevFetch;
  }
});

test("Blocker 1 (real API): stale in-flight Session A readApi(/api/overview) 200 after switching to Session B aborts without polluting Session B cache", async () => {
  const prevFetch = globalThis.fetch;
  const authEvents: string[] = [];
  const unsubscribe = subscribeAuthSync((ev) => authEvents.push(ev.kind));
  let resolveSlowSessionA!: (res: Response) => void;
  let callNum = 0;
  const requests: Array<{ auth: string | undefined; ifNoneMatch: string | undefined }> = [];

  globalThis.fetch = ((_input: string | URL | Request, init?: RequestInit) => {
    const headers = (init?.headers ?? {}) as Record<string, string>;
    requests.push({ auth: headers.Authorization, ifNoneMatch: headers["If-None-Match"] });
    callNum += 1;
    if (callNum === 1) {
      return new Promise<Response>((resolve) => {
        resolveSlowSessionA = resolve;
      });
    }
    return Promise.resolve(
      new Response(JSON.stringify({ tenant: "B", active: 4 }), {
        status: 200,
        headers: { ETag: 'W/"session-b-overview"' },
      }),
    );
  }) as typeof fetch;

  try {
    setServeToken("tok_session_a");
    const pendingSessionA = readApi("/api/overview", new AbortController().signal, (raw) => raw);

    setServeToken("tok_session_b");

    // Late 200 with ETag from Session A arrives after auth transition.
    resolveSlowSessionA(
      new Response(JSON.stringify({ tenant: "A", active: 99 }), {
        status: 200,
        headers: { ETag: 'W/"session-a-stale"' },
      }),
    );

    await assert.rejects(pendingSessionA, { name: "AbortError" });
    assert.deepEqual(authEvents, []);

    const sessionBOverview = (await readApi(
      "/api/overview",
      new AbortController().signal,
      (raw) => raw,
    )) as { tenant: string; active: number };
    assert.deepEqual(sessionBOverview, { tenant: "B", active: 4 });
    assert.equal(requests[1].auth, "Bearer tok_session_b");
    assert.equal(
      requests[1].ifNoneMatch,
      undefined,
      "late Session A 200 MUST NOT seed Session B's shared readApi ETag cache",
    );
  } finally {
    unsubscribe();
    setServeToken(null);
    globalThis.fetch = prevFetch;
  }
});

test("Blocker 1 (real API): current-epoch readApi(/api/overview) 401 still broadcasts global unauthorized", async () => {
  const prevFetch = globalThis.fetch;
  const authEvents: string[] = [];
  const unsubscribe = subscribeAuthSync((ev) => authEvents.push(ev.kind));

  globalThis.fetch = (async () =>
    new Response(JSON.stringify({ error: "token revoked" }), { status: 401 })) as typeof fetch;

  try {
    setServeToken("tok_current");
    await assert.rejects(
      readApi("/api/overview", new AbortController().signal, (raw) => raw),
      (err: unknown) =>
        err instanceof Error &&
        !(err instanceof DOMException && err.name === "AbortError") &&
        /HTTP 401/.test(err.message),
    );
    assert.deepEqual(authEvents, ["unauthorized"]);
  } finally {
    unsubscribe();
    setServeToken(null);
    globalThis.fetch = prevFetch;
  }
});

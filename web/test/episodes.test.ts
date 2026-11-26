import assert from "node:assert/strict";
import { test } from "node:test";

import {
  availableMissionIds,
  episodeSyncPoints,
  episodesUrl,
  fetchOptionalEpisodes,
  filterEpisodes,
  outcomeTone,
  parseEpisodesResponse,
  type TaskEpisode,
} from "../src/episodes.ts";

function makeEpisode(overrides: Partial<TaskEpisode> = {}): TaskEpisode {
  return {
    task_id: 1,
    mission_id: 4,
    kind: "ephemeral",
    agent_id: "agent_worker_1",
    session_id: "session_worker_1",
    brief: "implement parser",
    created_by: "human",
    objective: 12,
    scenario: "012.sh",
    model: "default",
    difficulty: "l2",
    preset_id: "neon-l2",
    first_preset_id: "cheap-l1",
    engine: "agy",
    model_label: "neon",
    pinned: false,
    reissue_of: null,
    continues: null,
    reassign_of: null,
    state: "landed",
    outcome: "landed",
    rework: 1,
    quality_rejections: 1,
    reroutes: 1,
    restarts: 0,
    steals: 0,
    deliveries: 2,
    reports: 2,
    last_command_id: 102,
    last_report: "final v1",
    last_report_commit: "bbbb",
    review_note: "add edge test",
    base: "aaaa",
    candidate: "bbbb",
    metric: "score=1.0",
    last_gate_verdict: "accepted",
    review_sync_points: [
      {
        event_seq: 10,
        at_ms: 1000,
        revision: 0,
        accepted: false,
        note: "add edge test",
      },
      {
        event_seq: 14,
        at_ms: 2000,
        revision: 1,
        accepted: true,
        note: null,
      },
    ],
    gate_verdicts: [
      {
        event_seq: 12,
        at_ms: 1500,
        revision: 0,
        verdict: "rejected",
        class: "code",
        base: "aaaa",
        candidate: null,
        note: "failed assertion",
      },
      {
        event_seq: 16,
        at_ms: 2500,
        revision: 1,
        verdict: "accepted",
        class: "code",
        base: "aaaa",
        candidate: "bbbb",
        note: null,
      },
    ],
    terminal_by: null,
    terminal_reason: null,
    assigned_seq: 1,
    assigned_at_ms: 500,
    delivered_at_ms: 600,
    closed_seq: 18,
    closed_at_ms: 2600,
    updated_at_ms: 2600,
    stages: [
      { event_seq: 1, at_ms: 500, kind: "TaskAssigned" },
      { event_seq: 18, at_ms: 2600, kind: "TaskLanded" },
    ],
    ...overrides,
  };
}

function makePayload(episodes: TaskEpisode[] = [makeEpisode()]) {
  return {
    mission: null,
    episodes,
    summary: {
      total: episodes.length,
      landed: episodes.filter((e) => e.outcome === "landed").length,
      accepted: episodes.filter((e) => e.outcome === "accepted").length,
      rejected: episodes.filter((e) => e.outcome === "rejected").length,
      cancelled: episodes.filter((e) => e.outcome === "cancelled").length,
      aborted: episodes.filter((e) => e.outcome === "aborted").length,
      over_budget: episodes.filter((e) => e.outcome === "over_budget").length,
      in_flight: episodes.filter((e) => e.outcome === "in_flight").length,
      total_rework: episodes.reduce((acc, e) => acc + e.rework, 0),
      total_reroutes: episodes.reduce((acc, e) => acc + e.reroutes, 0),
      review_sync_points: episodes.reduce((acc, e) => acc + e.review_sync_points.length, 0),
      gate_verdicts: episodes.reduce((acc, e) => acc + e.gate_verdicts.length, 0),
      warnings: 0,
    },
    parity_ok: true,
    warnings: 0,
    drift_count: 0,
  };
}

test("episodesUrl formats optional mission query strictly", () => {
  assert.equal(episodesUrl(), "/api/episodes");
  assert.equal(episodesUrl(null), "/api/episodes");
  assert.equal(episodesUrl(42), "/api/episodes?mission=42");
});

test("parseEpisodesResponse accepts valid payloads and rejects malformed shapes", () => {
  const parsed = parseEpisodesResponse(makePayload());
  assert.equal(parsed.parity_ok, true);
  assert.equal(parsed.episodes.length, 1);
  assert.equal(parsed.episodes[0].review_sync_points.length, 2);
  assert.equal(parsed.episodes[0].gate_verdicts.length, 2);

  assert.throws(() => parseEpisodesResponse(null), /must be an object/);
  assert.throws(
    () => parseEpisodesResponse({ ...makePayload(), parity_ok: "yes" }),
    /parity_ok must be a boolean/,
  );
  assert.throws(
    () =>
      parseEpisodesResponse({
        ...makePayload([makeEpisode({ outcome: "mystery" as TaskEpisode["outcome"] })]),
      }),
    /invalid EpisodeOutcome/,
  );
  assert.throws(
    () =>
      parseEpisodesResponse({
        ...makePayload([makeEpisode({ difficulty: "l9" as TaskEpisode["difficulty"] })]),
      }),
    /invalid EpisodeDifficulty/,
  );
  assert.throws(
    () =>
      parseEpisodesResponse({
        ...makePayload(),
        summary: { total: "NaN" },
      }),
    /summary\.total must be a finite number/,
  );
});

test("fetchOptionalEpisodes handles 200 ETag, 304 Not Modified, HTTP errors, and network failures without throwing", async () => {
  let seenHeaders: HeadersInit | undefined;
  let seenUrl = "";
  const okRes = await fetchOptionalEpisodes({
    missionId: 7,
    headers: { Authorization: "Bearer test-tok" },
    etag: 'W/"old"',
    fetchImpl: (async (input: RequestInfo | URL, init?: RequestInit) => {
      seenUrl = String(input);
      seenHeaders = init?.headers;
      return new Response(JSON.stringify(makePayload()), {
        status: 200,
        headers: { ETag: 'W/"new-etag"' },
      });
    }) as typeof fetch,
  });
  assert.equal(seenUrl, "/api/episodes?mission=7");
  assert.equal((seenHeaders as Record<string, string>)["If-None-Match"], 'W/"old"');
  assert.equal((seenHeaders as Record<string, string>)["Authorization"], "Bearer test-tok");
  assert.equal(okRes.ok, true);
  if (okRes.ok && !okRes.notModified) {
    assert.equal(okRes.etag, 'W/"new-etag"');
    assert.equal(okRes.data.episodes.length, 1);
  } else {
    assert.fail("expected 200 payload");
  }

  const notMod = await fetchOptionalEpisodes({
    etag: 'W/"new-etag"',
    fetchImpl: (async () =>
      new Response(null, { status: 304, headers: { ETag: 'W/"new-etag"' } })) as typeof fetch,
  });
  assert.deepEqual(notMod, { ok: true, notModified: true, etag: 'W/"new-etag"' });

  const busy = await fetchOptionalEpisodes({
    fetchImpl: (async () =>
      new Response(JSON.stringify({ error: "reads pool saturated" }), {
        status: 503,
      })) as typeof fetch,
  });
  assert.deepEqual(busy, { ok: false, error: "reads pool saturated", status: 503 });

  const malformed = await fetchOptionalEpisodes({
    fetchImpl: (async () => new Response("{broken", { status: 200 })) as typeof fetch,
  });
  assert.equal(malformed.ok, false);
  if (!malformed.ok) {
    assert.match(malformed.error, /invalid episodes payload/);
    assert.equal(malformed.status, 200);
  }

  const networkErr = await fetchOptionalEpisodes({
    fetchImpl: (async () => {
      throw new Error("connection refused");
    }) as typeof fetch,
  });
  assert.deepEqual(networkErr, { ok: false, error: "connection refused", status: null });
});

test("filterEpisodes filters by mission, outcome, and rework > 0", () => {
  const eps = [
    makeEpisode({ task_id: 1, mission_id: 1, outcome: "landed", rework: 2 }),
    makeEpisode({ task_id: 2, mission_id: 1, outcome: "cancelled", rework: 0 }),
    makeEpisode({ task_id: 3, mission_id: 2, outcome: "landed", rework: 0 }),
    makeEpisode({ task_id: 4, mission_id: 2, outcome: "in_flight", rework: 1 }),
  ];

  assert.deepEqual(
    availableMissionIds(eps),
    [1, 2],
  );
  assert.deepEqual(
    filterEpisodes(eps, { missionId: 1 }).map((e) => e.task_id),
    [1, 2],
  );
  assert.deepEqual(
    filterEpisodes(eps, { outcome: "landed" }).map((e) => e.task_id),
    [1, 3],
  );
  assert.deepEqual(
    filterEpisodes(eps, { reworkOnly: true }).map((e) => e.task_id),
    [1, 4],
  );
  assert.deepEqual(
    filterEpisodes(eps, { missionId: 2, outcome: "in_flight", reworkOnly: true }).map(
      (e) => e.task_id,
    ),
    [4],
  );
});

test("episodeSyncPoints interleaves review and gate checkpoints by event_seq", () => {
  const timeline = episodeSyncPoints(makeEpisode());
  assert.deepEqual(
    timeline.map((item) => [item.kind, item.sync.event_seq]),
    [
      ["review", 10],
      ["gate", 12],
      ["review", 14],
      ["gate", 16],
    ],
  );
  assert.equal(outcomeTone("landed"), "success");
  assert.equal(outcomeTone("over_budget"), "warning");
  assert.equal(outcomeTone("aborted"), "danger");
  assert.equal(outcomeTone("in_flight"), "info");
  assert.equal(outcomeTone("cancelled"), "neutral");
});

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  blockerText,
  deployNote,
  gapText,
  groupByState,
  normalizeFeatureMap,
  objectiveName,
  ownerText,
  sourceLabel,
  verificationNote,
} from "../src/features.ts";
import type { FeatureBlocker } from "../src/features.ts";

const blocker = (value: Partial<FeatureBlocker>): FeatureBlocker => ({
  kind: "unknown",
  reason: null,
  event: null,
  at_ms: null,
  task: null,
  agent_id: null,
  session_id: null,
  scenario: null,
  ...value,
});

const feature = (value: Record<string, unknown>) => ({
  id: "x",
  title: "X",
  state: "planned",
  planned: true,
  implemented: false,
  verified: false,
  live: false,
  objectives: [],
  objective_rows: [],
  sources: [],
  unowned: true,
  gaps: [],
  evidence: { tasks: [], landings: [], verifications: [] },
  ...value,
});

test("the four states are normalized as they come, one column each", () => {
  const map = normalizeFeatureMap({
    generated_at_ms: 1000,
    missions: [
      {
        id: 4,
        text: "Spawn RSI",
        features: [
          feature({ id: "p", title: "Planned", state: "planned" }),
          feature({
            id: "i",
            title: "Implemented",
            state: "implemented",
            planned: false,
            implemented: true,
          }),
          feature({
            id: "v",
            title: "Verified",
            state: "verified",
            planned: false,
            implemented: true,
            verified: true,
          }),
          feature({
            id: "l",
            title: "Live",
            state: "live",
            planned: false,
            implemented: true,
            verified: true,
            live: true,
          }),
        ],
      },
    ],
  });
  const columns = groupByState(map.missions[0].features);
  assert.deepEqual(
    columns.map((column) => [column.state, column.features.map((entry) => entry.id)]),
    [
      ["planned", ["p"]],
      ["implemented", ["i"]],
      ["verified", ["v"]],
      ["live", ["l"]],
    ],
  );
});

test("a live capability is not silently verified", () => {
  const map = normalizeFeatureMap({
    missions: [
      {
        id: 4,
        features: [
          feature({
            id: "live-unverified",
            state: "live",
            planned: false,
            implemented: true,
            verified: false,
            live: true,
            evidence: {
              tasks: [],
              landings: [],
              verifications: [
                { objective: 1, result: "none", by: null, how: null, at_ms: null },
              ],
            },
          }),
        ],
      },
    ],
  });
  const entry = map.missions[0].features[0];
  assert.equal(entry.state, "live");
  assert.equal(entry.verified, false, "the server's boolean, not the state's");
  assert.equal(verificationNote(entry), "never verified");
});

test("a verification after a landing is read; a failed one says so", () => {
  const base = {
    evidence: {
      tasks: [],
      landings: [],
      verifications: [
        { objective: 1, result: "pass", by: "lead", how: "read it", at_ms: 20 },
        { objective: 1, result: "fail", by: "rev", how: "saw it", at_ms: 30 },
      ],
    },
  };
  const failed = normalizeFeatureMap({ missions: [{ id: 4, features: [feature(base)] }] })
    .missions[0].features[0];
  assert.equal(verificationNote(failed), "verification fail by rev");
  const passed = normalizeFeatureMap({
    missions: [
      {
        id: 4,
        features: [
          feature({
            state: "verified",
            planned: false,
            implemented: true,
            verified: true,
            evidence: {
              tasks: [],
              landings: [],
              verifications: [{ objective: 1, result: "pass", by: "lead", at_ms: 20 }],
            },
          }),
        ],
      },
    ],
  }).missions[0].features[0];
  assert.equal(verificationNote(passed), "verified by lead");
  // The newest pass is picked across objectives; when the server does not
  // call the capability verified (a landed sibling is unverified), an
  // Implemented card must not say "verified by".
  const partly = normalizeFeatureMap({
    missions: [
      {
        id: 4,
        features: [
          feature({
            state: "implemented",
            planned: false,
            implemented: true,
            verified: false,
            evidence: {
              tasks: [],
              landings: [],
              verifications: [
                { objective: 1, result: "pass", by: "lead", at_ms: 30 },
                { objective: 2, result: "fail", by: "rev", at_ms: 20 },
              ],
            },
          }),
        ],
      },
    ],
  }).missions[0].features[0];
  assert.equal(partly.state, "implemented");
  assert.equal(
    verificationNote(partly),
    "latest pass by lead; another landed objective is not verified",
  );
});

test("gaps name the objective that is behind, not a percentage", () => {
  assert.equal(
    gapText({ kind: "unverified", objectives: [1, 2], detail: null }, 4),
    "landed, not verified: 4/001, 4/002",
  );
  assert.equal(
    gapText({ kind: "not_deployed", objectives: [3], detail: null }, 4),
    "landed after the newest successful deploy: 4/003",
  );
  assert.equal(
    gapText({ kind: "live_not_observable", objectives: [], detail: null }, 6),
    "no deploy ships this source: live is not observable",
  );
  assert.equal(
    gapText({ kind: "no_task", objectives: [], detail: null }, 4),
    "no task has ever been assigned",
  );
  assert.equal(objectiveName(4, 3), "4/003");
  assert.equal(objectiveName(4, null), "objective ?");
});

test("sources keep their kind and stable reference, native or external", () => {
  const map = normalizeFeatureMap({
    missions: [
      {
        id: 4,
        features: [
          feature({
            sources: [
              { kind: "objective", ref: "objective:4/003", native: true, linked: true },
              {
                kind: "issue",
                ref: "issue:63",
                url: "https://github.com/thinkseek/spawn/issues/63",
                native: false,
                linked: false,
              },
              { kind: "request", ref: "request:owner-1", title: "the owner asked" },
            ],
          }),
        ],
      },
    ],
  });
  const sources = map.missions[0].features[0].sources;
  assert.equal(sources.length, 3);
  assert.equal(sourceLabel(sources[0]), "Objective 4/003");
  assert.equal(sourceLabel(sources[1]), "Issue #63");
  assert.equal(sourceLabel(sources[2]), "Owner request");
  assert.equal(sources[1].native, false);
  assert.equal(sources[1].url, "https://github.com/thinkseek/spawn/issues/63");
});

test("a deploy note distinguishes a failure from a live ship", () => {
  const featureWith = (deploy: Record<string, unknown>) =>
    normalizeFeatureMap({
      missions: [
        {
          id: 4,
          features: [
            feature({
              evidence: { tasks: [], landings: [], verifications: [], deploy },
            }),
          ],
        },
      ],
    }).missions[0].features[0];
  assert.equal(
    deployNote(featureWith({ ok: true, head: "a".repeat(40), event: 9, at_ms: 10 })),
    `deployed ${"a".repeat(12)}`,
  );
  assert.equal(
    deployNote(featureWith({ ok: false, head: "b".repeat(40), reason: "the build failed" })),
    `deploy ${"b".repeat(12)} failed: the build failed`,
  );
  assert.equal(deployNote(featureWith(null)), "no deploy record");
});

test("blockers and owners read as words", () => {
  assert.equal(
    blockerText(blocker({ kind: "mission_paused", reason: "owner hold" })),
    "mission paused: owner hold",
  );
  assert.equal(
    blockerText(blocker({ kind: "sent_back", task: 7, reason: "add a test" })),
    "task #7 sent back: add a test",
  );
  assert.equal(
    ownerText({
      task: 2,
      agent_id: "agent_w2",
      session_id: "session_w2",
      kind: "ephemeral",
      state: "delivered",
      since_ms: 1,
    }),
    "agent_w2 · task #2 (delivered)",
  );
  assert.equal(ownerText(null), null);
});

test("a partial answer normalizes to empty values instead of crashing", () => {
  const map = normalizeFeatureMap({ missions: [{}, { id: 6, features: [{}] }] });
  assert.equal(map.missions.length, 2);
  assert.equal(map.missions[0].backlog_error, null);
  assert.equal(map.missions[1].features[0].state, "planned");
  assert.deepEqual(map.missions[1].features[0].sources, []);
  assert.equal(map.missions[1].features[0].evidence.deploy, null);
  assert.equal(normalizeFeatureMap(null).missions.length, 0);
  // A backlog that could not be read is carried to the page, not hidden by
  // an empty answer.
  const unreadable = normalizeFeatureMap({
    missions: [
      {
        id: 4,
        backlog: "/m/backlog",
        backlog_error: "read /m/backlog: No such file or directory",
        features: [],
      },
    ],
  }).missions[0];
  assert.equal(unreadable.backlog_error, "read /m/backlog: No such file or directory");
  assert.equal(unreadable.features.length, 0);
});

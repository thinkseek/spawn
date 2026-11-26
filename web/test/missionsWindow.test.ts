import assert from "node:assert/strict";
import { test } from "node:test";

import {
  formatDifficultyLabel,
  formatTaskGrading,
  normalizeMissions,
  taskDifficultyTone,
  tierRoleForDifficulty,
} from "../src/missions.ts";
import { foldIdle, foldIdleSteady, sameFoldMap } from "../src/timeFold.ts";
import { todayWindow } from "../src/timeWindow.ts";

const T0 = 1_780_000_000_000;
const M = 60_000;
const H = 60 * M;
const DAY = 24 * H;
const identity = (time: number) => time;

test("the missions open on the last 24 h, followed to the live end (design §4.4)", () => {
  // Six days of tasks, one open now.
  const now = T0 + 6 * DAY;
  assert.deepEqual(todayWindow(identity, { start: T0, end: now }, now, DAY), {
    mode: "since",
    from: now - DAY,
  });
  // Through the fold: the same real time, on the folded axis.
  const shift = (time: number) => time - 5 * H;
  assert.deepEqual(todayWindow(shift, { start: T0, end: now }, now, DAY), {
    mode: "since",
    from: now - DAY - 5 * H,
  });
  // Quiet for three hours: still the last 24 h before now.
  assert.deepEqual(todayWindow(identity, { start: T0, end: now - 3 * H }, now, DAY), {
    mode: "since",
    from: now - DAY,
  });
  // Quiet for two days: the last day with any work, not an empty plot.
  assert.deepEqual(todayWindow(identity, { start: T0, end: now - 2 * DAY }, now, DAY), {
    mode: "since",
    from: now - 3 * DAY,
  });
  // Younger than a day: the whole of it.
  assert.deepEqual(todayWindow(identity, { start: now - 5 * H, end: now }, now, DAY), { mode: "fit" });
  assert.deepEqual(todayWindow(identity, { start: now - DAY, end: now }, now, DAY), { mode: "fit" });
});

test("the missions fold's band width holds while an open bar grows", () => {
  // Two bursts a day apart, the second still open: its end is now.
  const at = (now: number) =>
    [
      [T0, T0 + 2 * H],
      [T0 + DAY, now],
    ] as const;
  const plain = [foldIdle(at(T0 + DAY + H)), foldIdle(at(T0 + DAY + H + 1_000))];
  assert.notEqual(plain[0].keep, plain[1].keep, "3% of a growing active time moves each second");
  assert.ok(!sameFoldMap(plain[0], plain[1]));
  const steady = [foldIdleSteady(at(T0 + DAY + H)), foldIdleSteady(at(T0 + DAY + H + 1_000))];
  assert.equal(steady[0].keep, steady[1].keep);
  assert.ok(sameFoldMap(steady[0], steady[1]), "so the window is not refolded each second");
  // A power of two, at most the plain width and more than half of it.
  const keep = steady[0].keep;
  assert.equal(2 ** Math.log2(keep), keep);
  assert.equal(Number.isInteger(Math.log2(keep)), true);
  assert.ok(keep <= plain[0].keep && keep > plain[0].keep / 2, `${keep} vs ${plain[0].keep}`);
  // Nothing to fold, or a keep given: as foldIdle.
  assert.equal(foldIdleSteady([[T0, T0 + H]]).breaks.length, 0);
  assert.equal(foldIdleSteady(at(T0 + DAY + H), { keep: 1234 }).keep, 1234);
});

test("formatDifficultyLabel, taskDifficultyTone, and tierRoleForDifficulty cover L1/L2/L3 and null", () => {
  assert.equal(formatDifficultyLabel("l1"), "L1");
  assert.equal(formatDifficultyLabel("l2"), "L2");
  assert.equal(formatDifficultyLabel("l3"), "L3");
  assert.equal(formatDifficultyLabel(null), null);

  assert.equal(taskDifficultyTone("l1"), "neutral");
  assert.equal(taskDifficultyTone("l2"), "info");
  assert.equal(taskDifficultyTone("l3"), "purple");
  assert.equal(taskDifficultyTone(null), "neutral");

  assert.equal(tierRoleForDifficulty("l1"), "mechanical-offload");
  assert.equal(tierRoleForDifficulty("l2"), "mainline");
  assert.equal(tierRoleForDifficulty("l3"), "top-decision");
  assert.equal(tierRoleForDifficulty(null), null);
});

test("formatTaskGrading hides ungraded and legacy unrouted default l2 rows gracefully", () => {
  assert.equal(formatTaskGrading({}), null);
  assert.equal(
    formatTaskGrading({
      difficulty: null,
      routed_difficulty: null,
      preset_id: null,
      first_preset_id: null,
      tier_role: null,
      quality_rejections: 0,
      engine_label: null,
      model_label: null,
      execution_profile: null,
      rework_step_up: null,
    }),
    null,
  );
  // Historical Postgres default: difficulty=2 ('l2'), quality_rejections=0,
  // no preset/first_preset/step-up, and MISSION_BOARD fills tier_role='mainline'.
  assert.equal(
    formatTaskGrading({
      difficulty: "l2",
      routed_difficulty: "l2",
      tier_role: "mainline",
      preset_id: null,
      first_preset_id: null,
      quality_rejections: 0,
      engine_label: null,
      model_label: null,
      execution_profile: null,
      rework_step_up: null,
    }),
    null,
  );
});

test("formatTaskGrading renders L1, L2, and L3 direct-routed and explicit unrouted tasks", () => {
  const l1 = formatTaskGrading({
    difficulty: "l1",
    routed_difficulty: "l1",
    tier_role: "mechanical-offload",
    preset_id: "topdown-l1-mechanical",
    first_preset_id: "topdown-l1-mechanical",
    quality_rejections: 0,
    engine_label: "claude",
    model_label: "deepseek-v4.1-flash",
    execution_profile: "mechanical-l1",
    rework_step_up: null,
  });
  assert.deepEqual(l1, {
    difficulty: "l1",
    routedDifficulty: "l1",
    difficultyLabel: "L1",
    routedDifficultyLabel: "L1",
    initialDifficultyTone: "neutral",
    difficultyTone: "neutral",
    difficultyBadge: "L1",
    compactDifficultyBadge: "L1",
    stepUpLabel: null,
    steppedUp: false,
    tierRole: "mechanical-offload",
    presetId: "topdown-l1-mechanical",
    firstPresetId: "topdown-l1-mechanical",
    rerouted: false,
    presetTrajectory: null,
    presetBadge: "topdown-l1-mechanical",
    engineLabel: "claude",
    modelLabel: "deepseek-v4.1-flash",
    engineModelLabel: "claude/deepseek-v4.1-flash",
    executionProfile: "mechanical-l1",
    qualityRejections: 0,
    hasReworkHistory: false,
    reworkBadge: null,
    compactReworkBadge: null,
    reworkNote: null,
    difficultyTitle: "Difficulty L1 · tier mechanical-offload",
    presetTitle: "Preset topdown-l1-mechanical · profile mechanical-l1 · claude/deepseek-v4.1-flash",
    reworkTitle: "",
    tooltipLine: "L1 · mechanical-offload · topdown-l1-mechanical · claude/deepseek-v4.1-flash",
    reworkStepUp: null,
  });

  const l2 = formatTaskGrading({
    difficulty: "l2",
    routed_difficulty: "l2",
    tier_role: "mainline",
    preset_id: "topdown-l2-neon",
    first_preset_id: "topdown-l2-neon",
    quality_rejections: 0,
    engine_label: "agy",
    model_label: "gemini-pro-orchestrator-neon",
    execution_profile: "neon-mainline",
    rework_step_up: null,
  });
  assert.equal(l2?.difficultyBadge, "L2");
  assert.equal(l2?.difficultyTone, "info");
  assert.equal(l2?.presetBadge, "topdown-l2-neon");
  assert.equal(l2?.reworkBadge, null);

  const l3 = formatTaskGrading({
    difficulty: "l3",
    routed_difficulty: "l3",
    tier_role: "top-decision",
    preset_id: "topdown-l3-fable",
    first_preset_id: "topdown-l3-fable",
    quality_rejections: 0,
    engine_label: null,
    model_label: "claude-fable-5-2",
    execution_profile: null,
    rework_step_up: null,
  });
  assert.equal(l3?.difficultyBadge, "L3");
  assert.equal(l3?.difficultyTone, "purple");
  assert.equal(l3?.engineModelLabel, "claude-fable-5-2");

  const explicitUnroutedL1 = formatTaskGrading({
    difficulty: "l1",
    routed_difficulty: "l1",
    quality_rejections: 0,
  });
  assert.equal(explicitUnroutedL1?.difficultyBadge, "L1");
  assert.equal(explicitUnroutedL1?.tierRole, "mechanical-offload");
  assert.equal(explicitUnroutedL1?.steppedUp, false);
});

test("formatTaskGrading handles same-tier quality rejection without step-up", () => {
  const sameTier = formatTaskGrading({
    difficulty: "l2",
    routed_difficulty: "l2",
    tier_role: "mainline",
    preset_id: "topdown-l2-neon",
    first_preset_id: "topdown-l2-neon",
    quality_rejections: 1,
    engine_label: "agy",
    model_label: "gemini-pro-orchestrator-neon",
    execution_profile: "neon-mainline",
    rework_step_up: {
      quality_rejections: 1,
      from_difficulty: "l2",
      routed_difficulty: "l2",
      stepped_up: false,
      first_preset_id: "topdown-l2-neon",
      preset_id: "topdown-l2-neon",
    },
  });
  assert.equal(sameTier?.steppedUp, false);
  assert.equal(sameTier?.rerouted, false);
  assert.equal(sameTier?.difficultyBadge, "L2");
  assert.equal(sameTier?.compactDifficultyBadge, "L2");
  assert.equal(sameTier?.difficultyTone, "info");
  assert.equal(sameTier?.initialDifficultyTone, "info");
  assert.equal(sameTier?.qualityRejections, 1);
  assert.equal(sameTier?.hasReworkHistory, true);
  assert.equal(sameTier?.reworkBadge, "1 quality rej");
  assert.equal(sameTier?.compactReworkBadge, "1r");
  assert.equal(sameTier?.reworkNote, "same tier (L2)");
});

test("formatTaskGrading formats l1 -> l2, l1 -> l3, and l2 -> l3 difficulty step-ups and cross-preset reroutes", () => {
  const l1ToL2 = formatTaskGrading({
    difficulty: "l1",
    routed_difficulty: "l2",
    tier_role: "mainline",
    preset_id: "topdown-l2-neon",
    first_preset_id: "topdown-l1-mechanical",
    quality_rejections: 1,
    engine_label: "agy",
    model_label: "gemini-pro-orchestrator-neon",
    execution_profile: "neon-mainline",
    rework_step_up: {
      quality_rejections: 1,
      from_difficulty: "l1",
      routed_difficulty: "l2",
      stepped_up: true,
      first_preset_id: "topdown-l1-mechanical",
      preset_id: "topdown-l2-neon",
    },
  });
  assert.equal(l1ToL2?.steppedUp, true);
  assert.equal(l1ToL2?.rerouted, true);
  assert.equal(l1ToL2?.initialDifficultyTone, "neutral");
  assert.equal(l1ToL2?.difficultyTone, "warning");
  assert.equal(l1ToL2?.difficultyBadge, "L1 → L2");
  assert.equal(l1ToL2?.compactDifficultyBadge, "L1→L2");
  assert.equal(l1ToL2?.stepUpLabel, "L1 → L2");
  assert.equal(l1ToL2?.presetTrajectory, "topdown-l1-mechanical → topdown-l2-neon");
  assert.equal(l1ToL2?.presetBadge, "topdown-l1-mechanical → topdown-l2-neon");
  assert.equal(l1ToL2?.reworkBadge, "step-up · 1 rej");
  assert.equal(l1ToL2?.compactReworkBadge, "↑1");
  assert.equal(l1ToL2?.reworkNote, "stepped up L1 → L2");

  const l2ToL3 = formatTaskGrading({
    difficulty: "l2",
    routed_difficulty: "l3",
    tier_role: "top-decision",
    preset_id: "topdown-l3-fable",
    first_preset_id: "topdown-l2-neon",
    quality_rejections: 2,
    engine_label: "claude",
    model_label: "claude-fable-5-2",
    execution_profile: "fable-5-2",
    rework_step_up: {
      quality_rejections: 2,
      from_difficulty: "l2",
      routed_difficulty: "l3",
      stepped_up: true,
      first_preset_id: "topdown-l2-neon",
      preset_id: "topdown-l3-fable",
    },
  });
  assert.equal(l2ToL3?.initialDifficultyTone, "info");
  assert.equal(l2ToL3?.difficultyTone, "warning");
  assert.equal(l2ToL3?.difficultyBadge, "L2 → L3");
  assert.equal(l2ToL3?.compactDifficultyBadge, "L2→L3");
  assert.equal(l2ToL3?.presetBadge, "topdown-l2-neon → topdown-l3-fable");
  assert.equal(l2ToL3?.reworkBadge, "step-up · 2 rej");
  assert.equal(l2ToL3?.compactReworkBadge, "↑2");
  assert.equal(l2ToL3?.reworkNote, "stepped up L2 → L3");

  const sameTierReroute = formatTaskGrading({
    difficulty: "l2",
    routed_difficulty: "l2",
    tier_role: "mainline",
    preset_id: "topdown-l2-backup",
    first_preset_id: "topdown-l2-neon",
    quality_rejections: 0,
    rework_step_up: {
      quality_rejections: 0,
      from_difficulty: "l2",
      routed_difficulty: "l2",
      stepped_up: false,
      first_preset_id: "topdown-l2-neon",
      preset_id: "topdown-l2-backup",
    },
  });
  assert.equal(sameTierReroute?.steppedUp, false);
  assert.equal(sameTierReroute?.rerouted, true);
  assert.equal(sameTierReroute?.presetTrajectory, "topdown-l2-neon → topdown-l2-backup");
  assert.equal(sameTierReroute?.reworkBadge, "rerouted");
  assert.equal(sameTierReroute?.compactReworkBadge, "⇄");
  assert.equal(sameTierReroute?.reworkNote, "rerouted (L2)");
});

test("normalizeMissions parses native SQL grading fields and tolerates historical/missing fields", () => {
  const normalized = normalizeMissions({
    missions: [
      {
        id: 99,
        text: "Grading rollout",
        state: "active",
        tasks: [
          {
            id: 101,
            state: "landed",
            difficulty: "l2",
            routed_difficulty: "l2",
            tier_role: "mainline",
            preset_id: null,
            first_preset_id: null,
            quality_rejections: 0,
            rework_step_up: null,
            created_at_ms: T0,
          },
          {
            id: 102,
            state: "revise",
            difficulty: "L1",
            routed_difficulty: "L3",
            tier_role: "TOP-DECISION",
            preset_id: "topdown-l3-fable",
            first_preset_id: "topdown-l1-mechanical",
            quality_rejections: 3.9,
            engine_label: "claude",
            model_label: "claude-fable-5-2",
            execution_profile: "fable-5-2",
            rework_step_up: {
              quality_rejections: 3,
              from_difficulty: "L1",
              routed_difficulty: "L3",
              stepped_up: true,
              first_preset_id: "topdown-l1-mechanical",
              preset_id: "topdown-l3-fable",
            },
            created_at_ms: T0 + M,
          },
          {
            id: 103,
            state: "assigned",
            // Completely absent grading fields (pre-grading server).
            created_at_ms: T0 + 2 * M,
          },
        ],
      },
    ],
  });

  const tasks = normalized.missions[0].tasks;
  assert.equal(tasks.length, 3);
  assert.equal(formatTaskGrading(tasks[0]), null);
  assert.equal(formatTaskGrading(tasks[2]), null);

  const graded = tasks[1];
  assert.equal(graded.difficulty, "l1");
  assert.equal(graded.routed_difficulty, "l3");
  assert.equal(graded.tier_role, "top-decision");
  assert.equal(graded.quality_rejections, 3);
  assert.equal(graded.rework_step_up?.from_difficulty, "l1");
  assert.equal(graded.rework_step_up?.routed_difficulty, "l3");

  const view = formatTaskGrading(graded);
  assert.equal(view?.difficultyBadge, "L1 → L3");
  assert.equal(view?.compactDifficultyBadge, "L1→L3");
  assert.equal(view?.presetTrajectory, "topdown-l1-mechanical → topdown-l3-fable");
  assert.equal(view?.reworkBadge, "step-up · 3 rej");
  assert.equal(view?.compactReworkBadge, "↑3");
  assert.equal(view?.reworkNote, "stepped up L1 → L3");
});

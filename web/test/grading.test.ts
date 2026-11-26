import assert from "node:assert/strict";
import { test } from "node:test";

import {
  groupByTopDownTier,
  normalizeGrading,
  taskGradingSummary,
  tierRoleForGrade,
} from "../src/grading.ts";

test("tierRoleForGrade maps top-down tiers cleanly and handles null", () => {
  assert.equal(tierRoleForGrade("l3"), "top-decision");
  assert.equal(tierRoleForGrade("l2"), "mainline");
  assert.equal(tierRoleForGrade("l1"), "mechanical-offload");
  assert.equal(tierRoleForGrade(null), null);
});

test("normalizeGrading handles empty, malformed, and top-down catalog payloads", () => {
  assert.deepEqual(normalizeGrading(null), { presets: [], profiles: [] });
  assert.deepEqual(normalizeGrading({ presets: [null, { engine_label: "agy" }] }), {
    presets: [],
    profiles: [],
  });

  const catalog = normalizeGrading({
    presets: [
      {
        preset_id: "topdown-l3-fable",
        engine_label: "claude",
        model_label: "claude-fable-5-2",
        model_profile: "default",
        execution_profile: "fable-5-2",
        cost_rank: 30,
        state: "active",
        locked_grade: "l3",
        routing_grade: "l3",
        driver: "claude",
        label_only: true,
        tier_role: "top-decision",
        reprobe_due: false,
        window_start_ms: 1000,
      },
      {
        preset_id: "topdown-l2-neon",
        engine_label: "agy",
        model_label: "gemini-pro-orchestrator-neon",
        model_profile: "default",
        execution_profile: "neon-mainline",
        cost_rank: 20,
        state: "active",
        locked_grade: "l2",
        routing_grade: "l2",
        driver: "agy",
        label_only: false,
        // Deliberately omit tier_role & reprobe_due to test client fallback.
      },
      {
        preset_id: "topdown-l1-mechanical",
        engine_label: "claude",
        model_label: "deepseek-v4.1-flash",
        model_profile: "cheap",
        execution_profile: "mechanical-l1",
        cost_rank: 10,
        state: "demoted",
        onboarded_grade: "l2",
        calibrated_grade: "l1",
        routing_grade: "l1",
        reprobe_due: true,
      },
      {
        preset_id: "fresh-unexamined",
        engine_label: "claude",
        model_label: "kimi",
        model_profile: "cheap",
        cost_rank: 5,
        state: "unexamined",
      },
    ],
  });

  assert.equal(catalog.presets.length, 4);
  assert.deepEqual(catalog.profiles, ["cheap", "fable-5-2", "mechanical-l1", "neon-mainline"]);

  const l2 = catalog.presets.find((p) => p.preset_id === "topdown-l2-neon");
  assert.equal(l2?.tier_role, "mainline");
  assert.equal(l2?.reprobe_due, false);
  assert.equal(l2?.driver, "agy");

  const l1 = catalog.presets.find((p) => p.preset_id === "topdown-l1-mechanical");
  assert.equal(l1?.tier_role, "mechanical-offload");
  assert.equal(l1?.reprobe_due, true);

  const groups = groupByTopDownTier(catalog.presets);
  assert.deepEqual(
    groups.map((g) => g.id),
    ["t3", "t2", "t1", "ungraded"],
  );
  assert.equal(groups[0].presets[0].model_label, "claude-fable-5-2");
  assert.equal(groups[1].presets[0].model_label, "gemini-pro-orchestrator-neon");
  assert.equal(groups[2].presets[0].model_label, "deepseek-v4.1-flash");
  assert.equal(groups[3].presets[0].preset_id, "fresh-unexamined");
});

test("groupByTopDownTier sorts active ahead of demoted and omits empty ungraded bucket", () => {
  const catalog = normalizeGrading({
    presets: [
      {
        preset_id: "demoted-l2",
        model_label: "legacy",
        cost_rank: 1,
        state: "demoted",
        calibrated_grade: "l2",
        routing_grade: "l2",
      },
      {
        preset_id: "topdown-l2-neon",
        model_label: "gemini-pro-orchestrator-neon",
        cost_rank: 20,
        state: "active",
        locked_grade: "l2",
        routing_grade: "l2",
      },
    ],
  });

  const groups = groupByTopDownTier(catalog.presets);
  assert.deepEqual(
    groups.map((g) => g.id),
    ["t3", "t2", "t1"],
  );
  assert.deepEqual(
    groups[1].presets.map((p) => p.preset_id),
    ["topdown-l2-neon", "demoted-l2"],
  );
  assert.equal(groups[0].presets.length, 0);
  assert.equal(groups[2].presets.length, 0);
});

test("taskGradingSummary gracefully returns null when grading fields are absent and detects reroutes", () => {
  assert.equal(taskGradingSummary({}), null);
  assert.equal(
    taskGradingSummary({ difficulty: null, preset_id: null, quality_rejections: 0 }),
    null,
  );

  const stepped = taskGradingSummary({
    difficulty: "l1",
    preset_id: "topdown-l2-neon",
    first_preset_id: "topdown-l1-mechanical",
    quality_rejections: 1,
    execution_profile: "neon-mainline",
  });
  assert.deepEqual(stepped, {
    difficulty: "l1",
    tierRole: "mechanical-offload",
    presetId: "topdown-l2-neon",
    firstPresetId: "topdown-l1-mechanical",
    rerouted: true,
    qualityRejections: 1,
    executionProfile: "neon-mainline",
  });
});

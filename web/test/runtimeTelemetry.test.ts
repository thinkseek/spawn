import assert from "node:assert/strict";
import { test } from "node:test";

import {
  INITIAL_RUNTIME_TELEMETRY,
  classifyCostAmount,
  formatCostAmountText,
  formatTelemetryUsd,
  normalizeCostWindowTelemetry,
  normalizeFleetTelemetry,
  normalizeGradingTelemetry,
} from "../src/runtimeTelemetry.ts";

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

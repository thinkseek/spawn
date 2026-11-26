import assert from "node:assert/strict";
import { test } from "node:test";

import {
  featureTruthSummaryFor,
  filterPurityFindings,
  filterTruthFindings,
  isEnforcedTruthFinding,
  normalizeFeatureTruth,
  parseFeatureTruthResponse,
  truthUrl,
} from "../src/featureCoverage.ts";

function sampleTruthPayload(overrides: Record<string, unknown> = {}) {
  return {
    schema_version: 1,
    project: "Spawn",
    root: "/repo",
    registry_path: "/repo/docs/feature-map/registry.json",
    git_head: "96b2f5ca61032da4eb4e353ec8b71578e0180214",
    today: "2026-11-26",
    projection: {
      file_edges: 20,
      structural: 2,
      unowned: 1,
      internal: 10,
      hierarchy: 3,
      declared: 3,
      implied: 1,
      undeclared: 2,
    },
    counts: {
      error: 1,
      warn: 2,
      info: 1,
      waived: 1,
      enforced_violations: 1,
      under_declared: 1,
      implied_only: 1,
      test_only_undeclared: 1,
      ghost: 0,
      inverted: 0,
      unverifiable: 0,
      static_cycle: 1,
      basis_without_note: 0,
      kernel_leak: 0,
      entrypoint_imported: 0,
      waiver_unused: 0,
      by_code: {
        under_declared: 1,
        implied_only: 1,
        test_only_undeclared: 1,
        static_cycle: 1,
      },
    },
    findings: [
      {
        code: "under_declared",
        severity: "error",
        waived: false,
        from_any_of: ["cap-a"],
        to_any_of: ["cap-b"],
        evidence_total: 2,
        evidence: [
          {
            from: "crates/spawn-cli/src/a.rs",
            to: "crates/spawn-core/src/b.rs",
            kind: "use",
            scope: "prod",
            line: 14,
            precision: "item",
          },
        ],
        fix: "add depends_on cap-b on cap-a",
      },
      {
        code: "implied_only",
        severity: "warn",
        waived: false,
        from_any_of: ["cap-a"],
        to_any_of: ["cap-c"],
        evidence_total: 1,
        evidence: [
          {
            from: "crates/spawn-cli/src/a.rs",
            to: "crates/spawn-core/src/c.rs",
            kind: "use",
            scope: "prod",
            line: 22,
            precision: "item",
          },
        ],
        fix: "declare direct depends_on or route through cap-b",
      },
      {
        code: "test_only_undeclared",
        severity: "info",
        waived: true,
        from_any_of: ["cap-d"],
        to_any_of: ["cap-b"],
        evidence_total: 1,
        evidence: [
          {
            from: "web/test/foo.test.ts",
            to: "web/src/bar.ts",
            kind: "import",
            scope: "test",
            line: 3,
            precision: "item",
          },
        ],
        fix: "test-only edge; optional depends_on",
      },
      {
        code: "static_cycle",
        severity: "warn",
        waived: false,
        features: ["cap-x", "cap-y"],
        evidence_total: 0,
        evidence: [],
        fix: "break cycle between cap-x and cap-y",
      },
    ],
    purity: {
      schema_version: 1,
      clean: false,
      counts: {
        error: 1,
        warn: 0,
        by_code: {
          domain_direct_files: 1,
        },
      },
      findings: [
        {
          code: "domain_direct_files",
          severity: "error",
          feature_id: "dom-bad",
          detail: "domain root owns 1 file(s) directly",
          sample_paths: ["crates/spawn-cli/src/main.rs"],
          fix: "move files from this domain root into child capability nodes",
        },
      ],
    },
    graph: {
      units: 7,
      files: 120,
      edges: 400,
      diagnostics: 0,
    },
    ...overrides,
  };
}

test("truthUrl builds scoped URLs cleanly", () => {
  assert.equal(truthUrl(), "/api/features/truth");
  assert.equal(truthUrl({ mission: null, project: null }), "/api/features/truth");
  assert.equal(truthUrl({ mission: 4 }), "/api/features/truth?mission=4");
  assert.equal(truthUrl({ project: "My Project" }), "/api/features/truth?project=My%20Project");
  assert.equal(truthUrl({ project: "   " }), "/api/features/truth");
  assert.equal(truthUrl({ mission: 4, project: "Ignored" }), "/api/features/truth?mission=4");
});

test("parseFeatureTruthResponse parses valid payloads and rejects malformed responses", () => {
  const parsed = parseFeatureTruthResponse(sampleTruthPayload());
  assert.equal(parsed.project, "Spawn");
  assert.equal(parsed.counts.under_declared, 1);
  assert.equal(parsed.counts.enforced_violations, 1);
  assert.equal(parsed.enforced_violations.length, 1);
  assert.equal(isEnforcedTruthFinding(parsed.findings[0]), true);
  assert.equal(isEnforcedTruthFinding(parsed.findings[1]), false);
  assert.equal(parsed.purity.clean, false);
  assert.equal(parsed.purity.findings.length, 1);

  assert.throws(() => parseFeatureTruthResponse(null), /JSON object/);
  assert.throws(() => parseFeatureTruthResponse({}), /schema_version/);
  assert.throws(
    () => parseFeatureTruthResponse({ schema_version: 1, counts: {} }),
    /findings array/,
  );
  assert.throws(
    () => parseFeatureTruthResponse({ schema_version: 1, counts: {}, findings: [], purity: {} }),
    /purity/,
  );
});

test("normalizeFeatureTruth fills derived counts when fields are omitted", () => {
  const normalized = normalizeFeatureTruth({
    schema_version: 1,
    counts: {},
    findings: [
      {
        code: "ghost",
        severity: "error",
        waived: false,
        from_any_of: ["cap-g"],
        to_any_of: ["cap-h"],
        evidence: [],
        fix: "remove ghost depends_on",
      },
    ],
    purity: {
      counts: { error: 0, warn: 0 },
      findings: [],
    },
  });
  assert.equal(normalized.counts.ghost, 1);
  assert.equal(normalized.counts.enforced_violations, 1);
  assert.equal(normalized.enforced_violations.length, 1);
  assert.equal(normalized.purity.clean, true);
});

test("filterTruthFindings and filterPurityFindings filter by severity, code, waived, and query", () => {
  const data = parseFeatureTruthResponse(sampleTruthPayload());

  assert.equal(filterTruthFindings(data.findings, {}).length, 4);
  assert.equal(filterTruthFindings(data.findings, { includeWaived: false }).length, 3);
  assert.equal(filterTruthFindings(data.findings, { severity: "error" }).length, 1);
  assert.equal(filterTruthFindings(data.findings, { code: "static_cycle" }).length, 1);
  assert.equal(filterTruthFindings(data.findings, { query: "cap-c" }).length, 1);
  assert.equal(filterTruthFindings(data.findings, { query: "foo.test.ts" }).length, 1);
  assert.equal(
    filterTruthFindings(data.findings, { includeWaived: false, query: "foo.test.ts" }).length,
    0,
  );

  assert.equal(filterPurityFindings(data.purity.findings, { severity: "error" }).length, 1);
  assert.equal(filterPurityFindings(data.purity.findings, { severity: "warn" }).length, 0);
  assert.equal(filterPurityFindings(data.purity.findings, { query: "main.rs" }).length, 1);
});

test("featureTruthSummaryFor summarizes findings for one feature and degrades cleanly on null", () => {
  const data = parseFeatureTruthResponse(sampleTruthPayload());

  assert.equal(featureTruthSummaryFor(null, "cap-a"), null);

  const summaryA = featureTruthSummaryFor(data, "cap-a");
  assert.ok(summaryA);
  assert.equal(summaryA.status, "error");
  assert.equal(summaryA.truthErrors, 1);
  assert.equal(summaryA.truthWarnings, 1);
  assert.equal(summaryA.enforcedViolations, 1);
  assert.equal(summaryA.truthFindings.length, 2);

  const summaryX = featureTruthSummaryFor(data, "cap-x");
  assert.ok(summaryX);
  assert.equal(summaryX.status, "warn");
  assert.equal(summaryX.truthWarnings, 1);

  const summaryDom = featureTruthSummaryFor(data, "dom-bad");
  assert.ok(summaryDom);
  assert.equal(summaryDom.status, "error");
  assert.equal(summaryDom.purityErrors, 1);

  const summaryClean = featureTruthSummaryFor(data, "cap-unrelated");
  assert.ok(summaryClean);
  assert.equal(summaryClean.status, "clean");
});

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  accountingIdentitiesHold,
  canSubmitWildFileAssociation,
  consistencyIssueCount,
  coverageUrl,
  INITIAL_COVERAGE_SCOPE,
  isCurrentCoverageScope,
  nextCoverageScope,
  normalizeCoverageProjects,
  featureAssociatedFiles,
  fileAssociatedFeatures,
  formatBytes,
  initialWildFileSelection,
  initiativesAndSlices,
  isResearchInitiative,
  isRegistryReadOnlyRefusal,
  isScanClean,
  kindLabel,
  normalizeFeatureCoverage,
  parseCoverageHttpResponse,
  parseFeatureCoverageResponse,
  REGISTRY_READ_ONLY_CODE,
  registryReadOnlyNotice,
  toggleWildFileSelection,
} from "../src/featureCoverage.ts";

function basePayload(overrides: Record<string, unknown> = {}) {
  return {
    root: "/repo",
    registry_path: "/repo/docs/feature-map/registry.json",
    report: {
      schema_version: 1,
      scanned_at_ms: 1795000000000,
      initiative_id: "t559u",
      boundary: {
        project_name: "Spawn",
        description: "Tracked project files plus untracked in-scope files.",
        included_categories: ["tracked-source", "tests", "docs"],
        exclusions: [
          {
            id: "rust-build-output",
            pattern: "target/**",
            category: "build-cache",
            reason: "Cargo build outputs are derived.",
          },
          {
            id: "secrets-env-dotfile",
            pattern: "**/.env",
            category: "secrets",
            reason: "Secret files are excluded.",
          },
        ],
      },
      counts: {
        total_seen: 3,
        in_scope: 2,
        covered: 1,
        unassociated_wild: 1,
        stale_references: 0,
        explicit_exclusions: 1,
        multi_feature_files: 0,
        orphan_features: 0,
        invalid_relations: 0,
      },
      unassociated_files: [
        {
          path: "crates/spawn-core/src/new_wild.rs",
          tracked: false,
          size_bytes: 512,
          reason: "In-scope untracked file exists on disk with no feature association",
          evidence: ["git_tracked=false", "size_bytes=512", "matched_exclusion=none"],
          suggested_features: ["t559u-slice-b-coverage"],
        },
      ],
      stale_references: [],
      excluded_files: [
        {
          path: ".env",
          rule_id: "secrets-env-dotfile",
          category: "secrets",
          reason: "Secret files are excluded.",
        },
      ],
      bidirectional_index: {
        feature_to_files: {
          "t559u-slice-b-coverage": ["crates/spawn-core/src/feature_coverage.rs"],
        },
        file_to_features: {
          "crates/spawn-core/src/feature_coverage.rs": ["t559u-slice-b-coverage"],
        },
      },
      consistency: {
        orphan_features: [],
        dangling_relations: [],
        self_links: [],
        cycle_errors: [],
        duplicate_feature_ids: [],
        invalid_paths: [],
      },
      ...(overrides.report as Record<string, unknown> | undefined),
    },
    features: [
      {
        id: "t559u-feature-map-initiative",
        title: "Feature Map Initiative (t559u)",
        kind: "initiative",
        parent: null,
        summary: "Full-project feature map, mindmap, coverage, and storage keys.",
        status_note: "Active",
        file_count: 1,
        relations: [{ kind: "contains", target: "t559u-slice-b-coverage" }],
        provenance: { objectives: ["t559u"], commits: ["6c5d22782"] },
      },
      {
        id: "t559u-slice-b-coverage",
        title: "Slice B: File Coverage & Wild-File Scan",
        kind: "slice",
        parent: "t559u-feature-map-initiative",
        summary: "One-click wild-file scan and coverage core.",
        file_count: 1,
        relations: [],
        provenance: {},
      },
      {
        id: "t567u-multi-machine-distributed",
        title: "Multi-Machine Distributed Compute & Storage (t567u/t568u)",
        kind: "research-initiative",
        parent: null,
        summary: "Staged research/design initiative — not production.",
        status_note: "Staged research/design only",
        file_count: 1,
        relations: [{ kind: "relates_to", target: "t559u-feature-map-initiative" }],
        provenance: {},
      },
    ],
  };
}

test("t565u #1: new in-scope wild file is surfaced with path, reason, evidence, and suggestions", () => {
  const data = normalizeFeatureCoverage(basePayload());
  assert.equal(isScanClean(data.report), false);
  assert.equal(data.report.counts.unassociated_wild, 1);
  assert.equal(data.report.unassociated_files.length, 1);
  const finding = data.report.unassociated_files[0];
  assert.equal(finding.path, "crates/spawn-core/src/new_wild.rs");
  assert.match(finding.reason, /no feature association/i);
  assert.ok(finding.evidence.includes("git_tracked=false"));
  assert.deepEqual(finding.suggested_features, ["t559u-slice-b-coverage"]);
});

test("t565u #2 & #3: after many-to-many association, wild finding disappears and bidirectional index reflects both sides", () => {
  const sharedPath = "crates/spawn-core/src/new_wild.rs";
  const data = normalizeFeatureCoverage({
    ...basePayload({
      report: {
        counts: {
          total_seen: 3,
          in_scope: 2,
          covered: 2,
          unassociated_wild: 0,
          stale_references: 0,
          explicit_exclusions: 1,
          multi_feature_files: 1,
          orphan_features: 0,
          invalid_relations: 0,
        },
        unassociated_files: [],
        bidirectional_index: {
          feature_to_files: {
            "t559u-slice-b-coverage": [
              "crates/spawn-core/src/feature_coverage.rs",
              sharedPath,
            ],
            "t565u-wild-file-scan": [sharedPath],
          },
          file_to_features: {
            "crates/spawn-core/src/feature_coverage.rs": ["t559u-slice-b-coverage"],
            [sharedPath]: ["t559u-slice-b-coverage", "t565u-wild-file-scan"],
          },
        },
      },
    }),
    update: {
      path: sharedPath,
      added_to_features: ["t559u-slice-b-coverage", "t565u-wild-file-scan"],
      already_associated_features: [],
      all_associated_features: ["t559u-slice-b-coverage", "t565u-wild-file-scan"],
    },
  });

  assert.equal(isScanClean(data.report), true);
  assert.equal(data.report.counts.unassociated_wild, 0);
  assert.equal(data.report.counts.multi_feature_files, 1);
  assert.deepEqual(fileAssociatedFeatures(data.report, sharedPath), [
    "t559u-slice-b-coverage",
    "t565u-wild-file-scan",
  ]);
  assert.ok(featureAssociatedFiles(data.report, "t559u-slice-b-coverage").includes(sharedPath));
  assert.ok(featureAssociatedFiles(data.report, "t565u-wild-file-scan").includes(sharedPath));
  assert.equal(data.update?.path, sharedPath);
});

test("t565u #4: explicitly excluded files stay separate, never counted as covered or wild", () => {
  const data = normalizeFeatureCoverage(basePayload());
  assert.equal(data.report.counts.explicit_exclusions, 1);
  assert.equal(data.report.counts.in_scope, 2);
  assert.equal(data.report.counts.covered, 1);
  assert.equal(data.report.excluded_files.length, 1);
  assert.equal(data.report.excluded_files[0].path, ".env");
  assert.equal(data.report.excluded_files[0].category, "secrets");
  assert.equal(fileAssociatedFeatures(data.report, ".env").length, 0);
  assert.equal(
    data.report.unassociated_files.some((item) => item.path === ".env"),
    false,
  );
});

test("t565u #5: stale references are strictly separated from wild files", () => {
  const data = normalizeFeatureCoverage(
    basePayload({
      report: {
        counts: {
          total_seen: 2,
          in_scope: 1,
          covered: 1,
          unassociated_wild: 0,
          stale_references: 1,
          explicit_exclusions: 1,
          multi_feature_files: 0,
          orphan_features: 0,
          invalid_relations: 0,
        },
        unassociated_files: [],
        stale_references: [
          {
            path: "crates/spawn-core/src/deleted.rs",
            referencing_features: ["t559u-slice-b-coverage"],
            rename_candidates: ["crates/spawn-core/src/feature_coverage.rs"],
          },
        ],
      },
    }),
  );
  assert.equal(isScanClean(data.report), false);
  assert.equal(data.report.counts.unassociated_wild, 0);
  assert.equal(data.report.counts.stale_references, 1);
  assert.equal(data.report.stale_references[0].path, "crates/spawn-core/src/deleted.rs");
  assert.deepEqual(data.report.stale_references[0].rename_candidates, [
    "crates/spawn-core/src/feature_coverage.rs",
  ]);
});

test("initiatives, research-initiative badging, and empty-payload resilience", () => {
  const data = normalizeFeatureCoverage(basePayload());
  const tracked = initiativesAndSlices(data.features);
  assert.equal(tracked.length, 3);
  const research = tracked.find((f) => f.id === "t567u-multi-machine-distributed");
  assert.ok(research);
  assert.equal(isResearchInitiative(research), true);
  assert.equal(kindLabel(research.kind), "Research / design");
  assert.equal(consistencyIssueCount(data.report.consistency), 0);
  assert.equal(formatBytes(512), "512 B");
  assert.equal(formatBytes(2048), "2.0 KiB");

  const fallback = normalizeFeatureCoverage(null);
  assert.equal(fallback.features.length, 0);
  assert.equal(fallback.report.counts.in_scope, 0);
  assert.equal(fallback.update, null);
});

test("accountingIdentitiesHold validates exact counts and catches excluded-file leakage", () => {
  const clean = normalizeFeatureCoverage(basePayload());
  assert.equal(accountingIdentitiesHold(clean.report), true);

  // Break covered + wild == in_scope
  const badCoverage = normalizeFeatureCoverage(
    basePayload({
      report: {
        counts: {
          total_seen: 3,
          in_scope: 2,
          covered: 2,
          unassociated_wild: 1,
          stale_references: 0,
          explicit_exclusions: 1,
          multi_feature_files: 0,
          orphan_features: 0,
          invalid_relations: 0,
        },
      },
    }),
  );
  assert.equal(accountingIdentitiesHold(badCoverage.report), false);

  // Excluded file leaked into bidirectional covered index
  const leakedCovered = normalizeFeatureCoverage(
    basePayload({
      report: {
        counts: {
          total_seen: 3,
          in_scope: 2,
          covered: 2,
          unassociated_wild: 0,
          stale_references: 0,
          explicit_exclusions: 1,
          multi_feature_files: 0,
          orphan_features: 0,
          invalid_relations: 0,
        },
        unassociated_files: [],
        bidirectional_index: {
          feature_to_files: {
            "t559u-slice-b-coverage": ["crates/spawn-core/src/feature_coverage.rs", ".env"],
          },
          file_to_features: {
            "crates/spawn-core/src/feature_coverage.rs": ["t559u-slice-b-coverage"],
            ".env": ["t559u-slice-b-coverage"],
          },
        },
      },
    }),
  );
  assert.equal(accountingIdentitiesHold(leakedCovered.report), false);
});

test("orphan features and every consistency failure class mark scan not clean", () => {
  const fields = [
    "orphan_features",
    "dangling_relations",
    "self_links",
    "cycle_errors",
    "duplicate_feature_ids",
    "invalid_paths",
  ] as const;

  for (const field of fields) {
    const data = normalizeFeatureCoverage(
      basePayload({
        report: {
          counts: {
            total_seen: 2,
            in_scope: 1,
            covered: 1,
            unassociated_wild: 0,
            stale_references: 0,
            explicit_exclusions: 1,
            multi_feature_files: 0,
            orphan_features: field === "orphan_features" ? 1 : 0,
            invalid_relations:
              field === "dangling_relations" || field === "self_links" || field === "cycle_errors"
                ? 1
                : 0,
          },
          unassociated_files: [],
          consistency: {
            orphan_features: [],
            dangling_relations: [],
            self_links: [],
            cycle_errors: [],
            duplicate_feature_ids: [],
            invalid_paths: [],
            [field]: [`fixture-${field}`],
          },
        },
      }),
    );
    assert.equal(consistencyIssueCount(data.report.consistency), 1, `expected 1 issue for ${field}`);
    assert.equal(isScanClean(data.report), false, `expected scan not clean for ${field}`);
    assert.equal(accountingIdentitiesHold(data.report), true);
  }
});

test("mixed wild, stale, multi-feature, and excluded fixture stays strictly separated", () => {
  const data = normalizeFeatureCoverage(
    basePayload({
      report: {
        counts: {
          total_seen: 6,
          in_scope: 4,
          covered: 2,
          unassociated_wild: 2,
          stale_references: 1,
          explicit_exclusions: 2,
          multi_feature_files: 1,
          orphan_features: 0,
          invalid_relations: 0,
        },
        unassociated_files: [
          {
            path: "crates/spawn-core/src/unmapped_tracked.rs",
            tracked: true,
            size_bytes: 300,
            reason: "In-scope git-tracked file has no feature association in the registry",
            evidence: ["git_tracked=true", "matched_exclusion=none"],
            suggested_features: ["t559u-slice-b-coverage"],
          },
          {
            path: "docs/untracked_note.md",
            tracked: false,
            size_bytes: 120,
            reason: "In-scope untracked file exists on disk with no feature association in the registry",
            evidence: ["git_tracked=false", "matched_exclusion=none"],
            suggested_features: [],
          },
        ],
        stale_references: [
          {
            path: "crates/spawn-core/src/removed.rs",
            referencing_features: ["t559u-slice-b-coverage"],
            rename_candidates: ["crates/spawn-core/src/unmapped_tracked.rs"],
          },
        ],
        excluded_files: [
          {
            path: ".env",
            rule_id: "secrets-env-dotfile",
            category: "secrets",
            reason: "Secret files are excluded.",
          },
          {
            path: "secrets/",
            rule_id: "secrets-directory",
            category: "secrets",
            reason: "Dedicated secrets directories are never crawled or indexed.",
          },
        ],
        bidirectional_index: {
          feature_to_files: {
            "t559u-slice-b-coverage": [
              "crates/spawn-core/src/feature_coverage.rs",
              "crates/spawn-core/src/lib.rs",
            ],
            "t565u-wild-file-scan": ["crates/spawn-core/src/feature_coverage.rs"],
          },
          file_to_features: {
            "crates/spawn-core/src/feature_coverage.rs": [
              "t559u-slice-b-coverage",
              "t565u-wild-file-scan",
            ],
            "crates/spawn-core/src/lib.rs": ["t559u-slice-b-coverage"],
          },
        },
      },
    }),
  );

  assert.equal(accountingIdentitiesHold(data.report), true);
  assert.equal(isScanClean(data.report), false);
  assert.equal(data.report.counts.unassociated_wild, 2);
  assert.equal(data.report.counts.stale_references, 1);
  assert.equal(data.report.counts.explicit_exclusions, 2);
  assert.equal(data.report.counts.multi_feature_files, 1);
  assert.equal(fileAssociatedFeatures(data.report, "secrets/").length, 0);
});

test("multi-slice initiative coverage fixture normalizes domain hierarchy, cross-slice links, and research badge without reading outside web/", () => {
  const payload = {
    ...basePayload({
      report: {
        counts: {
          total_seen: 12,
          in_scope: 10,
          covered: 10,
          unassociated_wild: 0,
          stale_references: 0,
          explicit_exclusions: 2,
          multi_feature_files: 4,
          orphan_features: 0,
          invalid_relations: 0,
        },
        unassociated_files: [],
        excluded_files: [
          {
            path: "target/",
            rule_id: "rust-build-output",
            category: "build-cache",
            reason: "Cargo build outputs are derived.",
          },
          {
            path: ".env",
            rule_id: "secrets-env-dotfile",
            category: "secrets",
            reason: "Secret files are excluded.",
          },
        ],
        bidirectional_index: {
          feature_to_files: {
            "t559u-slice-a-mindmap": [
              "docs/feature-map-mindmap.md",
              "web/src/featuresGraph.ts",
              "web/src/components/features/FeatureMindMap.tsx",
              "web/src/components/features/FeatureInspector.tsx",
              "web/test/featuresGraph.test.ts",
            ],
            "t559u-slice-b-coverage": [
              "crates/spawn-core/src/feature_coverage.rs",
              "docs/feature-map/registry.json",
            ],
            "t559u-slice-c-storage-keys": [
              "crates/spawn-cli/src/backup/recoverable.rs",
              "crates/spawn-cli/tests/recoverable_storage.rs",
              "docs/recoverable-storage-keys.md",
            ],
            "t567u-multi-machine-distributed": [
              "docs/recoverable-storage-keys.md",
              "docs/feature-map/t567u-t568u-distributed-research.md",
            ],
          },
          file_to_features: {
            "docs/feature-map-mindmap.md": [
              "cap-feature-map-status",
              "t559u-feature-map-initiative",
              "t559u-slice-a-mindmap",
            ],
            "web/src/featuresGraph.ts": ["cap-feature-map-status", "t559u-slice-a-mindmap"],
            "web/src/components/features/FeatureMindMap.tsx": ["t559u-slice-a-mindmap"],
            "web/src/components/features/FeatureInspector.tsx": ["t559u-slice-a-mindmap"],
            "web/test/featuresGraph.test.ts": ["t559u-slice-a-mindmap"],
            "crates/spawn-core/src/feature_coverage.rs": [
              "t559u-slice-b-coverage",
              "t565u-wild-file-scan",
            ],
            "docs/feature-map/registry.json": ["t559u-slice-b-coverage"],
            "crates/spawn-cli/src/backup/recoverable.rs": ["t559u-slice-c-storage-keys"],
            "crates/spawn-cli/tests/recoverable_storage.rs": ["t559u-slice-c-storage-keys"],
            "docs/recoverable-storage-keys.md": [
              "t559u-slice-c-storage-keys",
              "t567u-multi-machine-distributed",
            ],
          },
        },
      },
    }),
    features: [
      {
        id: "domain-dashboard-web",
        title: "Observer HTTP API & Web Dashboard",
        kind: "domain",
        parent: null,
        summary: "Organizational root domain.",
        status_note: null,
        file_count: 0,
        relations: [{ kind: "contains", target: "cap-feature-map-status" }],
        provenance: {},
      },
      {
        id: "t559u-feature-map-initiative",
        title: "Full-Project Feature Map Initiative (t559u)",
        kind: "initiative",
        parent: null,
        summary: "Multi-slice initiative.",
        status_note: "Active",
        file_count: 3,
        relations: [{ kind: "contains", target: "t559u-slice-a-mindmap" }],
        provenance: {},
      },
      {
        id: "t559u-slice-a-mindmap",
        title: "Slice A: Interactive Feature Mindmap",
        kind: "slice",
        parent: "t559u-feature-map-initiative",
        summary: "Mindmap graph view.",
        status_note: null,
        file_count: 5,
        relations: [{ kind: "depends_on", target: "cap-feature-map-status" }],
        provenance: {},
      },
      {
        id: "t559u-slice-b-coverage",
        title: "Slice B: File Coverage & Wild-File Scan",
        kind: "slice",
        parent: "t559u-feature-map-initiative",
        summary: "Coverage core & UI.",
        status_note: null,
        file_count: 2,
        relations: [],
        provenance: {},
      },
      {
        id: "t559u-slice-c-storage-keys",
        title: "Slice C: Content-Derived Storage Keys",
        kind: "slice",
        parent: "t559u-feature-map-initiative",
        summary: "Deterministic fm-cas/v1 storage keys.",
        status_note: null,
        file_count: 3,
        relations: [],
        provenance: {},
      },
      {
        id: "t567u-multi-machine-distributed",
        title: "Multi-Machine Distributed Research (t567u/t568u)",
        kind: "research-initiative",
        parent: null,
        summary: "Staged research/design only.",
        status_note: "Not production",
        file_count: 2,
        relations: [{ kind: "relates_to", target: "t559u-feature-map-initiative" }],
        provenance: {},
      },
    ],
  };

  const parsed = parseFeatureCoverageResponse(payload);
  assert.equal(isScanClean(parsed.report), true);
  assert.equal(accountingIdentitiesHold(parsed.report), true);
  assert.equal(parsed.features.find((f) => f.id === "domain-dashboard-web")?.file_count, 0);
  const tracked = initiativesAndSlices(parsed.features);
  assert.equal(tracked.length, 5);
  const research = tracked.find((f) => f.id === "t567u-multi-machine-distributed");
  assert.ok(research);
  assert.equal(isResearchInitiative(research), true);
  assert.equal(research.parent, null);
  assert.deepEqual(fileAssociatedFeatures(parsed.report, "docs/recoverable-storage-keys.md"), [
    "t559u-slice-c-storage-keys",
    "t567u-multi-machine-distributed",
  ]);
});

test("Review A F1 / B F4: wild-file selection starts empty even with suggestions and requires explicit selection", () => {
  const data = normalizeFeatureCoverage(basePayload());
  const finding = data.report.unassociated_files[0];
  assert.ok(finding.suggested_features.length > 0);

  const initial = initialWildFileSelection(finding);
  assert.deepEqual(initial, []);
  assert.equal(canSubmitWildFileAssociation(initial, false), false);

  // Explicit user selection enables submission and supports many-to-many toggles.
  const once = toggleWildFileSelection(initial, finding.suggested_features[0]);
  assert.deepEqual(once, ["t559u-slice-b-coverage"]);
  assert.equal(canSubmitWildFileAssociation(once, false), true);
  assert.equal(canSubmitWildFileAssociation(once, true), false);

  const twice = toggleWildFileSelection(once, "t565u-wild-file-scan");
  assert.deepEqual(twice, ["t559u-slice-b-coverage", "t565u-wild-file-scan"]);
  const cleared = toggleWildFileSelection(
    toggleWildFileSelection(twice, "t559u-slice-b-coverage"),
    "t565u-wild-file-scan",
  );
  assert.deepEqual(cleared, []);
  assert.equal(canSubmitWildFileAssociation(cleared, false), false);

  // Guard WildFileScanPanel.tsx against regressing back to pre-selecting suggested_features.
  const here = path.dirname(fileURLToPath(import.meta.url));
  const panelSource = fs.readFileSync(
    path.resolve(here, "../src/components/features/WildFileScanPanel.tsx"),
    "utf8",
  );
  assert.equal(panelSource.includes("initialWildFileSelection("), true);
  assert.equal(panelSource.includes("suggested_features.slice("), false);
});

test("Review A F4: non-JSON 200 and malformed coverage payloads fail visibly instead of rendering zero drift", async () => {
  const makeError = (status: number, detail: string) =>
    new Error(detail ? `HTTP ${status}: ${detail}` : `HTTP ${status}`);

  // 1. Non-JSON 200 (e.g., HTML proxy error page or index.html fallback) must reject.
  await assert.rejects(
    () =>
      parseCoverageHttpResponse(
        {
          ok: true,
          status: 200,
          json: () => Promise.reject(new SyntaxError("Unexpected token '<'")),
        },
        makeError,
      ),
    /non-JSON body/i,
  );

  // 2. Malformed / empty JSON 200 payloads must reject rather than returning a fake clean scan.
  for (const malformed of [null, {}, { ok: true }, { report: {}, features: [] }]) {
    assert.throws(
      () => parseFeatureCoverageResponse(malformed),
      /Invalid coverage response/i,
    );
    await assert.rejects(
      () =>
        parseCoverageHttpResponse(
          {
            ok: true,
            status: 200,
            json: () => Promise.resolve(malformed),
          },
          makeError,
        ),
      /Invalid coverage response/i,
    );
  }

  // 3. HTTP error responses preserve status and optional JSON error detail.
  await assert.rejects(
    () =>
      parseCoverageHttpResponse(
        {
          ok: false,
          status: 500,
          json: () => Promise.resolve({ error: "registry unreadable" }),
        },
        makeError,
      ),
    /HTTP 500: registry unreadable/,
  );
  await assert.rejects(
    () =>
      parseCoverageHttpResponse(
        {
          ok: false,
          status: 502,
          json: () => Promise.reject(new SyntaxError("bad gateway html")),
        },
        makeError,
      ),
    /HTTP 502/,
  );

  // 4. Valid 200 JSON coverage payload succeeds cleanly.
  const valid = await parseCoverageHttpResponse(
    {
      ok: true,
      status: 200,
      json: () => Promise.resolve(basePayload()),
    },
    makeError,
  );
  assert.equal(valid.report.counts.in_scope, 2);
  assert.equal(valid.report.counts.unassociated_wild, 1);
});

test("read-only deployed registry: payload fields normalize, banner text, and Associate is disabled", () => {
  // A server that predates the fields still accepts associations.
  const legacy = normalizeFeatureCoverage(basePayload());
  assert.equal(legacy.registry_writable, true);
  assert.equal(legacy.read_only_reason, null);
  assert.equal(registryReadOnlyNotice(legacy), null);

  const writable = parseFeatureCoverageResponse(
    { ...basePayload(), registry_writable: true, read_only_reason: null },
  );
  assert.equal(writable.registry_writable, true);
  assert.equal(registryReadOnlyNotice(writable), null);

  const reason =
    "the directory /srv/controller/docs/feature-map holding the feature registry is read-only (permission denied)";
  const readOnly = parseFeatureCoverageResponse(
    { ...basePayload(), registry_writable: false, read_only_reason: reason },
  );
  assert.equal(readOnly.registry_writable, false);
  assert.equal(readOnly.read_only_reason, reason);
  assert.equal(registryReadOnlyNotice(readOnly), reason);
  // A missing or blank reason still yields a clear banner.
  for (const blank of [null, "", "   ", 7]) {
    const data = normalizeFeatureCoverage(
      { ...basePayload(), registry_writable: false, read_only_reason: blank },
    );
    assert.equal(data.registry_writable, false);
    assert.match(registryReadOnlyNotice(data) ?? "", /read-only/);
  }
  // Only an explicit `false` is read-only; a reason without it is ignored.
  const stray = normalizeFeatureCoverage(
    { ...basePayload(), registry_writable: "no", read_only_reason: reason },
  );
  assert.equal(stray.registry_writable, true);
  assert.equal(stray.read_only_reason, null);

  // Associate stays disabled on a read-only registry whatever is selected.
  const selected = ["t559u-slice-b-coverage"];
  assert.equal(canSubmitWildFileAssociation(selected, false, true), true);
  assert.equal(canSubmitWildFileAssociation(selected, false, false), false);
  assert.equal(canSubmitWildFileAssociation(selected), true);

  // The panel wires the banner and the disabled controls to these helpers.
  const here = path.dirname(fileURLToPath(import.meta.url));
  const panelSource = fs.readFileSync(
    path.resolve(here, "../src/components/features/WildFileScanPanel.tsx"),
    "utf8",
  );
  assert.ok(panelSource.includes("registryReadOnlyNotice(data)"));
  assert.ok(panelSource.includes('data-testid="registry-read-only-banner"'));
  assert.ok(
    panelSource.includes("canSubmitWildFileAssociation(selected, saving, registryWritable)"),
  );
  assert.ok(panelSource.includes("disabled={!registryWritable}"));
});

test("read-only deployed registry: the associate route's structured 409 surfaces its clear message", async () => {
  const makeError = (status: number, detail: string) =>
    new Error(detail ? `HTTP ${status}: ${detail}` : `HTTP ${status}`);
  const refusal = {
    error:
      "the feature registry on this deployment is read-only, so the association was not saved: the directory /srv/c holding the feature registry is read-only (permission denied)",
    code: REGISTRY_READ_ONLY_CODE,
    registry_writable: false,
    read_only_reason: "the directory /srv/c holding the feature registry is read-only (permission denied)",
    registry_path: "/srv/c/docs/feature-map/registry.json",
  };
  assert.equal(isRegistryReadOnlyRefusal(409, refusal), true);
  assert.equal(isRegistryReadOnlyRefusal(400, refusal), false);
  assert.equal(isRegistryReadOnlyRefusal(409, { error: "conflict" }), false);
  assert.equal(isRegistryReadOnlyRefusal(409, null), false);

  await assert.rejects(
    () =>
      parseCoverageHttpResponse(
        { ok: false, status: 409, json: () => Promise.resolve(refusal) },
        makeError,
      ),
    (error: Error) => {
      assert.match(error.message, /^HTTP 409: the feature registry on this deployment is read-only/);
      assert.doesNotMatch(error.message, /os error/);
      return true;
    },
  );
  // Without an `error` string the reason (or a default) still explains it.
  await assert.rejects(
    () =>
      parseCoverageHttpResponse(
        {
          ok: false,
          status: 409,
          json: () => Promise.resolve({ code: REGISTRY_READ_ONLY_CODE, registry_writable: false }),
        },
        makeError,
      ),
    /HTTP 409: .*read-only/,
  );
});

test("any project: coverage routes carry ?mission=<id>, the default project stays unparameterized", () => {
  assert.equal(coverageUrl("/api/features/coverage", null), "/api/features/coverage");
  assert.equal(coverageUrl("/api/features/scan", 9), "/api/features/scan?mission=9");
  assert.equal(coverageUrl("/api/features/associate", 4), "/api/features/associate?mission=4");

  // The server lists every eligible mission, however many: 60 options,
  // mission #57 among them, all kept in order.
  const many = normalizeCoverageProjects({
    projects: Array.from({ length: 60 }, (_, index) => ({
      mission: index + 1,
      text: `Mission ${index + 1}`,
      registry_path: `/srv/p${index + 1}/docs/feature-map/registry.json`,
    })),
  });
  assert.equal(many.length, 60);
  assert.equal(many[56].mission, 57);
  assert.equal(many[56].registry, "/srv/p57/docs/feature-map/registry.json");

  const projects = normalizeCoverageProjects({
    projects: [
      { mission: 4, text: "Spawn RSI\nmore", registry_path: "/srv/spawn/docs/feature-map/registry.json" },
      { mission: 9, text: "", registry_path: "/srv/acme/docs/feature-map/registry.json" },
      { mission: 10, text: "x".repeat(60), registry_path: "/srv/long/map.json" },
      // Malformed entries are dropped, never offered.
      { mission: "11", registry_path: "/srv/x.json" },
      { mission: 12, registry_path: "" },
      { mission: 1.5, registry_path: "/srv/y.json" },
      null,
    ],
  });
  assert.deepEqual(
    projects.map((project) => project.mission),
    [4, 9, 10],
  );
  assert.equal(projects[0].label, "Mission #4 — Spawn RSI");
  assert.equal(projects[0].registry, "/srv/spawn/docs/feature-map/registry.json");
  assert.equal(projects[1].label, "Mission #9");
  assert.equal(projects[2].label, `Mission #10 — ${"x".repeat(47)}…`);
  for (const empty of [null, {}, { projects: null }, { projects: [] }]) {
    assert.deepEqual(normalizeCoverageProjects(empty), []);
  }

  // The page and its client pass the selected mission to all three routes.
  const here = path.dirname(fileURLToPath(import.meta.url));
  const api = fs.readFileSync(path.resolve(here, "../src/featureCoverageApi.ts"), "utf8");
  for (const route of ["/api/features/coverage", "/api/features/scan", "/api/features/associate"]) {
    assert.ok(api.includes(`coverageUrl("${route}", mission)`), route);
  }
  const page = fs.readFileSync(
    path.resolve(here, "../src/components/features/CoveragePage.tsx"),
    "utf8",
  );
  assert.ok(page.includes("fetchFeatureCoverage(controller.signal, started.mission)"));
  assert.ok(page.includes("runWildFileScan(undefined, started.mission)"));
  assert.ok(page.includes("associateWildFile(path, featureIds, undefined, started.mission)"));
  assert.ok(page.includes('data-testid="coverage-project-select"'));
  // The picker reads the uncapped project list, not /api/features' missions.
  assert.ok(page.includes("fetchCoverageProjects(controller.signal)"));
  assert.ok(api.includes('"/api/features/coverage/projects"'));
});

test("any project: a late load, scan or association from an earlier project selection is discarded (A -> B -> A)", () => {
  const a1 = nextCoverageScope(INITIAL_COVERAGE_SCOPE, 4);
  assert.deepEqual(a1, { mission: 4, epoch: 1 });
  assert.equal(isCurrentCoverageScope(a1, a1), true);
  const b = nextCoverageScope(a1, 9);
  // A's scan finishing while B is selected must not land.
  assert.equal(isCurrentCoverageScope(a1, b), false);
  const a2 = nextCoverageScope(b, 4);
  // Back on A: the first visit's completion is still stale.
  assert.equal(isCurrentCoverageScope(a1, a2), false);
  assert.equal(isCurrentCoverageScope(b, a2), false);
  assert.equal(isCurrentCoverageScope(a2, a2), true);
  // The default project is a scope of its own.
  const home = nextCoverageScope(a2, null);
  assert.equal(home.mission, null);
  assert.equal(isCurrentCoverageScope(INITIAL_COVERAGE_SCOPE, home), false);

  // Every completion in the page checks its scope before it lands, only the
  // current scope's report is drawn, and the panel remounts per scope.
  const here = path.dirname(fileURLToPath(import.meta.url));
  const page = fs.readFileSync(
    path.resolve(here, "../src/components/features/CoveragePage.tsx"),
    "utf8",
  );
  const guards = page.split("if (!isCurrentCoverageScope(started, scopeRef.current)) return;").length - 1;
  assert.ok(guards >= 6, `each success and failure path is guarded (${guards})`);
  assert.ok(page.includes("shown && isCurrentCoverageScope(shown.scope, scope) ? shown.data : null"));
  assert.ok(page.includes("key={scope.epoch}"));
  assert.equal(page.includes("runWildFileScan(undefined, mission)"), false);
});

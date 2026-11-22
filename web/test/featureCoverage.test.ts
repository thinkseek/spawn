import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  accountingIdentitiesHold,
  consistencyIssueCount,
  featureAssociatedFiles,
  fileAssociatedFeatures,
  formatBytes,
  initiativesAndSlices,
  isResearchInitiative,
  isScanClean,
  kindLabel,
  normalizeFeatureCoverage,
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

test("real docs/feature-map/registry.json satisfies semantic non-catchall invariants", () => {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const registryPath = path.resolve(here, "../../docs/feature-map/registry.json");
  const raw = JSON.parse(fs.readFileSync(registryPath, "utf8")) as {
    schema_version: number;
    initiative_id: string;
    boundary: { exclusions: Array<{ id: string; pattern: string }> };
    features: Array<{
      id: string;
      kind: string;
      parent: string | null;
      files: string[];
    }>;
  };

  assert.equal(raw.schema_version, 1);
  assert.equal(raw.initiative_id, "t559u");
  assert.equal(raw.boundary.exclusions.length, 26);

  const ids = new Set(raw.features.map((f) => f.id));
  for (const catchAll of [
    "domain-workspace-docs",
    "cap-rsi-backlog-specs",
    "cap-rsi-gate-scenarios",
    "cap-web-fleet-home-views",
    "cap-web-missions-rsi-trace-views",
  ]) {
    assert.equal(ids.has(catchAll), false, `catch-all ${catchAll} must not exist`);
  }

  for (const feat of raw.features) {
    if (feat.kind === "domain") {
      assert.equal(feat.files.length, 0, `domain ${feat.id} must not hold stray files`);
    }
  }

  const sliceA = raw.features.find((f) => f.id === "t559u-slice-a-mindmap");
  assert.ok(sliceA);
  for (const expected of [
    "docs/feature-map-mindmap.md",
    "web/src/featuresGraph.ts",
    "web/src/components/features/FeatureMindMap.tsx",
    "web/src/components/features/FeatureInspector.tsx",
    "web/test/featuresGraph.test.ts",
  ]) {
    assert.equal(sliceA.files.includes(expected), true, `sliceA missing ${expected}`);
  }
  assert.equal(sliceA.files.includes("web/src/App.tsx"), false);
  assert.equal(sliceA.files.includes("web/src/components/Sidebar.tsx"), false);

  const sliceC = raw.features.find((f) => f.id === "t559u-slice-c-storage-keys");
  assert.ok(sliceC);
  for (const expected of [
    "crates/spawn-cli/src/backup/recoverable.rs",
    "crates/spawn-cli/tests/recoverable_storage.rs",
    "docs/recoverable-storage-keys.md",
  ]) {
    assert.equal(sliceC.files.includes(expected), true, `sliceC missing ${expected}`);
  }
  assert.equal(sliceC.files.includes("docs/storage.md"), false);

  const t567u = raw.features.find((f) => f.id === "t567u-multi-machine-distributed");
  assert.ok(t567u);
  assert.equal(t567u.kind, "research-initiative");
  assert.equal(t567u.parent, null);
  assert.equal(t567u.files.includes("docs/storage.md"), false);
});

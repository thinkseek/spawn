import assert from "node:assert/strict";
import { test } from "node:test";

import { normalizeFeatureMap } from "../src/features.ts";
import type { Feature, FeatureMission, RelationKind } from "../src/features.ts";
import {
  allBranchIds,
  ancestorsOf,
  buildFeatureMindMapTree,
  buildFileIndex,
  buildRelationIndex,
  defaultExpandedIds,
  getSharedCoOwners,
  layoutMindMap,
  mindMapGraphIdentity,
  reconcileExpandedIds,
  searchMindMap,
  shouldAutoFitOnInspectorToggle,
  shouldAutoFitOnRefresh,
  validateFeatureGraph,
} from "../src/featuresGraph.ts";

function makeMission(features: Array<Record<string, unknown>>, id = 4): FeatureMission {
  return normalizeFeatureMap({
    generated_at_ms: 1000,
    missions: [
      {
        id,
        text: `Mission #${id}: Spawn RSI`,
        features: features.map((f, index) => ({
          id: `f-${index + 1}`,
          title: `Feature ${index + 1}`,
          state: "planned",
          planned: true,
          implemented: false,
          verified: false,
          live: false,
          objectives: [index + 1],
          objective_rows: [],
          sources: [],
          unowned: true,
          gaps: [],
          evidence: { tasks: [], landings: [], verifications: [] },
          ...f,
        })),
      },
    ],
  }).missions[0];
}

test("explicit parent hierarchy constructs a navigable tree with subtree counts", () => {
  const mission = makeMission([
    { id: "platform", title: "Platform", kind: "domain", state: "live", planned: false, implemented: true, verified: true, live: true },
    { id: "serve", title: "Serve API", parent: "platform", kind: "subsystem", state: "verified", planned: false, implemented: true, verified: true },
    { id: "mindmap", title: "Feature Mind Map", parent: "platform", kind: "initiative", state: "implemented", planned: false, implemented: true, relations: [{ kind: "depends_on", target: "serve", label: "reads /api/features" }] },
    { id: "inspector", title: "Inspector", parent: "mindmap", kind: "slice", state: "planned" },
  ]);

  const tree = buildFeatureMindMapTree(mission);
  assert.equal(tree.hasExplicitHierarchy, true);
  assert.equal(tree.effectiveGrouping, "explicit");
  assert.deepEqual(tree.nodes[tree.rootId].children, ["platform"]);
  assert.deepEqual(tree.nodes["platform"].children, ["serve", "mindmap"]);
  assert.deepEqual(tree.nodes["mindmap"].children, ["inspector"]);
  assert.deepEqual(tree.nodes["platform"].stateCounts, {
    planned: 1,
    implemented: 1,
    verified: 1,
    live: 1,
  });
  assert.equal(tree.nodes["platform"].totalFeatures, 4);
  assert.equal(tree.nodes["platform"].descendantCount, 3);
  assert.deepEqual(ancestorsOf(tree, "inspector"), [tree.rootId, "platform", "mindmap"]);
});

test("dense 189-node flat mission clusters into domain/state branches with <=12 range sub-branches", () => {
  const prefixes = [
    "Serve: HTTP route",
    "Web: Dashboard view",
    "Gate: Verification scenario",
    "Mission: Backlog rule",
    "Agents: Session handoff",
    "Store: Postgres query",
  ];
  const states = ["planned", "implemented", "verified", "live"] as const;
  const rawFeatures = Array.from({ length: 189 }, (_, i) => {
    const num = i + 1;
    const state = states[i % states.length];
    return {
      id: `objective-${String(num).padStart(3, "0")}`,
      title: `${prefixes[i % prefixes.length]} #${num}`,
      state,
      planned: state === "planned",
      implemented: state !== "planned",
      verified: state === "verified" || state === "live",
      live: state === "live",
      objectives: [num],
    };
  });

  const mission = makeMission(rawFeatures, 4);
  const tree = buildFeatureMindMapTree(mission);

  assert.equal(tree.hasExplicitHierarchy, false);
  assert.equal(tree.effectiveGrouping, "topic");
  assert.equal(tree.nodes[tree.rootId].totalFeatures, 189);

  // Top-level branches are clean domain clusters (6 topics), never 189 direct children!
  const rootChildren = tree.nodes[tree.rootId].children;
  assert.equal(rootChildren.length, 6);

  // Every cluster with > 12 features is folded into range sub-branches of <= 12 leaves.
  for (const branchId of rootChildren) {
    const branch = tree.nodes[branchId];
    assert.equal(branch.nodeKind, "cluster");
    assert.ok(branch.totalFeatures >= 31, `expected >= 31 items in ${branch.title}`);
    for (const subId of branch.children) {
      const sub = tree.nodes[subId];
      assert.equal(sub.nodeKind, "cluster");
      assert.ok(sub.children.length <= 12, `sub-branch ${sub.title} has <= 12 direct features`);
      assert.match(sub.title, /^Objectives 4\/\d{3}–4\/\d{3}$/);
    }
  }

  // Default expanded set keeps initial canvas calm (root expanded, branches collapsed with +N).
  const initialExpanded = defaultExpandedIds(tree);
  assert.equal(initialExpanded.has(tree.rootId), true);
  assert.equal(initialExpanded.size, 1);

  const layout = layoutMindMap(tree, { expandedIds: initialExpanded });
  // Only root + 6 top-level domain branches visible initially (7 nodes, zero wall of 189!).
  assert.equal(layout.nodes.length, 7);
  for (const node of layout.nodes) {
    if (node.id !== tree.rootId) {
      assert.equal(node.expanded, false);
      assert.ok(node.collapsedCount > 0, "collapsed branch reports descendant count");
    }
  }

  // Grouping by state also works cleanly and folds each state branch into <= 12 sub-branches.
  const stateTree = buildFeatureMindMapTree(mission, { groupingMode: "state" });
  assert.equal(stateTree.effectiveGrouping, "state");
  assert.equal(stateTree.nodes[stateTree.rootId].children.length, 4);
});

test("expand/collapse controls visibility, collapsed counts, and non-overlapping coordinates", () => {
  const mission = makeMission([
    { id: "root-a", title: "Root A" },
    { id: "child-a1", title: "Child A1", parent: "root-a" },
    { id: "child-a2", title: "Child A2", parent: "root-a" },
    { id: "leaf-a2a", title: "Leaf A2A", parent: "child-a2" },
    { id: "root-b", title: "Root B", relations: [{ kind: "blocks", target: "child-a1", label: "must land first" }] },
  ]);

  const tree = buildFeatureMindMapTree(mission);
  const relationIndex = buildRelationIndex(mission.features);

  // Collapsed to root only: root + root-a + root-b visible; relation to collapsed child-a1 is counted hidden.
  const collapsed = layoutMindMap(tree, {
    expandedIds: new Set([tree.rootId]),
    relationIndex,
  });
  assert.equal(collapsed.nodes.length, 3);
  assert.equal(collapsed.nodeMap["root-a"].collapsedCount, 3);
  assert.equal(collapsed.relationEdges.length, 0);
  assert.equal(collapsed.hiddenRelationCount, 1);

  // Expanded all: every node visible, relation edge rendered, no vertical overlap between siblings.
  const expanded = layoutMindMap(tree, {
    expandedIds: allBranchIds(tree),
    relationIndex,
  });
  assert.equal(expanded.nodes.length, 6);
  assert.equal(expanded.hierarchyEdges.length, 5);
  assert.equal(expanded.relationEdges.length, 1);
  assert.equal(expanded.relationEdges[0].kind, "blocks");
  assert.equal(expanded.relationEdges[0].label, "must land first");
  assert.ok(expanded.nodeMap["child-a1"].x > expanded.nodeMap["root-a"].x);
  assert.ok(
    expanded.nodeMap["child-a2"].y >=
      expanded.nodeMap["child-a1"].y + expanded.nodeMap["child-a1"].height,
    "siblings never overlap vertically",
  );

  // Relation kind filter hides edges whose kind is toggled off.
  const filtered = layoutMindMap(tree, {
    expandedIds: allBranchIds(tree),
    relationIndex,
    activeRelationKinds: new Set<RelationKind>(["depends_on"]),
  });
  assert.equal(filtered.relationEdges.length, 0);
});

test("search matching finds by title, tag, file, and objective and auto-expands ancestors", () => {
  const mission = makeMission([
    { id: "domain-a", title: "Storage" },
    {
      id: "pg-pool",
      title: "Postgres Pool",
      parent: "domain-a",
      tags: ["sql", "perf"],
      files: ["crates/spawn-postgres/src/pool.rs"],
      objectives: [488],
    },
    { id: "domain-b", title: "Frontend" },
  ]);

  const tree = buildFeatureMindMapTree(mission);
  const byFile = searchMindMap(tree, "pool.rs");
  assert.equal(byFile.matchedIds.has("pg-pool"), true);
  assert.equal(byFile.ancestorIds.has("domain-a"), true);
  assert.equal(byFile.ancestorIds.has(tree.rootId), true);

  const byObjective = searchMindMap(tree, "488");
  assert.equal(byObjective.matchedIds.has("pg-pool"), true);

  // Even if domain-a is collapsed in expandedIds, search auto-expands it so pg-pool is visible!
  const layout = layoutMindMap(tree, {
    expandedIds: new Set([tree.rootId]),
    searchQuery: "pool.rs",
  });
  assert.ok(layout.nodeMap["pg-pool"], "matched node is auto-expanded into view");
  assert.equal(layout.nodeMap["pg-pool"].isMatch, true);
  assert.equal(layout.nodeMap["domain-b"].isDimmed, true);
});

test("bidirectional file index surfaces multi-feature shared files and co-owners", () => {
  const mission = makeMission([
    { id: "feat-a", title: "Feature A", files: ["web/src/features.ts", "web/src/a.ts"] },
    { id: "feat-b", title: "Feature B", files: ["web/src/features.ts", "web/src/b.ts"] },
    { id: "feat-c", title: "Feature C", files: ["web/src/features.ts"] },
  ]);

  const fileIndex = buildFileIndex(mission.features);
  assert.deepEqual(fileIndex.allFiles, [
    "web/src/a.ts",
    "web/src/b.ts",
    "web/src/features.ts",
  ]);
  assert.deepEqual(Object.keys(fileIndex.sharedFiles), ["web/src/features.ts"]);
  assert.deepEqual(
    fileIndex.sharedFiles["web/src/features.ts"].map((f) => f.id),
    ["feat-a", "feat-b", "feat-c"],
  );
  assert.deepEqual(
    getSharedCoOwners(fileIndex, "web/src/features.ts", "feat-b").map((f) => f.id),
    ["feat-a", "feat-c"],
  );
  assert.deepEqual(getSharedCoOwners(fileIndex, "web/src/a.ts", "feat-a"), []);
});

test("validateFeatureGraph detects self-links, dangling targets, and hierarchy/dependency cycles", () => {
  const validMission = makeMission([
    { id: "a", title: "A" },
    { id: "b", title: "B", parent: "a", relations: [{ kind: "depends_on", target: "a" }] },
  ]);
  assert.deepEqual(validateFeatureGraph(validMission.features), []);

  const broken = makeMission([
    {
      id: "self",
      title: "Self",
      parent: "self",
      relations: [{ kind: "blocks", target: "self" }],
    },
    {
      id: "dangling",
      title: "Dangling",
      parent: "missing-parent",
      relations: [{ kind: "depends_on", target: "missing-target" }],
    },
    {
      id: "p1",
      title: "P1",
      parent: "p2",
      relations: [{ kind: "depends_on", target: "p2" }],
    },
    {
      id: "p2",
      title: "P2",
      parent: "p1",
      relations: [{ kind: "depends_on", target: "p1" }],
    },
    {
      id: "c1",
      title: "C1",
      relations: [{ kind: "contains", target: "c2" }],
    },
    {
      id: "c2",
      title: "C2",
      relations: [{ kind: "contains", target: "c1" }],
    },
  ]);

  const issues = validateFeatureGraph(broken.features);
  const kinds = new Set(issues.map((i) => i.kind));
  assert.ok(kinds.has("self_link"), "detects self-link");
  assert.ok(kinds.has("dangling_parent"), "detects dangling parent");
  assert.ok(kinds.has("dangling_relation"), "detects dangling relation target");
  assert.ok(kinds.has("parent_cycle"), "detects parent cycle");
  assert.ok(kinds.has("depends_on_cycle"), "detects depends_on cycle");
  assert.ok(kinds.has("contains_cycle"), "detects contains cycle");

  // Even on cyclic input, tree construction terminates cleanly without hanging.
  const tree = buildFeatureMindMapTree(broken);
  assert.equal(tree.nodes[tree.rootId].totalFeatures, broken.features.length);
  // Ensure Feature type check stays clean
  const sample: Feature | undefined = broken.features[0];
  assert.equal(sample?.id, "self");
});

test("mindMapGraphIdentity and reconcileExpandedIds preserve user expansion and zoom across poll refreshes (Review A F2)", () => {
  const initialMission = makeMission(
    [
      { id: "domain-a", title: "Domain A", kind: "domain", state: "implemented", planned: false, implemented: true },
      { id: "feat-1", title: "Feature 1", parent: "domain-a", state: "implemented", planned: false, implemented: true },
      { id: "feat-2", title: "Feature 2", parent: "domain-a", state: "planned" },
      { id: "domain-b", title: "Domain B", kind: "domain", state: "planned" },
    ],
    4,
  );
  const tree1 = buildFeatureMindMapTree(initialMission);
  const id1 = mindMapGraphIdentity(initialMission, tree1);

  // User expands domain-a manually and zooms in.
  const userExpanded = new Set<string>([tree1.rootId, "domain-a"]);

  // Live 4s poll returns a brand-new mission object where feat-1 moved to live and feat-3 was added.
  const refreshedMission = makeMission(
    [
      { id: "domain-a", title: "Domain A", kind: "domain", state: "live", planned: false, implemented: true, verified: true, live: true },
      { id: "feat-1", title: "Feature 1", parent: "domain-a", state: "live", planned: false, implemented: true, verified: true, live: true },
      { id: "feat-2", title: "Feature 2", parent: "domain-a", state: "verified", planned: false, implemented: true, verified: true },
      { id: "feat-3", title: "Feature 3", parent: "domain-a", state: "planned" },
      { id: "domain-b", title: "Domain B", kind: "domain", state: "planned" },
    ],
    4,
  );
  const tree2 = buildFeatureMindMapTree(refreshedMission);
  const id2 = mindMapGraphIdentity(refreshedMission, tree2);

  assert.equal(id2, id1, "poll data refresh on same mission/grouping keeps identical graph identity");
  assert.equal(shouldAutoFitOnRefresh(id1, id2), false, "poll refresh must NOT auto-fit or reset zoom/pan");

  const reconciled = reconcileExpandedIds(userExpanded, tree2, id1, id2);
  assert.equal(reconciled, userExpanded, "same-graph refresh with all expanded IDs intact returns the exact same Set");
  assert.equal(reconciled.has("domain-a"), true);

  // If a previously expanded branch disappears on refresh, drop only the removed ID and keep surviving ones.
  const prunedMission = makeMission(
    [
      { id: "domain-a", title: "Domain A", kind: "domain" },
      { id: "feat-1", title: "Feature 1", parent: "domain-a" },
    ],
    4,
  );
  const tree3 = buildFeatureMindMapTree(prunedMission);
  const id3 = mindMapGraphIdentity(prunedMission, tree3);
  const withDeleted = new Set<string>([tree1.rootId, "domain-a", "domain-b"]);
  const reconciledAfterDelete = reconcileExpandedIds(withDeleted, tree3, id1, id3);
  assert.equal(reconciledAfterDelete.has("domain-a"), true);
  assert.equal(reconciledAfterDelete.has("domain-b"), false);

  // Switching to a different mission OR changing grouping mode changes graph identity and resets cleanly.
  const otherMission = makeMission([{ id: "m6-a", title: "Other" }], 6);
  const otherTree = buildFeatureMindMapTree(otherMission);
  const otherId = mindMapGraphIdentity(otherMission, otherTree);
  assert.notEqual(otherId, id1);
  assert.equal(shouldAutoFitOnRefresh(id1, otherId), true);
  assert.deepEqual(
    reconcileExpandedIds(userExpanded, otherTree, id1, otherId),
    defaultExpandedIds(otherTree),
  );

  // Changing flat grouping mode ("auto" -> "state") also changes graph identity.
  const flatMission = makeMission(
    [
      { id: "obj-1", title: "Serve: one", state: "live", planned: false, implemented: true, verified: true, live: true },
      { id: "obj-2", title: "Web: two", state: "planned" },
    ],
    4,
  );
  const topicTree = buildFeatureMindMapTree(flatMission, { groupingMode: "auto" });
  const stateTree = buildFeatureMindMapTree(flatMission, { groupingMode: "state" });
  assert.notEqual(
    mindMapGraphIdentity(flatMission, topicTree),
    mindMapGraphIdentity(flatMission, stateTree),
  );

  // Inspector open/close auto-fits only before the user manually pans or zooms.
  assert.equal(shouldAutoFitOnInspectorToggle(false), true);
  assert.equal(shouldAutoFitOnInspectorToggle(true), false);
});

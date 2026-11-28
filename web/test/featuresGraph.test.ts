import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { test } from "node:test";

import { normalizeFeatureMap } from "../src/features.ts";
import type { Feature, FeatureMission, RelationKind } from "../src/features.ts";
import {
  allBranchIds,
  ancestorsOf,
  buildActiveSpineModel,
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

test("buildActiveSpineModel handles empty and all-live missions and transitions cleanly on focus", () => {
  const emptyMission = makeMission([], 4);
  const emptyRel = buildRelationIndex(emptyMission.features);
  const emptyModel = buildActiveSpineModel(emptyMission, emptyRel, null);
  assert.equal(emptyModel.kind, "empty");
  assert.equal(emptyModel.focusId, null);
  assert.deepEqual(emptyModel.spine, []);
  assert.deepEqual(emptyModel.upstream, []);
  assert.deepEqual(emptyModel.downstream, []);
  assert.equal(emptyModel.summary.totalFeatures, 0);

  const allLive = makeMission([
    {
      id: "dom",
      title: "Domain",
      kind: "domain",
      state: "live",
      planned: false,
      implemented: true,
      verified: true,
      live: true,
    },
    {
      id: "core",
      title: "Core",
      state: "live",
      planned: false,
      implemented: true,
      verified: true,
      live: true,
    },
    {
      id: "edge",
      title: "Edge",
      state: "live",
      planned: false,
      implemented: true,
      verified: true,
      live: true,
      relations: [{ kind: "depends_on", target: "core" }],
    },
  ]);
  const liveRel = buildRelationIndex(allLive.features);
  const liveModel = buildActiveSpineModel(allLive, liveRel, null);
  assert.equal(liveModel.kind, "all_live");
  assert.equal(liveModel.focusId, null);
  assert.deepEqual(
    liveModel.spine.map((n) => n.feature.id),
    ["core", "edge"],
  );
  assert.equal(liveModel.summary.liveCount, 3);
  assert.equal(liveModel.summary.nonLiveCount, 0);

  // Focusing an individual node switches cleanly to focused mode with upstream/downstream.
  const focused = buildActiveSpineModel(allLive, liveRel, "edge");
  assert.equal(focused.kind, "focused");
  assert.equal(focused.focusId, "edge");
  assert.deepEqual(
    focused.spine.map((n) => n.feature.id),
    ["edge"],
  );
  assert.deepEqual(
    focused.upstream.map((u) => [u.feature.id, u.depth, u.proximity]),
    [["core", 1, "direct"]],
  );
  assert.deepEqual(focused.downstream, []);

  // Unknown focusId falls back to auto mode rather than returning an invalid focused mode.
  const unknownFocus = buildActiveSpineModel(allLive, liveRel, "does-not-exist");
  assert.equal(unknownFocus.kind, "all_live");
  assert.equal(unknownFocus.focusId, null);
});

test("buildActiveSpineModel extracts transitive upstream/downstream and preserves server state immutability", () => {
  // Graph:
  //   root-db (live) <- depends_on - auth (implemented, active owner)
  //   gate (planned) - blocks -> auth
  //   auth <- depends_on - api (planned) <- depends_on - ui (planned)
  //   blocked-hotfix (planned, blocked) - blocks -> ui
  const rawList = [
    {
      id: "root-db",
      title: "Root DB",
      state: "live",
      planned: false,
      implemented: true,
      verified: true,
      live: true,
      objectives: [1],
    },
    {
      id: "gate",
      title: "Security Gate",
      state: "planned",
      objectives: [2],
      relations: [{ kind: "blocks", target: "auth", label: "needs audit" }],
    },
    {
      id: "auth",
      title: "Auth Service",
      state: "implemented",
      planned: false,
      implemented: true,
      verified: false,
      live: false,
      objectives: [3],
      owner: {
        task: 42,
        agent_id: "worker-alpha",
        session_id: "sess-auth-42",
        kind: "worker",
        state: "working",
        since_ms: 5000,
      },
      relations: [{ kind: "depends_on", target: "root-db", label: "schema" }],
    },
    {
      id: "api",
      title: "HTTP API",
      state: "planned",
      objectives: [4],
      relations: [{ kind: "depends_on", target: "auth" }],
    },
    {
      id: "ui",
      title: "Web UI",
      state: "planned",
      objectives: [5],
      relations: [{ kind: "depends_on", target: "api" }],
    },
    {
      id: "blocked-hotfix",
      title: "Hotfix Blocker",
      state: "planned",
      objectives: [6],
      blocked: {
        kind: "main_red",
        reason: "clippy red",
        task: 55,
        agent_id: "worker-beta",
        session_id: "sess-hotfix-55",
      },
      relations: [{ kind: "blocks", target: "ui" }],
    },
  ];

  const mission = makeMission(rawList);
  const rel = buildRelationIndex(mission.features);
  const model = buildActiveSpineModel(mission, rel, null);

  assert.equal(model.kind, "active_work");
  // Blocked hotfix ranks ahead of active owner auth; domain/idle planned nodes stay out of spine.
  assert.deepEqual(
    model.spine.map((n) => [n.feature.id, n.role, n.workSignal.kind, n.sessionId]),
    [
      ["blocked-hotfix", "in_flight", "blocked", "sess-hotfix-55"],
      ["auth", "in_flight", "active_owner", "sess-auth-42"],
    ],
  );
  // Upstream for spine includes incoming blocks (gate, planned priority) and direct depends_on (root-db, live).
  assert.deepEqual(
    model.upstream.map((u) => [
      u.feature.id,
      u.depth,
      u.proximity,
      u.links.map((l) => `${l.edgeKind}:${l.viaFeatureId}`),
    ]),
    [
      ["gate", 1, "direct", ["blocked_by:auth"]],
      ["root-db", 1, "direct", ["depends_on:auth"]],
    ],
  );
  // Downstream unlocked includes direct api (required_by auth, objective 4) and ui (blocks blocked-hotfix, objective 5; kept at min depth 1).
  assert.deepEqual(
    model.downstream.map((d) => [d.feature.id, d.depth, d.proximity]),
    [
      ["api", 1, "direct"],
      ["ui", 1, "direct"],
    ],
  );

  // Shuffling input feature array must produce identical deterministic order.
  const shuffled = makeMission([...rawList].reverse());
  const shuffledModel = buildActiveSpineModel(shuffled, buildRelationIndex(shuffled.features), null);
  assert.deepEqual(
    shuffledModel.spine.map((n) => n.feature.id),
    model.spine.map((n) => n.feature.id),
  );
  assert.deepEqual(
    shuffledModel.upstream.map((n) => n.feature.id),
    model.upstream.map((n) => n.feature.id),
  );
  assert.deepEqual(
    shuffledModel.downstream.map((n) => n.feature.id),
    model.downstream.map((n) => n.feature.id),
  );

  // Server states are never overwritten even when upstream dependencies are incomplete.
  const authFeature = mission.features.find((f) => f.id === "auth");
  assert.equal(authFeature?.state, "implemented");
  assert.equal(authFeature?.implemented, true);
  assert.equal(authFeature?.verified, false);
  assert.equal(authFeature?.live, false);
});

test("buildActiveSpineModel guards against self-links, 2-node cycles, and transitive sub-cycles", () => {
  const mission = makeMission([
    {
      id: "a",
      title: "Node A",
      state: "implemented",
      planned: false,
      implemented: true,
      relations: [
        { kind: "depends_on", target: "a" },
        { kind: "depends_on", target: "b" },
      ],
    },
    {
      id: "b",
      title: "Node B",
      state: "planned",
      relations: [
        { kind: "depends_on", target: "c" },
        { kind: "depends_on", target: "a" },
      ],
    },
    {
      id: "c",
      title: "Node C",
      state: "planned",
      relations: [{ kind: "depends_on", target: "b" }],
    },
  ]);

  const rel = buildRelationIndex(mission.features);
  const model = buildActiveSpineModel(mission, rel, "a");

  assert.equal(model.kind, "focused");
  assert.deepEqual(
    model.upstream.map((u) => [u.feature.id, u.depth]),
    [
      ["b", 1],
      ["c", 2],
    ],
  );
  assert.deepEqual(
    model.downstream.map((d) => [d.feature.id, d.depth]),
    [
      ["b", 1],
      ["c", 2],
    ],
  );
  assert.ok(model.summary.cycleGuardedEdges > 0);
});

test("Fix 3: diamond DAGs report 0 cycle warnings while real cycles still register", () => {
  // Acyclic diamond DAG: A -> B, A -> C, C -> B
  const diamondMission = makeMission([
    {
      id: "a",
      title: "Focus A",
      state: "implemented",
      planned: false,
      implemented: true,
      relations: [
        { kind: "depends_on", target: "b" },
        { kind: "depends_on", target: "c" },
      ],
    },
    {
      id: "c",
      title: "Middle C",
      state: "planned",
      relations: [{ kind: "depends_on", target: "b" }],
    },
    {
      id: "b",
      title: "Base B",
      state: "live",
      planned: false,
      implemented: true,
      verified: true,
      live: true,
    },
  ]);

  assert.deepEqual(validateFeatureGraph(diamondMission.features), []);
  const diamondRel = buildRelationIndex(diamondMission.features);

  const focusA = buildActiveSpineModel(diamondMission, diamondRel, "a");
  assert.equal(focusA.kind, "focused");
  assert.equal(focusA.summary.cycleGuardedEdges, 0);
  assert.deepEqual(
    focusA.upstream.map((u) => [u.feature.id, u.depth]),
    [
      ["c", 1],
      ["b", 1],
    ],
  );

  // Downstream diamond from B (C depends_on B, A depends_on B & C) must also report 0 cycles.
  const focusB = buildActiveSpineModel(diamondMission, diamondRel, "b");
  assert.equal(focusB.kind, "focused");
  assert.equal(focusB.summary.cycleGuardedEdges, 0);
  assert.deepEqual(
    focusB.downstream.map((d) => [d.feature.id, d.depth]),
    [
      ["a", 1],
      ["c", 1],
    ],
  );
});

test("Fix 4, 5 & 6: task-scoped session binding never borrows across tasks, isFocused is removed, and cards avoid nested role=button", () => {
  const mission = makeMission([
    {
      id: "owner-no-session",
      title: "Active Task 42 Without Session",
      state: "implemented",
      planned: false,
      implemented: true,
      owner: {
        task: 42,
        agent_id: "worker-42",
        session_id: null,
        kind: "worker",
        state: "working",
        since_ms: 2000,
      },
      last_owner: {
        task: 17,
        agent_id: "worker-17",
        session_id: "sess-17",
        kind: "worker",
        state: "done",
        since_ms: 1000,
      },
      evidence: {
        tasks: [
          { id: 17, state: "done", agent_id: "worker-17", session_id: "sess-17", updated_at_ms: 1000 },
          { id: 42, state: "working", agent_id: "worker-42", session_id: null, updated_at_ms: 2000 },
        ],
        landings: [],
        verifications: [],
      },
    },
    {
      id: "blocked-no-session",
      title: "Blocked Task 43 Without Session",
      state: "planned",
      blocked: {
        kind: "sent_back",
        reason: "needs fix",
        task: 43,
        agent_id: "worker-43",
        session_id: null,
      },
      evidence: {
        tasks: [
          { id: 19, state: "done", agent_id: "worker-19", session_id: "sess-19", updated_at_ms: 900 },
          { id: 43, state: "sent_back", agent_id: "worker-43", session_id: null, updated_at_ms: 2100 },
        ],
        landings: [],
        verifications: [],
      },
    },
    {
      id: "owner-from-same-task",
      title: "Task 44 Carries Matching Task Session",
      state: "verified",
      planned: false,
      implemented: true,
      verified: true,
      owner: {
        task: 44,
        agent_id: "worker-44",
        session_id: null,
        kind: "worker",
        state: "working",
        since_ms: 2200,
      },
      evidence: {
        tasks: [
          { id: 12, state: "done", agent_id: "worker-12", session_id: "sess-12", updated_at_ms: 800 },
          { id: 44, state: "working", agent_id: "worker-44", session_id: "sess-44", updated_at_ms: 2200 },
        ],
        landings: [],
        verifications: [],
      },
    },
  ]);

  const rel = buildRelationIndex(mission.features);
  const activeModel = buildActiveSpineModel(mission, rel, null);
  const byFeature = new Map(activeModel.spine.map((n) => [n.feature.id, n]));

  const node42 = byFeature.get("owner-no-session")!;
  assert.equal(node42.sessionId, null);
  assert.equal(node42.workSignal.kind, "active_owner");
  if (node42.workSignal.kind === "active_owner") {
    assert.equal(node42.workSignal.sessionId, null);
  }
  assert.equal("isFocused" in node42, false);

  const node43 = byFeature.get("blocked-no-session")!;
  assert.equal(node43.sessionId, null);

  const node44 = byFeature.get("owner-from-same-task")!;
  assert.equal(node44.sessionId, "sess-44");

  const focused42 = buildActiveSpineModel(mission, rel, "owner-no-session");
  assert.equal(focused42.spine[0].role, "focused");
  assert.equal("isFocused" in focused42.spine[0], false);

  // Guard ActiveSpineView against nested interactive elements (`role="button"`).
  const activeSpineSrc = readFileSync(
    new URL("../src/components/features/ActiveSpineView.tsx", import.meta.url),
    "utf8",
  );
  assert.doesNotMatch(activeSpineSrc, /role="button"/);
});

test("Blocker 2: Active Spine outer scroll, 3-column grid, and inspector align on xl (1280px) across built CSS viewports", () => {
  const mapViewSrc = readFileSync(
    new URL("../src/components/features/FeatureMapView.tsx", import.meta.url),
    "utf8",
  );
  const spineViewSrc = readFileSync(
    new URL("../src/components/features/ActiveSpineView.tsx", import.meta.url),
    "utf8",
  );
  const inspectorSrc = readFileSync(
    new URL("../src/components/features/FeatureInspector.tsx", import.meta.url),
    "utf8",
  );

  // Align all Spine view transitions cleanly at xl (zero md/lg dead zones).
  assert.match(
    mapViewSrc,
    /data-spine-scroll-shell="true"[\s\S]*overflow-y-auto[\s\S]*xl:overflow-y-hidden/,
  );
  assert.match(
    mapViewSrc,
    /flex shrink-0 flex-col gap-3 xl:min-h-0 xl:flex-1 xl:shrink xl:flex-row/,
  );
  assert.match(mapViewSrc, /desktopBreakpoint="xl"/);
  assert.match(
    mapViewSrc,
    /desktopBreakpoint === "xl"\s*\?\s*"shrink-0 xl:min-h-0 xl:max-h-none xl:shrink-0"/,
  );
  assert.doesNotMatch(
    mapViewSrc,
    /desktopBreakpoint === "xl"\s*\?\s*"shrink-0 xl:min-h-0 xl:max-h-none xl:shrink"/,
  );
  assert.match(spineViewSrc, /flex flex-col gap-3 xl:min-h-0 xl:min-w-0 xl:flex-1/);
  assert.match(
    spineViewSrc,
    /grid grid-cols-1 gap-3 xl:min-h-0 xl:flex-1 xl:grid-cols-3 xl:grid-rows-\[minmax\(0,1fr\)\]/,
  );
  assert.match(
    spineViewSrc,
    /flex min-h-\[240px\] min-w-0 flex-col rounded-card bg-surface shadow-card xl:min-h-0 xl:overflow-hidden/,
  );
  assert.match(
    spineViewSrc,
    /flex flex-col gap-2 p-2\.5 xl:min-h-0 xl:flex-1 xl:overflow-y-auto/,
  );
  assert.match(inspectorSrc, /wideAt === "xl" \? "xl:w-\[420px\] xl:shrink-0"/);

  // Parse the actual compiled CSS bundle when `web/dist/assets/index-*.css` exists
  // (mandatory after `vite build` so responsive breakpoints & declarations are proven).
  const assetsUrl = new URL("../dist/assets/", import.meta.url);
  let cssFile: string | null = null;
  try {
    const names = readdirSync(assetsUrl);
    cssFile = names.find((name) => /^index-.*\.css$/.test(name)) ?? null;
  } catch {
    cssFile = null;
  }
  if (!cssFile) return;

  const css = readFileSync(new URL(cssFile, assetsUrl), "utf8");
  const escapedClassToken = (cls: string) =>
    cls.replace(/([:\\/[\],()])/g, "\\$1");

  const requiredBase = [
    "overflow-y-auto",
    "flex-col",
    "shrink-0",
    "grid-cols-1",
  ];
  for (const cls of requiredBase) {
    assert.ok(
      css.includes(`.${escapedClassToken(cls)}{`),
      `built CSS missing base rule for .${cls}`,
    );
  }

  // Extract all `@media (width>=80rem)` (`1280px` xl breakpoint) balanced blocks.
  const xlBlocks: string[] = [];
  for (const match of css.matchAll(/@media\s*\(\s*width\s*>=\s*80rem\s*\)\s*\{/g)) {
    let depth = 1;
    let cursor = (match.index ?? 0) + match[0].length;
    const blockStart = cursor;
    while (cursor < css.length && depth > 0) {
      if (css[cursor] === "{") depth += 1;
      else if (css[cursor] === "}") depth -= 1;
      cursor += 1;
    }
    xlBlocks.push(css.slice(blockStart, cursor - 1));
  }
  assert.ok(xlBlocks.length > 0, "built CSS must emit @media (width>=80rem)");
  const xlBlock = xlBlocks.join("\n");

  const expectedXlRules: Array<[string, RegExp]> = [
    ["xl:overflow-y-hidden", /overflow-y:\s*hidden/],
    ["xl:flex-row", /flex-direction:\s*row/],
    ["xl:min-h-0", /min-height:\s*0/],
    ["xl:min-w-0", /min-width:\s*0/],
    ["xl:flex-1", /flex:\s*1/],
    ["xl:shrink", /flex-shrink:\s*1/],
    ["xl:grid-cols-3", /grid-template-columns:\s*repeat\(3/],
    ["xl:grid-rows-[minmax(0,1fr)]", /grid-template-rows:\s*minmax\(0,\s*1fr\)/],
    ["xl:overflow-hidden", /overflow:\s*hidden/],
    ["xl:overflow-y-auto", /overflow-y:\s*auto/],
    ["xl:w-[420px]", /width:\s*420px/],
    ["xl:shrink-0", /flex-shrink:\s*0/],
  ];
  for (const [cls, propRe] of expectedXlRules) {
    const token = `.${escapedClassToken(cls)}{`;
    const idx = xlBlock.indexOf(token);
    assert.ok(idx >= 0, `built CSS missing ${token} inside @media (width>=80rem)`);
    const bodyEnd = xlBlock.indexOf("}", idx);
    const decl = xlBlock.slice(idx + token.length, bodyEnd);
    assert.match(decl, propRe, `built rule for .${cls} must declare ${propRe}, got ${decl}`);
  }

  // Verify layout cascade metrics across narrow (390..1279), exact 1280x613 xl boundary, and 1440px.
  const evaluateCascadeAtWidth = (width: number) => {
    const isXl = width >= 1280;
    const horizontalPaddingPx = width >= 640 ? 40 : 32; // px-4 sm:px-5
    const rowGapPx = 12; // gap-3
    const drawerWidthPx = isXl ? 420 : null;
    return {
      outerOverflowY: isXl ? "hidden" : "auto",
      innerDirection: isXl ? "row" : "column",
      innerShrink: isXl ? 1 : 0,
      drawerShrink: 0, // base `shrink-0` + `xl:shrink-0` on InspectorDrawer + `xl:shrink-0` on FeatureInspector
      drawerWidthPx,
      spineMinWidth: isXl ? 0 : "auto", // `xl:min-w-0` lets 3-column spine yield width to the 420px drawer
      spineAvailableWidthPx: isXl
        ? width - horizontalPaddingPx - rowGapPx - (drawerWidthPx ?? 0)
        : width - horizontalPaddingPx,
      gridColumns: isXl ? 3 : 1,
      columnOverflow: isXl ? "hidden" : "visible",
      columnListOverflowY: isXl ? "auto" : "visible",
    };
  };

  for (const width of [390, 500, 800, 1024, 1279]) {
    const layout = evaluateCascadeAtWidth(width);
    assert.equal(layout.outerOverflowY, "auto", `width ${width}px must enable outer vertical scrolling`);
    assert.equal(layout.innerDirection, "column");
    assert.equal(layout.innerShrink, 0, `width ${width}px stacked shell must not compress away`);
    assert.equal(layout.gridColumns, 1);
    assert.equal(layout.columnOverflow, "visible");
  }

  for (const width of [1280, 1440]) {
    const desktop = evaluateCascadeAtWidth(width);
    assert.equal(desktop.outerOverflowY, "hidden");
    assert.equal(desktop.innerDirection, "row");
    assert.equal(desktop.drawerShrink, 0, `width ${width}px (e.g. 1280x613) must keep drawer shrink-0`);
    assert.equal(desktop.drawerWidthPx, 420, `width ${width}px must preserve full 420px FeatureInspector drawer`);
    assert.equal(desktop.spineMinWidth, 0, `width ${width}px spine container must declare xl:min-w-0`);
    assert.ok(desktop.spineAvailableWidthPx > 0);
    assert.equal(desktop.gridColumns, 3);
    assert.equal(desktop.columnOverflow, "hidden");
    assert.equal(desktop.columnListOverflowY, "auto");
  }
  assert.equal(evaluateCascadeAtWidth(1280).spineAvailableWidthPx, 808);
});

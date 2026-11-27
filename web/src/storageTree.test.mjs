import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildGraphLayout,
  collectLineageIds,
  fallbackRecoveryCommands,
  historyActionMeta,
  normalizeStorageSnapshot,
  normalizeStorageTree,
  refKindMeta,
  shortOid,
} from "./storageTree.ts";

test("normalizeStorageTree handles null, malformed, git mode, and D6 DAG payloads", () => {
  const empty = normalizeStorageTree(null);
  assert.deepEqual(empty.projects, []);
  assert.equal(empty.active_project, null);
  assert.equal(empty.summary.backend, "git");
  assert.deepEqual(empty.nodes, []);
  assert.deepEqual(empty.refs, []);
  assert.deepEqual(empty.history, []);

  const valid = normalizeStorageTree({
    projects: ["proj-alpha", "proj-beta"],
    active_project: "proj-alpha",
    summary: {
      total_snapshots: 2,
      live_snapshots: 2,
      collected_snapshots: 0,
      active_refs: 2,
      history_entries: 1,
      truncated: false,
      backend: "d6",
      mode_note: "D6 active",
    },
    nodes: [
      {
        snapshot_id: "sha256:" + "a".repeat(64),
        parent_ids: ["sha256:" + "0".repeat(64)],
        tree_oid: "sha256:" + "b".repeat(64),
        created_at_ms: 2000,
        git_oid: "1".repeat(40),
        state: "live",
        is_merge: false,
        refs: [{ name: "main", kind: "main", ref_kind: "main", updated_at_ms: 2000 }],
      },
      null,
      { git_oid: "missing-id" },
    ],
    refs: [
      {
        name: "main",
        kind: "main",
        ref_kind: "main",
        snapshot_id: "sha256:" + "a".repeat(64),
        updated_at_ms: 2000,
      },
    ],
    history: [
      {
        seq: 1,
        project: "proj-alpha",
        name: "main",
        kind: "main",
        ref_kind: "main",
        old_snapshot: null,
        new_snapshot: "sha256:" + "a".repeat(64),
        reason: "bootstrap main",
        action: "created",
        at_ms: 2000,
      },
    ],
  });

  assert.equal(valid.projects.length, 2);
  assert.equal(valid.nodes.length, 1);
  assert.equal(valid.summary.backend, "d6");
  assert.equal(valid.refs[0].name, "main");
  assert.equal(valid.history[0].action, "created");
});

test("normalizeStorageSnapshot parses ancestors, children, history, and recovery commands", () => {
  assert.equal(normalizeStorageSnapshot({}), null);
  const s1 = "sha256:" + "1".repeat(64);
  const s0 = "sha256:" + "0".repeat(64);
  const detail = normalizeStorageSnapshot({
    snapshot: {
      snapshot_id: s1,
      project: "proj-alpha",
      parent_ids: [s0],
      tree_oid: "sha256:" + "c".repeat(64),
      git_oid: "2".repeat(40),
      state: "live",
      created_at_ms: 5000,
      is_merge: false,
    },
    refs: [{ name: "worker/a", kind: "worker", ref_kind: "branch", updated_at_ms: 5000 }],
    ancestors: [
      {
        snapshot_id: s0,
        git_oid: "0".repeat(40),
        tree_oid: "sha256:" + "d".repeat(64),
        parent_ids: [],
        state: "live",
        created_at_ms: 1000,
        depth: 1,
      },
    ],
    children: [],
    history: [],
    recovery_commands: [
      {
        id: "create_branch",
        label: "Create branch",
        description: "Branch cleanly",
        command: "git checkout -b recover/11111111 22222222",
        risk: "branch",
      },
    ],
  });

  assert.ok(detail);
  assert.equal(detail.snapshot.project, "proj-alpha");
  assert.equal(detail.ancestors.length, 1);
  assert.equal(detail.ancestors[0].depth, 1);
  assert.equal(detail.recovery_commands[0].risk, "branch");
});

test("buildGraphLayout keeps main on lane 0, assigns parallel agent lanes, and draws merge edges", () => {
  const s0 = "sha256:" + "0".repeat(64);
  const s1 = "sha256:" + "1".repeat(64);
  const s2 = "sha256:" + "2".repeat(64);
  const s3 = "sha256:" + "3".repeat(64);

  const nodes = [
    {
      snapshot_id: s3,
      parent_ids: [s0, s1],
      tree_oid: "sha256:" + "f".repeat(64),
      created_at_ms: 4000,
      git_oid: "3".repeat(40),
      state: "live",
      is_merge: true,
      refs: [{ name: "main", kind: "main", ref_kind: "main", updated_at_ms: 4000 }],
    },
    {
      snapshot_id: s2,
      parent_ids: [s0],
      tree_oid: "sha256:" + "e".repeat(64),
      created_at_ms: 3000,
      git_oid: "2".repeat(40),
      state: "live",
      is_merge: false,
      refs: [{ name: "task/42", kind: "task", ref_kind: "branch", updated_at_ms: 3000 }],
    },
    {
      snapshot_id: s1,
      parent_ids: [s0],
      tree_oid: "sha256:" + "d".repeat(64),
      created_at_ms: 2000,
      git_oid: "1".repeat(40),
      state: "live",
      is_merge: false,
      refs: [{ name: "worker/a", kind: "worker", ref_kind: "branch", updated_at_ms: 2000 }],
    },
    {
      snapshot_id: s0,
      parent_ids: [],
      tree_oid: "sha256:" + "c".repeat(64),
      created_at_ms: 1000,
      git_oid: "0".repeat(40),
      state: "live",
      is_merge: false,
      refs: [],
    },
  ];
  const refs = [
    { name: "main", kind: "main", ref_kind: "main", snapshot_id: s3, updated_at_ms: 4000 },
    { name: "worker/a", kind: "worker", ref_kind: "branch", snapshot_id: s1, updated_at_ms: 2000 },
    { name: "task/42", kind: "task", ref_kind: "branch", snapshot_id: s2, updated_at_ms: 3000 },
  ];

  const layout = buildGraphLayout(nodes, refs);
  const byId = new Map(layout.nodes.map((n) => [n.id, n]));
  assert.equal(byId.get(s3)?.lane, 0);
  assert.equal(byId.get(s0)?.lane, 0);
  assert.ok((byId.get(s1)?.lane ?? 0) >= 1);
  assert.ok((byId.get(s2)?.lane ?? 0) >= 1);
  assert.notEqual(byId.get(s1)?.lane, byId.get(s2)?.lane, "concurrent branches use distinct lanes");
  assert.ok(layout.laneCount >= 3);

  const mergeEdge = layout.edges.find((e) => e.fromId === s3 && e.toId === s1);
  assert.ok(mergeEdge?.isMergeEdge);
  assert.ok(mergeEdge?.path.includes(" C "), "merge convergence uses curved Bezier path");

  const mainEdge = layout.edges.find((e) => e.fromId === s3 && e.toId === s0);
  assert.ok(mainEdge?.isMainEdge);

  const lineage = collectLineageIds(nodes, s3);
  assert.deepEqual([...lineage].sort(), [s0, s1, s3].sort());
});

test("shortOid, refKindMeta, historyActionMeta, and fallbackRecoveryCommands format cleanly", () => {
  assert.equal(shortOid("sha256:abcdef1234567890"), "abcdef12");
  assert.equal(shortOid(null), "—");
  assert.equal(refKindMeta("main").tone, "success");
  assert.equal(refKindMeta("worker").tone, "info");
  assert.equal(refKindMeta("task").tone, "purple");
  assert.equal(historyActionMeta("rollback").tone, "danger");
  assert.equal(historyActionMeta("merged").tone, "success");

  const cmds = fallbackRecoveryCommands(
    {
      snapshot_id: "sha256:" + "ab".repeat(32),
      parent_ids: [],
      tree_oid: "sha256:" + "cd".repeat(32),
      created_at_ms: 1000,
      git_oid: "ef".repeat(20),
      state: "live",
      is_merge: false,
      refs: [],
    },
    "proj-x",
  );
  assert.equal(cmds.length, 5);
  assert.ok(cmds.some((c) => c.command.includes("spawn storage cutover --rollback git --project proj-x")));
});

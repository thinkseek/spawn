import type { BadgeTone } from "./components/ui/badge.tsx";

export type StorageRefKind = "main" | "task" | "worker" | "wip" | "tag";

export type RefLogAction =
  | "created"
  | "merged"
  | "fast-forwarded"
  | "rolled-back"
  | "retired"
  | "moved";

export type StorageBackend = "d6" | "cow" | "overlay" | "git";

export type SnapshotState = "live" | "collected";

export interface StorageRefBadge {
  name: string;
  kind: StorageRefKind;
  updated_at_ms: number;
}

export interface StorageRefEntry extends StorageRefBadge {
  snapshot_id: string;
}

export interface StorageTreeNode {
  snapshot_id: string;
  parent_ids: string[];
  tree_oid: string;
  created_at_ms: number;
  git_oid: string;
  state: SnapshotState;
  refs: StorageRefBadge[];
}

export function isMergeSnapshot(node: { parent_ids: readonly string[] }): boolean {
  return node.parent_ids.length >= 2;
}

export interface StorageHistoryEntry {
  seq: number;
  project: string;
  name: string;
  kind: StorageRefKind;
  old_snapshot: string | null;
  new_snapshot: string;
  reason: string;
  action: RefLogAction;
  at_ms: number;
}

export interface StorageTreeSummary {
  total_snapshots: number;
  live_snapshots: number;
  collected_snapshots: number;
  active_refs: number;
  history_entries: number;
  truncated: boolean;
  backend: StorageBackend;
  mode_note: string;
}

export interface StorageTreeResponse {
  projects: string[];
  active_project: string | null;
  summary: StorageTreeSummary;
  nodes: StorageTreeNode[];
  refs: StorageRefEntry[];
  history: StorageHistoryEntry[];
}

export interface StorageAncestor {
  snapshot_id: string;
  git_oid: string;
  tree_oid: string;
  parent_ids: string[];
  state: SnapshotState;
  created_at_ms: number;
  depth: number;
}

export interface StorageChildSnapshot {
  snapshot_id: string;
  git_oid: string;
  tree_oid: string;
  parent_ids: string[];
  state: SnapshotState;
  created_at_ms: number;
}

export interface StorageRecoveryCommand {
  id: string;
  label: string;
  description: string;
  command: string;
  risk: "read_only" | "branch" | "rollback";
}

export interface StorageSnapshotDetail {
  snapshot: StorageTreeNode & { project: string };
  refs: StorageRefBadge[];
  ancestors: StorageAncestor[];
  children: StorageChildSnapshot[];
  history: StorageHistoryEntry[];
  recovery_commands: StorageRecoveryCommand[];
}

export interface GraphNodeLayout {
  id: string;
  row: number;
  lane: number;
  x: number;
  y: number;
  isMainLane: boolean;
  isMerge: boolean;
  node: StorageTreeNode;
}

export interface GraphEdgeLayout {
  id: string;
  fromId: string;
  toId: string;
  fromLane: number;
  toLane: number;
  fromX: number;
  fromY: number;
  toX: number;
  toY: number;
  path: string;
  isMainEdge: boolean;
  isMergeEdge: boolean;
  isDanglingParent: boolean;
}

export interface GraphLayout {
  nodes: GraphNodeLayout[];
  edges: GraphEdgeLayout[];
  laneCount: number;
  width: number;
  height: number;
  rowHeight: number;
  laneSpacing: number;
  padLeft: number;
  padTop: number;
}

type JsonObj = Record<string, unknown>;

function asObj(value: unknown): JsonObj {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObj)
    : {};
}

function asStr(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function asNullableStr(value: unknown): string | null {
  return typeof value === "string" && value.length > 0 ? value : null;
}

function asNum(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function asBool(value: unknown, fallback = false): boolean {
  return typeof value === "boolean" ? value : fallback;
}

function asStrArray(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.filter((item): item is string => typeof item === "string");
}

function normalizeRefKind(value: unknown): StorageRefKind {
  switch (value) {
    case "main":
    case "task":
    case "worker":
    case "wip":
    case "tag":
      return value;
    default:
      return "worker";
  }
}

function normalizeRefLogAction(value: unknown): RefLogAction {
  switch (value) {
    case "created":
    case "merged":
    case "fast-forwarded":
    case "rolled-back":
    case "retired":
    case "moved":
      return value;
    default:
      return "moved";
  }
}

function normalizeStorageBackend(value: unknown): StorageBackend {
  switch (value) {
    case "d6":
    case "cow":
    case "overlay":
    case "git":
      return value;
    default:
      return "git";
  }
}

function normalizeSnapshotState(value: unknown): SnapshotState {
  return value === "collected" ? "collected" : "live";
}

function normalizeRefBadge(raw: unknown): StorageRefBadge | null {
  const o = asObj(raw);
  const name = asStr(o.name);
  if (!name) return null;
  return {
    name,
    kind: normalizeRefKind(o.kind),
    updated_at_ms: asNum(o.updated_at_ms, 0),
  };
}

function normalizeRefEntry(raw: unknown): StorageRefEntry | null {
  const badge = normalizeRefBadge(raw);
  if (!badge) return null;
  const o = asObj(raw);
  const snapshotId = asStr(o.snapshot_id);
  if (!snapshotId) return null;
  return { ...badge, snapshot_id: snapshotId };
}

function normalizeNode(raw: unknown): StorageTreeNode | null {
  const o = asObj(raw);
  const snapshotId = asStr(o.snapshot_id);
  if (!snapshotId) return null;
  const parentIds = asStrArray(o.parent_ids);
  const refs = Array.isArray(o.refs)
    ? o.refs.map(normalizeRefBadge).filter((item): item is StorageRefBadge => item !== null)
    : [];
  return {
    snapshot_id: snapshotId,
    parent_ids: parentIds,
    tree_oid: asStr(o.tree_oid),
    created_at_ms: asNum(o.created_at_ms, 0),
    git_oid: asStr(o.git_oid),
    state: normalizeSnapshotState(o.state),
    refs,
  };
}

function normalizeHistoryEntry(raw: unknown): StorageHistoryEntry | null {
  const o = asObj(raw);
  const name = asStr(o.name);
  const newSnapshot = asStr(o.new_snapshot);
  if (!name || !newSnapshot) return null;
  return {
    seq: asNum(o.seq, 0),
    project: asStr(o.project),
    name,
    kind: normalizeRefKind(o.kind),
    old_snapshot: asNullableStr(o.old_snapshot),
    new_snapshot: newSnapshot,
    reason: asStr(o.reason),
    action: normalizeRefLogAction(o.action),
    at_ms: asNum(o.at_ms, 0),
  };
}

export function normalizeStorageTree(raw: unknown): StorageTreeResponse {
  const o = asObj(raw);
  const summaryObj = asObj(o.summary);
  const nodes = Array.isArray(o.nodes)
    ? o.nodes.map(normalizeNode).filter((item): item is StorageTreeNode => item !== null)
    : [];
  const refs = Array.isArray(o.refs)
    ? o.refs.map(normalizeRefEntry).filter((item): item is StorageRefEntry => item !== null)
    : [];
  const history = Array.isArray(o.history)
    ? o.history
        .map(normalizeHistoryEntry)
        .filter((item): item is StorageHistoryEntry => item !== null)
    : [];
  return {
    projects: asStrArray(o.projects),
    active_project: asNullableStr(o.active_project),
    summary: {
      total_snapshots: asNum(summaryObj.total_snapshots, nodes.length),
      live_snapshots: asNum(summaryObj.live_snapshots, 0),
      collected_snapshots: asNum(summaryObj.collected_snapshots, 0),
      active_refs: asNum(summaryObj.active_refs, refs.length),
      history_entries: asNum(summaryObj.history_entries, history.length),
      truncated: asBool(summaryObj.truncated, false),
      backend: normalizeStorageBackend(summaryObj.backend),
      mode_note: asStr(
        summaryObj.mode_note,
        "No D6 snapshots recorded yet.",
      ),
    },
    nodes,
    refs,
    history,
  };
}

function normalizeRisk(value: unknown): StorageRecoveryCommand["risk"] {
  if (value === "branch" || value === "rollback") return value;
  return "read_only";
}

export function normalizeStorageSnapshot(raw: unknown): StorageSnapshotDetail | null {
  const o = asObj(raw);
  const snapObj = asObj(o.snapshot);
  const baseNode = normalizeNode(snapObj);
  if (!baseNode) return null;
  const refs = Array.isArray(o.refs)
    ? o.refs.map(normalizeRefBadge).filter((item): item is StorageRefBadge => item !== null)
    : [];
  const ancestors: StorageAncestor[] = Array.isArray(o.ancestors)
    ? o.ancestors
        .map((item) => {
          const a = asObj(item);
          const id = asStr(a.snapshot_id);
          if (!id) return null;
          return {
            snapshot_id: id,
            git_oid: asStr(a.git_oid),
            tree_oid: asStr(a.tree_oid),
            parent_ids: asStrArray(a.parent_ids),
            state: normalizeSnapshotState(a.state),
            created_at_ms: asNum(a.created_at_ms, 0),
            depth: asNum(a.depth, 1),
          };
        })
        .filter((item): item is StorageAncestor => item !== null)
    : [];
  const children: StorageChildSnapshot[] = Array.isArray(o.children)
    ? o.children
        .map((item) => {
          const c = asObj(item);
          const id = asStr(c.snapshot_id);
          if (!id) return null;
          return {
            snapshot_id: id,
            git_oid: asStr(c.git_oid),
            tree_oid: asStr(c.tree_oid),
            parent_ids: asStrArray(c.parent_ids),
            state: normalizeSnapshotState(c.state),
            created_at_ms: asNum(c.created_at_ms, 0),
          };
        })
        .filter((item): item is StorageChildSnapshot => item !== null)
    : [];
  const history = Array.isArray(o.history)
    ? o.history
        .map(normalizeHistoryEntry)
        .filter((item): item is StorageHistoryEntry => item !== null)
    : [];
  const recoveryCommands: StorageRecoveryCommand[] = Array.isArray(o.recovery_commands)
    ? o.recovery_commands
        .map((item) => {
          const cmd = asObj(item);
          const command = asStr(cmd.command);
          if (!command) return null;
          return {
            id: asStr(cmd.id, "command"),
            label: asStr(cmd.label, "Command"),
            description: asStr(cmd.description),
            command,
            risk: normalizeRisk(cmd.risk),
          };
        })
        .filter((item): item is StorageRecoveryCommand => item !== null)
    : [];

  return {
    snapshot: {
      ...baseNode,
      refs,
      project: asStr(snapObj.project),
    },
    refs,
    ancestors,
    children,
    history,
    recovery_commands: recoveryCommands,
  };
}

export function shortOid(id: string | null | undefined, length = 8): string {
  if (!id) return "—";
  const stripped = id.startsWith("sha256:") ? id.slice(7) : id;
  return Array.from(stripped).slice(0, Math.max(4, length)).join("");
}

export function refKindMeta(kind: StorageRefKind): {
  label: string;
  tone: BadgeTone;
} {
  switch (kind) {
    case "main":
      return { label: "main", tone: "success" };
    case "worker":
      return { label: "worker", tone: "info" };
    case "task":
      return { label: "task", tone: "purple" };
    case "wip":
      return { label: "wip", tone: "warning" };
    case "tag":
      return { label: "tag", tone: "neutral" };
  }
}

export function historyActionMeta(action: RefLogAction): {
  label: string;
  tone: BadgeTone;
} {
  switch (action) {
    case "created":
      return { label: "created", tone: "info" };
    case "merged":
      return { label: "merged", tone: "success" };
    case "fast-forwarded":
      return { label: "fast-forwarded", tone: "success" };
    case "rolled-back":
      return { label: "rolled-back", tone: "danger" };
    case "retired":
      return { label: "retired", tone: "warning" };
    case "moved":
      return { label: "moved", tone: "neutral" };
  }
}

export function shellToken(value: string): string {
  const clean = value.replace(/\0/g, "");
  if (clean.length > 0 && /^[A-Za-z0-9_./:@%+=,-]+$/.test(clean)) {
    return clean;
  }
  return `'${clean.replace(/'/g, "'\\''")}'`;
}

export function fallbackRecoveryCommands(
  node: StorageTreeNode,
  project: string | null,
): StorageRecoveryCommand[] {
  const shortId = shortOid(node.snapshot_id, 12);
  const proj = project && project.length > 0 ? project : "default";
  const gitTarget = node.git_oid || shortId;
  const safeProj = shellToken(proj);
  const safeGitTarget = shellToken(gitTarget);
  const safeRecoverBranch = shellToken(`recover/${shortId}`);
  const safeRollbackReason = shellToken(`time-machine rollback to ${shortId}`);
  return [
    {
      id: "verify_status",
      label: "Inspect D6 cutover status",
      description: "Read-only inspection of current D6/Git cutover phase and ref ledger.",
      command: `spawn storage cutover --status --project ${safeProj} --json`,
      risk: "read_only",
    },
    {
      id: "verify_parity",
      label: "Verify CAS parity",
      description: "Read-only parity check between D6 Merkle snapshots and Git border.",
      command: `spawn storage cutover --verify --project ${safeProj} --json`,
      risk: "read_only",
    },
    {
      id: "inspect_commit",
      label: "Inspect projected commit",
      description: "Read commit metadata and diff stat for the projected Git OID.",
      command: `git show --stat ${safeGitTarget}`,
      risk: "read_only",
    },
    {
      id: "create_branch",
      label: "Create recovery branch",
      description: "Non-destructive branch at this snapshot to explore or cherry-pick.",
      command: `git checkout -b ${safeRecoverBranch} ${safeGitTarget}`,
      risk: "branch",
    },
    {
      id: "rollback_git",
      label: "Rollback project to writable Git",
      description: "Retires active D6 refs while preserving immutable audit history.",
      command: `spawn storage cutover --rollback git --project ${safeProj} --reason ${safeRollbackReason}`,
      risk: "rollback",
    },
  ];
}

export function collectLineageIds(
  nodes: StorageTreeNode[],
  selectedId: string | null,
): Set<string> {
  const result = new Set<string>();
  if (!selectedId) return result;
  const byId = new Map(nodes.map((node) => [node.snapshot_id, node]));
  const queue = [selectedId];
  while (queue.length > 0) {
    const id = queue.shift();
    if (!id || result.has(id)) continue;
    result.add(id);
    const node = byId.get(id);
    if (!node) continue;
    for (const parent of node.parent_ids) {
      if (!result.has(parent) && byId.has(parent)) {
        queue.push(parent);
      }
    }
  }
  return result;
}

const ROW_HEIGHT = 64;
const LANE_SPACING = 28;
const PAD_LEFT = 22;
const PAD_TOP = 32;

/**
 * Computes a deterministic swimlane DAG layout:
 * - Lane 0 holds the `main` first-parent spine (`main` ref first-parent chain).
 * - Concurrent worker/task/branch lineages claim the lowest free lane >= 1
 *   and release lanes cleanly once their chain converges back to `main` or ends.
 */
export function buildGraphLayout(
  nodes: StorageTreeNode[],
  refs: StorageRefEntry[],
): GraphLayout {
  if (nodes.length === 0) {
    return {
      nodes: [],
      edges: [],
      laneCount: 1,
      width: PAD_LEFT * 2 + 12,
      height: 0,
      rowHeight: ROW_HEIGHT,
      laneSpacing: LANE_SPACING,
      padLeft: PAD_LEFT,
      padTop: PAD_TOP,
    };
  }

  const ordered = [...nodes].sort((a, b) => {
    if (b.created_at_ms !== a.created_at_ms) return b.created_at_ms - a.created_at_ms;
    return a.snapshot_id.localeCompare(b.snapshot_id);
  });

  const byId = new Map(ordered.map((node) => [node.snapshot_id, node]));
  const mainSpine = new Set<string>();

  const mainRef =
    refs.find((r) => r.kind === "main" || r.name === "main") ??
    null;
  let cursor =
    mainRef?.snapshot_id ??
    ordered.find((n) => n.refs.some((r) => r.kind === "main" || r.name === "main"))?.snapshot_id ??
    ordered[0]?.snapshot_id ??
    null;

  while (cursor && !mainSpine.has(cursor)) {
    const node = byId.get(cursor);
    if (!node) break;
    mainSpine.add(cursor);
    cursor = node.parent_ids[0] ?? null;
  }

  // Track which node ID each active lane >= 1 expects next as it walks downward.
  const activeLanes: Array<string | null> = [null];
  const nodeLane = new Map<string, number>();

  const claimBranchLane = (id: string, exclude: Set<number>): number => {
    for (let i = 1; i < activeLanes.length; i += 1) {
      if (activeLanes[i] === id && !exclude.has(i)) return i;
    }
    for (let i = 1; i < activeLanes.length; i += 1) {
      if (activeLanes[i] === null && !exclude.has(i)) return i;
    }
    let next = activeLanes.length;
    while (exclude.has(next)) next += 1;
    while (activeLanes.length <= next) activeLanes.push(null);
    return next;
  };

  for (const node of ordered) {
    const id = node.snapshot_id;
    let lane = 0;
    if (!mainSpine.has(id)) {
      const existing = activeLanes.findIndex((expected, idx) => idx > 0 && expected === id);
      lane = existing > 0 ? existing : claimBranchLane(id, new Set([0]));
    }
    nodeLane.set(id, lane);

    if (lane > 0 && activeLanes[lane] === id) {
      activeLanes[lane] = null;
    }

    const usedLanesForParents = new Set<number>();
    node.parent_ids.forEach((parentId, parentIdx) => {
      if (!byId.has(parentId)) return;
      if (mainSpine.has(parentId)) {
        return;
      }
      if (parentIdx === 0 && lane > 0 && activeLanes[lane] === null) {
        activeLanes[lane] = parentId;
        usedLanesForParents.add(lane);
      } else {
        const already = activeLanes.findIndex(
          (expected, idx) => idx > 0 && expected === parentId,
        );
        if (already <= 0) {
          const assigned = claimBranchLane(parentId, usedLanesForParents);
          activeLanes[assigned] = parentId;
          usedLanesForParents.add(assigned);
        }
      }
    });
  }

  let maxLane = 0;
  for (const lane of nodeLane.values()) {
    if (lane > maxLane) maxLane = lane;
  }

  const rowById = new Map<string, number>();
  const layoutNodes: GraphNodeLayout[] = ordered.map((node, row) => {
    const lane = nodeLane.get(node.snapshot_id) ?? 0;
    const x = PAD_LEFT + lane * LANE_SPACING;
    const y = PAD_TOP + row * ROW_HEIGHT;
    rowById.set(node.snapshot_id, row);
    return {
      id: node.snapshot_id,
      row,
      lane,
      x,
      y,
      isMainLane: lane === 0,
      isMerge: isMergeSnapshot(node),
      node,
    };
  });

  const layoutById = new Map(layoutNodes.map((n) => [n.id, n]));
  const edges: GraphEdgeLayout[] = [];

  for (const item of layoutNodes) {
    item.node.parent_ids.forEach((parentId, parentIndex) => {
      const parentLayout = layoutById.get(parentId);
      if (!parentLayout) {
        const toX = item.x;
        const toY = Math.min(item.y + ROW_HEIGHT * 0.45, PAD_TOP + ordered.length * ROW_HEIGHT - 8);
        edges.push({
          id: `${item.id}->${parentId}`,
          fromId: item.id,
          toId: parentId,
          fromLane: item.lane,
          toLane: item.lane,
          fromX: item.x,
          fromY: item.y,
          toX,
          toY,
          path: `M ${item.x} ${item.y} L ${toX} ${toY}`,
          isMainEdge: item.lane === 0 && parentIndex === 0,
          isMergeEdge: parentIndex > 0,
          isDanglingParent: true,
        });
        return;
      }

      const x1 = item.x;
      const y1 = item.y;
      const x2 = parentLayout.x;
      const y2 = parentLayout.y;
      const dy = Math.max(20, (y2 - y1) * 0.52);
      const path =
        x1 === x2
          ? `M ${x1} ${y1} L ${x2} ${y2}`
          : `M ${x1} ${y1} C ${x1} ${y1 + dy}, ${x2} ${y2 - dy}, ${x2} ${y2}`;

      edges.push({
        id: `${item.id}->${parentId}`,
        fromId: item.id,
        toId: parentId,
        fromLane: item.lane,
        toLane: parentLayout.lane,
        fromX: x1,
        fromY: y1,
        toX: x2,
        toY: y2,
        path,
        isMainEdge: item.lane === 0 && parentLayout.lane === 0 && parentIndex === 0,
        isMergeEdge: parentIndex > 0,
        isDanglingParent: false,
      });
    });
  }

  const laneCount = maxLane + 1;
  return {
    nodes: layoutNodes,
    edges,
    laneCount,
    width: Math.max(48, PAD_LEFT * 2 + maxLane * LANE_SPACING + 8),
    height: ordered.length * ROW_HEIGHT,
    rowHeight: ROW_HEIGHT,
    laneSpacing: LANE_SPACING,
    padLeft: PAD_LEFT,
    padTop: PAD_TOP,
  };
}

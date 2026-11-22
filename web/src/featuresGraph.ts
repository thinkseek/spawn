// Pure TypeScript mind-map tree, typed-relation graph, bidirectional file
// index, validation, search, and deterministic left-to-right layout engine.
//
// Zero React or DOM imports so every function is 100% testable under
// `node --test`. Never derives or mutates feature states: every state comes
// straight from the server's Feature read model.

import type {
  Feature,
  FeatureMission,
  FeatureState,
  RelationKind,
} from "./features.ts";
import { RELATION_KINDS, STATE_INFO, STATES } from "./features.ts";

export interface RelationMeta {
  label: string;
  inverseLabel: string;
  /** CSS custom property used for SVG stroke & arrowhead fill. */
  strokeVar: string;
  /** Tailwind badge styling using Spawn design tokens. */
  badgeClass: string;
  /** Optional SVG stroke-dasharray pattern for visual distinction. */
  dashArray?: string;
}

export const RELATION_META: Record<RelationKind, RelationMeta> = {
  depends_on: {
    label: "depends on",
    inverseLabel: "required by",
    strokeVar: "var(--info)",
    badgeClass: "bg-info-tint text-info",
  },
  blocks: {
    label: "blocks",
    inverseLabel: "blocked by",
    strokeVar: "var(--destructive)",
    badgeClass: "bg-fail-tint text-destructive",
  },
  implements: {
    label: "implements",
    inverseLabel: "implemented by",
    strokeVar: "var(--success)",
    badgeClass: "bg-ok-tint text-success",
  },
  extends: {
    label: "extends",
    inverseLabel: "extended by",
    strokeVar: "var(--trace-tool)",
    badgeClass: "bg-ok-tint text-success",
    dashArray: "5 3",
  },
  relates_to: {
    label: "relates to",
    inverseLabel: "related to",
    strokeVar: "var(--muted-foreground)",
    badgeClass: "bg-idle-tint text-muted-foreground",
    dashArray: "2 3",
  },
  supersedes: {
    label: "supersedes",
    inverseLabel: "superseded by",
    strokeVar: "var(--warning)",
    badgeClass: "bg-wait-tint text-warning",
    dashArray: "6 3",
  },
  verifies: {
    label: "verifies",
    inverseLabel: "verified by",
    strokeVar: "var(--ok-mark)",
    badgeClass: "bg-ok-tint text-success",
    dashArray: "6 2 2 2",
  },
  documented_by: {
    label: "documented by",
    inverseLabel: "documents",
    strokeVar: "var(--jev)",
    badgeClass: "bg-judge-tint text-jev",
    dashArray: "3 3",
  },
  contains: {
    label: "contains",
    inverseLabel: "contained by",
    strokeVar: "var(--accent-ink)",
    badgeClass: "bg-info-tint text-accent-ink",
  },
};

export type GraphGroupingMode = "auto" | "topic" | "state";

export type MindMapNodeKind = "mission" | "cluster" | "feature";

export interface MindMapTreeNode {
  id: string;
  nodeKind: MindMapNodeKind;
  title: string;
  subtitle: string | null;
  feature: Feature | null;
  /** Feature state distribution across this node's subtree (including self if feature). */
  stateCounts: Record<FeatureState, number>;
  /** Total feature nodes contained in this subtree (including self if feature). */
  totalFeatures: number;
  /** Number of descendant tree nodes (direct + transitive children). */
  descendantCount: number;
  /** Direct child IDs in deterministic display order. */
  children: string[];
  parentId: string | null;
  depth: number;
  clusterState?: FeatureState | null;
}

export interface FeatureMindMapTree {
  rootId: string;
  nodes: Record<string, MindMapTreeNode>;
  /** Feature id -> tree node id (identical for feature nodes). */
  featureIds: string[];
  /** Whether explicit `parent` hierarchy links shaped the tree. */
  hasExplicitHierarchy: boolean;
  /** Effective grouping used when organizing flat roots. */
  effectiveGrouping: "explicit" | "flat" | "topic" | "state";
}

export interface FileIndex {
  /** Repo-relative file path -> sorted owning features. */
  fileToFeatures: Record<string, Feature[]>;
  /** Only files owned by > 1 feature (many-to-many shared files). */
  sharedFiles: Record<string, Feature[]>;
  /** Sorted unique file paths across the mission. */
  allFiles: string[];
}

export interface OutgoingRelation {
  kind: RelationKind;
  target: string;
  label: string | null;
  targetFeature: Feature | null;
}

export interface IncomingRelation {
  kind: RelationKind;
  source: string;
  label: string | null;
  sourceFeature: Feature | null;
}

export interface RelationIndex {
  outgoing: Record<string, OutgoingRelation[]>;
  incoming: Record<string, IncomingRelation[]>;
  /** Counts per relation kind across the mission. */
  countsByKind: Record<RelationKind, number>;
  total: number;
}

export type ValidationIssueKind =
  | "self_link"
  | "dangling_parent"
  | "dangling_relation"
  | "parent_cycle"
  | "depends_on_cycle"
  | "contains_cycle";

export interface GraphValidationIssue {
  kind: ValidationIssueKind;
  featureId: string;
  targetId?: string;
  relationKind?: RelationKind;
  cycle?: string[];
  message: string;
}

export interface PositionedMindMapNode {
  id: string;
  node: MindMapTreeNode;
  x: number;
  y: number;
  width: number;
  height: number;
  expanded: boolean;
  hasChildren: boolean;
  /** Count of hidden descendant nodes when collapsed (0 when expanded or leaf). */
  collapsedCount: number;
  isMatch: boolean;
  isDimmed: boolean;
}

export interface MindMapHierarchyEdge {
  id: string;
  sourceId: string;
  targetId: string;
  path: string;
}

export interface MindMapRelationEdge {
  id: string;
  sourceId: string;
  targetId: string;
  kind: RelationKind;
  label: string | null;
  path: string;
  labelX: number;
  labelY: number;
}

export interface MindMapLayout {
  nodes: PositionedMindMapNode[];
  nodeMap: Record<string, PositionedMindMapNode>;
  hierarchyEdges: MindMapHierarchyEdge[];
  relationEdges: MindMapRelationEdge[];
  /** Number of relations whose endpoints are currently collapsed/hidden. */
  hiddenRelationCount: number;
  width: number;
  height: number;
  matchedIds: Set<string>;
  totalVisibleFeatures: number;
}

export interface BuildTreeOptions {
  groupingMode?: GraphGroupingMode;
  /** Max direct feature leaves in a flat cluster before splitting into range sub-branches. */
  maxDirectChildren?: number;
}

export interface LayoutOptions {
  expandedIds: Set<string>;
  searchQuery?: string;
  activeRelationKinds?: Set<RelationKind>;
  relationIndex?: RelationIndex;
}

const NODE_DIMENSIONS: Record<MindMapNodeKind, { width: number; height: number }> = {
  mission: { width: 248, height: 86 },
  cluster: { width: 226, height: 68 },
  feature: { width: 256, height: 78 },
};

const COL_GAP = 86;
const ROW_GAP = 16;
const PADDING = 48;

function emptyStateCounts(): Record<FeatureState, number> {
  return { planned: 0, implemented: 0, verified: 0, live: 0 };
}

function firstLine(text: string): string {
  const line = text.split(/\r?\n/, 1)[0]?.trim() ?? "";
  return line.length > 96 ? `${line.slice(0, 95)}…` : line;
}

/** Build a bidirectional index of repo files -> owning features. */
export function buildFileIndex(features: Feature[]): FileIndex {
  const fileToFeatures: Record<string, Feature[]> = {};
  const sortedFeatures = [...features].sort((a, b) => a.id.localeCompare(b.id));
  for (const feature of sortedFeatures) {
    const seenForFeature = new Set<string>();
    for (const file of feature.files) {
      if (!file || seenForFeature.has(file)) continue;
      seenForFeature.add(file);
      if (!fileToFeatures[file]) {
        fileToFeatures[file] = [];
      }
      fileToFeatures[file].push(feature);
    }
  }
  const allFiles = Object.keys(fileToFeatures).sort((a, b) => a.localeCompare(b));
  const sharedFiles: Record<string, Feature[]> = {};
  for (const file of allFiles) {
    if (fileToFeatures[file].length > 1) {
      sharedFiles[file] = fileToFeatures[file];
    }
  }
  return { fileToFeatures, sharedFiles, allFiles };
}

/** Co-owning features of a file, excluding the currently inspected feature. */
export function getSharedCoOwners(
  index: FileIndex,
  filePath: string,
  excludeFeatureId: string,
): Feature[] {
  const owners = index.fileToFeatures[filePath];
  if (!owners || owners.length <= 1) return [];
  return owners.filter((owner) => owner.id !== excludeFeatureId);
}

/** Build outgoing & incoming typed-relation lookup maps. */
export function buildRelationIndex(features: Feature[]): RelationIndex {
  const byId = new Map<string, Feature>(features.map((f) => [f.id, f]));
  const outgoing: Record<string, OutgoingRelation[]> = {};
  const incoming: Record<string, IncomingRelation[]> = {};
  const countsByKind = Object.fromEntries(
    RELATION_KINDS.map((kind) => [kind, 0]),
  ) as Record<RelationKind, number>;

  let total = 0;
  for (const feature of features) {
    outgoing[feature.id] = [];
    if (!incoming[feature.id]) incoming[feature.id] = [];
  }

  for (const feature of features) {
    for (const rel of feature.relations) {
      const targetFeature = byId.get(rel.target) ?? null;
      outgoing[feature.id].push({
        kind: rel.kind,
        target: rel.target,
        label: rel.label,
        targetFeature,
      });
      if (!incoming[rel.target]) incoming[rel.target] = [];
      incoming[rel.target].push({
        kind: rel.kind,
        source: feature.id,
        label: rel.label,
        sourceFeature: feature,
      });
      countsByKind[rel.kind] += 1;
      total += 1;
    }
  }

  return { outgoing, incoming, countsByKind, total };
}

/** Validate explicit feature links and hierarchy, detecting self-links,
 * dangling targets, and cycles in `parent`, `depends_on`, and `contains`. */
export function validateFeatureGraph(features: Feature[]): GraphValidationIssue[] {
  const issues: GraphValidationIssue[] = [];
  const ids = new Set(features.map((f) => f.id));

  const parentEdges = new Map<string, string[]>();
  const dependsEdges = new Map<string, string[]>();
  const containsEdges = new Map<string, string[]>();

  for (const feature of features) {
    if (feature.parent !== null) {
      if (feature.parent === feature.id) {
        issues.push({
          kind: "self_link",
          featureId: feature.id,
          targetId: feature.parent,
          message: `Feature \`${feature.id}\` cannot be its own parent`,
        });
      } else if (!ids.has(feature.parent)) {
        issues.push({
          kind: "dangling_parent",
          featureId: feature.id,
          targetId: feature.parent,
          message: `Feature \`${feature.id}\` references missing parent \`${feature.parent}\``,
        });
      } else {
        parentEdges.set(feature.id, [feature.parent]);
      }
    }

    for (const rel of feature.relations) {
      if (rel.target === feature.id) {
        issues.push({
          kind: "self_link",
          featureId: feature.id,
          targetId: rel.target,
          relationKind: rel.kind,
          message: `Feature \`${feature.id}\` cannot link \`${rel.kind}\` to itself`,
        });
      } else if (!ids.has(rel.target)) {
        issues.push({
          kind: "dangling_relation",
          featureId: feature.id,
          targetId: rel.target,
          relationKind: rel.kind,
          message: `Feature \`${feature.id}\` relation \`${rel.kind}\` references missing target \`${rel.target}\``,
        });
      } else if (rel.kind === "depends_on") {
        const list = dependsEdges.get(feature.id) ?? [];
        list.push(rel.target);
        dependsEdges.set(feature.id, list);
      } else if (rel.kind === "contains") {
        const list = containsEdges.get(feature.id) ?? [];
        list.push(rel.target);
        containsEdges.set(feature.id, list);
      }
    }
  }

  for (const cycle of findCycles(parentEdges)) {
    issues.push({
      kind: "parent_cycle",
      featureId: cycle[0],
      cycle,
      message: `Parent hierarchy cycle: ${cycle.join(" -> ")}`,
    });
  }
  for (const cycle of findCycles(dependsEdges)) {
    issues.push({
      kind: "depends_on_cycle",
      featureId: cycle[0],
      cycle,
      message: `depends_on cycle: ${cycle.join(" -> ")}`,
    });
  }
  for (const cycle of findCycles(containsEdges)) {
    issues.push({
      kind: "contains_cycle",
      featureId: cycle[0],
      cycle,
      message: `contains cycle: ${cycle.join(" -> ")}`,
    });
  }

  return issues;
}

function findCycles(edges: Map<string, string[]>): string[][] {
  const state = new Map<string, 1 | 2>(); // 1 = visiting, 2 = visited
  const stack: string[] = [];
  const cycles: string[][] = [];
  const reported = new Set<string>();

  function dfs(node: string): void {
    state.set(node, 1);
    stack.push(node);
    for (const next of edges.get(node) ?? []) {
      const nextState = state.get(next);
      if (nextState === 1) {
        const start = stack.indexOf(next);
        const cycle = [...stack.slice(start), next];
        const canonical = [...cycle.slice(0, -1)].sort().join("|");
        if (!reported.has(canonical)) {
          reported.add(canonical);
          cycles.push(cycle);
        }
      } else if (nextState === undefined) {
        dfs(next);
      }
    }
    stack.pop();
    state.set(node, 2);
  }

  for (const node of [...edges.keys()].sort()) {
    if (state.get(node) === undefined) {
      dfs(node);
    }
  }
  return cycles;
}

const DOMAIN_KEYWORDS: Array<{ key: string; label: string; pattern: RegExp }> = [
  {
    key: "serve-api",
    label: "Serve & API",
    pattern: /\b(serve|api|http|endpoint|route|etag|sse|poll|server|auth|token)\b/i,
  },
  {
    key: "web-ui",
    label: "Web UI & Views",
    pattern: /\b(web|ui|view|page|dashboard|sidebar|card|chart|timeline|mind.?map|drawer|theme|css|frontend)\b/i,
  },
  {
    key: "gate-verify",
    label: "Gate & Verification",
    pattern: /\b(gate|verify|verification|scenario|check|test|audit|lint|clippy|scorecard|judge)\b/i,
  },
  {
    key: "mission-backlog",
    label: "Mission & Backlog",
    pattern: /\b(mission|backlog|objective|feature.?map|mapping|rsi|roadmap|plan)\b/i,
  },
  {
    key: "agents-sessions",
    label: "Agents & Sessions",
    pattern: /\b(agent|session|worker|resident|task|supervisor|review|handoff|prompt)\b/i,
  },
  {
    key: "storage-postgres",
    label: "Storage & Postgres",
    pattern: /\b(store|postgres|sql|database|db|migration|schema|event|log|record|stream)\b/i,
  },
  {
    key: "deploy-ops",
    label: "Deploy & Runtime",
    pattern: /\b(deploy|ship|release|systemd|service|git|land|landing|branch|worktree|cli)\b/i,
  },
];

function extractTopic(feature: Feature): { key: string; label: string } | null {
  if (feature.tags.length > 0) {
    const tag = feature.tags[0];
    return {
      key: `tag:${tag}`,
      label: tag.replace(/[-_]+/g, " ").replace(/\b\w/g, (ch) => ch.toUpperCase()),
    };
  }

  const prefixMatch = feature.title.match(/^([A-Za-z][A-Za-z0-9 /_-]{1,26})(?::|\s+—\s+|\s+-\s+)/);
  if (prefixMatch) {
    const raw = prefixMatch[1].trim();
    const key = raw.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
    if (key.length >= 2 && key !== "objective") {
      return {
        key: `prefix:${key}`,
        label: raw.charAt(0).toUpperCase() + raw.slice(1),
      };
    }
  }

  if (feature.files.length > 0) {
    const parts = feature.files[0].split("/");
    if (parts.length >= 2) {
      const dir = `${parts[0]}/${parts[1]}`;
      return { key: `dir:${dir}`, label: dir };
    }
  }

  const text = `${feature.title} ${feature.summary ?? ""}`;
  for (const domain of DOMAIN_KEYWORDS) {
    if (domain.pattern.test(text)) {
      return { key: `domain:${domain.key}`, label: domain.label };
    }
  }

  return null;
}

function objectiveOrderKey(feature: Feature): number {
  if (feature.objectives.length > 0) {
    return Math.min(...feature.objectives);
  }
  const match = feature.id.match(/(\d+)/);
  return match ? Number(match[1]) : Number.MAX_SAFE_INTEGER;
}

function sortFeaturesDeterministically(features: Feature[]): Feature[] {
  return [...features].sort((a, b) => {
    const oa = objectiveOrderKey(a);
    const ob = objectiveOrderKey(b);
    if (oa !== ob) return oa - ob;
    return a.title.localeCompare(b.title) || a.id.localeCompare(b.id);
  });
}

function rangeLabel(missionId: number, slice: Feature[]): string {
  const first = slice[0];
  const last = slice[slice.length - 1];
  const firstObj = first.objectives[0];
  const lastObj = last.objectives[0];
  if (firstObj !== undefined && lastObj !== undefined) {
    const pad = (n: number) => `${missionId}/${String(n).padStart(3, "0")}`;
    return firstObj === lastObj ? `Objective ${pad(firstObj)}` : `Objectives ${pad(firstObj)}–${pad(lastObj)}`;
  }
  const a = (first.title[0] ?? "#").toUpperCase();
  const b = (last.title[0] ?? "#").toUpperCase();
  return a === b ? `Group ${a}` : `Group ${a}–${b}`;
}

/** Construct a deterministic hierarchical Mind Map tree for any mission:
 * - Preserves explicit `parent` hierarchies.
 * - Automatically organizes dense flat objective missions (like 189 objectives)
 *   into clean domain/topic or state branches, with objective-range sub-branches
 *   whenever a flat branch exceeds `maxDirectChildren` (default 12). */
export function buildFeatureMindMapTree(
  mission: FeatureMission,
  options: BuildTreeOptions = {},
): FeatureMindMapTree {
  const maxDirectChildren = Math.max(4, options.maxDirectChildren ?? 12);
  const groupingMode = options.groupingMode ?? "auto";
  const rootId = `mission:${mission.id}`;
  const nodes: Record<string, MindMapTreeNode> = {};

  nodes[rootId] = {
    id: rootId,
    nodeKind: "mission",
    title: firstLine(mission.text) || `Mission #${mission.id}`,
    subtitle: `mission #${mission.id} · ${mission.features.length} capabilities`,
    feature: null,
    stateCounts: emptyStateCounts(),
    totalFeatures: 0,
    descendantCount: 0,
    children: [],
    parentId: null,
    depth: 0,
  };

  const byId = new Map<string, Feature>(mission.features.map((f) => [f.id, f]));

  // Determine valid non-cyclic parent links.
  const validParent = new Map<string, string>();
  for (const feature of mission.features) {
    if (feature.parent && feature.parent !== feature.id && byId.has(feature.parent)) {
      validParent.set(feature.id, feature.parent);
    }
  }
  // Break any cycle defensively so tree construction always terminates.
  for (const id of [...validParent.keys()].sort()) {
    const visited = new Set<string>([id]);
    let cur = validParent.get(id);
    while (cur !== undefined) {
      if (visited.has(cur)) {
        validParent.delete(id);
        break;
      }
      visited.add(cur);
      cur = validParent.get(cur);
    }
  }

  const hasExplicitHierarchy = validParent.size > 0;

  // Build feature nodes upfront.
  for (const feature of mission.features) {
    nodes[feature.id] = {
      id: feature.id,
      nodeKind: "feature",
      title: feature.title,
      subtitle: feature.id,
      feature,
      stateCounts: emptyStateCounts(),
      totalFeatures: 1,
      descendantCount: 0,
      children: [],
      parentId: null,
      depth: 1,
    };
  }

  // Attach explicit parent -> child edges among features.
  const rootFeatures: Feature[] = [];
  for (const feature of sortFeaturesDeterministically(mission.features)) {
    const parentId = validParent.get(feature.id);
    if (parentId) {
      nodes[parentId].children.push(feature.id);
      nodes[feature.id].parentId = parentId;
    } else {
      rootFeatures.push(feature);
    }
  }

  const attachWithRangeFolding = (
    parentId: string,
    features: Feature[],
    clusterPrefix: string,
  ): void => {
    const ordered = sortFeaturesDeterministically(features);
    if (ordered.length <= maxDirectChildren) {
      for (const f of ordered) {
        nodes[parentId].children.push(f.id);
        nodes[f.id].parentId = parentId;
      }
      return;
    }
    const chunkCount = Math.ceil(ordered.length / maxDirectChildren);
    for (let index = 0; index < chunkCount; index += 1) {
      const slice = ordered.slice(index * maxDirectChildren, (index + 1) * maxDirectChildren);
      const subId = `${clusterPrefix}:range-${index + 1}`;
      nodes[subId] = {
        id: subId,
        nodeKind: "cluster",
        title: rangeLabel(mission.id, slice),
        subtitle: `${slice.length} capabilities`,
        feature: null,
        stateCounts: emptyStateCounts(),
        totalFeatures: 0,
        descendantCount: 0,
        children: [],
        parentId,
        depth: 0,
      };
      nodes[parentId].children.push(subId);
      for (const f of slice) {
        nodes[subId].children.push(f.id);
        nodes[f.id].parentId = subId;
      }
    }
  };

  let effectiveGrouping: FeatureMindMapTree["effectiveGrouping"] = hasExplicitHierarchy
    ? "explicit"
    : "flat";

  const shouldClusterRoots =
    groupingMode !== "auto" ||
    (!hasExplicitHierarchy && rootFeatures.length > maxDirectChildren) ||
    (hasExplicitHierarchy && rootFeatures.length > maxDirectChildren * 2);

  if (!shouldClusterRoots) {
    for (const feature of rootFeatures) {
      nodes[rootId].children.push(feature.id);
      nodes[feature.id].parentId = rootId;
    }
  } else {
    const useTopic = groupingMode === "topic" || groupingMode === "auto";
    let groupedByTopic: Array<{ key: string; label: string; items: Feature[] }> | null = null;

    if (useTopic) {
      const buckets = new Map<string, { label: string; items: Feature[] }>();
      let matchedCount = 0;
      for (const feature of rootFeatures) {
        const topic = extractTopic(feature);
        if (topic) {
          matchedCount += 1;
          const bucket = buckets.get(topic.key) ?? { label: topic.label, items: [] };
          bucket.items.push(feature);
          buckets.set(topic.key, bucket);
        } else {
          const bucket = buckets.get("other") ?? { label: "General Capabilities", items: [] };
          bucket.items.push(feature);
          buckets.set("other", bucket);
        }
      }

      // Collapse singleton topic buckets into General Capabilities when dense
      // so we don't produce 40 1-item branches.
      const meaningful: Array<{ key: string; label: string; items: Feature[] }> = [];
      const overflow: Feature[] = [];
      for (const [key, bucket] of buckets.entries()) {
        if (key === "other") {
          overflow.push(...bucket.items);
        } else if (bucket.items.length >= 2) {
          meaningful.push({ key, label: bucket.label, items: bucket.items });
        } else {
          overflow.push(...bucket.items);
        }
      }
      meaningful.sort(
        (a, b) => b.items.length - a.items.length || a.label.localeCompare(b.label),
      );

      const meaningfulCoverage =
        rootFeatures.length === 0
          ? 0
          : meaningful.reduce((sum, g) => sum + g.items.length, 0) / rootFeatures.length;

      if (
        groupingMode === "topic" ||
        (meaningful.length >= 2 && matchedCount > 0 && meaningfulCoverage >= 0.4)
      ) {
        if (overflow.length > 0) {
          meaningful.push({
            key: "general",
            label: "General Capabilities",
            items: overflow,
          });
        }
        groupedByTopic = meaningful;
      }
    }

    if (groupedByTopic && groupedByTopic.length > 0) {
      effectiveGrouping = "topic";
      for (const group of groupedByTopic) {
        const clusterId = `cluster:${mission.id}:${group.key}`;
        nodes[clusterId] = {
          id: clusterId,
          nodeKind: "cluster",
          title: group.label,
          subtitle: `${group.items.length} capabilities`,
          feature: null,
          stateCounts: emptyStateCounts(),
          totalFeatures: 0,
          descendantCount: 0,
          children: [],
          parentId: rootId,
          depth: 1,
        };
        nodes[rootId].children.push(clusterId);
        attachWithRangeFolding(clusterId, group.items, clusterId);
      }
    } else {
      effectiveGrouping = "state";
      for (const state of STATES) {
        const items = rootFeatures.filter((f) => f.state === state);
        if (items.length === 0) continue;
        const clusterId = `cluster:${mission.id}:state-${state}`;
        nodes[clusterId] = {
          id: clusterId,
          nodeKind: "cluster",
          title: STATE_INFO[state].label,
          subtitle: `${items.length} capabilities`,
          feature: null,
          stateCounts: emptyStateCounts(),
          totalFeatures: 0,
          descendantCount: 0,
          children: [],
          parentId: rootId,
          depth: 1,
          clusterState: state,
        };
        nodes[rootId].children.push(clusterId);
        attachWithRangeFolding(clusterId, items, clusterId);
      }
    }
  }

  // Compute depths, stateCounts, totalFeatures, and descendantCount bottom-up.
  function populateMetrics(nodeId: string, depth: number): void {
    const node = nodes[nodeId];
    node.depth = depth;
    const counts = emptyStateCounts();
    let totalFeatures = 0;
    let descendantCount = 0;

    if (node.feature) {
      counts[node.feature.state] += 1;
      totalFeatures += 1;
    }

    for (const childId of node.children) {
      populateMetrics(childId, depth + 1);
      const child = nodes[childId];
      for (const state of STATES) {
        counts[state] += child.stateCounts[state];
      }
      totalFeatures += child.totalFeatures;
      descendantCount += 1 + child.descendantCount;
    }

    node.stateCounts = counts;
    node.totalFeatures = totalFeatures;
    node.descendantCount = descendantCount;
  }

  populateMetrics(rootId, 0);

  return {
    rootId,
    nodes,
    featureIds: mission.features.map((f) => f.id),
    hasExplicitHierarchy,
    effectiveGrouping,
  };
}

/** Choose a clean initial expanded set so small maps open fully and dense maps
 * (like 189 objectives) open to their top-level branches without overlap. */
export function defaultExpandedIds(tree: FeatureMindMapTree): Set<string> {
  const expanded = new Set<string>([tree.rootId]);
  const root = tree.nodes[tree.rootId];
  if (!root) return expanded;

  const totalNodes = Object.keys(tree.nodes).length;
  if (totalNodes <= 30) {
    for (const node of Object.values(tree.nodes)) {
      if (node.children.length > 0) {
        expanded.add(node.id);
      }
    }
    return expanded;
  }

  // For dense missions (> 48 features), keep only the root expanded so the
  // user lands on clean top-level domain/state branches with +N counts.
  if (root.totalFeatures > 48) {
    return expanded;
  }

  // For medium trees, expand depth 1 only if the resulting visible count stays calm.
  const depth1Children = root.children.reduce(
    (sum, childId) => sum + (tree.nodes[childId]?.children.length ?? 0),
    0,
  );
  if (depth1Children <= 20) {
    for (const childId of root.children) {
      if ((tree.nodes[childId]?.children.length ?? 0) > 0) {
        expanded.add(childId);
      }
    }
  }
  return expanded;
}

/** Expand every branch node in the tree. */
export function allBranchIds(tree: FeatureMindMapTree): Set<string> {
  const ids = new Set<string>();
  for (const node of Object.values(tree.nodes)) {
    if (node.children.length > 0) {
      ids.add(node.id);
    }
  }
  return ids;
}

/** Return ancestor node IDs (from root down to immediate parent) for a node. */
export function ancestorsOf(tree: FeatureMindMapTree, nodeId: string): string[] {
  const result: string[] = [];
  const seen = new Set<string>();
  let cur = tree.nodes[nodeId]?.parentId ?? null;
  while (cur !== null && !seen.has(cur)) {
    seen.add(cur);
    result.push(cur);
    cur = tree.nodes[cur]?.parentId ?? null;
  }
  return result.reverse();
}

/** Search nodes across id, title, summary, tags, files, objectives, and sources. */
export function searchMindMap(
  tree: FeatureMindMapTree,
  rawQuery: string,
): { matchedIds: Set<string>; ancestorIds: Set<string> } {
  const query = rawQuery.trim().toLowerCase();
  const matchedIds = new Set<string>();
  const ancestorIds = new Set<string>();
  if (!query) return { matchedIds, ancestorIds };

  for (const node of Object.values(tree.nodes)) {
    if (node.nodeKind === "mission") continue;
    let matches =
      node.title.toLowerCase().includes(query) ||
      node.id.toLowerCase().includes(query);

    if (!matches && node.feature) {
      const f = node.feature;
      matches =
        (f.summary?.toLowerCase().includes(query) ?? false) ||
        f.kind.toLowerCase().includes(query) ||
        f.state.toLowerCase().includes(query) ||
        f.tags.some((t) => t.toLowerCase().includes(query)) ||
        f.files.some((file) => file.toLowerCase().includes(query)) ||
        f.objectives.some(
          (obj) => String(obj) === query || String(obj).padStart(3, "0").includes(query),
        ) ||
        f.sources.some(
          (src) =>
            src.ref.toLowerCase().includes(query) ||
            (src.title?.toLowerCase().includes(query) ?? false),
        );
    }

    if (matches) {
      matchedIds.add(node.id);
      for (const ancestorId of ancestorsOf(tree, node.id)) {
        ancestorIds.add(ancestorId);
      }
    }
  }

  return { matchedIds, ancestorIds };
}

/** Compute deterministic left-to-right mind-map coordinates, smooth cubic
 * Bezier hierarchy curves, and curved directed typed-relation overlay arcs. */
export function layoutMindMap(
  tree: FeatureMindMapTree,
  options: LayoutOptions,
): MindMapLayout {
  const searchQuery = options.searchQuery?.trim() ?? "";
  const { matchedIds, ancestorIds } = searchMindMap(tree, searchQuery);
  const hasSearch = searchQuery.length > 0;

  const effectiveExpanded = new Set<string>(options.expandedIds);
  for (const ancestorId of ancestorIds) {
    effectiveExpanded.add(ancestorId);
  }

  const subtreeHeights = new Map<string, number>();

  function measure(nodeId: string): number {
    const node = tree.nodes[nodeId];
    const dim = NODE_DIMENSIONS[node.nodeKind];
    const isExpanded = effectiveExpanded.has(nodeId) && node.children.length > 0;
    if (!isExpanded) {
      subtreeHeights.set(nodeId, dim.height);
      return dim.height;
    }
    let total = 0;
    for (let i = 0; i < node.children.length; i += 1) {
      if (i > 0) total += ROW_GAP;
      total += measure(node.children[i]);
    }
    const height = Math.max(dim.height, total);
    subtreeHeights.set(nodeId, height);
    return height;
  }

  const root = tree.nodes[tree.rootId];
  if (!root) {
    return {
      nodes: [],
      nodeMap: {},
      hierarchyEdges: [],
      relationEdges: [],
      hiddenRelationCount: 0,
      width: 600,
      height: 300,
      matchedIds,
      totalVisibleFeatures: 0,
    };
  }

  const totalTreeHeight = measure(tree.rootId);
  const positioned: PositionedMindMapNode[] = [];
  const nodeMap: Record<string, PositionedMindMapNode> = {};
  const hierarchyEdges: MindMapHierarchyEdge[] = [];
  let maxX = PADDING;
  let maxY = PADDING;
  let totalVisibleFeatures = 0;

  function place(nodeId: string, x: number, topY: number): void {
    const node = tree.nodes[nodeId];
    const dim = NODE_DIMENSIONS[node.nodeKind];
    const bandHeight = subtreeHeights.get(nodeId) ?? dim.height;
    const y = topY + (bandHeight - dim.height) / 2;
    const hasChildren = node.children.length > 0;
    const expanded = hasChildren && effectiveExpanded.has(nodeId);
    const collapsedCount = hasChildren && !expanded ? node.descendantCount : 0;
    const isMatch = matchedIds.has(nodeId);
    const isDimmed = hasSearch && !isMatch && !ancestorIds.has(nodeId);

    const entry: PositionedMindMapNode = {
      id: nodeId,
      node,
      x,
      y,
      width: dim.width,
      height: dim.height,
      expanded,
      hasChildren,
      collapsedCount,
      isMatch,
      isDimmed,
    };
    positioned.push(entry);
    nodeMap[nodeId] = entry;

    if (node.nodeKind === "feature") {
      totalVisibleFeatures += 1;
    }
    maxX = Math.max(maxX, x + dim.width);
    maxY = Math.max(maxY, y + dim.height);

    if (expanded) {
      let childrenTotalHeight = 0;
      for (let i = 0; i < node.children.length; i += 1) {
        if (i > 0) childrenTotalHeight += ROW_GAP;
        childrenTotalHeight += subtreeHeights.get(node.children[i]) ?? 0;
      }
      let cursorY = topY + (bandHeight - childrenTotalHeight) / 2;
      const childX = x + dim.width + COL_GAP;
      const parentAnchorX = x + dim.width;
      const parentAnchorY = y + dim.height / 2;

      for (const childId of node.children) {
        const childBand = subtreeHeights.get(childId) ?? 0;
        place(childId, childX, cursorY);
        const childPos = nodeMap[childId];
        const childAnchorX = childPos.x;
        const childAnchorY = childPos.y + childPos.height / 2;
        const midX = (parentAnchorX + childAnchorX) / 2;
        hierarchyEdges.push({
          id: `hier:${nodeId}->${childId}`,
          sourceId: nodeId,
          targetId: childId,
          path: `M ${parentAnchorX} ${parentAnchorY} C ${midX} ${parentAnchorY}, ${midX} ${childAnchorY}, ${childAnchorX} ${childAnchorY}`,
        });
        cursorY += childBand + ROW_GAP;
      }
    }
  }

  place(tree.rootId, PADDING, PADDING);

  // Build curved directed typed-relation overlay edges between visible feature nodes.
  const activeKinds = options.activeRelationKinds ?? new Set<RelationKind>(RELATION_KINDS);
  const relationEdges: MindMapRelationEdge[] = [];
  let hiddenRelationCount = 0;
  let maxArcX = maxX;

  if (options.relationIndex) {
    const pairCount = new Map<string, number>();
    for (const [sourceId, rels] of Object.entries(options.relationIndex.outgoing)) {
      for (const rel of rels) {
        if (!activeKinds.has(rel.kind)) continue;
        const src = nodeMap[sourceId];
        const dst = nodeMap[rel.target];
        if (!src || !dst) {
          hiddenRelationCount += 1;
          continue;
        }
        const pairKey = [sourceId, rel.target].sort().join("|");
        const parallelIdx = pairCount.get(pairKey) ?? 0;
        pairCount.set(pairKey, parallelIdx + 1);

        const curve = buildRelationCurve(src, dst, parallelIdx);
        maxArcX = Math.max(maxArcX, curve.maxX);
        relationEdges.push({
          id: `rel:${sourceId}:${rel.kind}:${rel.target}`,
          sourceId,
          targetId: rel.target,
          kind: rel.kind,
          label: rel.label,
          path: curve.path,
          labelX: curve.labelX,
          labelY: curve.labelY,
        });
      }
    }
  }

  return {
    nodes: positioned,
    nodeMap,
    hierarchyEdges,
    relationEdges,
    hiddenRelationCount,
    width: Math.max(maxArcX + PADDING + 40, 680),
    height: Math.max(totalTreeHeight + PADDING * 2, maxY + PADDING, 360),
    matchedIds,
    totalVisibleFeatures,
  };
}

function buildRelationCurve(
  src: PositionedMindMapNode,
  dst: PositionedMindMapNode,
  parallelIdx: number,
): { path: string; labelX: number; labelY: number; maxX: number } {
  const srcMidY = src.y + src.height / 2;
  const dstMidY = dst.y + dst.height / 2;
  const sameColumn = Math.abs(src.x - dst.x) < 20;

  if (sameColumn) {
    // Arc out to the right so sibling links never collide with the tree trunk.
    const x1 = src.x + src.width;
    const y1 = srcMidY;
    const x2 = dst.x + dst.width;
    const y2 = dstMidY;
    const dy = Math.abs(y2 - y1);
    const bow = Math.min(180, 48 + dy * 0.22 + parallelIdx * 24);
    const cx = Math.max(x1, x2) + bow;
    const path = `M ${x1} ${y1} C ${cx} ${y1}, ${cx} ${y2}, ${x2} ${y2}`;
    // Cubic Bezier midpoint at t = 0.5
    const labelX = 0.125 * x1 + 0.375 * cx + 0.375 * cx + 0.125 * x2;
    const labelY = (y1 + y2) / 2;
    return { path, labelX, labelY, maxX: cx + 16 };
  }

  const leftToRight = src.x < dst.x;
  const x1 = leftToRight ? src.x + src.width : src.x;
  const y1 = srcMidY;
  const x2 = leftToRight ? dst.x : dst.x + dst.width;
  const y2 = dstMidY;
  const dx = Math.abs(x2 - x1);
  const spread = Math.max(44, dx * 0.38);
  const archSign = parallelIdx % 2 === 0 ? -1 : 1;
  const archOffset = archSign * (28 + Math.floor(parallelIdx / 2) * 22);
  const c1x = leftToRight ? x1 + spread : x1 - spread;
  const c2x = leftToRight ? x2 - spread : x2 + spread;
  const c1y = y1 + archOffset;
  const c2y = y2 + archOffset;

  const path = `M ${x1} ${y1} C ${c1x} ${c1y}, ${c2x} ${c2y}, ${x2} ${y2}`;
  const labelX = 0.125 * x1 + 0.375 * c1x + 0.375 * c2x + 0.125 * x2;
  const labelY = 0.125 * y1 + 0.375 * c1y + 0.375 * c2y + 0.125 * y2;
  return { path, labelX, labelY, maxX: Math.max(x1, x2, c1x, c2x) };
}

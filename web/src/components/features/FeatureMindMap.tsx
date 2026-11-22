import {
  ChevronDown,
  ChevronRight,
  FileCode,
  Link2,
  Maximize2,
  Minus,
  OctagonAlert,
  Plus,
  RotateCcw,
  Search,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { FeatureMission, RelationKind } from "../../features";
import { RELATION_KINDS, STATE_INFO, STATES } from "../../features";
import type {
  FeatureMindMapTree,
  FileIndex,
  GraphGroupingMode,
  MindMapRelationEdge,
  PositionedMindMapNode,
  RelationIndex,
} from "../../featuresGraph";
import {
  RELATION_META,
  allBranchIds,
  ancestorsOf,
  defaultExpandedIds,
  layoutMindMap,
} from "../../featuresGraph";
import { FOCUS_RING, chip, cx } from "../ui";

export interface FeatureMindMapProps {
  mission: FeatureMission;
  tree: FeatureMindMapTree;
  fileIndex: FileIndex;
  relationIndex: RelationIndex;
  groupingMode: GraphGroupingMode;
  onGroupingModeChange: (mode: GraphGroupingMode) => void;
  selectedNodeId: string | null;
  onSelectNode: (nodeId: string | null) => void;
  /** Incrementing request token to center the canvas on `selectedNodeId`. */
  focusTrigger: number;
}

const MIN_ZOOM = 0.28;
const MAX_ZOOM = 1.9;

/** Interactive Mind-Map canvas with drag pan, wheel/button zoom, expand/collapse
 * branches, live search with ancestor auto-expansion, and curved directed
 * typed-relation overlay edges. */
export function FeatureMindMap({
  mission,
  tree,
  fileIndex,
  relationIndex,
  groupingMode,
  onGroupingModeChange,
  selectedNodeId,
  onSelectNode,
  focusTrigger,
}: FeatureMindMapProps) {
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const [expandedIds, setExpandedIds] = useState<Set<string>>(() => defaultExpandedIds(tree));
  const [searchQuery, setSearchQuery] = useState("");
  const [activeRelations, setActiveRelations] = useState<Set<RelationKind>>(
    () => new Set<RelationKind>(RELATION_KINDS),
  );
  const [hoveredNodeId, setHoveredNodeId] = useState<string | null>(null);
  const [transform, setTransform] = useState<{ x: number; y: number; scale: number }>({
    x: 24,
    y: 24,
    scale: 1,
  });
  const [dragging, setDragging] = useState(false);
  const dragOriginRef = useRef<{
    clientX: number;
    clientY: number;
    startX: number;
    startY: number;
  } | null>(null);

  // Reset expanded state when mission or grouping changes.
  useEffect(() => {
    setExpandedIds(defaultExpandedIds(tree));
    setSearchQuery("");
  }, [tree]);

  // Whenever a node is selected (e.g., jumped to from Inspector), ensure all
  // ancestors are expanded so the target node is guaranteed visible.
  useEffect(() => {
    if (!selectedNodeId || !tree.nodes[selectedNodeId]) return;
    const needed = ancestorsOf(tree, selectedNodeId);
    if (needed.length === 0) return;
    setExpandedIds((prev) => {
      if (needed.every((id) => prev.has(id))) return prev;
      const next = new Set(prev);
      for (const id of needed) next.add(id);
      return next;
    });
  }, [selectedNodeId, tree]);

  const layout = useMemo(
    () =>
      layoutMindMap(tree, {
        expandedIds,
        searchQuery,
        activeRelationKinds: activeRelations,
        relationIndex,
      }),
    [tree, expandedIds, searchQuery, activeRelations, relationIndex],
  );

  const fitToView = useCallback(() => {
    const el = viewportRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return;
    const pad = 28;
    const scaleX = (rect.width - pad * 2) / Math.max(layout.width, 1);
    const scaleY = (rect.height - pad * 2) / Math.max(layout.height, 1);
    const scale = Math.min(1, Math.max(MIN_ZOOM, Math.min(scaleX, scaleY)));
    const x = Math.max(pad, (rect.width - layout.width * scale) / 2);
    const y = Math.max(pad, (rect.height - layout.height * scale) / 2);
    setTransform({ x, y, scale });
  }, [layout.width, layout.height]);

  // Fit on mission/tree change.
  useEffect(() => {
    const raf = window.requestAnimationFrame(() => fitToView());
    return () => window.cancelAnimationFrame(raf);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mission.id, tree.effectiveGrouping]);

  // Center on focused node when triggered explicitly (e.g. clicking a link in Inspector).
  useEffect(() => {
    if (focusTrigger === 0 || !selectedNodeId) return;
    const pos = layout.nodeMap[selectedNodeId];
    const el = viewportRef.current;
    if (!pos || !el) return;
    const rect = el.getBoundingClientRect();
    setTransform((prev) => {
      const scale = Math.max(prev.scale, 0.85);
      const centerX = pos.x + pos.width / 2;
      const centerY = pos.y + pos.height / 2;
      return {
        scale,
        x: rect.width / 2 - centerX * scale,
        y: rect.height / 2 - centerY * scale,
      };
    });
  }, [focusTrigger, selectedNodeId, layout.nodeMap]);

  const activeFocusId = hoveredNodeId ?? selectedNodeId;

  const connectedNodeIds = useMemo(() => {
    const set = new Set<string>();
    if (!activeFocusId) return set;
    set.add(activeFocusId);
    for (const edge of layout.relationEdges) {
      if (edge.sourceId === activeFocusId) set.add(edge.targetId);
      if (edge.targetId === activeFocusId) set.add(edge.sourceId);
    }
    for (const edge of layout.hierarchyEdges) {
      if (edge.sourceId === activeFocusId) set.add(edge.targetId);
      if (edge.targetId === activeFocusId) set.add(edge.sourceId);
    }
    return set;
  }, [activeFocusId, layout.relationEdges, layout.hierarchyEdges]);

  const toggleExpand = useCallback((nodeId: string) => {
    setExpandedIds((prev) => {
      const next = new Set(prev);
      if (next.has(nodeId)) {
        next.delete(nodeId);
      } else {
        next.add(nodeId);
      }
      return next;
    });
  }, []);

  const handleExpandAll = useCallback(() => {
    setExpandedIds(allBranchIds(tree));
  }, [tree]);

  const handleCollapseToBranches = useCallback(() => {
    setExpandedIds(new Set([tree.rootId]));
  }, [tree.rootId]);

  const zoomBy = useCallback((factor: number) => {
    const el = viewportRef.current;
    setTransform((prev) => {
      const nextScale = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, prev.scale * factor));
      if (!el) return { ...prev, scale: nextScale };
      const rect = el.getBoundingClientRect();
      const cx = rect.width / 2;
      const cy = rect.height / 2;
      const ratio = nextScale / prev.scale;
      return {
        scale: nextScale,
        x: cx - (cx - prev.x) * ratio,
        y: cy - (cy - prev.y) * ratio,
      };
    });
  }, []);

  const handleReset = useCallback(() => {
    setTransform({ x: 24, y: 24, scale: 1 });
  }, []);

  const handleWheel = useCallback((event: React.WheelEvent<HTMLDivElement>) => {
    event.preventDefault();
    const el = viewportRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const pointerX = event.clientX - rect.left;
    const pointerY = event.clientY - rect.top;
    const factor = event.deltaY < 0 ? 1.1 : 1 / 1.1;
    setTransform((prev) => {
      const nextScale = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, prev.scale * factor));
      const ratio = nextScale / prev.scale;
      return {
        scale: nextScale,
        x: pointerX - (pointerX - prev.x) * ratio,
        y: pointerY - (pointerY - prev.y) * ratio,
      };
    });
  }, []);

  const handlePointerDown = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) return;
      const target = event.target as HTMLElement;
      if (target.closest("button, input, a, [data-mindmap-node='true']")) return;
      event.currentTarget.setPointerCapture(event.pointerId);
      setDragging(true);
      dragOriginRef.current = {
        clientX: event.clientX,
        clientY: event.clientY,
        startX: transform.x,
        startY: transform.y,
      };
    },
    [transform.x, transform.y],
  );

  const handlePointerMove = useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    const origin = dragOriginRef.current;
    if (!origin) return;
    const dx = event.clientX - origin.clientX;
    const dy = event.clientY - origin.clientY;
    setTransform((prev) => ({
      ...prev,
      x: origin.startX + dx,
      y: origin.startY + dy,
    }));
  }, []);

  const handlePointerUp = useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    if (dragOriginRef.current) {
      dragOriginRef.current = null;
      setDragging(false);
      if (event.currentTarget.hasPointerCapture(event.pointerId)) {
        event.currentTarget.releasePointerCapture(event.pointerId);
      }
    }
  }, []);

  const toggleRelationKind = useCallback((kind: RelationKind) => {
    setActiveRelations((prev) => {
      const next = new Set(prev);
      if (next.has(kind)) next.delete(kind);
      else next.add(kind);
      return next;
    });
  }, []);

  const presentRelationKinds = useMemo(
    () => RELATION_KINDS.filter((kind) => relationIndex.countsByKind[kind] > 0),
    [relationIndex],
  );

  const matchCount = layout.matchedIds.size;

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col rounded-card border border-surface-border bg-surface shadow-card overflow-hidden">
      {/* Top control bar */}
      <div className="flex flex-wrap items-center gap-2 border-b border-surface-border bg-page-canvas px-3 py-2">
        {/* Search */}
        <div className="relative flex min-w-[200px] flex-1 max-w-xs items-center">
          <Search
            size={13}
            className="pointer-events-none absolute left-2.5 text-faint-foreground"
          />
          <input
            type="text"
            value={searchQuery}
            onChange={(event) => setSearchQuery(event.target.value)}
            placeholder="Filter nodes, objectives, files, tags…"
            aria-label="Filter mind map"
            className={cx(
              "h-7 w-full rounded-control border border-surface-border bg-surface pl-7 pr-7 text-caption text-foreground placeholder:text-faint-foreground",
              FOCUS_RING,
            )}
          />
          {searchQuery && (
            <button
              type="button"
              onClick={() => setSearchQuery("")}
              aria-label="Clear search"
              className="absolute right-1.5 grid size-5 place-items-center rounded text-faint-foreground hover:text-foreground"
            >
              <X size={12} />
            </button>
          )}
        </div>
        {searchQuery.trim() !== "" && (
          <span className="tnum rounded-chip bg-info-tint px-2 py-0.5 text-micro font-medium text-info">
            {matchCount} match{matchCount === 1 ? "" : "es"}
          </span>
        )}

        {/* Grouping selector (useful for flat/dense missions) */}
        {!tree.hasExplicitHierarchy && mission.features.length > 12 && (
          <div className="flex items-center gap-1" role="group" aria-label="Branch grouping">
            {(
              [
                ["auto", "Auto branches"],
                ["state", "By state"],
              ] as const
            ).map(([mode, label]) => (
              <button
                key={mode}
                type="button"
                onClick={() => onGroupingModeChange(mode)}
                className={cx(chip(groupingMode === mode), "h-7 px-2.5", FOCUS_RING)}
              >
                {label}
              </button>
            ))}
          </div>
        )}

        {/* Expand / collapse */}
        <div className="flex items-center gap-1">
          <button
            type="button"
            onClick={handleExpandAll}
            className={cx(
              "inline-flex h-7 items-center gap-1 rounded-control bg-surface px-2 text-caption text-muted-foreground shadow-hairline hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING,
            )}
          >
            Expand all
          </button>
          <button
            type="button"
            onClick={handleCollapseToBranches}
            className={cx(
              "inline-flex h-7 items-center gap-1 rounded-control bg-surface px-2 text-caption text-muted-foreground shadow-hairline hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING,
            )}
          >
            Collapse to branches
          </button>
        </div>

        {/* Zoom controls */}
        <div className="ml-auto flex items-center gap-1">
          <button
            type="button"
            onClick={() => zoomBy(1 / 1.2)}
            title="Zoom out"
            aria-label="Zoom out"
            className={cx(
              "grid size-7 place-items-center rounded-control bg-surface text-muted-foreground shadow-hairline hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING,
            )}
          >
            <Minus size={13} />
          </button>
          <span className="tnum w-11 text-center text-micro text-muted-foreground">
            {Math.round(transform.scale * 100)}%
          </span>
          <button
            type="button"
            onClick={() => zoomBy(1.2)}
            title="Zoom in"
            aria-label="Zoom in"
            className={cx(
              "grid size-7 place-items-center rounded-control bg-surface text-muted-foreground shadow-hairline hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING,
            )}
          >
            <Plus size={13} />
          </button>
          <button
            type="button"
            onClick={fitToView}
            title="Fit map to viewport"
            className={cx(
              "inline-flex h-7 items-center gap-1 rounded-control bg-surface px-2 text-caption text-muted-foreground shadow-hairline hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING,
            )}
          >
            <Maximize2 size={12} /> Fit
          </button>
          <button
            type="button"
            onClick={handleReset}
            title="Reset zoom and pan"
            className={cx(
              "inline-flex h-7 items-center gap-1 rounded-control bg-surface px-2 text-caption text-muted-foreground shadow-hairline hover:bg-surface-hover hover:text-foreground",
              FOCUS_RING,
            )}
          >
            <RotateCcw size={12} /> Reset
          </button>
        </div>
      </div>

      {/* Relation-type filter & legend bar (shown when relations exist) */}
      {presentRelationKinds.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5 border-b border-surface-border bg-surface px-3 py-1.5">
          <span className="inline-flex items-center gap-1 text-micro font-medium text-faint-foreground">
            <Link2 size={11} /> Links:
          </span>
          {presentRelationKinds.map((kind) => {
            const meta = RELATION_META[kind];
            const on = activeRelations.has(kind);
            const count = relationIndex.countsByKind[kind];
            return (
              <button
                key={kind}
                type="button"
                onClick={() => toggleRelationKind(kind)}
                aria-pressed={on}
                className={cx(
                  "inline-flex h-6 items-center gap-1.5 rounded-full px-2 text-micro transition-colors",
                  on
                    ? "bg-surface-selected font-medium text-foreground shadow-hairline"
                    : "bg-page-canvas text-faint-foreground hover:text-muted-foreground",
                  FOCUS_RING,
                )}
              >
                <svg width="16" height="8" aria-hidden="true" className="shrink-0">
                  <line
                    x1="1"
                    y1="4"
                    x2="15"
                    y2="4"
                    stroke={meta.strokeVar}
                    strokeWidth="2"
                    strokeDasharray={meta.dashArray}
                    opacity={on ? 1 : 0.35}
                  />
                </svg>
                <span>{meta.label}</span>
                <span className="tnum text-faint-foreground">{count}</span>
              </button>
            );
          })}
          {layout.hiddenRelationCount > 0 && (
            <span className="ml-auto text-micro text-faint-foreground">
              {layout.hiddenRelationCount} link{layout.hiddenRelationCount === 1 ? "" : "s"} in
              collapsed branches
            </span>
          )}
        </div>
      )}

      {/* Interactive pan/zoom canvas */}
      <div
        ref={viewportRef}
        onWheel={handleWheel}
        onPointerDown={handlePointerDown}
        onPointerMove={handlePointerMove}
        onPointerUp={handlePointerUp}
        onPointerCancel={handlePointerUp}
        className={cx(
          "relative min-h-[460px] flex-1 overflow-hidden select-none bg-page-canvas",
          dragging ? "cursor-grabbing" : "cursor-grab",
        )}
        style={{
          backgroundImage:
            "radial-gradient(circle, var(--line) 1px, transparent 1px)",
          backgroundSize: "20px 20px",
          backgroundPosition: `${transform.x % 20}px ${transform.y % 20}px`,
        }}
      >
        <div
          className="absolute top-0 left-0 origin-top-left"
          style={{
            width: layout.width,
            height: layout.height,
            transform: `translate(${transform.x}px, ${transform.y}px) scale(${transform.scale})`,
          }}
        >
          {/* SVG Layer: Hierarchy curves + Typed relation arcs */}
          <svg
            width={layout.width}
            height={layout.height}
            className="pointer-events-none absolute inset-0 overflow-visible"
            aria-hidden="true"
          >
            <defs>
              {RELATION_KINDS.map((kind) => (
                <marker
                  key={kind}
                  id={`arrow-${kind}`}
                  viewBox="0 0 10 10"
                  refX="8.5"
                  refY="5"
                  markerWidth="6"
                  markerHeight="6"
                  orient="auto-start-reverse"
                >
                  <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill={RELATION_META[kind].strokeVar} />
                </marker>
              ))}
            </defs>

            {/* Hierarchy Bezier edges */}
            <g>
              {layout.hierarchyEdges.map((edge) => {
                const highlighted =
                  activeFocusId !== null &&
                  (edge.sourceId === activeFocusId || edge.targetId === activeFocusId);
                return (
                  <path
                    key={edge.id}
                    d={edge.path}
                    fill="none"
                    stroke={highlighted ? "var(--accent-ink)" : "var(--line-strong)"}
                    strokeWidth={highlighted ? 2.25 : 1.5}
                    opacity={activeFocusId && !highlighted ? 0.45 : 0.9}
                  />
                );
              })}
            </g>

            {/* Cross-cutting typed relation edges */}
            <g>
              {layout.relationEdges.map((edge) => (
                <RelationArc
                  key={edge.id}
                  edge={edge}
                  activeFocusId={activeFocusId}
                />
              ))}
            </g>
          </svg>

          {/* HTML Node Layer */}
          {layout.nodes.map((positioned) => (
            <MindMapNodeCard
              key={positioned.id}
              positioned={positioned}
              selected={selectedNodeId === positioned.id}
              connected={activeFocusId !== null && connectedNodeIds.has(positioned.id)}
              hasFocus={activeFocusId !== null}
              fileIndex={fileIndex}
              relationIndex={relationIndex}
              onSelect={() => onSelectNode(positioned.id)}
              onToggleExpand={() => toggleExpand(positioned.id)}
              onHoverChange={(hovering) =>
                setHoveredNodeId(hovering ? positioned.id : null)
              }
            />
          ))}
        </div>

        {/* Bottom-left legend & status pill */}
        <div className="pointer-events-none absolute bottom-3 left-3 flex flex-wrap items-center gap-2 rounded-control border border-surface-border bg-surface/95 px-2.5 py-1.5 text-micro text-muted-foreground shadow-card backdrop-blur-xs">
          {STATES.map((state) => (
            <span key={state} className="inline-flex items-center gap-1">
              <span className={cx("size-2 rounded-full", STATE_INFO[state].dot)} />
              <span>{STATE_INFO[state].label}</span>
            </span>
          ))}
          <span className="text-faint-foreground">·</span>
          <span className="tnum">
            {layout.totalVisibleFeatures}/{mission.features.length} visible
          </span>
          <span className="hidden text-faint-foreground sm:inline">
            · Drag to pan · Scroll to zoom · Click node to inspect
          </span>
        </div>
      </div>
    </div>
  );
}

function RelationArc({
  edge,
  activeFocusId,
}: {
  edge: MindMapRelationEdge;
  activeFocusId: string | null;
}) {
  const meta = RELATION_META[edge.kind];
  const connected =
    activeFocusId !== null &&
    (edge.sourceId === activeFocusId || edge.targetId === activeFocusId);
  const dimmed = activeFocusId !== null && !connected;
  const labelText = edge.label ? `${meta.label}: ${edge.label}` : meta.label;

  return (
    <g opacity={dimmed ? 0.16 : connected ? 1 : 0.72}>
      <path
        d={edge.path}
        fill="none"
        stroke={meta.strokeVar}
        strokeWidth={connected ? 2.25 : 1.5}
        strokeDasharray={meta.dashArray}
        markerEnd={`url(#arrow-${edge.kind})`}
      />
      {connected && (
        <g transform={`translate(${edge.labelX}, ${edge.labelY})`}>
          <rect
            x={-Math.min(110, labelText.length * 3.4 + 10)}
            y={-10}
            width={Math.min(220, labelText.length * 6.8 + 20)}
            height={19}
            rx={6}
            fill="var(--surface)"
            stroke={meta.strokeVar}
            strokeWidth={1}
          />
          <text
            textAnchor="middle"
            dominantBaseline="central"
            fill="var(--foreground)"
            fontSize="10.5"
            fontFamily="var(--font-sans)"
          >
            {labelText.length > 30 ? `${labelText.slice(0, 29)}…` : labelText}
          </text>
        </g>
      )}
    </g>
  );
}

function MindMapNodeCard({
  positioned,
  selected,
  connected,
  hasFocus,
  fileIndex,
  relationIndex,
  onSelect,
  onToggleExpand,
  onHoverChange,
}: {
  positioned: PositionedMindMapNode;
  selected: boolean;
  connected: boolean;
  hasFocus: boolean;
  fileIndex: FileIndex;
  relationIndex: RelationIndex;
  onSelect: () => void;
  onToggleExpand: () => void;
  onHoverChange: (hovering: boolean) => void;
}) {
  const { node } = positioned;
  const feature = node.feature;

  const relationCount = feature
    ? (relationIndex.outgoing[feature.id]?.length ?? 0) +
      (relationIndex.incoming[feature.id]?.length ?? 0)
    : 0;

  const sharedFileCount = feature
    ? feature.files.filter((file) => (fileIndex.sharedFiles[file]?.length ?? 0) > 1).length
    : 0;

  const dimmed = positioned.isDimmed || (hasFocus && !connected && !selected);

  return (
    <div
      data-mindmap-node="true"
      onMouseEnter={() => onHoverChange(true)}
      onMouseLeave={() => onHoverChange(false)}
      style={{
        left: positioned.x,
        top: positioned.y,
        width: positioned.width,
        height: positioned.height,
      }}
      className={cx(
        "absolute flex items-stretch rounded-card border bg-surface shadow-card transition-[box-shadow,opacity,border-color]",
        selected
          ? "border-accent-ink ring-2 ring-accent-ink/30 shadow-raised z-20"
          : positioned.isMatch
            ? "border-info ring-2 ring-info/30 z-10"
            : connected
              ? "border-line-strong shadow-raised z-10"
              : "border-surface-border hover:border-line-strong",
        dimmed && "opacity-45",
      )}
    >
      {/* Clickable node body */}
      <button
        type="button"
        onClick={onSelect}
        className={cx(
          "flex min-w-0 flex-1 flex-col justify-between px-2.5 py-2 text-left rounded-l-card",
          !positioned.hasChildren && "rounded-r-card",
          FOCUS_RING,
        )}
      >
        {node.nodeKind === "feature" && feature ? (
          <>
            <div className="flex min-w-0 items-center gap-1.5">
              <span
                className={cx(
                  "inline-flex shrink-0 items-center gap-1 rounded-chip px-1.5 py-px text-micro font-medium ring-1 ring-inset",
                  STATE_INFO[feature.state].badge,
                )}
              >
                <span className={cx("size-1.5 rounded-full", STATE_INFO[feature.state].dot)} />
                {STATE_INFO[feature.state].label}
              </span>
              {feature.kind !== "capability" && (
                <span className="truncate font-mono text-micro text-faint-foreground">
                  {feature.kind}
                </span>
              )}
              <span className="ml-auto flex shrink-0 items-center gap-1">
                {feature.blocked && (
                  <span
                    title="Blocked"
                    className="inline-flex items-center text-destructive"
                  >
                    <OctagonAlert size={11} />
                  </span>
                )}
                {feature.gaps.length > 0 && (
                  <span
                    title={`${feature.gaps.length} gap(s)`}
                    className="tnum rounded-chip bg-wait-tint px-1 text-micro text-warning"
                  >
                    {feature.gaps.length}g
                  </span>
                )}
              </span>
            </div>

            <div
              className="truncate text-label font-semibold text-foreground"
              title={feature.title}
            >
              {feature.title}
            </div>

            <div className="flex min-w-0 items-center gap-2 text-micro text-faint-foreground">
              <span className="truncate font-mono">{feature.id}</span>
              <span className="ml-auto flex shrink-0 items-center gap-1.5">
                {feature.objectives.length > 0 && (
                  <span className="tnum">{feature.objectives.length} obj</span>
                )}
                {relationCount > 0 && (
                  <span
                    className="tnum inline-flex items-center gap-0.5 text-info"
                    title={`${relationCount} typed link(s)`}
                  >
                    <Link2 size={10} />
                    {relationCount}
                  </span>
                )}
                {sharedFileCount > 0 && (
                  <span
                    className="tnum inline-flex items-center gap-0.5 text-muted-foreground"
                    title={`${sharedFileCount} shared file(s)`}
                  >
                    <FileCode size={10} />
                    {sharedFileCount}
                  </span>
                )}
              </span>
            </div>
          </>
        ) : (
          <>
            <div className="flex items-center gap-1.5">
              {node.clusterState && (
                <span
                  className={cx("size-2 shrink-0 rounded-full", STATE_INFO[node.clusterState].dot)}
                />
              )}
              <span className="truncate text-label font-semibold text-foreground" title={node.title}>
                {node.title}
              </span>
            </div>
            <div className="flex items-center gap-2 text-micro text-muted-foreground">
              <span className="tnum font-medium">{node.totalFeatures} cap</span>
              <span className="flex items-center gap-1.5">
                {STATES.map((state) =>
                  node.stateCounts[state] > 0 ? (
                    <span
                      key={state}
                      title={`${node.stateCounts[state]} ${STATE_INFO[state].label}`}
                      className="tnum inline-flex items-center gap-0.5"
                    >
                      <span className={cx("size-1.5 rounded-full", STATE_INFO[state].dot)} />
                      {node.stateCounts[state]}
                    </span>
                  ) : null,
                )}
              </span>
            </div>
          </>
        )}
      </button>

      {/* Expand / Collapse button on right edge */}
      {positioned.hasChildren && (
        <button
          type="button"
          onClick={(event) => {
            event.stopPropagation();
            onToggleExpand();
          }}
          aria-expanded={positioned.expanded}
          aria-label={
            positioned.expanded
              ? `Collapse ${node.title}`
              : `Expand ${node.title} (+${positioned.collapsedCount})`
          }
          title={
            positioned.expanded
              ? "Collapse branch"
              : `Expand branch (+${positioned.collapsedCount} nodes)`
          }
          className={cx(
            "flex w-9 shrink-0 flex-col items-center justify-center gap-0.5 rounded-r-card border-l border-surface-border bg-page-canvas/70 text-muted-foreground transition-colors hover:bg-surface-hover hover:text-foreground",
            FOCUS_RING,
          )}
        >
          {positioned.expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
          {!positioned.expanded && positioned.collapsedCount > 0 && (
            <span className="tnum rounded-chip bg-surface-selected px-1 text-[10px] font-medium text-foreground">
              +{positioned.collapsedCount}
            </span>
          )}
        </button>
      )}
    </div>
  );
}

import {
  Check,
  Copy,
  GitBranch,
  GitCommitHorizontal,
  GitMerge,
  History,
  RefreshCw,
  Search,
  ShieldCheck,
  Terminal,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { usePoll } from "../../api";
import { relativeTime } from "../../format";
import {
  buildGraphLayout,
  collectLineageIds,
  fallbackRecoveryCommands,
  historyActionMeta,
  refKindMeta,
  shortOid,
  type StorageHistoryEntry,
  type StorageRecoveryCommand,
  type StorageSnapshotDetail,
  type StorageTreeNode,
  type StorageTreeResponse,
} from "../../storageTree.ts";
import { fetchStorageSnapshot, fetchStorageTree } from "../../storageTreeApi";
import { Fact, SectionLabel } from "../fleet/bits";
import { Button, Card, CardHeader, Drawer, EmptyState, LiveStatus, PageHeader, cx } from "../ui";
import { Badge, type BadgeTone } from "../ui/badge";

const POLL_MS = 10_000;
const DEFAULT_LIMIT = 120;

type NodeFilter = "all" | "main" | "merges" | "branches";

const LANE_COLORS = [
  "var(--success)",
  "var(--chart-1)",
  "var(--info)",
  "var(--warning)",
  "var(--brand)",
] as const;

function laneStroke(lane: number, isMergeEdge: boolean): string {
  if (isMergeEdge) return "var(--info)";
  if (lane === 0) return LANE_COLORS[0];
  return LANE_COLORS[1 + ((lane - 1) % (LANE_COLORS.length - 1))];
}

function backendTone(backend: string): BadgeTone {
  if (backend === "d6") return "success";
  if (backend === "cow" || backend === "overlay") return "info";
  return "neutral";
}

function riskBadgeMeta(risk: StorageRecoveryCommand["risk"]): {
  label: string;
  tone: BadgeTone;
} {
  switch (risk) {
    case "read_only":
      return { label: "Read-only", tone: "success" };
    case "branch":
      return { label: "New branch", tone: "info" };
    case "rollback":
      return { label: "Rollback", tone: "danger" };
  }
}

function formatUtc(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "—";
  return `${new Date(ms).toISOString().replace("T", " ").slice(0, 19)} UTC`;
}

export function StorageTreeView() {
  const [selectedProject, setSelectedProject] = useState<string | null>(null);
  const [refreshNonce, setRefreshNonce] = useState(0);
  const [filter, setFilter] = useState<NodeFilter>("all");
  const [query, setQuery] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const pollKey = `storage-tree:${selectedProject ?? "auto"}:${refreshNonce}`;
  const treePoll = usePoll<StorageTreeResponse>(
    pollKey,
    (signal) => fetchStorageTree(selectedProject, DEFAULT_LIMIT, signal),
    POLL_MS,
  );
  const data = treePoll.data;

  const currentProject = selectedProject ?? data?.active_project ?? null;
  const layout = useMemo(
    () => (data ? buildGraphLayout(data.nodes, data.refs) : null),
    [data],
  );

  const selectedNode = useMemo(
    () => data?.nodes.find((node) => node.snapshot_id === selectedId) ?? null,
    [data, selectedId],
  );

  const activeLineage = useMemo(
    () => (data ? collectLineageIds(data.nodes, selectedId) : new Set<string>()),
    [data, selectedId],
  );

  const filteredLayoutNodes = useMemo(() => {
    if (!layout) return [];
    const needle = query.trim().toLowerCase();
    return layout.nodes.filter((item) => {
      if (filter === "main" && !item.isMainLane) return false;
      if (filter === "merges" && !item.isMerge) return false;
      if (filter === "branches" && item.isMainLane) return false;
      if (!needle) return true;
      return (
        item.id.toLowerCase().includes(needle) ||
        item.node.git_oid.toLowerCase().includes(needle) ||
        item.node.tree_oid.toLowerCase().includes(needle) ||
        item.node.refs.some((r) => r.name.toLowerCase().includes(needle))
      );
    });
  }, [layout, filter, query]);

  const showFullGraph = filter === "all" && query.trim() === "";

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <div className="flex min-w-0 items-center gap-2">
            <h1 className="truncate text-body font-semibold text-foreground">
              Storage tree
            </h1>
            {data && (
              <Badge tone={backendTone(data.summary.backend)} dot>
                {data.summary.backend.toUpperCase()}
              </Badge>
            )}
          </div>
        }
      >
        {data && data.projects.length > 0 && (
          <label className="flex items-center gap-1.5 text-caption text-muted-foreground">
            <span className="hidden sm:inline">Project</span>
            <select
              aria-label="Storage project"
              value={currentProject ?? ""}
              onChange={(event) => {
                const next = event.target.value;
                setSelectedProject(next || null);
                setSelectedId(null);
              }}
              className="h-7 rounded-control border border-surface-border bg-surface px-2 text-caption text-foreground focus-visible:outline-2 focus-visible:outline-ring"
            >
              {data.projects.map((proj) => (
                <option key={proj} value={proj}>
                  {proj}
                </option>
              ))}
            </select>
          </label>
        )}
        <Button
          onClick={() => setRefreshNonce((n) => n + 1)}
          title="Refresh storage evolution DAG"
        >
          <RefreshCw size={12} />
          <span className="hidden sm:inline">Refresh</span>
        </Button>
        <LiveStatus
          error={treePoll.error}
          label={
            data
              ? `${data.summary.total_snapshots} snap${data.summary.total_snapshots === 1 ? "" : "s"} · ${data.summary.active_refs} ref${data.summary.active_refs === 1 ? "" : "s"}`
              : "Connecting"
          }
        />
      </PageHeader>

      {!data ? (
        <div className="grid flex-1 place-items-center p-4">
          <p className="text-caption text-muted-foreground">
            {treePoll.error
              ? `Could not reach /api/storage/tree: ${treePoll.error}`
              : "Loading D6 storage evolution tree…"}
          </p>
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto flex max-w-[1440px] flex-col gap-4 p-3 sm:p-4">
            <SummaryStrip data={data} />

            {data.nodes.length === 0 ? (
              <Card>
                <EmptyState
                  icon={<GitBranch size={20} />}
                  title={
                    data.summary.backend === "d6"
                      ? "No D6 snapshots recorded yet"
                      : `Storage tree unavailable in ${data.summary.backend} mode`
                  }
                  hint={
                    <div className="mt-1 flex max-w-xl flex-col items-center gap-2 text-caption text-muted-foreground">
                      <p>{data.summary.mode_note}</p>
                      <code className="rounded bg-muted px-2 py-1 font-mono text-micro text-foreground select-all">
                        spawn storage cutover --status --json
                      </code>
                    </div>
                  }
                />
              </Card>
            ) : (
              <div className="grid grid-cols-1 items-start gap-4 xl:grid-cols-[minmax(0,1fr)_360px]">
                <Card className="min-w-0 overflow-hidden">
                  <CardHeader
                    icon={<GitMerge size={14} />}
                    title="Evolution DAG & Time Machine"
                    meta={
                      <span className="tnum">
                        {filteredLayoutNodes.length}/{data.nodes.length} snapshots
                        {data.summary.truncated ? " (capped)" : ""}
                      </span>
                    }
                  />

                  <div className="flex flex-wrap items-center gap-2 border-b border-surface-border px-3 py-2.5 sm:px-4">
                    <div
                      role="group"
                      aria-label="Filter snapshots"
                      className="inline-flex rounded-md border border-surface-border bg-page-canvas p-0.5"
                    >
                      {(
                        [
                          ["all", "All"],
                          ["main", "Main"],
                          ["merges", "Merges"],
                          ["branches", "Branches"],
                        ] as const
                      ).map(([value, label]) => (
                        <button
                          key={value}
                          type="button"
                          onClick={() => setFilter(value)}
                          className={cx(
                            "rounded px-2.5 py-1 text-caption font-medium transition-colors",
                            filter === value
                              ? "bg-surface-selected text-foreground shadow-hairline"
                              : "text-muted-foreground hover:text-foreground",
                          )}
                        >
                          {label}
                        </button>
                      ))}
                    </div>

                    <div className="relative ml-auto min-w-[180px] flex-1 sm:max-w-xs">
                      <Search
                        size={13}
                        aria-hidden="true"
                        className="pointer-events-none absolute top-1/2 left-2.5 -translate-y-1/2 text-faint-foreground"
                      />
                      <input
                        type="search"
                        value={query}
                        onChange={(event) => setQuery(event.target.value)}
                        placeholder="Filter OID, tree, or ref…"
                        aria-label="Filter snapshots by OID or ref"
                        className="h-7 w-full rounded-control border border-surface-border bg-page-canvas pr-2.5 pl-7 text-caption text-foreground placeholder:text-muted-foreground focus-visible:outline-2 focus-visible:outline-ring"
                      />
                    </div>
                  </div>

                  {filteredLayoutNodes.length === 0 ? (
                    <EmptyState
                      icon={<Search size={18} />}
                      title="No snapshots match the current filter"
                      hint="Clear the search text or switch back to All snapshots."
                    />
                  ) : showFullGraph && layout ? (
                    <DagTimeline
                      layout={layout}
                      selectedId={selectedId}
                      activeLineage={activeLineage}
                      onSelect={setSelectedId}
                    />
                  ) : (
                    <FilteredSnapshotList
                      nodes={filteredLayoutNodes.map((item) => item.node)}
                      selectedId={selectedId}
                      onSelect={setSelectedId}
                    />
                  )}
                </Card>

                <div className="flex min-w-0 flex-col gap-4">
                  <ActiveRefsCard
                    data={data}
                    selectedId={selectedId}
                    onSelect={setSelectedId}
                  />
                  <RefLogCard
                    history={data.history}
                    selectedId={selectedId}
                    onSelect={setSelectedId}
                  />
                </div>
              </div>
            )}
          </div>
        </div>
      )}

      {selectedNode && (
        <SnapshotDrawer
          node={selectedNode}
          project={currentProject}
          onSelectSnapshot={setSelectedId}
          onClose={() => setSelectedId(null)}
        />
      )}
    </main>
  );
}

function SummaryStrip({ data }: { data: StorageTreeResponse }) {
  const merges = data.nodes.filter((n) => n.is_merge).length;
  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
      <Card className="p-3.5">
        <SectionLabel>Backend mode</SectionLabel>
        <div className="mt-1.5 flex items-center gap-2">
          <span className="text-body font-semibold text-foreground uppercase">
            {data.summary.backend}
          </span>
          <Badge tone={backendTone(data.summary.backend)} dot>
            {data.active_project ?? "none"}
          </Badge>
        </div>
        <p className="mt-1 line-clamp-2 text-caption text-muted-foreground">
          {data.summary.mode_note}
        </p>
      </Card>

      <Card className="p-3.5">
        <SectionLabel>Snapshots in DAG</SectionLabel>
        <div className="tnum mt-1.5 text-lg font-semibold text-foreground">
          {data.summary.total_snapshots}
        </div>
        <p className="mt-1 text-caption text-muted-foreground">
          {data.summary.live_snapshots} live · {data.summary.collected_snapshots} collected
        </p>
      </Card>

      <Card className="p-3.5">
        <SectionLabel>Active refs</SectionLabel>
        <div className="tnum mt-1.5 text-lg font-semibold text-foreground">
          {data.summary.active_refs}
        </div>
        <p className="mt-1 text-caption text-muted-foreground">
          {merges} merge node{merges === 1 ? "" : "s"} in view
        </p>
      </Card>

      <Card className="p-3.5">
        <SectionLabel>Ref log events</SectionLabel>
        <div className="tnum mt-1.5 text-lg font-semibold text-foreground">
          {data.summary.history_entries}
        </div>
        <p className="mt-1 text-caption text-muted-foreground">
          Append-only transition ledger
        </p>
      </Card>
    </div>
  );
}

function DagTimeline({
  layout,
  selectedId,
  activeLineage,
  onSelect,
}: {
  layout: NonNullable<ReturnType<typeof buildGraphLayout>>;
  selectedId: string | null;
  activeLineage: Set<string>;
  onSelect: (id: string) => void;
}) {
  const hasSelection = selectedId !== null && activeLineage.size > 0;
  return (
    <div className="overflow-x-auto">
      <div className="relative flex min-w-[540px]">
        <svg
          width={layout.width}
          height={layout.height}
          viewBox={`0 0 ${layout.width} ${layout.height}`}
          aria-label="D6 snapshot evolution graph"
          className="shrink-0 select-none"
        >
          {layout.edges.map((edge) => {
            const highlighted =
              !hasSelection ||
              (activeLineage.has(edge.fromId) &&
                (activeLineage.has(edge.toId) || edge.isDanglingParent));
            const laneForColor = edge.isMergeEdge ? edge.toLane : edge.fromLane;
            return (
              <path
                key={edge.id}
                d={edge.path}
                fill="none"
                stroke={laneStroke(laneForColor, edge.isMergeEdge)}
                strokeWidth={edge.isMainEdge ? 2.5 : 2}
                strokeDasharray={edge.isDanglingParent ? "3 3" : undefined}
                strokeLinecap="round"
                opacity={highlighted ? 0.92 : 0.25}
              />
            );
          })}

          {layout.nodes.map((item) => {
            const isSelected = item.id === selectedId;
            const inLineage = !hasSelection || activeLineage.has(item.id);
            const color = laneStroke(item.lane, false);
            return (
              <g
                key={item.id}
                onClick={() => onSelect(item.id)}
                className="cursor-pointer"
                opacity={inLineage ? 1 : 0.35}
              >
                {isSelected && (
                  <circle
                    cx={item.x}
                    cy={item.y}
                    r={11}
                    fill="none"
                    stroke="var(--ring)"
                    strokeWidth={2}
                  />
                )}
                {item.isMerge && (
                  <circle
                    cx={item.x}
                    cy={item.y}
                    r={8}
                    fill="var(--surface)"
                    stroke="var(--info)"
                    strokeWidth={2}
                  />
                )}
                <circle
                  cx={item.x}
                  cy={item.y}
                  r={item.isMerge ? 4.5 : item.isMainLane ? 6 : 5}
                  fill={color}
                  stroke="var(--surface)"
                  strokeWidth={1.5}
                />
              </g>
            );
          })}
        </svg>

        <div className="min-w-0 flex-1 divide-y divide-surface-border">
          {layout.nodes.map((item) => (
            <SnapshotRow
              key={item.id}
              node={item.node}
              isMainLane={item.isMainLane}
              selected={item.id === selectedId}
              dimmed={hasSelection && !activeLineage.has(item.id)}
              height={layout.rowHeight}
              onSelect={onSelect}
            />
          ))}
        </div>
      </div>
    </div>
  );
}

function FilteredSnapshotList({
  nodes,
  selectedId,
  onSelect,
}: {
  nodes: StorageTreeNode[];
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  return (
    <div className="divide-y divide-surface-border">
      {nodes.map((node) => (
        <SnapshotRow
          key={node.snapshot_id}
          node={node}
          isMainLane={node.refs.some((r) => r.kind === "main")}
          selected={node.snapshot_id === selectedId}
          dimmed={false}
          height={64}
          onSelect={onSelect}
        />
      ))}
    </div>
  );
}

function SnapshotRow({
  node,
  isMainLane,
  selected,
  dimmed,
  height,
  onSelect,
}: {
  node: StorageTreeNode;
  isMainLane: boolean;
  selected: boolean;
  dimmed: boolean;
  height: number;
  onSelect: (id: string) => void;
}) {
  return (
    <button
      type="button"
      onClick={() => onSelect(node.snapshot_id)}
      style={{ height }}
      className={cx(
        "flex w-full items-center justify-between gap-3 px-3 text-left transition-colors hover:bg-surface-hover focus-visible:outline-2 focus-visible:outline-ring sm:px-4",
        selected && "bg-surface-selected",
        dimmed && "opacity-55",
      )}
    >
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="font-mono text-caption font-semibold text-foreground">
            {shortOid(node.snapshot_id, 10)}
          </span>
          {node.git_oid && (
            <span className="font-mono text-micro text-muted-foreground">
              git:{shortOid(node.git_oid, 7)}
            </span>
          )}
          {node.is_merge && (
            <Badge tone="info" icon={<GitMerge size={11} />}>
              Merge ({node.parent_ids.length}p)
            </Badge>
          )}
          {isMainLane && !node.is_merge && node.refs.length === 0 && (
            <Badge tone="neutral">spine</Badge>
          )}
          {node.state !== "live" && (
            <Badge tone="warning">{node.state}</Badge>
          )}
          {node.refs.map((r) => {
            const meta = refKindMeta(r.kind);
            return (
              <Badge key={r.name} tone={meta.tone} dot>
                {r.name}
              </Badge>
            );
          })}
        </div>
        <div className="mt-0.5 flex items-center gap-2 text-micro text-muted-foreground">
          <span className="truncate font-mono">
            tree {shortOid(node.tree_oid, 8)}
          </span>
          <span aria-hidden="true">·</span>
          <span>
            {node.parent_ids.length === 0
              ? "root"
              : `${node.parent_ids.length} parent${node.parent_ids.length === 1 ? "" : "s"}`}
          </span>
        </div>
      </div>

      <div className="shrink-0 text-right">
        <div
          title={formatUtc(node.created_at_ms)}
          className="tnum text-caption text-muted-foreground"
        >
          {relativeTime(node.created_at_ms)}
        </div>
      </div>
    </button>
  );
}

function ActiveRefsCard({
  data,
  selectedId,
  onSelect,
}: {
  data: StorageTreeResponse;
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  return (
    <Card className="overflow-hidden">
      <CardHeader
        icon={<GitBranch size={14} />}
        title="Active refs"
        meta={<span className="tnum">{data.refs.length}</span>}
      />
      {data.refs.length === 0 ? (
        <EmptyState
          title="No active refs"
          hint="Refs appear once a D6 main or worker branch is recorded."
        />
      ) : (
        <div className="divide-y divide-surface-border">
          {data.refs.map((r) => {
            const meta = refKindMeta(r.kind);
            const active = r.snapshot_id === selectedId;
            return (
              <button
                key={r.name}
                type="button"
                onClick={() => onSelect(r.snapshot_id)}
                className={cx(
                  "flex w-full items-center justify-between gap-2 px-3.5 py-2.5 text-left transition-colors hover:bg-surface-hover",
                  active && "bg-surface-selected",
                )}
              >
                <div className="min-w-0">
                  <div className="flex items-center gap-1.5">
                    <Badge tone={meta.tone} dot>
                      {meta.label}
                    </Badge>
                    <span className="truncate font-mono text-caption font-medium text-foreground">
                      {r.name}
                    </span>
                  </div>
                </div>
                <span className="tnum shrink-0 font-mono text-micro text-muted-foreground">
                  {shortOid(r.snapshot_id, 8)}
                </span>
              </button>
            );
          })}
        </div>
      )}
    </Card>
  );
}

function RefLogCard({
  history,
  selectedId,
  onSelect,
}: {
  history: StorageHistoryEntry[];
  selectedId: string | null;
  onSelect: (id: string) => void;
}) {
  return (
    <Card className="overflow-hidden">
      <CardHeader
        icon={<History size={14} />}
        title="Ref log evolution"
        meta={<span className="tnum">{history.length} recent</span>}
      />
      {history.length === 0 ? (
        <EmptyState title="No ref transitions yet" />
      ) : (
        <div className="max-h-[420px] divide-y divide-surface-border overflow-y-auto">
          {history.map((entry) => {
            const meta = historyActionMeta(entry.action);
            const active = entry.new_snapshot === selectedId;
            return (
              <button
                key={entry.seq}
                type="button"
                onClick={() => onSelect(entry.new_snapshot)}
                className={cx(
                  "flex w-full flex-col gap-1 px-3.5 py-2.5 text-left transition-colors hover:bg-surface-hover",
                  active && "bg-surface-selected",
                )}
              >
                <div className="flex items-center justify-between gap-2">
                  <div className="flex min-w-0 items-center gap-1.5">
                    <Badge tone={meta.tone}>{meta.label}</Badge>
                    <span className="truncate font-mono text-caption font-medium text-foreground">
                      {entry.name}
                    </span>
                  </div>
                  <span
                    title={formatUtc(entry.at_ms)}
                    className="tnum shrink-0 text-micro text-muted-foreground"
                  >
                    #{entry.seq} · {relativeTime(entry.at_ms)}
                  </span>
                </div>
                <div className="flex items-center gap-1.5 font-mono text-micro text-muted-foreground">
                  <span>{shortOid(entry.old_snapshot, 7)}</span>
                  <span aria-hidden="true">→</span>
                  <span className="text-foreground">{shortOid(entry.new_snapshot, 7)}</span>
                </div>
                {entry.reason && (
                  <p className="truncate text-caption text-muted-foreground">
                    {entry.reason}
                  </p>
                )}
              </button>
            );
          })}
        </div>
      )}
    </Card>
  );
}

function SnapshotDrawer({
  node,
  project,
  onSelectSnapshot,
  onClose,
}: {
  node: StorageTreeNode;
  project: string | null;
  onSelectSnapshot: (id: string) => void;
  onClose: () => void;
}) {
  const [detail, setDetail] = useState<StorageSnapshotDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [copiedId, setCopiedId] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError(null);
    fetchStorageSnapshot(node.snapshot_id, controller.signal)
      .then((res) => {
        if (!controller.signal.aborted) {
          setDetail(res);
          setLoading(false);
        }
      })
      .catch((err: unknown) => {
        if (!controller.signal.aborted) {
          setError(err instanceof Error ? err.message : String(err));
          setLoading(false);
        }
      });
    return () => controller.abort();
  }, [node.snapshot_id]);

  const snap = detail?.snapshot ?? { ...node, project: project ?? "" };
  const commands =
    detail && detail.recovery_commands.length > 0
      ? detail.recovery_commands
      : fallbackRecoveryCommands(node, project);
  const refs = detail ? detail.refs : node.refs;

  const copyCommand = async (id: string, command: string) => {
    try {
      await navigator.clipboard.writeText(command);
      setCopiedId(id);
      window.setTimeout(() => {
        setCopiedId((cur) => (cur === id ? null : cur));
      }, 1500);
    } catch {
      setCopiedId(null);
    }
  };

  return (
    <Drawer
      onClose={onClose}
      header={
        <div className="flex min-w-0 items-center gap-2">
          <GitCommitHorizontal size={16} className="shrink-0 text-muted-foreground" />
          <span className="truncate font-mono text-body font-semibold text-foreground">
            Snapshot {shortOid(snap.snapshot_id, 12)}
          </span>
          {snap.is_merge && (
            <Badge tone="info" icon={<GitMerge size={11} />}>
              Merge
            </Badge>
          )}
          <Badge tone={snap.state === "live" ? "success" : "warning"}>
            {snap.state}
          </Badge>
        </div>
      }
    >
      <div className="min-h-0 flex-1 overflow-y-auto p-4 sm:p-5">
        <div className="flex flex-col gap-5">
          {error && (
            <div className="rounded-md border border-destructive/30 bg-fail-tint px-3 py-2 text-caption text-destructive">
              Could not load extended snapshot lineage: {error}
            </div>
          )}

          <Card className="p-4">
            <SectionLabel
              trailing={
                <span className="tnum text-micro text-muted-foreground">
                  {formatUtc(snap.created_at_ms)}
                </span>
              }
            >
              Merkle Identity
            </SectionLabel>
            <div className="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-2">
              <Fact label="Snapshot ID" mono>
                <span className="select-all">{snap.snapshot_id}</span>
              </Fact>
              <Fact label="Projected Git OID" mono>
                <span className="select-all">{snap.git_oid || "—"}</span>
              </Fact>
              <Fact label="Tree OID" mono>
                <span className="select-all">{snap.tree_oid || "—"}</span>
              </Fact>
              <Fact label="Project">
                {snap.project || project || "—"}
              </Fact>
            </div>
            {refs.length > 0 && (
              <div className="mt-3 flex flex-wrap items-center gap-1.5 border-t border-surface-border pt-3">
                <span className="text-micro font-medium text-muted-foreground">
                  Attached refs:
                </span>
                {refs.map((r) => {
                  const meta = refKindMeta(r.kind);
                  return (
                    <Badge key={r.name} tone={meta.tone} dot>
                      {r.name}
                    </Badge>
                  );
                })}
              </div>
            )}
          </Card>

          <Card className="p-4">
            <SectionLabel
              trailing={
                loading ? (
                  <span className="text-micro text-muted-foreground">Loading lineage…</span>
                ) : null
              }
            >
              Lineage (Parents &amp; Children)
            </SectionLabel>
            <div className="mt-3 grid grid-cols-1 gap-4 sm:grid-cols-2">
              <div>
                <div className="text-caption font-medium text-foreground">
                  Parents ({snap.parent_ids.length})
                </div>
                {snap.parent_ids.length === 0 ? (
                  <p className="mt-1 text-caption text-muted-foreground">
                    Initial root snapshot
                  </p>
                ) : (
                  <div className="mt-1.5 flex flex-wrap gap-1.5">
                    {snap.parent_ids.map((pid, idx) => (
                      <button
                        key={pid}
                        type="button"
                        onClick={() => onSelectSnapshot(pid)}
                        className="inline-flex items-center gap-1 rounded border border-surface-border bg-page-canvas px-2 py-1 font-mono text-micro text-foreground hover:bg-surface-hover"
                      >
                        <span className="text-muted-foreground">p{idx + 1}:</span>
                        {shortOid(pid, 10)}
                      </button>
                    ))}
                  </div>
                )}
              </div>

              <div>
                <div className="text-caption font-medium text-foreground">
                  Children ({detail?.children.length ?? 0})
                </div>
                {!detail || detail.children.length === 0 ? (
                  <p className="mt-1 text-caption text-muted-foreground">
                    No descendant snapshots in view
                  </p>
                ) : (
                  <div className="mt-1.5 flex flex-wrap gap-1.5">
                    {detail.children.map((child) => (
                      <button
                        key={child.snapshot_id}
                        type="button"
                        onClick={() => onSelectSnapshot(child.snapshot_id)}
                        className="inline-flex items-center gap-1 rounded border border-surface-border bg-page-canvas px-2 py-1 font-mono text-micro text-foreground hover:bg-surface-hover"
                      >
                        {shortOid(child.snapshot_id, 10)}
                      </button>
                    ))}
                  </div>
                )}
              </div>
            </div>

            {detail && detail.ancestors.length > 0 && (
              <div className="mt-4 border-t border-surface-border pt-3">
                <div className="text-caption font-medium text-foreground">
                  Ancestor chain ({detail.ancestors.length})
                </div>
                <div className="mt-2 flex flex-wrap gap-1.5">
                  {detail.ancestors.slice(0, 12).map((anc) => (
                    <button
                      key={anc.snapshot_id}
                      type="button"
                      onClick={() => onSelectSnapshot(anc.snapshot_id)}
                      className="inline-flex items-center gap-1 rounded bg-muted px-2 py-0.5 font-mono text-micro text-foreground hover:bg-surface-hover"
                    >
                      <span className="text-muted-foreground">-{anc.depth}</span>
                      {shortOid(anc.snapshot_id, 8)}
                    </button>
                  ))}
                </div>
              </div>
            )}
          </Card>

          <Card className="p-4">
            <SectionLabel
              trailing={
                <span className="inline-flex items-center gap-1 text-micro text-muted-foreground">
                  <ShieldCheck size={12} /> Safe CLI preview
                </span>
              }
            >
              Time-Machine Recovery Commands
            </SectionLabel>
            <div className="mt-3 flex flex-col gap-2.5">
              {commands.map((cmd) => {
                const risk = riskBadgeMeta(cmd.risk);
                const copied = copiedId === cmd.id;
                return (
                  <div
                    key={cmd.id}
                    className="rounded-md border border-surface-border bg-page-canvas p-3"
                  >
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <div className="flex items-center gap-2">
                        <Terminal size={13} className="text-muted-foreground" />
                        <span className="text-caption font-medium text-foreground">
                          {cmd.label}
                        </span>
                        <Badge tone={risk.tone}>{risk.label}</Badge>
                      </div>
                      <button
                        type="button"
                        onClick={() => copyCommand(cmd.id, cmd.command)}
                        className="inline-flex h-6 items-center gap-1 rounded border border-surface-border bg-surface px-2 text-micro font-medium text-foreground hover:bg-surface-hover"
                      >
                        {copied ? <Check size={11} /> : <Copy size={11} />}
                        {copied ? "Copied" : "Copy"}
                      </button>
                    </div>
                    <p className="mt-1 text-caption text-muted-foreground">
                      {cmd.description}
                    </p>
                    <pre className="mt-2 overflow-x-auto rounded bg-muted px-2.5 py-1.5 font-mono text-micro text-foreground select-all">
                      {cmd.command}
                    </pre>
                  </div>
                );
              })}
            </div>
          </Card>

          {detail && detail.history.length > 0 && (
            <Card className="p-4">
              <SectionLabel>Scoped Ref Transitions ({detail.history.length})</SectionLabel>
              <div className="mt-2 divide-y divide-surface-border">
                {detail.history.map((entry) => {
                  const meta = historyActionMeta(entry.action);
                  return (
                    <div key={entry.seq} className="py-2 first:pt-0 last:pb-0">
                      <div className="flex items-center justify-between gap-2">
                        <div className="flex items-center gap-1.5">
                          <Badge tone={meta.tone}>{meta.label}</Badge>
                          <span className="font-mono text-caption font-medium text-foreground">
                            {entry.name}
                          </span>
                        </div>
                        <span className="tnum text-micro text-muted-foreground">
                          #{entry.seq} · {relativeTime(entry.at_ms)}
                        </span>
                      </div>
                      <div className="mt-0.5 font-mono text-micro text-muted-foreground">
                        {shortOid(entry.old_snapshot, 8)} → {shortOid(entry.new_snapshot, 8)}
                      </div>
                      {entry.reason && (
                        <div className="mt-0.5 text-caption text-muted-foreground">
                          {entry.reason}
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            </Card>
          )}
        </div>
      </div>
    </Drawer>
  );
}

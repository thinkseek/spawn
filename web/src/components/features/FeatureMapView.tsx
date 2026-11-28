import {
  ArrowUpRight,
  ExternalLink,
  FileSearch,
  GitBranch,
  LayoutGrid,
  Network,
  OctagonAlert,
  UserRoundX,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";

import { usePoll } from "../../api";
import { clockTime, relativeTime } from "../../format";
import { freshAt } from "../../validated";
import type {
  Feature,
  FeatureDeploy,
  FeatureLanding,
  FeatureMission,
  FeatureSource,
  FeatureTask,
} from "../../features";
import {
  backlogExplanation,
  blockerText,
  deployNote,
  gapText,
  groupByState,
  isExternal,
  landingLabel,
  newestVerification,
  objectiveName,
  ownerText,
  sourceLabel,
  verificationNote,
} from "../../features";
import type { FeatureTruthResponse } from "../../featureCoverage";
import { fetchFeatureMap, fetchOptionalFeatureTruth } from "../../featuresApi";
import type {
  FeatureMindMapTree,
  FileIndex,
  GraphGroupingMode,
  GraphValidationIssue,
  MindMapTreeNode,
  RelationIndex,
} from "../../featuresGraph";
import {
  buildActiveSpineModel,
  buildFeatureMindMapTree,
  buildFileIndex,
  buildRelationIndex,
  validateFeatureGraph,
} from "../../featuresGraph";
import { Card, FOCUS_RING, LiveStatus, PageHeader, chip, cx, tr, useLocale } from "../ui";
import { ActiveSpineView } from "./ActiveSpineView";
import { FeatureInspector } from "./FeatureInspector";
import { FeatureMindMap } from "./FeatureMindMap";
import { RuntimeTelemetryStrip } from "./RuntimeTelemetryStrip";

export type ViewMode = "spine" | "mindmap" | "columns";

interface FeatureMapViewProps {
  /** Open a task's session timeline: the evidence behind a capability. */
  onOpenSession: (sessionId: string) => void;
  /** Jump to the File coverage & one-click unassociated-file (wild file) scan page. */
  onOpenCoverage?: () => void;
}

/** The Feature map: a mission's capabilities, planned, implemented,
 * verified and live, each with the records that say so. Supports Active Spine
 * (tri-column dependency focus), interactive Mind Map, and State Columns.
 * States remain strictly server-derived from Spawn's event log. */
export function FeatureMapView({ onOpenSession, onOpenCoverage }: FeatureMapViewProps) {
  const { locale } = useLocale();
  const poll = usePoll("features", fetchFeatureMap, 4_000, { checked: true });
  const [chosen, setChosen] = useState<number | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>("spine");
  const [groupingMode, setGroupingMode] = useState<GraphGroupingMode>("auto");
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  const [focusTrigger, setFocusTrigger] = useState(0);
  const [truthData, setTruthData] = useState<FeatureTruthResponse | null>(null);

  const data = poll.data;
  const mission =
    data?.missions.find((entry) => entry.id === chosen) ?? data?.missions[0] ?? null;

  // Non-blocking read-only enrichment from `/api/features/truth`.
  const missionId = mission?.id ?? null;
  useEffect(() => {
    const controller = new AbortController();
    void fetchOptionalFeatureTruth(controller.signal, missionId).then((next) => {
      if (!controller.signal.aborted) setTruthData(next);
    });
    return () => controller.abort();
  }, [missionId]);

  const tree = useMemo(
    () => (mission ? buildFeatureMindMapTree(mission, { groupingMode }) : null),
    [mission, groupingMode],
  );
  const fileIndex = useMemo(
    () => buildFileIndex(mission?.features ?? []),
    [mission?.features],
  );
  const relationIndex = useMemo(
    () => buildRelationIndex(mission?.features ?? []),
    [mission?.features],
  );
  const graphIssues = useMemo(
    () => validateFeatureGraph(mission?.features ?? []),
    [mission?.features],
  );

  // Clear selection if the selected node no longer exists in the active mission tree.
  useEffect(() => {
    if (selectedNodeId && tree && !tree.nodes[selectedNodeId]) {
      setSelectedNodeId(null);
    }
  }, [selectedNodeId, tree]);

  const handleJumpToNode = useCallback((nodeId: string) => {
    setSelectedNodeId(nodeId);
    setFocusTrigger((n) => n + 1);
  }, []);

  const handleInspectInMap = useCallback((featureId: string) => {
    setViewMode("mindmap");
    setSelectedNodeId(featureId);
    setFocusTrigger((n) => n + 1);
  }, []);

  const selectedNode = selectedNodeId && tree ? (tree.nodes[selectedNodeId] ?? null) : null;
  const focusFeatureId = selectedNode?.feature?.id ?? null;
  const activeSpine = useMemo(
    () => (mission ? buildActiveSpineModel(mission, relationIndex, focusFeatureId) : null),
    [mission, relationIndex, focusFeatureId],
  );

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            {tr(locale, "能力地图", "Feature map")}
            {mission ? ` · ${tr(locale, `任务 #${mission.id}`, `mission #${mission.id}`)}` : ""}
          </h1>
        }
      >
        {/* View mode toggle: Active Spine (default) vs Mind Map vs State Columns */}
        <div
          className="flex items-center gap-1"
          role="group"
          aria-label={tr(locale, "能力地图视图模式", "Feature map view mode")}
        >
          <button
            type="button"
            onClick={() => setViewMode("spine")}
            aria-pressed={viewMode === "spine"}
            className={cx(chip(viewMode === "spine"), FOCUS_RING)}
          >
            <GitBranch size={13} />
            {tr(locale, "主心骨", "Active spine")}
          </button>
          <button
            type="button"
            onClick={() => setViewMode("mindmap")}
            aria-pressed={viewMode === "mindmap"}
            className={cx(chip(viewMode === "mindmap"), FOCUS_RING)}
          >
            <Network size={13} />
            {tr(locale, "思维导图", "Mind map")}
          </button>
          <button
            type="button"
            onClick={() => setViewMode("columns")}
            aria-pressed={viewMode === "columns"}
            className={cx(chip(viewMode === "columns"), FOCUS_RING)}
          >
            <LayoutGrid size={13} />
            {tr(locale, "状态分栏", "State columns")}
          </button>
          {onOpenCoverage && (
            <button
              type="button"
              onClick={onOpenCoverage}
              title={tr(
                locale,
                "打开文件覆盖率与未关联野文件扫描",
                "Open file coverage & one-click unassociated-file (野文件) scan",
              )}
              className={cx(chip(false), FOCUS_RING)}
            >
              <FileSearch size={13} />
              {tr(locale, "野文件扫描", "Wild-file scan")}
            </button>
          )}
        </div>

        {data && data.missions.length > 1 && (
          <div className="flex gap-1.5">
            {data.missions.map((entry) => (
              <button
                key={entry.id}
                type="button"
                onClick={() => {
                  setChosen(entry.id);
                  setSelectedNodeId(null);
                }}
                aria-current={entry.id === mission?.id ? "true" : undefined}
                className={cx("tnum", chip(entry.id === mission?.id), FOCUS_RING)}
              >
                #{entry.id}
              </button>
            ))}
          </div>
        )}
        <LiveStatus
          error={poll.error}
          label={
            data
              ? tr(
                  locale,
                  `更新于 ${relativeTime(freshAt(data.generated_at_ms, poll.checkedAt))}`,
                  `Updated ${relativeTime(freshAt(data.generated_at_ms, poll.checkedAt))}`,
                )
              : tr(locale, "连接中", "Connecting")
          }
        />
      </PageHeader>

      {!data ? (
        <div className="grid flex-1 place-items-center text-label text-muted-foreground">
          {poll.error
            ? tr(locale, `无法读取能力地图：${poll.error}`, `The feature map could not be read: ${poll.error}`)
            : tr(locale, "正在读取能力地图…", "Reading the map…")}
        </div>
      ) : !mission || !tree || !activeSpine ? (
        <div className="grid flex-1 place-items-center px-6 text-center text-label text-muted-foreground">
          {tr(locale, "当前没有包含能力地图的开放任务。", "No open objective mission has a capability map.")}
        </div>
      ) : viewMode === "spine" ? (
        <div
          data-spine-scroll-shell="true"
          className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto px-4 py-3 sm:px-5 xl:overflow-y-hidden"
        >
          <MissionHeaderStack
            mission={mission}
            graphIssues={graphIssues}
            relationsTotal={relationIndex.total}
            sharedFilesCount={Object.keys(fileIndex.sharedFiles).length}
          />
          <div className="flex shrink-0 flex-col gap-3 xl:min-h-0 xl:flex-1 xl:shrink xl:flex-row">
            <ActiveSpineView
              mission={mission}
              model={activeSpine}
              selectedNodeId={selectedNodeId}
              onSelectFeature={handleJumpToNode}
              onResetFocus={() => setSelectedNodeId(null)}
              onOpenSession={onOpenSession}
              onInspectInMindMap={handleInspectInMap}
            />
            {selectedNode && (
              <InspectorDrawer
                mission={mission}
                node={selectedNode}
                tree={tree}
                fileIndex={fileIndex}
                relationIndex={relationIndex}
                truth={truthData}
                desktopBreakpoint="xl"
                onSelectNode={handleJumpToNode}
                onOpenSession={onOpenSession}
                onClose={() => setSelectedNodeId(null)}
              />
            )}
          </div>
          <UnmappedNotice mission={mission} />
        </div>
      ) : viewMode === "mindmap" ? (
        <div className="flex min-h-0 flex-1 flex-col gap-3 px-4 py-3 sm:px-5">
          <MissionHeaderStack
            mission={mission}
            graphIssues={graphIssues}
            relationsTotal={relationIndex.total}
            sharedFilesCount={Object.keys(fileIndex.sharedFiles).length}
          />
          <div className="flex min-h-0 flex-1 flex-col gap-3 lg:flex-row">
            <FeatureMindMap
              mission={mission}
              tree={tree}
              fileIndex={fileIndex}
              relationIndex={relationIndex}
              groupingMode={groupingMode}
              onGroupingModeChange={setGroupingMode}
              selectedNodeId={selectedNodeId}
              onSelectNode={setSelectedNodeId}
              focusTrigger={focusTrigger}
            />
            {selectedNode && (
              <InspectorDrawer
                mission={mission}
                node={selectedNode}
                tree={tree}
                fileIndex={fileIndex}
                relationIndex={relationIndex}
                truth={truthData}
                onSelectNode={handleJumpToNode}
                onOpenSession={onOpenSession}
                onClose={() => setSelectedNodeId(null)}
              />
            )}
          </div>
          <UnmappedNotice mission={mission} />
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto flex max-w-[1600px] flex-col gap-3 px-4 py-4 sm:px-5">
            <MissionHeaderStack
              mission={mission}
              graphIssues={graphIssues}
              relationsTotal={relationIndex.total}
              sharedFilesCount={Object.keys(fileIndex.sharedFiles).length}
            />
            <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-4">
              {groupByState(mission.features).map((column) => (
                <section
                  key={column.state}
                  aria-label={column.info.label}
                  className="flex min-w-0 flex-col gap-2"
                >
                  <header className="rounded-card bg-surface px-3 py-2 shadow-card">
                    <div className="flex items-center gap-2">
                      <span className={cx("size-2 rounded-full", column.info.dot)} />
                      <h2 className="text-label font-semibold text-foreground">
                        {column.info.label}
                      </h2>
                      <span className="tnum ml-auto text-caption text-muted-foreground">
                        {column.features.length}
                      </span>
                    </div>
                    <p className="mt-0.5 text-micro text-faint-foreground">{column.info.blurb}</p>
                  </header>
                  {column.features.length === 0 ? (
                    <p className="rounded-card border border-dashed border-surface-border px-3 py-4 text-caption text-faint-foreground">
                      Nothing in this state.
                    </p>
                  ) : (
                    column.features.map((feature) => (
                      <FeatureCard
                        key={feature.id}
                        feature={feature}
                        mission={mission}
                        onOpenSession={onOpenSession}
                        onInspectInMap={() => handleInspectInMap(feature.id)}
                      />
                    ))
                  )}
                </section>
              ))}
            </div>
            <UnmappedNotice mission={mission} />
            <p className="pb-2 text-micro text-faint-foreground">
              Planned is not implemented (a task, a merge or a review is not a landing).
              Implemented is not verified. Verified is not live: live is a successful deploy
              recorded after the newest landing, and a mission no deploy ships is never called
              live here.
            </p>
          </div>
        </div>
      )}
    </main>
  );
}

function MissionHeaderStack({
  mission,
  graphIssues,
  relationsTotal,
  sharedFilesCount,
}: {
  mission: FeatureMission;
  graphIssues: GraphValidationIssue[];
  relationsTotal: number;
  sharedFilesCount: number;
}) {
  return (
    <>
      {mission.blocker && <MissionBlocker mission={mission} />}
      {mission.backlog_error && <BacklogError mission={mission} />}
      {mission.mapping.error && <MappingError mission={mission} />}
      {graphIssues.length > 0 && (
        <GraphIssuesBanner count={graphIssues.length} firstMessage={graphIssues[0].message} />
      )}
      <MissionHead
        mission={mission}
        relationsTotal={relationsTotal}
        sharedFilesCount={sharedFilesCount}
      />
      <RuntimeTelemetryStrip />
    </>
  );
}

function InspectorDrawer({
  mission,
  node,
  tree,
  fileIndex,
  relationIndex,
  truth,
  desktopBreakpoint = "lg",
  onSelectNode,
  onOpenSession,
  onClose,
}: {
  mission: FeatureMission;
  node: MindMapTreeNode;
  tree: FeatureMindMapTree;
  fileIndex: FileIndex;
  relationIndex: RelationIndex;
  truth: FeatureTruthResponse | null;
  desktopBreakpoint?: "lg" | "xl";
  onSelectNode: (nodeId: string) => void;
  onOpenSession: (sessionId: string) => void;
  onClose: () => void;
}) {
  return (
    <div
      className={cx(
        "flex min-h-[320px] max-h-[55vh] flex-col overflow-hidden rounded-card border border-surface-border shadow-card",
        desktopBreakpoint === "xl"
          ? "shrink-0 xl:min-h-0 xl:max-h-none xl:shrink-0"
          : "lg:min-h-0 lg:max-h-none",
      )}
    >
      <FeatureInspector
        mission={mission}
        node={node}
        tree={tree}
        fileIndex={fileIndex}
        relationIndex={relationIndex}
        truth={truth}
        wideAt={desktopBreakpoint}
        onSelectNode={onSelectNode}
        onOpenSession={onOpenSession}
        onClose={onClose}
      />
    </div>
  );
}

function UnmappedNotice({ mission }: { mission: FeatureMission }) {
  if (mission.unmapped.length === 0) return null;
  return (
    <p className="shrink-0 rounded-card border border-surface-border bg-surface px-3 py-2 text-caption text-muted-foreground">
      <span className="font-medium text-foreground">
        {mission.unmapped.length} objective
        {mission.unmapped.length === 1 ? "" : "s"} not in the mapping:
      </span>{" "}
      {mission.unmapped.map((objective) => objectiveName(mission.id, objective.id)).join(", ")}
    </p>
  );
}

function GraphIssuesBanner({ count, firstMessage }: { count: number; firstMessage: string }) {
  return (
    <div className="flex shrink-0 items-start gap-3 rounded-card border border-warning/30 bg-wait-tint px-4 py-2">
      <OctagonAlert size={16} className="mt-0.5 shrink-0 text-warning" />
      <div className="min-w-0 text-caption text-foreground">
        {count} feature graph link warning{count === 1 ? "" : "s"}:{" "}
        <span className="font-mono">{firstMessage}</span>
      </div>
    </div>
  );
}

function MissionBlocker({ mission }: { mission: FeatureMission }) {
  const blocker = mission.blocker;
  if (!blocker) return null;
  return (
    <div className="flex shrink-0 items-start gap-3 rounded-card border border-destructive/30 bg-fail-tint px-4 py-2.5">
      <OctagonAlert size={16} className="mt-0.5 shrink-0 text-destructive" />
      <div className="min-w-0 text-label text-foreground">{blockerText(blocker)}</div>
    </div>
  );
}

function BacklogError({ mission }: { mission: FeatureMission }) {
  return (
    <div className="flex shrink-0 items-start gap-3 rounded-card border border-warning/30 bg-wait-tint px-4 py-2.5">
      <OctagonAlert size={16} className="mt-0.5 shrink-0 text-warning" />
      <div className="min-w-0 text-label text-foreground">
        {backlogExplanation(mission)}:{" "}
        <span className="font-mono text-caption">{mission.backlog_error}</span>
      </div>
    </div>
  );
}

function MappingError({ mission }: { mission: FeatureMission }) {
  return (
    <div className="flex shrink-0 items-start gap-3 rounded-card border border-warning/30 bg-wait-tint px-4 py-2.5">
      <OctagonAlert size={16} className="mt-0.5 shrink-0 text-warning" />
      <div className="min-w-0 text-label text-foreground">
        The mapping {mission.mapping.path} could not be read, so each backlog objective is shown
        instead: <span className="font-mono text-caption">{mission.mapping.error}</span>
      </div>
    </div>
  );
}

function MissionHead({
  mission,
  relationsTotal,
  sharedFilesCount,
}: {
  mission: FeatureMission;
  relationsTotal: number;
  sharedFilesCount: number;
}) {
  const counts = mission.counts;
  return (
    <Card className="shrink-0 px-4 py-2.5">
      <div className="flex flex-wrap items-start gap-x-4 gap-y-2">
        <div className="min-w-0 flex-1">
          <p className="text-label text-foreground">{mission.text}</p>
          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-caption text-muted-foreground">
            <span>{mission.state}</span>
            {mission.gate && (
              <span className="truncate font-mono text-micro" title={mission.gate}>
                gate {mission.gate.split("/").slice(-2).join("/")}
              </span>
            )}
            <span>accepts by {mission.accept}</span>
            {mission.resident?.name && <span>resident {mission.resident.name}</span>}
            <span>
              {mission.shipped_by_deploy
                ? "Spawn's deploy ships this source"
                : "no deploy ships this source"}
            </span>
            {mission.mapping.kind === "file" && (
              <span className="truncate font-mono text-micro" title={mission.mapping.path ?? ""}>
                mapping {mission.mapping.path}
              </span>
            )}
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          <Count label="capabilities" value={counts.features} />
          <Count label="planned" value={counts.planned} />
          <Count label="implemented" value={counts.implemented} tone="info" />
          <Count label="verified" value={counts.verified} tone="success" />
          <Count label="live" value={counts.live} tone="success" />
          {relationsTotal > 0 && <Count label="links" value={relationsTotal} tone="info" />}
          {sharedFilesCount > 0 && <Count label="shared files" value={sharedFilesCount} tone="info" />}
          <Count label="unowned" value={counts.unowned} tone="warning" />
          <Count label="blocked" value={counts.blocked} tone="danger" />
        </div>
      </div>
      {mission.source && (
        <p className="mt-1.5 text-micro text-faint-foreground">
          Provenance: {sourceLabel(mission.source)}
          {mission.source.title ? ` — ${mission.source.title}` : ""}
        </p>
      )}
    </Card>
  );
}

function Count({
  label,
  value,
  tone = "muted",
}: {
  label: string;
  value: number;
  tone?: "muted" | "info" | "warning" | "danger" | "success";
}) {
  const style =
    value === 0
      ? "bg-muted text-muted-foreground"
      : tone === "danger"
        ? "bg-fail-tint text-destructive"
        : tone === "warning"
          ? "bg-wait-tint text-warning"
          : tone === "success"
            ? "bg-ok-tint text-success"
            : tone === "info"
              ? "bg-info-tint text-info"
              : "bg-surface-selected text-foreground";
  return (
    <span className={cx("tnum rounded-chip px-1.5 py-px text-caption", style)} title={label}>
      {value} {label}
    </span>
  );
}

function FeatureCard({
  feature,
  mission,
  onOpenSession,
  onInspectInMap,
}: {
  feature: Feature;
  mission: FeatureMission;
  onOpenSession: (sessionId: string) => void;
  onInspectInMap: () => void;
}) {
  const newest = newestTask(feature);
  return (
    <Card className="flex min-w-0 flex-col gap-2 px-3 py-2.5">
      <div className="flex min-w-0 items-start gap-2">
        <h3 className="min-w-0 flex-1 text-label font-semibold text-foreground">
          {feature.title}
        </h3>
        {feature.complete && (
          <span className="shrink-0 rounded-chip bg-ok-tint px-1.5 py-px text-micro font-medium text-success">
            complete
          </span>
        )}
        {feature.unowned && (
          <span
            title="No task in flight and not live: nobody is working on it"
            className="inline-flex shrink-0 items-center gap-1 rounded-chip bg-muted px-1.5 py-px text-micro text-muted-foreground"
          >
            <UserRoundX size={10} /> unowned
          </span>
        )}
      </div>
      <div className="flex items-center justify-between gap-2">
        <p className="truncate font-mono text-micro text-faint-foreground">{feature.id}</p>
        <button
          type="button"
          onClick={onInspectInMap}
          className={cx(
            "inline-flex shrink-0 items-center gap-0.5 rounded-chip px-1 text-micro text-info hover:underline",
            FOCUS_RING,
          )}
        >
          <Network size={10} /> map
        </button>
      </div>
      {feature.summary && (
        <p className="text-caption text-muted-foreground">{feature.summary}</p>
      )}

      {feature.objective_rows.length > 0 && (
        <ul className="space-y-0.5">
          {feature.objective_rows.map((objective) => (
            <li key={objective.id} className="flex min-w-0 items-baseline gap-1.5 text-micro">
              <span className="shrink-0 font-mono text-faint-foreground">
                {objectiveName(mission.id, objective.id)}
              </span>
              <span className="min-w-0 truncate text-muted-foreground" title={objective.title ?? ""}>
                {objective.title}
              </span>
            </li>
          ))}
        </ul>
      )}

      {feature.sources.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {feature.sources.map((source) => (
            <SourceChip key={source.ref} source={source} />
          ))}
        </div>
      )}

      {feature.blocked && (
        <p className="rounded-control bg-fail-tint px-2 py-1 text-micro text-destructive">
          {blockerText(feature.blocked)}
        </p>
      )}

      <OwnerLine
        feature={feature}
        onOpenSession={onOpenSession}
      />

      <dl className="grid grid-cols-[auto_1fr] gap-x-2 gap-y-0.5 text-micro">
        <dt className="text-faint-foreground">landed</dt>
        <dd className="min-w-0 truncate text-foreground">{landingText(feature)}</dd>
        <dt className="text-faint-foreground">verified</dt>
        <dd className="min-w-0 truncate text-foreground">
          {verificationNote(feature)}
          {newestVerification(feature)?.at_ms ? (
            <span className="text-faint-foreground">
              {" "}
              · {relativeTime(newestVerification(feature)?.at_ms)}
            </span>
          ) : null}
        </dd>
        <dt className="text-faint-foreground">deployed</dt>
        <dd
          className="min-w-0 truncate text-foreground"
          title={deployTitle(feature.evidence.deploy)}
        >
          {deployNote(feature)}
        </dd>
        {newest && (
          <>
            <dt className="text-faint-foreground">review</dt>
            <dd className="min-w-0 truncate text-foreground">{reviewText(newest)}</dd>
          </>
        )}
      </dl>

      {feature.gaps.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {feature.gaps.map((gap) => (
            <span
              key={gap.kind + gap.objectives.join(",")}
              title={gap.detail ?? undefined}
              className="rounded-chip bg-wait-tint px-1.5 py-px text-micro text-warning"
            >
              {gapText(gap, mission.id)}
            </span>
          ))}
        </div>
      )}
    </Card>
  );
}

function OwnerLine({
  feature,
  onOpenSession,
}: {
  feature: Feature;
  onOpenSession: (sessionId: string) => void;
}) {
  const owner = feature.owner;
  const last = feature.last_owner;
  const session = owner?.session_id ?? last?.session_id ?? null;
  const theText = ownerText(owner) ?? (last ? `last: ${ownerText(last) ?? ""}` : null);
  if (theText === null) {
    return <p className="text-micro text-faint-foreground">no task yet</p>;
  }
  return (
    <p className="flex min-w-0 items-center gap-1 text-micro text-muted-foreground">
      <span className="truncate">{theText}</span>
      {session && (
        <button
          type="button"
          onClick={() => onOpenSession(session)}
          className={cx(
            "inline-flex shrink-0 items-center gap-0.5 rounded-chip px-1 text-micro text-info hover:underline",
            FOCUS_RING,
          )}
        >
          session <ArrowUpRight size={10} />
        </button>
      )}
    </p>
  );
}

function SourceChip({ source }: { source: FeatureSource }) {
  const label = sourceLabel(source);
  const title = source.title ?? source.ref;
  if (isExternal(source) && source.url) {
    return (
      <a
        href={source.url}
        target="_blank"
        rel="noreferrer"
        title={title}
        className="inline-flex max-w-full items-center gap-0.5 truncate rounded-chip bg-surface-selected px-1.5 py-px text-micro text-info hover:underline"
      >
        {label} <ExternalLink size={9} className="shrink-0" />
      </a>
    );
  }
  return (
    <span
      title={title}
      className={cx(
        "max-w-full truncate rounded-chip px-1.5 py-px text-micro",
        source.linked
          ? "bg-surface-selected text-muted-foreground"
          : "bg-muted text-faint-foreground",
      )}
    >
      {label}
      {source.native && !source.linked ? " (missing)" : ""}
    </span>
  );
}

function newestTask(feature: Feature): FeatureTask | null {
  let newest: FeatureTask | null = null;
  for (const task of feature.evidence.tasks) {
    if (newest === null || (task.updated_at_ms ?? 0) > (newest.updated_at_ms ?? 0)) {
      newest = task;
    }
  }
  return newest;
}

function landingText(feature: Feature): string {
  const landing = newestLanding(feature);
  if (landing === null) return "not landed";
  return landingLabel(landing, landing.at_ms ? relativeTime(landing.at_ms) : "");
}

function newestLanding(feature: Feature): FeatureLanding | null {
  let newest: FeatureLanding | null = null;
  for (const landing of feature.evidence.landings) {
    if (newest === null || (landing.at_ms ?? 0) > (newest.at_ms ?? 0)) {
      newest = landing;
    }
  }
  return newest;
}

function reviewText(task: FeatureTask): string {
  const gate = task.review.last_gate;
  if (task.review.sent_back) return "sent back";
  if (task.review.accepted) {
    return gate ? `gate ${gate.verdict}` : `accepted at revision ${task.revisions}`;
  }
  if (gate) return `${gate.verdict}${gate.reason ? `: ${gate.reason}` : ""}`;
  return task.state;
}

function deployTitle(deploy: FeatureDeploy | null): string | undefined {
  if (!deploy) return undefined;
  return [
    deploy.head ? `head ${deploy.head}` : null,
    deploy.by ? `by ${deploy.by}` : null,
    deploy.at_ms ? clockTime(deploy.at_ms) : null,
    deploy.reason || null,
  ]
    .filter(Boolean)
    .join("\n");
}

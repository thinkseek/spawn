import { Suspense, useCallback, useMemo, useState } from "react";
import { ChevronRight } from "lucide-react";

import type { Alert, PollResult } from "../../api";
import { freshAt } from "../../validated";
import type { Attention } from "../../attention";
import type { Fleet } from "../../fleet";
import { relativeTime } from "../../format";
import type { NeedItem } from "../../home";
import {
  actionableHomeNeeds,
  agentNames,
  agentsNow,
  buildSystemStatusModel,
  countOutageAlerts,
  missionProgressSummary,
  nameTask,
} from "../../home";
import { tr } from "../../i18n";
import type { Missions } from "../../missions";
import type { FleetContext, Liveness } from "../../ordering";
import {
  effectiveSnapshotFreshness,
  groupAgents,
  missingAlwaysOn,
  residentName,
  uptimeMs,
  usualEngine,
} from "../../ordering";
import { useNow } from "../rsi/Phase";
import { RouteErrorBoundary, retryableLazy } from "../RouteErrorBoundary";
import { FOCUS_RING_INSET, LiveStatus, PageHeader, cx, useLocale } from "../ui";
import { useAway } from "../home/Home";
import { HomeView } from "../home/HomeView";
import { ApprovalsQueue } from "./ApprovalsQueue";
import { MasterComposer } from "./MasterComposer";

const loadAgentDetail = () =>
  import("./AgentDetail").then((mod) => ({ default: mod.AgentDetail }));
const loadFleetDetailsSection = () => import("./FleetDetailsSection");

const AgentDetail = retryableLazy(loadAgentDetail);
const FleetDetailsSection = retryableLazy(loadFleetDetailsSection);

export function prefetchAgentDetail(): void {
  void loadAgentDetail().catch(() => {});
}

export function prefetchFleetDetails(): void {
  void loadFleetDetailsSection().catch(() => {});
}

interface FleetViewProps {
  /** Its agents marked by what is escalated (App, attention.ts). */
  fleet: PollResult<Fleet>;
  /** The dispatcher's view, `/api/attention` (App polls it). */
  attention: PollResult<Attention>;
  /** Residents and held tasks, from /api/missions (App polls it). */
  context: FleetContext;
  /** The missions themselves, for completion progress rows. */
  missions: Missions | null;
  /** The alert window (folded into details digest). */
  alerts: Alert[] | null;
  /** Raw "Needs you" computed in App; filtered to actionable items on Home. */
  needs: NeedItem[];
  /** Action-only filtered needs for Home. */
  actionableNeeds?: NeedItem[];
  onShowMissions: () => void;
  onShowFeatures?: () => void;
  onShowAgents?: () => void;
  selectedId: string | null;
  onSelect: (sessionId: string | null) => void;
  onOpenTimeline: (sessionId: string) => void;
}

/**
 * Primary Overview page (`#fleet`): leads with System Status & Mission Completion,
 * keeps Approvals / Action Required conditional, and folds heavy engineering
 * diagnostics (`AlwaysOnAgents`, `HealthStrip`, `NodeRows`, `FleetBoard`,
 * `Digest`, `Interventions`, `CoreLines`) cleanly at the bottom.
 */
export function FleetView({
  fleet,
  attention,
  context,
  missions,
  alerts,
  needs,
  actionableNeeds,
  selectedId,
  onSelect,
  onOpenTimeline,
  onShowMissions,
  onShowFeatures,
  onShowAgents,
}: FleetViewProps) {
  const { locale } = useLocale();
  const [detailsOpen, setDetailsOpen] = useState(false);
  const now = useNow(15_000);
  const data = fleet.data;
  const selected =
    data?.agents.find((agent) => agent.session_id === selectedId) ?? null;
  const updated = data?.health.generated_at_ms ?? null;
  const agents = data?.agents;
  const groups = useMemo(() => groupAgents(agents ?? [], context), [agents, context]);
  const names = useMemo(() => agentNames(agents ?? [], context), [agents, context]);
  const onBoard = agents?.find((agent) => agent.role === "supervisor") ?? null;
  const supervisor = useMemo(
    () => ({
      session:
        attention.data?.supervisor?.session_id ?? onBoard?.session_id ?? "session_supervisor",
      live: attention.data?.supervisor?.live ?? onBoard?.live ?? true,
    }),
    [
      attention.data?.supervisor?.session_id,
      attention.data?.supervisor?.live,
      onBoard?.session_id,
      onBoard?.live,
    ],
  );
  const engine = useMemo(() => usualEngine(agents ?? []), [agents]);
  const alwaysOn = groups[0].agents;
  const missing = useMemo(() => missingAlwaysOn(agents ?? [], context), [agents, context]);
  const liveness = useMemo<Liveness>(
    () => ({
      total: alwaysOn.length + missing.length,
      up: alwaysOn.filter((agent) => agent.live).length,
      shortest: alwaysOn.reduce<Liveness["shortest"]>((best, agent) => {
        const ms = uptimeMs(agent);
        if (ms === null || agent.started_at_ms === null || (best && best.ms <= ms)) return best;
        const name =
          agent.role === "supervisor"
            ? "supervisor"
            : `resident ${residentName(agent, context) ?? agent.agent_id}`;
        return { name, ms, since: agent.started_at_ms };
      }, null),
    }),
    [alwaysOn, missing.length, context],
  );

  const latestNodeBeatMs = useMemo(() => {
    if (!data?.nodes.length) return null;
    let max: number | null = null;
    for (const node of data.nodes) {
      if (node.sampled_ms !== null && (max === null || node.sampled_ms > max)) {
        max = node.sampled_ms;
      }
    }
    return max;
  }, [data?.nodes]);
  const rawSnapshotAt = data?.generated_at_ms ?? updated;
  const snapshotAt = rawSnapshotAt !== null ? freshAt(rawSnapshotAt, fleet.checkedAt) : null;
  const displayUpdated = updated !== null ? freshAt(updated, fleet.checkedAt) : null;
  const freshness = effectiveSnapshotFreshness(
    rawSnapshotAt,
    snapshotAt,
    now,
    fleet.checkedAt,
    fleet.error,
    latestNodeBeatMs,
  );
  const freshnessLabel = data
    ? freshness.level === "fresh"
      ? displayUpdated
        ? tr(locale, `更新于 ${relativeTime(displayUpdated)}`, `Updated ${relativeTime(displayUpdated)}`)
        : tr(locale, "实时", "Live")
      : `${freshness.label} · ${displayUpdated ? relativeTime(displayUpdated) : "live"}`
    : tr(locale, "连接中", "Connecting");

  const away = useAway();
  const openAgent = useCallback(
    (session: string) => {
      prefetchAgentDetail();
      onSelect(session);
    },
    [onSelect],
  );
  const agentsNowModel = useMemo(
    () => agentsNow(agents ?? null, context),
    [agents, context],
  );

  const actionItems = useMemo(
    () => actionableNeeds ?? actionableHomeNeeds(needs),
    [actionableNeeds, needs],
  );
  const missionProgress = useMemo(
    () => missionProgressSummary(missions, now),
    [missions, now],
  );
  const systemStatus = useMemo(
    () =>
      buildSystemStatusModel({
        health: data?.health ?? null,
        liveness,
        agents: agents ?? null,
        nodes: data?.nodes ?? [],
        missionProgress,
        actionCount: actionItems.filter((item) => item.tone === "act").length,
        criticalOutageCount: countOutageAlerts(actionItems),
        freshnessLevel: freshness.level,
      }),
    [data?.health, data?.nodes, liveness, agents, missionProgress, actionItems, freshness.level],
  );

  return (
    <main className="flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas">
      <PageHeader
        title={
          <h1 className="truncate text-body font-semibold text-foreground">
            {tr(locale, "系统总览", "Overview")}
          </h1>
        }
      >
        {actionItems.length > 0 && (
          <span className="tnum hidden rounded-chip bg-wait-tint px-2 py-0.5 text-caption font-medium text-warning sm:inline">
            {tr(locale, `${actionItems.length} 项待处理`, `${actionItems.length} to act on`)}
          </span>
        )}
        <LiveStatus
          error={fleet.error}
          freshness={freshness.level}
          label={freshnessLabel}
        />
      </PageHeader>

      {!data ? (
        <div className="grid flex-1 place-items-center">
          <p className="text-caption text-faint-foreground">
            {fleet.error
              ? tr(locale, `无法获取控制面状态：${fleet.error}`, `Could not reach the fleet API: ${fleet.error}`)
              : tr(locale, "正在加载系统状态与任务进度…", "Loading system status and missions…")}
          </p>
        </div>
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          <div className="mx-auto flex max-w-[1200px] flex-col gap-3 p-3 sm:p-4">
            {freshness.level === "frozen" && (
              <div
                role="status"
                className="rounded-lg border border-destructive/30 bg-fail-tint px-3 py-2 text-caption text-destructive"
              >
                {freshness.label}：
                {tr(locale, "控制面快照未及时更新", "Control-plane snapshot has stalled")}
                {freshness.driftMs !== null && freshness.driftMs > 0
                  ? tr(
                      locale,
                      `（节点心跳仍领先 ${Math.floor(freshness.driftMs / 60_000)}m）`,
                      ` (node heartbeats lead by ${Math.floor(freshness.driftMs / 60_000)}m)`,
                    )
                  : freshness.ageMs !== null
                    ? tr(
                        locale,
                        `（已滞后 ${Math.floor(freshness.ageMs / 60_000)}m）`,
                        ` (${Math.floor(freshness.ageMs / 60_000)}m behind)`,
                      )
                    : ""}
                {tr(locale, "，请先核实节点健康后再派单。", ". Verify node health before dispatching.")}
              </div>
            )}

            <HomeView
              now={now}
              systemStatus={systemStatus}
              missionProgress={missionProgress}
              freshnessLevel={freshness.level}
              freshnessLabel={freshnessLabel}
              actionableNeeds={actionItems}
              approvals={<ApprovalsQueue onOpenAgent={openAgent} />}
              masterControl={<MasterComposer />}
              agents={agentsNowModel}
              onOpenAgent={openAgent}
              onShowMissions={onShowMissions}
              onShowAgents={onShowAgents ?? (() => undefined)}
              onShowFeatures={onShowFeatures ?? (() => undefined)}
            />

            <button
              type="button"
              onClick={() => setDetailsOpen((value) => !value)}
              onMouseEnter={prefetchFleetDetails}
              onFocus={prefetchFleetDetails}
              aria-expanded={detailsOpen}
              className={cx(
                "mt-1 flex items-center gap-1.5 self-start rounded-control px-2 py-1 text-caption font-medium text-muted-foreground hover:bg-surface-hover hover:text-foreground",
                FOCUS_RING_INSET,
              )}
            >
              <ChevronRight
                size={12}
                aria-hidden="true"
                className={cx("transition-transform motion-reduce:transition-none", detailsOpen && "rotate-90")}
              />
              {tr(locale, "展开系统详情与工程指标", "System details & engineering metrics")}
              <span className="tnum font-normal text-faint-foreground">
                {tr(
                  locale,
                  `${data.agents.length} 个 Agent · 节点 · 资源 · 主控日志`,
                  `${data.agents.length} agents · nodes · resources · digest`,
                )}
              </span>
            </button>

            {detailsOpen && (
              <RouteErrorBoundary resetKey="fleet-details" compact>
                <Suspense
                  fallback={
                    <div
                      role="status"
                      className="rounded-card border border-surface-border bg-surface px-4 py-3 text-caption text-muted-foreground"
                    >
                      {tr(locale, "正在加载系统详情与工程指标…", "Loading system details & engineering metrics…")}
                    </div>
                  }
                >
                  <FleetDetailsSection
                    now={now}
                    data={data}
                    attention={attention}
                    context={context}
                    missions={missions}
                    alerts={alerts}
                    agents={agents}
                    awayFromMs={away?.from ?? null}
                    awayAtMs={away?.at ?? null}
                    supervisor={supervisor}
                    alwaysOn={alwaysOn}
                    missing={missing}
                    liveness={liveness}
                    groups={groups}
                    engine={engine}
                    selectedId={selectedId}
                    onSelect={onSelect}
                  />
                </Suspense>
              </RouteErrorBoundary>
            )}
          </div>
        </div>
      )}

      {selected && (
        <RouteErrorBoundary resetKey={selected.session_id} compact>
          <Suspense
            fallback={
              <div
                role="status"
                aria-live="polite"
                className="fixed bottom-4 right-4 z-50 rounded-card border border-surface-border bg-surface px-3 py-2 text-caption text-muted-foreground shadow-card"
              >
                {tr(locale, "正在加载 Agent 详情…", "Loading agent details…")}
              </div>
            }
          >
            <AgentDetail
              key={selected.session_id}
              agent={selected}
              name={names.get(selected.session_id) ?? null}
              held={nameTask(selected, context)}
              onClose={() => onSelect(null)}
              onOpenTimeline={onOpenTimeline}
            />
          </Suspense>
        </RouteErrorBoundary>
      )}
    </main>
  );
}

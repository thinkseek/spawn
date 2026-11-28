import { useMemo, useState } from "react";
import { ChevronRight } from "lucide-react";

import type { Alert, PollResult } from "../../api";
import { fetchAlertsSince, usePoll } from "../../api";
import type { Attention } from "../../attention";
import type { Fleet } from "../../fleet";
import { relativeTime } from "../../format";
import type { NeedItem } from "../../home";
import {
  actionableHomeNeeds,
  agentNames,
  agentsNow,
  alertsSpanMs,
  allNotes,
  buildSystemStatusModel,
  digestSince,
  digestWindow,
  latestNotes,
  missionProgressSummary,
  nameTask,
} from "../../home";
import { tr } from "../../i18n";
import type { Missions } from "../../missions";
import type { FleetContext } from "../../ordering";
import { groupAgents, missingAlwaysOn, residentName, snapshotFreshness, usualEngine } from "../../ordering";
import { useNow } from "../rsi/Phase";
import { FOCUS_RING_INSET, LiveStatus, PageHeader, cx, useLocale } from "../ui";
import { useAway } from "../home/Home";
import { HomeRuntimeDetails, HomeView } from "../home/HomeView";
import { AgentDetail } from "./AgentDetail";
import { AlwaysOnAgents, uptimeMs } from "./AlwaysOnAgents";
import { FleetBoard } from "./FleetBoard";
import { InputBox } from "./InputBox";
import { CORE_LINES_POLL_MS, fetchCoreLines } from "../home/CoreLines";
import { coreLinesView } from "../../coreLines";
import { figureState } from "../../interventions.ts";
import { INTERVENTIONS_POLL_MS, fetchInterventions } from "./Interventions";
import { HealthStrip } from "./HealthStrip";
import { NodeRows } from "./NodeRows";
import { ApprovalsQueue } from "./ApprovalsQueue";
import { DigestPanel } from "./DigestPanel";
import { MasterComposer } from "./MasterComposer";
import type { Liveness } from "./HealthStrip";

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
  useNow(1_000);
  const data = fleet.data;
  const selected =
    data?.agents.find((agent) => agent.session_id === selectedId) ?? null;
  const updated = data?.health.generated_at_ms ?? null;
  const agents = data?.agents;
  const groups = useMemo(() => groupAgents(agents ?? [], context), [agents, context]);
  const names = useMemo(() => agentNames(agents ?? [], context), [agents, context]);
  const onBoard = agents?.find((agent) => agent.role === "supervisor") ?? null;
  const supervisor = {
    session:
      attention.data?.supervisor?.session_id ?? onBoard?.session_id ?? "session_supervisor",
    live: attention.data?.supervisor?.live ?? onBoard?.live ?? true,
  };
  const engine = useMemo(() => usualEngine(agents ?? []), [agents]);
  const alwaysOn = groups[0].agents;
  const missing = useMemo(() => missingAlwaysOn(agents ?? [], context), [agents, context]);
  const liveness: Liveness = {
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
  };

  const now = Date.now();
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
  const freshness = snapshotFreshness(data?.generated_at_ms ?? updated, now, latestNodeBeatMs);
  const freshnessLabel = data
    ? freshness.level === "fresh"
      ? updated
        ? tr(locale, `更新于 ${relativeTime(updated)}`, `Updated ${relativeTime(updated)}`)
        : tr(locale, "实时", "Live")
      : `${freshness.label} · ${updated ? relativeTime(updated) : "live"}`
    : tr(locale, "连接中", "Connecting");

  const away = useAway();
  const digestSpan = digestWindow(now, away?.from ?? null);
  const span = alertsSpanMs(now, digestSpan.since);
  const digestAlerts = usePoll(`alerts:since:${span}`, (signal) => fetchAlertsSince(span, signal), 60_000);
  const day = usePoll("interventions-24h", (signal) => fetchInterventions("24h", signal), INTERVENTIONS_POLL_MS);
  const week = usePoll("interventions-7d", (signal) => fetchInterventions("7d", signal), INTERVENTIONS_POLL_MS);
  const coreLines = usePoll("core-lines", fetchCoreLines, CORE_LINES_POLL_MS);
  const openAgent = (session: string) => onSelect(session);

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
              agents={agentsNow(agents ?? null, context)}
              onOpenAgent={openAgent}
              onShowMissions={onShowMissions}
              onShowAgents={onShowAgents ?? (() => undefined)}
              onShowFeatures={onShowFeatures ?? (() => undefined)}
            />

            <button
              type="button"
              onClick={() => setDetailsOpen((value) => !value)}
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
              <>
                <DigestPanel />
                <HomeRuntimeDetails
                  now={now}
                  notes={latestNotes(attention.data, now)}
                  allNotes={allNotes(attention.data)}
                  digest={digestSince(
                    missions,
                    digestAlerts.data ?? alerts,
                    now,
                    digestSpan.since,
                    agents ?? null,
                    digestSpan.away,
                  )}
                  back={away?.at ?? null}
                  interventions={{
                    day: figureState(day.data, day.error),
                    week: figureState(week.data, week.error),
                  }}
                  coreLines={{
                    answer: coreLines.data,
                    error: coreLines.data ? null : coreLines.error,
                  }}
                  talk={
                    <InputBox
                      key={supervisor.session}
                      sessionId={supervisor.session}
                      to="the supervisor"
                      stopped={!supervisor.live}
                    />
                  }
                />
                <AlwaysOnAgents
                  agents={alwaysOn}
                  missing={missing}
                  context={context}
                  usualEngine={engine}
                  selectedId={selectedId}
                  onSelect={onSelect}
                />
                <HealthStrip
                  health={data.health}
                  liveness={liveness}
                  diagnostics={coreLines.data ? coreLinesView(coreLines.data).diagnostics : []}
                />
                <NodeRows fleetNodes={data.nodes} />
                <FleetBoard
                  groups={groups}
                  total={data.agents}
                  context={context}
                  usualEngine={engine}
                  selectedId={selectedId}
                  onSelect={onSelect}
                />
              </>
            )}
          </div>
        </div>
      )}

      {selected && (
        <AgentDetail
          key={selected.session_id}
          agent={selected}
          name={names.get(selected.session_id) ?? null}
          held={nameTask(selected, context)}
          onClose={() => onSelect(null)}
          onOpenTimeline={onOpenTimeline}
        />
      )}
    </main>
  );
}

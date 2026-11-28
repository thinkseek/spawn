import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import {
  Activity,
  ArrowRight,
  Bot,
  CheckCircle2,
  CircleAlert,
  CircleCheck,
  Copy,
  Flag,
  History,
  Info,
  MessageSquareText,
  Play,
  Server,
  ShieldAlert,
  Target,
} from "lucide-react";

import { postResume } from "../../api";
import { newKey } from "../../input";
import { relativeTime } from "../../format";
import type {
  AgentsNow,
  Digest,
  MissionProgressRow,
  MissionProgressSummary,
  NeedItem,
  NoteRow,
  NotesShown,
  SystemStatusModel,
} from "../../home";
import { awayFrom, needsSummary } from "../../home";
import { tr } from "../../i18n";
import { HomeCard } from "../aui/HomeCard";
import { Badge, Button, Card, EmptyState } from "../aui/kit";
import { PixelGridLoader, ToolChip } from "../BeautifulPrimitives";
import { FOCUS_RING, FOCUS_RING_INSET, cx, sinceText, useLocale } from "../ui";

const GIB = 1024 ** 3;

function formatGiB(bytes: number): string {
  const gib = bytes / GIB;
  return `${gib.toFixed(gib < 10 ? 1 : 0)} GiB`;
}

/** What a resume from the page came to. */
type Resumed =
  | { state: "idle" }
  | { state: "sending" }
  | { state: "done" }
  | { state: "refused"; why: string };

/** A halted mission's Resume button with idempotency key per halt. */
function ResumeButton({ resume }: { resume: { mission: number; halt: number } }) {
  const { locale } = useLocale();
  const [key] = useState(() => newKey());
  const [resumed, setResumed] = useState<Resumed>({ state: "idle" });
  const send = () => {
    setResumed({ state: "sending" });
    void postResume(resume.mission, { halt: resume.halt, key }).then(
      ({ status, body }) => {
        if (status === 202) {
          setResumed({ state: "done" });
          return;
        }
        const error = (body as { error?: unknown } | null)?.error;
        setResumed({
          state: "refused",
          why:
            typeof error === "string"
              ? error
              : status === null
                ? tr(locale, "无法连接服务端", "the server was not reached")
                : `HTTP ${status}`,
        });
      },
      () =>
        setResumed({
          state: "refused",
          why: tr(locale, "无法连接服务端", "the server was not reached"),
        }),
    );
  };
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      {resumed.state === "refused" && (
        <span className="max-w-64 truncate text-micro text-destructive" title={resumed.why}>
          {resumed.why}
        </span>
      )}
      <Button
        variant="secondary"
        size="sm"
        onClick={send}
        disabled={resumed.state === "sending" || resumed.state === "done"}
        loading={resumed.state === "sending"}
        title={tr(locale, `恢复任务 #${resume.mission}`, `Resume mission #${resume.mission} from this halt`)}
        icon={<Play size={12} aria-hidden="true" />}
      >
        {resumed.state === "sending"
          ? tr(locale, "恢复中…", "Resuming")
          : resumed.state === "done"
            ? tr(locale, "已恢复", "Resumed")
            : tr(locale, "恢复运行", "Resume")}
      </Button>
    </span>
  );
}

/** A command to copy. */
function CopyCommand({ command }: { command: string }) {
  const { locale } = useLocale();
  const [copied, setCopied] = useState(false);
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5">
      <code className="min-w-0 truncate rounded-chip bg-inset px-1.5 py-0.5 font-mono text-micro text-foreground select-all">
        {command}
      </code>
      <Button
        variant="ghost"
        size="sm"
        onClick={() => {
          void navigator.clipboard?.writeText(command).then(
            () => {
              setCopied(true);
              window.setTimeout(() => setCopied(false), 1_500);
            },
            () => setCopied(false),
          );
        }}
        title={tr(locale, "复制命令", "Copy command")}
        icon={<Copy size={11} aria-hidden="true" />}
      >
        {copied ? tr(locale, "已复制", "Copied") : tr(locale, "复制", "Copy")}
      </Button>
    </span>
  );
}

function NeedDetail({ item }: { item: NeedItem }) {
  const { locale } = useLocale();
  const [open, setOpen] = useState(false);
  if (!item.detail) return null;
  return (
    <p className={cx("mt-0.5 text-caption text-muted-foreground", !open && "flex min-w-0 items-baseline")}>
      <span className={cx(!open && "min-w-0 truncate")}>{open && item.full ? item.full : item.detail}</span>
      {item.full && (
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          aria-expanded={open}
          className={cx("ml-1.5 shrink-0 rounded-chip text-faint-foreground hover:text-foreground hover:underline", FOCUS_RING)}
        >
          {open ? tr(locale, "收起", "less") : tr(locale, "展开", "more")}
        </button>
      )}
    </p>
  );
}

/**
 * Primary System Status hero shown at the very top of Home:
 * overall health badge, node connectivity, always-on daemon liveness,
 * active agents, and overall mission completion.
 */
export function SystemStatusHero({
  status,
  missionProgress,
  freshnessLevel,
  freshnessLabel,
  onShowMissions,
  onShowAgents,
}: {
  status: SystemStatusModel;
  missionProgress: MissionProgressSummary;
  freshnessLevel: "fresh" | "aging" | "frozen";
  freshnessLabel: string;
  onShowMissions: () => void;
  onShowAgents: () => void;
}) {
  const { locale } = useLocale();
  const toneMeta =
    status.tone === "degraded"
      ? {
          pill: "bg-fail-tint text-destructive",
          icon: <ShieldAlert size={14} className="shrink-0 text-destructive" />,
          label: tr(locale, "运行受阻", "Degraded"),
        }
      : status.tone === "attention"
        ? {
            pill: "bg-wait-tint text-warning",
            icon: <CircleAlert size={14} className="shrink-0 text-warning" />,
            label: tr(locale, "需要关注", "Attention"),
          }
        : {
            pill: "bg-ok-tint text-success",
            icon: <CheckCircle2 size={14} className="shrink-0 text-success" />,
            label: tr(locale, "系统运行正常", "System Healthy"),
          };

  const progressPct = missionProgress.overallPct;

  return (
    <section
      aria-label={tr(locale, "系统状态总览", "System status overview")}
      className="rounded-card border border-surface-border bg-surface p-4 shadow-card sm:p-5"
    >
      <div className="flex flex-wrap items-center gap-2.5 border-b border-surface-border pb-3">
        <div className="flex min-w-0 items-center gap-2">
          <Activity size={15} className="shrink-0 text-muted-foreground" aria-hidden="true" />
          <h2 className="text-body font-semibold text-foreground">
            {tr(locale, "系统状态", "System Status")}
          </h2>
          <span
            className={cx(
              "inline-flex items-center gap-1 rounded-chip px-2 py-0.5 text-micro font-semibold",
              toneMeta.pill,
            )}
          >
            {toneMeta.icon}
            {toneMeta.label}
          </span>
        </div>

        <div className="ml-auto flex flex-wrap items-center gap-2 text-micro text-muted-foreground">
          {status.memoryAvailBytes !== null && (
            <span className="tnum rounded-chip bg-inset px-2 py-0.5">
              {tr(locale, "可用内存", "Mem free")} {formatGiB(status.memoryAvailBytes)}
              {status.memoryUsedPct !== null ? ` (${100 - status.memoryUsedPct}%)` : ""}
            </span>
          )}
          {status.tightestDiskFreeBytes !== null && (
            <span
              className={cx(
                "tnum rounded-chip px-2 py-0.5",
                status.diskLowCount > 0 ? "bg-wait-tint text-warning" : "bg-inset",
              )}
            >
              {tr(locale, "磁盘余量", "Disk")} {formatGiB(status.tightestDiskFreeBytes)}
            </span>
          )}
          <span
            className={cx(
              "tnum",
              freshnessLevel === "frozen"
                ? "text-destructive"
                : freshnessLevel === "aging"
                  ? "text-warning"
                  : "text-faint-foreground",
            )}
          >
            {freshnessLabel}
          </span>
        </div>
      </div>

      <div className="mt-3.5 grid grid-cols-2 gap-2.5 lg:grid-cols-4">
        <div className="rounded-control border border-surface-border/70 bg-inset px-3 py-2.5">
          <div className="flex items-center gap-1.5 text-micro text-faint-foreground">
            <Server size={12} aria-hidden="true" />
            <span>{tr(locale, "节点连接", "Nodes")}</span>
          </div>
          <div className="tnum mt-1 text-title-sm font-semibold text-foreground">
            {status.nodesTotal > 0 ? `${status.nodesOnline}/${status.nodesTotal}` : tr(locale, "主控在线", "Primary")}
          </div>
          <p className="mt-0.5 truncate text-micro text-muted-foreground">
            {status.nodesTotal > 0
              ? tr(
                  locale,
                  `${status.nodesOnline} 台在线 · 共 ${status.nodesTotal} 台`,
                  `${status.nodesOnline} of ${status.nodesTotal} online`,
                )
              : tr(locale, "当前仅单节点控制面", "Single-host control plane")}
          </p>
        </div>

        <div className="rounded-control border border-surface-border/70 bg-inset px-3 py-2.5">
          <div className="flex items-center gap-1.5 text-micro text-faint-foreground">
            <CheckCircle2 size={12} aria-hidden="true" />
            <span>{tr(locale, "常驻进程", "Always-On")}</span>
          </div>
          <div
            className={cx(
              "tnum mt-1 text-title-sm font-semibold",
              status.alwaysOnTotal === 0
                ? "text-foreground"
                : status.alwaysOnUp < status.alwaysOnTotal
                  ? "text-warning"
                  : "text-success",
            )}
          >
            {status.alwaysOnUp}/{status.alwaysOnTotal}
          </div>
          <p className="tnum mt-0.5 truncate text-micro text-muted-foreground">
            {tr(locale, `24h 重启 ${status.restarts24h} 次`, `${status.restarts24h} restarts · 24h`)}
          </p>
        </div>

        <button
          type="button"
          onClick={onShowAgents}
          className={cx(
            "rounded-control border border-surface-border/70 bg-inset px-3 py-2.5 text-left transition-colors hover:bg-surface-hover",
            FOCUS_RING_INSET,
          )}
        >
          <div className="flex items-center gap-1.5 text-micro text-faint-foreground">
            <Bot size={12} aria-hidden="true" />
            <span>{tr(locale, "活跃 Agents", "Active Agents")}</span>
            <ArrowRight size={11} className="ml-auto text-faint-foreground" aria-hidden="true" />
          </div>
          <div className="tnum mt-1 flex items-baseline gap-1.5">
            <span className="text-title-sm font-semibold text-foreground">{status.workingCount}</span>
            <span className="text-caption text-muted-foreground">
              {tr(locale, "运行中", "working")}
            </span>
          </div>
          <p className="tnum mt-0.5 truncate text-micro text-muted-foreground">
            {tr(
              locale,
              `在线 ${status.agentsLive}/${status.agentsTotal} · 等待 ${status.waitingCount}`,
              `${status.agentsLive}/${status.agentsTotal} live · ${status.waitingCount} waiting`,
            )}
            {status.blockedCount > 0 && (
              <span className="ml-1 font-medium text-warning">
                · {tr(locale, `阻塞 ${status.blockedCount}`, `${status.blockedCount} blocked`)}
              </span>
            )}
          </p>
        </button>

        <button
          type="button"
          onClick={onShowMissions}
          className={cx(
            "rounded-control border border-surface-border/70 bg-inset px-3 py-2.5 text-left transition-colors hover:bg-surface-hover",
            FOCUS_RING_INSET,
          )}
        >
          <div className="flex items-center gap-1.5 text-micro text-faint-foreground">
            <Target size={12} aria-hidden="true" />
            <span>{tr(locale, "任务总体完成度", "Task Completion")}</span>
            <ArrowRight size={11} className="ml-auto text-faint-foreground" aria-hidden="true" />
          </div>
          <div className="tnum mt-1 flex items-baseline gap-1.5">
            <span className="text-title-sm font-semibold text-foreground">
              {progressPct !== null ? `${progressPct}%` : "—"}
            </span>
            <span className="text-caption text-muted-foreground">
              ({missionProgress.completedTasks}/{missionProgress.totalTasks})
            </span>
          </div>
          <div className="mt-1.5 h-1.5 w-full overflow-hidden rounded-full bg-muted">
            <div
              className={cx(
                "h-full rounded-full transition-[width] duration-300",
                status.haltedMissions > 0
                  ? "bg-destructive"
                  : progressPct === 100
                    ? "bg-success"
                    : "bg-info",
              )}
              style={{ width: `${progressPct ?? 0}%` }}
            />
          </div>
        </button>
      </div>
    </section>
  );
}

const PROGRESS_BADGE: Record<
  MissionProgressRow["status"],
  { tone: "info" | "danger" | "neutral"; zh: string; en: string }
> = {
  active: { tone: "info", zh: "进行中", en: "Active" },
  halted: { tone: "danger", zh: "已阻断", en: "Halted" },
  paused: { tone: "neutral", zh: "已暂停", en: "Paused" },
  idle: { tone: "neutral", zh: "待命", en: "Idle" },
};

/**
 * Main centerpiece card on Home: shows every active/open mission with
 * completion bar, completed/total ratio, landed today, and active tasks.
 */
export function MissionCompletionCard({
  model,
  onShowMissions,
  onShowFeatures,
}: {
  model: MissionProgressSummary;
  onShowMissions: () => void;
  onShowFeatures?: () => void;
}) {
  const { locale } = useLocale();
  const visibleRows = model.rows.filter(
    (row) => row.status === "active" || row.status === "halted" || row.totalTasks > 0,
  );
  const quietIdle = model.rows.filter(
    (row) => row.status !== "active" && row.status !== "halted" && row.totalTasks === 0,
  );

  return (
    <HomeCard
      icon={<Target size={14} />}
      title={tr(locale, "任务完成度", "Mission Completion")}
      meta={
        <div className="flex items-center gap-2">
          <span className="tnum text-caption text-muted-foreground">
            {tr(
              locale,
              `${model.openMissions} 个进行中 · 今日交付 ${model.landedToday}`,
              `${model.openMissions} open · ${model.landedToday} landed today`,
            )}
          </span>
          {onShowFeatures && (
            <Button variant="ghost" size="sm" onClick={onShowFeatures}>
              {tr(locale, "能力地图", "Feature Map")}
            </Button>
          )}
          <Button variant="secondary" size="sm" onClick={onShowMissions}>
            {tr(locale, "全部任务", "All Missions")}
          </Button>
        </div>
      }
    >
      {visibleRows.length === 0 ? (
        <div className="space-y-1">
          <p className="text-caption text-muted-foreground">
            {tr(locale, "当前没有进行中的任务。", "No active missions with tasks right now.")}
          </p>
          {model.closedMissions > 0 && (
            <p className="text-micro text-faint-foreground">
              {tr(locale, `另有 ${model.closedMissions} 个已关闭任务。`, `${model.closedMissions} closed missions.`)}
            </p>
          )}
        </div>
      ) : (
        <ul className="divide-y divide-surface-border">
          {visibleRows.map((row) => {
            const pct = row.completionPct ?? 0;
            const meta = PROGRESS_BADGE[row.status];
            const shownTasks = row.inFlight.slice(0, 4);
            const hiddenCount = Math.max(0, row.inFlight.length - shownTasks.length);
            return (
              <li key={row.id ?? row.name} className="py-3 first:pt-0 last:pb-0">
                <button
                  type="button"
                  onClick={onShowMissions}
                  className={cx(
                    "-mx-2 block w-[calc(100%+1rem)] rounded-control px-2 py-1.5 text-left hover:bg-surface-hover",
                    FOCUS_RING_INSET,
                  )}
                >
                  <div className="flex min-w-0 flex-wrap items-center gap-2">
                    <span className="tnum shrink-0 text-caption text-faint-foreground">#{row.id}</span>
                    <span className="min-w-0 truncate text-label font-semibold text-foreground">
                      {row.name}
                    </span>
                    <Badge tone={meta.tone}>{tr(locale, meta.zh, meta.en)}</Badge>
                    <div className="ml-auto flex shrink-0 items-baseline gap-2.5">
                      {row.landedToday > 0 && (
                        <span className="tnum text-micro text-success">
                          {tr(locale, `今日 +${row.landedToday}`, `+${row.landedToday} today`)}
                        </span>
                      )}
                      <span className="tnum text-caption font-semibold text-foreground">
                        {row.completionPct !== null ? `${row.completionPct}%` : "—"}
                        <span className="ml-1 font-normal text-muted-foreground">
                          ({row.completedTasks}/{row.totalTasks})
                        </span>
                      </span>
                    </div>
                  </div>

                  <div className="mt-2 h-2 w-full overflow-hidden rounded-full bg-inset">
                    <div
                      className={cx(
                        "h-full rounded-full transition-[width] duration-300",
                        row.status === "halted"
                          ? "bg-destructive"
                          : pct === 100
                            ? "bg-success"
                            : "bg-info",
                      )}
                      style={{ width: `${pct}%` }}
                    />
                  </div>

                  {row.haltedReason && (
                    <p className="mt-1.5 truncate text-caption text-destructive">
                      {row.haltedReason}
                    </p>
                  )}

                  {shownTasks.length > 0 && (
                    <ul className="mt-2 space-y-1">
                      {shownTasks.map((task) => (
                        <li key={task.task_id ?? task.title} className="flex min-w-0 items-baseline gap-2 text-caption">
                          <span className="tnum shrink-0 rounded-chip bg-inset px-1.5 py-px font-mono text-micro text-muted-foreground shadow-hairline">
                            T{task.task_id}
                          </span>
                          <span className="min-w-0 flex-1 truncate text-foreground">
                            {task.title ?? tr(locale, "未命名任务", "untitled")}
                          </span>
                          {task.conflictText && (
                            <span className="shrink-0 font-medium text-warning">{task.conflictText}</span>
                          )}
                          <span className="tnum shrink-0 text-muted-foreground">
                            {task.state}
                            {task.since_ms !== null && ` ${sinceText(task.since_ms)}`}
                          </span>
                        </li>
                      ))}
                      {hiddenCount > 0 && (
                        <li className="tnum text-micro text-faint-foreground">
                          {tr(locale, `还有 ${hiddenCount} 项进行中任务…`, `+${hiddenCount} more in flight…`)}
                        </li>
                      )}
                    </ul>
                  )}
                </button>
              </li>
            );
          })}
        </ul>
      )}
      {(quietIdle.length > 0 || model.closedMissions > 0) && visibleRows.length > 0 && (
        <p className="border-t border-surface-border pt-2 text-caption text-faint-foreground">
          {quietIdle.length > 0 &&
            tr(
              locale,
              `另有 ${quietIdle.length} 个待命任务（${quietIdle.map((r) => `#${r.id} ${r.name}`).join("、")}）`,
              `Idle: ${quietIdle.map((r) => `#${r.id} ${r.name}`).join(", ")}`,
            )}
          {quietIdle.length > 0 && model.closedMissions > 0 && " · "}
          {model.closedMissions > 0 &&
            tr(locale, `${model.closedMissions} 个已结束`, `${model.closedMissions} closed`)}
        </p>
      )}
    </HomeCard>
  );
}

export function NeedsYou({
  items,
  onOpenAgent,
  showJudgeHint = false,
}: {
  items: NeedItem[];
  onOpenAgent: (session: string) => void;
  showJudgeHint?: boolean;
}) {
  const { locale } = useLocale();
  const act = items.filter((item) => item.tone === "act").length;
  if (items.length === 0) {
    return (
      <Card as="section">
        <EmptyState
          bare
          title={tr(locale, "暂无待处理项。", "Nothing needs you.")}
          icon={<CircleCheck className="text-success" />}
        />
      </Card>
    );
  }
  return (
    <HomeCard
      icon={<CircleAlert size={14} className={act ? "text-destructive" : undefined} />}
      title={tr(locale, "待你处理", "Action Required")}
      meta={<span className="tnum text-caption text-muted-foreground">{needsSummary(items, locale)}</span>}
    >
      <ul className="divide-y divide-surface-border">
        {items.map((item) => (
          <li key={item.key} className="flex min-w-0 flex-wrap items-start gap-x-3 gap-y-2 py-2.5 first:pt-0 last:pb-0">
            {item.tone === "act" ? (
              <Flag size={13} className="mt-0.5 shrink-0 text-destructive" aria-label={tr(locale, "待处理", "to act on")} />
            ) : (
              <Info size={13} className="mt-0.5 shrink-0 text-faint-foreground" aria-label={tr(locale, "仅知悉", "to know")} />
            )}
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-baseline gap-2">
                <span className="min-w-0 truncate text-label font-medium text-foreground">{item.title}</span>
                {item.since_ms !== null && (
                  <span className="tnum shrink-0 text-caption text-faint-foreground">{sinceText(item.since_ms)}</span>
                )}
              </div>
              <NeedDetail item={item} />
              {showJudgeHint && item.judge && (
                <p className="mt-0.5 min-w-0 truncate text-caption text-faint-foreground">
                  {tr(locale, "误报或已过时？", "Should not have been raised?")}{" "}
                  <code className="rounded-chip bg-inset px-1 font-mono text-micro text-muted-foreground select-all">
                    {item.judge}
                  </code>
                </p>
              )}
            </div>
            <div className="flex min-w-0 max-w-full shrink-0 flex-wrap items-center gap-2 self-center">
              {item.command && <CopyCommand command={item.command} />}
              {item.resume && (
                <ResumeButton key={`${item.resume.mission}:${item.resume.halt}`} resume={item.resume} />
              )}
              {item.session && (
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => onOpenAgent(item.session ?? "")}
                  iconRight={<ArrowRight size={12} aria-hidden="true" />}
                >
                  {tr(locale, "查看", "Open")}
                </Button>
              )}
            </div>
          </li>
        ))}
      </ul>
    </HomeCard>
  );
}

export function AgentsNowCard({ model, onOpenAgent }: { model: AgentsNow; onOpenAgent: (session: string) => void }) {
  const { locale } = useLocale();
  return (
    <HomeCard
      icon={<Bot size={14} />}
      title={tr(locale, "运行中的 Agents", "Working now")}
      meta={<Badge tone="neutral" count={model.working.length} />}
    >
      {model.working.length === 0 ? (
        <p className="text-caption text-muted-foreground">
          {tr(locale, "当前没有正在执行任务的 Agent。", "No agent is working.")}
        </p>
      ) : (
        <ul className="divide-y divide-surface-border">
          {model.working.map((agent) => (
            <li key={agent.session_id}>
              <button
                type="button"
                onClick={() => onOpenAgent(agent.session_id)}
                className={cx(
                  "-mx-2 grid w-[calc(100%+1rem)] grid-cols-1 items-baseline gap-x-3 rounded-control px-2 py-2 text-left hover:bg-surface-hover sm:grid-cols-[minmax(0,14rem)_minmax(0,1fr)_minmax(0,16rem)]",
                  FOCUS_RING_INSET,
                )}
              >
                <span className="inline-flex min-w-0 items-center gap-2">
                  <PixelGridLoader tone="success" size="xs" />
                  <span className="truncate text-label font-medium text-foreground">{agent.name}</span>
                </span>
                <span className="truncate text-caption text-muted-foreground" title={agent.task ?? undefined}>
                  {agent.task ?? tr(locale, "无绑定任务", "no task")}
                </span>
                <span className="tnum flex min-w-0 items-center gap-1.5 text-caption text-foreground sm:justify-end">
                  <ToolChip label={agent.doing} tone="active" />
                  {agent.since_ms !== null && (
                    <span className="shrink-0 text-faint-foreground">{sinceText(agent.since_ms)}</span>
                  )}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
      {model.idle.length > 0 && (
        <p className="border-t border-surface-border pt-2 text-caption text-faint-foreground">
          {tr(locale, "空闲：", "Idle: ")}
          {model.idle.map((agent, index) => (
            <span key={agent.session_id}>
              {index > 0 && ", "}
              <button
                type="button"
                onClick={() => onOpenAgent(agent.session_id)}
                className={cx("rounded-chip text-muted-foreground hover:text-foreground hover:underline", FOCUS_RING)}
              >
                {agent.name}
              </button>
              {agent.since_ms !== null && ` ${sinceText(agent.since_ms)}`}
            </span>
          ))}
        </p>
      )}
    </HomeCard>
  );
}

export function SupervisorNotes({
  model,
  all,
  talk,
}: {
  model: NotesShown;
  all: NoteRow[];
  talk: ReactNode;
}) {
  const { locale } = useLocale();
  const [open, setOpen] = useState(false);
  const shown = open ? all : model.shown;
  const counts = [
    model.more > 0 ? tr(locale, `还有 ${model.more} 条`, `${model.more} more`) : null,
    model.earlier > 0 ? tr(locale, `更早 ${model.earlier} 条`, `${model.earlier} earlier`) : null,
  ].filter((part): part is string => part !== null);
  return (
    <HomeCard
      icon={<MessageSquareText size={14} />}
      title={tr(locale, "主控备注", "From the supervisor")}
      meta={
        (counts.length > 0 || model.settled > 0) && (
          <Button variant="ghost" size="sm" onClick={() => setOpen((value) => !value)} aria-expanded={open}>
            {open
              ? tr(locale, "仅看最新", "Show the latest")
              : counts.length > 0
                ? counts.join(" · ")
                : tr(locale, `${model.settled} 条已结束`, `${model.settled} settled`)}
          </Button>
        )
      }
    >
      {shown.length === 0 ? (
        <p className="text-caption text-muted-foreground">
          {tr(locale, "近 6 小时无新增主控备注。", "Nothing new in the last 6 hours.")}
        </p>
      ) : (
        <ul className="divide-y divide-surface-border">
          {shown.map((note, index) => {
            const row = open ? (note as NoteRow) : null;
            const states = row ? row.tasks : (note as NotesShown["shown"][number]).taskStates;
            const earlier = row !== null && !row.current;
            return (
              <li key={`${note.at_ms}-${index}`} className="flex min-w-0 items-baseline gap-3 py-2 first:pt-0">
                <div className="min-w-0 flex-1">
                  <p
                    className={cx(
                      "text-caption",
                      row?.settled || earlier ? "text-muted-foreground" : "text-foreground",
                      !open && "truncate",
                    )}
                    title={note.text}
                  >
                    {note.text}
                  </p>
                  {(row?.settled || earlier || states) && (
                    <p className="mt-0.5 text-micro text-faint-foreground">
                      {earlier && (
                        <Badge
                          tone="neutral"
                          className="mr-1.5"
                          title="Said in an earlier turn"
                        >
                          earlier
                        </Badge>
                      )}
                      {row?.settled && (
                        <Badge tone="neutral" className="mr-1.5" title="Its tasks have all ended">
                          settled
                        </Badge>
                      )}
                      {states}
                    </p>
                  )}
                </div>
                <span className="tnum shrink-0 text-caption text-faint-foreground">{relativeTime(note.at_ms)}</span>
              </li>
            );
          })}
        </ul>
      )}
      <div className="border-t border-surface-border pt-3">{talk}</div>
    </HomeCard>
  );
}

/** Where the page keeps when it was last seen (the browser's own storage). */
const SEEN_KEY = "spawn.seen";

function readSeen(): number | null {
  try {
    const value = Number(window.localStorage.getItem(SEEN_KEY));
    return Number.isFinite(value) && value > 0 ? value : null;
  } catch {
    return null;
  }
}

function writeSeen(at: number) {
  try {
    window.localStorage.setItem(SEEN_KEY, String(at));
  } catch {
    // Ignore storage errors in private mode.
  }
}

export interface Away {
  from: number;
  at: number;
}

export function useAway(): Away | null {
  const [away, setAway] = useState<Away | null>(() => {
    const now = Date.now();
    const from = awayFrom(readSeen(), now);
    return from === null ? null : { from, at: now };
  });
  useEffect(() => {
    const looking = () => document.visibilityState === "visible" && document.hasFocus();
    const seen = () => writeSeen(Date.now());
    const back = () => {
      const now = Date.now();
      const from = awayFrom(readSeen(), now);
      if (from !== null) setAway({ from, at: now });
      seen();
    };
    const onVisibility = () => {
      if (document.visibilityState === "hidden") seen();
      else if (looking()) back();
    };
    if (looking()) seen();
    const timer = window.setInterval(() => {
      if (looking()) seen();
    }, 60_000);
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("focus", back);
    window.addEventListener("blur", seen);
    window.addEventListener("pagehide", seen);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("focus", back);
      window.removeEventListener("blur", seen);
      window.removeEventListener("pagehide", seen);
    };
  }, []);
  return away;
}

const DIGEST_TONE: Record<string, string> = {
  landed: "text-success",
  "sent back": "text-warning",
  stopped: "text-muted-foreground",
  moved: "text-muted-foreground",
  mission: "text-info",
  alert: "text-destructive",
};

function awaySpan(ms: number): string {
  const hours = Math.round(ms / 3_600_000);
  if (hours < 48) return `${hours} h`;
  const days = Math.floor(hours / 24);
  const rest = hours % 24;
  return rest ? `${days} d ${rest} h` : `${days} d`;
}

export function DigestCard({ digest, back }: { digest: Digest; back: number | null }) {
  const { locale } = useLocale();
  const [open, setOpen] = useState(false);
  const title = digest.away
    ? tr(
        locale,
        `离开期间 · ${awaySpan((back ?? Date.now()) - digest.since_ms)}`,
        `Since you were away · ${awaySpan((back ?? Date.now()) - digest.since_ms)}`,
      )
    : tr(locale, "今日动态", "Today");
  return (
    <Card as="section">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
        title={digest.away ? `Since ${new Date(digest.since_ms).toLocaleString()}` : "The last 24 hours"}
        className={cx("-m-2 flex min-h-10 w-[calc(100%+1rem)] items-center gap-2 rounded-control px-2 text-left hover:bg-surface-hover", FOCUS_RING_INSET)}
      >
        <History size={14} className="shrink-0 text-faint-foreground" aria-hidden="true" />
        <span className="shrink-0 text-label font-medium text-foreground">{title}</span>
        <span className="tnum min-w-0 truncate text-caption text-muted-foreground">
          {tr(
            locale,
            `${digest.landed} 交付 · ${digest.sentBack} 打回 · ${digest.alerts} 告警`,
            `${digest.landed} landed · ${digest.sentBack} sent back · ${digest.alerts} alert${digest.alerts === 1 ? "" : "s"}`,
          )}
        </span>
        <span className="ml-auto shrink-0 text-caption text-muted-foreground">
          {open ? tr(locale, "收起", "Hide") : tr(locale, "展开", "Show")}
        </span>
      </button>
      {open &&
        (digest.bands.length === 0 ? (
          <p className="border-t border-surface-border pt-3 text-caption text-faint-foreground">
            {tr(locale, "该时间窗内无变更记录。", "Nothing happened in this window.")}
          </p>
        ) : (
          <div className="-mx-4 -mb-4 max-h-96 overflow-y-auto border-t border-surface-border pb-2">
            {digest.bands.map((band) => (
              <section key={band.label} aria-label={band.label}>
                <h3 className="sticky top-0 bg-surface px-4 pt-2.5 pb-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
                  {band.label}
                  <span className="tnum ml-1.5 font-normal tracking-normal">{band.entries.length}</span>
                </h3>
                <ul className="divide-y divide-surface-border">
                  {band.entries.map((entry, index) => (
                    <li key={`${entry.at_ms}-${index}`} className="flex min-w-0 items-baseline gap-3 px-4 py-1.5 text-caption">
                      <span className="tnum w-16 shrink-0 whitespace-nowrap text-faint-foreground">
                        {new Date(entry.at_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
                      </span>
                      <span className={cx("w-16 shrink-0 font-medium", DIGEST_TONE[entry.kind])}>{entry.kind}</span>
                      <span className="min-w-0 flex-1 truncate text-foreground" title={entry.text}>
                        {entry.text}
                      </span>
                    </li>
                  ))}
                </ul>
              </section>
            ))}
          </div>
        ))}
    </Card>
  );
}

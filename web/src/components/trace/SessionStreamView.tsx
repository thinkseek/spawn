import {
  AlertTriangle,
  Brain,
  ChevronDown,
  ChevronRight,
  Clock,
  FilePenLine,
  FileSearch,
  GitCommitHorizontal,
  Info,
  MessageSquare,
  Play,
  Repeat,
  Terminal,
  Wrench,
} from "lucide-react";
import { useMemo, useState } from "react";
import type { ReactNode } from "react";

import type { TimelineEvent } from "../../api";
import { pairs } from "../../eventFormat";
import { clockTime, formatDuration } from "../../format";
import {
  buildSessionStream,
  filterStreamSteps,
  type StreamFilter,
  type StreamStep,
  type StreamStepKind,
  type StreamTurn,
} from "../../sessionStream";
import type { KnownCommand, KnownExecution } from "../../trace";
import { EmptyState, FOCUS_RING_INSET, cx, tr, useLocale, type Locale } from "../ui";
import { useNowWhile } from "../timeaxis/useTimeWindow";

function stepVisual(kind: StreamStepKind, locale: Locale): {
  icon: ReactNode;
  label: string;
  tone: string;
} {
  switch (kind) {
    case "file_edit":
      return {
        icon: <FilePenLine size={13} />,
        label: tr(locale, "改文件", "Edit"),
        tone: "bg-ok-tint text-success",
      };
    case "file_read":
      return {
        icon: <FileSearch size={13} />,
        label: tr(locale, "读/查", "Read"),
        tone: "bg-inset text-muted-foreground",
      };
    case "shell":
      return {
        icon: <Terminal size={13} />,
        label: tr(locale, "命令", "Shell"),
        tone: "bg-muted text-foreground",
      };
    case "thinking":
      return {
        icon: <Brain size={13} />,
        label: tr(locale, "思考", "Thinking"),
        tone: "bg-wait-tint text-warning",
      };
    case "assistant":
      return {
        icon: <MessageSquare size={13} />,
        label: tr(locale, "回复", "Assistant"),
        tone: "bg-muted text-info",
      };
    case "checkpoint":
      return {
        icon: <GitCommitHorizontal size={13} />,
        label: tr(locale, "提交点", "Commit"),
        tone: "bg-ok-tint text-success",
      };
    case "prompt":
      return {
        icon: <Play size={13} />,
        label: tr(locale, "指令", "Prompt"),
        tone: "bg-muted text-foreground",
      };
    case "error":
      return {
        icon: <AlertTriangle size={13} />,
        label: tr(locale, "异常", "Error"),
        tone: "bg-fail-tint text-destructive",
      };
    case "job":
      return {
        icon: <Clock size={13} />,
        label: tr(locale, "后台任务", "Job"),
        tone: "bg-inset text-muted-foreground",
      };
    default:
      return {
        icon: <Wrench size={13} />,
        label: tr(locale, "工具", "Tool"),
        tone: "bg-inset text-muted-foreground",
      };
  }
}

function StepRow({ step, locale }: { step: StreamStep; locale: Locale }) {
  const [expanded, setExpanded] = useState(false);
  const vis = stepVisual(step.kind, locale);

  return (
    <li className="group border-b border-surface-border/70 last:border-b-0">
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
        className={cx(
          "flex w-full min-w-0 items-start gap-2.5 px-3 py-2 text-left transition-colors hover:bg-surface-hover sm:px-4",
          FOCUS_RING_INSET,
        )}
      >
        <span className="tnum mt-0.5 w-7 shrink-0 font-mono text-micro text-faint-foreground">
          #{step.index}
        </span>

        <span
          className={cx(
            "mt-0.5 inline-flex shrink-0 items-center gap-1 rounded-chip px-1.5 py-px text-micro font-medium",
            step.status === "failed" ? "bg-fail-tint text-destructive" : vis.tone,
          )}
        >
          {vis.icon}
          {vis.label}
        </span>

        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
            <span className="font-mono text-caption font-semibold text-foreground">
              {step.title}
            </span>
            {step.summary && (
              <span className="min-w-0 truncate font-mono text-caption text-muted-foreground">
                {step.summary}
              </span>
            )}
          </div>
          {step.body && !expanded && (
            <p className="mt-0.5 line-clamp-2 break-words text-caption text-faint-foreground">
              {step.body}
            </p>
          )}
        </div>

        <div className="ml-auto flex shrink-0 items-center gap-1.5">
          {step.repeatedCount >= 2 && (
            <span
              className="tnum inline-flex items-center gap-0.5 rounded-chip bg-wait-tint px-1.5 py-px text-micro font-medium text-warning"
              title={tr(locale, "连续重复执行相同操作（长程循环信号）", "Consecutive identical step (possible loop)")}
            >
              <Repeat size={10} aria-hidden="true" />
              ×{step.repeatedCount}
            </span>
          )}
          {step.slow && (
            <span
              className="inline-flex items-center gap-0.5 rounded-chip bg-wait-tint px-1.5 py-px text-micro font-medium text-warning"
              title={tr(locale, "耗时 ≥ 30s，属于慢步骤", "Duration >= 30s (slow step)")}
            >
              <Clock size={10} aria-hidden="true" />
              {tr(locale, "慢", "Slow")}
            </span>
          )}
          {step.status === "failed" && step.outcome && (
            <span className="max-w-28 truncate rounded-chip bg-fail-tint px-1.5 py-px font-mono text-micro font-medium text-destructive">
              {step.outcome}
            </span>
          )}
          <span
            className={cx(
              "tnum min-w-12 text-right font-mono text-micro",
              step.status === "running" ? "text-info font-medium" : "text-muted-foreground",
            )}
          >
            {step.status === "running"
              ? tr(locale, "进行中", "running")
              : step.durationMs !== null
                ? formatDuration(step.durationMs)
                : clockTime(step.startMs)}
          </span>
          <span className="text-faint-foreground">
            {expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
          </span>
        </div>
      </button>

      {expanded && (
        <div className="border-t border-surface-border/60 bg-inset/60 px-3 py-2.5 text-caption sm:px-4">
          {step.body && (
            <pre className="mb-2 max-h-60 overflow-auto whitespace-pre-wrap break-words rounded border border-surface-border bg-surface p-2 font-mono text-micro text-foreground">
              {step.body}
            </pre>
          )}
          <div className="space-y-1.5">
            {step.events.map((ev) => (
              <div key={ev.event_id} className="rounded border border-surface-border/70 bg-surface px-2.5 py-1.5">
                <div className="flex flex-wrap items-center gap-2 text-micro">
                  <span className="tnum font-mono text-faint-foreground">#{ev.session_seq}</span>
                  <span className="font-mono font-medium text-foreground">{ev.event_name}</span>
                  <span className="rounded bg-inset px-1 font-mono text-faint-foreground">{ev.source}</span>
                  <span className="tnum ml-auto font-mono text-faint-foreground">{clockTime(ev.recorded_at_ms)}</span>
                </div>
                {pairs(ev.detail).length > 0 && (
                  <dl className="mt-1 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-0.5 font-mono text-micro">
                    {pairs(ev.detail).map(([k, v], idx) => (
                      <div key={`${k}:${idx}`} className="contents">
                        <dt className="text-faint-foreground">{k || "detail"}</dt>
                        <dd className="break-words text-muted-foreground">{v}</dd>
                      </div>
                    ))}
                  </dl>
                )}
              </div>
            ))}
          </div>
        </div>
      )}
    </li>
  );
}

function TurnSection({
  turn,
  filter,
  locale,
}: {
  turn: StreamTurn;
  filter: StreamFilter;
  locale: Locale;
}) {
  const visible = useMemo(() => filterStreamSteps(turn.steps, filter), [turn.steps, filter]);

  return (
    <section className="overflow-hidden rounded-card border border-surface-border bg-surface shadow-card">
      <header className="flex flex-wrap items-center gap-2 border-b border-surface-border bg-inset/40 px-3 py-2 sm:px-4">
        <span className="tnum rounded-chip bg-surface px-2 py-px text-micro font-semibold text-foreground shadow-hairline">
          {tr(locale, `第 ${turn.turnNumber} 轮`, `Turn ${turn.turnNumber}`)}
        </span>
        <span className="tnum font-mono text-micro text-faint-foreground">
          {clockTime(turn.startMs)}
        </span>
        {turn.executionId && (
          <span className="hidden rounded bg-inset px-1.5 py-px font-mono text-micro text-faint-foreground sm:inline">
            {turn.executionId}
          </span>
        )}
        <span
          className={cx(
            "rounded-chip px-1.5 py-px text-micro font-medium",
            turn.status === "failed"
              ? "bg-fail-tint text-destructive"
              : turn.status === "running"
                ? "bg-muted text-info"
                : "bg-ok-tint text-success",
          )}
        >
          {turn.status === "failed"
            ? tr(locale, "异常结束", "Failed")
            : turn.status === "running"
              ? tr(locale, "执行中", "Running")
              : tr(locale, "完成", "Done")}
        </span>

        <div className="ml-auto flex flex-wrap items-center gap-3 text-micro text-muted-foreground">
          <span className="tnum">
            {tr(locale, `${turn.steps.length} 步`, `${turn.steps.length} steps`)}
          </span>
          {(turn.inputTokens > 0 || turn.outputTokens > 0) && (
            <span className="tnum font-mono">
              {turn.inputTokens.toLocaleString()} in / {turn.outputTokens.toLocaleString()} out
            </span>
          )}
          <span className="tnum font-mono font-semibold text-foreground">
            {turn.durationMs !== null ? formatDuration(turn.durationMs) : "—"}
          </span>
        </div>
      </header>

      {turn.prompt && (
        <div className="border-b border-surface-border/70 bg-inset/25 px-3 py-2 text-caption text-muted-foreground sm:px-4">
          <span className="mr-1.5 font-medium text-foreground">
            {tr(locale, "输入指令：", "Prompt:")}
          </span>
          <span className="break-words">{turn.prompt}</span>
        </div>
      )}

      {turn.coarseOnly && (
        <div className="flex items-start gap-2 border-b border-surface-border/70 bg-wait-tint/40 px-3 py-2 text-micro text-warning sm:px-4">
          <Info size={13} className="mt-0.5 shrink-0" aria-hidden="true" />
          <span>
            {tr(
              locale,
              "本轮当前仅采集到轮次起止与时长，未写入工具调用 / 文件改动 / 思考过程等细粒度步骤（需后端事件统一采集支持）。",
              "Only coarse turn boundaries were recorded for this turn; fine-grained tool, thinking, and file-edit events are missing from the backend log.",
            )}
          </span>
        </div>
      )}

      {visible.length === 0 ? (
        <p className="px-4 py-3 text-caption text-faint-foreground">
          {tr(locale, "当前筛选条件下本轮无匹配步骤。", "No steps in this turn match the active filter.")}
        </p>
      ) : (
        <ol>
          {visible.map((step) => (
            <StepRow key={step.id} step={step} locale={locale} />
          ))}
        </ol>
      )}
    </section>
  );
}

/**
 * Sequential step-by-step evaluation feed for an Agent Session:
 * renders every captured turn and action in chronological order with exact
 * durations, category badges, slow/loop flags, and expandable raw details.
 */
export function SessionStreamView({
  events,
  executions,
  commands,
  to = null,
  header,
}: {
  events: TimelineEvent[];
  executions: readonly KnownExecution[];
  commands: ReadonlyMap<string, KnownCommand>;
  to?: string | null;
  header: ReactNode;
}) {
  const { locale } = useLocale();
  const [filter, setFilter] = useState<StreamFilter>("all");
  const hasRunning = executions.some((e) => e.ended_at_ms === null);
  const now = useNowWhile(hasRunning);

  const model = useMemo(
    () => buildSessionStream(events, now, executions, commands, to),
    [events, now, executions, commands, to],
  );
  const s = model.summary;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex min-h-11 shrink-0 flex-wrap items-center gap-2 border-b border-surface-border px-4 py-1.5 sm:px-5">
        {header}
        <div
          role="group"
          aria-label={tr(locale, "步骤筛选", "Step filter")}
          className="ml-auto flex items-center gap-1"
        >
          {(
            [
              ["all", tr(locale, "全部步骤", "All")],
              ["key", tr(locale, "关键动作", "Key")],
              ["issues", tr(locale, "异常/慢步骤", "Issues")],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              onClick={() => setFilter(id)}
              aria-pressed={filter === id}
              className={cx(
                "rounded-chip px-2 py-0.5 text-micro font-medium transition-colors",
                FOCUS_RING_INSET,
                filter === id
                  ? "bg-surface-selected text-foreground shadow-hairline"
                  : "text-muted-foreground hover:bg-surface-hover hover:text-foreground",
              )}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      {/* Summary metrics bar for evaluating long-horizon behavior */}
      <div className="flex shrink-0 flex-wrap items-center gap-x-4 gap-y-1.5 border-b border-surface-border bg-inset/40 px-4 py-2 text-micro sm:px-5">
        <span className="tnum text-muted-foreground">
          <strong className="font-semibold text-foreground">{s.totalTurns}</strong>{" "}
          {tr(locale, "轮次", "turns")}
        </span>
        <span className="tnum text-muted-foreground">
          <strong className="font-semibold text-foreground">{s.totalSteps}</strong>{" "}
          {tr(locale, "步骤", "steps")}
        </span>
        <span className="tnum text-muted-foreground">
          <strong className="font-semibold text-success">{s.fileEdits}</strong>{" "}
          {tr(locale, "改文件", "edits")}
        </span>
        <span className="tnum text-muted-foreground">
          <strong className="font-semibold text-foreground">{s.shellRuns}</strong>{" "}
          {tr(locale, "命令", "shell")}
        </span>
        {s.failedSteps > 0 && (
          <span className="tnum rounded-chip bg-fail-tint px-1.5 py-px font-medium text-destructive">
            {tr(locale, `${s.failedSteps} 失败`, `${s.failedSteps} failed`)}
          </span>
        )}
        {s.slowSteps > 0 && (
          <span className="tnum rounded-chip bg-wait-tint px-1.5 py-px font-medium text-warning">
            {tr(locale, `${s.slowSteps} 慢步骤`, `${s.slowSteps} slow`)}
          </span>
        )}
        {s.repeatedSteps > 0 && (
          <span className="tnum rounded-chip bg-wait-tint px-1.5 py-px font-medium text-warning">
            {tr(locale, `${s.repeatedSteps} 疑似循环`, `${s.repeatedSteps} repeated`)}
          </span>
        )}
        {s.coarseTurns > 0 && (
          <span className="tnum rounded-chip bg-muted px-1.5 py-px text-muted-foreground">
            {tr(locale, `${s.coarseTurns} 轮仅含粗粒度耗时`, `${s.coarseTurns} coarse-only turn(s)`)}
          </span>
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto p-3 sm:p-4">
        {model.turns.length === 0 ? (
          <EmptyState
            title={tr(locale, "暂无可展示的步骤流", "No sequential steps captured yet")}
            hint={tr(
              locale,
              "可切换至「原始事件」查看底层事件日志。",
              "Switch to Raw List to inspect underlying log records.",
            )}
          />
        ) : (
          <div className="mx-auto flex max-w-[1000px] flex-col gap-3">
            {model.turns.map((turn) => (
              <TurnSection key={turn.id} turn={turn} filter={filter} locale={locale} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

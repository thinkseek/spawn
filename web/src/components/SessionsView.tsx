import { PanelRight } from "lucide-react";

import type { Freshness, Observation, OverviewSession, RsiSummary } from "../api";
import type { SessionTail } from "../api";
import { inputTarget } from "../commands";
import { relativeTime } from "../format";
import { DetailsRail, ExecutionSummary, RailContent } from "./DetailsRail";
import { Timeline } from "./Timeline";
import { Button, Drawer, LiveStatus, PageHeader, tr, useLocale } from "./ui";

export interface SessionsViewProps {
  sessions: OverviewSession[];
  detail: SessionTail | null;
  sessionError: string | null;
  freshness: Freshness | null;
  error: string | null;
  agentId: string | null;
  sessionId: string | null;
  sessionName: string | null;
  rsi: RsiSummary | null;
  observation: Observation | null;
  selectedId: string | null;
  pollNames: Map<string, string>;
  railOpen: boolean;
  onSelectSession: (sessionId: string) => void;
  onSetRailOpen: (open: boolean) => void;
}

/** Lazy-loaded Session timeline & trace inspection workspace (`#/sessions`). */
export function SessionsView({
  sessions,
  detail,
  sessionError,
  freshness,
  error,
  agentId,
  sessionId,
  sessionName,
  rsi,
  observation,
  selectedId,
  pollNames,
  railOpen,
  onSelectSession,
  onSetRailOpen,
}: SessionsViewProps) {
  const { locale } = useLocale();

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      <main className="flex min-w-0 flex-1 flex-col bg-page-canvas">
        <PageHeader
          title={
            <>
              <h1
                className="min-w-0 truncate text-body font-semibold text-foreground"
                title={[sessionName, agentId, sessionId].filter(Boolean).join("\n") || undefined}
              >
                {sessionName ?? agentId ?? sessionId ?? tr(locale, "未选择会话", "No session selected")}
              </h1>
              {sessionId && (sessionName || agentId) && (
                <span className="hidden truncate rounded bg-muted px-1.5 py-px font-mono text-micro text-muted-foreground select-all sm:inline">
                  {sessionId}
                </span>
              )}
              {/* No judged state here: the header says what is
                  recorded (the execution, ExecutionSummary), and the
                  judgement, which can lag it ("Completed" beside a
                  running execution), is in the Observation card. */}
            </>
          }
        >
          <span className="tnum hidden text-caption text-muted-foreground lg:inline">
            {freshness
              ? tr(locale, `${freshness.event_count} 条事件`, `${freshness.event_count} events`)
              : tr(locale, "0 条事件", "0 events")}
            <span className="mx-1.5 text-faint-foreground">·</span>
            {freshness
              ? tr(
                  locale,
                  `最近 ${relativeTime(freshness.last_event_at_ms)}`,
                  `last ${relativeTime(freshness.last_event_at_ms)}`,
                )
              : tr(locale, "暂无事件", "no events yet")}
          </span>
          <ExecutionSummary detail={detail} className="hidden md:inline-flex" />
          <LiveStatus error={error} label="" />
          <Button
            onClick={() => onSetRailOpen(true)}
            className="lg:hidden"
            title={tr(locale, "会话详情", "Session details")}
          >
            <PanelRight size={13} />
            {tr(locale, "详情", "Details")}
          </Button>
        </PageHeader>

        {sessions.length > 1 && (
          <div className="flex shrink-0 gap-1.5 overflow-x-auto border-b border-surface-border px-3 py-2 md:hidden">
            {sessions.map((entry) => (
              <button
                key={entry.session_id}
                type="button"
                onClick={() => onSelectSession(entry.session_id)}
                className={`shrink-0 rounded-md px-2.5 py-1 text-caption ${
                  entry.session_id === selectedId
                    ? "bg-surface-selected text-foreground"
                    : "text-muted-foreground"
                }`}
              >
                {pollNames.get(entry.session_id) ?? (entry.agent_id || entry.session_id)}
              </button>
            ))}
          </div>
        )}

        {!selectedId ? (
          <div className="grid min-h-0 flex-1 place-items-center px-6">
            <p className="text-caption text-faint-foreground">
              {tr(locale, "请等待或选择一个会话。", "Waiting for a session.")}
            </p>
          </div>
        ) : detail ? (
          <Timeline
            key={selectedId}
            sessionId={detail.session_id}
            tail={detail.window}
            executions={detail.executions}
            to={inputTarget(detail.session_id, pollNames.get(detail.session_id) ?? null, detail.agent_id)}
            reassignedTo={detail.reassigned_to}
            reassignedFrom={detail.reassigned_from}
            onOpenSession={onSelectSession}
          />
        ) : (
          <div className="grid min-h-0 flex-1 place-items-center px-6">
            <p className="text-caption text-faint-foreground">
              {sessionError
                ? tr(locale, "无法加载该会话。", "Could not load this session.")
                : tr(locale, "正在加载会话…", "Loading session…")}
            </p>
          </div>
        )}
      </main>

      <DetailsRail
        className="hidden lg:flex"
        rsi={rsi}
        detail={detail}
        observation={observation}
      />
      {railOpen && (
        <Drawer
          width="max-w-sm"
          onClose={() => onSetRailOpen(false)}
          header={
            <h2 className="text-title-sm font-semibold text-foreground">
              {tr(locale, "会话详情", "Session details")}
            </h2>
          }
        >
          <div className="min-h-0 flex-1 space-y-3 overflow-y-auto bg-app-shell p-3">
            <RailContent
              rsi={rsi}
              detail={detail}
              observation={observation}
              withExecution
            />
          </div>
        </Drawer>
      )}
    </div>
  );
}

import {
  ArrowRight,
  CircleAlert,
  History,
  Send,
  Sparkles,
} from "lucide-react";
import { useState } from "react";

import { usePoll } from "../../api";
import type {
  CommandRecord,
  FleetAgent,
  Judgement,
  JudgementRecord,
} from "../../fleet";
import {
  FLEET_STATES,
  allowedStates,
  ambiguityQuestion,
  ambiguityText,
  fetchAgentPane,
  fetchCommands,
  fetchJudgements,
  judgeKind,
  percent,
  reasonText,
  stateLabelUpper,
} from "../../fleet";
import { clockTime, formatDuration, relativeTime } from "../../format";
import { freshAt } from "../../validated";
import { LiveBadge, TerminalBlock, TerminalPanel } from "../Terminal";
import { Ago, Button, Card, CardHeader, Drawer, EmptyState, cx } from "../ui";
import {
  Fact,
  FleetStateBadge,
  FleetStateDot,
  JudgeBadge,
  Meter,
  RoleBadge,
  SectionLabel,
} from "./bits";
import { DoingIcon, doingNow } from "./FleetBoard";
import { InputBox } from "./InputBox";

interface AgentDetailProps {
  agent: FleetAgent;
  onClose: () => void;
  onOpenTimeline: (sessionId: string) => void;
}

/** Slide-over with everything about one agent; its polls stop on close. */
export function AgentDetail({ agent, onClose, onOpenTimeline }: AgentDetailProps) {
  const sessionId = agent.session_id;
  const judgements = usePoll<JudgementRecord[]>(
    `judgements:${sessionId}`,
    (signal) => fetchJudgements(sessionId, signal),
    4_000,
  );
  const commands = usePoll<CommandRecord[]>(
    `commands:${sessionId}`,
    (signal) => fetchCommands(sessionId, signal),
    5_000,
  );
  // A judgement picked from the history; null follows the latest.
  const [pinnedSeq, setPinnedSeq] = useState<number | null>(null);
  const pinned =
    pinnedSeq == null
      ? null
      : (judgements.data?.find((entry) => entry.session_seq === pinnedSeq) ??
        null);

  const shown: { judgement: Judgement | null; at: number | null; seq: number | null } =
    pinned
      ? {
          judgement: pinned.observation,
          at: pinned.recorded_at_ms,
          seq: pinned.session_seq,
        }
      : {
          judgement: agent.judgement,
          at: agent.judged_at_ms,
          seq: agent.judgement?.based_on_seq ?? null,
        };

  return (
    <Drawer
      width="max-w-6xl"
      onClose={onClose}
      header={
        <>
          <FleetStateDot state={agent.state} />
          <h2 className="min-w-0 truncate text-title-sm font-semibold text-foreground">
            {agent.agent_id || sessionId}
          </h2>
          <RoleBadge role={agent.role} />
          <FleetStateBadge state={agent.state} />
          <LiveBadge live={agent.live} />
          {agent.attention && (
            <span
              className="text-caption font-medium text-warning"
              title="In the dispatcher's Escalations, at the top of the Fleet page"
            >
              escalated
            </span>
          )}
          <span className="ml-auto" />
          <Button onClick={() => onOpenTimeline(sessionId)} title="Open the event timeline">
            <span className="hidden sm:inline">Session timeline</span>
            <span className="sm:hidden">Timeline</span>
            <ArrowRight size={12} />
          </Button>
        </>
      }
    >
      <div className="min-h-0 flex-1 overflow-y-auto bg-app-shell">
        <AgentSummary agent={agent} />
        <div className="border-b border-surface-border bg-page-canvas px-4 py-2.5 sm:px-5">
          <InputBox
            sessionId={sessionId}
            to={agent.agent_id || sessionId}
            stopped={!agent.live}
            commands={commands.data}
          />
        </div>
        <div className="grid gap-3 p-3 sm:p-4 xl:grid-cols-[minmax(0,1.1fr)_minmax(0,1fr)]">
          <div className="flex min-w-0 flex-col gap-3">
            <JevPanel
              judgement={shown.judgement}
              at={shown.at}
              seq={shown.seq}
              pinned={pinned !== null}
              onBackToLatest={() => setPinnedSeq(null)}
            />
          </div>
          <div className="flex min-w-0 flex-col gap-3">
            <LivePane sessionId={sessionId} />
            <JudgementHistory
              records={judgements.data}
              error={judgements.error}
              selectedSeq={pinned ? pinnedSeq : null}
              onSelect={(seq) =>
                setPinnedSeq((current) => (current === seq ? null : seq))
              }
            />
            <CommandList records={commands.data} error={commands.error} />
          </div>
        </div>
      </div>
    </Drawer>
  );
}

function AgentSummary({ agent }: { agent: FleetAgent }) {
  const now = doingNow(agent);
  const activity = agent.activity;
  return (
    <div className="border-b border-surface-border bg-page-canvas px-4 py-3 sm:px-5">
      {agent.task ? (
        <details className="group">
          <summary className="cursor-pointer list-none text-label text-foreground">
            <span className="line-clamp-2 whitespace-pre-wrap group-open:line-clamp-none">
              {agent.task}
            </span>
          </summary>
        </details>
      ) : (
        <p className="text-label text-faint-foreground">No task recorded.</p>
      )}
      <div className="mt-3 grid grid-cols-2 gap-x-4 gap-y-2 sm:grid-cols-4 lg:grid-cols-8">
        <Fact label="Doing now" mono>
          <span className="inline-flex items-center gap-1">
            <DoingIcon now={now} size={11} />
            {now.label}
          </span>
        </Fact>
        <Fact label="Last event"><Ago ms={activity.last_event_at_ms} /></Fact>
        <Fact label="In state since"><Ago ms={agent.state_since_ms} /></Fact>
        <Fact label="Engine · execution" mono>
          {[agent.engine, agent.execution_id].filter(Boolean).join(" · ") || "—"}
        </Fact>
        <Fact label="Started">{clockTime(agent.started_at_ms)}</Fact>
        <Fact label="Ended">
          {agent.ended_at_ms ? clockTime(agent.ended_at_ms) : agent.live ? "running" : "—"}
        </Fact>
        <Fact label="Events · tools · execs">
          <span className="tnum">
            {activity.event_count ?? "—"} · {activity.tool_calls ?? "—"} ·{" "}
            {activity.executions ?? "—"}
          </span>
        </Fact>
        <Fact label="Session" mono>
          {agent.session_id}
        </Fact>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Jev panel

function JevPanel({
  judgement,
  at,
  seq,
  pinned,
  onBackToLatest,
}: {
  judgement: Judgement | null;
  at: number | null;
  seq: number | null;
  pinned: boolean;
  onBackToLatest: () => void;
}) {
  if (!judgement) {
    return (
      <Card>
        <CardHeader icon={<Sparkles size={14} />} title="Latest judgement" />
        <EmptyState title="Not judged yet" hint="The observer has not classified this agent." />
      </Card>
    );
  }

  const kind = judgeKind(judgement.judge);
  const allowed = allowedStates(judgement.ambiguity);
  const ambiguity = ambiguityText(judgement.ambiguity);
  const question = ambiguityQuestion(judgement.ambiguity);
  const reason = reasonText(judgement.reason);
  const unavailable = judgement.reason === "JUDGE_UNAVAILABLE";
  const evidence = judgement.evidence;

  return (
    <Card className={cx(pinned && "ring-1 ring-jev/40")}>
      <CardHeader
        icon={<Sparkles size={14} className={kind === "jev" ? "text-jev" : undefined} />}
        title={pinned ? `Judgement at seq ${seq ?? "—"}` : "Latest judgement"}
        meta={
          <>
            {pinned && (
              <button
                type="button"
                onClick={onBackToLatest}
                className="rounded px-1.5 py-0.5 text-caption font-medium text-jev hover:bg-jev/10"
              >
                Back to latest
              </button>
            )}
            <Ago ms={at} />
          </>
        }
      />
      <div className="space-y-4 p-4">
        {/* Verdict line */}
        <div className="flex flex-wrap items-center gap-2">
          <JudgeBadge judge={judgement.judge} />
          <span className="text-caption text-muted-foreground">decided</span>
          <FleetStateBadge state={judgement.state} />
          <span className="tnum text-caption text-muted-foreground">
            at {percent(judgement.confidence)} confidence
          </span>
          {judgement.based_on_seq != null && (
            <span className="tnum ml-auto font-mono text-micro text-faint-foreground">
              based on seq {judgement.based_on_seq}
            </span>
          )}
        </div>

        {judgement.attention && (
          <p
            className="text-caption text-muted-foreground"
            title="The judge's opinion only. What is escalated is the dispatcher's view: the Escalations at the top of the Fleet page."
          >
            <span className="font-medium text-foreground">The judge asks for attention</span>
            {judgement.attention_reason && <> · {reasonText(judgement.attention_reason)}</>}
            <span className="text-faint-foreground"> · its opinion, not the dispatcher's escalations</span>
          </p>
        )}

        {reason && (
          <div
            className={cx(
              "flex items-start gap-2 rounded-md px-3 py-2 text-caption",
              unavailable
                ? "bg-warning/10 text-warning ring-1 ring-warning/25 ring-inset"
                : "bg-muted text-foreground",
            )}
          >
            {unavailable && <CircleAlert size={13} className="mt-px shrink-0" />}
            <span>
              <span className="text-muted-foreground">Reason · </span>
              {reason}
              <span className="ml-1.5 font-mono text-micro text-faint-foreground">
                {judgement.reason}
              </span>
            </span>
          </div>
        )}

        {/* Why the rules handed it over */}
        {judgement.ambiguity ? (
          <div>
            <SectionLabel>Why Jev was asked</SectionLabel>
            <p className="mt-1.5 text-label text-foreground">
              {ambiguity}
              {question && (
                <span className="text-muted-foreground"> — {question}</span>
              )}
            </p>
            {allowed && (
              <div className="mt-2 flex flex-wrap items-center gap-1.5 text-caption">
                <span className="text-muted-foreground">Facts allow</span>
                {FLEET_STATES.map((state) => {
                  const ok = allowed.includes(state);
                  return (
                    <span
                      key={state}
                      title={ok ? "Allowed by the facts" : "Ruled out by the facts"}
                      className={cx(
                        "inline-flex h-5 items-center gap-1 rounded px-1.5 ring-1 ring-inset",
                        ok
                          ? "bg-surface-raised text-foreground ring-border-strong"
                          : "text-faint-foreground line-through ring-surface-border",
                      )}
                    >
                      {stateLabelUpper(state)}
                    </span>
                  );
                })}
              </div>
            )}
          </div>
        ) : (
          kind === "rules" && (
            <p className="text-caption text-muted-foreground">
              The rules settled this from the facts alone — no ambiguity, so Jev
              was not asked.
            </p>
          )
        )}

        {/* Distribution */}
        {judgement.probabilities ? (
          <div>
            <SectionLabel trailing={judgement.latency_ms != null && (
              <span className="tnum text-micro text-faint-foreground">
                answered in {formatDuration(judgement.latency_ms)}
              </span>
            )}>
              Jev's distribution
            </SectionLabel>
            <ul className="mt-2 space-y-1.5">
              {FLEET_STATES.map((state) => {
                const p = judgement.probabilities?.[state] ?? 0;
                const ruledOut = allowed != null && !allowed.includes(state);
                const isFinal = state === judgement.state;
                const isRules = state === judgement.rules_state;
                return (
                  <li
                    key={state}
                    className={cx(
                      "grid grid-cols-[6.5rem_minmax(0,1fr)_2.75rem_7.5rem] items-center gap-2 text-caption",
                      ruledOut && "opacity-55",
                    )}
                  >
                    <span className="flex items-center gap-1.5">
                      <FleetStateDot state={state} size="sm" />
                      <span className={isFinal ? "font-medium text-foreground" : "text-muted-foreground"}>
                        {stateLabelUpper(state)}
                      </span>
                    </span>
                    <span
                      className={cx(
                        "relative h-2 overflow-hidden rounded-sm",
                        ruledOut ? "bg-hatch" : "bg-muted",
                      )}
                    >
                      <span
                        className={cx(
                          "absolute inset-y-0 left-0 rounded-sm",
                          isFinal
                            ? kind === "jev"
                              ? "bg-jev"
                              : "bg-foreground/70"
                            : "bg-muted-foreground/45",
                        )}
                        style={{ width: `${Math.max(0, Math.min(1, p)) * 100}%` }}
                      />
                    </span>
                    <span className={cx("tnum text-right", isFinal ? "text-foreground" : "text-muted-foreground")}>
                      {percent(p)}
                    </span>
                    <span className="flex gap-1 text-micro">
                      {isFinal && (
                        <span className="rounded bg-jev/12 px-1 text-jev">final</span>
                      )}
                      {isRules && (
                        <span className="rounded bg-muted px-1 text-muted-foreground">rules</span>
                      )}
                      {ruledOut && (
                        <span className="text-faint-foreground">ruled out</span>
                      )}
                    </span>
                  </li>
                );
              })}
            </ul>
            {allowed && (
              <p className="mt-1.5 text-micro text-faint-foreground">
                Jev's answer is narrowed to the states the facts allow; the
                final state is the most likely allowed one.
              </p>
            )}
          </div>
        ) : kind === "jev" ? (
          <p className="text-caption text-faint-foreground">
            No distribution was recorded for this judgement.
          </p>
        ) : null}

        {/* Rules vs final, risk scores */}
        {(judgement.rules_state ||
          judgement.needs_human != null ||
          judgement.stuck != null) && (
          <div className="grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-[minmax(0,1.3fr)_minmax(0,1fr)_minmax(0,1fr)]">
            <div className="col-span-2 min-w-0 sm:col-span-1">
              <div className="text-caption text-muted-foreground">Rules' guess → final</div>
              <div className="mt-1 flex flex-wrap items-center gap-1.5">
                {judgement.rules_state ? (
                  <>
                    <FleetStateBadge state={judgement.rules_state} />
                    <ArrowRight size={12} className="text-faint-foreground" />
                    <FleetStateBadge state={judgement.state} />
                    <span
                      className={cx(
                        "text-caption",
                        judgement.rules_state === judgement.state
                          ? "text-muted-foreground"
                          : "font-medium text-warning",
                      )}
                    >
                      {judgement.rules_state === judgement.state
                        ? "agree"
                        : unavailable
                          ? "rules stood"
                          : "Jev overrode"}
                    </span>
                  </>
                ) : (
                  <span className="text-caption text-faint-foreground">—</span>
                )}
              </div>
            </div>
            <Meter label="Needs human" value={judgement.needs_human} />
            <Meter label="Stuck" value={judgement.stuck} />
          </div>
        )}

        {/* Evidence */}
        {evidence && (
          <div className="space-y-2">
            <SectionLabel
              trailing={
                judgement.fingerprint && (
                  <span className="font-mono text-micro text-faint-foreground" title="Evidence fingerprint">
                    {judgement.fingerprint}
                  </span>
                )
              }
            >
              What Jev saw
            </SectionLabel>
            {evidence.facts.length > 0 && (
              <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 rounded-md bg-surface-raised/60 px-3 py-2 sm:grid-cols-3">
                {evidence.facts.map(([key, value]) => (
                  <Fact key={key} label={key} mono>
                    <span title={value}>{key.endsWith("_ms") ? formatFactMs(value) : value}</span>
                  </Fact>
                ))}
              </div>
            )}
            {evidence.task && (
              <div className="rounded-md border border-surface-border px-3 py-2">
                <div className="text-micro text-faint-foreground">task</div>
                <p className="mt-0.5 line-clamp-4 whitespace-pre-wrap text-caption text-foreground">
                  {evidence.task}
                </p>
              </div>
            )}
            {evidence.screen_tail ? (
              <TerminalBlock text={evidence.screen_tail} maxHeight="22rem" tail />
            ) : (
              <p className="text-caption text-faint-foreground">No screen captured.</p>
            )}
          </div>
        )}
      </div>
    </Card>
  );
}

function formatFactMs(value: string): string {
  const n = Number(value);
  return Number.isFinite(n) ? formatDuration(n) : value;
}

// ---------------------------------------------------------------------------
// Live pane

function LivePane({ sessionId }: { sessionId: string }) {
  const pane = usePoll(
    `pane:${sessionId}`,
    (signal) => fetchAgentPane(sessionId, signal),
    3_000,
    { checked: true },
  );
  const data = pane.data;
  return (
    <TerminalPanel
      className="h-[26rem]"
      title="Live peek"
      tag={data?.terminal}
      text={data?.text ?? ""}
      placeholder={
        pane.error
          ? `Could not read the pane: ${pane.error}`
          : data
            ? data.live
              ? "The pane is empty."
              : "No terminal is running for this agent."
            : "Connecting…"
      }
      status={
        <>
          <span className="hidden text-micro text-faint-foreground sm:inline">
            tmux capture, not stored
          </span>
          {data?.captured_at_ms ? (
            <span className="tnum text-caption text-faint-foreground">
              {relativeTime(freshAt(data.captured_at_ms, pane.checkedAt))}
            </span>
          ) : null}
          <LiveBadge live={data?.live === true} known={data !== null} />
        </>
      }
    />
  );
}

// ---------------------------------------------------------------------------
// History and commands

function JudgementHistory({
  records,
  error,
  selectedSeq,
  onSelect,
}: {
  records: JudgementRecord[] | null;
  error: string | null;
  selectedSeq: number | null;
  onSelect: (seq: number) => void;
}) {
  return (
    <Card className="flex flex-col overflow-hidden">
      <CardHeader
        icon={<History size={14} />}
        title="Judgement history"
        meta={
          records && (
            <span className="text-faint-foreground">
              newest first · click to inspect
            </span>
          )
        }
      />
      {!records ? (
        <p className="px-4 py-3 text-caption text-faint-foreground">
          {error ? `Could not load judgements: ${error}` : "Loading…"}
        </p>
      ) : records.length === 0 ? (
        <EmptyState title="No judgements yet" />
      ) : (
        <ul className="max-h-[20rem] divide-y divide-surface-border overflow-y-auto">
          {records.map((record, index) => {
            const o = record.observation;
            const seq = record.session_seq;
            const disagree = o.rules_state != null && o.rules_state !== o.state;
            return (
              <li key={`${seq ?? "x"}-${index}`}>
                <button
                  type="button"
                  disabled={seq == null}
                  onClick={() => seq != null && onSelect(seq)}
                  className={cx(
                    "grid w-full grid-cols-[4.25rem_4.75rem_6.5rem_2.75rem_minmax(0,1fr)] items-center gap-2 px-4 py-1.5 text-left text-caption transition-colors",
                    seq != null && seq === selectedSeq
                      ? "bg-jev/10"
                      : "hover:bg-surface-hover",
                  )}
                >
                  <span
                    className="tnum text-muted-foreground"
                    title={record.recorded_at_ms ? new Date(record.recorded_at_ms).toLocaleString() : undefined}
                  >
                    {clockTime(record.recorded_at_ms)}
                  </span>
                  <span>
                    <JudgeBadge judge={o.judge} />
                  </span>
                  <span className="flex items-center gap-1.5">
                    <FleetStateDot state={o.state} size="sm" />
                    <span className="text-foreground">{stateLabelUpper(o.state)}</span>
                  </span>
                  <span className="tnum text-right text-muted-foreground">
                    {percent(o.confidence)}
                  </span>
                  <span className="min-w-0 truncate text-faint-foreground">
                    {[
                      disagree && o.rules_state
                        ? `rules: ${stateLabelUpper(o.rules_state).toLowerCase()}`
                        : null,
                      ambiguityText(o.ambiguity),
                      reasonText(o.reason),
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </Card>
  );
}

const PRINCIPAL_LABEL: Record<string, string> = {
  human: "Human",
  spawn_supervisor: "Supervisor",
  "spawn-dispatch": "Dispatcher",
  "spawn-liveness": "Liveness",
  spawn: "Spawn",
};

function PrincipalBadge({ principal }: { principal: string }) {
  const human = principal === "human";
  return (
    <span
      title={principal}
      className={cx(
        "inline-flex h-5 shrink-0 items-center rounded-md px-1.5 text-micro font-medium ring-1 ring-inset",
        human
          ? "bg-warning/12 text-warning ring-warning/30"
          : principal === "spawn_supervisor"
            ? "bg-foreground/[0.07] text-foreground ring-border-strong"
            : "bg-muted text-muted-foreground ring-surface-border",
      )}
    >
      {PRINCIPAL_LABEL[principal] ?? principal}
    </span>
  );
}

function CommandOutcome({ record }: { record: CommandRecord }) {
  if (record.succeeded === true) {
    return <span className="text-success">ok</span>;
  }
  if (record.succeeded === false) {
    return <span className="text-destructive">failed</span>;
  }
  if (record.waiting) {
    return (
      <span
        className="text-warning"
        title="The agent is in a turn: the Bridge types it when the turn ends"
      >
        waiting for its turn
      </span>
    );
  }
  return <span className="text-muted-foreground">{record.state ?? "pending"}</span>;
}

function CommandList({
  records,
  error,
}: {
  records: CommandRecord[] | null;
  error: string | null;
}) {
  return (
    <Card className="flex flex-col overflow-hidden">
      <CardHeader icon={<Send size={14} />} title="Commands into this agent" />
      {!records ? (
        <p className="px-4 py-3 text-caption text-faint-foreground">
          {error ? `Could not load commands: ${error}` : "Loading…"}
        </p>
      ) : records.length === 0 ? (
        <EmptyState title="No commands yet" />
      ) : (
        <ul className="max-h-[22rem] divide-y divide-surface-border overflow-y-auto">
          {records.map((record, index) => (
            <li key={record.command_id ?? `i${index}`} className="px-4 py-2">
              <div className="flex flex-wrap items-center gap-2 text-caption">
                <span
                  className="tnum text-muted-foreground"
                  title={record.created_at_ms ? new Date(record.created_at_ms).toLocaleString() : undefined}
                >
                  {clockTime(record.created_at_ms)}
                </span>
                <PrincipalBadge principal={record.principal} />
                <span className="font-mono text-foreground">{record.action}</span>
                <CommandOutcome record={record} />
                {record.created_at_ms != null && record.finished_at_ms != null && (
                  <span className="tnum text-faint-foreground">
                    {formatDuration(record.finished_at_ms - record.created_at_ms)}
                  </span>
                )}
                {record.command_id != null && (
                  <span className="tnum ml-auto font-mono text-micro text-faint-foreground">
                    #{record.command_id}
                  </span>
                )}
              </div>
              {record.error && (
                <p className="mt-1 text-caption text-destructive">{record.error}</p>
              )}
              {record.text && (
                <details className="group mt-1">
                  <summary className="cursor-pointer list-none text-caption text-muted-foreground">
                    <span className="line-clamp-1 group-open:hidden">
                      {record.text.replace(/\s+/g, " ")}
                    </span>
                  </summary>
                  <p className="whitespace-pre-wrap text-caption text-foreground">
                    {record.text}
                  </p>
                </details>
              )}
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}


import { BellRing, CircleCheck, ChevronRight, MessageCircleQuestion } from "lucide-react";

import type { FleetAgent } from "../../fleet";
import {
  ambiguityText,
  humanQuestion,
  percent,
  reasonText,
  rulesDisagree,
  stateLabelUpper,
} from "../../fleet";
import { formatDuration } from "../../format";
import { cx } from "../ui";
import { FleetStateBadge, JudgeBadge, RoleBadge } from "./bits";

/** When the agent started waiting for a person, as far as the API says. */
export function waitingSince(agent: FleetAgent): number | null {
  return agent.state_since_ms ?? agent.judged_at_ms;
}

function waited(agent: FleetAgent): string {
  const since = waitingSince(agent);
  return since ? formatDuration(Math.max(0, Date.now() - since)) : "—";
}

/** Most urgent first: highest needs_human, then whoever waited longest. */
function rank(left: FleetAgent, right: FleetAgent): number {
  const need = (agent: FleetAgent) => agent.judgement?.needs_human ?? -1;
  return (
    need(right) - need(left) ||
    (waitingSince(left) ?? Infinity) - (waitingSince(right) ?? Infinity)
  );
}

export function AttentionQueue({
  agents,
  onSelect,
}: {
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
}) {
  const queue = agents.filter((agent) => agent.attention).sort(rank);

  if (queue.length === 0) {
    return (
      <section
        aria-label="Attention queue"
        className="flex shrink-0 items-center gap-3 rounded-lg border border-success/20 bg-success/[0.06] px-4 py-3"
      >
        <CircleCheck size={18} className="shrink-0 text-success" />
        <div className="min-w-0">
          <div className="text-label font-medium text-foreground">
            No one needs a human
          </div>
          <div className="text-caption text-muted-foreground">
            Every agent is either working or settled without you.
          </div>
        </div>
      </section>
    );
  }

  return (
    <section
      aria-label="Attention queue"
      className="shrink-0 overflow-hidden rounded-lg border border-warning/30 bg-warning/[0.05]"
    >
      <header className="flex h-9 items-center gap-2 border-b border-warning/20 px-4">
        <BellRing size={14} className="shrink-0 text-warning" />
        <h2 className="text-label font-medium text-foreground">
          Needs a human
        </h2>
        <span className="tnum rounded bg-warning/15 px-1.5 py-px text-micro font-medium text-warning">
          {queue.length}
        </span>
      </header>
      <ul className="divide-y divide-warning/15">
        {queue.map((agent) => (
          <li key={agent.session_id}>
            <AttentionRow agent={agent} onSelect={onSelect} />
          </li>
        ))}
      </ul>
    </section>
  );
}

function AttentionRow({
  agent,
  onSelect,
}: {
  agent: FleetAgent;
  onSelect: (sessionId: string) => void;
}) {
  const judgement = agent.judgement;
  const ambiguity = ambiguityText(judgement?.ambiguity ?? null);
  const reason = reasonText(judgement?.reason ?? null);
  const question = humanQuestion(judgement?.evidence?.screen_tail);
  const disagree = rulesDisagree(judgement);
  const unavailable = judgement?.reason === "JUDGE_UNAVAILABLE";

  return (
    <button
      type="button"
      onClick={() => onSelect(agent.session_id)}
      className="group flex w-full items-start gap-3 px-4 py-2.5 text-left transition-colors hover:bg-warning/[0.06]"
    >
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
          <span className="truncate text-label font-semibold text-foreground">
            {agent.agent_id || agent.session_id}
          </span>
          <RoleBadge role={agent.role} />
          <FleetStateBadge state={agent.state} />
          <JudgeBadge judge={judgement?.judge} />
          <span className="tnum text-caption text-muted-foreground">
            waiting{" "}
            <span className="font-medium text-foreground">{waited(agent)}</span>
          </span>
        </div>
        <p className="mt-1 flex flex-wrap items-baseline gap-x-1.5 text-caption text-muted-foreground">
          <span>
            needs human{" "}
            <span className="tnum font-medium text-foreground">
              {percent(judgement?.needs_human)}
            </span>
          </span>
          <Dot />
          <span>
            stuck{" "}
            <span className="tnum font-medium text-foreground">
              {percent(judgement?.stuck)}
            </span>
          </span>
          {ambiguity && (
            <>
              <Dot />
              <span>{ambiguity}</span>
            </>
          )}
          {reason && (
            <>
              <Dot />
              <span className={cx(unavailable && "text-warning")}>{reason}</span>
            </>
          )}
          {disagree && (
            <>
              <Dot />
              <span>
                rules said {stateLabelUpper(disagree).toLowerCase()}
              </span>
            </>
          )}
        </p>
        {question && (
          <p className="mt-1 flex items-start gap-1.5 text-caption text-foreground">
            <MessageCircleQuestion
              size={13}
              className="mt-px shrink-0 text-warning"
            />
            <span className="line-clamp-2">{question}</span>
          </p>
        )}
      </div>
      <ChevronRight
        size={14}
        className="mt-1 shrink-0 text-faint-foreground transition-colors group-hover:text-foreground"
      />
    </button>
  );
}

function Dot() {
  return <span className="text-faint-foreground">·</span>;
}

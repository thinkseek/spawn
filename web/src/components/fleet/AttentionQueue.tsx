import {
  BellRing,
  ChevronRight,
  CircleCheck,
  MessageCircleQuestion,
  NotebookPen,
  TriangleAlert,
} from "lucide-react";

import type { PollResult } from "../../api";
import type {
  Attention,
  AttentionItem,
  OwnerNote,
  SupervisorQuestion,
  SupervisorView,
} from "../../attention";
import { attentionGroups, itemSince } from "../../attention";
import type { FleetAgent, FleetState } from "../../fleet";
import {
  ambiguityText,
  fleetState,
  humanQuestion,
  percent,
  reasonText,
  rulesDisagree,
  stateLabelUpper,
} from "../../fleet";
import { clockTime, formatDuration } from "../../format";
import { HeldSources } from "../missions/HeldSources";
import { cx } from "../ui";
import { FleetStateBadge, JudgeBadge, RoleBadge } from "./bits";

/**
 * "Needs a human", top of the Fleet page. It shows what the dispatcher's own
 * code holds (`/api/attention`): the supervisor's open question, then the
 * escalations not yet told to the supervisor, then those it was told that
 * still hold. Below it, the supervisor's lines "For the owner": read, not
 * counted, since it does not wait on them. An older server without that
 * endpoint gets the list of agents whose latest judgement asks for
 * attention, labelled as the judges' only.
 */
export function AttentionQueue({
  view,
  agents,
  onSelect,
}: {
  /** `/api/attention`, polled by App; a failed poll keeps the last view. */
  view: PollResult<Attention>;
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
}) {
  if (view.data) {
    return (
      <DispatcherQueue
        attention={view.data}
        agents={agents}
        onSelect={onSelect}
        stale={view.error}
      />
    );
  }
  if (view.error) {
    return (
      <JudgementQueue
        agents={agents}
        onSelect={onSelect}
        unavailable={view.error}
      />
    );
  }
  return (
    <section
      aria-label="Attention queue"
      className="flex h-9 shrink-0 items-center gap-2 rounded-lg border border-border px-4"
    >
      <BellRing size={14} className="shrink-0 text-faint-foreground" />
      <span className="text-caption text-muted-foreground">
        Reading the dispatcher's view…
      </span>
    </section>
  );
}

function age(since: number | null): string | null {
  return since === null ? null : formatDuration(Math.max(0, Date.now() - since));
}

/** The dispatcher's view: question, then untold, then told; then the
 * owner's holds with tasks waiting behind them, and the supervisor's lines
 * for the owner. */
function DispatcherQueue(props: {
  attention: Attention;
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
  stale: string | null;
}) {
  return (
    <>
      <Queue {...props} />
      <HeldSources
        holds={props.attention.holds}
        title="Held by the owner"
        hint="no task lands on these sources until the hold is released or expires; their gates still run"
      />
      <OwnerNotes notes={attentionGroups(props.attention).forOwner} />
    </>
  );
}

/** The supervisor's lines for the owner: what its grants do not cover. It
 * does not wait on them, so they ask nothing; they are there to be read. */
function OwnerNotes({ notes }: { notes: OwnerNote[] }) {
  if (notes.length === 0) return null;
  return (
    <section
      aria-label="For the owner"
      className="shrink-0 rounded-lg border border-border px-4 py-2"
    >
      <header className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <NotebookPen size={14} className="shrink-0 text-muted-foreground" />
        <h2 className="text-label font-medium text-foreground">For the owner</h2>
        <span className="tnum text-micro text-muted-foreground">
          {notes.length}
        </span>
        <span className="text-micro text-faint-foreground">
          the supervisor's lines; it does not wait on them
        </span>
      </header>
      <ul className="mt-1 divide-y divide-border">
        {notes.map((note) => (
          <li key={note.text} className="flex items-baseline gap-3 py-1.5">
            <p className="min-w-0 flex-1 break-words text-caption text-foreground">
              {note.text}
            </p>
            {note.at_ms !== null && (
              <span
                className="tnum shrink-0 text-caption text-muted-foreground"
                title={clockTime(note.at_ms)}
              >
                {age(note.at_ms)} ago
              </span>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

/** The queue itself: question, then untold, then told. */
function Queue({
  attention,
  agents,
  onSelect,
  stale,
}: {
  attention: Attention;
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
  /** Why the latest poll failed, while the last view stays on screen. */
  stale: string | null;
}) {
  const groups = attentionGroups(attention);
  const known = new Set(agents.map((agent) => agent.session_id));
  const select = (session: string) =>
    known.has(session) ? () => onSelect(session) : undefined;
  const supervisor = attention.supervisor;

  if (groups.count === 0) {
    return (
      <section
        aria-label="Attention queue"
        className="flex shrink-0 items-center gap-3 rounded-lg border border-success/20 bg-success/[0.06] px-4 py-3"
      >
        <CircleCheck size={18} className="shrink-0 text-success" />
        <div className="min-w-0 flex-1">
          <div className="text-label font-medium text-foreground">
            No one needs a human
          </div>
          <div className="text-caption text-muted-foreground">
            The dispatcher holds no escalation and the supervisor asks nothing.
          </div>
        </div>
        <Stale error={stale} />
        <SupervisorStatus supervisor={supervisor} />
      </section>
    );
  }

  return (
    <section
      aria-label="Attention queue"
      className="shrink-0 overflow-hidden rounded-lg border border-warning/30 bg-warning/[0.05]"
    >
      <header className="flex min-h-9 flex-wrap items-center gap-x-2 gap-y-1 border-b border-warning/20 px-4 py-1.5">
        <BellRing size={14} className="shrink-0 text-warning" />
        <h2 className="text-label font-medium text-foreground">
          Needs a human
        </h2>
        <span className="tnum rounded bg-warning/15 px-1.5 py-px text-micro font-medium text-warning">
          {groups.count}
        </span>
        <span
          className="text-micro text-faint-foreground"
          title="Computed by the dispatcher's own code: what it would escalate this moment"
        >
          from the dispatcher
        </span>
        <Stale error={stale} />
        <span className="ml-auto">
          <SupervisorStatus supervisor={supervisor} />
        </span>
      </header>

      {groups.question && supervisor && (
        <QuestionBlock
          question={groups.question}
          supervisor={supervisor}
          onOpen={select(supervisor.session_id)}
        />
      )}

      <ItemSection
        title="Not yet told to the supervisor"
        items={groups.untold}
        select={select}
      />
      <ItemSection
        title="Told to the supervisor, unresolved"
        items={groups.told}
        select={select}
        muted
      />

      {attention.warnings.length > 0 && (
        <p className="flex items-start gap-1.5 border-t border-warning/15 px-4 py-2 text-caption text-muted-foreground">
          <TriangleAlert size={13} className="mt-px shrink-0 text-warning" />
          <span>
            Not everything could be read: {attention.warnings.join("; ")}
          </span>
        </p>
      )}
    </section>
  );
}

/** A view the latest poll could not refresh. */
function Stale({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <span
      className="text-micro font-medium text-warning"
      title={`/api/attention: ${error}`}
    >
      not refreshed
    </span>
  );
}

/** The supervisor's liveness and judgement, in a word or two. */
function SupervisorStatus({ supervisor }: { supervisor: SupervisorView | null }) {
  if (!supervisor) {
    return (
      <span className="text-caption text-faint-foreground">
        no supervisor: escalations wait here
      </span>
    );
  }
  if (!supervisor.live) {
    return (
      <span className="text-caption font-medium text-warning">
        supervisor not running
        {supervisor.end_cause ? ` (${supervisor.end_cause})` : ""}
      </span>
    );
  }
  const judgement = supervisor.judgement;
  if (!judgement) {
    return (
      <span className="text-caption text-muted-foreground">
        supervisor running
      </span>
    );
  }
  const state: FleetState = fleetState(judgement.state);
  return (
    <span className="inline-flex items-center gap-1.5 text-caption text-muted-foreground">
      supervisor
      <FleetStateBadge state={state} size="sm" />
      {judgement.needs_human !== null && (
        <span>
          needs human{" "}
          <span className="tnum font-medium text-foreground">
            {percent(judgement.needs_human)}
          </span>
        </span>
      )}
    </span>
  );
}

/** The supervisor's open question, in full. */
function QuestionBlock({
  question,
  supervisor,
  onOpen,
}: {
  question: SupervisorQuestion;
  supervisor: SupervisorView;
  onOpen?: () => void;
}) {
  const asked = question.asked_at_ms;
  const human = question.human;
  const humanEarlier =
    human?.at_ms != null && asked !== null && asked - human.at_ms > 60_000;
  return (
    <div className="border-b border-warning/20 px-4 py-3">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <MessageCircleQuestion size={15} className="shrink-0 text-warning" />
        <h3 className="text-label font-semibold text-foreground">
          The supervisor is waiting for you
        </h3>
        {asked !== null && (
          <span className="tnum text-caption text-muted-foreground">
            asked{" "}
            <span className="font-medium text-foreground">{age(asked)}</span>{" "}
            ago, at {clockTime(asked)}
          </span>
        )}
        {supervisor.judgement && (
          <span className="text-caption text-muted-foreground">
            · {stateLabelUpper(fleetState(supervisor.judgement.state))}, needs
            human {percent(supervisor.judgement.needs_human)}
          </span>
        )}
      </div>

      {human && (
        <p className="mt-2 rounded-md border border-warning/30 bg-warning/[0.08] px-3 py-2 text-body font-medium text-foreground">
          {human.text}
          {humanEarlier && human.at_ms !== null && (
            <span className="ml-2 text-caption font-normal text-muted-foreground">
              asked {age(human.at_ms)} ago, at {clockTime(human.at_ms)}, and
              not answered since
            </span>
          )}
        </p>
      )}

      {question.text ? (
        <div className="mt-2 max-h-80 overflow-y-auto whitespace-pre-wrap break-words rounded-md border border-border bg-background/60 px-3 py-2 font-mono text-caption text-foreground">
          {question.text}
        </div>
      ) : (
        <p className="mt-2 text-caption text-muted-foreground">
          Its words could not be read here; its pane shows them.
        </p>
      )}

      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-caption text-muted-foreground">
        <span>
          Answer it in the supervisor's own pane (
          <code className="font-mono text-micro">session_supervisor</code>
          ); this page only reads.
        </span>
        {question.source === "screen" && (
          <span className="text-faint-foreground">
            Shown from its last judged screen: the transcript could not be read.
          </span>
        )}
        {onOpen && (
          <button
            type="button"
            onClick={onOpen}
            className="inline-flex items-center gap-0.5 font-medium text-foreground underline-offset-2 hover:underline"
          >
            Open its pane
            <ChevronRight size={12} />
          </button>
        )}
      </div>
    </div>
  );
}

function ItemSection({
  title,
  items,
  select,
  muted = false,
}: {
  title: string;
  items: AttentionItem[];
  select: (session: string) => (() => void) | undefined;
  muted?: boolean;
}) {
  if (items.length === 0) return null;
  return (
    <div className="border-b border-warning/15 last:border-b-0">
      <div className="flex items-center gap-2 px-4 pb-1 pt-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground">
        {title}
        <span className="tnum normal-case tracking-normal">{items.length}</span>
      </div>
      <ul className="divide-y divide-warning/10">
        {items.map((item) => (
          <li key={item.tag || `${item.session_id} ${item.line}`}>
            <ItemRow item={item} onSelect={select(item.session_id)} muted={muted} />
          </li>
        ))}
      </ul>
    </div>
  );
}

function ItemRow({
  item,
  onSelect,
  muted,
}: {
  item: AttentionItem;
  onSelect?: () => void;
  muted: boolean;
}) {
  const since = age(itemSince(item));
  const when = item.told
    ? since && `told ${since} ago`
    : since && `for ${since}`;
  const body = (
    <>
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
          <span
            className={cx(
              "text-label font-semibold",
              muted ? "text-muted-foreground" : "text-foreground",
            )}
          >
            {item.why}
          </span>
          <span className="truncate text-caption text-muted-foreground">
            {item.agent_id ?? item.session_id}
          </span>
          {when && (
            <span
              className="tnum ml-auto shrink-0 text-caption text-muted-foreground"
              title={
                item.told && item.told_at_ms === null
                  ? "When its digest was delivered is not known; this is the age of its record"
                  : undefined
              }
            >
              {when}
            </span>
          )}
        </div>
        <p
          className="mt-0.5 line-clamp-3 break-words text-caption text-muted-foreground"
          title={item.line}
        >
          {item.line}
        </p>
      </div>
      {onSelect && (
        <ChevronRight
          size={14}
          className="mt-1 shrink-0 text-faint-foreground transition-colors group-hover:text-foreground"
        />
      )}
    </>
  );
  return onSelect ? (
    <button
      type="button"
      onClick={onSelect}
      className="group flex w-full items-start gap-3 px-4 py-2 text-left transition-colors hover:bg-warning/[0.06]"
    >
      {body}
    </button>
  ) : (
    <div className="flex w-full items-start gap-3 px-4 py-2">{body}</div>
  );
}

// ---------------------------------------------------------------------------
// Fallback: an older server without `/api/attention`. The list of agents
// whose latest judgement asks for attention (the judge's flag), as before,
// labelled as judgements only.

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

function Unavailable({ error }: { error: string }) {
  return (
    <span
      className="text-micro text-faint-foreground"
      title={`/api/attention: ${error}`}
    >
      dispatcher view unavailable: judgements only
    </span>
  );
}

function JudgementQueue({
  agents,
  onSelect,
  unavailable,
}: {
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
  unavailable: string;
}) {
  const queue = agents.filter((agent) => agent.judgement?.attention === true).sort(rank);

  if (queue.length === 0) {
    return (
      <section
        aria-label="Attention queue"
        className="flex shrink-0 items-center gap-3 rounded-lg border border-success/20 bg-success/[0.06] px-4 py-3"
      >
        <CircleCheck size={18} className="shrink-0 text-success" />
        <div className="min-w-0 flex-1">
          <div className="text-label font-medium text-foreground">
            No judge asks for attention
          </div>
          <div className="text-caption text-muted-foreground">
            Who needs a human is the dispatcher's to say, and its view could not be read.
          </div>
        </div>
        <Unavailable error={unavailable} />
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
          Judges ask for attention
        </h2>
        <span className="tnum rounded bg-warning/15 px-1.5 py-px text-micro font-medium text-warning">
          {queue.length}
        </span>
        <span className="ml-auto">
          <Unavailable error={unavailable} />
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

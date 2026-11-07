import {
  BellRing,
  ChevronDown,
  ChevronRight,
  CircleCheck,
  Hourglass,
  MessageCircle,
  MessageCircleQuestion,
  NotebookPen,
  TriangleAlert,
} from "lucide-react";
import type { ReactNode } from "react";
import { useState } from "react";

import type { PollResult } from "../../api";
import type {
  Attention,
  AttentionItem,
  OnOwnJob,
  OwnerNote,
  SupervisorQuestion,
  SupervisorView,
} from "../../attention";
import {
  attentionGroups,
  itemSince,
  questionSummary,
  toldFolded,
} from "../../attention";
import type { FleetAgent, FleetState } from "../../fleet";
import {
  ambiguityText,
  fleetState,
  humanQuestion,
  jobsText,
  percent,
  reasonText,
  rulesDisagree,
  stateLabelUpper,
} from "../../fleet";
import { clockTime, formatDuration } from "../../format";
import { HeldSources } from "../missions/HeldSources";
import { LabeledId, cx } from "../ui";
import { FleetStateBadge, JudgeBadge, RoleBadge } from "./bits";

/**
 * "Escalations", top of the Fleet page: what the dispatcher raised, told to
 * the supervisor or not, as its own code computes it (`/api/attention`),
 * and the box to type to the supervisor (`talk`), always there. What the
 * supervisor asks, when it asks, comes first, in at most three lines, with
 * the box right under it; then the escalations not yet told to the
 * supervisor; then those it was told that still hold, folded to a count
 * while nothing is pending. Below it, the supervisor's lines "For the
 * owner": read, not counted, since it does not wait on them. Nothing here
 * waits on a person: the supervisor decides and reports, and the owner may
 * type to it whenever they choose. An older server without that endpoint
 * gets the list of agents whose latest judgement asks for attention,
 * labelled as the judges' only.
 */
export function AttentionQueue({
  view,
  agents,
  onSelect,
  talk,
}: {
  /** `/api/attention`, polled by App; a failed poll keeps the last view. */
  view: PollResult<Attention>;
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
  /** The input box to the supervisor, shown in every state of the queue. */
  talk: ReactNode;
}) {
  if (view.data) {
    return (
      <DispatcherQueue
        attention={view.data}
        agents={agents}
        onSelect={onSelect}
        stale={view.error}
        talk={talk}
      />
    );
  }
  if (view.error) {
    return (
      <JudgementQueue
        agents={agents}
        onSelect={onSelect}
        unavailable={view.error}
        talk={talk}
      />
    );
  }
  return (
    <section
      aria-label="Escalations"
      className="shrink-0 overflow-hidden rounded-lg border border-surface-border bg-surface"
    >
      <header className="flex h-9 items-center gap-2 border-b border-surface-border px-4">
        <BellRing size={14} className="shrink-0 text-faint-foreground" />
        <h2 className="text-label font-medium text-foreground">Escalations</h2>
        <span className="text-caption text-muted-foreground">
          Reading the dispatcher's view…
        </span>
      </header>
      <div className="px-4 py-2.5">{talk}</div>
    </section>
  );
}

function age(since: number | null): string | null {
  return since === null ? null : formatDuration(Math.max(0, Date.now() - since));
}

/** The dispatcher's view: what the supervisor asks and the box to it, then
 * untold, then told; then the owner's holds with tasks waiting behind them,
 * and the supervisor's lines for the owner. */
function DispatcherQueue(props: {
  attention: Attention;
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
  stale: string | null;
  talk: ReactNode;
}) {
  const groups = attentionGroups(props.attention);
  return (
    <>
      <Queue {...props} />
      <OnOwnJobs workers={groups.onOwnJobs} agents={props.agents} onSelect={props.onSelect} />
      <HeldSources
        holds={props.attention.holds}
        title="Held by the owner"
        hint="no task lands on these sources until the hold is released or expires; their gates still run"
      />
      <OwnerNotes notes={groups.forOwner} />
    </>
  );
}

/** Workers the rules hold as working on a background job of their own: in
 * no queue (no Jev call, no idle item, no nudge, no stall), each with its
 * job and the job's age. A job past its bound is an item in the queue. */
function OnOwnJobs({
  workers,
  agents,
  onSelect,
}: {
  workers: OnOwnJob[];
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
}) {
  if (workers.length === 0) return null;
  const known = new Set(agents.map((agent) => agent.session_id));
  return (
    <section
      aria-label="Working on their own background jobs"
      className="shrink-0 rounded-lg border border-border px-4 py-2"
    >
      <header className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <Hourglass size={14} className="shrink-0 text-success" />
        <h2 className="text-label font-medium text-foreground">On their own background jobs</h2>
        <span className="tnum text-micro text-muted-foreground">{workers.length}</span>
        <span className="text-micro text-faint-foreground">
          working, not idle: the engine wakes each when its job reports
        </span>
      </header>
      <ul className="mt-1 divide-y divide-border">
        {workers.map((worker) => (
          <li key={worker.session_id}>
            <button
              type="button"
              disabled={!known.has(worker.session_id)}
              onClick={() => onSelect(worker.session_id)}
              className="flex w-full items-baseline gap-3 py-1.5 text-left enabled:hover:bg-surface-hover"
            >
              <span className="min-w-0 max-w-[60%] shrink-0">
                <LabeledId
                  label={worker.label}
                  id={worker.agent_id || worker.session_id}
                  className="text-caption font-medium text-foreground"
                  clamp={1}
                />
              </span>
              <span className="min-w-0 flex-1 truncate font-mono text-caption text-muted-foreground">
                {jobsText(worker.jobs) ?? "—"}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </section>
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

/** The queue itself: what the supervisor asks and the box to it, then
 * untold, then told. */
function Queue({
  attention,
  agents,
  onSelect,
  stale,
  talk,
}: {
  attention: Attention;
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
  /** Why the latest poll failed, while the last view stays on screen. */
  stale: string | null;
  talk: ReactNode;
}) {
  const groups = attentionGroups(attention);
  // Null follows the default (toldFolded); a click decides from then on.
  const [toldOpen, setToldOpen] = useState<boolean | null>(null);
  const folded = toldOpen === null ? toldFolded(groups) : !toldOpen;
  const known = new Set(agents.map((agent) => agent.session_id));
  const select = (session: string) =>
    known.has(session) ? () => onSelect(session) : undefined;
  const supervisor = attention.supervisor;
  const none = groups.count === 0;

  return (
    <section
      aria-label="Escalations"
      className="shrink-0 overflow-hidden rounded-lg border border-surface-border bg-surface"
    >
      <header className="flex min-h-9 flex-wrap items-center gap-x-2 gap-y-1 border-b border-surface-border px-4 py-1.5">
        {none ? (
          <CircleCheck size={14} className="shrink-0 text-success" />
        ) : (
          <BellRing size={14} className="shrink-0 text-warning" />
        )}
        <h2 className="text-label font-medium text-foreground">Escalations</h2>
        {none ? (
          <span className="text-caption text-muted-foreground">
            none: the dispatcher holds nothing and the supervisor asks nothing
          </span>
        ) : (
          <span
            className="tnum rounded bg-warning/15 px-1.5 py-px text-micro font-medium text-warning"
            title="What the dispatcher raised, told to the supervisor or not, and what the supervisor asks"
          >
            {groups.count}
          </span>
        )}
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

      <div className="border-b border-surface-border px-4 py-2.5 last:border-b-0">
        {groups.question && supervisor && (
          <Asks question={groups.question} onOpen={select(supervisor.session_id)} />
        )}
        {talk}
      </div>

      <ItemSection
        title="Not yet told to the supervisor"
        items={groups.untold}
        select={select}
      />
      {groups.told.length > 0 &&
        (folded ? (
          <button
            type="button"
            aria-expanded={false}
            onClick={() => setToldOpen(true)}
            className="flex w-full items-center gap-1.5 border-b border-surface-border px-4 py-2 text-left text-caption text-muted-foreground transition-colors last:border-b-0 hover:bg-surface-hover"
          >
            <ChevronRight size={13} className="shrink-0" />
            <span>
              <span className="tnum font-medium text-foreground">
                {groups.told.length}
              </span>{" "}
              told to the supervisor, unresolved
            </span>
            <span className="text-faint-foreground">
              · each in a digest it received
            </span>
          </button>
        ) : (
          <ItemSection
            title="Told to the supervisor, unresolved"
            items={groups.told}
            select={select}
            muted
            onFold={() => setToldOpen(false)}
          />
        ))}

      {attention.warnings.length > 0 && (
        <p className="flex items-start gap-1.5 border-t border-surface-border px-4 py-2 text-caption text-muted-foreground">
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
    <span
      className="inline-flex items-center gap-1.5 text-caption text-muted-foreground"
      title={
        judgement.needs_human !== null
          ? `Its judge's needs_human: ${percent(judgement.needs_human)}`
          : undefined
      }
    >
      supervisor
      <FleetStateBadge state={state} size="sm" />
    </span>
  );
}

/** What the supervisor asks, in at most three lines (its `HUMAN:` line, or
 * its reply's last paragraph), and its whole reply on request. */
function Asks({
  question,
  onOpen,
}: {
  question: SupervisorQuestion;
  onOpen?: () => void;
}) {
  const [whole, setWhole] = useState(false);
  const summary = questionSummary(question);
  const asked = question.human?.at_ms ?? question.asked_at_ms;
  return (
    <div className="mb-2.5">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-caption text-muted-foreground">
        <MessageCircle size={13} className="shrink-0" />
        <h3 className="text-label font-medium text-foreground">
          The supervisor asks
        </h3>
        {asked !== null && (
          <span className="tnum">
            {age(asked)} ago, at {clockTime(asked)}
          </span>
        )}
        {question.source === "screen" && (
          <span
            className="text-faint-foreground"
            title="The transcript could not be read"
          >
            from its last judged screen
          </span>
        )}
        {onOpen && (
          <button
            type="button"
            onClick={onOpen}
            className="ml-auto inline-flex items-center gap-0.5 font-medium text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
          >
            Open its pane
            <ChevronRight size={12} />
          </button>
        )}
      </div>
      {summary.text ? (
        <p
          className="mt-1 line-clamp-3 whitespace-pre-wrap break-words text-label text-foreground"
          title={summary.text}
        >
          {summary.text}
        </p>
      ) : (
        <p className="mt-1 text-caption text-muted-foreground">
          Its words could not be read here; its pane shows them.
        </p>
      )}
      {summary.more && (
        <button
          type="button"
          aria-expanded={whole}
          onClick={() => setWhole((open) => !open)}
          className="mt-0.5 inline-flex items-center gap-0.5 text-caption font-medium text-muted-foreground hover:text-foreground"
        >
          {whole ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
          {whole ? "hide the whole reply" : "show the whole reply"}
        </button>
      )}
      {whole && question.text && (
        <div className="mt-1.5 max-h-80 overflow-y-auto whitespace-pre-wrap break-words rounded-md border border-surface-border bg-background/60 px-3 py-2 font-mono text-caption text-foreground">
          {question.text}
        </div>
      )}
    </div>
  );
}

function ItemSection({
  title,
  items,
  select,
  muted = false,
  onFold,
}: {
  title: string;
  items: AttentionItem[];
  select: (session: string) => (() => void) | undefined;
  muted?: boolean;
  /** Fold the section to its count again. */
  onFold?: () => void;
}) {
  if (items.length === 0) return null;
  const heading = (
    <>
      {onFold && <ChevronDown size={12} className="shrink-0" />}
      {title}
      <span className="tnum normal-case tracking-normal">{items.length}</span>
    </>
  );
  const headingClass =
    "flex items-center gap-2 px-4 pb-1 pt-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground";
  return (
    <div className="border-b border-surface-border last:border-b-0">
      {onFold ? (
        <button
          type="button"
          aria-expanded
          onClick={onFold}
          className={cx(headingClass, "w-full text-left hover:text-foreground")}
        >
          {heading}
        </button>
      ) : (
        <div className={headingClass}>{heading}</div>
      )}
      <ul className="divide-y divide-surface-border">
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
          <span className="min-w-0 text-caption text-muted-foreground">
            <LabeledId label={item.label} id={item.agent_id ?? item.session_id} clamp={1} />
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
      className="group flex w-full items-start gap-3 px-4 py-2 text-left transition-colors hover:bg-surface-hover"
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
  talk,
}: {
  agents: FleetAgent[];
  onSelect: (sessionId: string) => void;
  unavailable: string;
  talk: ReactNode;
}) {
  const queue = agents.filter((agent) => agent.judgement?.attention === true).sort(rank);

  return (
    <section
      aria-label="Escalations"
      className="shrink-0 overflow-hidden rounded-lg border border-surface-border bg-surface"
    >
      <header className="flex h-9 items-center gap-2 border-b border-surface-border px-4">
        <BellRing size={14} className="shrink-0 text-faint-foreground" />
        <h2 className="text-label font-medium text-foreground">
          {queue.length > 0 ? "Judges ask for attention" : "No judge asks for attention"}
        </h2>
        {queue.length > 0 && (
          <span className="tnum rounded bg-warning/15 px-1.5 py-px text-micro font-medium text-warning">
            {queue.length}
          </span>
        )}
        <span className="ml-auto">
          <Unavailable error={unavailable} />
        </span>
      </header>
      <div className="border-b border-surface-border px-4 py-2.5 last:border-b-0">
        {talk}
      </div>
      {queue.length > 0 && (
        <ul className="divide-y divide-surface-border">
          {queue.map((agent) => (
            <li key={agent.session_id}>
              <AttentionRow agent={agent} onSelect={onSelect} />
            </li>
          ))}
        </ul>
      )}
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
      className="group flex w-full items-start gap-3 px-4 py-2.5 text-left transition-colors hover:bg-surface-hover"
    >
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
          <span className="min-w-0 text-label font-semibold text-foreground">
            <LabeledId label={agent.label} id={agent.agent_id || agent.session_id} clamp={1} />
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

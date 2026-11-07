// Read model of `/api/attention`: the Escalations queue (what the
// dispatcher raised, told to the supervisor or not, and what the supervisor
// asks) as the dispatcher computes it (crates/spawn-cli/src/serve/attention.rs).
// The server decides what is an escalation, what the supervisor was told and
// whether the supervisor asks something; this module only normalizes the
// answer and groups it for display. No dispatcher rule is repeated here.
// It is also the UI's one definition of what is escalated (`needsHuman`):
// the queue's count, the sidebar's, and every "escalated" mark.
//
// Pure: no runtime imports, so `node --test` loads it as it is.

/** One escalation of the dispatcher's plan. */
export interface AttentionItem {
  session_id: string;
  /** The agent of the session, or null for `host` and `missions` items. */
  agent_id: string | null;
  /** The digest's heading: what the supervisor is asked to do about it. */
  why: string;
  line: string;
  tag: string;
  /** When the record it rests on was written, if the server knows. */
  since_ms: number | null;
  /** Whether a digest that carried it reached the supervisor. */
  told: boolean;
  /** When that digest first reached the supervisor, if known. */
  told_at_ms: number | null;
}

export interface SupervisorJudgement {
  state: string;
  needs_human: number | null;
  stuck: number | null;
  ambiguity: string | null;
  confidence: number | null;
  judge: string;
  attention: boolean;
  attention_reason: string | null;
  judged_at_ms: number | null;
  current: boolean;
}

/** What the supervisor asks the human, when it waits at its prompt. */
export interface SupervisorQuestion {
  /** Its last reply, in full (or the screen when the transcript is unreadable). */
  text: string | null;
  source: "transcript" | "screen" | null;
  asked_at_ms: number | null;
  /** Its last `HUMAN:` line since a person last answered, if any. */
  human: { text: string; at_ms: number | null } | null;
}

/** A line the supervisor wrote for the owner ("For the owner: ..."): what
 * its grants do not cover. It does not wait on it, so it asks nothing and
 * nothing answers it; it is there to be read. */
export interface OwnerNote {
  text: string;
  /** When it wrote the line, if the transcript says. */
  at_ms: number | null;
}

export interface SupervisorView {
  session_id: string;
  agent_id: string;
  live: boolean;
  execution_id: string | null;
  end_cause: string | null;
  busy: boolean;
  turn_ended: boolean;
  judgement: SupervisorJudgement | null;
  question: SupervisorQuestion | null;
  /** Its lines for the owner, newest first. */
  for_owner: OwnerNote[];
}

/** One of a worker's open background jobs. */
export interface OwnJob {
  id: string;
  /** `bash`, `monitor` or `agent`. */
  kind: string;
  started_at_ms: number | null;
  /** Its age when the view was read, on the database's clock. */
  age_ms: number | null;
}

/** A worker the rules hold as working on a background job of its own (its
 * turn ended while a job it started runs): in no queue, listed so a person
 * sees why. */
export interface OnOwnJob {
  session_id: string;
  agent_id: string;
  judged_at_ms: number | null;
  /** Oldest first. */
  jobs: OwnJob[];
}

export interface Attention {
  generated_at_ms: number;
  pending: AttentionItem[];
  told: AttentionItem[];
  supervisor: SupervisorView | null;
  /** Workers on their own background jobs, not escalated. */
  on_own_jobs: OnOwnJob[];
  warnings: string[];
}

/** What the queue shows, top to bottom. */
export interface AttentionGroups {
  question: SupervisorQuestion | null;
  /** Not yet told to the supervisor. */
  untold: AttentionItem[];
  /** Told to the supervisor, still unresolved. */
  told: AttentionItem[];
  /** Everything a person may need to look at. */
  count: number;
  /** The supervisor's lines for the owner, newest first: to read, not
   * counted, since the supervisor does not wait on them. */
  forOwner: OwnerNote[];
  /** Workers on their own background jobs: working, not counted. */
  onOwnJobs: OnOwnJob[];
}

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : null;
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function text(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function optionalText(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function normalizeItem(value: unknown, told: boolean): AttentionItem | null {
  const raw = object(value);
  if (!raw) return null;
  const session = text(raw.session_id);
  const tag = text(raw.tag);
  if (!session && !tag) return null;
  return {
    session_id: session,
    agent_id: optionalText(raw.agent_id),
    why: text(raw.why, "escalation"),
    line: text(raw.line),
    tag,
    since_ms: optionalNumber(raw.since_ms),
    // The list it came in says whether it was told; the flag only confirms.
    told,
    told_at_ms: told ? optionalNumber(raw.told_at_ms) : null,
  };
}

function normalizeJudgement(value: unknown): SupervisorJudgement | null {
  const raw = object(value);
  if (!raw) return null;
  return {
    state: text(raw.state, "UNKNOWN").toUpperCase(),
    needs_human: optionalNumber(raw.needs_human),
    stuck: optionalNumber(raw.stuck),
    ambiguity: optionalText(raw.ambiguity),
    confidence: optionalNumber(raw.confidence),
    judge: text(raw.judge, "unknown"),
    attention: raw.attention === true,
    attention_reason: optionalText(raw.attention_reason),
    judged_at_ms: optionalNumber(raw.judged_at_ms),
    current: raw.current === true,
  };
}

function normalizeQuestion(value: unknown): SupervisorQuestion | null {
  const raw = object(value);
  if (!raw) return null;
  const human = object(raw.human);
  const humanText = human ? optionalText(human.text) : null;
  const source =
    raw.source === "transcript" || raw.source === "screen" ? raw.source : null;
  return {
    text: optionalText(raw.text),
    source,
    asked_at_ms: optionalNumber(raw.asked_at_ms),
    human:
      human && humanText
        ? { text: humanText, at_ms: optionalNumber(human.at_ms) }
        : null,
  };
}

function normalizeNotes(value: unknown): OwnerNote[] {
  return list(value).flatMap((entry) => {
    const raw = object(entry);
    const note = raw ? optionalText(raw.text) : null;
    return raw && note ? [{ text: note, at_ms: optionalNumber(raw.at_ms) }] : [];
  });
}

function normalizeOnOwnJobs(value: unknown): OnOwnJob[] {
  return list(value).flatMap((entry) => {
    const raw = object(entry);
    const session = raw ? optionalText(raw.session_id) : null;
    if (!raw || !session) return [];
    const jobs = list(raw.jobs).flatMap((job) => {
      const fields = object(job);
      const id = fields ? optionalText(fields.id) : null;
      return fields && id
        ? [{
            id,
            kind: text(fields.kind, "job"),
            started_at_ms: optionalNumber(fields.started_at_ms),
            age_ms: optionalNumber(fields.age_ms),
          }]
        : [];
    });
    return [{
      session_id: session,
      agent_id: text(raw.agent_id),
      judged_at_ms: optionalNumber(raw.judged_at_ms),
      jobs,
    }];
  });
}

function normalizeSupervisor(value: unknown): SupervisorView | null {
  const raw = object(value);
  if (!raw) return null;
  return {
    session_id: text(raw.session_id, "session_supervisor"),
    agent_id: text(raw.agent_id, "agent_supervisor"),
    live: raw.live === true,
    execution_id: optionalText(raw.execution_id),
    end_cause: optionalText(raw.end_cause),
    busy: raw.busy === true,
    turn_ended: raw.turn_ended === true,
    judgement: normalizeJudgement(raw.judgement),
    question: normalizeQuestion(raw.question),
    for_owner: normalizeNotes(raw.for_owner),
  };
}

/** `/api/attention`, every field normalized: a server that omits one gives
 * an empty value, never a crash. Throws when the answer is not the
 * dispatcher's view at all (no `pending` list), so the caller falls back. */
export function normalizeAttention(value: unknown): Attention {
  const raw = object(value);
  if (!raw || !Array.isArray(raw.pending)) {
    throw new Error("not a dispatcher view: no pending list");
  }
  const items = (entries: unknown, told: boolean) =>
    list(entries).flatMap((entry) => {
      const item = normalizeItem(entry, told);
      return item ? [item] : [];
    });
  return {
    generated_at_ms: optionalNumber(raw.generated_at_ms) ?? Date.now(),
    pending: items(raw.pending, false),
    told: items(raw.told, true),
    supervisor: normalizeSupervisor(raw.supervisor),
    on_own_jobs: normalizeOnOwnJobs(raw.on_own_jobs),
    warnings: list(raw.warnings).filter(
      (warning): warning is string => typeof warning === "string",
    ),
  };
}

/** When an item started to matter: when its digest reached the supervisor
 * for a told one (when it was given to someone), else when its record was
 * written. */
export function itemSince(item: AttentionItem): number | null {
  return item.told ? (item.told_at_ms ?? item.since_ms) : item.since_ms;
}

/** Oldest first; an item of unknown age after every dated one, in the
 * dispatcher's order. */
function byAge(left: AttentionItem, right: AttentionItem): number {
  const a = itemSince(left);
  const b = itemSince(right);
  if (a === null || b === null) return (a === null ? 1 : 0) - (b === null ? 1 : 0);
  return a - b;
}

/** The queue's sections: what the supervisor asks, then what the
 * supervisor was not told yet, then what it was told and is unresolved;
 * beside them, its lines for the owner. */
export function attentionGroups(attention: Attention): AttentionGroups {
  const untold = [...attention.pending].sort(byAge);
  const told = [...attention.told].sort(byAge);
  return {
    question: attention.supervisor?.question ?? null,
    untold,
    told,
    count: needsHuman(attention).count,
    forOwner: attention.supervisor?.for_owner ?? [],
    onOwnJobs: attention.on_own_jobs,
  };
}

/** What is escalated (named `needsHuman` since task 107): the UI's one
 * definition, read from the dispatcher's view and nothing else. A person
 * may look at it; nothing in the fleet waits for them to. */
export interface NeedsHuman {
  /** What a person may need to look at: every pending item, every item
   * told and unresolved, and the supervisor's open question. The queue's
   * count and the sidebar's. */
  count: number;
  /** The sessions and agents marked escalated: those with a pending or
   * told-and-unresolved item, and the supervisor while it asks something. */
  ids: ReadonlySet<string>;
}

/** What is escalated, by `/api/attention`. A judge's `attention` flag is not
 * part of it: that is the judge's opinion, shown with its judgement (the
 * dispatcher decides what to escalate, and does not escalate, for one, a
 * managed worker between tasks). Items about the host or the missions count
 * without marking an agent. */
export function needsHuman(attention: Attention): NeedsHuman {
  const ids = new Set<string>();
  for (const item of [...attention.pending, ...attention.told]) {
    // `host` and `missions` name no agent's session.
    if (item.session_id) ids.add(item.session_id);
    if (item.agent_id) ids.add(item.agent_id);
  }
  const supervisor = attention.supervisor;
  const question = supervisor?.question ?? null;
  if (supervisor && question) {
    ids.add(supervisor.session_id);
    ids.add(supervisor.agent_id);
  }
  return {
    count: attention.pending.length + attention.told.length + (question ? 1 : 0),
    ids,
  };
}

/** Whether `agent` is escalated by `needs` (false while the dispatcher's
 * view is unknown: no mark is guessed). */
export function agentNeedsHuman(
  needs: NeedsHuman | null,
  agent: { session_id: string; agent_id: string },
): boolean {
  return (
    needs !== null &&
    (needs.ids.has(agent.session_id) || (agent.agent_id !== "" && needs.ids.has(agent.agent_id)))
  );
}

/** `agents` with `attention` set to whether each is escalated by `needs`:
 * the field the board, the sidebar and their ordering read. */
export function markNeedsHuman<T extends { session_id: string; agent_id: string; attention: boolean }>(
  agents: readonly T[],
  needs: NeedsHuman | null,
): T[] {
  return agents.map((agent) => {
    const attention = agentNeedsHuman(needs, agent);
    return agent.attention === attention ? agent : { ...agent, attention };
  });
}

/** Whether the told-and-unresolved items start folded to a one-line count:
 * when nothing is pending (nothing the supervisor was not told yet), they
 * are old news, each already in a digest. */
export function toldFolded(groups: AttentionGroups): boolean {
  return groups.untold.length === 0;
}

/** What the queue shows of the supervisor's question, in at most three
 * lines: its `HUMAN:` line, or else its reply's last paragraph; and whether
 * the whole reply says more. */
export interface QuestionSummary {
  /** The line or paragraph shown; null when nothing could be read. */
  text: string | null;
  /** The whole reply has more than `text`: offer to show it. */
  more: boolean;
}

/** The last paragraph of `text` (paragraphs part at a blank line). */
export function lastParagraph(text: string): string | null {
  const paragraphs = text
    .split(/\n[ \t]*\n/)
    .map((paragraph) => paragraph.trim())
    .filter((paragraph) => paragraph !== "");
  return paragraphs.at(-1) ?? null;
}

export function questionSummary(question: SupervisorQuestion): QuestionSummary {
  const reply = question.text?.trim() ?? "";
  const human = question.human?.text.trim() ?? "";
  const text = human !== "" ? human : lastParagraph(reply);
  const shown = text ?? "";
  return { text, more: reply !== "" && reply !== shown };
}

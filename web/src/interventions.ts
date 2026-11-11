// The north star on Home (objective 049): human interventions per
// agent-hour over 24 h and 7 d, from `GET /api/interventions?window=`
// (crates/spawn-cli/src/scorecard/interventions.rs), and the interventions
// behind each figure. A person's command refused and sent again is one
// intervention (an intent); its retries are said, not counted. Pure, with
// no import, so node loads it alone (type stripping, no bundler): the
// page's fetch lives in components/fleet/Interventions.tsx.

/** One intervention, as the API lists it. */
export interface InterventionItem {
  /** `command`, `resume`, `owner_task` or `cancel_reassign`. */
  kind: string;
  at_ms: number;
  /** A command's principal, agent and session; null for the other kinds. */
  principal: string | null;
  agent: string | null;
  session: string | null;
  line: string;
  /** An intent's command ids in order; [] for the other kinds. */
  commands: number[];
  attempts: number | null;
  /** `taken`, `failed` or `pending`; null for the other kinds. */
  outcome: string | null;
}

/** `GET /api/interventions?window=24h|7d`. */
export interface InterventionsAnswer {
  window: string;
  since_ms: number;
  until_ms: number;
  agent_hours: number | null;
  total: number;
  by_kind: Record<string, number>;
  retries: number;
  per_agent_hour: number | null;
  items: InterventionItem[];
}

/** One line of the list. */
export interface InterventionRow {
  kind: string;
  /** A command's agent; "" for the other kinds. */
  who: string;
  line: string;
  /** "#<id>, #<id> (<n> tries)"; "" for the other kinds. */
  commands: string;
  outcome: string;
}

export interface InterventionsView {
  /** Per agent-hour to 3 decimals; "—" with no agent hours. */
  figure: string;
  count: string;
  /** Null when no command was sent again. */
  retries: string | null;
  rows: InterventionRow[];
}

function count(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function row(raw: unknown): InterventionRow {
  const item = (typeof raw === "object" && raw !== null ? raw : {}) as Partial<InterventionItem>;
  const kind = text(item.kind);
  const ids = Array.isArray(item.commands)
    ? item.commands.filter((id) => typeof id === "number" && Number.isFinite(id))
    : [];
  const attempts = count(item.attempts);
  let commands = ids.map((id) => `#${id}`).join(", ");
  if (commands !== "" && attempts > 1) commands += ` (${attempts} tries)`;
  return {
    kind,
    who: kind === "command" ? text(item.agent) : "",
    line: text(item.line),
    commands,
    outcome: text(item.outcome),
  };
}

/** The page's words for one window's answer. */
export function interventionsView(
  answer: Partial<InterventionsAnswer> | null | undefined,
): InterventionsView {
  const perHour = answer?.per_agent_hour;
  const figure = typeof perHour === "number" && Number.isFinite(perHour) ? perHour.toFixed(3) : "—";
  const total = count(answer?.total);
  const retries = count(answer?.retries);
  return {
    figure,
    count: total === 1 ? "1 intervention" : `${total} interventions`,
    retries: retries <= 0 ? null : retries === 1 ? "1 retry not counted" : `${retries} retries not counted`,
    rows: Array.isArray(answer?.items) ? answer.items.map(row) : [],
  };
}

/**
 * What one window's figure shows (Home): the answer's view; the reason the
 * read failed when there is no answer (a 500 such as a statement timeout,
 * no server) rather than "Loading…" for ever; or loading, before the first
 * answer or failure. A failure after an answer keeps the answer on screen
 * and says the last read failed, and why.
 */
export type FigureState =
  | { kind: "loading" }
  | { kind: "failed"; reason: string }
  | { kind: "view"; view: InterventionsView; stale: string | null };

export function figureState(
  answer: Partial<InterventionsAnswer> | null | undefined,
  error: string | null | undefined,
): FigureState {
  const reason = typeof error === "string" && error.trim() !== "" ? error.trim() : null;
  if (answer) return { kind: "view", view: interventionsView(answer), stale: reason };
  if (reason) return { kind: "failed", reason };
  return { kind: "loading" };
}

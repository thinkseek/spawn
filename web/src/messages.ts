/* A command's message in words (objective 045).
 *
 * What Spawn types into an agent for a command (an input, a start or a
 * resume with a prompt) is a message, tracked by the command's id from the
 * records: recorded, typed, taken (its marker reached the engine's
 * transcript), answered (the turn it started ended). `GET /api/fleet/
 * <session>/commands` gives each command its `message`, null for one that
 * types nothing. The agent page says it beside the command:
 * "not typed yet", "typed, not taken yet", "not taken (never typed)",
 * "not taken (typed)", "taken in 2 s · answered in 4 min · taken 2×".
 * An answer by the agent (objective 046: `spawn ack <id> done|declined
 * <why>`, or the report of the task the message delivered) follows:
 * " · done", " · declined: <why>".
 *
 * Pure, and loads with node alone (no imports): the black-box scenario
 * runs it as it is. */

/** A command's message as the commands API gives it. */
/** The agent's answer to a message: done, or declined and why. */
export interface MessageActed {
  outcome: "done" | "declined" | string;
  /** Why; null for a done with none. */
  why: string | null;
}

export interface CommandMessage {
  state: "recorded" | "typed" | "taken" | "answered" | "acted" | string;
  typed_at_ms: number | null;
  taken_at_ms: number | null;
  answered_at_ms: number | null;
  /** How many transcript lines took it (2 when the engine wrote it twice). */
  taken: number;
  /** Not taken within its bound (SPAWN_MESSAGE_TAKEN_SECS). */
  not_taken: boolean;
  /** Its answer, null until the agent answered (an older server has none). */
  acted?: MessageActed | null;
  acted_at_ms?: number | null;
}

/** The longest why said whole; a longer one is cut to one less, and "…". */
const WHY_CHARS = 80;

/** The answer in words: "done", "declined: <why>" (a why longer than 80
 * characters cut to its first 79 and "…"); null when none. */
export function actedWords(acted: MessageActed | null | undefined): string | null {
  if (!acted) return null;
  if (acted.outcome !== "declined") return acted.outcome;
  const why = (acted.why ?? "").trim();
  if (why === "") return "declined";
  const chars = Array.from(why);
  const shown = chars.length > WHY_CHARS ? `${chars.slice(0, WHY_CHARS - 1).join("")}…` : why;
  return `declined: ${shown}`;
}

/** What `messageLine` reads of a command. */
export interface MessageCommand {
  created_at_ms: number | null;
  message?: CommandMessage | null;
}

/** A duration, rounded down: "<1 s", "<n> s" under a minute, "<n> min"
 * under an hour, else "<n> h". */
export function messageDuration(ms: number): string {
  const span = Math.max(0, ms);
  if (span < 1_000) return "<1 s";
  if (span < 60_000) return `${Math.floor(span / 1_000)} s`;
  if (span < 3_600_000) return `${Math.floor(span / 60_000)} min`;
  return `${Math.floor(span / 3_600_000)} h`;
}

/** The words for a command's message, null when it has none. Taken: how
 * long after the command was created; answered: how long after it was
 * taken; the count when more than one line took it. */
export function messageLine(command: MessageCommand): string | null {
  const message = command.message;
  if (!message) return null;
  if (message.taken_at_ms === null || message.taken_at_ms === undefined || message.taken < 1) {
    const typed = message.typed_at_ms !== null && message.typed_at_ms !== undefined;
    if (message.not_taken) return typed ? "not taken (typed)" : "not taken (never typed)";
    return typed ? "typed, not taken yet" : "not typed yet";
  }
  const parts = [`taken in ${messageDuration(message.taken_at_ms - (command.created_at_ms ?? message.taken_at_ms))}`];
  if (message.answered_at_ms !== null && message.answered_at_ms !== undefined) {
    parts.push(`answered in ${messageDuration(message.answered_at_ms - message.taken_at_ms)}`);
  }
  if (message.taken > 1) parts.push(`taken ${message.taken}×`);
  const acted = actedWords(message.acted);
  if (acted) parts.push(acted);
  return parts.join(" · ");
}

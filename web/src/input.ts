// A person's input to a session, typed on this page: `POST
// /api/session/<id>/input` (crates/spawn-cli/src/serve.rs), the one thing
// the page writes. This module holds the rules the input box follows: what
// may be sent, what the probe and the answer say, and what became of a
// message, read from the session's commands (`/api/fleet/<id>/commands`:
// its state, and `waiting` once the Bridge holds it for the agent's turn).
//
// Pure: no runtime imports, so `node --test` loads it as it is.

/** Longest text the server takes, in UTF-8 bytes (serve.rs `MAX_INPUT_TEXT`). */
export const MAX_INPUT_TEXT = 8192;

/** A fresh idempotency key, a UUID v4. From `getRandomValues`, which every
 * page has: `crypto.randomUUID` exists only in a secure context, and the
 * intranet page is plain http. */
export function newKey(
  fill: (bytes: Uint8Array) => Uint8Array = (bytes) => crypto.getRandomValues(bytes),
): string {
  const bytes = fill(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** A control character a terminal would read as a key: every one but tab
 * and line ends (the server refuses the same). */
const CONTROL = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/;

/** Why `text` cannot be sent, or null when it can. */
export function textProblem(text: string): string | null {
  if (text.trim() === "") return "Nothing to send.";
  const bytes = new TextEncoder().encode(text).length;
  if (bytes > MAX_INPUT_TEXT) {
    return `Too long: ${bytes} bytes, at most ${MAX_INPUT_TEXT}.`;
  }
  if (CONTROL.test(text)) return "It holds a control character; only tab and line ends are typed.";
  return null;
}

/** Whether this page may write, by the probe's answer. */
export type Writable =
  | { state: "checking" }
  | { state: "yes" }
  | { state: "no"; reason: string };

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : {};
}

function errorText(body: unknown): string | null {
  const error = object(body).error;
  return typeof error === "string" && error !== "" ? error : null;
}

/** Why a write was refused, in one line, by its HTTP status (`null`: the
 * server was not reached). */
function refusal(status: number | null, body: unknown): string {
  if (status === null) return "Cannot reach the server.";
  if (status === 401) {
    return "Read-only here: this page has no token (open it once as /?token=<SPAWN_SERVE_TOKEN>).";
  }
  if (status === 403) {
    return "Read-only from this address: only the owner's own page, an open network's page or the token may type.";
  }
  const error = errorText(body);
  if (status >= 500) return `Input unavailable: ${error ?? `HTTP ${status}`}.`;
  return error ?? `HTTP ${status}`;
}

/** What the probe (`{"probe": true}`) says: yes on its 200; on an older
 * server, which has no such path, read-only. */
export function probeVerdict(status: number | null, body: unknown): Writable {
  if (status === 200 && object(body).writable === true) return { state: "yes" };
  if (status === 200 || status === 400 || status === 404 || status === 405) {
    return { state: "no", reason: "Read-only: this server takes no input (an older spawn serve)." };
  }
  return { state: "no", reason: refusal(status, body) };
}

/** The server's answer to a message: its command, or why it was refused. */
export type SendAnswer =
  | { ok: true; command_id: number }
  | { ok: false; error: string; status: number | null };

export function sendAnswer(status: number | null, body: unknown): SendAnswer {
  const id = object(body).command_id;
  if (status !== null && status >= 200 && status < 300 && typeof id === "number") {
    return { ok: true, command_id: id };
  }
  return { ok: false, error: refusal(status, body), status };
}

/** Whether a message not taken keeps its key for the next try, by the
 * refusal's status. It does (sent again unchanged, it types once), except
 * after 409: the server holds that key for another message (another
 * session's, or other text), so the same key would be refused for good. */
export function keepsKey(status: number | null): boolean {
  return status !== 409;
}

/** One message this page sent (or tried to). */
export interface Sent {
  /** Its idempotency key: sending it again is the same command. */
  key: string;
  text: string;
  at_ms: number;
  /** The command the server made, once it answered. */
  command_id: number | null;
  /** Why it was not taken, when it was not. */
  error: string | null;
  /** What became of it, once nothing more will ([`withOutcomes`]): the
   * commands read are the session's latest few, and a command that newer
   * ones pushed off that list is still typed (or failed). */
  outcome?: Progress;
}

/** A command of the session, as `/api/fleet/<id>/commands` lists it. */
export interface CommandState {
  command_id: number | null;
  state: string | null;
  succeeded: boolean | null;
  error: string | null;
  finished_at_ms: number | null;
  /** The Bridge recorded it waiting for the agent's turn. */
  waiting: boolean;
}

/** What became of a message. */
export type Progress =
  | { stage: "sending" }
  | { stage: "refused"; error: string }
  | { stage: "queued"; command_id: number }
  | { stage: "waiting"; command_id: number }
  | { stage: "typed"; command_id: number; at_ms: number | null }
  | { stage: "failed"; command_id: number; error: string };

/** `sent`'s progress, from the session's `commands` (null while unread: its
 * latest few, newest first). A command not listed is what it was last seen
 * to end as (`outcome`), else queued: not listed yet. */
export function progress(sent: Sent, commands: readonly CommandState[] | null): Progress {
  if (sent.error !== null) return { stage: "refused", error: sent.error };
  const id = sent.command_id;
  if (id === null) return { stage: "sending" };
  const command = commands?.find((entry) => entry.command_id === id);
  if (!command) return sent.outcome ?? { stage: "queued", command_id: id };
  if (command.state === "finished") {
    return command.succeeded === true
      ? { stage: "typed", command_id: id, at_ms: command.finished_at_ms }
      : { stage: "failed", command_id: id, error: command.error ?? "failed" };
  }
  return command.waiting ? { stage: "waiting", command_id: id } : { stage: "queued", command_id: id };
}

/** Whether nothing more will happen to it. */
export function settled(state: Progress): boolean {
  return state.stage === "typed" || state.stage === "failed" || state.stage === "refused";
}

/** `sent` with what became of each message recorded on it (`outcome`) once
 * `commands` shows its command typed or failed: a later reading that no
 * longer lists the command (newer ones took its place) does not turn it
 * back into queued. The same array when nothing new ended. */
export function withOutcomes(sent: Sent[], commands: readonly CommandState[] | null): Sent[] {
  let changed = false;
  const next = sent.map((entry) => {
    if (entry.command_id === null || entry.outcome) return entry;
    const state = progress(entry, commands);
    if (state.stage !== "typed" && state.stage !== "failed") return entry;
    changed = true;
    return { ...entry, outcome: state };
  });
  return changed ? next : sent;
}

/** A reader asked again and again (`tick`) that never has two requests out
 * and never hands on a reading it was not asked for any more: a tick while
 * a request is out starts none (a slow server would otherwise answer
 * several at once, an old answer after a newer one), and after `stop`
 * (the box is gone, or reads another session) the answer of the request
 * still out is dropped and its request aborted. A request that fails keeps
 * the last reading; the next tick may succeed. */
export function serialPoll<T>(
  read: (signal: AbortSignal) => Promise<T>,
  take: (value: T) => void,
): { tick: () => Promise<void>; stop: () => void } {
  let inFlight = false;
  let stopped = false;
  let controller: AbortController | null = null;
  return {
    tick: async () => {
      if (inFlight || stopped) return;
      inFlight = true;
      controller = new AbortController();
      try {
        const value = await read(controller.signal);
        if (!stopped) take(value);
      } catch {
        // Keep the last reading.
      } finally {
        inFlight = false;
      }
    },
    stop: () => {
      stopped = true;
      controller?.abort();
    },
  };
}

/** `sent` with `entry` first: a resend (the same key) replaces its earlier
 * try; at most `keep` are kept. */
export function withSent(sent: readonly Sent[], entry: Sent, keep = 3): Sent[] {
  return [entry, ...sent.filter((other) => other.key !== entry.key)].slice(0, keep);
}

/** Normalize one entry of `/api/fleet/<id>/commands` to what [`progress`]
 * reads (an older server lists no `waiting`). */
export function commandState(value: unknown): CommandState {
  const raw = object(value);
  const number = (field: unknown) =>
    typeof field === "number" && Number.isFinite(field) ? field : null;
  return {
    command_id: number(raw.command_id),
    state: typeof raw.state === "string" ? raw.state : null,
    succeeded: typeof raw.succeeded === "boolean" ? raw.succeeded : null,
    error: typeof raw.error === "string" ? raw.error : null,
    finished_at_ms: number(raw.finished_at_ms),
    waiting: raw.waiting === true,
  };
}

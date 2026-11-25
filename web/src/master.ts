// Master Control (issue #91 P1b): a person's message to the supervisor,
// `POST /api/master/message` with `{text, mode, key}` (crates/spawn-cli/src/
// serve.rs `master_message`). A stopped supervisor is woken first, never
// refused; `steer` presses Esc into a running turn first, `queue` waits for
// the turn, `restart` stops a running supervisor first, `auto` lets the
// server choose. The key is the message's for good (kept in the outbox,
// outbox.ts): sent again, the same request answers the first call's ids,
// so a message replayed after a network drop is typed once.
//
// Pure (no imports), tested in web/test/master.test.ts.

export type MasterMode = "auto" | "steer" | "queue" | "restart";

export const MASTER_MODES: readonly MasterMode[] = ["auto", "steer", "queue", "restart"];
export const MASTER_URL = "/api/master/message";
/** `spawn_postgres::MAX_MASTER_KEY_BYTES`. */
export const MAX_MASTER_KEY_BYTES = 240;

/** What each mode does, in a line. */
export const MODE_HELP: Record<MasterMode, string> = {
  auto: "Let Spawn choose: typed when the supervisor's turn ends, waking it if it is stopped",
  steer: "Interrupt the running turn (Esc) and type this now",
  queue: "Wait for the running turn to end, then type it",
  restart: "Stop the supervisor, start it again, then type it",
};

export function isMode(value: unknown): value is MasterMode {
  return typeof value === "string" && (MASTER_MODES as readonly string[]).includes(value);
}

/** The body of a message. */
export function masterBody(text: string, mode: MasterMode, key: string): { text: string; mode: MasterMode; key: string } {
  return { text, mode, key };
}

export type MasterVerdict =
  | { state: "sent"; command_id: number | null; woke: boolean; replayed: boolean }
  /** Not reached, or the server failed: kept, sent again with its key. */
  | { state: "retry"; error: string }
  /** Refused for good (a bad message, a key used by another, no right to
   * write): dropped from the outbox, the reason shown. */
  | { state: "refused"; error: string; status: number };

type JsonObject = Record<string, unknown>;

const object = (value: unknown): JsonObject | null =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as JsonObject) : null;

/** What the server's answer to a message says. Sent only as the server
 * says it: 202 with a numeric `command_id`. Any other success (a bare 200,
 * a 202 naming no command: a proxy's or a portal's page, not the
 * server's) is not taken as sent: the message stays and is sent again with
 * its key, which the server types once. */
export function masterVerdict(status: number | null, body: unknown): MasterVerdict {
  if (status === null) return { state: "retry", error: "Cannot reach the server: kept, sent when it is back." };
  const value = object(body);
  if (status >= 200 && status < 300) {
    if (status === 202 && typeof value?.command_id === "number" && Number.isFinite(value.command_id)) {
      return {
        state: "sent",
        command_id: value.command_id,
        woke: value.woke === true,
        replayed: value.replayed === true,
      };
    }
    return { state: "retry", error: `HTTP ${status} without a command id: not the server's answer; kept` };
  }
  const error = typeof value?.error === "string" ? value.error : `HTTP ${status}`;
  // 408, 429 and 5xx are the server's moment, not the message's.
  if (status >= 500 || status === 408 || status === 429) return { state: "retry", error };
  return { state: "refused", error, status };
}

/** How long a queued `steer` or `restart` may wait in the outbox and still
 * be sent by itself: past it, an interrupt or a restart of minutes ago is
 * not what the person means now, so it waits for them to send it anyway
 * or discard it. `auto` and `queue` messages never expire. */
export const INTERRUPT_EXPIRY_MS = 5 * 60_000;

/** Whether a queued message of `mode`, written at `createdAtMs`, is too
 * old to send by itself at `nowMs`. */
export function expired(mode: string, createdAtMs: number, nowMs: number): boolean {
  return (mode === "steer" || mode === "restart") && nowMs - createdAtMs > INTERRUPT_EXPIRY_MS;
}

/** How long one send of a message waits for the server. */
export const MASTER_TIMEOUT_MS = 15_000;

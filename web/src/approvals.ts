// The approvals queue (issue #91 P1b): `GET /api/approvals`, the prompts a
// person must answer read from the live panes (numbered choices,
// permission buttons, yes/no, confirm), sessions whose judgement asks for a
// person, and halted missions; and the one click that answers one, `POST
// /api/approvals/<session_id>/act` (crates/spawn-cli/src/serve/approvals.rs).
//
// An answer of a prompt carries the item's `id` as `approval_id`: the server
// presses its keys only while the pane still shows that prompt (409
// otherwise, nothing pressed). Each click has an idempotency key, sent as
// `key` and as the `Idempotency-Key` header: a retried POST presses nothing
// twice. A halted mission's one option is its resume
// (`POST /api/missions/<id>/resume` with `{halt, key}`).
//
// Pure (type imports only), tested in web/test/approvals.test.ts.

export type ApprovalKind = "choice" | "permission" | "yes_no" | "confirm" | "attention" | "mission_halted";
export type ApprovalAction = "allow" | "allow_always" | "deny" | "keys" | "resume";

export interface ApprovalOption {
  id: string;
  label: string;
  keys: string[];
  text: string | null;
  danger: boolean;
  action: ApprovalAction;
}

export interface Approval {
  /** The `approval_id` an answer names. */
  id: string;
  kind: ApprovalKind;
  session_id: string | null;
  agent_id: string | null;
  /** Active terminal backend is `"herdr"` (`herdr-core` / `spawn-hc`); `null`
   * for `mission_halted`. Kept as an open string to preserve defensive parsing
   * of historical payloads. */
  backend: string | null;
  label: string;
  summary: string | null;
  excerpt: string;
  options: ApprovalOption[];
  /** A mission's resume: its mission and halt event. */
  resume: { mission: number; halt: number } | null;
  judgement: { state: string | null; needs_human: number | null; reason: string | null } | null;
  detected_at_ms: number | null;
}

export interface Approvals {
  generated_at_ms: number | null;
  items: Approval[];
  warnings: string[];
}

type JsonObject = Record<string, unknown>;

const object = (value: unknown): JsonObject | null =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as JsonObject) : null;
const text = (value: unknown): string | null => (typeof value === "string" ? value : null);
const number = (value: unknown): number | null =>
  typeof value === "number" && Number.isFinite(value) ? value : null;

const KINDS: readonly string[] = ["choice", "permission", "yes_no", "confirm", "attention", "mission_halted"];
const ACTIONS: readonly string[] = ["allow", "allow_always", "deny", "keys", "resume"];

function option(raw: unknown): ApprovalOption | null {
  const value = object(raw);
  const id = text(value?.id);
  const label = text(value?.label);
  if (!value || !id || label === null || !ACTIONS.includes(value.action as string)) return null;
  return {
    id,
    label,
    keys: Array.isArray(value.keys) ? value.keys.filter((key): key is string => typeof key === "string") : [],
    text: text(value.text),
    danger: value.danger === true,
    action: value.action as ApprovalAction,
  };
}

/** `/api/missions/<id>/resume` and its halt, as the item names them. */
function resumeOf(raw: unknown): Approval["resume"] {
  const value = object(raw);
  const match = /^\/api\/missions\/(\d+)\/resume$/.exec(text(value?.endpoint) ?? "");
  const halt = number(value?.halt);
  return match && halt !== null ? { mission: Number(match[1]), halt } : null;
}

function judgement(raw: unknown): Approval["judgement"] {
  const value = object(raw);
  if (!value) return null;
  return { state: text(value.state), needs_human: number(value.needs_human), reason: text(value.reason) };
}

/** `/api/approvals`' answer. An item without an id or a known kind is
 * left out, an option without a known action dropped; an answer that is
 * not `{items: [...]}` throws (the read failed, not "nothing to answer"). */
export function normalizeApprovals(raw: unknown): Approvals {
  const value = object(raw);
  if (!value || !Array.isArray(value.items)) throw new Error("not a list of approvals");
  const items = value.items
    .map(object)
    .filter((item): item is JsonObject => !!item && !!text(item.id) && KINDS.includes(item.kind as string))
    .map((item) => ({
      id: item.id as string,
      kind: item.kind as ApprovalKind,
      session_id: text(item.session_id),
      agent_id: text(item.agent_id),
      backend: text(item.backend),
      label: text(item.label) ?? "",
      summary: text(item.summary),
      excerpt: text(item.excerpt) ?? "",
      options: Array.isArray(item.options)
        ? item.options.map(option).filter((entry): entry is ApprovalOption => entry !== null)
        : [],
      resume: item.kind === "mission_halted" ? resumeOf(item.resume) : null,
      judgement: judgement(item.judgement),
      detected_at_ms: number(item.detected_at_ms),
    }));
  return {
    generated_at_ms: number(value.generated_at_ms),
    items,
    warnings: Array.isArray(value.warnings)
      ? value.warnings.filter((warning): warning is string => typeof warning === "string")
      : [],
  };
}

/** The body of the act answering `item` with `choice`, under idempotency
 * key `key`: the action, the item's id as `approval_id` and the option's
 * id as `choice_id` (the server presses that option and no other, and only
 * while that prompt shows), and the option's keys for a `keys` answer (a
 * numbered choice's digit). Null for an option that is no act (a
 * mission's resume, an item of no session, a keys answer with no keys). */
export function actBody(item: Approval, choice: ApprovalOption, key: string): Record<string, unknown> | null {
  if (item.session_id === null || choice.action === "resume") return null;
  const body: Record<string, unknown> = { action: choice.action, approval_id: item.id, choice_id: choice.id, key };
  if (choice.action === "keys") {
    if (choice.keys.length === 0) return null;
    body.keys = choice.keys;
  }
  return body;
}

/** The act's URL for a session. */
export function actUrl(sessionId: string): string {
  return `/api/approvals/${encodeURIComponent(sessionId)}/act`;
}

export type ActVerdict =
  | { ok: true; command_ids: number[] }
  /** `retry`: worth sending again with the same key (not reached, 5xx);
   * `stale`: the prompt changed, read the queue again. */
  | { ok: false; error: string; retry: boolean; stale: boolean; refused: boolean };

const errorOf = (body: unknown): string | null => text(object(body)?.error);

function failed(status: number | null, body: unknown): ActVerdict & { ok: false } {
  if (status === null) {
    return { ok: false, error: "Cannot reach the server.", retry: true, stale: false, refused: false };
  }
  const error = errorOf(body) ?? `HTTP ${status}`;
  return {
    ok: false,
    error,
    retry: status >= 500,
    stale: status === 409,
    refused: status === 401 || status === 403,
  };
}

/** What an act's answer says: taken only as the server says it, 202 with
 * its command ids (numbers, at least one); 409 the prompt is not that one
 * any more (or the key was used); 401 or 403 this page may not answer; no
 * answer at all, or a 5xx, is worth a retry with the same key. Any other
 * answer, a bare 200 included, is not taken. */
export function actVerdict(status: number | null, body: unknown): ActVerdict {
  if (status === 202) {
    const ids = object(body)?.command_ids;
    if (Array.isArray(ids) && ids.length > 0 && ids.every((id) => typeof id === "number" && Number.isFinite(id))) {
      return { ok: true, command_ids: ids as number[] };
    }
    return { ok: false, error: "The server took it but named no command.", retry: false, stale: false, refused: false };
  }
  if (status !== null && status >= 200 && status < 300) {
    return { ok: false, error: `HTTP ${status}: not taken as an answer`, retry: false, stale: false, refused: false };
  }
  return failed(status, body);
}

/** What a mission resume's answer says (`POST /api/missions/<id>/resume`):
 * taken only as 202 with its `mission.resumed` event. */
export function resumeVerdict(status: number | null, body: unknown): ActVerdict {
  if (status === 202) {
    const event = object(body)?.event;
    if (event !== null && event !== undefined) return { ok: true, command_ids: [] };
    return { ok: false, error: "The server took it but named no event.", retry: false, stale: false, refused: false };
  }
  if (status !== null && status >= 200 && status < 300) {
    return { ok: false, error: `HTTP ${status}: not taken as a resume`, retry: false, stale: false, refused: false };
  }
  return failed(status, body);
}

/** The probe's answer: may this page answer (`{"probe": true}` first, as
 * the input box asks)? */
export function probeWritable(status: number | null, body: unknown): { writable: boolean; reason: string | null } {
  if (status === 200 && object(body)?.writable === true) return { writable: true, reason: null };
  if (status === null) return { writable: false, reason: "Cannot reach the server." };
  return { writable: false, reason: errorOf(body) ?? `HTTP ${status}` };
}

/** How an item reads in a word or two. */
export function kindText(kind: ApprovalKind): string {
  switch (kind) {
    case "choice":
      return "choice";
    case "permission":
      return "permission";
    case "yes_no":
      return "yes / no";
    case "confirm":
      return "confirm";
    case "attention":
      return "needs a person";
    case "mission_halted":
      return "mission halted";
  }
}

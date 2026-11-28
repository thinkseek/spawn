import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { normalizeAttention, type Attention } from "./attention";
import {
  chooseToken,
  serveCredential,
  unauthorizedReason,
  validateSubmittedToken,
  type AuthPromptReason,
  type TokenChoice,
} from "./token";
import { markFresh, pageHidden, Validated } from "./validated";
import { readTail } from "./sessionWindow.ts";
import type { LiveStatus, LiveTopic } from "./stream.ts";
import { CHANNEL_NAME, LiveStream, topicsOf } from "./stream.ts";
import type { Approvals } from "./approvals.ts";
import { actUrl, normalizeApprovals } from "./approvals.ts";
import type { Digest } from "./digest.ts";
import { normalizeDigest } from "./digest.ts";
import { MASTER_TIMEOUT_MS, MASTER_URL } from "./master.ts";
import type { EventWindow, PageReader } from "./sessionWindow.ts";

export type SessionState =
  "working" | "waiting" | "blocked" | "completed" | "unknown";

export interface Observation {
  state: string;
  reason: string | null;
  based_on_seq: number;
  confidence: number;
  judge: string;
}

export interface OverviewSession {
  session_id: string;
  agent_id: string;
  state: SessionState;
  session_seq: number;
  recorded_at_ms: number;
  observation: Observation | null;
}

/** The task in flight of the RSI objective mission. */
export interface RsiOpenTask {
  id: number;
  state: string;
  objective: number | null;
  revisions: number;
  session_id: string | null;
  last_verdict: string | null;
}

/** The first open objective mission, as `/api/overview` summarises it. */
export interface RsiSummary {
  mission: number;
  halted: string | null;
  baseline: string | null;
  resident_session: string | null;
  next_objective: number | null;
  open_task: RsiOpenTask | null;
  objectives: {
    open: number;
    solved: number;
    met: number;
    /** The owner's own objectives, which the fleet never assigns. */
    owner_judged: number;
  };
}

export interface Overview {
  generated_at_ms: number;
  sessions: OverviewSession[];
  rsi: RsiSummary | null;
}

/** One alert Spawn raised (`/api/alerts`): the same array `spawn alerts
 * --once --json` answers. */
export interface Alert {
  key: string;
  kind: string;
  subject: string;
  state: "open" | "notice" | "cleared";
  fired_event_id: number;
  fired_at_ms: number;
  evidence: unknown;
  cleared_event_id: number | null;
  cleared_at_ms: number | null;
  cleared_evidence: unknown;
  text: string;
  /** An open agent_stuck's session's last scored judgement (the score it
   * stands on may be hours old while the judge is down); null otherwise. */
  scored_at_ms?: number | null;
  /** How it ended (objective 092): null while open and for a notice,
   * "cleared" when its condition cleared it, else the owner's verdict
   * ("false", "stale" or "duplicate", `spawn alert judge`). */
  outcome?: string | null;
  /** A task's or mission's notice that its subject's next one follows:
   * that one's key (objective 151, the notice fold); null otherwise. */
  superseded_by?: string | null;
}

export interface Execution {
  execution_id: string;
  engine: string;
  started_at_ms: number;
  ended_at_ms: number | null;
  cause: string | null;
  exit_code: number | null;
  exit_reported: boolean;
  duration_ms: number | null;
  /** The execution of the same session a restart's resume replaced
   * (`spawn task restart`, the page's Restart); null otherwise. */
  restart_of?: string | null;
}

/** One end of a reassign (`spawn task reassign`, the page's Reassign): a
 * task and the session of its worker. */
export interface TaskLink {
  task: number;
  session_id: string;
}

export interface OpenTool {
  execution_id: string | null;
  tool_use_id: string;
  tool_name: string;
  session_seq: number;
}

export interface Freshness {
  last_session_seq: number;
  last_event_id: number;
  last_event_at_ms: number;
  event_count: number;
}

export interface TimelineEvent {
  session_seq: number;
  event_id: number;
  execution_id: string | null;
  source: string;
  event_name: string;
  detail: unknown;
  recorded_at_ms: number;
}

export interface SessionDetail {
  session_id: string;
  agent_id: string;
  state: SessionState;
  session_seq: number;
  executions: Execution[];
  /** The task this session's task was reassigned to, and its worker's
   * session; null when none was. */
  reassigned_to?: TaskLink | null;
  /** The task this session's task took over, and its old worker's
   * session; null when it took over none. */
  reassigned_from?: TaskLink | null;
  open_tools: OpenTool[];
  freshness: Freshness | null;
  timeline: TimelineEvent[];
}

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : {};
}

function list(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function text(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function number(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function optionalText(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function state(value: unknown): SessionState {
  if (typeof value !== "string") return "unknown";
  const key = value.toLowerCase();
  if (
    key === "working" ||
    key === "waiting" ||
    key === "blocked" ||
    key === "completed"
  ) {
    return key;
  }
  return "unknown";
}

function normalizeObservation(value: unknown): Observation | null {
  if (value === null || typeof value !== "object") return null;
  const raw = object(value);
  return {
    state: text(raw.state, "unknown"),
    reason: optionalText(raw.reason),
    based_on_seq: number(raw.based_on_seq),
    confidence: number(raw.confidence),
    judge: text(raw.judge, "unknown"),
  };
}

function normalizeRsi(value: unknown): RsiSummary | null {
  if (value === null || typeof value !== "object") return null;
  const raw = object(value);
  const mission = optionalNumber(raw.mission);
  if (mission === null) return null;

  const taskRaw = object(raw.open_task);
  const openTask =
    typeof taskRaw.id === "number"
      ? {
          id: number(taskRaw.id),
          state: text(taskRaw.state, "unknown"),
          objective: optionalNumber(taskRaw.objective),
          revisions: number(taskRaw.revisions),
          session_id: optionalText(taskRaw.session_id),
          last_verdict: optionalText(taskRaw.last_verdict),
        }
      : null;
  const counts = object(raw.objectives);

  return {
    mission,
    halted: optionalText(raw.halted),
    baseline: optionalText(raw.baseline),
    resident_session: optionalText(object(raw.resident).session_id),
    next_objective: optionalNumber(raw.next_objective),
    open_task: openTask,
    objectives: {
      open: number(counts.open),
      solved: number(counts.solved),
      met: number(counts.met),
      owner_judged: number(counts.owner_judged),
    },
  };
}

export function normalizeOverview(value: unknown): Overview {
  const raw = object(value);
  return {
    generated_at_ms: number(raw.generated_at_ms, Date.now()),
    sessions: list(raw.sessions).flatMap((entry) => {
      const session = object(entry);
      const sessionId = text(session.session_id);
      if (!sessionId) return [];
      const observation = normalizeObservation(session.observation);
      return [
        {
          session_id: sessionId,
          agent_id: text(session.agent_id),
          // The stated contract carries `state`; the current server derives it
          // from the observation. Accept both.
          state: state(session.state ?? observation?.state),
          session_seq: number(session.session_seq ?? observation?.based_on_seq),
          recorded_at_ms: number(session.recorded_at_ms),
          observation,
        },
      ];
    }),
    rsi: normalizeRsi(raw.rsi),
  };
}

function alertState(value: unknown): Alert["state"] {
  const key = typeof value === "string" ? value.toLowerCase() : "";
  if (key === "open" || key === "notice" || key === "cleared") return key;
  return "open";
}

export function normalizeAlerts(value: unknown): Alert[] {
  return list(value).map((entry) => {
    const alert = object(entry);
    return {
      key: text(alert.key),
      kind: text(alert.kind, "alert"),
      subject: text(alert.subject),
      state: alertState(alert.state),
      fired_event_id: number(alert.fired_event_id),
      fired_at_ms: number(alert.fired_at_ms),
      evidence: alert.evidence ?? null,
      cleared_event_id: optionalNumber(alert.cleared_event_id),
      cleared_at_ms: optionalNumber(alert.cleared_at_ms),
      cleared_evidence: alert.cleared_evidence ?? null,
      text: text(alert.text),
      outcome: optionalText(alert.outcome),
      superseded_by: optionalText(alert.superseded_by),
    };
  });
}

/** A reassign's `{task, session_id}`, or null for anything else. */
function taskLink(value: unknown): TaskLink | null {
  const link = object(value);
  const task = optionalNumber(link.task);
  const session = optionalText(link.session_id);
  return task !== null && session ? { task, session_id: session } : null;
}

export function normalizeSession(value: unknown): SessionDetail {
  const raw = object(value);
  // The stated contract puts state fields at the top level; the current
  // server nests them under `state`. Accept both.
  const stateObject =
    raw.state !== null && typeof raw.state === "object"
      ? object(raw.state)
      : {};
  const observation = normalizeObservation(stateObject.observation);
  return {
    session_id: text(raw.session_id),
    agent_id: text(raw.agent_id, text(stateObject.agent_id)),
    state: state(
      typeof raw.state === "string" ? raw.state : observation?.state,
    ),
    session_seq: number(raw.session_seq ?? stateObject.session_seq),
    executions: list(raw.executions).map((entry) => {
      const execution = object(entry);
      return {
        execution_id: text(execution.execution_id),
        engine: text(execution.engine),
        started_at_ms: number(execution.started_at_ms),
        ended_at_ms: optionalNumber(execution.ended_at_ms),
        cause: optionalText(execution.cause),
        exit_code: optionalNumber(execution.exit_code),
        exit_reported: execution.exit_reported === true,
        duration_ms: optionalNumber(execution.duration_ms),
        restart_of: optionalText(execution.restart_of),
      };
    }),
    reassigned_to: taskLink(raw.reassigned_to),
    reassigned_from: taskLink(raw.reassigned_from),
    open_tools: list(raw.open_tools).map((entry) => {
      const tool = object(entry);
      return {
        execution_id: optionalText(tool.execution_id),
        tool_use_id: text(tool.tool_use_id),
        tool_name: text(tool.tool_name, "tool"),
        session_seq: number(tool.session_seq),
      };
    }),
    freshness:
      raw.freshness !== null && typeof raw.freshness === "object"
        ? {
            last_session_seq: number(object(raw.freshness).last_session_seq),
            last_event_id: number(object(raw.freshness).last_event_id),
            last_event_at_ms: number(object(raw.freshness).last_event_at_ms),
            event_count: number(object(raw.freshness).event_count),
          }
        : null,
    timeline: list(raw.timeline)
      .map((entry) => {
        const event = object(entry);
        return {
          session_seq: number(event.session_seq),
          event_id: number(event.event_id),
          execution_id: optionalText(event.execution_id),
          source: text(event.source, "unknown"),
          event_name: text(event.event_name, "unknown"),
          detail: event.detail,
          recorded_at_ms: number(event.recorded_at_ms),
        };
      })
      .sort(
        (left, right) =>
          left.session_seq - right.session_seq ||
          left.event_id - right.event_id,
      ),
  };
}

const TOKEN_KEY = "spawn.serve.token";

/** The token `spawn serve` wants (`SPAWN_SERVE_TOKEN`): read from the page
 * URL's `?token=` once, then kept in this browser's localStorage, so new
 * tabs, reloads and restarts of the server need no token in the URL. The
 * token reads, and lets this page type into a session; a wrong one is
 * replaced by the next `?token=`.
 *
 * The token leaves the address bar (and so history and screenshots) before
 * storage is touched: a browser whose storage throws keeps it in memory
 * only, never in the URL. */
const tokenChoice: TokenChoice = (() => {
  const fromUrl = new URLSearchParams(window.location.search).get("token");
  if (fromUrl !== null) {
    try {
      window.history.replaceState(null, "", withoutToken(window.location.href));
    } catch {
      // No history API: nothing more can be done about the address bar.
    }
  }
  let saved: string | null = null;
  try {
    saved = localStorage.getItem(TOKEN_KEY);
  } catch {
    // Storage disabled: the token from the URL lives in this page only.
  }
  const choice = chooseToken(fromUrl, saved);
  try {
    if (choice.save !== null) localStorage.setItem(TOKEN_KEY, choice.save);
    else if (choice.clear) localStorage.removeItem(TOKEN_KEY);
  } catch {
    // As above: kept in this page only.
  }
  if (choice.problem) console.warn(`spawn: ${choice.problem}`);
  return choice;
})();

/** The token every API read carries: always a header-safe credential
 * ([`chooseToken`]), never a raw `?token=`. Mutable so the login gate and
 * sign-out button can set or clear it without a full page reload. */
let serveToken: string | null = tokenChoice.token;
let initialTokenProblem: string | null = tokenChoice.problem;

type UnauthorizedListener = (reason: AuthPromptReason) => void;
const unauthorizedListeners = new Set<UnauthorizedListener>();

/** Subscribe to 401 unauthorized events raised by API reads or writes. */
export function subscribeUnauthorized(listener: UnauthorizedListener): () => void {
  unauthorizedListeners.add(listener);
  return () => unauthorizedListeners.delete(listener);
}

function notifyUnauthorized(): void {
  const reason = unauthorizedReason(serveToken !== null, initialTokenProblem);
  for (const listener of unauthorizedListeners) listener(reason);
}

export function getServeToken(): string | null {
  return serveToken;
}

export function getInitialTokenProblem(): string | null {
  return initialTokenProblem;
}

/** Update or clear the active serve token, persist/clear localStorage, and flush cached ETags. */
export function setServeToken(next: string | null): void {
  const normalized = next === null ? null : serveCredential(next);
  serveToken = normalized;
  initialTokenProblem = null;
  try {
    if (normalized !== null) localStorage.setItem(TOKEN_KEY, normalized);
    else localStorage.removeItem(TOKEN_KEY);
  } catch {
    // Storage disabled: kept in this page only.
  }
  reads.clear();
  if (normalized === null) live.stop();
}

export type AuthProbeResult =
  | { kind: "authenticated" }
  | { kind: "unauthorized" }
  | { kind: "unreachable"; message: string };

/** Probe whether the current session (stored token or loopback no-token host) is accepted. */
export async function probeAuth(signal?: AbortSignal): Promise<AuthProbeResult> {
  try {
    const response = await fetch("/api/overview", {
      signal,
      cache: "no-store",
      headers: apiHeaders(),
    });
    await response.arrayBuffer().catch(() => undefined);
    if (response.ok || response.status === 304) return { kind: "authenticated" };
    if (response.status === 401 || response.status === 403) return { kind: "unauthorized" };
    return { kind: "unreachable", message: `HTTP ${response.status}` };
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return {
      kind: "unreachable",
      message: cause instanceof Error ? cause.message : "无法连接到服务端",
    };
  }
}

export type TokenVerifyResult =
  | { ok: true }
  | { ok: false; kind: "invalid" | "rejected" | "unreachable"; message: string };

/** Validate format, verify against `GET /api/overview`, and store on success. */
export async function verifyAndApplyToken(
  raw: string,
  signal?: AbortSignal,
): Promise<TokenVerifyResult> {
  const checked = validateSubmittedToken(raw);
  if (!checked.ok) {
    return { ok: false, kind: "invalid", message: checked.message };
  }
  try {
    const response = await fetch("/api/overview", {
      signal,
      cache: "no-store",
      headers: {
        Accept: "application/json",
        Authorization: `Bearer ${checked.token}`,
      },
    });
    await response.arrayBuffer().catch(() => undefined);
    if (response.ok || response.status === 304) {
      setServeToken(checked.token);
      return { ok: true };
    }
    if (response.status === 401 || response.status === 403) {
      return {
        ok: false,
        kind: "rejected",
        message: "Token 无效或被服务端拒绝（HTTP 401），请核对 SPAWN_SERVE_TOKEN 后重试。",
      };
    }
    return {
      ok: false,
      kind: "unreachable",
      message: `服务端返回异常状态（HTTP ${response.status}），请稍后重试。`,
    };
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return {
      ok: false,
      kind: "unreachable",
      message: cause instanceof Error ? `无法连接服务端：${cause.message}` : "无法连接服务端",
    };
  }
}

/** `href` without its `?token=` parameter; every other part is kept. */
export function withoutToken(href: string): string {
  const url = new URL(href);
  url.searchParams.delete("token");
  return url.toString();
}

/** The error of a failed API read. A 401 says how to give the token. */
export function apiError(status: number, detail = ""): Error {
  if (status === 401) {
    notifyUnauthorized();
    const problem = initialTokenProblem ? `${initialTokenProblem} ` : "";
    return new Error(
      problem +
      (serveToken
        ? "HTTP 401: the saved token is wrong; open this page once as /?token=<SPAWN_SERVE_TOKEN from spawn.env>"
        : "HTTP 401: no token; open this page once as /?token=<SPAWN_SERVE_TOKEN from spawn.env>, or from this machine at http://127.0.0.1:8787"),
    );
  }
  return new Error(detail ? `HTTP ${status}: ${detail}` : `HTTP ${status}`);
}

/** Headers of every API read: JSON, plus the serve token when there is one. */
export function apiHeaders(): Record<string, string> {
  const headers: Record<string, string> = { Accept: "application/json" };
  if (serveToken) headers.Authorization = `Bearer ${serveToken}`;
  return headers;
}

/** The page's last answer at each URL, with its tag (validated.ts). */
const reads = new Validated();

/** GET an API read and normalize it; answered "not modified" (304), the
 * value read last time, the same object. Every module's reads go here. */
export function readApi<T>(
  url: string,
  signal: AbortSignal,
  normalize: (raw: unknown) => T,
): Promise<T> {
  return reads.read(url, normalize, { signal, headers: apiHeaders(), fail: apiError });
}

export async function fetchOverview(signal: AbortSignal): Promise<Overview> {
  return readApi("/api/overview", signal, normalizeOverview);
}

/** The Escalations queue as the dispatcher computes it. Throws on an
 * older server without `/api/attention` (404): the queue falls back. */
export async function fetchAttention(signal: AbortSignal): Promise<Attention> {
  return readApi("/api/attention", signal, normalizeAttention);
}

/** `POST /api/sessions/<id>/input` with `body` (a person's input, or the
 * probe): the HTTP status, null when the server was not reached, and its
 * JSON answer. The one write of the page (crates/spawn-cli/src/serve.rs):
 * the browser adds this page's Origin, and the token goes as with reads. */
export async function postInput(
  sessionId: string,
  body: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<{ status: number | null; body: unknown }> {
  return postWrite(sessionId, "input", body, signal);
}

/** `POST /api/sessions/<id>/key` with `{name, key}`: a person's named key
 * (backlog P1), a command of its own, with the input's guards. 409 when the
 * session runs no execution, with the reason. */
export async function postKey(
  sessionId: string,
  body: { name: string; key: string },
  signal?: AbortSignal,
): Promise<{ status: number | null; body: unknown }> {
  return postWrite(sessionId, "key", body, signal);
}

/** `POST /api/sessions/<id>/restart` or `/reassign` with `{reason, key}`: a
 * person restarts the session's delivered task (a fresh execution of its
 * own session, its work kept) or reassigns it to a fresh worker whose
 * branch starts from this one's work. 202 with `{task, session_id,
 * agent_id, principal}`, the task and worker that now carry the work (and
 * the same answer when the same key is sent again); 409 with the reason
 * when the session holds no delivered task or the key was already used for
 * another move. */
export async function postTaskMove(
  sessionId: string,
  action: "restart" | "reassign",
  body: { reason: string; key: string },
  signal?: AbortSignal,
): Promise<{ status: number | null; body: unknown }> {
  return postWrite(sessionId, action, body, signal);
}

/** `POST /api/missions/<id>/resume` with `{halt, key}`: a person resumes a
 * halted mission on the halt the page showed (its `mission.halted` event).
 * 202 with `{event}`, the `mission.resumed` record (the same one for the
 * same key again); 409 with the reason when the mission is not halted, or
 * is halted on another halt. With the input's guards. */
export async function postResume(
  mission: number,
  body: { halt: number; key: string } | { probe: true },
  signal?: AbortSignal,
): Promise<{ status: number | null; body: unknown }> {
  return postJson(`/api/missions/${encodeURIComponent(String(mission))}/resume`, body, signal);
}

async function postWrite(
  sessionId: string,
  path: "input" | "key" | "restart" | "reassign",
  body: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<{ status: number | null; body: unknown }> {
  return postJson(
    `/api/sessions/${encodeURIComponent(sessionId)}/${path}`,
    body,
    signal,
    keyHeader(body),
  );
}

/** POST `body` as JSON to `url` with the page's headers: the HTTP status,
 * null when the server was not reached, and its JSON answer. */
async function postJson(
  url: string,
  body: Record<string, unknown>,
  signal?: AbortSignal,
  extra: Record<string, string> = {},
): Promise<{ status: number | null; body: unknown }> {
  let response: Response;
  try {
    response = await fetch(url, {
      method: "POST",
      signal,
      cache: "no-store",
      headers: { ...apiHeaders(), "Content-Type": "application/json", ...extra },
      body: JSON.stringify(body),
    });
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return { status: null, body: null };
  }
  if (response.status === 401) {
    notifyUnauthorized();
  }
  let answer: unknown = null;
  try {
    answer = await response.json();
  } catch {
    // Not JSON: the status says enough.
  }
  return { status: response.status, body: answer };
}

/** The same key as the body's `key`, as the `Idempotency-Key` header
 * too (the server refuses the two when they differ). */
function keyHeader(body: Record<string, unknown>): Record<string, string> {
  return typeof body.key === "string" ? { "Idempotency-Key": body.key } : {};
}

/** `GET /api/approvals` (approvals.ts). */
export async function fetchApprovals(signal: AbortSignal): Promise<Approvals> {
  return readApi("/api/approvals", signal, normalizeApprovals);
}

/** `POST /api/approvals/<session_id>/act` with `body` (an answer with its
 * `approval_id` and key, or `{"probe": true}`). */
export async function postApprovalAct(
  sessionId: string,
  body: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<{ status: number | null; body: unknown }> {
  return postJson(actUrl(sessionId), body, signal, keyHeader(body));
}

/** `POST /api/master/message` with `{text, mode, key}` or `{"probe": true}`
 * (master.ts). One that waits more than `timeoutMs` is given up as not
 * reached (status null): the outbox sends it again with its key. Aborted
 * by `signal`, it throws. */
export async function postMaster(
  body: Record<string, unknown>,
  signal?: AbortSignal,
  timeoutMs = MASTER_TIMEOUT_MS,
): Promise<{ status: number | null; body: unknown }> {
  const controller = new AbortController();
  let timedOut = false;
  const timer = window.setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);
  const onAbort = () => controller.abort();
  signal?.addEventListener("abort", onAbort);
  try {
    return await postJson(MASTER_URL, body, controller.signal, keyHeader(body));
  } catch (cause) {
    if (timedOut && !signal?.aborted) {
      return { status: null, body: { error: `No answer in ${Math.round(timeoutMs / 1000)} s.` } };
    }
    throw cause;
  } finally {
    window.clearTimeout(timer);
    signal?.removeEventListener("abort", onAbort);
  }
}

/** `GET /api/context/digest` (digest.ts). */
export async function fetchDigest(signal: AbortSignal): Promise<Digest> {
  return readApi("/api/context/digest", signal, normalizeDigest);
}

/** The page's one live stream (stream.ts), shared among its tabs: started
 * by App, read by every poll that names its topics. */
export const live = new LiveStream({
  fetch: (url, init) => fetch(url, init),
  headers: apiHeaders,
  channel: () => (typeof BroadcastChannel === "function" ? new BroadcastChannel(CHANNEL_NAME) : null),
  locks:
    typeof navigator !== "undefined" && "locks" in navigator && navigator.locks
      ? {
          request: (name: string, callback: () => Promise<void>) => navigator.locks.request(name, callback),
        }
      : null,
  document: typeof document !== "undefined" ? document : null,
  online: typeof window !== "undefined" ? window : null,
});

const subscribeStatus = (onChange: () => void) =>
  live.subscribe((signal) => {
    if (signal.kind === "status") onChange();
  });

/** The live stream's status, rendered again when it changes. */
export function useLiveStatus(): LiveStatus {
  return useSyncExternalStore(subscribeStatus, () => live.status);
}

/** A poll that names live topics polls this many times slower while the
 * stream is live: its events say when to read again. */
export const LIVE_STRETCH = 4;
/** The soonest a live signal reads a poll again after its last read. */
export const LIVE_MIN_GAP_MS = 750;

/** The alerts Spawn raised: open ones and the window's transitions, the
 * array `spawn alerts --once --json` answers. */
export async function fetchAlerts(signal: AbortSignal): Promise<Alert[]> {
  return readApi("/api/alerts", signal, normalizeAlerts);
}

/** The same, over the last `spanMs` (the server's window is 24 h unless
 * asked): home's digest reads as far back as the reader was away. */
export async function fetchAlertsSince(spanMs: number, signal: AbortSignal): Promise<Alert[]> {
  return readApi(`/api/alerts?since=${Math.max(1, Math.round(spanMs))}`, signal, normalizeAlerts);
}

export async function fetchSession(
  id: string,
  signal: AbortSignal,
): Promise<SessionDetail> {
  const url = `/api/sessions/${encodeURIComponent(id)}?after=0&limit=300`;
  return readApi(url, signal, normalizeSession);
}

/** A session's state and its newest events as a window (sessionWindow.ts):
 * one read for the session's latest sequence number (an `after` past any
 * event answers its state with an empty timeline), then the newest `count`
 * sequence numbers, read back further while the observer's hidden events
 * leave it short. `timeline` is the window's events. No endpoint of its
 * own: /api/sessions/<id> pages forward from `after`. */
export interface SessionTail extends SessionDetail {
  window: EventWindow;
}

export async function fetchSessionTail(
  id: string,
  count: number,
  signal: AbortSignal,
): Promise<SessionTail> {
  const base = `/api/sessions/${encodeURIComponent(id)}`;
  const head = await readApi(`${base}?after=${Number.MAX_SAFE_INTEGER}&limit=1`, signal, normalizeSession);
  const latest = Math.max(head.session_seq, head.freshness?.last_session_seq ?? 0);
  const window = await readTail(latest, sessionReader(id, signal), count, count);
  return { ...head, timeline: window.events, window };
}

/** Read `limit` sequence numbers of a session after `after`. */
export function sessionReader(id: string, signal: AbortSignal): PageReader {
  return async (after, limit) =>
    (
      await readApi(
        `/api/sessions/${encodeURIComponent(id)}?after=${after}&limit=${limit}`,
        signal,
        normalizeSession,
      )
    ).timeline;
}

export interface PollResult<T> {
  data: T | null;
  error: string | null;
  /** When the last answer arrived, a "not modified" one included: the
   * data held then. Kept only for a poll that asks (`checked`); null
   * before the first answer. */
  checkedAt?: number | null;
}

export interface PollOptions {
  /** Keep `checkedAt`: a view that says how fresh its data is. Its owner
   * renders again on every answer, so the others do not ask. */
  checked?: boolean;
  /** The live topics (stream.ts `topicsOf`) that make this read stale: a
   * signal of one reads it again at once (at most every
   * [`LIVE_MIN_GAP_MS`]), and while the stream is live the interval is
   * [`LIVE_STRETCH`] times longer. */
  live?: readonly LiveTopic[];
}

/** Poll a read endpoint every `intervalMs`, starting immediately. While
 * the page is hidden (another tab, a locked phone) it does not poll, and
 * it polls at once when the page shows again. An answer that did not
 * change is the same object, so nothing renders again. */
export function usePoll<T>(
  key: string,
  load: (signal: AbortSignal) => Promise<T>,
  intervalMs = 2_000,
  options: PollOptions = {},
): PollResult<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [checkedAt, setCheckedAt] = useState<number | null>(null);
  const loadRef = useRef(load);
  loadRef.current = load;
  const keyRef = useRef<string | null>(null);
  const checked = options.checked === true;
  const liveTopics = (options.live ?? []).join(",");

  useEffect(() => {
    let cancelled = false;
    let inFlight = false;
    let controller: AbortController | null = null;
    // A new key is a different resource: drop the old one. A new interval
    // for the same key keeps what is on screen.
    if (keyRef.current !== key) {
      keyRef.current = key;
      setData(null);
      setError(null);
      setCheckedAt(null);
    }

    const topics = liveTopics === "" ? [] : liveTopics.split(",");
    let lastAt = 0;
    let liveTimer: number | undefined;
    let stale = false;
    const tick = async (force = false, fresh = false) => {
      if (pageHidden(document)) return;
      if (inFlight) {
        if (force) stale = true;
        return;
      }
      // While the stream is live its events say when to read; the timer
      // is only the backstop.
      if (
        !force &&
        topics.length > 0 &&
        live.status.state === "live" &&
        Date.now() - lastAt < intervalMs * LIVE_STRETCH
      ) {
        return;
      }
      inFlight = true;
      lastAt = Date.now();
      controller = new AbortController();
      // A read the live stream asked for wants the answer as it is now,
      // not one shared from up to a second ago (validated.ts FRESH).
      if (fresh) markFresh(controller.signal);
      try {
        const next = await loadRef.current(controller.signal);
        if (!cancelled) {
          setData(next);
          setError(null);
          if (checked) setCheckedAt(Date.now());
        }
      } catch (cause) {
        const aborted =
          cause instanceof DOMException && cause.name === "AbortError";
        if (!cancelled && !aborted) {
          setError(cause instanceof Error ? cause.message : "request failed");
        }
      } finally {
        inFlight = false;
        if (stale && !cancelled) {
          stale = false;
          onLive();
        }
      }
    };
    const onVisibility = () => {
      if (!pageHidden(document)) void tick(true);
    };
    // A live signal of one of its topics: read again soon, once for a
    // burst of events.
    function onLive() {
      if (liveTimer !== undefined) return;
      const wait = Math.max(0, lastAt + LIVE_MIN_GAP_MS - Date.now());
      liveTimer = window.setTimeout(() => {
        liveTimer = undefined;
        void tick(true, true);
      }, wait);
    }
    const unsubscribe =
      topics.length > 0
        ? live.subscribe((signal) => {
            if (topicsOf(signal).some((topic) => topics.includes(topic))) onLive();
          })
        : () => undefined;

    void tick(true);
    const timer = window.setInterval(() => void tick(), intervalMs);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      controller?.abort();
      window.clearInterval(timer);
      window.clearTimeout(liveTimer);
      unsubscribe();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [key, intervalMs, checked, liveTopics]);

  return { data, error, checkedAt };
}

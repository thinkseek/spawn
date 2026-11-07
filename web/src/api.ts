import { useEffect, useRef, useState } from "react";
import { normalizeAttention, type Attention } from "./attention";
import { chooseToken, type TokenChoice } from "./token";
import { pageHidden, Validated } from "./validated";

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
  objectives: { open: number; solved: number; met: number };
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
    };
  });
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
      };
    }),
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
 * ([`chooseToken`]), never a raw `?token=`. */
const serveToken: string | null = tokenChoice.token;

/** `href` without its `?token=` parameter; every other part is kept. */
export function withoutToken(href: string): string {
  const url = new URL(href);
  url.searchParams.delete("token");
  return url.toString();
}

/** The error of a failed API read. A 401 says how to give the token. */
export function apiError(status: number, detail = ""): Error {
  if (status === 401) {
    const problem = tokenChoice.problem ? `${tokenChoice.problem} ` : "";
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

/** `POST /api/session/<id>/input` with `body` (a person's input, or the
 * probe): the HTTP status, null when the server was not reached, and its
 * JSON answer. The one write of the page (crates/spawn-cli/src/serve.rs):
 * the browser adds this page's Origin, and the token goes as with reads. */
export async function postInput(
  sessionId: string,
  body: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<{ status: number | null; body: unknown }> {
  let response: Response;
  try {
    response = await fetch(`/api/session/${encodeURIComponent(sessionId)}/input`, {
      method: "POST",
      signal,
      cache: "no-store",
      headers: { ...apiHeaders(), "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    return { status: null, body: null };
  }
  let answer: unknown = null;
  try {
    answer = await response.json();
  } catch {
    // Not JSON: the status says enough.
  }
  return { status: response.status, body: answer };
}

/** The alerts Spawn raised: open ones and the window's transitions, the
 * array `spawn alerts --once --json` answers. */
export async function fetchAlerts(signal: AbortSignal): Promise<Alert[]> {
  return readApi("/api/alerts", signal, normalizeAlerts);
}

export async function fetchSession(
  id: string,
  signal: AbortSignal,
): Promise<SessionDetail> {
  const url = `/api/session/${encodeURIComponent(id)}?after=0&limit=300`;
  return readApi(url, signal, normalizeSession);
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

    const tick = async () => {
      if (inFlight || pageHidden(document)) return;
      inFlight = true;
      controller = new AbortController();
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
      }
    };
    const onVisibility = () => {
      if (!pageHidden(document)) void tick();
    };

    void tick();
    const timer = window.setInterval(() => void tick(), intervalMs);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      controller?.abort();
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [key, intervalMs, checked]);

  return { data, error, checkedAt };
}

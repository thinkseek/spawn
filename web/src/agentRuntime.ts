// Pure Agent Runtime ADT types, wire parsers, ANSI SGR tokenizer,
// fetch-injectable HTTP client, capability evaluators, and state-machine
// reducers (Wave-AR AR-3).
//
// Zero React, DOM, or `window` imports so `node --test` executes this module
// directly via native TypeScript stripping.

import {
  FORBIDDEN_HOST_OR_ORIGIN_MESSAGE,
  isAbortError,
  type SessionRole,
} from "./token.ts";

// ---------------------------------------------------------------------------
// Wire Domain Types (mirroring crates/spawn-core/src/tui.rs)
// ---------------------------------------------------------------------------

export type AgentStatus = "idle" | "working" | "blocked" | "unknown";

export type TerminalOrigin =
  | {
      kind: "spawn_managed";
      owner_session: string;
      terminal_name: string;
      workspace: string | null;
    }
  | { kind: "external" };

export type ExitCause =
  | { kind: "code"; code: number }
  | { kind: "signal"; signal: number }
  | { kind: "lost"; reason: string | null };

export type TerminalLiveState =
  | { state: "live"; pid: number | null }
  | { state: "exited"; cause: ExitCause };

export interface DiscoveredTerminal {
  address: string;
  machine: string | null;
  name: string | null;
  origin: TerminalOrigin;
  state: TerminalLiveState;
  cwd: string;
  title: string;
  argv: string[];
  labels: Record<string, string>;
  cols: number;
  rows: number;
  agent: string | null;
  agent_status: AgentStatus;
  turn: number;
  keep: boolean;
}

export interface AgentsListResponse {
  agents: DiscoveredTerminal[];
  count: number;
}

export type ReadMode =
  | { mode: "screen" }
  | { mode: "history" }
  | { mode: "stitched" }
  | { mode: "other"; detail: string };

export interface CursorPos {
  row: number;
  col: number;
}

export type SnapshotFidelity =
  | {
      fidelity: "styled_vt";
      cursor: CursorPos | null;
      alt_screen: boolean;
      styled_lines: string[];
    }
  | {
      fidelity: "plain_text_only";
      read_mode: ReadMode;
      complete: boolean;
      why: string | null;
    };

export interface TuiChoice {
  index: number | null;
  label: string;
  selected: boolean;
  line_index: number;
}

export interface TuiSnapshot {
  address: string;
  cols: number;
  rows: number;
  live_state: TerminalLiveState;
  title: string | null;
  fidelity: SnapshotFidelity;
  plain_lines: string[];
  agent: string | null;
  agent_status: AgentStatus;
  rule: string | null;
  blocker_visible: boolean;
  choices: TuiChoice[];
  guard: string;
  snapshot_hash: number;
}

export type StatusPrecondition = "idle" | "working";

export type SendPolicy =
  | { kind: "when_unblocked" }
  | { kind: "force_even_if_blocked" }
  | { kind: "only_if"; expected_guard: string; status?: StatusPrecondition }
  | { kind: "only_if_idle" }
  | { kind: "only_if_working" };

export type TuiAction =
  | {
      action: "send_text";
      text: string;
      policy: SendPolicy;
      wait_timeout_ms?: number;
    }
  | {
      action: "send_keys";
      keys: string[];
      expected_guard?: string;
    }
  | {
      action: "interrupt";
      timeout_ms?: number;
    }
  | {
      action: "wait_settled";
      after_turn?: number;
      output_contains?: string;
      timeout_ms: number;
    };

export type InterruptOutcome =
  | { outcome: "stopped" }
  | { outcome: "exited" }
  | { outcome: "still_working" }
  | { outcome: "unreported" }
  | { outcome: "other"; raw: string };

export type WaitReason =
  | { kind: "status" }
  | { kind: "exit"; cause: ExitCause }
  | { kind: "output" }
  | { kind: "timeout" }
  | { kind: "not_started" }
  | { kind: "children" }
  | { kind: "message"; from: string | null }
  | { kind: "other"; raw: string; ended: boolean };

export type TuiActionOutcome =
  | {
      outcome_kind: "text_sent";
      status: string | null;
      turn: number;
      started: boolean | null;
    }
  | {
      outcome_kind: "keys_sent";
      status: string | null;
      turn: number;
    }
  | {
      outcome_kind: "interrupted";
      outcome: InterruptOutcome;
      key_sent: string | null;
    }
  | {
      outcome_kind: "waited";
      terminal: string;
      reason: WaitReason;
      status: string | null;
      turn: number;
    };

// ---------------------------------------------------------------------------
// Address, KeyChord & ScreenGuard Validators + Endpoint URLs
// ---------------------------------------------------------------------------

const SCREEN_GUARD_RE = /^0[xX][0-9a-fA-F]{1,16}$/;
const ASCII_CONTROL_OR_SPACE_RE = /[\u0000-\u0020\u007f-\u009f\s]/;

/** Mirrors `TerminalAddress::parse` in `crates/spawn-core/src/tui.rs`. */
export function validateTerminalAddress(raw: string): boolean {
  if (
    raw.length === 0 ||
    raw.startsWith("/") ||
    raw.endsWith("/") ||
    raw.includes("//") ||
    ASCII_CONTROL_OR_SPACE_RE.test(raw)
  ) {
    return false;
  }
  const segments = raw.split("/");
  if (segments.length > 2) return false;
  return segments.every((seg) => seg.length > 0 && seg !== "." && seg !== "..");
}

/** Validate `0x`-prefixed 1..16 hex digit ScreenGuard format. */
export function isValidScreenGuard(raw: string): boolean {
  return SCREEN_GUARD_RE.test(raw);
}

const SUPPORTED_NAMED_KEYS = new Map<string, string>([
  ["esc", "esc"],
  ["escape", "esc"],
  ["enter", "enter"],
  ["return", "enter"],
  ["tab", "tab"],
  ["backspace", "backspace"],
  ["space", "space"],
  ["delete", "delete"],
  ["del", "delete"],
  ["insert", "insert"],
  ["home", "home"],
  ["end", "end"],
  ["pageup", "pageup"],
  ["pgup", "pageup"],
  ["pagedown", "pagedown"],
  ["pgdn", "pagedown"],
  ["up", "up"],
  ["down", "down"],
  ["left", "left"],
  ["right", "right"],
  ["backslash", "\\"],
]);

const SINGLE_KEY_CHARS = new Set([
  "`",
  "\\",
  "[",
  "]",
  ",",
  "=",
  "-",
  ".",
  "'",
  ";",
  "/",
]);

function isSupportedSingleKeyChar(ch: string): boolean {
  if (ch.length !== 1) return false;
  return (
    (ch >= "a" && ch <= "z") ||
    (ch >= "0" && ch <= "9") ||
    SINGLE_KEY_CHARS.has(ch)
  );
}

function normalizePrimaryKey(token: string): string | null {
  const lower = token.toLowerCase();
  if (isSupportedSingleKeyChar(lower)) return lower;
  const fMatch = /^f([1-9]|1[0-2])$/.exec(lower);
  if (fMatch) return `f${fMatch[1]}`;
  const named = SUPPORTED_NAMED_KEYS.get(lower);
  if (named) return named;
  const digitMatch = /^digit([0-9])$/.exec(lower);
  if (digitMatch) return digitMatch[1];
  return null;
}

/** Normalize one VT key chord (`"ctrl+c"`, `"Control+Shift+Tab"`, `"down"`), or null fail-closed. */
export function normalizeKeyChord(raw: string): string | null {
  if (raw.length === 0 || /\s/.test(raw)) return null;
  const parts = raw.split("+");
  if (parts.some((p) => p.length === 0)) return null;
  const keyToken = parts[parts.length - 1];
  const modTokens = parts.slice(0, -1);
  let ctrl = false;
  let alt = false;
  let shift = false;
  for (const mod of modTokens) {
    const lower = mod.toLowerCase();
    if (lower === "ctrl" || lower === "control") {
      if (ctrl) return null;
      ctrl = true;
    } else if (lower === "alt" || lower === "meta" || lower === "opt") {
      if (alt) return null;
      alt = true;
    } else if (lower === "shift") {
      if (shift) return null;
      shift = true;
    } else {
      return null;
    }
  }
  const key = normalizePrimaryKey(keyToken);
  if (!key) return null;
  const prefix = `${ctrl ? "ctrl+" : ""}${alt ? "alt+" : ""}${shift ? "shift+" : ""}`;
  return `${prefix}${key}`;
}

/** Parse space/comma-separated VT key chords fail-closed. */
export function parseKeyChordList(
  raw: string,
): { ok: true; keys: string[] } | { ok: false; message: string } {
  const tokens = raw
    .split(/[\s,]+/)
    .map((part) => part.trim())
    .filter((part) => part.length > 0);
  if (tokens.length === 0) {
    return { ok: false, message: "请输入至少一个按键规范（例如 down、enter、ctrl+c）" };
  }
  const normalized: string[] = [];
  for (const token of tokens) {
    const chord = normalizeKeyChord(token);
    if (!chord) {
      return { ok: false, message: `不支持的按键规范：${token}` };
    }
    normalized.push(chord);
  }
  return { ok: true, keys: normalized };
}

export function agentsListUrl(includeAll: boolean): string {
  return includeAll ? "/api/agents?all=true" : "/api/agents";
}

export function agentSnapshotUrl(address: string): string {
  if (!validateTerminalAddress(address)) {
    throw new Error(`invalid terminal address: ${address}`);
  }
  return `/api/agents/${encodeURIComponent(address)}/snapshot`;
}

export function agentActionUrl(address: string): string {
  if (!validateTerminalAddress(address)) {
    throw new Error(`invalid terminal address: ${address}`);
  }
  return `/api/agents/${encodeURIComponent(address)}/action`;
}

// ---------------------------------------------------------------------------
// Fail-Closed Wire Parsers
// ---------------------------------------------------------------------------

type JsonObj = Record<string, unknown>;

function requireObject(value: unknown, label: string): JsonObj {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`expected object for ${label}`);
  }
  return value as JsonObj;
}

function requireString(obj: JsonObj, key: string, label: string): string {
  const val = obj[key];
  if (typeof val !== "string") {
    throw new Error(`expected string ${label}.${key}`);
  }
  return val;
}

function requireOptionalString(obj: JsonObj, key: string, label: string): string | null {
  const val = obj[key];
  if (val === undefined || val === null) return null;
  if (typeof val !== "string") {
    throw new Error(`expected string|null ${label}.${key}`);
  }
  return val;
}

function requireNumber(obj: JsonObj, key: string, label: string): number {
  const val = obj[key];
  if (typeof val !== "number" || !Number.isFinite(val)) {
    throw new Error(`expected finite number ${label}.${key}`);
  }
  return val;
}

function requireOptionalNumber(obj: JsonObj, key: string, label: string): number | null {
  const val = obj[key];
  if (val === undefined || val === null) return null;
  if (typeof val !== "number" || !Number.isFinite(val)) {
    throw new Error(`expected number|null ${label}.${key}`);
  }
  return val;
}

function requireBoolean(obj: JsonObj, key: string, label: string): boolean {
  const val = obj[key];
  if (typeof val !== "boolean") {
    throw new Error(`expected boolean ${label}.${key}`);
  }
  return val;
}

function requireStringArray(obj: JsonObj, key: string, label: string): string[] {
  const val = obj[key];
  if (!Array.isArray(val)) {
    throw new Error(`expected array ${label}.${key}`);
  }
  for (let i = 0; i < val.length; i++) {
    if (typeof val[i] !== "string") {
      throw new Error(`expected string at ${label}.${key}[${i}]`);
    }
  }
  return val as string[];
}

function parseAgentStatus(value: unknown, label: string): AgentStatus {
  if (
    value === "idle" ||
    value === "working" ||
    value === "blocked" ||
    value === "unknown"
  ) {
    return value;
  }
  throw new Error(`unexpected AgentStatus at ${label}: ${String(value)}`);
}

function parseTerminalOrigin(value: unknown, label: string): TerminalOrigin {
  const obj = requireObject(value, label);
  const kind = requireString(obj, "kind", label);
  if (kind === "external") return { kind: "external" };
  if (kind === "spawn_managed") {
    return {
      kind: "spawn_managed",
      owner_session: requireString(obj, "owner_session", label),
      terminal_name: requireString(obj, "terminal_name", label),
      workspace: requireOptionalString(obj, "workspace", label),
    };
  }
  throw new Error(`unexpected TerminalOrigin kind at ${label}: ${kind}`);
}

function parseExitCause(value: unknown, label: string): ExitCause {
  const obj = requireObject(value, label);
  const kind = requireString(obj, "kind", label);
  if (kind === "code") {
    return { kind: "code", code: requireNumber(obj, "code", label) };
  }
  if (kind === "signal") {
    return { kind: "signal", signal: requireNumber(obj, "signal", label) };
  }
  if (kind === "lost") {
    return { kind: "lost", reason: requireOptionalString(obj, "reason", label) };
  }
  throw new Error(`unexpected ExitCause kind at ${label}: ${kind}`);
}

function parseTerminalLiveState(value: unknown, label: string): TerminalLiveState {
  const obj = requireObject(value, label);
  const st = requireString(obj, "state", label);
  if (st === "live") {
    return { state: "live", pid: requireOptionalNumber(obj, "pid", label) };
  }
  if (st === "exited") {
    return {
      state: "exited",
      cause: parseExitCause(obj.cause, `${label}.cause`),
    };
  }
  throw new Error(`unexpected TerminalLiveState at ${label}: ${st}`);
}

function parseLabels(value: unknown, label: string): Record<string, string> {
  const obj = requireObject(value, label);
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(obj)) {
    if (typeof v !== "string") {
      throw new Error(`expected string label at ${label}.${k}`);
    }
    out[k] = v;
  }
  return out;
}

function parseDiscoveredTerminal(value: unknown, label: string): DiscoveredTerminal {
  const obj = requireObject(value, label);
  const address = requireString(obj, "address", label);
  if (!validateTerminalAddress(address)) {
    throw new Error(`invalid terminal address at ${label}: ${address}`);
  }
  return {
    address,
    machine: requireOptionalString(obj, "machine", label),
    name: requireOptionalString(obj, "name", label),
    origin: parseTerminalOrigin(obj.origin, `${label}.origin`),
    state: parseTerminalLiveState(obj.state, `${label}.state`),
    cwd: requireString(obj, "cwd", label),
    title: requireString(obj, "title", label),
    argv: requireStringArray(obj, "argv", label),
    labels: parseLabels(obj.labels, `${label}.labels`),
    cols: requireNumber(obj, "cols", label),
    rows: requireNumber(obj, "rows", label),
    agent: requireOptionalString(obj, "agent", label),
    agent_status: parseAgentStatus(obj.agent_status, `${label}.agent_status`),
    turn: requireNumber(obj, "turn", label),
    keep: requireBoolean(obj, "keep", label),
  };
}

/** Parse `{ agents, count }` returned by `GET /api/agents`. */
export function parseAgentsListResponse(raw: unknown): AgentsListResponse {
  const obj = requireObject(raw, "AgentsListResponse");
  if (!Array.isArray(obj.agents)) {
    throw new Error("expected array AgentsListResponse.agents");
  }
  const agents = obj.agents.map((entry, idx) =>
    parseDiscoveredTerminal(entry, `agents[${idx}]`),
  );
  const count = requireNumber(obj, "count", "AgentsListResponse");
  return { agents, count };
}

function parseReadMode(value: unknown, label: string): ReadMode {
  const obj = requireObject(value, label);
  const mode = requireString(obj, "mode", label);
  if (mode === "screen" || mode === "history" || mode === "stitched") {
    return { mode };
  }
  if (mode === "other") {
    return { mode: "other", detail: requireString(obj, "detail", label) };
  }
  throw new Error(`unexpected ReadMode at ${label}: ${mode}`);
}

function parseCursorPos(value: unknown, label: string): CursorPos | null {
  if (value === undefined || value === null) return null;
  const obj = requireObject(value, label);
  return {
    row: requireNumber(obj, "row", label),
    col: requireNumber(obj, "col", label),
  };
}

function parseSnapshotFidelity(value: unknown, label: string): SnapshotFidelity {
  const obj = requireObject(value, label);
  const kind = requireString(obj, "fidelity", label);
  if (kind === "styled_vt") {
    return {
      fidelity: "styled_vt",
      cursor: parseCursorPos(obj.cursor, `${label}.cursor`),
      alt_screen: requireBoolean(obj, "alt_screen", label),
      styled_lines: requireStringArray(obj, "styled_lines", label),
    };
  }
  if (kind === "plain_text_only") {
    return {
      fidelity: "plain_text_only",
      read_mode: parseReadMode(obj.read_mode, `${label}.read_mode`),
      complete: requireBoolean(obj, "complete", label),
      why: requireOptionalString(obj, "why", label),
    };
  }
  throw new Error(`unexpected SnapshotFidelity at ${label}: ${kind}`);
}

function parseTuiChoice(value: unknown, label: string): TuiChoice {
  const obj = requireObject(value, label);
  return {
    index: requireOptionalNumber(obj, "index", label),
    label: requireString(obj, "label", label),
    selected: requireBoolean(obj, "selected", label),
    line_index: requireNumber(obj, "line_index", label),
  };
}

/** Parse `TuiSnapshot` returned by `GET /api/agents/<addr>/snapshot`. */
export function parseTuiSnapshot(raw: unknown): TuiSnapshot {
  const obj = requireObject(raw, "TuiSnapshot");
  const address = requireString(obj, "address", "TuiSnapshot");
  if (!validateTerminalAddress(address)) {
    throw new Error(`invalid TuiSnapshot.address: ${address}`);
  }
  const guard = requireString(obj, "guard", "TuiSnapshot");
  if (!isValidScreenGuard(guard)) {
    throw new Error(`invalid TuiSnapshot.guard: ${guard}`);
  }
  if (!Array.isArray(obj.choices)) {
    throw new Error("expected array TuiSnapshot.choices");
  }
  return {
    address,
    cols: requireNumber(obj, "cols", "TuiSnapshot"),
    rows: requireNumber(obj, "rows", "TuiSnapshot"),
    live_state: parseTerminalLiveState(obj.live_state, "TuiSnapshot.live_state"),
    title: requireOptionalString(obj, "title", "TuiSnapshot"),
    fidelity: parseSnapshotFidelity(obj.fidelity, "TuiSnapshot.fidelity"),
    plain_lines: requireStringArray(obj, "plain_lines", "TuiSnapshot"),
    agent: requireOptionalString(obj, "agent", "TuiSnapshot"),
    agent_status: parseAgentStatus(obj.agent_status, "TuiSnapshot.agent_status"),
    rule: requireOptionalString(obj, "rule", "TuiSnapshot"),
    blocker_visible: requireBoolean(obj, "blocker_visible", "TuiSnapshot"),
    choices: obj.choices.map((entry, idx) =>
      parseTuiChoice(entry, `TuiSnapshot.choices[${idx}]`),
    ),
    guard,
    snapshot_hash: requireNumber(obj, "snapshot_hash", "TuiSnapshot"),
  };
}

function parseInterruptOutcome(value: unknown, label: string): InterruptOutcome {
  const obj = requireObject(value, label);
  const outcome = requireString(obj, "outcome", label);
  if (
    outcome === "stopped" ||
    outcome === "exited" ||
    outcome === "still_working" ||
    outcome === "unreported"
  ) {
    return { outcome };
  }
  if (outcome === "other") {
    return { outcome: "other", raw: requireString(obj, "raw", label) };
  }
  throw new Error(`unexpected InterruptOutcome at ${label}: ${outcome}`);
}

function parseWaitReason(value: unknown, label: string): WaitReason {
  const obj = requireObject(value, label);
  const kind = requireString(obj, "kind", label);
  if (
    kind === "status" ||
    kind === "output" ||
    kind === "timeout" ||
    kind === "not_started" ||
    kind === "children"
  ) {
    return { kind };
  }
  if (kind === "exit") {
    return { kind: "exit", cause: parseExitCause(obj.cause, `${label}.cause`) };
  }
  if (kind === "message") {
    return { kind: "message", from: requireOptionalString(obj, "from", label) };
  }
  if (kind === "other") {
    return {
      kind: "other",
      raw: requireString(obj, "raw", label),
      ended: requireBoolean(obj, "ended", label),
    };
  }
  throw new Error(`unexpected WaitReason at ${label}: ${kind}`);
}

/** Parse `TuiActionOutcome` returned by `POST /api/agents/<addr>/action`. */
export function parseTuiActionOutcome(raw: unknown): TuiActionOutcome {
  const obj = requireObject(raw, "TuiActionOutcome");
  const kind = requireString(obj, "outcome_kind", "TuiActionOutcome");
  if (kind === "text_sent") {
    const startedRaw = obj.started;
    if (startedRaw !== undefined && startedRaw !== null && typeof startedRaw !== "boolean") {
      throw new Error("expected boolean|null TuiActionOutcome.started");
    }
    return {
      outcome_kind: "text_sent",
      status: requireOptionalString(obj, "status", "TuiActionOutcome"),
      turn: requireNumber(obj, "turn", "TuiActionOutcome"),
      started: startedRaw ?? null,
    };
  }
  if (kind === "keys_sent") {
    return {
      outcome_kind: "keys_sent",
      status: requireOptionalString(obj, "status", "TuiActionOutcome"),
      turn: requireNumber(obj, "turn", "TuiActionOutcome"),
    };
  }
  if (kind === "interrupted") {
    return {
      outcome_kind: "interrupted",
      outcome: parseInterruptOutcome(obj.outcome, "TuiActionOutcome.outcome"),
      key_sent: requireOptionalString(obj, "key_sent", "TuiActionOutcome"),
    };
  }
  if (kind === "waited") {
    return {
      outcome_kind: "waited",
      terminal: requireString(obj, "terminal", "TuiActionOutcome"),
      reason: parseWaitReason(obj.reason, "TuiActionOutcome.reason"),
      status: requireOptionalString(obj, "status", "TuiActionOutcome"),
      turn: requireNumber(obj, "turn", "TuiActionOutcome"),
    };
  }
  throw new Error(`unexpected TuiActionOutcome kind: ${kind}`);
}

// ---------------------------------------------------------------------------
// Closed HTTP Error Classification & Fetch-Injectable Client
// ---------------------------------------------------------------------------

export type AgentHttpError =
  | { kind: "unauthorized"; message: string }
  | { kind: "forbidden_readonly"; message: string }
  | { kind: "forbidden_external"; message: string }
  | { kind: "forbidden_origin"; message: string }
  | { kind: "not_found"; message: string }
  | {
      kind: "guard_mismatch";
      message: string;
      expectedGuard: string | null;
      actualGuard: string | null;
    }
  | { kind: "unobservable_selection"; message: string }
  | { kind: "state_conflict"; message: string }
  | { kind: "bad_request"; message: string }
  | { kind: "unavailable"; message: string }
  | { kind: "network"; message: string };

export type AgentResult<T> =
  | { ok: true; data: T }
  | { ok: false; error: AgentHttpError };

export interface AgentResponseLike {
  ok: boolean;
  status: number;
  headers: { get(name: string): string | null };
  json(): Promise<unknown>;
  arrayBuffer(): Promise<ArrayBuffer>;
}

export type AgentFetch = (
  url: string,
  init: RequestInit,
) => Promise<AgentResponseLike>;

export interface AgentEtagEntry<T> {
  etag: string;
  data: T;
}

const GUARD_MISMATCH_RE = /expected\s+(0x[0-9a-fA-F]{1,16}),\s*got\s+(0x[0-9a-fA-F]{1,16})/i;

/** Map an HTTP status + server `error` message into a closed `AgentHttpError` variant. */
export function classifyAgentHttpError(
  status: number | null,
  detail: string,
  access: "read" | "write",
): AgentHttpError {
  if (status === null) {
    return { kind: "network", message: detail || "无法连接到服务端" };
  }
  if (status === 401) {
    return {
      kind: "unauthorized",
      message: detail || "认证已失效（HTTP 401），请重新提供 Token",
    };
  }
  if (status === 403) {
    if (/external/i.test(detail)) {
      return {
        kind: "forbidden_external",
        message: detail || "只读权限禁止访问非 Spawn 托管的外部终端（HTTP 403）",
      };
    }
    if (access === "write" || /read-only viewer/i.test(detail)) {
      return {
        kind: "forbidden_readonly",
        message: detail || "只读权限受限（HTTP 403）：ReadOnlyViewer 禁止执行变更动作",
      };
    }
    return {
      kind: "forbidden_origin",
      message: detail || FORBIDDEN_HOST_OR_ORIGIN_MESSAGE,
    };
  }
  if (status === 404) {
    return { kind: "not_found", message: detail || "目标终端不存在或已退出（HTTP 404）" };
  }
  if (status === 409) {
    if (/guard_mismatch/i.test(detail)) {
      const m = GUARD_MISMATCH_RE.exec(detail);
      return {
        kind: "guard_mismatch",
        message: detail,
        expectedGuard: m ? m[1] : null,
        actualGuard: m ? m[2] : null,
      };
    }
    if (/unobservable_selection/i.test(detail)) {
      return { kind: "unobservable_selection", message: detail };
    }
    return { kind: "state_conflict", message: detail || "终端状态冲突（HTTP 409）" };
  }
  if (status === 400 || status === 413 || status === 415) {
    return { kind: "bad_request", message: detail || `请求参数无效（HTTP ${status}）` };
  }
  return {
    kind: "unavailable",
    message: detail || `服务暂不可用（HTTP ${status}）`,
  };
}

async function readErrorDetail(response: AgentResponseLike): Promise<string> {
  try {
    const body = await response.json();
    if (
      body !== null &&
      typeof body === "object" &&
      !Array.isArray(body) &&
      typeof (body as JsonObj).error === "string"
    ) {
      return (body as JsonObj).error as string;
    }
  } catch {
    // Non-JSON error body: fallback to HTTP status message.
  }
  return "";
}

async function getAgentJson<T>(
  fetcher: AgentFetch,
  baseHeaders: Record<string, string>,
  url: string,
  parse: (raw: unknown) => T,
  options: {
    signal?: AbortSignal;
    etagCache?: Map<string, AgentEtagEntry<T>>;
    bypassEtag?: boolean;
    onUnauthorized?: () => void;
  } = {},
): Promise<AgentResult<T>> {
  const cached = !options.bypassEtag ? options.etagCache?.get(url) : undefined;
  const headers: Record<string, string> = {
    Accept: "application/json",
    ...baseHeaders,
    "Cache-Control": options.bypassEtag ? "no-cache" : "max-age=1",
  };
  if (cached) {
    headers["If-None-Match"] = cached.etag;
  }
  let response: AgentResponseLike;
  try {
    response = await fetcher(url, {
      method: "GET",
      signal: options.signal,
      cache: "no-store",
      headers,
    });
  } catch (cause) {
    if (isAbortError(cause)) throw cause;
    return {
      ok: false,
      error: classifyAgentHttpError(
        null,
        cause instanceof Error ? cause.message : "网络请求失败",
        "read",
      ),
    };
  }
  if (response.status === 304 && cached) {
    await response.arrayBuffer().catch(() => undefined);
    return { ok: true, data: cached.data };
  }
  if (response.status === 401) {
    const detail = await readErrorDetail(response);
    options.onUnauthorized?.();
    return { ok: false, error: classifyAgentHttpError(401, detail, "read") };
  }
  if (!response.ok) {
    const detail = await readErrorDetail(response);
    return {
      ok: false,
      error: classifyAgentHttpError(response.status, detail, "read"),
    };
  }
  let raw: unknown;
  try {
    raw = await response.json();
  } catch {
    return {
      ok: false,
      error: {
        kind: "unavailable",
        message: "服务端返回了无法解析的 JSON 响应",
      },
    };
  }
  try {
    const parsed = parse(raw);
    const etag = response.headers.get("ETag");
    if (etag && options.etagCache) {
      options.etagCache.set(url, { etag, data: parsed });
    } else if (options.etagCache) {
      options.etagCache.delete(url);
    }
    return { ok: true, data: parsed };
  } catch (err) {
    return {
      ok: false,
      error: {
        kind: "unavailable",
        message: err instanceof Error ? err.message : "响应数据格式校验失败",
      },
    };
  }
}

export async function fetchAgentsWithFetch(
  fetcher: AgentFetch,
  headers: Record<string, string>,
  includeAll: boolean,
  options: {
    signal?: AbortSignal;
    etagCache?: Map<string, AgentEtagEntry<AgentsListResponse>>;
    bypassEtag?: boolean;
    onUnauthorized?: () => void;
  } = {},
): Promise<AgentResult<AgentsListResponse>> {
  return getAgentJson(
    fetcher,
    headers,
    agentsListUrl(includeAll),
    parseAgentsListResponse,
    options,
  );
}

export async function fetchAgentSnapshotWithFetch(
  fetcher: AgentFetch,
  headers: Record<string, string>,
  address: string,
  options: {
    signal?: AbortSignal;
    etagCache?: Map<string, AgentEtagEntry<TuiSnapshot>>;
    bypassEtag?: boolean;
    onUnauthorized?: () => void;
  } = {},
): Promise<AgentResult<TuiSnapshot>> {
  if (!validateTerminalAddress(address)) {
    return {
      ok: false,
      error: { kind: "bad_request", message: `非法终端地址：${address}` },
    };
  }
  return getAgentJson(
    fetcher,
    headers,
    agentSnapshotUrl(address),
    parseTuiSnapshot,
    options,
  );
}

export async function postAgentActionWithFetch(
  fetcher: AgentFetch,
  headers: Record<string, string>,
  address: string,
  action: TuiAction,
  options: {
    signal?: AbortSignal;
    onUnauthorized?: () => void;
  } = {},
): Promise<AgentResult<TuiActionOutcome>> {
  if (!validateTerminalAddress(address)) {
    return {
      ok: false,
      error: { kind: "bad_request", message: `非法终端地址：${address}` },
    };
  }
  let response: AgentResponseLike;
  try {
    response = await fetcher(agentActionUrl(address), {
      method: "POST",
      signal: options.signal,
      cache: "no-store",
      headers: {
        Accept: "application/json",
        ...headers,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(action),
    });
  } catch (cause) {
    if (isAbortError(cause)) throw cause;
    return {
      ok: false,
      error: classifyAgentHttpError(
        null,
        cause instanceof Error ? cause.message : "网络请求失败",
        "write",
      ),
    };
  }
  if (response.status === 401) {
    const detail = await readErrorDetail(response);
    options.onUnauthorized?.();
    return { ok: false, error: classifyAgentHttpError(401, detail, "write") };
  }
  if (!response.ok) {
    const detail = await readErrorDetail(response);
    return {
      ok: false,
      error: classifyAgentHttpError(response.status, detail, "write"),
    };
  }
  let raw: unknown;
  try {
    raw = await response.json();
  } catch {
    return {
      ok: false,
      error: {
        kind: "unavailable",
        message: "服务端返回了无法解析的操作结果",
      },
    };
  }
  try {
    return { ok: true, data: parseTuiActionOutcome(raw) };
  } catch (err) {
    return {
      ok: false,
      error: {
        kind: "unavailable",
        message: err instanceof Error ? err.message : "操作结果校验失败",
      },
    };
  }
}

// ---------------------------------------------------------------------------
// Closed Discriminated Union State Machines (Zero Boolean Combinatorial Bloat)
// ---------------------------------------------------------------------------

export type AgentDiscoveryState =
  | { kind: "loading"; includeAll: boolean }
  | {
      kind: "ready";
      includeAll: boolean;
      agents: DiscoveredTerminal[];
      refreshedAtMs: number;
    }
  | { kind: "unauthorized"; includeAll: boolean; message: string }
  | { kind: "forbidden"; includeAll: boolean; message: string }
  | {
      kind: "error";
      includeAll: boolean;
      message: string;
      previous: DiscoveredTerminal[] | null;
    };

export type DiscoveryEvent =
  | { type: "fetch_started"; includeAll: boolean }
  | {
      type: "fetch_succeeded";
      includeAll: boolean;
      agents: DiscoveredTerminal[];
      nowMs: number;
    }
  | { type: "fetch_failed"; includeAll: boolean; error: AgentHttpError };

export function reduceDiscovery(
  state: AgentDiscoveryState,
  event: DiscoveryEvent,
): AgentDiscoveryState {
  const previous =
    state.kind === "ready"
      ? state.agents
      : state.kind === "error"
        ? state.previous
        : null;
  switch (event.type) {
    case "fetch_started":
      if (state.kind === "ready" && state.includeAll === event.includeAll) {
        return state;
      }
      return { kind: "loading", includeAll: event.includeAll };
    case "fetch_succeeded":
      return {
        kind: "ready",
        includeAll: event.includeAll,
        agents: event.agents,
        refreshedAtMs: event.nowMs,
      };
    case "fetch_failed":
      if (event.error.kind === "unauthorized") {
        return {
          kind: "unauthorized",
          includeAll: event.includeAll,
          message: event.error.message,
        };
      }
      if (
        event.error.kind === "forbidden_origin" ||
        event.error.kind === "forbidden_readonly"
      ) {
        return {
          kind: "forbidden",
          includeAll: event.includeAll,
          message: event.error.message,
        };
      }
      return {
        kind: "error",
        includeAll: event.includeAll,
        message: event.error.message,
        previous,
      };
  }
}

export type SnapshotLoadState =
  | { kind: "idle" }
  | { kind: "loading"; address: string; previous: TuiSnapshot | null }
  | {
      kind: "ready";
      address: string;
      snapshot: TuiSnapshot;
      capturedAtMs: number;
    }
  | { kind: "unauthorized"; address: string; message: string }
  | { kind: "forbidden_external"; address: string; message: string }
  | { kind: "not_found"; address: string; message: string }
  | {
      kind: "error";
      address: string;
      message: string;
      previous: TuiSnapshot | null;
    };

export type SnapshotEvent =
  | { type: "cleared" }
  | { type: "selected"; address: string }
  | {
      type: "fetch_succeeded";
      address: string;
      snapshot: TuiSnapshot;
      nowMs: number;
    }
  | { type: "fetch_failed"; address: string; error: AgentHttpError };

export function reduceSnapshot(
  state: SnapshotLoadState,
  event: SnapshotEvent,
): SnapshotLoadState {
  switch (event.type) {
    case "cleared":
      return { kind: "idle" };
    case "selected": {
      const sameTarget =
        (state.kind === "ready" || state.kind === "loading" || state.kind === "error") &&
        state.address === event.address;
      const previous = sameTarget
        ? state.kind === "ready"
          ? state.snapshot
          : state.previous
        : null;
      if (state.kind === "ready" && state.address === event.address) {
        return state;
      }
      return { kind: "loading", address: event.address, previous };
    }
    case "fetch_succeeded":
      return {
        kind: "ready",
        address: event.address,
        snapshot: event.snapshot,
        capturedAtMs: event.nowMs,
      };
    case "fetch_failed": {
      if (event.error.kind === "unauthorized") {
        return {
          kind: "unauthorized",
          address: event.address,
          message: event.error.message,
        };
      }
      if (event.error.kind === "forbidden_external" || event.error.kind === "forbidden_origin") {
        return {
          kind: "forbidden_external",
          address: event.address,
          message: event.error.message,
        };
      }
      if (event.error.kind === "not_found") {
        return {
          kind: "not_found",
          address: event.address,
          message: event.error.message,
        };
      }
      const samePrevious =
        (state.kind === "ready" || state.kind === "loading" || state.kind === "error") &&
        state.address === event.address
          ? state.kind === "ready"
            ? state.snapshot
            : state.previous
          : null;
      return {
        kind: "error",
        address: event.address,
        message: event.error.message,
        previous: samePrevious,
      };
    }
  }
}

export type ConflictRefreshPhase = "refreshing" | "refreshed" | "refresh_failed";

export type ActionSubmissionState =
  | { kind: "idle" }
  | { kind: "submitting"; address: string; action: TuiAction }
  | {
      kind: "succeeded";
      address: string;
      action: TuiAction;
      outcome: TuiActionOutcome;
      completedAtMs: number;
    }
  | {
      kind: "conflict_guard_mismatch";
      address: string;
      attemptedAction: TuiAction;
      staleGuard: string | null;
      reportedGuard: string | null;
      message: string;
      refreshPhase: ConflictRefreshPhase;
      freshGuard: string | null;
    }
  | { kind: "blocked_unobservable_selection"; address: string; message: string }
  | { kind: "forbidden_readonly"; address: string; message: string }
  | { kind: "unauthorized"; address: string; message: string }
  | { kind: "failed"; address: string; message: string };

export type ActionEvent =
  | { type: "reset" }
  | { type: "submit_started"; address: string; action: TuiAction }
  | {
      type: "submit_succeeded";
      address: string;
      action: TuiAction;
      outcome: TuiActionOutcome;
      nowMs: number;
    }
  | {
      type: "submit_failed";
      address: string;
      action: TuiAction;
      error: AgentHttpError;
    }
  | { type: "conflict_snapshot_refreshed"; address: string; freshGuard: string }
  | { type: "conflict_snapshot_refresh_failed"; address: string };

function guardFromAction(action: TuiAction): string | null {
  if (action.action === "send_text" && action.policy.kind === "only_if") {
    return action.policy.expected_guard;
  }
  if (action.action === "send_keys") {
    return action.expected_guard ?? null;
  }
  return null;
}

export function reduceAction(
  state: ActionSubmissionState,
  event: ActionEvent,
): ActionSubmissionState {
  switch (event.type) {
    case "reset":
      return { kind: "idle" };
    case "submit_started":
      return {
        kind: "submitting",
        address: event.address,
        action: event.action,
      };
    case "submit_succeeded":
      return {
        kind: "succeeded",
        address: event.address,
        action: event.action,
        outcome: event.outcome,
        completedAtMs: event.nowMs,
      };
    case "submit_failed": {
      const err = event.error;
      if (err.kind === "guard_mismatch") {
        return {
          kind: "conflict_guard_mismatch",
          address: event.address,
          attemptedAction: event.action,
          staleGuard: err.expectedGuard ?? guardFromAction(event.action),
          reportedGuard: err.actualGuard,
          message: err.message,
          refreshPhase: "refreshing",
          freshGuard: err.actualGuard,
        };
      }
      if (err.kind === "unobservable_selection") {
        return {
          kind: "blocked_unobservable_selection",
          address: event.address,
          message: err.message,
        };
      }
      if (err.kind === "forbidden_readonly" || err.kind === "forbidden_external" || err.kind === "forbidden_origin") {
        return {
          kind: "forbidden_readonly",
          address: event.address,
          message: err.message,
        };
      }
      if (err.kind === "unauthorized") {
        return {
          kind: "unauthorized",
          address: event.address,
          message: err.message,
        };
      }
      return {
        kind: "failed",
        address: event.address,
        message: err.message,
      };
    }
    case "conflict_snapshot_refreshed":
      if (
        state.kind === "conflict_guard_mismatch" &&
        state.address === event.address
      ) {
        return {
          ...state,
          refreshPhase: "refreshed",
          freshGuard: event.freshGuard,
        };
      }
      return state;
    case "conflict_snapshot_refresh_failed":
      if (
        state.kind === "conflict_guard_mismatch" &&
        state.address === event.address
      ) {
        return {
          ...state,
          refreshPhase: "refresh_failed",
        };
      }
      return state;
  }
}

// ---------------------------------------------------------------------------
// RBAC Visibility & Fail-Closed Action Capability Evaluators
// ---------------------------------------------------------------------------

/** `ReadOnlyViewer` cannot toggle `?all=true` or view External terminals. */
export function effectiveIncludeAll(
  role: SessionRole,
  requestedAll: boolean,
): boolean {
  return role.kind === "viewer" ? false : requestedAll;
}

/** Client-side defense-in-depth mirror of `DiscoveryScope::SpawnManagedOnly`. */
export function visibleAgentsForRole(
  agents: readonly DiscoveredTerminal[],
  role: SessionRole,
): DiscoveredTerminal[] {
  if (role.kind === "viewer") {
    return agents.filter((agent) => agent.origin.kind === "spawn_managed");
  }
  return [...agents];
}

export type GuardedDisableReason =
  | "none"
  | "viewer_readonly"
  | "no_snapshot"
  | "terminal_exited"
  | "plain_text_only";

export type GuardedCapability =
  | { allowed: true; guard: string }
  | { allowed: false; reason: Exclude<GuardedDisableReason, "none">; message: string };

export interface ActionCapabilities {
  canMutate: boolean;
  readOnlyReason: string | null;
  terminalExited: boolean;
  selectionObservable: boolean;
  guarded: GuardedCapability;
}

const VIEWER_READONLY_MSG =
  "当前为 ReadOnlyViewer 只读会话：仅支持观察 Spawn 受管终端，所有变更操作已禁用。";
const PLAIN_TEXT_GUARDED_MSG =
  "PlainTextOnly 快照缺少 SGR 反显与光标保真度，依赖当前选择/画面的 guarded 动作（only_if / SendKeys）已在 UI 与服务端同时禁止。";

/** Compute exact action permissions for the currently selected snapshot & role. */
export function evaluateActionCapabilities(
  role: SessionRole,
  serverReadOnlyRefused: boolean,
  snapshotState: SnapshotLoadState,
): ActionCapabilities {
  const snapshot = snapshotFromState(snapshotState);
  const terminalExited = snapshot !== null && snapshot.live_state.state === "exited";
  const selectionObservable =
    snapshot !== null && snapshot.fidelity.fidelity === "styled_vt";

  if (role.kind === "viewer" || serverReadOnlyRefused) {
    return {
      canMutate: false,
      readOnlyReason: VIEWER_READONLY_MSG,
      terminalExited,
      selectionObservable,
      guarded: {
        allowed: false,
        reason: "viewer_readonly",
        message: VIEWER_READONLY_MSG,
      },
    };
  }

  if (!snapshot) {
    return {
      canMutate: false,
      readOnlyReason: null,
      terminalExited: false,
      selectionObservable: false,
      guarded: {
        allowed: false,
        reason: "no_snapshot",
        message: "正在等待终端实时快照加载完成后才能操作。",
      },
    };
  }

  if (terminalExited) {
    return {
      canMutate: false,
      readOnlyReason: null,
      terminalExited: true,
      selectionObservable,
      guarded: {
        allowed: false,
        reason: "terminal_exited",
        message: "目标终端进程已退出，无法发送输入或按键。",
      },
    };
  }

  if (snapshot.fidelity.fidelity !== "styled_vt") {
    return {
      canMutate: true,
      readOnlyReason: null,
      terminalExited: false,
      selectionObservable: false,
      guarded: {
        allowed: false,
        reason: "plain_text_only",
        message: PLAIN_TEXT_GUARDED_MSG,
      },
    };
  }

  return {
    canMutate: true,
    readOnlyReason: null,
    terminalExited: false,
    selectionObservable: true,
    guarded: {
      allowed: true,
      guard: snapshot.guard,
    },
  };
}

export function snapshotFromState(state: SnapshotLoadState): TuiSnapshot | null {
  if (state.kind === "ready") return state.snapshot;
  if (state.kind === "loading" || state.kind === "error") return state.previous;
  return null;
}

export type SendPolicyMode =
  | "only_if_guard"
  | "only_if_guard_idle"
  | "only_if_guard_working"
  | "when_unblocked"
  | "force_even_if_blocked"
  | "only_if_idle"
  | "only_if_working";

export function isGuardedPolicyMode(mode: SendPolicyMode): boolean {
  return (
    mode === "only_if_guard" ||
    mode === "only_if_guard_idle" ||
    mode === "only_if_guard_working"
  );
}

export function defaultSendPolicyMode(snapshot: TuiSnapshot | null): SendPolicyMode {
  if (snapshot && snapshot.fidelity.fidelity === "styled_vt") {
    return "only_if_guard";
  }
  return "when_unblocked";
}

export function buildSendTextAction(params: {
  text: string;
  mode: SendPolicyMode;
  waitTimeoutMs?: number;
  capabilities: ActionCapabilities;
}): { ok: true; action: TuiAction } | { ok: false; message: string } {
  if (!params.capabilities.canMutate) {
    return {
      ok: false,
      message:
        params.capabilities.readOnlyReason ??
        (params.capabilities.terminalExited
          ? "终端已退出，不能发送文本"
          : "终端尚未就绪"),
    };
  }
  if (params.text.length === 0) {
    return { ok: false, message: "输入内容不能为空" };
  }
  if (
    params.waitTimeoutMs !== undefined &&
    (!Number.isInteger(params.waitTimeoutMs) ||
      params.waitTimeoutMs < 1 ||
      params.waitTimeoutMs > 120_000)
  ) {
    return { ok: false, message: "等待超时需在 1..=120000 ms 之间" };
  }

  let policy: SendPolicy;
  if (isGuardedPolicyMode(params.mode)) {
    if (!params.capabilities.guarded.allowed) {
      return { ok: false, message: params.capabilities.guarded.message };
    }
    const guard = params.capabilities.guarded.guard;
    if (params.mode === "only_if_guard") {
      policy = { kind: "only_if", expected_guard: guard };
    } else if (params.mode === "only_if_guard_idle") {
      policy = { kind: "only_if", expected_guard: guard, status: "idle" };
    } else {
      policy = { kind: "only_if", expected_guard: guard, status: "working" };
    }
  } else if (params.mode === "when_unblocked") {
    policy = { kind: "when_unblocked" };
  } else if (params.mode === "force_even_if_blocked") {
    policy = { kind: "force_even_if_blocked" };
  } else if (params.mode === "only_if_idle") {
    policy = { kind: "only_if_idle" };
  } else {
    policy = { kind: "only_if_working" };
  }

  const action: TuiAction =
    params.waitTimeoutMs !== undefined
      ? {
          action: "send_text",
          text: params.text,
          policy,
          wait_timeout_ms: params.waitTimeoutMs,
        }
      : {
          action: "send_text",
          text: params.text,
          policy,
        };
  return { ok: true, action };
}

export function buildSendKeysAction(params: {
  keys: string[];
  useGuard: boolean;
  capabilities: ActionCapabilities;
}): { ok: true; action: TuiAction } | { ok: false; message: string } {
  if (!params.capabilities.canMutate) {
    return {
      ok: false,
      message:
        params.capabilities.readOnlyReason ??
        (params.capabilities.terminalExited
          ? "终端已退出，不能发送按键"
          : "终端尚未就绪"),
    };
  }
  if (params.keys.length === 0) {
    return { ok: false, message: "至少需要一个按键规范" };
  }
  const normalized: string[] = [];
  for (const rawKey of params.keys) {
    const chord = normalizeKeyChord(rawKey);
    if (!chord) {
      return { ok: false, message: `无效的按键规范：${rawKey}` };
    }
    normalized.push(chord);
  }
  if (params.useGuard) {
    if (!params.capabilities.guarded.allowed) {
      return { ok: false, message: params.capabilities.guarded.message };
    }
    return {
      ok: true,
      action: {
        action: "send_keys",
        keys: normalized,
        expected_guard: params.capabilities.guarded.guard,
      },
    };
  }
  if (!params.capabilities.selectionObservable) {
    return { ok: false, message: PLAIN_TEXT_GUARDED_MSG };
  }
  return {
    ok: true,
    action: {
      action: "send_keys",
      keys: normalized,
    },
  };
}

// ---------------------------------------------------------------------------
// Styled-VT ANSI SGR Tokenizer (pure, XSS-safe span output)
// ---------------------------------------------------------------------------

export type AnsiColorTone =
  | "muted"
  | "danger"
  | "success"
  | "warning"
  | "info"
  | "purple"
  | "accent"
  | "bright";

export interface StyledSpan {
  text: string;
  bold: boolean;
  dim: boolean;
  underline: boolean;
  inverse: boolean;
  fg: AnsiColorTone | null;
  bg: AnsiColorTone | null;
}

interface SgrStyle {
  bold: boolean;
  dim: boolean;
  underline: boolean;
  inverse: boolean;
  fg: AnsiColorTone | null;
  bg: AnsiColorTone | null;
}

function defaultSgrStyle(): SgrStyle {
  return {
    bold: false,
    dim: false,
    underline: false,
    inverse: false,
    fg: null,
    bg: null,
  };
}

function fgToneFromCode(code: number): AnsiColorTone | null {
  switch (code) {
    case 30:
    case 90:
      return "muted";
    case 31:
    case 91:
      return "danger";
    case 32:
    case 92:
      return "success";
    case 33:
    case 93:
      return "warning";
    case 34:
    case 94:
      return "info";
    case 35:
    case 95:
      return "purple";
    case 36:
    case 96:
      return "accent";
    case 37:
    case 97:
      return "bright";
    default:
      return null;
  }
}

function applySgrParams(style: SgrStyle, rawParams: string): SgrStyle {
  let next = { ...style };
  const parts = rawParams === "" ? ["0"] : rawParams.split(";");
  for (let i = 0; i < parts.length; i++) {
    const code = Number.parseInt(parts[i] || "0", 10);
    if (!Number.isFinite(code)) continue;
    if (code === 0) {
      next = defaultSgrStyle();
    } else if (code === 1) {
      next.bold = true;
    } else if (code === 2) {
      next.dim = true;
    } else if (code === 4) {
      next.underline = true;
    } else if (code === 7) {
      next.inverse = true;
    } else if (code === 22) {
      next.bold = false;
      next.dim = false;
    } else if (code === 24) {
      next.underline = false;
    } else if (code === 27) {
      next.inverse = false;
    } else if (code === 39) {
      next.fg = null;
    } else if (code === 49) {
      next.bg = null;
    } else if ((code >= 30 && code <= 37) || (code >= 90 && code <= 97)) {
      next.fg = fgToneFromCode(code);
    } else if ((code >= 40 && code <= 47) || (code >= 100 && code <= 107)) {
      next.bg = fgToneFromCode(code - 10);
    } else if (code === 38 || code === 48 || code === 58) {
      const mode = parts[i + 1];
      if (mode === "5") {
        i += 2;
      } else if (mode === "2") {
        i += 4;
      }
    }
  }
  return next;
}

const ANSI_SEQUENCE_RE = /\u001b\[([0-9;]*)m|\u001b\[[0-9;?]*[@-~]/g;

/** Split a canonicalized `styled_lines` entry into React-safe SGR spans. */
export function parseStyledLine(line: string): StyledSpan[] {
  const spans: StyledSpan[] = [];
  let style = defaultSgrStyle();
  let lastIndex = 0;
  ANSI_SEQUENCE_RE.lastIndex = 0;
  for (;;) {
    const match = ANSI_SEQUENCE_RE.exec(line);
    if (!match) break;
    if (match.index > lastIndex) {
      const chunk = line.slice(lastIndex, match.index);
      if (chunk.length > 0) {
        spans.push({ text: chunk, ...style });
      }
    }
    if (match[1] !== undefined) {
      style = applySgrParams(style, match[1]);
    }
    lastIndex = match.index + match[0].length;
  }
  if (lastIndex < line.length) {
    spans.push({ text: line.slice(lastIndex), ...style });
  }
  return spans;
}

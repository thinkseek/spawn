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
import { tr, type Locale } from "./i18n.ts";

// ---------------------------------------------------------------------------
// Wire Domain Types (mirroring crates/spawn-core/src/tui.rs & agent_events.rs)
// ---------------------------------------------------------------------------

export type AgentStatus = "idle" | "working" | "blocked" | "unknown";

export type SessionFileFormat =
  | "claude_jsonl"
  | "codex_rollout_jsonl"
  | "agy_full_jsonl"
  | "opencode_jsonl"
  | "generic_jsonl";

export type SessionDiscoveryMethod =
  | "process_open_fd"
  | "process_env_and_cwd"
  | "driver_hint";

export type DiscoveryConfidence =
  | "exact_fd"
  | "matched_metadata"
  | "heuristic_recent";

export interface SessionAttachment {
  path: string;
  format: SessionFileFormat;
  method: SessionDiscoveryMethod;
  confidence: DiscoveryConfidence;
  native_session_id: string | null;
  cwd: string | null;
}

export type SessionDiscoveryStatus =
  | {
      status: "attached";
      method: SessionDiscoveryMethod;
      confidence: DiscoveryConfidence;
    }
  | { status: "no_live_pid" }
  | { status: "remote_terminal_unscanned" }
  | { status: "no_matching_session_file"; scanned_roots: number }
  | { status: "read_failed"; reason: string };

export interface TerminalTelemetrySummary {
  process_count: number;
  child_count: number;
  total_rss_kib: number;
  active_command: string | null;
  discovery_status: SessionDiscoveryStatus;
  session_bytes: number | null;
  session_mtime_ms: number | null;
}

export type EventOrigin = "herdr_pty" | "native_session" | "spawn_control";

export type EventFidelity = "screen_only" | "control_only" | "session_enriched";

export type ObservabilitySource =
  | "herdr_pty"
  | "native_session"
  | "spawn_control"
  | "proc_tree"
  | "osc7501";

export interface ProcessNode {
  pid: number;
  ppid: number;
  comm: string;
  state: string;
  rss_kib: number;
  command: string;
}

export interface ProcessTreeTelemetry {
  leader_pid: number;
  process_count: number;
  child_count: number;
  total_rss_kib: number;
  active_leaf_command: string | null;
  processes: ProcessNode[];
}

export interface InFlightTool {
  tool_call_id: string | null;
  tool_name: string;
  summary: string;
  started_at_ms: number | null;
}

export interface UsageTotals {
  last_model: string | null;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
}

export type AgentHealth =
  | { health: "healthy" }
  | { health: "busy" }
  | { health: "blocked"; rule: string }
  | { health: "stalled"; silent_ms: number }
  | { health: "erroring"; error_count: number }
  | { health: "degraded"; reason: string }
  | { health: "exited" };

export interface AgentObservabilitySummary {
  health: AgentHealth;
  discovery_status: SessionDiscoveryStatus;
  sources_observed: ObservabilitySource[];
  turn_count: number;
  tool_call_count: number;
  tool_error_count: number;
  error_count: number;
  blocked_count: number;
  in_flight_tools: InFlightTool[];
  usage_totals: UsageTotals;
  first_event_at_ms: number | null;
  last_event_at_ms: number | null;
  last_tool_name: string | null;
  last_error: string | null;
}

export type FusedEventKind =
  | {
      kind: "session_meta";
      engine: string | null;
      cli_version: string | null;
      cwd: string | null;
      model: string | null;
    }
  | { kind: "turn_started"; turn_id: string | null }
  | {
      kind: "turn_completed";
      turn_id: string | null;
      duration_ms: number | null;
      error: string | null;
    }
  | { kind: "user_input"; summary: string }
  | { kind: "thinking"; summary: string; duration_ms: number | null }
  | { kind: "assistant_message"; summary: string }
  | {
      kind: "tool_started";
      tool_call_id: string | null;
      tool_name: string;
      summary: string;
    }
  | {
      kind: "tool_completed";
      tool_call_id: string | null;
      tool_name: string;
      ok: boolean;
      duration_ms: number | null;
      summary: string;
    }
  | {
      kind: "usage_recorded";
      model: string | null;
      input_tokens: number;
      output_tokens: number;
      cache_read_tokens: number;
      cache_write_tokens: number;
      cumulative: boolean;
    }
  | { kind: "prompt_blocked"; rule: string; choices_count: number }
  | { kind: "process_state"; status: string; rule: string | null }
  | { kind: "error_observed"; message: string };

export interface FusedAgentEvent {
  canonical_key: string;
  timestamp_ms: number | null;
  fidelity: EventFidelity;
  origins: EventOrigin[];
  kind: FusedEventKind;
  session_id: string | null;
  terminal_address: string | null;
}

export interface FusedEventsReport {
  address: string;
  engine: string | null;
  session: SessionAttachment | null;
  fidelity: EventFidelity;
  summary: AgentObservabilitySummary;
  process_tree: ProcessTreeTelemetry | null;
  latest_key: string | null;
  cursor_reset: boolean;
  count: number;
  truncated: boolean;
  events: FusedAgentEvent[];
}

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
  session?: SessionAttachment | null;
  telemetry?: TerminalTelemetrySummary | null;
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

export const MAX_AFTER_KEY_BYTES = 256;
export const DEFAULT_AGENT_EVENTS_LIMIT = 100;
export const MAX_AGENT_EVENTS_LIMIT = 1000;
export const MAX_RETAINED_AGENT_EVENTS = 250;

const AFTER_KEY_CONTROL_RE = /[\u0000-\u001f\u007f-\u009f]/;
const UTF8_ENCODER = new TextEncoder();

/** Mirrors `parse_agent_after_key_query` in `crates/spawn-cli/src/serve.rs`. */
export function validateAfterKey(raw: string): boolean {
  if (raw.length === 0 || AFTER_KEY_CONTROL_RE.test(raw)) {
    return false;
  }
  return UTF8_ENCODER.encode(raw).length <= MAX_AFTER_KEY_BYTES;
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

export function agentEventsUrl(
  address: string,
  options: { limit?: number; afterKey?: string | null } = {},
): string {
  if (!validateTerminalAddress(address)) {
    throw new Error(`invalid terminal address: ${address}`);
  }
  const params = new URLSearchParams();
  if (options.limit !== undefined) {
    if (
      !Number.isInteger(options.limit) ||
      options.limit < 1 ||
      options.limit > MAX_AGENT_EVENTS_LIMIT
    ) {
      throw new Error(
        `limit must be an integer between 1 and ${MAX_AGENT_EVENTS_LIMIT}`,
      );
    }
    params.set("limit", String(options.limit));
  }
  if (options.afterKey !== undefined && options.afterKey !== null) {
    if (!validateAfterKey(options.afterKey)) {
      throw new Error(`invalid after_key cursor: ${options.afterKey}`);
    }
    params.set("after_key", options.afterKey);
  }
  const query = params.toString();
  const base = `/api/agents/${encodeURIComponent(address)}/events`;
  return query.length > 0 ? `${base}?${query}` : base;
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

function parseSessionFileFormat(value: unknown, label: string): SessionFileFormat {
  if (
    value === "claude_jsonl" ||
    value === "codex_rollout_jsonl" ||
    value === "agy_full_jsonl" ||
    value === "opencode_jsonl" ||
    value === "generic_jsonl"
  ) {
    return value;
  }
  throw new Error(`unexpected SessionFileFormat at ${label}: ${String(value)}`);
}

function parseSessionDiscoveryMethod(
  value: unknown,
  label: string,
): SessionDiscoveryMethod {
  if (
    value === "process_open_fd" ||
    value === "process_env_and_cwd" ||
    value === "driver_hint"
  ) {
    return value;
  }
  throw new Error(`unexpected SessionDiscoveryMethod at ${label}: ${String(value)}`);
}

function parseDiscoveryConfidence(value: unknown, label: string): DiscoveryConfidence {
  if (
    value === "exact_fd" ||
    value === "matched_metadata" ||
    value === "heuristic_recent"
  ) {
    return value;
  }
  throw new Error(`unexpected DiscoveryConfidence at ${label}: ${String(value)}`);
}

function parseSessionAttachment(value: unknown, label: string): SessionAttachment {
  const obj = requireObject(value, label);
  return {
    path: requireString(obj, "path", label),
    format: parseSessionFileFormat(obj.format, `${label}.format`),
    method: parseSessionDiscoveryMethod(obj.method, `${label}.method`),
    confidence: parseDiscoveryConfidence(obj.confidence, `${label}.confidence`),
    native_session_id: requireOptionalString(obj, "native_session_id", label),
    cwd: requireOptionalString(obj, "cwd", label),
  };
}

function parseSessionDiscoveryStatus(
  value: unknown,
  label: string,
): SessionDiscoveryStatus {
  const obj = requireObject(value, label);
  const status = requireString(obj, "status", label);
  if (status === "attached") {
    return {
      status: "attached",
      method: parseSessionDiscoveryMethod(obj.method, `${label}.method`),
      confidence: parseDiscoveryConfidence(obj.confidence, `${label}.confidence`),
    };
  }
  if (status === "no_live_pid" || status === "remote_terminal_unscanned") {
    return { status };
  }
  if (status === "no_matching_session_file") {
    return {
      status: "no_matching_session_file",
      scanned_roots: requireNumber(obj, "scanned_roots", label),
    };
  }
  if (status === "read_failed") {
    return {
      status: "read_failed",
      reason: requireString(obj, "reason", label),
    };
  }
  throw new Error(`unexpected SessionDiscoveryStatus at ${label}: ${status}`);
}

function parseTerminalTelemetrySummary(
  value: unknown,
  label: string,
): TerminalTelemetrySummary {
  const obj = requireObject(value, label);
  return {
    process_count: requireNumber(obj, "process_count", label),
    child_count: requireNumber(obj, "child_count", label),
    total_rss_kib: requireNumber(obj, "total_rss_kib", label),
    active_command: requireOptionalString(obj, "active_command", label),
    discovery_status: parseSessionDiscoveryStatus(
      obj.discovery_status,
      `${label}.discovery_status`,
    ),
    session_bytes: requireOptionalNumber(obj, "session_bytes", label),
    session_mtime_ms: requireOptionalNumber(obj, "session_mtime_ms", label),
  };
}

function parseDiscoveredTerminal(value: unknown, label: string): DiscoveredTerminal {
  const obj = requireObject(value, label);
  const address = requireString(obj, "address", label);
  if (!validateTerminalAddress(address)) {
    throw new Error(`invalid terminal address at ${label}: ${address}`);
  }
  const parsed: DiscoveredTerminal = {
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
  if (obj.session !== undefined && obj.session !== null) {
    parsed.session = parseSessionAttachment(obj.session, `${label}.session`);
  }
  if (obj.telemetry !== undefined && obj.telemetry !== null) {
    parsed.telemetry = parseTerminalTelemetrySummary(
      obj.telemetry,
      `${label}.telemetry`,
    );
  }
  return parsed;
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

function parseEventOrigin(value: unknown, label: string): EventOrigin {
  if (
    value === "herdr_pty" ||
    value === "native_session" ||
    value === "spawn_control"
  ) {
    return value;
  }
  throw new Error(`unexpected EventOrigin at ${label}: ${String(value)}`);
}

function parseEventFidelity(value: unknown, label: string): EventFidelity {
  if (
    value === "screen_only" ||
    value === "control_only" ||
    value === "session_enriched"
  ) {
    return value;
  }
  throw new Error(`unexpected EventFidelity at ${label}: ${String(value)}`);
}

function parseObservabilitySource(
  value: unknown,
  label: string,
): ObservabilitySource {
  if (
    value === "herdr_pty" ||
    value === "native_session" ||
    value === "spawn_control" ||
    value === "proc_tree" ||
    value === "osc7501"
  ) {
    return value;
  }
  throw new Error(`unexpected ObservabilitySource at ${label}: ${String(value)}`);
}

function parseProcessNode(value: unknown, label: string): ProcessNode {
  const obj = requireObject(value, label);
  return {
    pid: requireNumber(obj, "pid", label),
    ppid: requireNumber(obj, "ppid", label),
    comm: requireString(obj, "comm", label),
    state: requireString(obj, "state", label),
    rss_kib: requireNumber(obj, "rss_kib", label),
    command: requireString(obj, "command", label),
  };
}

function parseProcessTreeTelemetry(
  value: unknown,
  label: string,
): ProcessTreeTelemetry {
  const obj = requireObject(value, label);
  const rawProcesses = obj.processes ?? [];
  if (!Array.isArray(rawProcesses)) {
    throw new Error(`expected array at ${label}.processes`);
  }
  return {
    leader_pid: requireNumber(obj, "leader_pid", label),
    process_count: requireNumber(obj, "process_count", label),
    child_count: requireNumber(obj, "child_count", label),
    total_rss_kib: requireNumber(obj, "total_rss_kib", label),
    active_leaf_command: requireOptionalString(obj, "active_leaf_command", label),
    processes: rawProcesses.map((entry, idx) =>
      parseProcessNode(entry, `${label}.processes[${idx}]`),
    ),
  };
}

function parseInFlightTool(value: unknown, label: string): InFlightTool {
  const obj = requireObject(value, label);
  return {
    tool_call_id: requireOptionalString(obj, "tool_call_id", label),
    tool_name: requireString(obj, "tool_name", label),
    summary: requireString(obj, "summary", label),
    started_at_ms: requireOptionalNumber(obj, "started_at_ms", label),
  };
}

function parseUsageTotals(value: unknown, label: string): UsageTotals {
  const obj = requireObject(value, label);
  return {
    last_model: requireOptionalString(obj, "last_model", label),
    input_tokens: requireNumber(obj, "input_tokens", label),
    output_tokens: requireNumber(obj, "output_tokens", label),
    cache_read_tokens: requireNumber(obj, "cache_read_tokens", label),
    cache_write_tokens: requireNumber(obj, "cache_write_tokens", label),
  };
}

function parseAgentHealth(value: unknown, label: string): AgentHealth {
  const obj = requireObject(value, label);
  const health = requireString(obj, "health", label);
  if (health === "healthy" || health === "busy" || health === "exited") {
    return { health };
  }
  if (health === "blocked") {
    return { health: "blocked", rule: requireString(obj, "rule", label) };
  }
  if (health === "stalled") {
    return { health: "stalled", silent_ms: requireNumber(obj, "silent_ms", label) };
  }
  if (health === "erroring") {
    return {
      health: "erroring",
      error_count: requireNumber(obj, "error_count", label),
    };
  }
  if (health === "degraded") {
    return { health: "degraded", reason: requireString(obj, "reason", label) };
  }
  throw new Error(`unexpected AgentHealth at ${label}: ${health}`);
}

function parseObservabilitySummary(
  value: unknown,
  label: string,
): AgentObservabilitySummary {
  const obj = requireObject(value, label);
  if (!Array.isArray(obj.sources_observed)) {
    throw new Error(`expected array at ${label}.sources_observed`);
  }
  const rawTools = obj.in_flight_tools ?? [];
  if (!Array.isArray(rawTools)) {
    throw new Error(`expected array at ${label}.in_flight_tools`);
  }
  return {
    health: parseAgentHealth(obj.health, `${label}.health`),
    discovery_status: parseSessionDiscoveryStatus(
      obj.discovery_status,
      `${label}.discovery_status`,
    ),
    sources_observed: obj.sources_observed.map((entry, idx) =>
      parseObservabilitySource(entry, `${label}.sources_observed[${idx}]`),
    ),
    turn_count: requireNumber(obj, "turn_count", label),
    tool_call_count: requireNumber(obj, "tool_call_count", label),
    tool_error_count: requireNumber(obj, "tool_error_count", label),
    error_count: requireNumber(obj, "error_count", label),
    blocked_count: requireNumber(obj, "blocked_count", label),
    in_flight_tools: rawTools.map((entry, idx) =>
      parseInFlightTool(entry, `${label}.in_flight_tools[${idx}]`),
    ),
    usage_totals: parseUsageTotals(obj.usage_totals, `${label}.usage_totals`),
    first_event_at_ms: requireOptionalNumber(obj, "first_event_at_ms", label),
    last_event_at_ms: requireOptionalNumber(obj, "last_event_at_ms", label),
    last_tool_name: requireOptionalString(obj, "last_tool_name", label),
    last_error: requireOptionalString(obj, "last_error", label),
  };
}

function parseFusedEventKind(value: unknown, label: string): FusedEventKind {
  const obj = requireObject(value, label);
  const kind = requireString(obj, "kind", label);
  switch (kind) {
    case "session_meta":
      return {
        kind,
        engine: requireOptionalString(obj, "engine", label),
        cli_version: requireOptionalString(obj, "cli_version", label),
        cwd: requireOptionalString(obj, "cwd", label),
        model: requireOptionalString(obj, "model", label),
      };
    case "turn_started":
      return {
        kind,
        turn_id: requireOptionalString(obj, "turn_id", label),
      };
    case "turn_completed":
      return {
        kind,
        turn_id: requireOptionalString(obj, "turn_id", label),
        duration_ms: requireOptionalNumber(obj, "duration_ms", label),
        error: requireOptionalString(obj, "error", label),
      };
    case "user_input":
      return { kind, summary: requireString(obj, "summary", label) };
    case "thinking":
      return {
        kind,
        summary: requireString(obj, "summary", label),
        duration_ms: requireOptionalNumber(obj, "duration_ms", label),
      };
    case "assistant_message":
      return { kind, summary: requireString(obj, "summary", label) };
    case "tool_started":
      return {
        kind,
        tool_call_id: requireOptionalString(obj, "tool_call_id", label),
        tool_name: requireString(obj, "tool_name", label),
        summary: requireString(obj, "summary", label),
      };
    case "tool_completed":
      return {
        kind,
        tool_call_id: requireOptionalString(obj, "tool_call_id", label),
        tool_name: requireString(obj, "tool_name", label),
        ok: requireBoolean(obj, "ok", label),
        duration_ms: requireOptionalNumber(obj, "duration_ms", label),
        summary: requireString(obj, "summary", label),
      };
    case "usage_recorded": {
      const cumRaw = obj.cumulative;
      if (cumRaw !== undefined && typeof cumRaw !== "boolean") {
        throw new Error(`expected boolean at ${label}.cumulative`);
      }
      return {
        kind,
        model: requireOptionalString(obj, "model", label),
        input_tokens: requireNumber(obj, "input_tokens", label),
        output_tokens: requireNumber(obj, "output_tokens", label),
        cache_read_tokens: requireNumber(obj, "cache_read_tokens", label),
        cache_write_tokens: requireNumber(obj, "cache_write_tokens", label),
        cumulative: cumRaw ?? false,
      };
    }
    case "prompt_blocked":
      return {
        kind,
        rule: requireString(obj, "rule", label),
        choices_count: requireNumber(obj, "choices_count", label),
      };
    case "process_state":
      return {
        kind,
        status: requireString(obj, "status", label),
        rule: requireOptionalString(obj, "rule", label),
      };
    case "error_observed":
      return { kind, message: requireString(obj, "message", label) };
    default:
      throw new Error(`unexpected FusedEventKind at ${label}: ${kind}`);
  }
}

function parseFusedAgentEvent(value: unknown, label: string): FusedAgentEvent {
  const obj = requireObject(value, label);
  if (!Array.isArray(obj.origins)) {
    throw new Error(`expected array at ${label}.origins`);
  }
  return {
    canonical_key: requireString(obj, "canonical_key", label),
    timestamp_ms: requireOptionalNumber(obj, "timestamp_ms", label),
    fidelity: parseEventFidelity(obj.fidelity, `${label}.fidelity`),
    origins: obj.origins.map((entry, idx) =>
      parseEventOrigin(entry, `${label}.origins[${idx}]`),
    ),
    kind: parseFusedEventKind(obj.kind, `${label}.kind`),
    session_id: requireOptionalString(obj, "session_id", label),
    terminal_address: requireOptionalString(obj, "terminal_address", label),
  };
}

/** Parse `FusedEventsReport` returned by `GET /api/agents/<addr>/events`. */
export function parseFusedEventsReport(raw: unknown): FusedEventsReport {
  const obj = requireObject(raw, "FusedEventsReport");
  const address = requireString(obj, "address", "FusedEventsReport");
  if (!validateTerminalAddress(address)) {
    throw new Error(`invalid FusedEventsReport.address: ${address}`);
  }
  if (!Array.isArray(obj.events)) {
    throw new Error("expected array FusedEventsReport.events");
  }
  const cursorResetRaw = obj.cursor_reset;
  if (cursorResetRaw !== undefined && typeof cursorResetRaw !== "boolean") {
    throw new Error("expected boolean FusedEventsReport.cursor_reset");
  }
  return {
    address,
    engine: requireOptionalString(obj, "engine", "FusedEventsReport"),
    session:
      obj.session === undefined || obj.session === null
        ? null
        : parseSessionAttachment(obj.session, "FusedEventsReport.session"),
    fidelity: parseEventFidelity(obj.fidelity, "FusedEventsReport.fidelity"),
    summary: parseObservabilitySummary(obj.summary, "FusedEventsReport.summary"),
    process_tree:
      obj.process_tree === undefined || obj.process_tree === null
        ? null
        : parseProcessTreeTelemetry(
            obj.process_tree,
            "FusedEventsReport.process_tree",
          ),
    latest_key: requireOptionalString(obj, "latest_key", "FusedEventsReport"),
    cursor_reset: cursorResetRaw ?? false,
    count: requireNumber(obj, "count", "FusedEventsReport"),
    truncated: requireBoolean(obj, "truncated", "FusedEventsReport"),
    events: obj.events.map((entry, idx) =>
      parseFusedAgentEvent(entry, `FusedEventsReport.events[${idx}]`),
    ),
  };
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

export async function fetchAgentEventsWithFetch(
  fetcher: AgentFetch,
  headers: Record<string, string>,
  address: string,
  options: {
    limit?: number;
    afterKey?: string | null;
    signal?: AbortSignal;
    etagCache?: Map<string, AgentEtagEntry<FusedEventsReport>>;
    bypassEtag?: boolean;
    onUnauthorized?: () => void;
  } = {},
): Promise<AgentResult<FusedEventsReport>> {
  if (!validateTerminalAddress(address)) {
    return {
      ok: false,
      error: { kind: "bad_request", message: `非法终端地址：${address}` },
    };
  }
  let url: string;
  try {
    url = agentEventsUrl(address, {
      limit: options.limit,
      afterKey: options.afterKey,
    });
  } catch (err) {
    return {
      ok: false,
      error: {
        kind: "bad_request",
        message: err instanceof Error ? err.message : "非法事件流查询参数",
      },
    };
  }
  return getAgentJson(fetcher, headers, url, parseFusedEventsReport, {
    signal: options.signal,
    etagCache: options.etagCache,
    bypassEtag: options.bypassEtag,
    onUnauthorized: options.onUnauthorized,
  });
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

function stringArrayEqual(a: readonly string[], b: readonly string[]): boolean {
  if (a === b) return true;
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

function snapshotsEquivalent(a: TuiSnapshot, b: TuiSnapshot): boolean {
  if (a === b) return true;
  if (
    a.guard !== b.guard ||
    a.agent_status !== b.agent_status ||
    a.live_state.state !== b.live_state.state ||
    a.cols !== b.cols ||
    a.rows !== b.rows ||
    a.title !== b.title ||
    a.blocker_visible !== b.blocker_visible ||
    a.choices.length !== b.choices.length ||
    a.fidelity.fidelity !== b.fidelity.fidelity ||
    !stringArrayEqual(a.plain_lines, b.plain_lines)
  ) {
    return false;
  }
  if (a.fidelity.fidelity === "styled_vt" && b.fidelity.fidelity === "styled_vt") {
    return stringArrayEqual(a.fidelity.styled_lines, b.fidelity.styled_lines);
  }
  return true;
}

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
    case "fetch_succeeded": {
      if (
        state.kind === "ready" &&
        state.address === event.address &&
        snapshotsEquivalent(state.snapshot, event.snapshot)
      ) {
        if (event.nowMs === state.capturedAtMs || event.nowMs - state.capturedAtMs < 15_000) {
          return state;
        }
        return {
          kind: "ready",
          address: event.address,
          snapshot: state.snapshot,
          capturedAtMs: event.nowMs,
        };
      }
      return {
        kind: "ready",
        address: event.address,
        snapshot: event.snapshot,
        capturedAtMs: event.nowMs,
      };
    }
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

/** Client-side defense-in-depth mirror of `DiscoveryScope::SpawnManagedOnly`.
 * Note: `?all=true` controls `include_exited` on the server while External
 * visibility is strictly governed by the caller's RBAC principal. */
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
  selectedAddress: string | null,
): ActionCapabilities {
  const snapshot = snapshotFromState(snapshotState, selectedAddress);
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

export function snapshotFromState(
  state: SnapshotLoadState,
  selectedAddress: string | null,
): TuiSnapshot | null {
  if (selectedAddress === null) return null;
  if (state.kind === "ready") {
    return state.address === selectedAddress &&
      state.snapshot.address === selectedAddress
      ? state.snapshot
      : null;
  }
  if (state.kind === "loading" || state.kind === "error") {
    return state.address === selectedAddress &&
      state.previous !== null &&
      state.previous.address === selectedAddress
      ? state.previous
      : null;
  }
  return null;
}

export interface AgentEventsViewData {
  address: string;
  engine: string | null;
  session: SessionAttachment | null;
  fidelity: EventFidelity;
  summary: AgentObservabilitySummary;
  process_tree: ProcessTreeTelemetry | null;
  latest_key: string | null;
  events: FusedAgentEvent[];
  truncated: boolean;
  cursorResetCount: number;
  lastCursorResetAtMs: number | null;
  refreshedAtMs: number;
}

export type AgentEventsLoadState =
  | { kind: "idle" }
  | { kind: "loading"; address: string; previous: AgentEventsViewData | null }
  | { kind: "ready"; address: string; data: AgentEventsViewData }
  | { kind: "unauthorized"; address: string; message: string }
  | { kind: "forbidden_external"; address: string; message: string }
  | { kind: "not_found"; address: string; message: string }
  | {
      kind: "error";
      address: string;
      message: string;
      previous: AgentEventsViewData | null;
    };

export type AgentEventsAction =
  | { type: "cleared" }
  | { type: "selected"; address: string }
  | {
      type: "fetch_succeeded";
      address: string;
      requestedAfterKey: string | null;
      report: FusedEventsReport;
      nowMs: number;
      maxEvents?: number;
    }
  | { type: "fetch_failed"; address: string; error: AgentHttpError };

function mergeFusedEvents(
  previous: readonly FusedAgentEvent[],
  incoming: readonly FusedAgentEvent[],
  maxEvents: number,
): FusedAgentEvent[] {
  const cap = Math.max(1, maxEvents);
  const merged = [...previous];
  const indexByKey = new Map<string, number>();
  for (let i = 0; i < merged.length; i += 1) {
    indexByKey.set(merged[i].canonical_key, i);
  }
  for (const ev of incoming) {
    const existingIdx = indexByKey.get(ev.canonical_key);
    if (existingIdx !== undefined) {
      merged[existingIdx] = ev;
    } else {
      indexByKey.set(ev.canonical_key, merged.length);
      merged.push(ev);
    }
  }
  if (merged.length <= cap) return merged;
  return merged.slice(merged.length - cap);
}

function sameJsonValue(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  return JSON.stringify(a) === JSON.stringify(b);
}

export function reduceAgentEvents(
  state: AgentEventsLoadState,
  action: AgentEventsAction,
): AgentEventsLoadState {
  switch (action.type) {
    case "cleared":
      return { kind: "idle" };
    case "selected": {
      if (state.kind === "ready" && state.address === action.address) {
        return state;
      }
      const sameTarget =
        (state.kind === "loading" || state.kind === "error") &&
        state.address === action.address;
      const previous = sameTarget ? state.previous : null;
      return { kind: "loading", address: action.address, previous };
    }
    case "fetch_succeeded": {
      if (action.report.address !== action.address) {
        return state;
      }
      const prevData =
        (state.kind === "ready" ||
          state.kind === "loading" ||
          state.kind === "error") &&
        state.address === action.address
          ? state.kind === "ready"
            ? state.data
            : state.previous
          : null;
      const maxEvents = action.maxEvents ?? MAX_RETAINED_AGENT_EVENTS;
      const replaceWindow =
        action.requestedAfterKey === null ||
        action.report.cursor_reset ||
        prevData === null;
      const nextEvents = mergeFusedEvents(
        replaceWindow ? [] : prevData.events,
        action.report.events,
        maxEvents,
      );
      const nextLatestKey =
        action.report.latest_key !== null
          ? action.report.latest_key
          : replaceWindow
            ? null
            : (prevData?.latest_key ?? null);
      const cursorResetCount =
        (prevData?.cursorResetCount ?? 0) + (action.report.cursor_reset ? 1 : 0);
      const lastCursorResetAtMs = action.report.cursor_reset
        ? action.nowMs
        : (prevData?.lastCursorResetAtMs ?? null);

      if (
        state.kind === "ready" &&
        prevData !== null &&
        !action.report.cursor_reset &&
        action.report.events.length === 0 &&
        nextLatestKey === prevData.latest_key &&
        action.report.fidelity === prevData.fidelity &&
        action.report.engine === prevData.engine &&
        action.report.truncated === prevData.truncated &&
        action.nowMs - prevData.refreshedAtMs < 15_000 &&
        sameJsonValue(prevData.summary, action.report.summary) &&
        sameJsonValue(prevData.process_tree, action.report.process_tree) &&
        sameJsonValue(prevData.session, action.report.session)
      ) {
        return state;
      }

      return {
        kind: "ready",
        address: action.address,
        data: {
          address: action.address,
          engine: action.report.engine,
          session: action.report.session,
          fidelity: action.report.fidelity,
          summary: action.report.summary,
          process_tree: action.report.process_tree,
          latest_key: nextLatestKey,
          events: nextEvents,
          truncated: action.report.truncated,
          cursorResetCount,
          lastCursorResetAtMs,
          refreshedAtMs: action.nowMs,
        },
      };
    }
    case "fetch_failed": {
      if (action.error.kind === "unauthorized") {
        return {
          kind: "unauthorized",
          address: action.address,
          message: action.error.message,
        };
      }
      if (
        action.error.kind === "forbidden_external" ||
        action.error.kind === "forbidden_origin"
      ) {
        return {
          kind: "forbidden_external",
          address: action.address,
          message: action.error.message,
        };
      }
      if (action.error.kind === "not_found") {
        return {
          kind: "not_found",
          address: action.address,
          message: action.error.message,
        };
      }
      const samePrevious =
        (state.kind === "ready" ||
          state.kind === "loading" ||
          state.kind === "error") &&
        state.address === action.address
          ? state.kind === "ready"
            ? state.data
            : state.previous
          : null;
      return {
        kind: "error",
        address: action.address,
        message: action.error.message,
        previous: samePrevious,
      };
    }
  }
}

export function eventsDataFromState(
  state: AgentEventsLoadState,
  selectedAddress: string | null,
): AgentEventsViewData | null {
  if (selectedAddress === null) return null;
  if (state.kind === "ready") {
    return state.address === selectedAddress &&
      state.data.address === selectedAddress
      ? state.data
      : null;
  }
  if (state.kind === "loading" || state.kind === "error") {
    return state.address === selectedAddress &&
      state.previous !== null &&
      state.previous.address === selectedAddress
      ? state.previous
      : null;
  }
  return null;
}

// ---------------------------------------------------------------------------
// Observability Formatting & View-Model Helpers (ZH / EN)
// ---------------------------------------------------------------------------

export type ObservabilityBadgeTone =
  | "neutral"
  | "info"
  | "success"
  | "warning"
  | "danger"
  | "purple";

export function formatRssKib(kib: number): string {
  const safe = Math.max(0, Math.round(kib));
  if (safe < 1024) return `${safe} KiB`;
  const mib = safe / 1024;
  if (mib < 1024) {
    return mib >= 100 ? `${Math.round(mib)} MiB` : `${mib.toFixed(1)} MiB`;
  }
  return `${(mib / 1024).toFixed(2)} GiB`;
}

export function formatTokenCount(tokens: number): string {
  const safe = Math.max(0, Math.round(tokens));
  if (safe < 1_000) return String(safe);
  if (safe < 1_000_000) return `${(safe / 1_000).toFixed(1)}k`;
  return `${(safe / 1_000_000).toFixed(2)}M`;
}

export function formatDurationCompact(ms: number): string {
  const safe = Math.max(0, Math.round(ms));
  if (safe < 1_000) return `${safe}ms`;
  if (safe < 60_000) return `${(safe / 1_000).toFixed(1)}s`;
  const mins = Math.floor(safe / 60_000);
  const secs = Math.floor((safe % 60_000) / 1_000);
  if (safe < 3_600_000) return secs > 0 ? `${mins}m ${secs}s` : `${mins}m`;
  const hrs = Math.floor(safe / 3_600_000);
  const remMins = Math.floor((safe % 3_600_000) / 60_000);
  return remMins > 0 ? `${hrs}h ${remMins}m` : `${hrs}h`;
}

export function describeAgentHealth(
  health: AgentHealth,
  locale: Locale,
): {
  label: string;
  detail: string | null;
  tone: ObservabilityBadgeTone;
  live: boolean;
} {
  switch (health.health) {
    case "healthy":
      return {
        label: tr(locale, "健康在线", "Healthy"),
        detail: null,
        tone: "success",
        live: false,
      };
    case "busy":
      return {
        label: tr(locale, "运行中", "Busy"),
        detail: null,
        tone: "info",
        live: true,
      };
    case "blocked":
      return {
        label: tr(locale, "等待确认", "Blocked"),
        detail: health.rule,
        tone: "warning",
        live: false,
      };
    case "stalled":
      return {
        label: tr(locale, "疑似停滞", "Stalled"),
        detail: tr(
          locale,
          `静默 ${formatDurationCompact(health.silent_ms)}`,
          `silent ${formatDurationCompact(health.silent_ms)}`,
        ),
        tone: "warning",
        live: false,
      };
    case "erroring":
      return {
        label: tr(locale, "异常出错", "Erroring"),
        detail: tr(
          locale,
          `${health.error_count} 项错误`,
          `${health.error_count} errors`,
        ),
        tone: "danger",
        live: false,
      };
    case "degraded":
      return {
        label: tr(locale, "观测降级", "Degraded"),
        detail: health.reason,
        tone: "warning",
        live: false,
      };
    case "exited":
      return {
        label: tr(locale, "已退出", "Exited"),
        detail: null,
        tone: "neutral",
        live: false,
      };
  }
}

export function describeSessionDiscovery(
  status: SessionDiscoveryStatus,
  locale: Locale,
): {
  label: string;
  detail: string | null;
  tone: ObservabilityBadgeTone;
} {
  switch (status.status) {
    case "attached":
      return {
        label: tr(locale, "已挂载会话", "Session Attached"),
        detail: `${status.method} · ${status.confidence}`,
        tone: "success",
      };
    case "no_live_pid":
      return {
        label: tr(locale, "无活跃 PID", "No Live PID"),
        detail: null,
        tone: "neutral",
      };
    case "remote_terminal_unscanned":
      return {
        label: tr(locale, "远端未扫描", "Remote Unscanned"),
        detail: null,
        tone: "info",
      };
    case "no_matching_session_file":
      return {
        label: tr(locale, "未匹配会话日志", "No Session File"),
        detail: tr(
          locale,
          `已扫描 ${status.scanned_roots} 个目录`,
          `scanned ${status.scanned_roots} roots`,
        ),
        tone: "neutral",
      };
    case "read_failed":
      return {
        label: tr(locale, "读取失败", "Read Failed"),
        detail: status.reason,
        tone: "warning",
      };
  }
}

export function describeFusedEvent(
  event: FusedAgentEvent,
  locale: Locale,
): {
  badge: string;
  tone: ObservabilityBadgeTone;
  text: string;
  detail: string | null;
} {
  const k = event.kind;
  switch (k.kind) {
    case "session_meta":
      return {
        badge: "meta",
        tone: "purple",
        text: [k.engine, k.model, k.cli_version].filter(Boolean).join(" · ") || tr(locale, "会话元数据", "Session metadata"),
        detail: k.cwd,
      };
    case "turn_started":
      return {
        badge: "turn+",
        tone: "info",
        text: tr(locale, "回合开始", "Turn started"),
        detail: k.turn_id,
      };
    case "turn_completed":
      return {
        badge: k.error ? "turn!" : "turn✓",
        tone: k.error ? "danger" : "success",
        text: k.error
          ? tr(locale, `回合失败：${k.error}`, `Turn failed: ${k.error}`)
          : tr(locale, "回合完成", "Turn completed"),
        detail: k.duration_ms !== null ? formatDurationCompact(k.duration_ms) : k.turn_id,
      };
    case "user_input":
      return {
        badge: tr(locale, "输入", "input"),
        tone: "neutral",
        text: k.summary,
        detail: null,
      };
    case "thinking":
      return {
        badge: tr(locale, "思考", "think"),
        tone: "purple",
        text: k.summary,
        detail: k.duration_ms !== null ? formatDurationCompact(k.duration_ms) : null,
      };
    case "assistant_message":
      return {
        badge: tr(locale, "回复", "reply"),
        tone: "info",
        text: k.summary,
        detail: null,
      };
    case "tool_started":
      return {
        badge: `⚙ ${k.tool_name}`,
        tone: "info",
        text: k.summary || k.tool_name,
        detail: k.tool_call_id,
      };
    case "tool_completed":
      return {
        badge: `${k.ok ? "✓" : "✕"} ${k.tool_name}`,
        tone: k.ok ? "success" : "danger",
        text: k.summary || k.tool_name,
        detail: k.duration_ms !== null ? formatDurationCompact(k.duration_ms) : k.tool_call_id,
      };
    case "usage_recorded": {
      const total = `${formatTokenCount(k.input_tokens)}/${formatTokenCount(k.output_tokens)}`;
      return {
        badge: k.cumulative ? "usage∑" : "usage",
        tone: "neutral",
        text: tr(
          locale,
          `输入/输出 ${total}${k.cache_read_tokens > 0 ? ` · 缓存读 ${formatTokenCount(k.cache_read_tokens)}` : ""}`,
          `in/out ${total}${k.cache_read_tokens > 0 ? ` · cache ${formatTokenCount(k.cache_read_tokens)}` : ""}`,
        ),
        detail: k.model,
      };
    }
    case "prompt_blocked":
      return {
        badge: tr(locale, "阻塞", "blocked"),
        tone: "warning",
        text: `${k.rule} (${k.choices_count})`,
        detail: null,
      };
    case "process_state":
      return {
        badge: "proc",
        tone: k.status === "exited" ? "neutral" : "info",
        text: k.rule ? `${k.status} (${k.rule})` : k.status,
        detail: null,
      };
    case "error_observed":
      return {
        badge: tr(locale, "错误", "error"),
        tone: "danger",
        text: k.message,
        detail: null,
      };
  }
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

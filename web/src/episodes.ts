/**
 * Read-only Episode Replay & Review/Gate Sync-Point read model and resilient
 * HTTP client for `GET /api/episodes`.
 *
 * Intentionally imports no other `web/src/*` module so `node --test` executes
 * this module directly and `cap-mission-dispatch` stays dependency-pure.
 */

export const EPISODE_OUTCOMES = [
  "landed",
  "accepted",
  "rejected",
  "aborted",
  "over_budget",
  "cancelled",
  "in_flight",
] as const;

export type EpisodeOutcome = (typeof EPISODE_OUTCOMES)[number];

export type EpisodeDifficulty = "l1" | "l2" | "l3";

export interface ReviewSyncPoint {
  event_seq: number;
  at_ms: number;
  revision: number;
  accepted: boolean;
  note: string | null;
}

export interface GateSyncPoint {
  event_seq: number;
  at_ms: number;
  revision: number;
  verdict: string;
  class: string | null;
  base: string | null;
  candidate: string | null;
  note: string | null;
}

export interface EpisodeStage {
  event_seq: number;
  at_ms: number;
  kind: string;
}

export interface TaskEpisode {
  task_id: number;
  mission_id: number;
  kind: string;
  agent_id: string;
  session_id: string;
  brief: string;
  created_by: string | null;
  objective: number | null;
  scenario: string | null;
  model: string;
  difficulty: EpisodeDifficulty;
  preset_id: string | null;
  first_preset_id: string | null;
  engine: string | null;
  model_label: string | null;
  pinned: boolean;
  reissue_of: number | null;
  continues: number | null;
  reassign_of: number | null;
  state: string;
  outcome: EpisodeOutcome;
  rework: number;
  quality_rejections: number;
  reroutes: number;
  restarts: number;
  steals: number;
  deliveries: number;
  reports: number;
  last_command_id: number | null;
  last_report: string | null;
  last_report_commit: string | null;
  review_note: string | null;
  base: string | null;
  candidate: string | null;
  metric: string | null;
  last_gate_verdict: string | null;
  review_sync_points: ReviewSyncPoint[];
  gate_verdicts: GateSyncPoint[];
  terminal_by: string | null;
  terminal_reason: string | null;
  assigned_seq: number;
  assigned_at_ms: number;
  delivered_at_ms: number | null;
  closed_seq: number | null;
  closed_at_ms: number | null;
  updated_at_ms: number;
  stages: EpisodeStage[];
}

export interface EpisodeSummary {
  total: number;
  landed: number;
  accepted: number;
  rejected: number;
  cancelled: number;
  aborted: number;
  over_budget: number;
  in_flight: number;
  total_rework: number;
  total_reroutes: number;
  review_sync_points: number;
  gate_verdicts: number;
  warnings: number;
}

export interface EpisodesResponse {
  mission: number | null;
  episodes: TaskEpisode[];
  summary: EpisodeSummary;
  parity_ok: boolean;
  warnings: number;
  drift_count: number;
}

export interface EpisodeFilter {
  missionId?: number | null;
  outcome?: EpisodeOutcome | "all" | null;
  reworkOnly?: boolean;
}

export type SyncPointItem =
  | { kind: "review"; sync: ReviewSyncPoint }
  | { kind: "gate"; sync: GateSyncPoint };

export type OptionalEpisodesResult =
  | { ok: true; notModified?: false; data: EpisodesResponse; etag: string | null }
  | { ok: true; notModified: true; etag: string | null }
  | { ok: false; error: string; status: number | null };

export interface FetchOptionalEpisodesOptions {
  missionId?: number | null;
  signal?: AbortSignal;
  headers?: Record<string, string>;
  etag?: string | null;
  fetchImpl?: typeof fetch;
}

type JsonObject = Record<string, unknown>;

const OUTCOME_SET = new Set<string>(EPISODE_OUTCOMES);
const DIFFICULTY_SET = new Set<string>(["l1", "l2", "l3"]);

function asObject(value: unknown, context: string): JsonObject {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${context} must be an object`);
  }
  return value as JsonObject;
}

function asArray(value: unknown, context: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new Error(`${context} must be an array`);
  }
  return value;
}

function asString(value: unknown, context: string): string {
  if (typeof value !== "string") {
    throw new Error(`${context} must be a string`);
  }
  return value;
}

function optString(value: unknown, context: string): string | null {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string") {
    throw new Error(`${context} must be a string or null`);
  }
  return value;
}

function asNumber(value: unknown, context: string): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new Error(`${context} must be a finite number`);
  }
  return value;
}

function optNumber(value: unknown, context: string): number | null {
  if (value === undefined || value === null) return null;
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new Error(`${context} must be a finite number or null`);
  }
  return value;
}

function asBoolean(value: unknown, context: string): boolean {
  if (typeof value !== "boolean") {
    throw new Error(`${context} must be a boolean`);
  }
  return value;
}

function asOutcome(value: unknown, context: string): EpisodeOutcome {
  const raw = asString(value, context);
  if (!OUTCOME_SET.has(raw)) {
    throw new Error(`${context} has invalid EpisodeOutcome: ${JSON.stringify(raw)}`);
  }
  return raw as EpisodeOutcome;
}

function asDifficulty(value: unknown, context: string): EpisodeDifficulty {
  const raw = asString(value, context);
  if (!DIFFICULTY_SET.has(raw)) {
    throw new Error(`${context} has invalid EpisodeDifficulty: ${JSON.stringify(raw)}`);
  }
  return raw as EpisodeDifficulty;
}

function parseReviewSyncPoint(value: unknown, context: string): ReviewSyncPoint {
  const raw = asObject(value, context);
  return {
    event_seq: asNumber(raw.event_seq, `${context}.event_seq`),
    at_ms: asNumber(raw.at_ms, `${context}.at_ms`),
    revision: asNumber(raw.revision, `${context}.revision`),
    accepted: asBoolean(raw.accepted, `${context}.accepted`),
    note: optString(raw.note, `${context}.note`),
  };
}

function parseGateSyncPoint(value: unknown, context: string): GateSyncPoint {
  const raw = asObject(value, context);
  return {
    event_seq: asNumber(raw.event_seq, `${context}.event_seq`),
    at_ms: asNumber(raw.at_ms, `${context}.at_ms`),
    revision: asNumber(raw.revision, `${context}.revision`),
    verdict: asString(raw.verdict, `${context}.verdict`),
    class: optString(raw.class, `${context}.class`),
    base: optString(raw.base, `${context}.base`),
    candidate: optString(raw.candidate, `${context}.candidate`),
    note: optString(raw.note, `${context}.note`),
  };
}

function parseEpisodeStage(value: unknown, context: string): EpisodeStage {
  const raw = asObject(value, context);
  return {
    event_seq: asNumber(raw.event_seq, `${context}.event_seq`),
    at_ms: asNumber(raw.at_ms, `${context}.at_ms`),
    kind: asString(raw.kind, `${context}.kind`),
  };
}

function parseTaskEpisode(value: unknown, index: number): TaskEpisode {
  const ctx = `episodes[${index}]`;
  const raw = asObject(value, ctx);
  return {
    task_id: asNumber(raw.task_id, `${ctx}.task_id`),
    mission_id: asNumber(raw.mission_id, `${ctx}.mission_id`),
    kind: asString(raw.kind, `${ctx}.kind`),
    agent_id: asString(raw.agent_id, `${ctx}.agent_id`),
    session_id: asString(raw.session_id, `${ctx}.session_id`),
    brief: asString(raw.brief, `${ctx}.brief`),
    created_by: optString(raw.created_by, `${ctx}.created_by`),
    objective: optNumber(raw.objective, `${ctx}.objective`),
    scenario: optString(raw.scenario, `${ctx}.scenario`),
    model: asString(raw.model, `${ctx}.model`),
    difficulty: asDifficulty(raw.difficulty, `${ctx}.difficulty`),
    preset_id: optString(raw.preset_id, `${ctx}.preset_id`),
    first_preset_id: optString(raw.first_preset_id, `${ctx}.first_preset_id`),
    engine: optString(raw.engine, `${ctx}.engine`),
    model_label: optString(raw.model_label, `${ctx}.model_label`),
    pinned: raw.pinned === undefined ? false : asBoolean(raw.pinned, `${ctx}.pinned`),
    reissue_of: optNumber(raw.reissue_of, `${ctx}.reissue_of`),
    continues: optNumber(raw.continues, `${ctx}.continues`),
    reassign_of: optNumber(raw.reassign_of, `${ctx}.reassign_of`),
    state: asString(raw.state, `${ctx}.state`),
    outcome: asOutcome(raw.outcome, `${ctx}.outcome`),
    rework: asNumber(raw.rework, `${ctx}.rework`),
    quality_rejections: asNumber(raw.quality_rejections, `${ctx}.quality_rejections`),
    reroutes: asNumber(raw.reroutes, `${ctx}.reroutes`),
    restarts: asNumber(raw.restarts, `${ctx}.restarts`),
    steals: asNumber(raw.steals, `${ctx}.steals`),
    deliveries: asNumber(raw.deliveries, `${ctx}.deliveries`),
    reports: asNumber(raw.reports, `${ctx}.reports`),
    last_command_id: optNumber(raw.last_command_id, `${ctx}.last_command_id`),
    last_report: optString(raw.last_report, `${ctx}.last_report`),
    last_report_commit: optString(raw.last_report_commit, `${ctx}.last_report_commit`),
    review_note: optString(raw.review_note, `${ctx}.review_note`),
    base: optString(raw.base, `${ctx}.base`),
    candidate: optString(raw.candidate, `${ctx}.candidate`),
    metric: optString(raw.metric, `${ctx}.metric`),
    last_gate_verdict: optString(raw.last_gate_verdict, `${ctx}.last_gate_verdict`),
    review_sync_points: asArray(raw.review_sync_points, `${ctx}.review_sync_points`).map(
      (entry, idx) => parseReviewSyncPoint(entry, `${ctx}.review_sync_points[${idx}]`),
    ),
    gate_verdicts: asArray(raw.gate_verdicts, `${ctx}.gate_verdicts`).map((entry, idx) =>
      parseGateSyncPoint(entry, `${ctx}.gate_verdicts[${idx}]`),
    ),
    terminal_by: optString(raw.terminal_by, `${ctx}.terminal_by`),
    terminal_reason: optString(raw.terminal_reason, `${ctx}.terminal_reason`),
    assigned_seq: asNumber(raw.assigned_seq, `${ctx}.assigned_seq`),
    assigned_at_ms: asNumber(raw.assigned_at_ms, `${ctx}.assigned_at_ms`),
    delivered_at_ms: optNumber(raw.delivered_at_ms, `${ctx}.delivered_at_ms`),
    closed_seq: optNumber(raw.closed_seq, `${ctx}.closed_seq`),
    closed_at_ms: optNumber(raw.closed_at_ms, `${ctx}.closed_at_ms`),
    updated_at_ms: asNumber(raw.updated_at_ms, `${ctx}.updated_at_ms`),
    stages: asArray(raw.stages, `${ctx}.stages`).map((entry, idx) =>
      parseEpisodeStage(entry, `${ctx}.stages[${idx}]`),
    ),
  };
}

function parseEpisodeSummary(value: unknown): EpisodeSummary {
  const raw = asObject(value, "summary");
  return {
    total: asNumber(raw.total, "summary.total"),
    landed: asNumber(raw.landed, "summary.landed"),
    accepted: asNumber(raw.accepted, "summary.accepted"),
    rejected: asNumber(raw.rejected, "summary.rejected"),
    cancelled: asNumber(raw.cancelled, "summary.cancelled"),
    aborted: asNumber(raw.aborted, "summary.aborted"),
    over_budget: asNumber(raw.over_budget, "summary.over_budget"),
    in_flight: asNumber(raw.in_flight, "summary.in_flight"),
    total_rework: asNumber(raw.total_rework, "summary.total_rework"),
    total_reroutes: asNumber(raw.total_reroutes, "summary.total_reroutes"),
    review_sync_points: asNumber(raw.review_sync_points, "summary.review_sync_points"),
    gate_verdicts: asNumber(raw.gate_verdicts, "summary.gate_verdicts"),
    warnings: raw.warnings === undefined ? 0 : asNumber(raw.warnings, "summary.warnings"),
  };
}

/** Build the read-only URL for `/api/episodes`. */
export function episodesUrl(missionId?: number | null): string {
  if (typeof missionId === "number" && Number.isInteger(missionId)) {
    return `/api/episodes?mission=${encodeURIComponent(String(missionId))}`;
  }
  return "/api/episodes";
}

/** Strict runtime validator for `GET /api/episodes` JSON payloads. */
export function parseEpisodesResponse(value: unknown): EpisodesResponse {
  const raw = asObject(value, "episodes response");
  const summary = parseEpisodeSummary(raw.summary);
  const warnings =
    raw.warnings === undefined ? summary.warnings : asNumber(raw.warnings, "warnings");
  return {
    mission: optNumber(raw.mission, "mission"),
    episodes: asArray(raw.episodes, "episodes").map(parseTaskEpisode),
    summary,
    parity_ok: asBoolean(raw.parity_ok, "parity_ok"),
    warnings,
    drift_count:
      raw.drift_count === undefined ? 0 : asNumber(raw.drift_count, "drift_count"),
  };
}

/**
 * Resilient read-only fetch for `GET /api/episodes`.
 * Never throws; returns structured `{ ok: false, error, status }` when degraded
 * and preserves `304 Not Modified` responses when `etag` is supplied.
 */
export async function fetchOptionalEpisodes(
  options: FetchOptionalEpisodesOptions = {},
): Promise<OptionalEpisodesResult> {
  const fetchFn = options.fetchImpl ?? globalThis.fetch;
  if (typeof fetchFn !== "function") {
    return { ok: false, error: "fetch API unavailable", status: null };
  }
  const headers: Record<string, string> = {
    Accept: "application/json",
    ...(options.headers ?? {}),
  };
  if (options.etag) {
    headers["If-None-Match"] = options.etag;
  }

  let response: Response;
  try {
    response = await fetchFn(episodesUrl(options.missionId), {
      method: "GET",
      signal: options.signal,
      cache: "no-cache",
      headers,
    });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ok: false, error: message, status: null };
  }

  const nextEtag = response.headers.get("ETag") ?? options.etag ?? null;
  if (response.status === 304) {
    return { ok: true, notModified: true, etag: nextEtag };
  }

  if (!response.ok) {
    let message = `HTTP ${response.status}`;
    try {
      const text = await response.text();
      if (text) {
        try {
          const parsed = JSON.parse(text) as { error?: unknown };
          if (typeof parsed.error === "string" && parsed.error.length > 0) {
            message = parsed.error;
          } else {
            message = text;
          }
        } catch {
          message = text;
        }
      }
    } catch {
      // keep status text fallback
    }
    return { ok: false, error: message, status: response.status };
  }

  try {
    const payload: unknown = await response.json();
    return {
      ok: true,
      data: parseEpisodesResponse(payload),
      etag: nextEtag,
    };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ok: false, error: `invalid episodes payload: ${message}`, status: response.status };
  }
}

/** Pure filter helper over replayed episodes (by mission, outcome, rework > 0). */
export function filterEpisodes(
  episodes: readonly TaskEpisode[],
  filter: EpisodeFilter,
): TaskEpisode[] {
  return episodes.filter((ep) => {
    if (
      typeof filter.missionId === "number" &&
      Number.isFinite(filter.missionId) &&
      ep.mission_id !== filter.missionId
    ) {
      return false;
    }
    if (filter.outcome && filter.outcome !== "all" && ep.outcome !== filter.outcome) {
      return false;
    }
    if (filter.reworkOnly && ep.rework <= 0) {
      return false;
    }
    return true;
  });
}

/** Merge review and gate sync points for one episode in chronological sequence order. */
export function episodeSyncPoints(episode: TaskEpisode): SyncPointItem[] {
  const merged: SyncPointItem[] = [
    ...episode.review_sync_points.map((sync): SyncPointItem => ({ kind: "review", sync })),
    ...episode.gate_verdicts.map((sync): SyncPointItem => ({ kind: "gate", sync })),
  ];
  merged.sort((a, b) => {
    if (a.sync.event_seq !== b.sync.event_seq) {
      return a.sync.event_seq - b.sync.event_seq;
    }
    if (a.sync.at_ms !== b.sync.at_ms) {
      return a.sync.at_ms - b.sync.at_ms;
    }
    return a.kind === b.kind ? 0 : a.kind === "review" ? -1 : 1;
  });
  return merged;
}

/** Unique sorted mission IDs represented in `episodes`. */
export function availableMissionIds(episodes: readonly TaskEpisode[]): number[] {
  const ids = new Set<number>();
  for (const ep of episodes) {
    ids.add(ep.mission_id);
  }
  return Array.from(ids).sort((a, b) => a - b);
}

/** Badge tone helper matching `web/src/components/ui/badge.tsx` (`BadgeTone`). */
export function outcomeTone(
  outcome: EpisodeOutcome,
): "success" | "info" | "warning" | "danger" | "neutral" {
  switch (outcome) {
    case "landed":
    case "accepted":
      return "success";
    case "in_flight":
      return "info";
    case "over_budget":
      return "warning";
    case "rejected":
    case "aborted":
      return "danger";
    case "cancelled":
      return "neutral";
  }
}

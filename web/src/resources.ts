// What the host has now, as `/api/fleet`'s health reports it beside the
// disk: memory (MemAvailable and MemTotal from /proc/meminfo, read when the
// request was answered) and each gated mission's waits for gate room (its
// `gate_room_*` columns: the run of `no_room` gate outcomes of one task).
// A value the server could not read stays null: nothing here guesses one.
//
// Pure (imports of other pure modules only), tested in web/test/resources.test.ts.

import { localDateTime } from "./format.ts";
import { missionName } from "./ordering.ts";

/** The line under which memory is short, when the server names none: the
 * supervisor's old monitor watched MemAvailable under 4 GB. */
export const MEMORY_WARN_BYTES = 4 * 1024 ** 3;

export interface Memory {
  /** MemAvailable; null when the server could not read it. */
  available_bytes: number | null;
  /** MemTotal; null when the server could not read it. */
  total_bytes: number | null;
  /** Under this available memory is short. */
  warn_bytes: number;
}

/** One gated mission's waits for gate room. */
export interface GateRoom {
  mission: number;
  name: string;
  /** The task whose gate waits for room; null when the gate is not waiting. */
  task: number | null;
  /** When the run began and when its latest `no_room` was recorded. */
  since_ms: number | null;
  last_ms: number | null;
  /** `no_room` outcomes in the run. */
  waits: number;
  /** Whether the run was escalated to the supervisor (over two hours). */
  escalated: boolean;
}

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as JsonObject)
    : null;
}

function optionalNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/** The health's `memory`; null when the server reports none (an older one). */
export function normalizeMemory(value: unknown): Memory | null {
  const raw = object(value);
  if (!raw) return null;
  const warn = optionalNumber(raw.warn_bytes);
  return {
    available_bytes: optionalNumber(raw.available_bytes),
    total_bytes: optionalNumber(raw.total_bytes),
    warn_bytes: warn !== null && warn > 0 ? warn : MEMORY_WARN_BYTES,
  };
}

/** The health's `gate_room`, one row per gated mission; null when the
 * server reports none (an older one). A row without a mission id is
 * dropped. */
export function normalizeGateRoom(value: unknown): GateRoom[] | null {
  if (!Array.isArray(value)) return null;
  return value.flatMap((entry) => {
    const raw = object(entry);
    const mission = raw ? optionalNumber(raw.mission) : null;
    if (!raw || mission === null) return [];
    return [
      {
        mission,
        name: missionName({ id: mission, text: typeof raw.text === "string" ? raw.text : null }),
        task: optionalNumber(raw.task),
        since_ms: optionalNumber(raw.since_ms),
        last_ms: optionalNumber(raw.last_ms),
        waits: optionalNumber(raw.waits) ?? 0,
        escalated: raw.escalated === true,
      },
    ];
  });
}

/** The gates that wait for room now, the longest first. */
export function waitingGates(rows: readonly GateRoom[]): GateRoom[] {
  return rows
    .filter((row) => row.task !== null)
    .sort(
      (left, right) =>
        (left.since_ms ?? Infinity) - (right.since_ms ?? Infinity) || left.mission - right.mission,
    );
}

export type MemoryTone = "unread" | "short" | "fine";

/** Memory as the strip draws it: its tone, and a scale on which the bar's
 * fill and the warning line sit (the total when known; else twice the
 * warning, or a little past what is available, whichever is more). */
export function memoryBar(memory: Memory): { tone: MemoryTone; scale: number } {
  const available = memory.available_bytes;
  const tone: MemoryTone =
    available === null ? "unread" : available < memory.warn_bytes ? "short" : "fine";
  const scale =
    memory.total_bytes !== null && memory.total_bytes > 0
      ? memory.total_bytes
      : Math.max(memory.warn_bytes * 2, (available ?? 0) * 1.15, 1);
  return { tone, scale };
}

// What uses the disk, as `/api/fleet`'s `health.storage` reports it
// (docs/storage.md, "What uses the disk"): the transcript archive (kept
// whole, so the one steady grower), Claude Code's own transcripts, the
// workspaces, the database (with its event log's bytes) and the test
// databases the server is given, with their schemas (objective 074); what the archive grows by a day, what
// every grower that persists (the archive, Claude Code's transcripts, the
// database) grows by, and how long the free space lasts at their sum
// (objective 085). The sizes are read at most every
// 10 minutes (`read_at_ms`); the free space at each request.

/** A directory's regular files: their sizes summed, and how many. */
export interface StoragePlace {
  path: string;
  bytes: number;
  /** Null where the server does not count them (the workspaces). */
  files: number | null;
}

/** Disk usage for one workspace directory (PR #184). */
export interface WorkspaceDiskUsage {
  name: string;
  total_bytes: number;
  repo_bytes: number;
  git_bytes: number;
  build_cache_bytes: number;
  files: number;
  truncated: boolean;
}

/** Aggregate and bounded top-32 workspace disk breakdown (PR #184). */
export interface WorkspaceBreakdown {
  workspace_count: number;
  total_bytes: number;
  repo_bytes: number;
  git_bytes: number;
  build_cache_bytes: number;
  files: number;
  truncated: boolean;
  workspaces: WorkspaceDiskUsage[];
}

/** Opt-in shared host build cache usage, or residual `.caches` when disabled. */
export interface SharedBuildCache {
  path: string;
  bytes: number;
  files: number;
  enabled: boolean;
}

export interface Storage {
  /** Free bytes on the workspace root's file system; null when unread. */
  free_bytes: number | null;
  /** When the sizes were read. */
  read_at_ms: number | null;
  /** `<root>/.transcripts`; null without a workspace root. */
  archive: StoragePlace | null;
  /** Claude Code's projects directory, with the count of the newest sample
   * at least an hour old (`before`), null without one. */
  projects: (StoragePlace & { before: number | null }) | null;
  /** Everything under the root outside the archive and shared cache; null without a root. */
  workspaces: StoragePlace | null;
  /** Bounded workspace disk breakdown by bucket (PR #184); null when omitted. */
  workspace_breakdown: WorkspaceBreakdown | null;
  /** Opt-in shared build cache usage (PR #184); null when off and absent. */
  shared_build_cache: SharedBuildCache | null;
  /** The live database: its size, its event log's bytes (what the data
   * justifies; null from an older server) and its largest tables. */
  database: {
    bytes: number | null;
    log_bytes: number | null;
    tables: Array<{ name: string; bytes: number }>;
  } | null;
  /** The test databases the server is given (objective 074), the gates'
   * first; empty when none is (or from an older server). */
  test_databases: TestDatabase[];
  /** Bytes appended to the archive a day; null without a sample a day old. */
  growth_bytes_per_day: number | null;
  /** Bytes a day of each grower that persists, each null without a sample
   * a day old carrying its size (objective 085); null from an older server. */
  growth_parts: {
    archive: number | null;
    projects: number | null;
    database: number | null;
    /** The session backups' reflink-aware growth, from their status file's
     * `estimated_bytes` (objective 097); null when left out or without a
     * sample a day old. */
    session_backups: number | null;
  } | null;
  /** When each part of growth_parts is first measured: the oldest sample of
   * the last 7 days carrying its field plus a day; null without one (the
   * archive's also without a workspace root). Objective 098; null from an
   * older server. */
  growth_from_parts: Record<string, number | null> | null;
  /** The parts left out of the sum because their source cannot be read now
   * (objective 097): ["session_backups"] without a readable status file,
   * [] when every part is counted; null from an older server. */
  growth_excluded: string[] | null;
  /** The counted parts' sum; null while any counted part is null. */
  persistent_growth_bytes_per_day: number | null;
  /** The free space over the persistent growth, rounded down; null without
   * one above 0. */
  days_left: number | null;
  /** When the growth is first measured: the oldest sample of the last 7
   * days plus a day; null without a sample (or from an older server). */
  growth_from_ms: number | null;
  /** When the persistent growth (so days left) is first measured: the
   * latest of its parts' oldest samples plus a day (objective 091); null
   * while a part has no sample (or from an older server). */
  persistent_from_ms: number | null;
}

/** A test database: the gates' (`test`) or the engines' (`fleet_test`).
 * Its name, size and schemas are null when the server could not read it. */
export interface TestDatabase {
  role: string;
  name: string | null;
  bytes: number | null;
  /** Its schemas other than pg_*, information_schema and public. */
  schemas: number | null;
}

function testDatabases(value: unknown): TestDatabase[] {
  return (Array.isArray(value) ? value : []).flatMap((entry) => {
    const raw = object(entry);
    if (!raw || typeof raw.role !== "string") return [];
    return [
      {
        role: raw.role,
        name: typeof raw.name === "string" ? raw.name : null,
        bytes: optionalNumber(raw.bytes),
        schemas: optionalNumber(raw.schemas),
      },
    ];
  });
}

function place(value: unknown): StoragePlace | null {
  const raw = object(value);
  const bytes = raw ? optionalNumber(raw.bytes) : null;
  if (!raw || bytes === null) return null;
  return { path: typeof raw.path === "string" ? raw.path : "", bytes, files: optionalNumber(raw.files) };
}

function nonNegativeNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
}

const WORKSPACE_BREAKDOWN_MAX_ENTRIES = 32;

/** Bounded per-workspace disk breakdown (PR #184); null when omitted or unusable. */
export function normalizeWorkspaceBreakdown(value: unknown): WorkspaceBreakdown | null {
  const raw = object(value);
  if (!raw) return null;
  const hasCount = nonNegativeNumber(raw.workspace_count) !== null;
  const hasTotal = nonNegativeNumber(raw.total_bytes) !== null;
  const hasWorkspaces = Array.isArray(raw.workspaces);
  if (!hasCount && !hasTotal && !hasWorkspaces) return null;

  const normalizedEntries = (hasWorkspaces ? (raw.workspaces as unknown[]) : [])
    .flatMap((entry): WorkspaceDiskUsage[] => {
      const row = object(entry);
      const name = typeof row?.name === "string" ? row.name.trim() : "";
      if (!row || name === "") return [];
      const repo_bytes = nonNegativeNumber(row.repo_bytes) ?? 0;
      const git_bytes = nonNegativeNumber(row.git_bytes) ?? 0;
      const build_cache_bytes = nonNegativeNumber(row.build_cache_bytes) ?? 0;
      const total_bytes =
        nonNegativeNumber(row.total_bytes) ?? repo_bytes + git_bytes + build_cache_bytes;
      return [
        {
          name,
          total_bytes,
          repo_bytes,
          git_bytes,
          build_cache_bytes,
          files: nonNegativeNumber(row.files) ?? 0,
          truncated: row.truncated === true,
        },
      ];
    })
    .sort((left, right) => right.total_bytes - left.total_bytes || left.name.localeCompare(right.name))
    .slice(0, WORKSPACE_BREAKDOWN_MAX_ENTRIES);

  const summedTotal = normalizedEntries.reduce((sum, entry) => sum + entry.total_bytes, 0);
  const summedRepo = normalizedEntries.reduce((sum, entry) => sum + entry.repo_bytes, 0);
  const summedGit = normalizedEntries.reduce((sum, entry) => sum + entry.git_bytes, 0);
  const summedBuildCache = normalizedEntries.reduce((sum, entry) => sum + entry.build_cache_bytes, 0);
  const summedFiles = normalizedEntries.reduce((sum, entry) => sum + entry.files, 0);

  const repo_bytes = nonNegativeNumber(raw.repo_bytes) ?? summedRepo;
  const git_bytes = nonNegativeNumber(raw.git_bytes) ?? summedGit;
  const build_cache_bytes = nonNegativeNumber(raw.build_cache_bytes) ?? summedBuildCache;
  const total_bytes =
    nonNegativeNumber(raw.total_bytes) ??
    (hasWorkspaces ? Math.max(summedTotal, repo_bytes + git_bytes + build_cache_bytes) : repo_bytes + git_bytes + build_cache_bytes);

  return {
    workspace_count: nonNegativeNumber(raw.workspace_count) ?? normalizedEntries.length,
    total_bytes,
    repo_bytes,
    git_bytes,
    build_cache_bytes,
    files: nonNegativeNumber(raw.files) ?? summedFiles,
    truncated: raw.truncated === true,
    workspaces: normalizedEntries,
  };
}

/** Shared host build cache usage (PR #184); null when disabled and absent or omitted. */
export function normalizeSharedBuildCache(value: unknown): SharedBuildCache | null {
  const raw = object(value);
  if (!raw) return null;
  const hasPath = typeof raw.path === "string";
  const hasBytes = nonNegativeNumber(raw.bytes) !== null;
  const hasEnabled = typeof raw.enabled === "boolean";
  if (!hasPath && !hasBytes && !hasEnabled) return null;
  return {
    path: typeof raw.path === "string" ? raw.path.trim() : "",
    bytes: nonNegativeNumber(raw.bytes) ?? 0,
    files: nonNegativeNumber(raw.files) ?? 0,
    enabled: raw.enabled === true,
  };
}

/** `growth_from_parts`, each part a number or null; null without the object. */
function fromParts(value: unknown): Record<string, number | null> | null {
  const parts = object(value);
  if (!parts) return null;
  return Object.fromEntries(Object.entries(parts).map(([part, at]) => [part, optionalNumber(at)]));
}

/** The health's `storage`; null when the server reports none (an older one). */
export function normalizeStorage(value: unknown): Storage | null {
  const raw = object(value);
  if (!raw) return null;
  const projects = place(raw.projects);
  const database = object(raw.database);
  const parts = object(raw.growth_parts);
  return {
    free_bytes: optionalNumber(raw.free_bytes),
    read_at_ms: optionalNumber(raw.read_at_ms),
    archive: place(raw.archive),
    projects: projects && { ...projects, before: optionalNumber(object(raw.projects)?.before) },
    workspaces: place(raw.workspaces),
    workspace_breakdown: normalizeWorkspaceBreakdown(raw.workspace_breakdown),
    shared_build_cache: normalizeSharedBuildCache(raw.shared_build_cache),
    database: database && {
      bytes: optionalNumber(database.bytes),
      log_bytes: optionalNumber(database.log_bytes),
      tables: (Array.isArray(database.tables) ? database.tables : []).flatMap((entry) => {
        const table = object(entry);
        const bytes = table ? optionalNumber(table.bytes) : null;
        return table && typeof table.name === "string" && bytes !== null ? [{ name: table.name, bytes }] : [];
      }),
    },
    test_databases: testDatabases(raw.test_databases),
    growth_bytes_per_day: optionalNumber(raw.growth_bytes_per_day),
    growth_parts: parts && {
      archive: optionalNumber(parts.archive),
      projects: optionalNumber(parts.projects),
      database: optionalNumber(parts.database),
      session_backups: optionalNumber(parts.session_backups),
    },
    growth_from_parts: fromParts(raw.growth_from_parts),
    growth_excluded: Array.isArray(raw.growth_excluded)
      ? raw.growth_excluded.filter((part): part is string => typeof part === "string")
      : null,
    persistent_growth_bytes_per_day: optionalNumber(raw.persistent_growth_bytes_per_day),
    days_left: optionalNumber(raw.days_left),
    growth_from_ms: optionalNumber(raw.growth_from_ms),
    persistent_from_ms: optionalNumber(raw.persistent_from_ms),
  };
}

const MB = 1024 ** 2;

function formatCount(n: number): string {
  return n.toLocaleString("en-US");
}

/** "12.0 MB" under 1024 MB, "3.0 GB" from there (1 MB is 1024 * 1024
 * bytes); "—" when unread. */
export function storageSize(bytes: number | null | undefined): string {
  if (bytes == null || !Number.isFinite(bytes)) return "—";
  const mb = bytes / MB;
  return mb < 1024 ? `${mb.toFixed(1)} MB` : `${(mb / 1024).toFixed(1)} GB`;
}

export interface DiskBucketShares {
  repo_pct: number;
  git_pct: number;
  rebuildable_pct: number;
}

export interface WorkspaceUsageRow {
  name: string;
  total: string;
  repo: string;
  git: string;
  rebuildable: string;
  files: number;
  truncated: boolean;
  shares: DiskBucketShares;
  summary: string;
}

export interface WorkspaceBreakdownView {
  workspace_count: number;
  shown_count: number;
  hidden_count: number;
  total: string;
  repo: string;
  git: string;
  rebuildable: string;
  files: number;
  truncated: boolean;
  shares: DiskBucketShares;
  workspaces: WorkspaceUsageRow[];
}

export interface SharedBuildCacheView {
  status: "enabled" | "disabled" | "off";
  badge: string;
  size: string | null;
  files: number | null;
  path: string | null;
  note: string;
}

/** Compute bounded non-negative percentage shares (`<= 100` total) for workspace buckets. */
export function bucketShares(
  total: number,
  repo: number,
  git: number,
  rebuildable: number,
): DiskBucketShares {
  const safeRepo = Number.isFinite(repo) && repo > 0 ? repo : 0;
  const safeGit = Number.isFinite(git) && git > 0 ? git : 0;
  const safeRebuild = Number.isFinite(rebuildable) && rebuildable > 0 ? rebuildable : 0;
  const partsSum = safeRepo + safeGit + safeRebuild;
  const safeTotal = Number.isFinite(total) && total > 0 ? total : 0;
  const denom = Math.max(safeTotal, partsSum);
  if (denom <= 0 || partsSum <= 0) {
    return { repo_pct: 0, git_pct: 0, rebuildable_pct: 0 };
  }
  const raw = [safeRepo, safeGit, safeRebuild].map((part) => (part / denom) * 100);
  const floored = raw.map((pct) => Math.floor(pct));
  const target = Math.min(100, Math.round((partsSum / denom) * 100));
  let remainder = Math.max(0, target - (floored[0] + floored[1] + floored[2]));
  const order = [0, 1, 2].sort((a, b) => raw[b] - floored[b] - (raw[a] - floored[a]) || a - b);
  for (const index of order) {
    if (remainder <= 0) break;
    floored[index] += 1;
    remainder -= 1;
  }
  return { repo_pct: floored[0], git_pct: floored[1], rebuildable_pct: floored[2] };
}

function formatBucketSummary(
  repo: string,
  git: string,
  rebuildable: string,
  shares: DiskBucketShares,
): string {
  return `repo ${repo} (${shares.repo_pct}%) · .git ${git} (${shares.git_pct}%) · rebuildable ${rebuildable} (${shares.rebuildable_pct}%)`;
}

/** Compact view model for `health.storage.workspace_breakdown`; accepts either
 * a `Storage` object (raw or normalized) or a `WorkspaceBreakdown` directly. */
export function workspaceBreakdownView(value: unknown): WorkspaceBreakdownView | null {
  const obj = object(value);
  if (!obj) return null;
  const rawBreakdown =
    "workspace_breakdown" in obj || "workspaces" in obj && object(obj.workspaces) !== null || "free_bytes" in obj
      ? obj.workspace_breakdown
      : value;
  const breakdown = normalizeWorkspaceBreakdown(rawBreakdown);
  if (!breakdown) return null;

  const shares = bucketShares(
    breakdown.total_bytes,
    breakdown.repo_bytes,
    breakdown.git_bytes,
    breakdown.build_cache_bytes,
  );
  const workspaces: WorkspaceUsageRow[] = breakdown.workspaces.map((ws) => {
    const wsShares = bucketShares(
      ws.total_bytes,
      ws.repo_bytes,
      ws.git_bytes,
      ws.build_cache_bytes,
    );
    const repo = storageSize(ws.repo_bytes);
    const git = storageSize(ws.git_bytes);
    const rebuildable = storageSize(ws.build_cache_bytes);
    return {
      name: ws.name,
      total: storageSize(ws.total_bytes),
      repo,
      git,
      rebuildable,
      files: ws.files,
      truncated: ws.truncated,
      shares: wsShares,
      summary: formatBucketSummary(repo, git, rebuildable, wsShares),
    };
  });

  return {
    workspace_count: breakdown.workspace_count,
    shown_count: workspaces.length,
    hidden_count: Math.max(0, breakdown.workspace_count - workspaces.length),
    total: storageSize(breakdown.total_bytes),
    repo: storageSize(breakdown.repo_bytes),
    git: storageSize(breakdown.git_bytes),
    rebuildable: storageSize(breakdown.build_cache_bytes),
    files: breakdown.files,
    truncated: breakdown.truncated,
    shares,
    workspaces,
  };
}

/** Compact view model for `health.storage.shared_build_cache`; accepts either a
 * `Storage` object (raw or normalized) or a `SharedBuildCache | null` directly. */
export function sharedBuildCacheView(value: unknown): SharedBuildCacheView {
  const obj = object(value);
  const rawCache =
    obj &&
    ("shared_build_cache" in obj ||
      "workspace_breakdown" in obj ||
      "workspaces" in obj ||
      "archive" in obj ||
      "free_bytes" in obj)
      ? obj.shared_build_cache
      : value;
  const cache = normalizeSharedBuildCache(rawCache);
  if (!cache) {
    return {
      status: "off",
      badge: "off",
      size: null,
      files: null,
      path: null,
      note: "Disabled or not reported",
    };
  }
  const filesLabel = `${formatCount(cache.files)} ${cache.files === 1 ? "file" : "files"}`;
  if (!cache.enabled) {
    return {
      status: "disabled",
      badge: "disabled",
      size: storageSize(cache.bytes),
      files: cache.files,
      path: cache.path || null,
      note: `residual cache · ${filesLabel}`,
    };
  }
  return {
    status: "enabled",
    badge: "enabled",
    size: storageSize(cache.bytes),
    files: cache.files,
    path: cache.path || null,
    note: filesLabel,
  };
}

/** The storage rows' names of the test databases, by role. */
const TEST_DATABASE_NAMES: Record<string, string> = {
  test: "Gate test database",
  fleet_test: "Fleet test database",
};

/** The log's size is said from what a size shows as 0.1 MB: "log 0.0 MB"
 * would say nothing (a fresh install's log is a few kilobytes). */
export const LOG_SHOWN_BYTES = 0.05 * MB;

/** The test databases the card shows: all of them once one holds a schema
 * (what tests leave behind, objective 074) or could not be read, so the
 * gates' and the fleet's can be compared; none while every one is a fresh
 * database with no schema (its few MB are the template's catalog, nothing
 * that uses the disk). */
export function shownTestDatabases(databases: readonly TestDatabase[]): TestDatabase[] {
  return databases.some((database) => database.schemas === null || database.schemas > 0)
    ? [...databases]
    : [];
}

export interface StorageRow {
  name: string;
  size: string;
  note: string;
  /** A grower's row (the archive, Claude Code's transcripts, the database,
   * objective 098): the local date and time its own growth is first
   * measured, while that growth is null and the moment is after the sizes'
   * read; else null (and always null for a row with no growth part). */
  measured_from: string | null;
}

export interface StorageView {
  rows: StorageRow[];
  /** "about <days> days at the growth of the archive, Claude Code's
   * transcripts, the database and the session backups" when the session
   * backups are counted (objective 097), "... transcripts and the database"
   * when they are not (or from an older server); null without days left. */
  lasts: string | null;
  /** "session backups are not counted" while they are left out of the sum
   * (their status file does not read); null otherwise. */
  lasts_note: string | null;
  /** The local date and time the growth is first measured, while it is
   * not yet and that moment is still ahead of the sizes' read; else null. */
  measured_from: string | null;
  /** The local date and time how long the disk lasts is first measured,
   * while it is not yet and that moment is still ahead of the sizes' read;
   * else null (objective 091). */
  lasts_from: string | null;
}

/** The Storage card's words: what uses the disk, one row each, and how
 * long the free space lasts at the growth of what persists. Takes the server's
 * `health.storage` as it is or normalized. */
export function storageView(value: unknown): StorageView {
  const storage = normalizeStorage(value);
  const growth = storage?.growth_bytes_per_day ?? null;
  const from = storage?.growth_from_ms ?? null;
  const readAt = storage?.read_at_ms ?? null;
  // A moment already past promises nothing.
  const measuredFrom =
    growth === null && from !== null && readAt !== null && from > readAt ? localDateTime(from) : null;
  const projects = storage?.projects ?? null;
  const files = projects?.files ?? null;
  const before = projects?.before ?? null;
  const largest = storage?.database?.tables[0] ?? null;
  const logBytes = storage?.database?.log_bytes ?? null;
  const tests = shownTestDatabases(storage?.test_databases ?? []);
  const count = (n: number) => n.toLocaleString("en-US");
  // Each grower's own date (objective 098): its moment while its growth is
  // null and the moment is still ahead of the sizes' read.
  const lineFrom = (part: "archive" | "projects" | "database"): string | null => {
    const at = storage?.growth_from_parts?.[part] ?? null;
    const grows = storage?.growth_parts?.[part] ?? null;
    return grows === null && at !== null && readAt !== null && at > readAt ? localDateTime(at) : null;
  };
  const rows: StorageRow[] = [
    {
      name: "Transcript archive",
      size: storageSize(storage?.archive?.bytes),
      note:
        growth !== null
          ? `grows about ${storageSize(growth)} a day`
          : measuredFrom !== null
            ? `measured from ${measuredFrom}`
            : "not measured yet",
      measured_from: lineFrom("archive"),
    },
    {
      name: "Claude Code transcripts",
      size: storageSize(projects?.bytes),
      note:
        files === null
          ? ""
          : `${count(files)} files${before !== null && before > files ? `, down from ${count(before)}` : ""}`,
      measured_from: lineFrom("projects"),
    },
    { name: "Workspaces", size: storageSize(storage?.workspaces?.bytes), note: "", measured_from: null },
    {
      name: "Database",
      size: storageSize(storage?.database?.bytes),
      note: [
        largest ? `largest: ${largest.name} ${storageSize(largest.bytes)}` : null,
        logBytes !== null && logBytes >= LOG_SHOWN_BYTES ? `log ${storageSize(logBytes)}` : null,
      ]
        .filter((part) => part !== null)
        .join(", "),
      measured_from: lineFrom("database"),
    },
    ...tests.map((database) => ({
      name: TEST_DATABASE_NAMES[database.role] ?? `Test database (${database.role})`,
      size: storageSize(database.bytes),
      note: database.schemas === null ? "unreadable" : `${count(database.schemas)} ${database.schemas === 1 ? "schema" : "schemas"}`,
      measured_from: null,
    })),
  ];
  const days = storage?.days_left ?? null;
  // The session backups are counted when the server lists what it left out
  // and they are not among it; an older server lists nothing (085's words).
  const left = storage?.growth_excluded ?? null;
  const excluded = left?.includes("session_backups") ?? false;
  const backups = left !== null && !excluded;
  const lastsAt = storage?.persistent_from_ms ?? null;
  const lastsFrom =
    days === null && lastsAt !== null && readAt !== null && lastsAt > readAt ? localDateTime(lastsAt) : null;
  return {
    rows,
    lasts:
      days === null
        ? null
        : backups
          ? `about ${count(days)} days at the growth of the archive, Claude Code's transcripts, the database and the session backups`
          : `about ${count(days)} days at the growth of the archive, Claude Code's transcripts and the database`,
    lasts_note: excluded ? "session backups are not counted" : null,
    measured_from: measuredFrom,
    lasts_from: lastsFrom,
  };
}

// The newest off-host backups, as `/api/fleet`'s `health.backups` reports
// them (objective 82): the database's hourly copy (`backup.dumped`) and the
// daily bundle of transcripts and sources (`backup.bundled`). The alert step
// calls a dump older than 2 h, or a bundle older than 26 h, stale.

/** One upload: when it was recorded, its first key and its bytes. */
export interface BackupRecord {
  at_ms: number;
  key: string | null;
  bytes: number | null;
}

export interface Backups {
  dump: BackupRecord | null;
  bundle: BackupRecord | null;
}

function normalizeBackupRecord(value: unknown): BackupRecord | null {
  if (value === null || typeof value !== "object") return null;
  const raw = value as Record<string, unknown>;
  if (typeof raw.at_ms !== "number" || !Number.isFinite(raw.at_ms)) return null;
  return {
    at_ms: raw.at_ms,
    key: typeof raw.key === "string" ? raw.key : null,
    bytes: typeof raw.bytes === "number" && Number.isFinite(raw.bytes) ? raw.bytes : null,
  };
}

/** The health's `backups`; null when the server reports none (an older one). */
export function normalizeBackups(value: unknown): Backups | null {
  if (value === null || typeof value !== "object") return null;
  const raw = value as Record<string, unknown>;
  return { dump: normalizeBackupRecord(raw.dump), bundle: normalizeBackupRecord(raw.bundle) };
}

export interface BackupRow {
  name: string;
  /** `now` less the newest record's time; null when there is none. */
  age_ms: number | null;
  /** The newest record's first uploaded key; null when there is none. */
  key: string | null;
}

/** The Storage card's backup rows: the database copy, then the transcripts
 * and sources, each with its newest upload's age and key. Takes the
 * server's `health.backups` as it is or normalized. */
export function backupView(value: unknown, now: number): BackupRow[] {
  const backups = normalizeBackups(value);
  const row = (name: string, record: BackupRecord | null | undefined): BackupRow => ({
    name,
    age_ms: record ? now - record.at_ms : null,
    key: record ? record.key : null,
  });
  return [row("Database copy", backups?.dump), row("Transcripts and sources", backups?.bundle)];
}

/** A backup's age said as a span ("32 min", "10 h", "3 d"), or "no backup
 * yet" when there is none. */
export function backupAge(ageMs: number | null): string {
  if (ageMs === null) return "no backup yet";
  const minutes = Math.max(0, Math.round(ageMs / 60_000));
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.round(minutes / 60);
  return hours < 48 ? `${hours} h` : `${Math.round(hours / 24)} d`;
}

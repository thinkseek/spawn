// What the host has now, as `/api/fleet`'s health reports it beside the
// disk: memory (MemAvailable and MemTotal from /proc/meminfo, read when the
// request was answered) and each gated mission's waits for gate room (its
// `gate_room_*` columns: the run of `no_room` gate outcomes of one task).
// A value the server could not read stays null: nothing here guesses one.
//
// Pure (one import of another pure module), tested in web/test/resources.test.ts.

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
// databases the server is given, with their schemas (objective 074); what the archive grows by a day, and how
// long the free space lasts at that rate. The sizes are read at most every
// 10 minutes (`read_at_ms`); the free space at each request.

/** A directory's regular files: their sizes summed, and how many. */
export interface StoragePlace {
  path: string;
  bytes: number;
  /** Null where the server does not count them (the workspaces). */
  files: number | null;
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
  /** Everything under the root outside the archive; null without a root. */
  workspaces: StoragePlace | null;
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
  /** The free space over the growth, rounded down; null without a growth. */
  days_left: number | null;
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

/** The health's `storage`; null when the server reports none (an older one). */
export function normalizeStorage(value: unknown): Storage | null {
  const raw = object(value);
  if (!raw) return null;
  const projects = place(raw.projects);
  const database = object(raw.database);
  return {
    free_bytes: optionalNumber(raw.free_bytes),
    read_at_ms: optionalNumber(raw.read_at_ms),
    archive: place(raw.archive),
    projects: projects && { ...projects, before: optionalNumber(object(raw.projects)?.before) },
    workspaces: place(raw.workspaces),
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
    days_left: optionalNumber(raw.days_left),
  };
}

const MB = 1024 ** 2;

/** "12.0 MB" under 1024 MB, "3.0 GB" from there (1 MB is 1024 * 1024
 * bytes); "—" when unread. */
export function storageSize(bytes: number | null | undefined): string {
  if (bytes == null || !Number.isFinite(bytes)) return "—";
  const mb = bytes / MB;
  return mb < 1024 ? `${mb.toFixed(1)} MB` : `${(mb / 1024).toFixed(1)} GB`;
}

/** The storage rows' names of the test databases, by role. */
const TEST_DATABASE_NAMES: Record<string, string> = {
  test: "Gate test database",
  fleet_test: "Fleet test database",
};

export interface StorageRow {
  name: string;
  size: string;
  note: string;
}

export interface StorageView {
  rows: StorageRow[];
  /** "about <days> days at the archive's growth"; null without a growth. */
  lasts: string | null;
}

/** The Storage card's words: what uses the disk, one row each, and how
 * long the free space lasts at the archive's growth. Takes the server's
 * `health.storage` as it is or normalized. */
export function storageView(value: unknown): StorageView {
  const storage = normalizeStorage(value);
  const growth = storage?.growth_bytes_per_day ?? null;
  const projects = storage?.projects ?? null;
  const files = projects?.files ?? null;
  const before = projects?.before ?? null;
  const largest = storage?.database?.tables[0] ?? null;
  const logBytes = storage?.database?.log_bytes ?? null;
  const count = (n: number) => n.toLocaleString("en-US");
  const rows: StorageRow[] = [
    {
      name: "Transcript archive",
      size: storageSize(storage?.archive?.bytes),
      note: growth === null ? "not measured yet" : `grows about ${storageSize(growth)} a day`,
    },
    {
      name: "Claude Code transcripts",
      size: storageSize(projects?.bytes),
      note:
        files === null
          ? ""
          : `${count(files)} files${before !== null && before > files ? `, down from ${count(before)}` : ""}`,
    },
    { name: "Workspaces", size: storageSize(storage?.workspaces?.bytes), note: "" },
    {
      name: "Database",
      size: storageSize(storage?.database?.bytes),
      note: [
        largest ? `largest: ${largest.name} ${storageSize(largest.bytes)}` : null,
        logBytes !== null ? `log ${storageSize(logBytes)}` : null,
      ]
        .filter((part) => part !== null)
        .join(", "),
    },
    ...(storage?.test_databases ?? []).map((database) => ({
      name: TEST_DATABASE_NAMES[database.role] ?? `Test database (${database.role})`,
      size: storageSize(database.bytes),
      note: database.schemas === null ? "unreadable" : `${count(database.schemas)} schemas`,
    })),
  ];
  const days = storage?.days_left ?? null;
  return { rows, lasts: days === null ? null : `about ${count(days)} days at the archive's growth` };
}

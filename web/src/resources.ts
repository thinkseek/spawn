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

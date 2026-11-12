import assert from "node:assert/strict";
import { test } from "node:test";

import {
  MEMORY_WARN_BYTES,
  memoryBar,
  normalizeGateRoom,
  normalizeMemory,
  waitingGates,
} from "../src/resources.ts";

const GIB = 1024 ** 3;

test("memory is read as the server reports it, a missing value stays null", () => {
  assert.deepEqual(
    normalizeMemory({ available_bytes: 9 * GIB, total_bytes: 64 * GIB, warn_bytes: 4 * GIB }),
    { available_bytes: 9 * GIB, total_bytes: 64 * GIB, warn_bytes: 4 * GIB },
  );
  assert.deepEqual(normalizeMemory({ available_bytes: null, total_bytes: "lots" }), {
    available_bytes: null,
    total_bytes: null,
    warn_bytes: MEMORY_WARN_BYTES,
  });
  assert.equal(MEMORY_WARN_BYTES, 4 * GIB);
  assert.equal(normalizeMemory(undefined), null, "an older server reports none");
});

test("memory under the 4 GiB line is short; unread is unread, not zero", () => {
  const memory = (available: number | null, total: number | null) => ({
    available_bytes: available,
    total_bytes: total,
    warn_bytes: 4 * GIB,
  });
  assert.deepEqual(memoryBar(memory(3 * GIB, 64 * GIB)), { tone: "short", scale: 64 * GIB });
  assert.deepEqual(memoryBar(memory(20 * GIB, 64 * GIB)), { tone: "fine", scale: 64 * GIB });
  assert.equal(memoryBar(memory(null, 64 * GIB)).tone, "unread");
  // Without a total the bar runs to twice the line, or past what is free.
  assert.equal(memoryBar(memory(2 * GIB, null)).scale, 8 * GIB);
  assert.equal(memoryBar(memory(20 * GIB, null)).scale, 23 * GIB);
});

test("gate room: one row per gated mission, the waiting gates longest first", () => {
  const rows = normalizeGateRoom([
    { mission: 7, text: "Owner work on Spawn: changes", task: null, since_ms: null, last_ms: null, waits: 0, escalated: false },
    { mission: 4, text: "RSI: improve Spawn", task: 87, since_ms: 2_000, last_ms: 9_000, waits: 5, escalated: true },
    { mission: 9, text: null, task: 91, since_ms: 1_000, last_ms: 1_000, waits: 1, escalated: false },
    { text: "no id" },
    "nonsense",
  ]);
  assert.ok(rows);
  assert.deepEqual(
    rows.map((row) => [row.mission, row.name, row.task, row.waits, row.escalated]),
    [
      [7, "Owner work on Spawn", null, 0, false],
      [4, "RSI", 87, 5, true],
      [9, "Mission 9", 91, 1, false],
    ],
  );
  assert.deepEqual(
    waitingGates(rows).map((row) => row.mission),
    [9, 4],
  );
  assert.equal(normalizeGateRoom(undefined), null, "an older server reports none");
  assert.deepEqual(normalizeGateRoom([]), []);
});

// What uses the disk (objective 065): the Storage card's words.

import { readFileSync } from "node:fs";

import { backupAge, backupView, normalizeBackups, normalizeStorage, storageSize, storageView } from "../src/resources.ts";

const MB = 1024 ** 2;
const STORAGE = {
  free_bytes: 400 * GIB,
  read_at_ms: 1_000,
  archive: { path: "/ws/.transcripts", bytes: 12 * MB, files: 3 },
  projects: { path: "/home/x/.claude/projects", bytes: MB, files: 3, before: 40 },
  workspaces: { path: "/ws", bytes: 2 * MB },
  database: { bytes: 2.7 * GIB, tables: [{ name: "spawn_events", bytes: 2.5 * GIB }, { name: "mission_tasks", bytes: 9 * MB }] },
  growth_bytes_per_day: 0.07 * GIB,
  days_left: 5714,
};

test("a size is MB to one decimal under 1024 MB, GB from there", () => {
  assert.equal(storageSize(12 * MB), "12.0 MB");
  assert.equal(storageSize(1023.94 * MB), "1023.9 MB");
  assert.equal(storageSize(1024 * MB), "1.0 GB");
  assert.equal(storageSize(3 * GIB), "3.0 GB");
  assert.equal(storageSize(null), "—");
});

test("the storage rows say what uses the disk, its growth and Claude Code's fall", () => {
  const { rows, lasts } = storageView(STORAGE);
  assert.deepEqual(rows, [
    { name: "Transcript archive", size: "12.0 MB", note: "grows about 71.7 MB a day" },
    { name: "Claude Code transcripts", size: "1.0 MB", note: "3 files, down from 40" },
    { name: "Workspaces", size: "2.0 MB", note: "" },
    { name: "Database", size: "2.7 GB", note: "largest: spawn_events 2.5 GB" },
  ]);
  assert.equal(lasts, "about 5,714 days at the archive's growth");
});

test("no fall, no growth, no days: the card says so and invents nothing", () => {
  const steady = storageView({ ...STORAGE, projects: { ...STORAGE.projects, before: 3 } });
  assert.equal(steady.rows[1].note, "3 files");
  const grew = storageView({ ...STORAGE, projects: { ...STORAGE.projects, before: 1 } });
  assert.equal(grew.rows[1].note, "3 files", "a rise is no fall");
  const unsampled = storageView({ ...STORAGE, projects: { ...STORAGE.projects, before: null } });
  assert.equal(unsampled.rows[1].note, "3 files");
  const young = storageView({ ...STORAGE, growth_bytes_per_day: null, days_left: null });
  assert.deepEqual([young.rows[0].note, young.lasts], ["not measured yet", null]);
  const bare = storageView(null);
  assert.deepEqual(
    bare.rows.map((row) => row.size),
    ["—", "—", "—", "—"],
  );
  assert.equal(bare.lasts, null);
});

test("the health's storage is read as the server reports it, null from an older server", () => {
  const storage = normalizeStorage(STORAGE);
  assert.equal(storage?.projects?.before, 40);
  assert.equal(storage?.workspaces?.files, null, "the workspaces are not counted");
  assert.deepEqual(storage?.database?.tables[0], { name: "spawn_events", bytes: 2.5 * GIB });
  assert.equal(normalizeStorage(undefined), null);
  const rootless = normalizeStorage({ ...STORAGE, archive: null, workspaces: null, free_bytes: null });
  assert.deepEqual([rootless?.archive, rootless?.workspaces, rootless?.free_bytes], [null, null, null]);
});

test("the host section's Resources are followed by a Storage card drawn from storageView", () => {
  const strip = readFileSync(new URL("../src/components/fleet/HealthStrip.tsx", import.meta.url), "utf8");
  assert.match(strip, /<Cell label="Storage"[^>]*>\s*<StorageCard storage=\{health\.storage\} backups=\{health\.backups\} \/>/);
  assert.match(strip, /backupView\(backups, now\)/, "the card's backup rows are drawn from backupView");
  assert.match(strip, /const \{ rows, lasts \} = storageView\(storage\)/);
  assert.ok(strip.indexOf('label="Resources"') < strip.indexOf('label="Storage"'));
});

// Objective 074: the test databases the server is given, with their
// schemas, and the live event log's size beside the database's.

const TESTED = {
  ...STORAGE,
  database: { ...STORAGE.database, log_bytes: 35 * MB },
  test_databases: [
    { role: "test", name: "spawn_test", bytes: 20 * MB, schemas: 0 },
    { role: "fleet_test", name: "spawn_fleet_test", bytes: 2.7 * GIB, schemas: 2533 },
  ],
};

test("the test databases follow the database, the gates' first, each with its schemas", () => {
  const { rows } = storageView(TESTED);
  assert.deepEqual(rows.slice(3), [
    { name: "Database", size: "2.7 GB", note: "largest: spawn_events 2.5 GB, log 35.0 MB" },
    { name: "Gate test database", size: "20.0 MB", note: "0 schemas" },
    { name: "Fleet test database", size: "2.7 GB", note: "2,533 schemas" },
  ]);
});

test("no test database, no row; the log alone when there is no largest table", () => {
  const without = storageView({ ...TESTED, test_databases: [] }).rows;
  assert.equal(without.find((row) => /test database/.test(row.name)), undefined);
  assert.equal(without.length, 4);
  const older = storageView(STORAGE).rows;
  assert.equal(older.length, 4, "an older server reports none");
  const tableless = storageView({ ...TESTED, database: { bytes: MB, tables: [], log_bytes: 2 * MB } }).rows;
  assert.equal(tableless[3].note, "log 2.0 MB");
  const fleetOnly = storageView({ ...TESTED, test_databases: [TESTED.test_databases[1]] }).rows;
  assert.deepEqual(
    fleetOnly.slice(4).map((row) => row.name),
    ["Fleet test database"],
  );
});

test("a test database the server could not read is said unreadable, and kept", () => {
  const storage = normalizeStorage({
    ...TESTED,
    test_databases: [{ role: "test", name: null, bytes: null, schemas: null, error: "refused" }, { bad: 1 }],
  });
  assert.deepEqual(storage?.test_databases, [{ role: "test", name: null, bytes: null, schemas: null }]);
  assert.equal(storage?.database?.log_bytes, 35 * MB);
  const row = storageView(storage).rows[4];
  assert.deepEqual(row, { name: "Gate test database", size: "—", note: "unreadable" });
  assert.deepEqual(normalizeStorage(STORAGE)?.test_databases, []);
  assert.equal(normalizeStorage(STORAGE)?.database?.log_bytes, null);
});

test("fresh test databases (no schema) are no rows; a tiny log is not said", () => {
  const fresh = storageView({
    ...TESTED,
    database: { ...TESTED.database, log_bytes: 1392 },
    test_databases: [{ role: "test", name: "spawn_test", bytes: 8 * MB, schemas: 0 }],
  }).rows;
  assert.deepEqual(
    fresh.map((row) => row.name),
    ["Transcript archive", "Claude Code transcripts", "Workspaces", "Database"],
  );
  assert.equal(fresh[3].note, "largest: spawn_events 2.5 GB", "log 0.0 MB would say nothing");
  // One schema in either shows both, the empty one with "0 schemas".
  const left = storageView({
    ...TESTED,
    test_databases: [
      { role: "test", name: "spawn_test", bytes: 8 * MB, schemas: 0 },
      { role: "fleet_test", name: "spawn_fleet_test", bytes: 9 * MB, schemas: 1 },
    ],
  }).rows;
  assert.deepEqual(
    left.slice(4).map((row) => [row.name, row.note]),
    [
      ["Gate test database", "0 schemas"],
      ["Fleet test database", "1 schema"],
    ],
  );
  const small = storageView({ ...TESTED, database: { ...TESTED.database, log_bytes: 0.06 * MB } }).rows;
  assert.equal(small[3].note, "largest: spawn_events 2.5 GB, log 0.1 MB");
});

test("backupView: the database copy then the bundle, with their ages and keys", () => {
  const now = 100_000_000;
  const backups = {
    dump: { at_ms: now - 1_800_000, key: "postgres/hourly/a.dump.gpg", bytes: 9_400_000 },
    bundle: { at_ms: now - 36_000_000, key: "bundles/incr/b.tar.zst.gpg", bytes: 5_000_000 },
  };
  assert.deepEqual(backupView(backups, now), [
    { name: "Database copy", age_ms: 1_800_000, key: "postgres/hourly/a.dump.gpg" },
    { name: "Transcripts and sources", age_ms: 36_000_000, key: "bundles/incr/b.tar.zst.gpg" },
  ]);
  assert.deepEqual(backupView({ ...backups, bundle: null }, now)[1], {
    name: "Transcripts and sources",
    age_ms: null,
    key: null,
  });
  // An older server reports no backups: both rows have none.
  assert.deepEqual(
    backupView(undefined, now).map((row) => [row.name, row.age_ms, row.key]),
    [
      ["Database copy", null, null],
      ["Transcripts and sources", null, null],
    ],
  );
  assert.equal(normalizeBackups(undefined), null);
  assert.deepEqual(normalizeBackups({ dump: { at_ms: "soon" }, bundle: null }), { dump: null, bundle: null });
  assert.equal(backupAge(1_800_000 + 12_000), "30 min");
  assert.equal(backupAge(36_000_000), "10 h");
  assert.equal(backupAge(3 * 86_400_000), "3 d");
  assert.equal(backupAge(null), "no backup yet");
});

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

import { localDateTime } from "../src/format.ts";
import {
  backupAge,
  backupView,
  normalizeBackups,
  normalizeSharedBuildCache,
  normalizeStorage,
  normalizeWorkspaceBreakdown,
  sharedBuildCacheView,
  storageSize,
  storageView,
  workspaceBreakdownView,
} from "../src/resources.ts";

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
    { name: "Transcript archive", size: "12.0 MB", note: "grows about 71.7 MB a day", measured_from: null },
    { name: "Claude Code transcripts", size: "1.0 MB", note: "3 files, down from 40", measured_from: null },
    { name: "Workspaces", size: "2.0 MB", note: "", measured_from: null },
    { name: "Database", size: "2.7 GB", note: "largest: spawn_events 2.5 GB", measured_from: null },
  ]);
  assert.equal(lasts, "about 5,714 days at the growth of the archive, Claude Code's transcripts and the database");
});

test("how long the disk lasts is said at the growth of what persists; the archive keeps its own", () => {
  const view = storageView({
    ...STORAGE,
    growth_bytes_per_day: 2 * MB,
    growth_parts: { archive: 2 * MB, projects: MB, database: 7 * MB },
    persistent_growth_bytes_per_day: 10 * MB,
    days_left: 1234,
  });
  assert.equal(view.lasts, "about 1,234 days at the growth of the archive, Claude Code's transcripts and the database");
  assert.equal(view.rows[0].note, "grows about 2.0 MB a day", "the archive's row: its own growth");
  const normalized = normalizeStorage({ ...STORAGE, growth_parts: { archive: 5, projects: null, database: 7 } });
  // An older server's three parts: the session backups' is null (objective 097).
  assert.deepEqual(normalized?.growth_parts, { archive: 5, projects: null, database: 7, session_backups: null });
  assert.equal(normalized?.growth_excluded, null);
  assert.equal(normalized?.persistent_growth_bytes_per_day, null);
  // Not yet measured: the archive keeps its growth, the disk has no days.
  const young = storageView({ ...STORAGE, growth_bytes_per_day: 2 * MB, persistent_growth_bytes_per_day: null, days_left: null });
  assert.equal(young.lasts, null);
  assert.equal(young.rows[0].note, "grows about 2.0 MB a day");
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

test("while the growth is not measured yet, the archive's row says from when it is", () => {
  const at = Date.UTC(2026, 10, 12, 23, 31, 20);
  const local = localDateTime(at);
  const archive = (view: ReturnType<typeof storageView>) => view.rows[0].note;
  // Pending: no growth, and the moment is still ahead of the read.
  const pending = storageView({ ...STORAGE, read_at_ms: at - 3_600_000, growth_bytes_per_day: null, days_left: null, growth_from_ms: at });
  assert.deepEqual([archive(pending), pending.measured_from, pending.lasts], [`measured from ${local}`, local, null]);
  assert.equal(normalizeStorage({ ...STORAGE, growth_from_ms: at })?.growth_from_ms, at);
  // Measured: the growth, as before.
  const measured = storageView({ ...STORAGE, read_at_ms: at + 3_600_000, growth_from_ms: at });
  assert.deepEqual([archive(measured), measured.measured_from], ["grows about 71.7 MB a day", null]);
  // No sample (or an older server): not measured yet.
  const none = storageView({ ...STORAGE, growth_bytes_per_day: null, days_left: null, growth_from_ms: null });
  assert.deepEqual([archive(none), none.measured_from], ["not measured yet", null]);
  const older = storageView({ ...STORAGE, growth_bytes_per_day: null, days_left: null });
  assert.deepEqual([archive(older), older.measured_from], ["not measured yet", null]);
  // Already past: a moment gone promises nothing.
  const past = storageView({ ...STORAGE, read_at_ms: at + 3_600_000, growth_bytes_per_day: null, days_left: null, growth_from_ms: at });
  assert.deepEqual([archive(past), past.measured_from], ["not measured yet", null]);
  assert.equal(storageView(null).measured_from, null);
});

test("while how long the disk lasts is not measured yet, the card's last line says from when it is", () => {
  const at = Date.UTC(2026, 10, 12, 23, 31, 20);
  const local = localDateTime(at);
  // Pending: no days left, and the moment is still ahead of the read.
  const pending = storageView({ ...STORAGE, read_at_ms: at - 3_600_000, days_left: null, persistent_from_ms: at });
  assert.deepEqual([pending.lasts, pending.lasts_from], [null, local]);
  assert.equal(normalizeStorage({ ...STORAGE, persistent_from_ms: at })?.persistent_from_ms, at);
  assert.deepEqual(pending.rows, storageView({ ...STORAGE, days_left: null }).rows, "the rows are unchanged");
  // Measured: the days, no moment.
  const measured = storageView({ ...STORAGE, read_at_ms: at - 3_600_000, days_left: 40, persistent_from_ms: at });
  assert.equal(measured.lasts_from, null);
  assert.match(measured.lasts ?? "", /^about 40 days at /);
  // Already past: a moment gone promises nothing.
  const past = storageView({ ...STORAGE, read_at_ms: at + 3_600_000, days_left: null, persistent_from_ms: at });
  assert.equal(past.lasts_from, null);
  // None (or an older server).
  assert.equal(storageView({ ...STORAGE, days_left: null, persistent_from_ms: null }).lasts_from, null);
  assert.equal(storageView({ ...STORAGE, days_left: null }).lasts_from, null);
  assert.equal(storageView(null).lasts_from, null);
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
  assert.match(
    strip,
    /<Cell label="Storage"[^>]*>\s*<StorageCard storage=\{health\.storage\} backups=\{health\.backups\} diagnostics=\{diagnostics\} \/>/,
  );
  assert.match(strip, /backupView\(backups, now\)/, "the card's backup rows are drawn from backupView");
  assert.match(strip, /const \{ rows, lasts, lasts_note, measured_from, lasts_from \} = storageView\(storage\)/);
  // Objective 098: each line's own date beside it while it is set (not twice
  // when the note already says it, as the archive's 083 note does).
  assert.match(strip, /row\.measured_from !== null && row\.note !== `measured from \$\{row\.measured_from\}`/);
  assert.match(strip, /measured from \{row\.measured_from\}/);
  // Objective 097: the note that the session backups are not counted, under the line.
  assert.match(strip, /\{lasts_note !== null && <p[^>]*>\{lasts_note\}<\/p>\}/);
  assert.match(strip, /How long the disk lasts: \$\{lasts_from !== null \? `measured from \$\{lasts_from\}` : measured_from === null \? "not measured yet" : `measured from \$\{measured_from\}`\}/);
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
    { name: "Database", size: "2.7 GB", note: "largest: spawn_events 2.5 GB, log 35.0 MB", measured_from: null },
    { name: "Gate test database", size: "20.0 MB", note: "0 schemas", measured_from: null },
    { name: "Fleet test database", size: "2.7 GB", note: "2,533 schemas", measured_from: null },
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
  assert.deepEqual(row, { name: "Gate test database", size: "—", note: "unreadable", measured_from: null });
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

// Objective 097: how long the disk lasts names the session backups when
// they are counted, and says so when they are left out.
test("storageView counts the session backups or says they are not counted", () => {
  const counted = storageView({
    ...STORAGE,
    days_left: 1234,
    growth_parts: { archive: 1, projects: 1, database: 1, session_backups: 1 },
    growth_excluded: [],
  });
  assert.equal(
    counted.lasts,
    "about 1,234 days at the growth of the archive, Claude Code's transcripts, the database and the session backups",
  );
  assert.equal(counted.lasts_note, null);
  const excluded = storageView({
    ...STORAGE,
    days_left: 1234,
    growth_parts: { archive: 1, projects: 1, database: 1, session_backups: null },
    growth_excluded: ["session_backups"],
  });
  assert.equal(
    excluded.lasts,
    "about 1,234 days at the growth of the archive, Claude Code's transcripts and the database",
  );
  assert.equal(excluded.lasts_note, "session backups are not counted");
  // Left out with no days left yet: the note still says so.
  assert.equal(storageView({ ...STORAGE, days_left: null, growth_excluded: ["session_backups"] }).lasts_note,
    "session backups are not counted");
  // An older server lists nothing: 085's words, no note.
  const older = storageView({ ...STORAGE, days_left: 1234 });
  assert.equal(older.lasts, "about 1,234 days at the growth of the archive, Claude Code's transcripts and the database");
  assert.equal(older.lasts_note, null);
});

// Objective 098: each grower's line says from when its own growth is
// measured, while it is pending; local time, as the archive's 083 date.
test("each grower's row carries its own measured_from while its growth is pending", () => {
  const pad = (n: number) => String(n).padStart(2, "0");
  const local = (ms: number) => {
    const d = new Date(ms);
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  };
  const at = Date.UTC(2026, 10, 12, 23, 31, 20);
  const parts = { archive: at, projects: at + 60_000, database: at + 120_000, session_backups: at + 180_000 };
  const lines = (value: unknown) =>
    ["Transcript archive", "Claude Code transcripts", "Database"].map(
      (name) => storageView(value).rows.find((row) => row.name === name)?.measured_from,
    );
  const nulls = { archive: null, projects: null, database: null, session_backups: null };
  // Pending: each its own date.
  const pending = { ...STORAGE, read_at_ms: at - 3_600_000, growth_bytes_per_day: null, growth_parts: nulls, growth_from_ms: at, growth_from_parts: parts };
  assert.deepEqual(lines(pending), [local(at), local(at + 60_000), local(at + 120_000)]);
  // No new row for the session backups, nor a date on another row.
  const view = storageView(pending);
  assert.equal(view.rows.filter((row) => row.measured_from !== null).length, 3);
  assert.equal(view.rows.find((row) => row.name === "Workspaces")?.measured_from, null);
  // Measured: no date, whatever the moment.
  assert.deepEqual(
    lines({ ...pending, growth_bytes_per_day: 5, growth_parts: { archive: 5, projects: 1, database: 2, session_backups: null } }),
    [null, null, null],
  );
  // Only the database pending: only its line dated.
  assert.deepEqual(lines({ ...pending, growth_parts: { archive: 5, projects: 1, database: null, session_backups: null } }), [
    null,
    null,
    local(at + 120_000),
  ]);
  // A moment already past the read: none.
  assert.deepEqual(lines({ ...pending, read_at_ms: at + 3_600_000 }), [null, null, null]);
  // No moment, or an older server's answer: none.
  assert.deepEqual(lines({ ...pending, growth_from_parts: { archive: null, projects: null, database: null } }), [null, null, null]);
  assert.deepEqual(lines({ ...STORAGE, growth_parts: nulls }), [null, null, null]);
  assert.equal(normalizeStorage({ ...STORAGE, growth_from_parts: parts })?.growth_from_parts?.database, at + 120_000);
  assert.equal(normalizeStorage(STORAGE)?.growth_from_parts, null);
});

test("workspace_breakdown and shared_build_cache normalize full PR #184 payloads and compute percentages", () => {
  const raw = {
    ...STORAGE,
    workspaces: { path: "/ws", bytes: 1_000 * MB },
    workspace_breakdown: {
      workspace_count: 2,
      total_bytes: 1_000 * MB,
      repo_bytes: 500 * MB,
      git_bytes: 200 * MB,
      build_cache_bytes: 300 * MB,
      files: 1420,
      truncated: false,
      workspaces: [
        {
          name: "alpha",
          total_bytes: 200 * MB,
          repo_bytes: 100 * MB,
          git_bytes: 50 * MB,
          build_cache_bytes: 50 * MB,
          files: 300,
          truncated: false,
        },
        {
          name: "beta",
          total_bytes: 800 * MB,
          repo_bytes: 400 * MB,
          git_bytes: 150 * MB,
          build_cache_bytes: 250 * MB,
          files: 1120,
          truncated: false,
        },
      ],
    },
    shared_build_cache: {
      path: "/ws/.caches",
      bytes: 1.5 * GIB,
      files: 320,
      enabled: true,
    },
  };

  const storage = normalizeStorage(raw);
  assert.equal(storage?.workspace_breakdown?.workspace_count, 2);
  assert.deepEqual(
    storage?.workspace_breakdown?.workspaces.map((ws) => ws.name),
    ["beta", "alpha"],
  );
  assert.deepEqual(storage?.shared_build_cache, {
    path: "/ws/.caches",
    bytes: 1.5 * GIB,
    files: 320,
    enabled: true,
  });

  const breakdown = workspaceBreakdownView(storage);
  assert.ok(breakdown);
  assert.equal(breakdown.total, "1000.0 MB");
  assert.equal(breakdown.shown_count, 2);
  assert.equal(breakdown.hidden_count, 0);
  assert.deepEqual(breakdown.shares, { repo_pct: 50, git_pct: 20, rebuildable_pct: 30 });
  assert.equal(breakdown.workspaces[0].name, "beta");
  assert.equal(breakdown.workspaces[0].summary, "repo 400.0 MB (50%) · .git 150.0 MB (19%) · rebuildable 250.0 MB (31%)");

  assert.deepEqual(sharedBuildCacheView(storage), {
    status: "enabled",
    badge: "enabled",
    size: "1.5 GB",
    files: 320,
    path: "/ws/.caches",
    note: "320 files",
  });
});

test("older server payloads, null shared_build_cache, and disabled residual caches are handled cleanly", () => {
  const older = normalizeStorage(STORAGE);
  assert.equal(older?.workspace_breakdown, null);
  assert.equal(older?.shared_build_cache, null);
  assert.equal(workspaceBreakdownView(older), null);
  assert.deepEqual(sharedBuildCacheView(older), {
    status: "off",
    badge: "off",
    size: null,
    files: null,
    path: null,
    note: "Disabled or not reported",
  });

  const disabledResidual = sharedBuildCacheView({
    ...STORAGE,
    shared_build_cache: {
      path: "/ws/.caches",
      bytes: 25 * MB,
      files: 1,
      enabled: false,
    },
  });
  assert.deepEqual(disabledResidual, {
    status: "disabled",
    badge: "disabled",
    size: "25.0 MB",
    files: 1,
    path: "/ws/.caches",
    note: "residual cache · 1 file",
  });
});

test("workspace_breakdown caps at 32 workspaces, handles truncated/zero-byte/malformed data safely", () => {
  const many = Array.from({ length: 35 }, (_, idx) => ({
    name: `ws-${String(idx).padStart(2, "0")}`,
    total_bytes: (idx + 1) * MB,
    repo_bytes: (idx + 1) * MB,
    git_bytes: 0,
    build_cache_bytes: 0,
    files: 1,
    truncated: idx === 34,
  }));
  const view = workspaceBreakdownView({
    workspace_count: 35,
    total_bytes: 630 * MB,
    repo_bytes: 630 * MB,
    git_bytes: 0,
    build_cache_bytes: 0,
    files: 35,
    truncated: true,
    workspaces: [...many, { bad: true }, { name: "" }],
  });
  assert.ok(view);
  assert.equal(view.truncated, true);
  assert.equal(view.workspace_count, 35);
  assert.equal(view.shown_count, 32);
  assert.equal(view.hidden_count, 3);
  assert.equal(view.workspaces[0].name, "ws-34");
  assert.equal(view.workspaces[0].truncated, true);

  const emptyZero = workspaceBreakdownView({
    workspace_count: 1,
    total_bytes: 0,
    repo_bytes: 0,
    git_bytes: 0,
    build_cache_bytes: 0,
    files: 0,
    truncated: false,
    workspaces: [
      {
        name: "empty",
        total_bytes: 0,
        repo_bytes: -5,
        git_bytes: Number.NaN,
        build_cache_bytes: 0,
        files: -1,
        truncated: false,
      },
    ],
  });
  assert.deepEqual(emptyZero?.shares, { repo_pct: 0, git_pct: 0, rebuildable_pct: 0 });
  assert.deepEqual(emptyZero?.workspaces[0].shares, { repo_pct: 0, git_pct: 0, rebuildable_pct: 0 });

  assert.equal(normalizeWorkspaceBreakdown({}), null);
  assert.equal(normalizeWorkspaceBreakdown("invalid"), null);
  assert.equal(normalizeSharedBuildCache({}), null);
  assert.equal(normalizeSharedBuildCache(null), null);
});

test("HealthStrip renders WorkspaceBreakdownSection and SharedBuildCacheRow from resources helpers", () => {
  const strip = readFileSync(new URL("../src/components/fleet/HealthStrip.tsx", import.meta.url), "utf8");
  for (const needle of [
    "workspaceBreakdownView(storage)",
    "sharedBuildCacheView(storage)",
    "WorkspaceBreakdownSection",
    "SharedBuildCacheRow",
    "Shared build cache",
    "rebuildable",
  ]) {
    assert.ok(strip.includes(needle), `HealthStrip.tsx includes ${needle}`);
  }
  assert.equal(strip.includes("/api/"), false);
});

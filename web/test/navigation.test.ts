import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  DIAGNOSTIC_VIEWS,
  NAV_ENTRIES,
  PRIMARY_VIEWS,
  SECONDARY_VIEWS,
  isDiagnosticView,
  shouldExpandDiagnostics,
} from "../src/navigation.ts";
import {
  actionableHomeNeeds,
  buildSystemStatusModel,
  missionProgressSummary,
  type NeedItem,
} from "../src/home.ts";
import { normalizeMissions } from "../src/missions.ts";

test("side-tab taxonomy cleanly separates Core, Cost, and collapsed Diagnostics", () => {
  assert.deepEqual(PRIMARY_VIEWS, ["fleet", "missions", "agents", "features"]);
  assert.deepEqual(SECONDARY_VIEWS, ["cost"]);
  assert.deepEqual(DIAGNOSTIC_VIEWS, [
    "grading",
    "scorecard",
    "coverage",
    "rsi",
    "alerts",
    "storage",
    "show",
  ]);

  const combined = [...PRIMARY_VIEWS, ...SECONDARY_VIEWS, ...DIAGNOSTIC_VIEWS];
  assert.equal(new Set(combined).size, combined.length, "no duplicate view ids across tiers");
  assert.equal(NAV_ENTRIES.length, combined.length);

  assert.equal(isDiagnosticView("fleet"), false);
  assert.equal(isDiagnosticView("cost"), false);
  assert.equal(isDiagnosticView("coverage"), true);
  assert.equal(isDiagnosticView("scorecard"), true);
  assert.equal(isDiagnosticView("alerts"), true);

  assert.equal(shouldExpandDiagnostics("closed", "fleet"), false);
  assert.equal(shouldExpandDiagnostics("closed", "missions"), false);
  assert.equal(shouldExpandDiagnostics("closed", "coverage"), true);
  assert.equal(shouldExpandDiagnostics("closed", "scorecard"), true);
  assert.equal(shouldExpandDiagnostics("open", "fleet"), true);
});

test("App shell removes global OpenAlerts banner and MobileNav uses a slide-over side drawer instead of top horizontal tabs", () => {
  const appSrc = readFileSync(new URL("../src/App.tsx", import.meta.url), "utf8");
  const sidebarSrc = readFileSync(new URL("../src/components/Sidebar.tsx", import.meta.url), "utf8");

  assert.equal(appSrc.includes("<OpenAlerts"), false, "global OpenAlerts red bar must be removed");
  assert.ok(appSrc.includes("actionableHomeNeeds"), "App uses actionable-only Home badge counts");
  assert.ok(sidebarSrc.includes("LocaleSwitch"), "Sidebar and MobileNav expose 1-click locale switcher");
  assert.equal(
    sidebarSrc.includes("overflow-x-auto"),
    false,
    "MobileNav must not use a horizontal scrolling tab bar",
  );
  assert.ok(
    sidebarSrc.includes('aria-modal="true"') && sidebarSrc.includes("SideTabList"),
    "MobileNav renders a left slide-over side-tab drawer",
  );
});

test("actionableHomeNeeds drops raw system alert:* chatter while retaining halts, actions, and disk/memory warnings", () => {
  const items: NeedItem[] = [
    { key: "alert:stuck:1", tone: "act", title: "An agent looks stuck", detail: "...", since_ms: 1, command: null, session: "s1", judge: "spawn alert judge x" },
    { key: "alert:idle:6", tone: "know", title: "Mission idle", detail: "...", since_ms: 2, command: null, session: null },
    { key: "warning:0", tone: "know", title: "View incomplete", detail: "...", since_ms: null, command: null, session: null },
    { key: "halt:4:99", tone: "act", title: "Mission #4 halted", detail: "scenario failed", since_ms: 3, command: null, session: null, resume: { mission: 4, halt: 99 } },
    { key: "attention:esc:s2", tone: "act", title: "Needs decision", detail: "Approve step", since_ms: 4, command: null, session: "s2" },
    { key: "disk:workspace", tone: "know", title: "Disk workspace low", detail: "2 GiB free", since_ms: null, command: null, session: null },
  ];

  const filtered = actionableHomeNeeds(items);
  assert.deepEqual(
    filtered.map((item) => item.key),
    ["halt:4:99", "attention:esc:s2", "disk:workspace"],
  );
});

test("missionProgressSummary and buildSystemStatusModel compute accurate completion percentages and health tones", () => {
  const now = 1_794_224_000_000;
  const missions = normalizeMissions({
    missions: [
      {
        id: 12,
        text: "Spawn UX Redesign",
        state: "active",
        gate: "rsi/scenarios",
        tasks: [
          { id: 1, state: "landed", updated_at_ms: now - 30_000, stages: [{ stage: "landed", at_ms: now - 30_000 }] },
          { id: 2, state: "landed", updated_at_ms: now - 20_000, stages: [{ stage: "landed", at_ms: now - 20_000 }] },
          { id: 3, state: "delivered", task_title: "Redesign side tabs", updated_at_ms: now - 5_000, stages: [] },
          { id: 4, state: "cancelled", stages: [] },
        ],
      },
      {
        id: 8,
        text: "Old Completed",
        state: "closed",
        tasks: [{ id: 10, state: "landed", stages: [] }],
      },
    ],
    residents: [],
    holds: [],
  });

  const progress = missionProgressSummary(missions, now);
  assert.equal(progress.openMissions, 1);
  assert.equal(progress.closedMissions, 1);
  assert.equal(progress.completedTasks, 2);
  assert.equal(progress.totalTasks, 3);
  assert.equal(progress.overallPct, 67);
  assert.equal(progress.landedToday, 2);
  assert.equal(progress.rows[0].status, "active");
  assert.equal(progress.rows[0].completionPct, 67);

  const healthy = buildSystemStatusModel({
    health: null,
    liveness: { total: 2, up: 2 },
    agents: null,
    nodes: [{ state: "live" }, { state: "live" }],
    missionProgress: progress,
    actionCount: 0,
    freshnessLevel: "fresh",
  });
  assert.equal(healthy.tone, "healthy");
  assert.equal(healthy.nodesOnline, 2);
  assert.equal(healthy.nodesTotal, 2);

  const degraded = buildSystemStatusModel({
    health: null,
    liveness: { total: 2, up: 2 },
    agents: null,
    nodes: [{ state: "live" }, { state: "lost" }],
    missionProgress: progress,
    actionCount: 1,
    freshnessLevel: "fresh",
  });
  assert.equal(degraded.tone, "degraded");
});

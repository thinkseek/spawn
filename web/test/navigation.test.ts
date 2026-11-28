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
  alertTitle,
  buildSystemStatusModel,
  countOutageAlerts,
  missionProgressSummary,
  needsSummary,
  needsYou,
  type NeedItem,
} from "../src/home.ts";
import type { Alert } from "../src/api.ts";
import type { FleetAgent } from "../src/fleet.ts";
import { normalizeMissions } from "../src/missions.ts";
import { agentGroupLabel } from "../src/ordering.ts";

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
  // Explicit user toggle overrides diagnostic-route auto-expand
  assert.equal(shouldExpandDiagnostics("closed", "coverage", "closed"), false);
  assert.equal(shouldExpandDiagnostics("open", "coverage", "closed"), false);
  assert.equal(shouldExpandDiagnostics("closed", "fleet", "open"), true);
});

test("App shell removes dead MissionsTodayCard/OpenAlerts and MobileNav manages focus in its slide-over drawer", () => {
  const appSrc = readFileSync(new URL("../src/App.tsx", import.meta.url), "utf8");
  const sidebarSrc = readFileSync(new URL("../src/components/Sidebar.tsx", import.meta.url), "utf8");
  const homeSrc = readFileSync(new URL("../src/components/home/Home.tsx", import.meta.url), "utf8");
  const alertsSrc = readFileSync(new URL("../src/components/alerts/AlertsView.tsx", import.meta.url), "utf8");

  assert.equal(appSrc.includes("<OpenAlerts"), false, "global OpenAlerts red bar must be removed");
  assert.equal(homeSrc.includes("MissionsTodayCard"), false, "unused MissionsTodayCard dead code must be removed");
  assert.equal(alertsSrc.includes("export function OpenAlerts"), false, "unused OpenAlerts dead code must be removed");
  assert.ok(appSrc.includes("actionableHomeNeeds"), "App uses actionable-only Home badge counts");
  assert.ok(sidebarSrc.includes("LocaleSwitch"), "Sidebar and MobileNav expose 1-click locale switcher");
  assert.equal(
    sidebarSrc.includes("overflow-x-auto"),
    false,
    "MobileNav must not use a horizontal scrolling tab bar",
  );
  assert.ok(
    sidebarSrc.includes('aria-modal="true"') &&
      sidebarSrc.includes("closeBtnRef") &&
      sidebarSrc.includes("triggerRef"),
    "MobileNav renders an accessible drawer with open/close focus management",
  );
  assert.ok(
    sidebarSrc.includes("compactBadge") &&
      sidebarSrc.includes("<ThemeSwitch compact") &&
      sidebarSrc.includes("iconOnly"),
    "Collapsed rail preserves Home badge, ThemeSwitch, and SignOut controls",
  );
});

test("actionableHomeNeeds retains critical/outage/owner alerts and real halted:<id> keys while silencing noisy alerts", () => {
  const now = 1_794_224_000_000;
  const mkAlert = (
    key: string,
    kind: string,
    subject: string,
    text: string,
    evidence: unknown = null,
    fired_at_ms = now - 60_000,
  ): Alert => ({
    key,
    kind,
    subject,
    severity: "high",
    state: "open",
    text,
    evidence,
    fired_at_ms,
    cleared_at_ms: null,
    cleared_evidence: null,
    scored_at_ms: null,
  });

  const agents: FleetAgent[] = [
    {
      session_id: "s1",
      agent_id: "w1",
      live: true,
      judged_at_ms: now - 10_000,
      judgement: { stuck: 0.9, reason: "looping" },
    } as unknown as FleetAgent,
    {
      session_id: "s_stale",
      agent_id: "w_stale",
      live: true,
      judged_at_ms: now - 5_000,
      judgement: { stuck: null, reason: "judge skipped" },
    } as unknown as FleetAgent,
  ];

  const rawItems = needsYou({
    attention: null,
    missions: normalizeMissions({
      missions: [
        {
          id: 4,
          text: "Mission Four",
          state: "active",
          halted: "scenario failed",
          halt: { event: 99, at_ms: now - 30_000, gone: null },
          tasks: [],
        },
      ],
      residents: [],
      holds: [],
    }),
    alerts: [
      mkAlert("deploy_failed:abc", "deploy_failed", "deploy", "deploy failed on v1"),
      mkAlert("integrator_down", "integrator_down", "integrator", "integrator unreachable"),
      mkAlert("owner_request_waiting:7", "owner_request_waiting", "landing #7", "waiting for verdict", {
        withdraw: "spawn mission push --withdraw 7",
      }),
      mkAlert("agent_stuck:s1", "agent_stuck", "s1", "worker stuck", {
        stuck: 0.9,
        command: "spawn task restart --id 12",
      }, now - 120_000),
      mkAlert("agent_stuck:s_stale", "agent_stuck", "s_stale", "stale stuck alert", {
        stuck: 0.7,
      }, now - 180_000),
      mkAlert("mission_idle:6:40", "mission_idle", "mission #6", "idle"),
      mkAlert("backup_stale:pg", "backup_stale", "pg", "backup stale"),
      mkAlert("admission_wait:w", "admission_wait", "pool", "waiting"),
      mkAlert("validator_defect:1", "validator_defect", "main", "defect"),
      mkAlert("message_not_taken:s1", "message_not_taken", "s1", "unread"),
      mkAlert("source_away:4", "source_away", "mission #4", "away"),
      mkAlert("guardrail_out_of_range:disk", "guardrail_out_of_range", "disk", "range"),
    ],
    agents,
    health: null,
    missingAlwaysOn: 0,
    now,
    locale: "zh",
  });

  const filtered = actionableHomeNeeds(rawItems);
  assert.deepEqual(
    filtered.map((item) => item.key),
    [
      "alert:agent_stuck:s1",
      "alert:deploy_failed:abc",
      "alert:integrator_down",
      "alert:owner_request_waiting:7",
      "halted:4",
    ],
  );
  assert.equal(countOutageAlerts(filtered), 2, "deploy_failed and integrator_down count as outages");
  assert.match(filtered[1].title, /部署失败/);
  assert.equal(agentGroupLabel("always_on", "zh"), "常驻");
  assert.equal(needsSummary(filtered, "zh"), "5 项待处理");
  assert.match(alertTitle({ kind: "integrator_down", subject: "integrator" }, null, "zh"), /集成器离线/);

  const progress = missionProgressSummary(normalizeMissions({ missions: [], residents: [], holds: [] }), now);
  const outageHero = buildSystemStatusModel({
    health: null,
    liveness: { total: 1, up: 1 },
    agents,
    nodes: [{ state: "live" }],
    missionProgress: progress,
    actionCount: filtered.filter((item) => item.tone === "act").length,
    criticalOutageCount: countOutageAlerts(filtered),
    freshnessLevel: "fresh",
  });
  assert.equal(outageHero.tone, "degraded", "critical outage alerts elevate hero to degraded");

  const approvalOnly: NeedItem[] = filtered.filter((item) => item.key === "alert:owner_request_waiting:7");
  const approvalHero = buildSystemStatusModel({
    health: null,
    liveness: { total: 1, up: 1 },
    agents,
    nodes: [{ state: "live" }],
    missionProgress: progress,
    actionCount: approvalOnly.length,
    criticalOutageCount: countOutageAlerts(approvalOnly),
    freshnessLevel: "fresh",
  });
  assert.equal(approvalHero.tone, "attention", "owner-approval alerts elevate hero to attention");
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

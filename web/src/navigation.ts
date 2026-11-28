/** Side-tab navigation taxonomy for the Spawn web console.
 *
 * Keeps primary views (System Overview, Missions, Agents, Feature Map) up front,
 * Cost in a secondary slot, and engineering diagnostics (Coverage, Scorecard,
 * Grading, RSI, Alerts, Storage, Show) collapsed by default. */

import type { Locale } from "./i18n.ts";

export type NavView =
  | "fleet"
  | "agents"
  | "missions"
  | "grading"
  | "features"
  | "coverage"
  | "rsi"
  | "alerts"
  | "scorecard"
  | "cost"
  | "storage"
  | "show"
  | "sessions";

export type NavGroupId = "primary" | "secondary" | "diagnostics";

export interface NavEntry {
  id: NavView;
  group: NavGroupId;
  description: Record<Locale, string>;
}

/** Primary views always visible in the top section of the side navigation. */
export const PRIMARY_VIEWS: readonly NavView[] = [
  "fleet",
  "missions",
  "agents",
  "features",
];

/** Secondary operational views (Cost sits below primary views). */
export const SECONDARY_VIEWS: readonly NavView[] = ["cost"];

/** Engineering & diagnostic views tucked under the collapsible Diagnostics group. */
export const DIAGNOSTIC_VIEWS: readonly NavView[] = [
  "grading",
  "scorecard",
  "coverage",
  "rsi",
  "alerts",
  "storage",
  "show",
];

const DIAGNOSTIC_SET = new Set<NavView>(DIAGNOSTIC_VIEWS);

/** Complete ordered navigation taxonomy. */
export const NAV_ENTRIES: readonly NavEntry[] = [
  {
    id: "fleet",
    group: "primary",
    description: { zh: "系统运行状态与任务完成度总览", en: "System health and mission completion overview" },
  },
  {
    id: "missions",
    group: "primary",
    description: { zh: "活跃任务进展、交付与时间轴", en: "Mission deliverables, progress, and timeline" },
  },
  {
    id: "agents",
    group: "primary",
    description: { zh: "在线 Agent 状态与实时终端控制台", en: "Live agents and interactive terminal console" },
  },
  {
    id: "features",
    group: "primary",
    description: { zh: "能力拓扑、主心骨与依赖关系图", en: "Capability spine, mind map, and dependencies" },
  },
  {
    id: "cost",
    group: "secondary",
    description: { zh: "模型费用与 Token 消耗分布", en: "Model spend and token usage breakdown" },
  },
  {
    id: "grading",
    group: "diagnostics",
    description: { zh: "任务难度评级与预设路由统计", en: "Task difficulty grading and preset routing" },
  },
  {
    id: "scorecard",
    group: "diagnostics",
    description: { zh: "工程质量评分卡与自治指标", en: "Multi-day engineering scorecard and metrics" },
  },
  {
    id: "coverage",
    group: "diagnostics",
    description: { zh: "代码库文件映射覆盖率扫描", en: "Repository feature-map file coverage scan" },
  },
  {
    id: "rsi",
    group: "diagnostics",
    description: { zh: "自举改进门禁运行详情", en: "Recursive improvement gate runs and logs" },
  },
  {
    id: "alerts",
    group: "diagnostics",
    description: { zh: "系统告警与事件通知历史记录", en: "Quiet diagnostic log of system alerts and notices" },
  },
  {
    id: "storage",
    group: "diagnostics",
    description: { zh: "存储提交 DAG 与工作区快照", en: "Storage commit DAG and workspace snapshots" },
  },
  {
    id: "show",
    group: "diagnostics",
    description: { zh: "全屏大屏监控视图", en: "Wallboard fleet topology display" },
  },
];

/** Returns true when `view` belongs to the collapsible Diagnostics group. */
export function isDiagnosticView(view: NavView): boolean {
  return DIAGNOSTIC_SET.has(view);
}

/** Auto-expands Diagnostics only when the user opened it or navigated directly to a diagnostic route. */
export function shouldExpandDiagnostics(stored: "open" | "closed", activeView: NavView): boolean {
  return stored === "open" || isDiagnosticView(activeView);
}

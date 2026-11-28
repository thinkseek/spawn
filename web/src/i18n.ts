/** Bilingual UI locale state (`zh` / `en`), persisted under `spawn.locale`.
 *
 * Pure module at import time (no React/DOM dependency at module evaluation) so
 * both the browser UI and `node --test` execute it directly. */

export const LOCALE_KEY = "spawn.locale";
export const LOCALE_CHOICES = ["zh", "en"] as const;
export type Locale = (typeof LOCALE_CHOICES)[number];
export const DEFAULT_LOCALE: Locale = "zh";

const listeners = new Set<() => void>();
let currentLocale: Locale | null = null;
let storageBound = false;

/** Normalize a raw stored/navigator locale string into `"zh"` or `"en"`. */
export function normalizeLocale(raw: unknown, fallback: Locale = DEFAULT_LOCALE): Locale {
  if (typeof raw !== "string") return fallback;
  const lower = raw.trim().toLowerCase();
  if (lower === "zh" || lower.startsWith("zh-") || lower.startsWith("zh_")) return "zh";
  if (lower === "en" || lower.startsWith("en-") || lower.startsWith("en_")) return "en";
  return fallback;
}

/** Pick Chinese or English copy for the active locale. */
export function tr(locale: Locale, zh: string, en: string): string {
  return locale === "zh" ? zh : en;
}

function readStoredLocale(): Locale {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      return normalizeLocale(window.localStorage.getItem(LOCALE_KEY), DEFAULT_LOCALE);
    }
  } catch {
    // Ignore storage read errors in restricted environments.
  }
  return DEFAULT_LOCALE;
}

/** Current active UI locale (`"zh"` by default). */
export function getLocale(): Locale {
  if (currentLocale === null) {
    currentLocale = readStoredLocale();
  }
  return currentLocale;
}

/** Sync `<html lang>` with the active locale when running in a document. */
export function syncDocumentLocale(locale: Locale = getLocale()): void {
  if (typeof document === "undefined") return;
  document.documentElement.lang = locale === "zh" ? "zh-CN" : "en";
}

function notifyLocaleListeners(): void {
  for (const listener of listeners) listener();
}

/** Update the active locale, persist it, update `<html lang>`, and notify subscribers. */
export function setLocale(next: Locale): void {
  const normalized = normalizeLocale(next, DEFAULT_LOCALE);
  const changed = getLocale() !== normalized;
  currentLocale = normalized;
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      window.localStorage.setItem(LOCALE_KEY, normalized);
    }
  } catch {
    // Ignore storage write errors in private mode.
  }
  syncDocumentLocale(normalized);
  if (changed) notifyLocaleListeners();
}

function bindStorageSync(): void {
  if (storageBound || typeof window === "undefined" || typeof window.addEventListener !== "function") {
    return;
  }
  storageBound = true;
  window.addEventListener("storage", (event) => {
    if (event.key !== LOCALE_KEY) return;
    const next = normalizeLocale(event.newValue, DEFAULT_LOCALE);
    if (next !== currentLocale) {
      currentLocale = next;
      syncDocumentLocale(next);
      notifyLocaleListeners();
    }
  });
}

/** Subscribe to locale transitions across same-tab toggles and cross-tab storage updates. */
export function subscribeLocale(listener: () => void): () => void {
  bindStorageSync();
  listeners.add(listener);
  return () => listeners.delete(listener);
}

const NAV_LABELS: Record<string, { zh: string; en: string }> = {
  fleet: { zh: "总览", en: "Overview" },
  missions: { zh: "任务进度", en: "Missions" },
  agents: { zh: "Agents", en: "Agents" },
  features: { zh: "能力地图", en: "Feature Map" },
  cost: { zh: "费用", en: "Cost" },
  sessions: { zh: "会话", en: "Sessions" },
  grading: { zh: "评级", en: "Grading" },
  scorecard: { zh: "评分卡", en: "Scorecard" },
  coverage: { zh: "文件覆盖", en: "File Coverage" },
  rsi: { zh: "RSI 任务", en: "RSI Mission" },
  alerts: { zh: "告警日志", en: "Alerts Log" },
  storage: { zh: "存储树", en: "Storage Tree" },
  show: { zh: "拓扑大屏", en: "Topology" },
};

/** Localized navigation label for any Spawn route view. */
export function navLabel(id: string, locale: Locale): string {
  const entry = NAV_LABELS[id];
  if (!entry) return id;
  return locale === "zh" ? entry.zh : entry.en;
}

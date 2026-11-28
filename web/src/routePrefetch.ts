import type { NavView } from "./navigation.ts";

export type LazyNavView = Exclude<NavView, "fleet">;
export type ModuleLoader = () => Promise<unknown>;

/** Primary and commonly visited side-tab views warmed automatically in idle time. */
export const PRIMARY_PREFETCH_ORDER: ReadonlyArray<NavView> = [
  "missions",
  "agents",
  "features",
  "cost",
  "sessions",
];

export interface NetworkConnectionHint {
  saveData?: boolean;
  effectiveType?: string;
}

/** Return the ordered list of route chunks to prefetch after initial paint. */
export function prefetchPlan(activeView: NavView): NavView[] {
  return PRIMARY_PREFETCH_ORDER.filter((view) => view !== activeView);
}

/** Respect browser Save-Data mode and slow 2G connections when considering idle prefetch. */
export function shouldSkipIdlePrefetch(connection?: NetworkConnectionHint | null): boolean {
  if (!connection) return false;
  if (connection.saveData === true) return true;
  return connection.effectiveType === "slow-2g" || connection.effectiveType === "2g";
}

/** Registry interface created via dependency inversion so Sidebar/shell helpers
 * can trigger prefetch without downward imports to feature view modules. */
export interface RoutePrefetcher {
  register(loaders: Partial<Record<NavView, ModuleLoader>>): void;
  prefetch(view: NavView): Promise<void>;
}

/** Build a deduplicated route-module prefetcher that retries if a previous load failed. */
export function createPrefetcher(
  initialLoaders: Partial<Record<NavView, ModuleLoader>> = {},
): RoutePrefetcher {
  const registry: Partial<Record<NavView, ModuleLoader>> = { ...initialLoaders };
  const inflight = new Map<NavView, Promise<unknown>>();

  return {
    register(loaders) {
      Object.assign(registry, loaders);
    },
    prefetch(view) {
      const loader = registry[view];
      if (!loader) return Promise.resolve();
      const existing = inflight.get(view);
      if (existing) {
        return existing.then(
          () => undefined,
          () => undefined,
        );
      }
      const task = loader().catch((err) => {
        if (inflight.get(view) === task) inflight.delete(view);
        throw err;
      });
      inflight.set(view, task);
      return task.then(
        () => undefined,
        () => undefined,
      );
    },
  };
}

const sharedPrefetcher = createPrefetcher();

/** Register route chunk loaders from the composition root (`App.tsx`). */
export function registerRouteLoaders(loaders: Partial<Record<NavView, ModuleLoader>>): void {
  sharedPrefetcher.register(loaders);
}

/** Preload a view's split chunk (also warms Service Worker `/assets/*` cache). */
export function prefetchView(view: NavView): Promise<void> {
  return sharedPrefetcher.prefetch(view);
}

/** Stagger idle-time chunk downloads after initial mount without competing with API reads. */
export function scheduleIdlePrefetch(activeView: NavView): () => void {
  if (typeof window === "undefined") return () => undefined;
  const nav = navigator as Navigator & { connection?: NetworkConnectionHint };
  if (shouldSkipIdlePrefetch(nav.connection)) return () => undefined;

  const queue = prefetchPlan(activeView);
  if (queue.length === 0) return () => undefined;

  let cancelled = false;
  let idleHandle: number | null = null;
  let idleMode: "idle" | "timeout" | null = null;
  let stepTimer: number | null = null;
  let idx = 0;

  const win = window as Window & {
    requestIdleCallback?: (cb: () => void, opts?: { timeout: number }) => number;
    cancelIdleCallback?: (id: number) => void;
  };

  const step = () => {
    if (cancelled || idx >= queue.length) return;
    if (typeof document !== "undefined" && document.visibilityState === "hidden") {
      stepTimer = window.setTimeout(step, 1_000);
      return;
    }
    const target = queue[idx];
    idx += 1;
    void prefetchView(target);
    if (idx < queue.length) {
      stepTimer = window.setTimeout(step, 250);
    }
  };

  if (typeof win.requestIdleCallback === "function") {
    idleMode = "idle";
    idleHandle = win.requestIdleCallback(step, { timeout: 2_500 });
  } else {
    idleMode = "timeout";
    idleHandle = window.setTimeout(step, 1_500);
  }

  return () => {
    cancelled = true;
    if (idleHandle !== null) {
      if (idleMode === "idle" && typeof win.cancelIdleCallback === "function") {
        win.cancelIdleCallback(idleHandle);
      } else {
        window.clearTimeout(idleHandle);
      }
    }
    if (stepTimer !== null) {
      window.clearTimeout(stepTimer);
    }
  };
}

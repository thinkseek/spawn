/** Pure state & error-classification helpers for route-level ErrorBoundaries. */

export interface RouteErrorState {
  hasError: boolean;
  message: string;
  isChunkError: boolean;
  retryNonce: number;
  resetKey: string;
}

const CHUNK_ERROR_PATTERNS = [
  /ChunkLoadError/i,
  /Loading(?: CSS)? chunk [\w-]+ failed/i,
  /Failed to fetch dynamically imported module/i,
  /error loading dynamically imported module/i,
  /Importing a module script failed/i,
  /Unable to preload CSS/i,
];

/** Return error message text cleanly for Error objects and unknown throws. */
export function errorMessageOf(error: unknown): string {
  if (error instanceof Error && error.message.trim().length > 0) {
    return error.message.trim();
  }
  if (typeof error === "string" && error.trim().length > 0) {
    return error.trim();
  }
  return "Unexpected rendering error";
}

/** Whether `error` looks like a dynamic-import / lazy chunk fetch failure (e.g. during asset rollout). */
export function isChunkLoadError(error: unknown): boolean {
  const name = error instanceof Error ? error.name : "";
  const msg = errorMessageOf(error);
  return CHUNK_ERROR_PATTERNS.some((pattern) => pattern.test(name) || pattern.test(msg));
}

export function createRouteErrorState(resetKey = ""): RouteErrorState {
  return {
    hasError: false,
    message: "",
    isChunkError: false,
    retryNonce: 0,
    resetKey,
  };
}

export function routeErrorFromCaught(error: unknown, current: RouteErrorState): RouteErrorState {
  return {
    hasError: true,
    message: errorMessageOf(error),
    isChunkError: isChunkLoadError(error),
    retryNonce: current.retryNonce,
    resetKey: current.resetKey,
  };
}

/** Clear error state when the caller explicitly retries, bumping `retryNonce` so lazy loaders re-run. */
export function retryRouteErrorState(current: RouteErrorState): RouteErrorState {
  return {
    hasError: false,
    message: "",
    isChunkError: false,
    retryNonce: current.retryNonce + 1,
    resetKey: current.resetKey,
  };
}

/** Recover cleanly if the user switches routes/targets after a lazy boundary failed. */
export function syncRouteErrorResetKey(current: RouteErrorState, nextResetKey: string): RouteErrorState {
  if (current.resetKey === nextResetKey) return current;
  return {
    hasError: false,
    message: "",
    isChunkError: false,
    retryNonce: current.hasError ? current.retryNonce + 1 : current.retryNonce,
    resetKey: nextResetKey,
  };
}

/**
 * Pure attempt controller mirroring `retryableLazy`: caches the current loader
 * promise for a given `retryNonce`, and re-invokes `loader()` when `retryNonce`
 * advances after a failed chunk load so clicking Retry actually hits the network again.
 */
export function createRetryableLoader<T>(loader: () => Promise<T>): {
  load: (retryNonce: number) => Promise<T>;
  attempts: () => number;
} {
  let activeNonce = -1;
  let activePromise: Promise<T> | null = null;
  let count = 0;

  return {
    load(retryNonce: number): Promise<T> {
      if (activePromise && activeNonce === retryNonce) {
        return activePromise;
      }
      activeNonce = retryNonce;
      count += 1;
      activePromise = loader();
      return activePromise;
    },
    attempts: () => count,
  };
}

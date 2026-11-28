import { AlertTriangle, RefreshCw, RotateCcw } from "lucide-react";
import {
  Component,
  createContext,
  lazy,
  useContext,
  type ComponentProps,
  type ComponentType,
  type ErrorInfo,
  type LazyExoticComponent,
  type ReactNode,
} from "react";

import {
  createRouteErrorState,
  retryRouteErrorState,
  routeErrorFromCaught,
  syncRouteErrorResetKey,
  type RouteErrorState,
} from "../routeErrorBoundary.ts";
import { cx, tr, useLocale } from "./ui";

const RouteRetryContext = createContext(0);

/**
 * Wrap `React.lazy(loader)` so a rejected dynamic-import (e.g. stale hashed
 * chunk returning 404 right after a frontend rebuild) is re-created cleanly
 * when `RouteErrorBoundary` bumps its retry nonce instead of staying permanently
 * rejected inside React's lazy payload cache — while keeping already-resolved
 * modules cached across normal route switches with zero Suspense flicker.
 */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function retryableLazy<T extends ComponentType<any>>(
  loader: () => Promise<{ default: T }>,
): ComponentType<ComponentProps<T>> {
  let cachedNonce = -1;
  let resolved = false;
  let cachedComponent: LazyExoticComponent<T> | null = null;

  return function RetryableLazyComponent(props: ComponentProps<T>) {
    const retryNonce = useContext(RouteRetryContext);
    if (!cachedComponent || (!resolved && cachedNonce !== retryNonce)) {
      cachedNonce = retryNonce;
      resolved = false;
      cachedComponent = lazy(() =>
        loader().then((mod) => {
          resolved = true;
          return mod;
        }),
      );
    }
    const LazyComponent = cachedComponent;
    return <LazyComponent {...props} />;
  };
}

function RouteErrorFallback({
  message,
  isChunkError,
  onRetry,
  onReload,
  compact,
}: {
  message: string;
  isChunkError: boolean;
  onRetry: () => void;
  onReload: () => void;
  compact?: boolean;
}) {
  const { locale } = useLocale();
  return (
    <div
      role="alert"
      className={cx(
        "flex flex-col items-center justify-center rounded-card border border-warning/30 bg-surface text-center shadow-card",
        compact ? "my-4 gap-3 px-4 py-6" : "mx-auto my-8 max-w-md gap-4 px-6 py-8",
      )}
    >
      <div className="flex h-10 w-10 items-center justify-center rounded-full bg-warning/10 text-warning">
        <AlertTriangle className="h-5 w-5" aria-hidden="true" />
      </div>
      <div className="space-y-1.5">
        <h2 className="text-body font-semibold text-foreground">
          {isChunkError
            ? tr(locale, "页面资源已更新或加载失败", "Updated assets or module load failed")
            : tr(locale, "视图加载失败", "Failed to load view")}
        </h2>
        <p className="text-caption text-muted-foreground">
          {isChunkError
            ? tr(
                locale,
                "控制台可能刚完成了新版本构建，旧版本分块已失效。可重试加载或刷新整个页面。",
                "A newer console build may have replaced stale asset chunks. Retry loading this view or reload the page.",
              )
            : tr(
                locale,
                "该子视图在加载或渲染时出现异常，其余控制台功能仍可正常使用。",
                "This view hit an error while loading. The rest of the console remains responsive.",
              )}
        </p>
        {message && (
          <p className="break-all rounded border border-surface-border bg-subtle px-2.5 py-1.5 font-mono text-micro text-faint-foreground">
            {message}
          </p>
        )}
      </div>
      <div className="flex flex-wrap items-center justify-center gap-2.5 pt-1">
        <button
          type="button"
          onClick={onRetry}
          className="inline-flex min-h-11 items-center gap-1.5 rounded border border-brand/40 bg-brand/15 px-3.5 py-2 text-caption font-medium text-brand-light transition-colors hover:bg-brand/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/50"
        >
          <RotateCcw className="h-3.5 w-3.5" aria-hidden="true" />
          <span>{tr(locale, "重试", "Retry")}</span>
        </button>
        <button
          type="button"
          onClick={onReload}
          className="inline-flex min-h-11 items-center gap-1.5 rounded border border-surface-border bg-subtle px-3.5 py-2 text-caption font-medium text-foreground transition-colors hover:border-foreground/25 hover:bg-surface focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/50"
        >
          <RefreshCw className="h-3.5 w-3.5" aria-hidden="true" />
          <span>{tr(locale, "刷新页面", "Reload page")}</span>
        </button>
      </div>
    </div>
  );
}

export interface RouteErrorBoundaryProps {
  /** Changing this key (for example the active route hash or view id) clears any prior error. */
  resetKey: string;
  children: ReactNode;
  compact?: boolean;
  onRetry?: () => void;
  onReload?: () => void;
}

export class RouteErrorBoundary extends Component<RouteErrorBoundaryProps, RouteErrorState> {
  constructor(props: RouteErrorBoundaryProps) {
    super(props);
    this.state = createRouteErrorState(props.resetKey);
  }

  static getDerivedStateFromError(error: unknown): Partial<RouteErrorState> {
    return {
      hasError: true,
      message: routeErrorFromCaught(error, createRouteErrorState()).message,
      isChunkError: routeErrorFromCaught(error, createRouteErrorState()).isChunkError,
    };
  }

  static getDerivedStateFromProps(
    props: RouteErrorBoundaryProps,
    state: RouteErrorState,
  ): RouteErrorState | null {
    const next = syncRouteErrorResetKey(state, props.resetKey);
    return next === state ? null : next;
  }

  componentDidCatch(_error: unknown, _info: ErrorInfo): void {
    // Error details are captured cleanly by getDerivedStateFromError and displayed in the fallback card.
  }

  private handleRetry = (): void => {
    this.props.onRetry?.();
    this.setState((prev) => retryRouteErrorState(prev));
  };

  private handleReload = (): void => {
    if (this.props.onReload) {
      this.props.onReload();
      return;
    }
    if (typeof window !== "undefined") {
      window.location.reload();
    }
  };

  render(): ReactNode {
    if (this.state.hasError) {
      return (
        <RouteErrorFallback
          message={this.state.message}
          isChunkError={this.state.isChunkError}
          onRetry={this.handleRetry}
          onReload={this.handleReload}
          compact={this.props.compact}
        />
      );
    }
    return (
      <RouteRetryContext.Provider value={this.state.retryNonce} key={this.state.retryNonce}>
        {this.props.children}
      </RouteRetryContext.Provider>
    );
  }
}

import type { ReactNode } from "react";

import { PIXEL_GRID_DELAYS_MS } from "../beautifulPrimitives";
import { cx } from "./ui";

const TONE_BG = {
  accent: "bg-accent",
  success: "bg-success",
  warning: "bg-warning",
  muted: "bg-muted-foreground",
} as const;

const GRID_SIZE = {
  xs: "gap-[1.5px]",
  sm: "gap-[2px]",
  md: "gap-[2.5px]",
} as const;

const CELL_SIZE = {
  xs: "size-[2.5px]",
  sm: "size-[3px]",
  md: "size-[3.5px]",
} as const;

/**
 * Beautiful UI `01 Loading State` — crisp 3x3 pixel-grid shimmer loader.
 * Respects `prefers-reduced-motion: reduce` via `.bui-pixel-cell` in `index.css`.
 */
export function PixelGridLoader({
  tone = "accent",
  size = "sm",
  label,
  className,
}: {
  tone?: keyof typeof TONE_BG;
  size?: keyof typeof GRID_SIZE;
  label?: string;
  className?: string;
}) {
  return (
    <span
      role={label ? "status" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
      className={cx("inline-grid shrink-0 grid-cols-3", GRID_SIZE[size], className)}
    >
      {PIXEL_GRID_DELAYS_MS.map((delayMs, idx) => (
        <span
          key={idx}
          style={{ animationDelay: `${delayMs}ms` }}
          className={cx("bui-pixel-cell rounded-[1px]", CELL_SIZE[size], TONE_BG[tone])}
        />
      ))}
    </span>
  );
}

/**
 * Beautiful UI `05 Tool Chips` — compact monospace chip for active tools and jobs.
 */
export function ToolChip({
  icon,
  label,
  count,
  title,
  tone = "neutral",
  className,
}: {
  icon?: ReactNode;
  label: string;
  count?: number;
  title?: string;
  tone?: "neutral" | "active";
  className?: string;
}) {
  return (
    <span
      title={title ?? label}
      className={cx(
        "inline-flex min-w-0 max-w-full items-center gap-1.5 rounded-chip px-2 py-0.5 font-mono text-micro shadow-hairline",
        tone === "active"
          ? "bg-inset text-foreground"
          : "bg-inset text-muted-foreground",
        className,
      )}
    >
      {icon && <span className="flex shrink-0 items-center">{icon}</span>}
      <span className="min-w-0 truncate">{label}</span>
      {count !== undefined && count > 1 && (
        <span className="tnum shrink-0 text-faint-foreground">×{count}</span>
      )}
    </span>
  );
}

/** Tactile hairline `<kbd>` keycap badge for prompt shortcuts and numbered approval choices. */
export function KeyCap({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <kbd
      className={cx(
        "inline-flex min-w-[1.15rem] shrink-0 items-center justify-center rounded-[4px] bg-inset px-1 py-px font-mono text-[10px] leading-tight text-muted-foreground shadow-hairline",
        className,
      )}
    >
      {children}
    </kbd>
  );
}

/** Subtle breathing text treatment paired with PixelGridLoader during live async states. */
export function ShimmerLabel({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return <span className={cx("bui-shimmer-text", className)}>{children}</span>;
}

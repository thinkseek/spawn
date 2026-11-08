import { ChevronRight, WifiOff, X } from "lucide-react";
import { useEffect } from "react";
import type { ReactNode } from "react";

import { absoluteTitle, relativeTime } from "../format";
import { splitId } from "../ordering";

/** Join class names, skipping falsy ones. */
export function cx(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(" ");
}

export function Card({
  className,
  children,
}: {
  className?: string;
  children: ReactNode;
}) {
  return (
    <section
      className={cx(
        "rounded-lg border border-surface-border bg-surface",
        className,
      )}
    >
      {children}
    </section>
  );
}

export function CardHeader({
  icon,
  title,
  meta,
  className,
}: {
  icon?: ReactNode;
  title: ReactNode;
  meta?: ReactNode;
  className?: string;
}) {
  return (
    <header
      className={cx(
        "flex h-10 shrink-0 items-center gap-2 border-b border-surface-border px-4",
        className,
      )}
    >
      {icon && (
        <span className="flex shrink-0 text-faint-foreground">{icon}</span>
      )}
      <h2 className="min-w-0 truncate text-label font-medium text-foreground">
        {title}
      </h2>
      {meta && (
        <div className="ml-auto flex min-w-0 shrink-0 items-center gap-2 text-caption text-muted-foreground">
          {meta}
        </div>
      )}
    </header>
  );
}

/** Small caption above a value. */
export function Eyebrow({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={cx("text-caption text-muted-foreground", className)}>
      {children}
    </div>
  );
}

export function PageHeader({
  title,
  children,
}: {
  title: ReactNode;
  children?: ReactNode;
}) {
  return (
    <header className="flex h-12 shrink-0 items-center gap-3 border-b border-surface-border bg-page-canvas px-4 sm:px-5">
      <div className="flex min-w-0 items-center gap-2.5">{title}</div>
      <div className="ml-auto flex shrink-0 items-center gap-2">{children}</div>
    </header>
  );
}

/** Connection status of a poll: a calm dot when live, red when not. */
export function LiveStatus({
  error,
  label,
}: {
  error: string | null;
  label: ReactNode;
}) {
  if (error) {
    return (
      <span
        title={error}
        className="inline-flex h-6 items-center gap-1.5 rounded-md bg-destructive/12 px-2 text-caption font-medium text-destructive ring-1 ring-destructive/25 ring-inset"
      >
        <WifiOff size={12} />
        {error.startsWith("HTTP 401") ? "Token needed" : "Disconnected"}
      </span>
    );
  }
  return (
    <span className="inline-flex h-6 items-center gap-1.5 text-caption text-muted-foreground">
      <span className="relative flex size-2">
        <span className="absolute inline-flex size-full animate-spawn-pulse rounded-full bg-success/50" />
        <span className="relative inline-flex size-2 rounded-full bg-success" />
      </span>
      <span className="tnum">{label}</span>
    </span>
  );
}

export function Button({
  children,
  onClick,
  title,
  className,
}: {
  children: ReactNode;
  onClick: () => void;
  title?: string;
  className?: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      className={cx(
        "inline-flex h-7 shrink-0 items-center gap-1.5 rounded-md border border-surface-border bg-surface px-2.5 text-caption font-medium text-muted-foreground transition-colors hover:border-border-strong hover:bg-surface-hover hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-ring",
        className,
      )}
    >
      {children}
    </button>
  );
}

export function EmptyState({
  icon,
  title,
  hint,
  className,
}: {
  icon?: ReactNode;
  title: string;
  hint?: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cx(
        "flex flex-col items-center justify-center gap-1.5 px-6 py-8 text-center",
        className,
      )}
    >
      {icon && <span className="mb-1 text-faint-foreground">{icon}</span>}
      <p className="text-label font-medium text-muted-foreground">{title}</p>
      {hint && (
        <p className="max-w-sm text-caption text-faint-foreground">{hint}</p>
      )}
    </div>
  );
}

/** Right-hand slide-over with a backdrop; Escape closes it. */
export function Drawer({
  onClose,
  header,
  children,
  width = "max-w-4xl",
}: {
  onClose: () => void;
  header: ReactNode;
  children: ReactNode;
  width?: string;
}) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="fixed inset-0 z-40 flex justify-end">
      <div
        aria-hidden="true"
        onClick={onClose}
        className="animate-fade-in absolute inset-0 bg-black/55 backdrop-blur-[2px]"
      />
      <aside
        role="dialog"
        aria-modal="true"
        className={cx(
          "animate-drawer-in relative flex h-full w-full flex-col border-l border-border-strong bg-page-canvas shadow-[var(--floating-shadow)]",
          width,
        )}
      >
        <header className="flex h-14 shrink-0 items-center gap-3 border-b border-surface-border px-4 sm:px-5">
          <div className="flex min-w-0 flex-1 items-center gap-2.5">
            {header}
          </div>
          <kbd className="hidden rounded border border-surface-border px-1.5 py-0.5 font-mono text-micro text-faint-foreground sm:inline">
            esc
          </kbd>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="grid size-8 place-items-center rounded-md text-muted-foreground transition-colors hover:bg-surface-hover hover:text-foreground"
          >
            <X size={16} />
          </button>
        </header>
        {children}
      </aside>
    </div>
  );
}

export function Tabs<T extends string>({
  tabs,
  active,
  onChange,
}: {
  tabs: Array<{ id: T; label: ReactNode; count?: ReactNode }>;
  active: T;
  onChange: (id: T) => void;
}) {
  return (
    <div
      role="tablist"
      className="flex shrink-0 items-center gap-1 border-b border-surface-border px-3 sm:px-4"
    >
      {tabs.map((tab) => {
        const selected = tab.id === active;
        return (
          <button
            key={tab.id}
            type="button"
            role="tab"
            aria-selected={selected}
            onClick={() => onChange(tab.id)}
            className={cx(
              "relative -mb-px inline-flex h-10 items-center gap-1.5 border-b-2 px-2.5 text-label font-medium transition-colors",
              selected
                ? "border-foreground text-foreground"
                : "border-transparent text-muted-foreground hover:text-foreground",
            )}
          >
            {tab.label}
            {tab.count != null && (
              <span className="tnum rounded bg-muted px-1.5 py-px text-micro text-muted-foreground">
                {tab.count}
              </span>
            )}
          </button>
        );
      })}
    </div>
  );
}

/** The focus ring every interactive element shows on keyboard focus.
 * `outline-solid`, never `outline-none` beside it: in Tailwind 4,
 * `outline-none` sets the style variable `outline-2` reads to none. */
export const FOCUS_RING =
  "focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-offset-1 focus-visible:outline-ring";

/** The same ring drawn inside, for full-width rows a scroll box would clip. */
export const FOCUS_RING_INSET =
  "focus-visible:outline-2 focus-visible:outline-solid focus-visible:-outline-offset-2 focus-visible:outline-ring";

/**
 * A long id truncated in the middle: the head loses its end to an
 * ellipsis, the distinguishing suffix stays (`agent_worker_…_96`). The
 * whole id is the tooltip.
 */
export function MiddleId({
  id,
  className,
  title = true,
}: {
  id: string;
  className?: string;
  title?: boolean;
}) {
  const { head, tail } = splitId(id);
  return (
    <span
      className={cx("inline-flex min-w-0 max-w-full", className)}
      title={title ? id : undefined}
    >
      <span className="min-w-0 truncate">{head}</span>
      {tail && <span className="shrink-0">{tail}</span>}
    </span>
  );
}

/** What a row names first: the server's semantic label (one line that says
 * what the agent, task or item is doing, crates/spawn-cli/src/serve/labels.rs),
 * with the raw id under it, small and monospace, selectable whole: ids are
 * the keys of the CLI and of SQL. Without a label (an older server) the id
 * alone, as before. */
export function LabeledId({
  label,
  id,
  className,
  clamp = 2,
}: {
  label: string | null;
  id: string;
  className?: string;
  /** Lines the label may wrap to before it is cut. */
  clamp?: 1 | 2;
}) {
  if (!label) return <MiddleId id={id} className={className} />;
  return (
    <span className="flex min-w-0 max-w-full flex-col">
      <span
        className={cx("min-w-0", clamp === 2 ? "line-clamp-2 break-words" : "truncate", className)}
        title={label}
      >
        {label}
      </span>
      <MiddleId id={id} className="select-all font-mono text-micro font-normal text-faint-foreground" />
    </span>
  );
}

/** A relative time ("12m ago") that carries the absolute local time, and
 * UTC, in its tooltip. */
export function Ago({
  ms,
  className,
  format = relativeTime,
}: {
  ms: number | null | undefined;
  className?: string;
  format?: (ms: number | null | undefined) => string;
}) {
  const valid = ms != null && Number.isFinite(ms) && ms > 0;
  return (
    <time
      dateTime={valid ? new Date(ms).toISOString() : undefined}
      title={absoluteTitle(ms)}
      className={cx("tnum whitespace-nowrap", className)}
    >
      {format(ms)}
    </time>
  );
}

/** "12m" from "12m ago": a duration up to now. */
export function sinceText(ms: number | null | undefined): string {
  return relativeTime(ms).replace(" ago", "");
}

/**
 * The header of a group in a list: its label and count, and, when it
 * folds, a button with aria-expanded.
 */
export function GroupHeader({
  label,
  count,
  expanded,
  onToggle,
  trailing,
  className,
}: {
  label: ReactNode;
  count: number;
  /** Undefined for a group that does not fold. */
  expanded?: boolean;
  onToggle?: () => void;
  trailing?: ReactNode;
  className?: string;
}) {
  const body = (
    <>
      {onToggle && (
        <ChevronRight
          size={12}
          aria-hidden="true"
          className={cx(
            "shrink-0 transition-transform duration-150 motion-reduce:transition-none",
            expanded && "rotate-90",
          )}
        />
      )}
      <span>{label}</span>
      <span className="tnum font-normal tracking-normal text-muted-foreground">{count}</span>
    </>
  );
  return (
    <div
      className={cx(
        "flex items-center gap-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground",
        className,
      )}
    >
      {onToggle ? (
        <button
          type="button"
          aria-expanded={expanded}
          onClick={onToggle}
          className={cx(
            "-mx-1 inline-flex items-center gap-1.5 rounded px-1 py-0.5 uppercase hover:text-foreground",
            FOCUS_RING,
          )}
        >
          {body}
        </button>
      ) : (
        <span className="inline-flex items-center gap-1.5">{body}</span>
      )}
      {trailing && <span className="ml-auto normal-case tracking-normal">{trailing}</span>}
    </div>
  );
}

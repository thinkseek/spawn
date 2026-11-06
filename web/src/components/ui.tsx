import { WifiOff, X } from "lucide-react";
import { useEffect } from "react";
import type { ReactNode } from "react";

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

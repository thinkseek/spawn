/* Small pieces the missions cards and the missions timeline both show. */

import { ArrowUpRight, ChevronRight } from "lucide-react";
import type { ReactNode } from "react";

/** The task's agent, linked to its session in the fleet view. */
export function AgentLink({
  sessionId,
  agentId,
}: {
  sessionId: string | null;
  agentId: string | null;
}) {
  const label = agentId ?? sessionId;
  if (!label) return null;
  if (!sessionId) {
    return (
      <span className="truncate font-mono text-caption text-muted-foreground">
        {label}
      </span>
    );
  }
  return (
    <a
      href={`#/fleet/${encodeURIComponent(sessionId)}`}
      title={`Open ${sessionId} in the fleet view`}
      className="inline-flex min-w-0 items-center gap-0.5 truncate font-mono text-caption text-info hover:underline"
    >
      {label}
      <ArrowUpRight size={12} className="shrink-0" />
    </a>
  );
}

export function Collapsible({
  label,
  children,
  defaultOpen = false,
  onToggle,
}: {
  label: string;
  children: ReactNode;
  defaultOpen?: boolean;
  /** Told when the section opens or closes. */
  onToggle?: (open: boolean) => void;
}) {
  return (
    <details
      open={defaultOpen}
      className="group min-w-0"
      onToggle={onToggle ? (event) => onToggle(event.currentTarget.open) : undefined}
    >
      <summary className="flex cursor-pointer list-none items-center gap-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground select-none hover:text-muted-foreground [&::-webkit-details-marker]:hidden">
        <ChevronRight
          size={12}
          className="transition-transform group-open:rotate-90"
        />
        {label}
      </summary>
      <div className="mt-1.5 rounded-md border border-surface-border bg-page-canvas/60 px-3 py-2">
        {children}
      </div>
    </details>
  );
}
